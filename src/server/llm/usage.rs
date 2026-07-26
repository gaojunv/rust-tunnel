//! LLM 用量采集：从上游响应解析 token 用量（含缓存命中/未命中细分）。
//!
//! token 数一律来自上游 `usage` 字段，网关不做本地估算，与上游计费口径一致。
//! - 非流式：完整响应 JSON，直接读 `usage`。
//! - 流式：`usage` 只在末尾 chunk 出现，用 [`UsageSseScanner`] 边转发边扫描。
//!
//! 缓存字段各厂商命名不同，[`extract_usage`] 做多字段兜底。

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::Value;

use crate::server::db::{Database, LlmUsageInsert};

/// 一次请求解析出的 token 用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageInfo {
    pub prompt_tokens: i64,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

impl UsageInfo {
    /// 是否解析到任何有效数据（用于判断流式是否拿到末尾 usage）。
    pub fn is_empty(&self) -> bool {
        self.total_tokens == 0 && self.prompt_tokens == 0 && self.completion_tokens == 0
    }
}

/// 从上游响应 JSON 的 `usage` 对象提取用量。
///
/// 同时兼容 OpenAI 风格（`prompt_tokens`/`completion_tokens`）与 Anthropic 原生
/// （`input_tokens`/`output_tokens`）。缓存字段按以下优先级兜底：
/// 1. DeepSeek：`prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`
/// 2. OpenAI 新口径：`prompt_tokens_details.cached_tokens`
/// 3. Anthropic：`cache_read_input_tokens` / `cache_creation_input_tokens`
///
/// 恒等关系：`cache_hit + cache_miss == prompt`（无缓存信息时全记为 miss）。
pub fn extract_usage(usage: &Value) -> UsageInfo {
    if !usage.is_object() {
        return UsageInfo::default();
    }

    let get = |k: &str| usage.get(k).and_then(Value::as_i64);

    // 输入 token：OpenAI 用 prompt_tokens，Anthropic 用 input_tokens。
    // Anthropic 的 input_tokens 不含缓存部分，需加回 cache_read + cache_creation。
    let anthropic_cache_read = get("cache_read_input_tokens");
    let anthropic_cache_creation = get("cache_creation_input_tokens");

    let (prompt_tokens, mut cache_hit) = if let Some(pt) = get("prompt_tokens") {
        // ── OpenAI 系 ──
        let hit = get("prompt_cache_hit_tokens")
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_i64)
            })
            .unwrap_or(0);
        (pt, hit)
    } else if let Some(it) = get("input_tokens") {
        // ── Anthropic 原生 ──
        // input_tokens 是未缓存的新增输入；总输入 = it + cache_read + cache_creation。
        let hit = anthropic_cache_read.unwrap_or(0);
        let creation = anthropic_cache_creation.unwrap_or(0);
        (it + hit + creation, hit)
    } else {
        (0, 0)
    };

    // 完整性收敛：命中钳制到 [0, prompt]，未命中由恒等式确定，
    // 避免上游字段自相矛盾（如 hit > prompt）导致 hit + miss != prompt。
    cache_hit = cache_hit.clamp(0, prompt_tokens);
    let cache_miss = prompt_tokens - cache_hit;

    let completion_tokens = get("completion_tokens")
        .or_else(|| get("output_tokens"))
        .unwrap_or(0);
    let total_tokens = get("total_tokens").unwrap_or(prompt_tokens + completion_tokens);

    UsageInfo {
        prompt_tokens,
        cache_hit_tokens: cache_hit,
        cache_miss_tokens: cache_miss,
        completion_tokens,
        total_tokens,
    }
}

/// 从完整（非流式）响应体提取 usage。找不到 `usage` 返回默认零值。
pub fn extract_usage_from_body(body: &Value) -> UsageInfo {
    body.get("usage").map(extract_usage).unwrap_or_default()
}

/// 流式 SSE usage 扫描器：逐 chunk 喂入字节，扫描其中携带 usage 的行。
///
/// 覆盖两种流：
/// - OpenAI（含 Anthropic 回退路径转出的 OpenAI chunk）：`data: {... "usage": {...}}`
///   —— 需要上游注入 `stream_options.include_usage=true` 才会出现。
/// - Anthropic 原生直通流：`message_start`（input_tokens）+ `message_delta`（output_tokens）。
///
/// 只保留“最后一次见到的、非空的” usage。跨 chunk 边界的行由内部缓冲拼接。
#[derive(Debug, Default)]
pub struct UsageSseScanner {
    line_buf: String,
    /// Anthropic 流的 input 部分（来自 message_start）在 output 之前，需要合并。
    anthropic_input: Option<UsageInfo>,
    latest: UsageInfo,
}

impl UsageSseScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段上游字节（透传给客户端的同一份数据）。
    pub fn push(&mut self, bytes: &[u8]) {
        self.line_buf.push_str(&String::from_utf8_lossy(bytes));
        while let Some(pos) = self.line_buf.find('\n') {
            let line = self.line_buf[..pos].trim_end_matches('\r').to_string();
            self.line_buf.drain(..=pos);
            self.process_line(&line);
        }
    }

    fn process_line(&mut self, line: &str) {
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            return;
        };

        // ── Anthropic 原生事件 ──
        match chunk.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(u) = chunk
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .map(extract_usage)
                {
                    self.anthropic_input = Some(u);
                }
                return;
            }
            Some("message_delta") => {
                if let Some(delta_usage) = chunk.get("usage").map(extract_usage) {
                    // message_delta 的 usage 只有 output_tokens，合并 message_start 的输入。
                    let mut merged = self.anthropic_input.unwrap_or_default();
                    merged.completion_tokens = delta_usage.completion_tokens;
                    merged.total_tokens = merged.prompt_tokens + merged.completion_tokens;
                    self.latest = merged;
                }
                return;
            }
            _ => {}
        }

        // ── OpenAI 风格 chunk ──
        if let Some(usage) = chunk.get("usage") {
            if usage.is_object() {
                let u = extract_usage(usage);
                if !u.is_empty() {
                    self.latest = u;
                }
            }
        }
    }

    /// 结束扫描，返回收集到的 usage（可能为空）。
    pub fn finish(self) -> UsageInfo {
        self.latest
    }
}

/// 记录一次请求所需的标识信息（token 用量由响应解析补齐）。
#[derive(Debug, Clone, Default)]
pub struct UsageContext {
    pub api_key_id: Option<String>,
    pub api_key_name: String,
    pub provider_id: Option<String>,
    pub provider_name: String,
    pub model_id: Option<String>,
    /// 上游真实模型名。
    pub model_name: String,
    /// 客户端请求里的原始 model（别名）。
    pub requested_model: String,
    /// "openai" / "anthropic"。
    pub protocol: String,
    pub stream: bool,
}

impl UsageContext {
    fn into_insert(
        self,
        usage: UsageInfo,
        status_code: i32,
        success: bool,
        latency_ms: i64,
        error_type: Option<String>,
    ) -> LlmUsageInsert {
        LlmUsageInsert {
            api_key_id: self.api_key_id,
            api_key_name: self.api_key_name,
            provider_id: self.provider_id,
            provider_name: self.provider_name,
            model_id: self.model_id,
            model_name: self.model_name,
            requested_model: self.requested_model,
            protocol: self.protocol,
            stream: self.stream,
            status_code,
            success,
            prompt_tokens: usage.prompt_tokens,
            cache_hit_tokens: usage.cache_hit_tokens,
            cache_miss_tokens: usage.cache_miss_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            latency_ms,
            error_type,
        }
    }
}

/// fire-and-forget 写入一条用量日志（失败仅记日志，不影响请求）。
fn spawn_record(db: Database, insert: LlmUsageInsert) {
    tokio::spawn(async move {
        if let Err(e) = db.llm_insert_usage_log(&insert).await {
            tracing::warn!("failed to record LLM usage log: {}", e);
        }
    });
}

impl UsageContext {
    /// 记录一条失败请求（fire-and-forget，写入失败仅记日志）。
    ///
    /// 用于上游调用失败、认证失败、路由失败等场景，确保请求明细中
    /// 能看到失败的请求而不仅仅是成功请求。
    pub fn record_failure(
        self,
        db: &Database,
        status_code: i32,
        error_type: &str,
        started_at: std::time::Instant,
    ) {
        let insert = self.into_insert(
            UsageInfo::default(),
            status_code,
            false,
            started_at.elapsed().as_millis() as i64,
            Some(error_type.to_string()),
        );
        spawn_record(db.clone(), insert);
    }
}

/// 包裹上游成功响应，解析 usage 并异步落库，返回可继续发给客户端的响应。
///
/// - 非流式：整体缓冲 body（LLM 响应通常 < 1MB），解析后重建响应。
/// - 流式：包装字节流，边转发边扫描 usage，流结束时落库（不缓冲整流）。
///
/// `db` 为 None（无持久化）时直接透传，不产生任何开销。
pub async fn wrap_and_record(
    resp: Response,
    ctx: UsageContext,
    db: Option<Database>,
    started_at: std::time::Instant,
) -> Response {
    let Some(db) = db else {
        return resp;
    };

    let status = resp.status();
    let is_stream = ctx.stream;
    let status_code = status.as_u16() as i32;

    if !is_stream {
        // 缓冲整个 body（上限 16MB，与分发层一致）。
        let (parts, body) = resp.into_parts();
        let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => {
                // 读取失败：记一条无 token 的日志，返回错误。
                let insert = ctx.into_insert(
                    UsageInfo::default(),
                    status_code,
                    false,
                    started_at.elapsed().as_millis() as i64,
                    Some(format!("body read error: {e}")),
                );
                spawn_record(db, insert);
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("failed to read upstream response"))
                    .unwrap();
            }
        };
        let usage = serde_json::from_slice::<Value>(&bytes)
            .map(|v| extract_usage_from_body(&v))
            .unwrap_or_default();
        let insert = ctx.into_insert(
            usage,
            status_code,
            status.is_success(),
            started_at.elapsed().as_millis() as i64,
            None,
        );
        spawn_record(db, insert);
        return Response::from_parts(parts, Body::from(bytes));
    }

    // ── 流式：包装字节流 ──
    let (parts, body) = resp.into_parts();
    let scanner = std::sync::Arc::new(std::sync::Mutex::new(UsageSseScanner::new()));
    let scanner_for_stream = scanner.clone();

    let data_stream = body.into_data_stream().map(move |chunk| {
        if let Ok(ref bytes) = chunk {
            scanner_for_stream.lock().unwrap().push(bytes);
        }
        chunk
    });

    // 流耗尽时落库：追加一个末尾 future，消费完成后触发记录，且不产出额外字节。
    let tail_stream = futures_util::stream::once(async move {
        let usage = std::mem::take(&mut *scanner.lock().unwrap()).finish();
        let insert = ctx.into_insert(
            usage,
            status_code,
            status.is_success(),
            started_at.elapsed().as_millis() as i64,
            None,
        );
        spawn_record(db, insert);
    })
    .filter_map(|()| async move { None::<Result<axum::body::Bytes, axum::Error>> });

    let combined = data_stream.chain(tail_stream);
    Response::from_parts(parts, Body::from_stream(combined))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_openai_basic() {
        let u = extract_usage(&json!({
            "prompt_tokens": 100, "completion_tokens": 40, "total_tokens": 140
        }));
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 40);
        assert_eq!(u.total_tokens, 140);
        // 无缓存信息：全记为 miss
        assert_eq!(u.cache_hit_tokens, 0);
        assert_eq!(u.cache_miss_tokens, 100);
    }

    #[test]
    fn extract_deepseek_cache_fields() {
        let u = extract_usage(&json!({
            "prompt_tokens": 100,
            "prompt_cache_hit_tokens": 30,
            "prompt_cache_miss_tokens": 70,
            "completion_tokens": 20,
            "total_tokens": 120
        }));
        assert_eq!(u.cache_hit_tokens, 30);
        assert_eq!(u.cache_miss_tokens, 70);
        assert_eq!(u.cache_hit_tokens + u.cache_miss_tokens, u.prompt_tokens);
    }

    #[test]
    fn extract_openai_new_cached_tokens_details() {
        let u = extract_usage(&json!({
            "prompt_tokens": 200,
            "prompt_tokens_details": {"cached_tokens": 50},
            "completion_tokens": 10
        }));
        assert_eq!(u.cache_hit_tokens, 50);
        assert_eq!(u.cache_miss_tokens, 150);
    }

    #[test]
    fn extract_anthropic_native() {
        // input_tokens 是新增未缓存输入；cache_read + cache_creation 需加回总输入
        let u = extract_usage(&json!({
            "input_tokens": 10,
            "cache_read_input_tokens": 90,
            "cache_creation_input_tokens": 0,
            "output_tokens": 25
        }));
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.cache_hit_tokens, 90);
        assert_eq!(u.cache_miss_tokens, 10);
        assert_eq!(u.completion_tokens, 25);
        assert_eq!(u.total_tokens, 125);
    }

    #[test]
    fn extract_identity_holds_even_with_bad_fields() {
        // 上游给了自相矛盾的 hit>prompt，收敛到恒等式
        let u = extract_usage(&json!({
            "prompt_tokens": 50,
            "prompt_cache_hit_tokens": 999,
            "completion_tokens": 5
        }));
        assert_eq!(u.cache_hit_tokens + u.cache_miss_tokens, u.prompt_tokens);
        assert!(u.cache_hit_tokens <= u.prompt_tokens);
    }

    #[test]
    fn body_without_usage_is_zero() {
        let u = extract_usage_from_body(&json!({"id": "x", "choices": []}));
        assert!(u.is_empty());
    }

    #[test]
    fn scanner_openai_stream_usage_chunk() {
        let mut s = UsageSseScanner::new();
        s.push(b"data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        s.push(b"data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15}}\n\n");
        s.push(b"data: [DONE]\n\n");
        let u = s.finish();
        assert_eq!(u.prompt_tokens, 12);
        assert_eq!(u.completion_tokens, 3);
        assert_eq!(u.total_tokens, 15);
    }

    #[test]
    fn scanner_handles_split_chunk_boundary() {
        let mut s = UsageSseScanner::new();
        let line = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n";
        let bytes = line.as_bytes();
        let mid = bytes.len() / 2;
        s.push(&bytes[..mid]);
        s.push(&bytes[mid..]);
        let u = s.finish();
        assert_eq!(u.total_tokens, 9);
    }

    #[test]
    fn scanner_anthropic_native_stream() {
        let mut s = UsageSseScanner::new();
        s.push(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":90}}}\n\n");
        s.push(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":25}}\n\n");
        let u = s.finish();
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.cache_hit_tokens, 90);
        assert_eq!(u.completion_tokens, 25);
        assert_eq!(u.total_tokens, 125);
    }

    #[test]
    fn scanner_no_usage_returns_empty() {
        let mut s = UsageSseScanner::new();
        s.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        s.push(b"data: [DONE]\n\n");
        assert!(s.finish().is_empty());
    }
}
