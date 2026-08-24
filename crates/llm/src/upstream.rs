use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::StreamExt;
use reqwest::Client;

use super::ChatCompletionRequest;

/// 上游 HTTP 客户端配置（超时/连接池策略）。
///
/// [`LlmState`](crate::LlmState) 构造时按本配置构建 `reqwest::Client`；
/// 测试可用激进超时构造独立 client 注入，替代原全局 static 单例。
///
/// Timeout strategy:
/// - `connect_timeout`: 30 s — fast failure when the upstream is unreachable.
/// - `read_timeout`:   300 s (5 min) — detects genuinely hung connections while
///   allowing very large streaming responses (e.g. long tool-use / task-plan
///   generations) to complete without a premature "connection closed mid-response"
///   error.
/// - No global `timeout` — streaming LLM responses can legitimately take minutes;
///   the upstream provider enforces its own deadline.
///
/// HTTP/1.1 is forced (not HTTP/2) for two reasons:
/// 1. **read_timeout accuracy**: reqwest's `read_timeout` resets on every frame,
///    including HTTP/2 PING frames. Upstream providers that send h2 pings during
///    long generations would prevent the read timeout from ever firing, masking
///    a genuinely hung connection. HTTP/1.1 has no such pings.
/// 2. **Upstream compatibility**: Some LLM provider gateways have aggressive
///    idle timeouts (~120 s) on HTTP/2 connections. HTTP/1.1 with TCP keepalive
///    is more resilient to these middlebox timeouts.
#[derive(Debug, Clone)]
pub struct UpstreamClientConfig {
    pub connect_timeout: std::time::Duration,
    pub read_timeout: std::time::Duration,
    pub tcp_keepalive: std::time::Duration,
    pub pool_max_idle_per_host: usize,
}

impl Default for UpstreamClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: std::time::Duration::from_secs(30),
            read_timeout: std::time::Duration::from_mins(5),
            tcp_keepalive: std::time::Duration::from_mins(1),
            pool_max_idle_per_host: 10,
        }
    }
}

impl UpstreamClientConfig {
    /// 按配置构建 reqwest 客户端（连接池随 client 句柄共享，clone 廉价）。
    ///
    /// # Panics
    /// reqwest builder 内部 TLS/解析失败时 panic（与原全局 static 构建行为一致）。
    #[must_use]
    // builder 失败仅发生在 TLS 后端初始化等病态场景，与原 static 初始化的 expect 语义一致
    #[allow(clippy::expect_used)]
    pub fn build_client(&self) -> Client {
        Client::builder()
            .http1_only()
            .connect_timeout(self.connect_timeout)
            .read_timeout(self.read_timeout)
            .tcp_keepalive(self.tcp_keepalive)
            .pool_max_idle_per_host(self.pool_max_idle_per_host)
            .build()
            .expect("failed to build upstream HTTP client")
    }
}

/// 非流式上游响应体的统一大小上限（16MB）。
///
/// 三个消费点共用：
/// - `relay_upstream_body`：OpenAI/Anthropic 非流式直通的有界读取
/// - `openai_handler::rewrite_pseudo_tool_calls_in_response`：compat 非流式改写
/// - `format::convert_openai_to_anthropic_response`：Anthropic 非流式转换
///
/// 流式首事件守卫（`call_upstream_stream_guarded` 内的 `MAX_PREFIX_BYTES`，4MB）
/// 是另一个概念，不在此统一范围。
pub const MAX_UPSTREAM_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Strip potential secrets (API keys, tokens) from upstream error messages.
fn sanitize_error_message(body: &str) -> String {
    // Truncate at a valid UTF-8 character boundary (max 500 chars).
    let end = if body.len() <= 500 {
        body.len()
    } else {
        // Find the last complete char boundary at or before byte 500.
        // floor_char_boundary is stabilized in Rust 1.79+; we implement manually
        // to stay compatible.
        let mut boundary = 500;
        while boundary > 0 && !body.is_char_boundary(boundary) {
            boundary -= 1;
        }
        boundary
    };

    let truncated = if end < body.len() {
        format!("{}...", &body[..end])
    } else {
        body.to_string()
    };

    // Redact patterns (best-effort — not a security guarantee):
    // - `sk-<hex/alphanumeric>` → `sk-***`
    // - `Bearer <token>` (case-insensitive prefix) → `Bearer ***`
    //
    // We walk bytes and do manual substring matching to avoid pulling in `regex`.
    let bytes = truncated.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut pos = 0;

    while pos < n {
        // ── Check for `Bearer ` / `bearer ` ──
        let remaining = &truncated[pos..];
        if remaining.len() > 7 {
            let lower = remaining[..7].to_ascii_lowercase();
            if lower == "bearer " {
                // Skip the "Bearer " / "bearer " prefix.
                pos += 7;
                // Skip the token.
                while pos < n && !bytes[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                out.push_str("Bearer ***");
                continue;
            }
        }

        // ── Check for `sk-` followed by alphanumeric ──
        if pos + 3 <= n && bytes[pos] == b's' && bytes[pos + 1] == b'k' && bytes[pos + 2] == b'-' {
            let mut key_end = pos + 3;
            while key_end < n && bytes[key_end].is_ascii_alphanumeric() && key_end - pos <= 67 {
                key_end += 1;
            }
            if key_end > pos + 3 {
                out.push_str("sk-***");
                pos = key_end;
                continue;
            }
        }

        // ── Regular character ──
        let ch = truncated[pos..].chars().next().unwrap();
        out.push(ch);
        pos += ch.len_utf8();
    }

    out
}

/// 构造发往上游的 OpenAI 请求体。
///
/// 两种模式：
/// - 透传（`raw_body` 有值）：以原始请求体为基底，只覆盖网关必须改写的字段
///   （model 别名 → 真实模型名、stream 恒为显式布尔、流式注入 stream_options.include_usage），
///   其余参数原样上行。messages 保留客户端原样（含多模态 content 数组）。
/// - 重建（`raw_body` 为 None，anthropic 转换路径）：从 request 逐字段拼回，
///   可选字段仅在有值时挂上（避免部分上游对 null 敏感），
///   messages 用带 skip_serializing_if 的 ChatMessage 序列化。
///
/// 独立成公共函数是为了让调用方在发送前拿到完整请求体写日志
/// （`log_llm_request` 记录的就是这个 body，与实际发送内容逐字节一致）。
pub fn build_upstream_body(request: &ChatCompletionRequest) -> serde_json::Value {
    // 透传模式：以原始请求体为基底，定点覆盖网关必须改写的字段。
    if let Some(mut raw) = request.raw_body.clone() {
        // raw_body 恒为对象：openai handler 先 400 校验 model/messages 后才构造请求，
        // anthropic_to_openai 也构造对象 passthrough。
        debug_assert!(raw.is_object(), "raw_body 应为 JSON 对象");
        raw["model"] = request.model.clone().into(); // 别名 → 真实模型名
                                                     // stream 必须始终与网关决策一致：客户端省略 stream（或传非布尔值）时，
                                                     // 也保证上游收到显式布尔值，避免透传模式下 stream 变成 null。
                                                     // request.stream 与客户端显式值同源（as_bool().unwrap_or(false)），
                                                     // 显式提供时覆盖为同值（no-op），与重建模式旧行为保持一致。
        raw["stream"] = request.stream.into();
        if request.stream {
            // 幂等注入 include_usage：保留客户端已有 stream_options 字段。
            let so = raw
                .as_object_mut()
                .and_then(|o| o.get_mut("stream_options"))
                .and_then(|v| v.as_object_mut());
            if let Some(so) = so {
                so.insert("include_usage".into(), serde_json::Value::Bool(true));
            } else {
                raw["stream_options"] = serde_json::json!({ "include_usage": true });
            }
        }
        return raw;
    }
    // 重建模式：anthropic 转换路径（raw_body 为 None）。
    let mut req_body = serde_json::json!({
        "model": request.model,
        "messages": request.messages,
        "stream": request.stream,
    });
    if let Some(v) = request.max_tokens {
        req_body["max_tokens"] = v.into();
    }
    if let Some(v) = request.temperature {
        req_body["temperature"] = v.into();
    }
    if let Some(v) = request.top_p {
        req_body["top_p"] = v.into();
    }
    if let Some(tools) = &request.tools {
        req_body["tools"] = serde_json::Value::Array(tools.clone());
    }
    if let Some(choice) = &request.tool_choice {
        req_body["tool_choice"] = choice.clone();
    }
    if request.stream {
        req_body["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    req_body
}

/// Call an upstream LLM provider with OpenAI-compatible format.
/// Supports both streaming (SSE) and non-streaming modes.
pub async fn call_upstream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: &ChatCompletionRequest,
) -> Result<Response, (StatusCode, String)> {
    let req_body = build_upstream_body(request);
    call_upstream_with_body(client, base_url, api_key, &req_body, "v1/chat/completions").await
}

/// 用已构造好的请求体调用上游。
///
/// `path` 为上游 API 路径（如 `"v1/chat/completions"` 或 `"v1/responses"`）。
/// 调用方（handler）先用 `build_upstream_body` 构造 body、写入完整请求日志，
/// 再走这里发送——保证日志内容与实际发送的请求体一致。
pub async fn call_upstream_with_body(
    client: &Client,
    base_url: &str,
    api_key: &str,
    req_body: &serde_json::Value,
    path: &str,
) -> Result<Response, (StatusCode, String)> {
    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );

    let req = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(req_body);

    let resp = req.send().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Upstream connection failed: {}", e),
        )
    })?;

    let status = resp.status();

    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        // Sanitize: strip potential API key from error message
        let sanitized = sanitize_error_message(&body_text);
        // 诊断日志：上游 4xx/5xx 时记录转换后的请求体摘要，便于定位字段兼容问题。
        // 注意脱敏：messages 只记录结构（role/长度/工具字段），不记录正文内容。
        let req_debug = summarize_request_for_log(req_body);
        tracing::warn!(
            target: "llm_upstream",
            status = status.as_u16(),
            url = %url,
            request = %req_debug,
            upstream_error = %sanitized,
            "LLM upstream rejected request"
        );

        // 增强诊断：记录完整请求体（截断到 8KB）到系统日志，用于对比子代理/主代理差异。
        // 脱敏：移除 Authorization 头，但保留请求体的完整内容（包含 messages/tools）。
        let full_req = serde_json::to_string_pretty(req_body).unwrap_or_default();
        let truncated_req = if full_req.len() > 8192 {
            format!(
                "{}...\n[truncated, total {} bytes]",
                &full_req[..8192],
                full_req.len()
            )
        } else {
            full_req
        };
        tracing::warn!(
            target: "llm_upstream_debug",
            status = status.as_u16(),
            model = %req_body.get("model").and_then(|m| m.as_str()).unwrap_or(""),
            stream = req_body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
            message_count = req_body.get("messages").and_then(|m| m.as_array()).map_or(0, Vec::len),
            has_tools = req_body.get("tools").is_some(),
            full_request_body = %truncated_req,
            upstream_error_full = %sanitized,
            "LLM upstream 4xx/5xx - full request dump"
        );

        return Err((
            status,
            format!("Upstream error {}: {}", status.as_u16(), sanitized),
        ));
    }

    let is_stream = req_body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if is_stream {
        relay_upstream_stream(resp).await
    } else {
        relay_upstream_body(resp).await
    }
}

/// Relay a streaming (SSE) upstream response to the client.
///
/// 当上游连接意外断开（如 LLM 服务商网关 idle timeout），向客户端发送一个
/// OpenAI 风格的 SSE error chunk 再正常关闭流，而不是让 hyper 在中途截断响应
/// （客户端收到 "Connection closed mid-response"）。
async fn relay_upstream_stream(resp: reqwest::Response) -> Result<Response, (StatusCode, String)> {
    let byte_stream = resp.bytes_stream().map(|result| {
        result
            .map(|bytes| bytes.to_vec())
            .map_err(|e| std::io::Error::other(e.to_string()))
    });

    let body = Body::from_stream(byte_stream);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap())
}

/// Relay a non-streaming upstream response body to the client.
///
/// 有界读取：`bytes_stream()` 循环累加，累计超过 [`MAX_UPSTREAM_BODY_BYTES`]
/// 即返回 502，防止恶意/失控上游用无限 body 拖垮服务器内存。
async fn relay_upstream_body(resp: reqwest::Response) -> Result<Response, (StatusCode, String)> {
    let mut body_bytes: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to read upstream response: {}", e),
            )
        })?;
        body_bytes.extend_from_slice(&chunk);
        if body_bytes.len() > MAX_UPSTREAM_BODY_BYTES {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!(
                    "Upstream response exceeded {} byte limit",
                    MAX_UPSTREAM_BODY_BYTES
                ),
            ));
        }
    }

    let body = Body::from(body_bytes);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(body)
        .unwrap())
}

/// 非流式响应体的质量校验：读 body → 校验非空 + 合法 JSON → 重建 Response。
///
/// `relay_upstream_body` 对 200 空 body 或 200 非 JSON body 返回 Ok（无法区分），
/// 本函数在 body 层面做二次校验，把"假成功"识别为 502 供故障转移/原地重试：
/// - 空 body → Err(502, "empty response body ...")
/// - 非 JSON → Err(502, "malformed response body ...")
/// - 合法 JSON → Ok(重建的 Response)
///
/// 入参为 axum `Response`（而非 reqwest `Response`），因此同时适用于
/// `relay_upstream_body` 产物与 Anthropic 直通产物，调用方无需关心来源。
async fn validate_response_body(resp: Response) -> Result<Response, (StatusCode, String)> {
    let body_bytes = axum::body::to_bytes(resp.into_body(), MAX_UPSTREAM_BODY_BYTES)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("failed to read upstream body: {e}"),
            )
        })?;
    if body_bytes.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            "empty response body from upstream".into(),
        ));
    }
    if serde_json::from_slice::<serde_json::Value>(&body_bytes).is_err() {
        return Err((
            StatusCode::BAD_GATEWAY,
            "malformed response body (not valid JSON)".into(),
        ));
    }
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(body_bytes))
        .unwrap())
}

/// 判定错误消息是否为"上游 200 但空/畸形响应"类（可原地重试 + 可转移）。
///
/// 覆盖四种来源：
/// - `empty response body` / `malformed response body`：非流式 body 质量校验（本文件）
/// - `empty SSE stream`：流式首字节守卫的空流（`call_upstream_stream_guarded`）
/// - `invalid responses-format upstream body`：Responses 非流式转换的 parse 失败
///   （`super::responses::convert_responses_to_chat_response`）
fn is_malformed_error(msg: &str) -> bool {
    msg.contains("empty response body")
        || msg.contains("malformed response body")
        || msg.contains("empty SSE stream")
        || msg.contains("invalid responses-format upstream body")
}

/// 判定上游失败是否可转移（换下一个候选重试）。
///
/// 可转移：5xx（含连接/超时映射来的 502）、429（受 `failover_on_429` 开关控制）。
/// 不可转移：其余 4xx（请求本身问题，换模型大概率同样失败）。
pub fn is_retryable(status: StatusCode, failover_on_429: bool) -> bool {
    if status.is_server_error() {
        return true;
    }
    status == StatusCode::TOO_MANY_REQUESTS && failover_on_429
}

/// 从 provider `extra_config` JSON 读 `failover_on_429` 开关（默认 true）。
pub fn failover_on_429_enabled(extra_config: Option<&str>) -> bool {
    let Some(ec) = extra_config else { return true };
    serde_json::from_str::<serde_json::Value>(ec)
        .ok()
        .and_then(|v| v.get("failover_on_429")?.as_bool())
        .unwrap_or(true)
}

/// 定点改写请求体的 model 字段（重试循环内用，其他字段不动）。
pub fn set_body_model(body: &mut serde_json::Value, model: &str) {
    body["model"] = serde_json::Value::String(model.to_string());
}

/// 流式上游调用的首字节守卫。
///
/// `path` 为上游 API 路径（如 `"v1/chat/completions"` 或 `"v1/responses"`）。
/// 与 `call_upstream_with_body` 的差别：拿到 2xx 响应后不直接 relay，
/// 而是先缓冲到第一个 SSE `data:` 事件（30s 首字节超时），成功再把
/// "已缓冲前缀 + 剩余流"拼成响应体返回；失败按 `(status, msg)` 返回，
/// 供外层故障转移循环判定。
///
/// 适用场景：模型组候选链——确保客户端收到首字节前可以换候选重发。
pub async fn call_upstream_stream_guarded(
    client: &Client,
    base_url: &str,
    api_key: &str,
    req_body: &serde_json::Value,
    path: &str,
) -> Result<Response, (StatusCode, String)> {
    use futures_util::StreamExt;

    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(req_body)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Upstream connection failed: {}", e),
            )
        })?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        let sanitized = sanitize_error_message(&body_text);
        return Err((
            status,
            format!("Upstream error {}: {}", status.as_u16(), sanitized),
        ));
    }

    /// 首事件缓冲上限：SSE 首事件通常 <64KB；超限视为非 SSE/恶意流，走转移。
    const MAX_PREFIX_BYTES: usize = 4 * 1024 * 1024;

    // 缓冲到第一个 SSE data 事件（含跨 chunk 到达的情况），30s 首字节超时。
    let mut stream = resp.bytes_stream();
    let mut prefix: Vec<u8> = Vec::new();
    let first_event_deadline = std::time::Duration::from_secs(30);
    let collect = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream stream read failed: {}", e),
                )
            })?;
            prefix.extend_from_slice(&chunk);
            if prefix.len() > MAX_PREFIX_BYTES {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    "Upstream prefix exceeded limit (not an SSE stream?)".to_string(),
                ));
            }
            // 上游 SSE 事件结尾：\n\n（OpenAI 标准）或 \r\n\r\n（CRLF 风格）
            if prefix.windows(4).any(|w| w == b"\r\n\r\n")
                || prefix.windows(2).any(|w| w == b"\n\n")
            {
                return Ok(());
            }
        }
        // 流正常结束但没等到任何 SSE 事件——空流不放行，返回 502 供故障转移。
        // 上游立刻 [DONE] 的边界场景：首 chunk 通常包含 [DONE] 事件（含 \n\n），
        // 会在上面的 windows 检查中命中放行；真正到达这里的空流是异常的。
        if prefix.is_empty() {
            return Err((
                StatusCode::BAD_GATEWAY,
                "empty SSE stream from upstream (no events before EOF)".to_string(),
            ));
        }
        Ok(())
    };
    tokio::time::timeout(first_event_deadline, collect)
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                "Upstream first-byte timeout".to_string(),
            )
        })??;

    // 拼"前缀 replay + 剩余流"的响应体
    let prefix_stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(prefix) });
    let rest_stream = stream.map(|r| {
        r.map(|b| b.to_vec())
            .map_err(|e| std::io::Error::other(e.to_string()))
    });
    let body = Body::from_stream(prefix_stream.chain(rest_stream));
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap())
}

/// 候选链执行循环的结果。
///
/// `Success` 同时携带完整的 `Response` 与出账 `Candidate`，体积较大；
/// 接口按 Task 6 brief 约定（非 Box），仅在成功分支 move 一次，非热点，故 allow。
#[allow(clippy::large_enum_variant)]
pub enum FailoverOutcome {
    /// 某候选成功。
    Success {
        /// 上游响应（流式为首字节守卫后的拼接体）。
        resp: Response,
        /// 实际出账的候选（usage 落库以此为准）。
        candidate: crate::router::Candidate,
        /// 是否发生过转移（首选不是出账候选）。
        failed_over: bool,
        /// 本次成功调用是否走了 Anthropic 直通（`call_upstream_raw` /v1/messages）。
        /// 直通成功 → true（响应已是 Anthropic 格式，调用方跳过二次格式转换）；
        /// 转换路径（ChatCompletions/Responses）成功 → false。
        upstream_anthropic: bool,
    },
    /// 全部候选失败：返回最后一个被尝试候选的错误。
    Exhausted {
        /// HTTP 状态码（全熔断无尝试时为 503）。
        status: StatusCode,
        /// 错误消息。
        message: String,
        /// 是否尝试过非首选候选。
        failed_over: bool,
    },
}

/// 判定失败状态码是否为"确定性失败"（换多少次重试都必然失败），并给出失败类别：
/// - 401/403：认证/权限失败 → provider 级（key 失效/被禁，该 provider 全部模型同因失败）
/// - 404：模型级（上游不存在该模型）
///
/// 其余 4xx（400/422…）**不**纳入：它们可能是请求内容相关（上下文超限、内容策略拒绝），
/// 与具体请求强相关，转移到别的模型/提供商未必失败，缓存会导致错误地阻断后续请求。
fn deterministic_failure(status: StatusCode) -> Option<crate::down::FailureKind> {
    match status.as_u16() {
        401 | 403 => Some(crate::down::FailureKind::ProviderAuth),
        404 => Some(crate::down::FailureKind::Model),
        _ => None,
    }
}

/// 在候选链上执行上游调用：熔断跳过 + 确定性失败跳过 + 可转移重试。
///
/// 调用方负责 RAG/compat 改写与 `req_body` 构造；本函数循环内仅
/// clone body + 定点改 `model`。中途失败尝试不记 usage（仅 warn 日志）。
///
/// "假成功"识别：HTTP 200 但 body 为空 / 非 JSON（非流式），或 SSE 流
/// 在首个事件前 EOF（链长 >1 的流式守卫）——统一归为 502 畸形类失败，
/// 每候选原地重试最多 1 次后按 retryable 转移到下一候选。
///
/// `anthropic_body`：Anthropic 入口携带的原始请求体（OpenAI/Responses 入口为 None）。
/// 当候选 provider 配了 `anthropic_base_url` 且 `anthropic_body` 存在时，该候选走
/// `/v1/messages` 直通（原始 Anthropic body，model 替换为候选名）；否则走下方
/// ChatCompletions / Responses 转换分支。直通失败的处理（retryable/确定性/4xx）
/// 与转换分支完全一致——组内可混合直通与转换候选并互相故障转移。
pub async fn execute_with_failover(
    client: &Client,
    breakers: &crate::breaker::ModelBreakers,
    known: &crate::down::KnownFailures,
    chain: &crate::router::CandidateChain,
    req_body: &serde_json::Value,
    stream: bool,
    anthropic_body: Option<&serde_json::Value>,
) -> FailoverOutcome {
    let mut attempts = 0usize;
    let mut last_err: Option<(StatusCode, String)> = None;
    let mut last_attempted: Option<&str> = None; // 实际发起过上游调用的候选 model_id

    for cand in &chain.candidates {
        // 确定性失败跳过：TTL 内的 401/403/404 → 不发起网络调用，
        // 单模型链秒回缓存错误，多候选链跳过死候选继续尝试健康备选。
        let known_auth = format!("p:{}", cand.provider.id);
        let known_model = format!("m:{}", cand.model_id);
        if let Some(info) = known
            .lookup(&known_auth)
            .or_else(|| known.lookup(&known_model))
        {
            tracing::debug!(
                model_id = %cand.model_id,
                model = %cand.model_name,
                status = info.status,
                "LLM failover: candidate skipped (known deterministic failure)"
            );
            last_err = Some((
                StatusCode::from_u16(info.status).unwrap_or(StatusCode::BAD_GATEWAY),
                info.message,
            ));
            continue;
        }

        if !breakers.allow(&cand.model_id) {
            tracing::debug!(
                model_id = %cand.model_id,
                model = %cand.model_name,
                "LLM failover: candidate skipped (circuit open)"
            );
            continue;
        }
        // 每候选最多 1 + MAX_MALFORMED_RETRIES 次尝试：首次 + 原地重试
        // （仅针对"上游 200 但空/畸形响应"类 502——空 body / 非 JSON body /
        // 空 SSE 流 / Responses 转换 parse 失败）。这类失败穿透状态码判定
        // （HTTP 200），但 body 校验识别后属于可重试的上游抖动。
        let mut cand_attempt = 0usize;
        const MAX_MALFORMED_RETRIES: usize = 1; // 首次之外的额外重试次数

        let (result, upstream_anthropic) = loop {
            cand_attempt += 1;
            attempts += 1;
            last_attempted = Some(cand.model_id.as_str());

            let mut body = req_body.clone();
            set_body_model(&mut body, &cand.model_name);

            // ── Anthropic 直通分支：候选 provider 配了 anthropic_base_url 且入口携带原始
            //    Anthropic body 时，原始请求体（仅替换 model 为候选真实名）直发 /v1/messages。
            //    每候选独立判定：组内可混合"直通候选"与"转换候选"并互相故障转移。
            //    失败路径与转换分支共用（retryable / 确定性 4xx / 不可转移 4xx 不区分直通与否）。
            let (r, ua) = if let (Some(url), Some(raw)) =
                (&cand.provider.anthropic_base_url, anthropic_body)
            {
                let mut raw = raw.clone();
                set_body_model(&mut raw, &cand.model_name);
                (
                    call_upstream_raw(client, url, &cand.provider.api_key, "/v1/messages", &raw, stream)
                        .await,
                    true,
                )
            } else {
                // 按候选协议分支：ChatCompletions 走标准路径，Responses 先转换请求体再转换响应。
                let result = match cand.upstream_protocol {
                    crate::router::UpstreamProtocol::ChatCompletions => {
                        // 首字节守卫仅用于有转移目标（链长 >1）的流式请求；
                        // 单元素链流式回到 relay 直通（响应头即放行），与改造前行为完全一致，
                        // 避免首 token 延迟 >30s 的合法请求（超长 prefill、推理模型）被守卫 504 打断。
                        if stream && chain.candidates.len() > 1 {
                            call_upstream_stream_guarded(
                                client,
                                &cand.provider.base_url,
                                &cand.provider.api_key,
                                &body,
                                "v1/chat/completions",
                            )
                            .await
                        } else {
                            call_upstream_with_body(
                                client,
                                &cand.provider.base_url,
                                &cand.provider.api_key,
                                &body,
                                "v1/chat/completions",
                            )
                            .await
                        }
                    }
                    crate::router::UpstreamProtocol::Responses => {
                        // 转换请求体：chat → Responses 格式
                        body = super::responses::chat_body_to_responses_body(&body);
                        // 首字节守卫仅用于有转移目标（链长 >1）的流式请求
                        let upstream_resp = if stream && chain.candidates.len() > 1 {
                            call_upstream_stream_guarded(
                                client,
                                &cand.provider.base_url,
                                &cand.provider.api_key,
                                &body,
                                "v1/responses",
                            )
                            .await
                        } else {
                            call_upstream_with_body(
                                client,
                                &cand.provider.base_url,
                                &cand.provider.api_key,
                                &body,
                                "v1/responses",
                            )
                            .await
                        };
                        // 响应转换：Responses → Chat Completions 格式
                        match upstream_resp {
                            Ok(resp) => {
                                if stream {
                                    super::responses::convert_responses_stream_to_chat(resp)
                                } else {
                                    super::responses::convert_responses_to_chat_response(resp)
                                        .await
                                }
                            }
                            Err(e) => Err(e),
                        }
                    }
                };
                (result, false)
            };

            // 非流式：对 Ok(resp) 做 body 质量校验——200 空 body / 200 非 JSON
            // 这类"假成功"在状态码层面无法识别，统一归一为 Err(502) 走下方
            // 重试/转移判定。流式路径无法回读 body（响应头已发客户端），跳过。
            let r = if !stream {
                match r {
                    Ok(resp) => validate_response_body(resp).await,
                    Err(e) => Err(e),
                }
            } else {
                r
            };

            // 畸形/空流类失败：同候选原地重试（上限 MAX_MALFORMED_RETRIES 次）。
            let is_malformed = matches!(&r, Err((s, m))
                if *s == StatusCode::BAD_GATEWAY && is_malformed_error(m));
            if is_malformed && cand_attempt <= MAX_MALFORMED_RETRIES {
                tracing::warn!(
                    model_id = %cand.model_id,
                    model = %cand.model_name,
                    attempt = cand_attempt,
                    "LLM upstream returned malformed/empty response, retrying same candidate"
                );
                continue;
            }
            break (r, ua);
        };

        match result {
            Ok(resp) => {
                breakers.record_success(&cand.model_id);
                // 按 model_id 判定：组内允许"同 model_name 不同 provider/model_id"冗余候选，
                // 真实 provider 转移必须计为 failed_over（usage 出账归因）。
                let failed_over = chain
                    .candidates
                    .first()
                    .is_some_and(|first| first.model_id != cand.model_id);
                return FailoverOutcome::Success {
                    resp,
                    candidate: cand.clone(),
                    failed_over,
                    upstream_anthropic,
                };
            }
            Err((status, msg)) => {
                let retryable = is_retryable(
                    status,
                    failover_on_429_enabled(cand.provider.extra_config.as_deref()),
                );
                if retryable {
                    tracing::warn!(
                        group = ?chain.group_name,
                        from_model = %cand.model_name,
                        status = status.as_u16(),
                        reason = %msg,
                        "LLM failover: candidate failed, trying next"
                    );
                    breakers.record_failure(&cand.model_id);
                    last_err = Some((status, msg));
                    continue;
                }
                // 确定性失败（401/403/404）：记录到 known-failures 缓存，
                // 并继续尝试后续候选（组场景：dead key 不应再阻塞请求，应转移）。
                if let Some(kind) = deterministic_failure(status) {
                    let key = match kind {
                        crate::down::FailureKind::ProviderAuth => {
                            format!("p:{}", cand.provider.id)
                        }
                        crate::down::FailureKind::Model => format!("m:{}", cand.model_id),
                    };
                    known.record(&key, kind, status.as_u16(), &msg);
                    tracing::warn!(
                        group = ?chain.group_name,
                        from_model = %cand.model_name,
                        status = status.as_u16(),
                        "LLM failover: upstream deterministic failure recorded, trying next"
                    );
                    last_err = Some((status, msg));
                    continue;
                }
                // 不可转移（其余 4xx）：上游可达，计成功；立即终止
                breakers.record_success(&cand.model_id);
                return FailoverOutcome::Exhausted {
                    status,
                    message: msg,
                    // 与 Success 分支同源判定：实际尝试的最后一个候选非首选即转移
                    // （如 A retryable 失败后 B 返回 4xx → last_attempted=B≠首选 → true）
                    failed_over: last_attempted.is_some_and(|mid| {
                        Some(mid) != chain.candidates.first().map(|c| c.model_id.as_str())
                    }),
                };
            }
        }
    }

    match last_err {
        Some((status, message)) => FailoverOutcome::Exhausted {
            status,
            message,
            // 与 Success 分支同源判定：实际尝试的最后一个候选非首选即转移。
            // 首选被熔断跳过（allow=false 不计 attempts）+ 备选失败也计转移，
            // 避免 usage 把 model_name 记为从未尝试的首选、failover_from 归因错误。
            failed_over: last_attempted.is_some_and(|mid| {
                Some(mid) != chain.candidates.first().map(|c| c.model_id.as_str())
            }),
        },
        None => FailoverOutcome::Exhausted {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: if attempts > 0 {
                "all candidates failed".to_string()
            } else {
                "all_candidates_unavailable".to_string()
            },
            failed_over: false,
        },
    }
}

/// 透传原始请求到上游 Anthropic 端点，不做格式转换。
///
/// 认证策略：同时支持 `x-api-key`（Anthropic 原生）和 `Authorization: Bearer`（OpenAI 风格）。
/// 先尝试 `x-api-key`，若返回 401 则回退到 Bearer 头重试。
pub async fn call_upstream_raw(
    client: &Client,
    base_url: &str,
    api_key: &str,
    path: &str,
    body: &serde_json::Value,
    is_stream: bool,
) -> Result<Response, (StatusCode, String)> {
    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );

    // 先用 x-api-key 头尝试
    let resp = client
        .post(&url)
        .header("x-api-key", api_key)
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Upstream connection failed: {}", e),
            )
        })?;

    // 如果 401，回退到 Bearer 头重试
    let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream connection failed: {}", e),
                )
            })?
    } else {
        resp
    };

    let status = resp.status();

    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        let sanitized = sanitize_error_message(&body_text);
        return Err((
            status,
            format!("Upstream error {}: {}", status.as_u16(), sanitized),
        ));
    }

    if is_stream {
        relay_upstream_stream(resp).await
    } else {
        relay_upstream_body(resp).await
    }
}

/// 生成请求体的结构摘要用于诊断日志：保留字段名和值类型/长度，
/// 但 messages 的正文、tools 的 description/parameters 不落地（避免泄露对话内容）。
fn summarize_request_for_log(req_body: &serde_json::Value) -> String {
    use serde_json::json;

    let mut summary = serde_json::Map::new();
    for (k, v) in req_body.as_object().into_iter().flatten() {
        match k.as_str() {
            "messages" => {
                let msgs: Vec<serde_json::Value> = v
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|m| {
                                let role = m
                                    .get("role")
                                    .and_then(|r| r.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                let content_len = m
                                    .get("content")
                                    .and_then(|c| c.as_str())
                                    .map(str::len)
                                    .unwrap_or(0);
                                let has_tool_calls = m.get("tool_calls").is_some();
                                let tool_call_id = m.get("tool_call_id").is_some();
                                json!({
                                    "role": role,
                                    "content_len": content_len,
                                    "tool_calls": has_tool_calls,
                                    "tool_call_id": tool_call_id,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                summary.insert(k.clone(), json!(msgs));
            }
            "tools" => {
                // 只记录工具名，不记录 description/parameters
                let names: Vec<&str> = v
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| {
                                t.get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(|n| n.as_str())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                summary.insert(k.clone(), json!(names));
            }
            _ => {
                summary.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::Value::Object(summary).to_string()
}

/// Build an OpenAI-format error response.
pub fn error_response(status: StatusCode, message: String, error_type: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": error_type,
        }
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Build an Anthropic-format error response for Anthropic-protocol domains.
pub fn error_response_anthropic(status: StatusCode, message: String, error_type: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message,
        }
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Build a 404 "model not found" response that carries the available model list,
/// per spec: 未匹配 → 返回 404，body 中包含可用模型列表。
pub fn model_not_found_response(message: String, available_models: Vec<String>) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "available_models": available_models,
        }
    });
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 共享空知道失败缓存：既有用例不产生确定性失败记录，静态共享无害。
    static EMPTY_KNOWN: std::sync::LazyLock<crate::down::KnownFailures> =
        std::sync::LazyLock::new(crate::down::KnownFailures::new);

    /// 测试用上游 client（每用例独立构建，与生产共享连接池无交集）。
    fn test_client() -> Client {
        UpstreamClientConfig::default().build_client()
    }

    #[test]
    fn test_error_response_contains_openai_format() {
        let resp = error_response(
            StatusCode::UNAUTHORIZED,
            "Invalid key".into(),
            "authentication_error",
        );
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_error_response_anthropic_format() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = error_response_anthropic(
            StatusCode::NOT_FOUND,
            "Not found".into(),
            "invalid_request_error",
        );
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body =
            rt.block_on(async { axum::body::to_bytes(resp.into_body(), 1024).await.unwrap() });
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["type"], "error"); // Anthropic top-level type
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["message"], "Not found");
    }

    #[test]
    fn test_call_upstream_passthrough_status_code() {
        // Test that upstream errors pass through the original status code
        // rather than always returning 502 Bad Gateway
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = test_client();
            // Call a URL that returns 404
            let result = call_upstream(
                &client,
                "http://127.0.0.1:1", // non-existent server
                "test-key",
                &ChatCompletionRequest {
                    model: "test".into(),
                    messages: vec![],
                    stream: false,
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    tools: None,
                    tool_choice: None,
                    raw_body: None,
                },
            )
            .await;
            assert!(result.is_err());
            let (status, msg) = result.unwrap_err();
            // Connection refused should be 502
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            // Error message should NOT contain the API key
            assert!(
                !msg.contains("test-key"),
                "API key should not be in error message: {}",
                msg
            );
        });
    }

    #[test]
    fn test_error_message_no_api_key_leak() {
        // Just validate the pattern — error messages about upstream errors
        // should be generic and not echo back the API key
        let msg = "Upstream connection failed: connection refused";
        // Generic error messages should not contain sk- patterns
        assert!(!msg.contains("sk-"));
    }

    // ── sanitize_error_message tests ────────────────────────────

    #[test]
    fn sanitize_short_message_passes_through() {
        let input = "A simple error message";
        let result = sanitize_error_message(input);
        assert_eq!(result, input);
    }

    #[test]
    fn sanitize_truncates_long_message() {
        let long = "x".repeat(1000);
        let result = sanitize_error_message(&long);
        assert!(result.len() <= 510); // 500 chars + "..." + some overhead
        assert!(result.ends_with("..."));
    }

    #[test]
    fn sanitize_truncation_is_utf8_safe() {
        // Build a string where byte 500 falls inside a multi-byte character
        let mut s = "a".repeat(499);
        s.push('\u{4E2D}'); // 3-byte char at position 499
        s.push_str("end");
        let result = sanitize_error_message(&s);
        // Must not panic and must produce valid UTF-8
        assert!(result.ends_with("..."));
        // The 3-byte char at position 499 should either be fully included or excluded
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn sanitize_redacts_sk_key() {
        let input = "Error: invalid api key sk-abc123def456 for request";
        let result = sanitize_error_message(input);
        assert!(!result.contains("sk-abc123def456"));
        assert!(result.contains("sk-***"));
    }

    #[test]
    fn sanitize_redacts_bearer_token() {
        let input = "Unauthorized: Bearer eyJhbGciOiJIUzI1NiJ9.token.payload";
        let result = sanitize_error_message(input);
        assert!(!result.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(result.contains("Bearer ***"));
    }

    #[test]
    fn sanitize_redacts_lowercase_bearer() {
        let input = "unauthorized: bearer abcdefgh12345678";
        let result = sanitize_error_message(input);
        assert!(!result.contains("abcdefgh12345678"));
        assert!(result.contains("Bearer ***"));
    }

    #[test]
    fn sanitize_handles_multiple_keys() {
        let input = "Key sk-aaa111bbb222 and Bearer token123456789 for endpoint";
        let result = sanitize_error_message(input);
        assert!(!result.contains("sk-aaa111bbb222"));
        assert!(!result.contains("token123456789"));
        assert_eq!(
            result.matches("sk-***").count(),
            1,
            "should have one sk-***"
        );
        assert_eq!(
            result.matches("Bearer ***").count(),
            1,
            "should have one Bearer ***"
        );
    }

    #[test]
    fn sanitize_preserves_non_secret_content() {
        let input = "HTTP 502 Bad Gateway: upstream server returned error";
        let result = sanitize_error_message(input);
        assert!(result.contains("Bad Gateway"));
        assert!(result.contains("upstream server"));
    }

    #[test]
    fn sanitize_does_not_false_positive_on_unrelated_text() {
        // Text without any "sk-" or "Bearer" patterns should pass through unchanged.
        let input = "HTTP 502: task description processing failed";
        let result = sanitize_error_message(input);
        assert!(result.contains("task description"));
        assert!(result.contains("processing failed"));
    }

    #[test]
    fn sanitize_sk_without_alphanumeric_suffix_not_redacted() {
        // "sk-" without alphanumeric followers should not be redacted
        let input = "prefix sk- suffix";
        let result = sanitize_error_message(input);
        // "sk-" followed by a space (not alphanumeric) => not redacted
        assert!(result.contains("sk-"));
    }

    #[test]
    fn sanitize_empty_string_ok() {
        assert_eq!(sanitize_error_message(""), "");
    }

    #[test]
    fn sanitize_exactly_500_chars_not_truncated() {
        let s = "x".repeat(500);
        let result = sanitize_error_message(&s);
        assert!(!result.ends_with("..."));
        assert_eq!(result.len(), 500);
    }

    #[test]
    fn build_upstream_body_passthrough_unknown_params() {
        use crate::ChatCompletionRequest;
        use crate::ChatMessage;
        let raw = serde_json::json!({
            "model": "client-alias",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false,
            "stop": ["\n\n"],
            "seed": 42,
            "user": "abc",
            "temperature": 0.7,
        });
        let req = ChatCompletionRequest {
            model: "real-model".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: false,
            max_tokens: None,
            temperature: Some(0.7),
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = build_upstream_body(&req);
        // 未知参数原样保留
        assert_eq!(body["stop"], serde_json::json!(["\n\n"]));
        assert_eq!(body["seed"], 42);
        assert_eq!(body["user"], "abc");
        // model 被别名解析覆盖
        assert_eq!(body["model"], "real-model");
        // 非流式不注入 stream_options
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn build_upstream_body_passthrough_omitted_stream_defaults_to_false() {
        use crate::ChatCompletionRequest;
        use crate::ChatMessage;
        // 客户端省略 stream 字段：透传模式也必须发出显式布尔 false
        // （回归：此前透传原样返回 → stream 变 null，破坏非流式上游语义）。
        let raw = serde_json::json!({
            "model": "client-alias",
            "messages": [{"role": "user", "content": "hi"}],
            "seed": 42,
        });
        let req = ChatCompletionRequest {
            model: "real-model".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: false,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = build_upstream_body(&req);
        assert_eq!(body["stream"], false);
        // 未知参数仍原样保留
        assert_eq!(body["seed"], 42);
        // 非流式不注入 stream_options
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn build_upstream_body_stream_injects_include_usage_keeps_client_fields() {
        use crate::ChatCompletionRequest;
        use crate::ChatMessage;
        let raw = serde_json::json!({
            "model": "alias",
            "messages": [],
            "stream": true,
            "stream_options": {"max_tokens": 100},
        });
        let req = ChatCompletionRequest {
            model: "real".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: true,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = build_upstream_body(&req);
        let so = body["stream_options"].as_object().unwrap();
        // 客户端已有字段保留
        assert_eq!(so["max_tokens"], 100);
        // include_usage 被网关注入
        assert_eq!(so["include_usage"], true);
    }

    #[test]
    fn build_upstream_body_no_raw_body_rebuilds() {
        use crate::ChatCompletionRequest;
        use crate::ChatMessage;
        let req = ChatCompletionRequest {
            model: "m".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: false,
            max_tokens: Some(10),
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: None,
        };
        let body = build_upstream_body(&req);
        assert_eq!(body["model"], "m");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["max_tokens"], 10);
    }

    #[test]
    fn test_is_retryable() {
        use axum::http::StatusCode;
        // 5xx 可转移
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR, true));
        assert!(is_retryable(StatusCode::BAD_GATEWAY, true));
        assert!(is_retryable(StatusCode::SERVICE_UNAVAILABLE, true));
        // 429 受开关控制
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS, true));
        assert!(!is_retryable(StatusCode::TOO_MANY_REQUESTS, false));
        // 其他 4xx 不可转移
        assert!(!is_retryable(StatusCode::BAD_REQUEST, true));
        assert!(!is_retryable(StatusCode::UNAUTHORIZED, true));
        assert!(!is_retryable(StatusCode::NOT_FOUND, true));
        // 2xx 不在此函数语义内（调用方只在失败时调用），但保守返回 false
        assert!(!is_retryable(StatusCode::OK, true));
    }

    #[test]
    fn test_failover_on_429_enabled() {
        // 默认（无 extra_config / 无该 key）为 true
        assert!(failover_on_429_enabled(None));
        assert!(failover_on_429_enabled(Some("{}")));
        assert!(failover_on_429_enabled(Some(
            r#"{"compat_tool_history":true}"#
        )));
        // 显式配置
        assert!(failover_on_429_enabled(Some(r#"{"failover_on_429":true}"#)));
        assert!(!failover_on_429_enabled(Some(
            r#"{"failover_on_429":false}"#
        )));
        // 非法 JSON 保守默认 true
        assert!(failover_on_429_enabled(Some("not-json")));
    }

    #[test]
    fn test_set_body_model() {
        let mut body = serde_json::json!({"model": "old", "messages": []});
        set_body_model(&mut body, "new-model");
        assert_eq!(body["model"], "new-model");
        assert_eq!(body["messages"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn test_stream_guarded_success_replays_prefix() {
        // 起裸 TCP mock：延迟 50ms 后吐两个 SSE chunk 再关
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            // 读完请求头+体（简单粗暴：读一次足够测试体量）
            let _ = sock.read(&mut buf).await.unwrap();
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let client = test_client();
        let req_body = serde_json::json!({"model": "m", "stream": true});
        let resp = call_upstream_stream_guarded(&client, &format!("http://{}", addr), "k", &req_body, "v1/chat/completions")
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // 读全量 body：应包含两个 chunk（前缀 replay + 续传）
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("hel"));
        assert!(text.contains("lo"));
        assert!(text.contains("[DONE]"));
    }

    #[tokio::test]
    async fn test_stream_guarded_connect_failure_retryable() {
        // 绑定后立即丢弃 listener → 端口不可连
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let client = test_client();
        let req_body = serde_json::json!({"model": "m", "stream": true});
        let err = call_upstream_stream_guarded(&client, &format!("http://{}", addr), "k", &req_body, "v1/chat/completions")
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(is_retryable(err.0, true));
    }

    #[tokio::test]
    async fn test_stream_guarded_upstream_500_retryable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            let body = "{\"error\":\"boom\"}";
            let resp = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        let client = test_client();
        let req_body = serde_json::json!({"model": "m", "stream": true});
        let err = call_upstream_stream_guarded(&client, &format!("http://{}", addr), "k", &req_body, "v1/chat/completions")
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(is_retryable(err.0, true));
    }

    #[tokio::test]
    async fn test_stream_guarded_crlf_first_event_releases() {
        // 严格回归：mock 在首个 CRLF 事件后保持连接打开 2s 再发 [DONE] 并关闭——
        // 守卫若不能识别 \r\n\r\n，会傻等连接结束（>=2s，走"空流也放行"的 EOF 分支）
        // 才返回，ttfb 断言失败；正确行为 = 首个 CRLF 事件到达即放行（<1s）。
        // 修复前的宽松版（200ms 后关闭连接）会误走 EOF 分支通过，无法捕获 CRLF 回归。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            // 裸 HTTP/1.1 无 Content-Length + Connection: close：靠连接结束标识 EOF
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"cr\"}}]}\r\n\r\n")
                .await
                .unwrap();
            sock.flush().await.unwrap();
            // 关键：保持连接打开 2s，守卫应早已放行
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            sock.write_all(b"data: [DONE]\r\n\r\n").await.unwrap();
        });

        let client = test_client();
        let req_body = serde_json::json!({"model": "m", "stream": true});
        let started = std::time::Instant::now();
        let resp = call_upstream_stream_guarded(&client, &format!("http://{}", addr), "k", &req_body, "v1/chat/completions")
            .await
            .unwrap();
        let ttfb = started.elapsed();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            ttfb < std::time::Duration::from_secs(1),
            "首事件即放行，实际耗时 {:?}（CRLF 识别失效？）",
            ttfb
        );
        // 剩余流续传不丢字节：前缀已含第一段，续传含 [DONE]
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("cr"));
        assert!(text.contains("[DONE]"));
    }

    /// 测试辅助：起一个行为可控的 mock 上游（每个连接调 handler 一次）。
    async fn start_behavior_upstream<F>(mut on_conn: F) -> String
    where
        F: FnMut(
                tokio::net::TcpStream,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                on_conn(sock).await;
            }
        });
        format!("http://{}", addr)
    }

    /// 快速构造 CandidateChain（测试用）。
    fn test_chain(specs: &[(&str, &str, &str)]) -> crate::router::CandidateChain {
        test_chain_with_protocol(specs, crate::router::UpstreamProtocol::ChatCompletions)
    }

    /// 快速构造 CandidateChain，指定上游协议（测试用）。
    fn test_chain_with_protocol(
        specs: &[(&str, &str, &str)],
        protocol: crate::router::UpstreamProtocol,
    ) -> crate::router::CandidateChain {
        use crate::router::{Candidate, CandidateChain};
        use crate::ProviderConfig;
        CandidateChain {
            candidates: specs
                .iter()
                .enumerate()
                .map(|(i, (base, model_name, model_id))| Candidate {
                    provider: ProviderConfig {
                        id: format!("p{}", i),
                        name: format!("P{}", i),
                        provider_type: "deepseek".into(),
                        base_url: base.to_string(),
                        api_key: "k".into(),
                        extra_config: None,
                        anthropic_base_url: None,
                        enabled: true,
                        created_at: String::new(),
                        updated_at: String::new(),
                    },
                    model_name: model_name.to_string(),
                    model_id: model_id.to_string(),
                    priority: i as i64,
                    upstream_protocol: protocol,
                })
                .collect(),
            group_name: Some("g".into()),
        }
    }

    /// 快速构造混合协议 CandidateChain（测试用）。
    fn test_chain_mixed(specs: &[(&str, &str, &str, crate::router::UpstreamProtocol)]) -> crate::router::CandidateChain {
        use crate::router::{Candidate, CandidateChain};
        use crate::ProviderConfig;
        CandidateChain {
            candidates: specs
                .iter()
                .enumerate()
                .map(|(i, (base, model_name, model_id, proto))| Candidate {
                    provider: ProviderConfig {
                        id: format!("p{}", i),
                        name: format!("P{}", i),
                        provider_type: "deepseek".into(),
                        base_url: base.to_string(),
                        api_key: "k".into(),
                        extra_config: None,
                        anthropic_base_url: None,
                        enabled: true,
                        created_at: String::new(),
                        updated_at: String::new(),
                    },
                    model_name: model_name.to_string(),
                    model_id: model_id.to_string(),
                    priority: i as i64,
                    upstream_protocol: *proto,
                })
                .collect(),
            group_name: Some("g".into()),
        }
    }

    fn ok_sse_response_body() -> &'static str {
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\r\n\r\ndata: [DONE]\r\n\r\n"
    }

    /// 非流式 200 的合法 JSON body（body 质量校验后必须是合法 JSON 才能放行）。
    fn ok_json_response_body() -> &'static str {
        "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}"
    }

    async fn write_http(sock: &mut tokio::net::TcpStream, status: &str, body: &str) {
        use tokio::io::AsyncWriteExt;
        let resp = format!(
            "HTTP/1.1 {}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            body.len(),
            body
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
    }

    async fn drain_request(sock: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 8192];
        let _ = sock.read(&mut buf).await;
    }

    #[tokio::test]
    async fn test_failover_first_candidate_500_then_success() {
        let bad = start_behavior_upstream(|mut s| {
            Box::pin(async move {
                drain_request(&mut s).await;
                write_http(&mut s, "500 Internal Server Error", "{\"e\":1}").await;
            })
        })
        .await;
        let good = start_behavior_upstream(|mut s| {
            Box::pin(async move {
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", ok_sse_response_body()).await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad, "m-bad", "id-bad"), (&good, "m-good", "id-good")]);
        let body = serde_json::json!({"model": "router", "stream": true, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, true, None).await;
        let FailoverOutcome::Success {
            resp,
            candidate,
            failed_over,
            upstream_anthropic,
        } = out
        else {
            panic!("expected success");
        };
        assert!(failed_over);
        assert!(
            !upstream_anthropic,
            "无 anthropic_body 的候选走转换路径，upstream_anthropic 应为 false"
        );
        assert_eq!(candidate.model_id, "id-good");
        assert_eq!(resp.status(), StatusCode::OK);
        // 坏候选被记一次失败
        assert_eq!(breakers.snapshot("id-bad").consecutive_failures, 1);
        // 好候选成功复位
        assert_eq!(breakers.snapshot("id-good").consecutive_failures, 0);
    }

    #[tokio::test]
    async fn test_failover_exhausted_returns_last_error() {
        let bad1 = start_behavior_upstream(|mut s| {
            Box::pin(async move {
                drain_request(&mut s).await;
                write_http(&mut s, "500 Internal Server Error", "{\"e\":1}").await;
            })
        })
        .await;
        let bad2 = start_behavior_upstream(|mut s| {
            Box::pin(async move {
                drain_request(&mut s).await;
                write_http(&mut s, "503 Service Unavailable", "{\"e\":2}").await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad1, "m1", "id1"), (&bad2, "m2", "id2")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        let FailoverOutcome::Exhausted {
            status,
            failed_over,
            ..
        } = out
        else {
            panic!("expected exhausted");
        };
        assert!(failed_over);
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "返回最后一个候选的错误"
        );
    }

    #[tokio::test]
    async fn test_failover_400_not_retryable() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let h1 = hits.clone();
        let bad = start_behavior_upstream(move |mut s| {
            let h = h1.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "400 Bad Request", "{\"e\":\"bad\"}").await;
            })
        })
        .await;
        let h2 = hits.clone();
        let never = start_behavior_upstream(move |mut s| {
            let h = h2.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", ok_sse_response_body()).await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad, "m1", "id1"), (&never, "m2", "id2")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        let FailoverOutcome::Exhausted {
            status,
            failed_over,
            ..
        } = out
        else {
            panic!("expected exhausted (400 应立即终止)");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!failed_over, "400 不算转移");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "第二个候选不应被请求");
        // 4xx 视为上游健康：不计失败
        assert_eq!(breakers.snapshot("id1").consecutive_failures, 0);
    }

    #[tokio::test]
    async fn test_failover_skips_broken_candidate() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let bad_hits = Arc::new(AtomicUsize::new(0));
        let bh = bad_hits.clone();
        let bad = start_behavior_upstream(move |mut s| {
            let bh = bh.clone();
            Box::pin(async move {
                bh.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "500 Internal Server Error", "{}").await;
            })
        })
        .await;
        let good = start_behavior_upstream(|mut s| {
            Box::pin(async move {
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", ok_json_response_body()).await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad, "m-bad", "id-bad"), (&good, "m-good", "id-good")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        // 连续 5 次请求把 id-bad 打到熔断
        for _ in 0..5 {
            let _ = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        }
        assert_eq!(bad_hits.load(Ordering::SeqCst), 5);
        assert_eq!(
            breakers.snapshot("id-bad").state,
            crate::breaker::BreakerStateView::Open
        );
        // 第 6 次：坏候选被跳过，直接打好候选
        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        assert!(matches!(out, FailoverOutcome::Success { .. }));
        assert_eq!(bad_hits.load(Ordering::SeqCst), 5, "熔断后不再请求坏候选");
    }

    #[tokio::test]
    async fn test_failover_all_candidates_circuit_open() {
        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[
            ("http://127.0.0.1:1", "m1", "id1"),
            ("http://127.0.0.1:1", "m2", "id2"),
        ]);
        // 手动把两个候选打到熔断（5 连败）
        for _ in 0..crate::breaker::FAILURE_THRESHOLD {
            breakers.record_failure("id1");
            breakers.record_failure("id2");
        }
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});
        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        let FailoverOutcome::Exhausted {
            status,
            message,
            failed_over,
        } = out
        else {
            panic!("expected exhausted");
        };
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(message, "all_candidates_unavailable");
        assert!(!failed_over);
    }

    #[tokio::test]
    async fn test_failover_single_candidate_stream_no_guard() {
        // 单元素链流式：不走首字节守卫（旧 relay 行为，响应头即放行）。
        // mock 先发 200 响应头、sleep 2s 才发首个 SSE 事件——若仍走守卫，
        // 会等待首事件 ≥2s 才返回；旧行为应 <1s 拿到 Response。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            // 关键：2s 后才发首事件（守卫在此处才会放行，旧 relay 应立即放行）
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = sock
                .write_all(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n",
                )
                .await;
        });

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&format!("http://{}", addr), "m1", "id1")]);
        let body = serde_json::json!({"model": "router", "stream": true, "messages": []});

        let started = std::time::Instant::now();
        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, true, None).await;
        let ttfb = started.elapsed();
        let FailoverOutcome::Success {
            resp, failed_over, ..
        } = out
        else {
            panic!("expected success");
        };
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!failed_over);
        assert!(
            ttfb < std::time::Duration::from_secs(1),
            "单元素链流式应响应头即放行，实际耗时 {:?}（仍走首字节守卫？）",
            ttfb
        );
        // 剩余流透传完整（relay 直通，连接未截断）
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("hi"));
        assert!(text.contains("[DONE]"));
    }

    #[tokio::test]
    async fn test_failover_preferred_broken_backup_fails_marks_transfer() {
        // 首选熔断跳过（allow=false 不计 attempts）+ 备选被尝试且失败 → attempts==1。
        // 修复前 failed_over 用 attempts>1 判定 → false（归因错误，model_name 记为首选）；
        // 修复后按 last_attempted（备选）≠ 首选判定 → true。
        let backup = start_behavior_upstream(|mut s| {
            Box::pin(async move {
                drain_request(&mut s).await;
                write_http(&mut s, "500 Internal Server Error", "{\"e\":1}").await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        // 手动机 5 连败把首选熔断
        for _ in 0..crate::breaker::FAILURE_THRESHOLD {
            breakers.record_failure("id-broken");
        }
        let chain = test_chain(&[
            ("http://127.0.0.1:1", "m-broken", "id-broken"),
            (&backup, "m-backup", "id-backup"),
        ]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        let FailoverOutcome::Exhausted {
            status,
            failed_over,
            ..
        } = out
        else {
            panic!("expected exhausted");
        };
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            failed_over,
            "首选熔断跳过+备选失败应记为转移（failed_over=true）"
        );
        // 首选从未被请求（熔断跳过），备选被请求一次
        assert_eq!(
            breakers.snapshot("id-broken").state,
            crate::breaker::BreakerStateView::Open
        );
    }

    #[tokio::test]
    async fn test_stream_guarded_prefix_overflow() {
        // mock：200 后持续吐无事件分隔符的字节（每次 1MB 纯文本行，无 \n\n）
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            let chunk = vec![b'x'; 1024 * 1024];
            for _ in 0..8 {
                if sock.write_all(&chunk).await.is_err() {
                    break; // 客户端已断开（守卫返回 Err 后 drop）
                }
            }
        });

        let client = test_client();
        let req_body = serde_json::json!({"model": "m", "stream": true});
        let err = call_upstream_stream_guarded(&client, &format!("http://{}", addr), "k", &req_body, "v1/chat/completions")
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(err.1.contains("prefix") || err.1.contains("limit"));
    }

    #[tokio::test]
    async fn test_relay_upstream_body_overflow_returns_502() {
        // mock：200 后分块发送 >16MB 纯文本（无 Content-Length，靠 Connection: close 收尾）。
        // 有界读取累计超限必须返回 502，而非 `resp.bytes().await` 无上限读入整段内存。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            let chunk = vec![b'x'; 1024 * 1024];
            for _ in 0..20 {
                if sock.write_all(&chunk).await.is_err() {
                    break; // 客户端已断开（relay_upstream_body 返回 Err 后 drop）
                }
            }
        });

        // 用本地构建的上游 client 拿一个真实 2xx 响应（非流式直通路径的入参）
        let client = test_client();
        let resp = client
            .post(format!("http://{}/v1/chat/completions", addr))
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        let err = relay_upstream_body(resp).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(
            err.1.contains("exceeded"),
            "错误文案应含 exceeded: {}",
            err.1
        );
    }

    #[tokio::test]
    async fn test_failover_401_recorded_and_group_fails_over_to_backup() {
        // 组 [A(401), B(200)]：首选 key 失效 → 记录确定性失败并转移；
        // 第二次请求 A 被跳过（TTL 内），直达 B。
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let a_hits = Arc::new(AtomicUsize::new(0));
        let h = a_hits.clone();
        let a = start_behavior_upstream(move |mut s| {
            let h = h.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(
                    &mut s,
                    "401 Unauthorized",
                    "{\"error\":\"invalid api key\"}",
                )
                .await;
            })
        })
        .await;
        let b_hits = Arc::new(AtomicUsize::new(0));
        let h = b_hits.clone();
        let b = start_behavior_upstream(move |mut s| {
            let h = h.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", ok_json_response_body()).await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let known = crate::down::KnownFailures::new();
        let chain = test_chain(&[(&a, "m-a", "id-a"), (&b, "m-b", "id-b")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        // 第一次：A 401 → 记录并转移 → B 成功
        let out = execute_with_failover(&test_client(), &breakers, &known, &chain, &body, false, None).await;
        let FailoverOutcome::Success {
            candidate,
            failed_over,
            ..
        } = out
        else {
            panic!("expected success via backup");
        };
        assert_eq!(candidate.model_id, "id-b");
        assert!(failed_over, "从失效首选转移到备选应记为 failed_over");
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
        assert_eq!(b_hits.load(Ordering::SeqCst), 1);

        // 第二次：A 被跳过（known 失败），B 再次成功
        let out = execute_with_failover(&test_client(), &breakers, &known, &chain, &body, false, None).await;
        assert!(matches!(out, FailoverOutcome::Success { .. }));
        assert_eq!(
            a_hits.load(Ordering::SeqCst),
            1,
            "确定性失败后首选不再被请求"
        );
        assert_eq!(b_hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_failover_single_401_cached_returns_without_network() {
        // 单元素链 401：第一次真实调用失败并记录；第二次直接秒回，不再打上游。
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let a = start_behavior_upstream(move |mut s| {
            let h = h.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "401 Unauthorized", "{\"error\":\"bad key\"}").await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let known = crate::down::KnownFailures::new();
        let chain = test_chain(&[(&a, "m-a", "id-a")]);
        let body = serde_json::json!({"model": "single", "stream": false, "messages": []});

        // 第一次：网络调用 → 401
        let out = execute_with_failover(&test_client(), &breakers, &known, &chain, &body, false, None).await;
        let FailoverOutcome::Exhausted { status, .. } = out else {
            panic!("expected exhausted");
        };
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // 第二次：known 命中，不发起网络调用，仍返回 401 错误
        let out = execute_with_failover(&test_client(), &breakers, &known, &chain, &body, false, None).await;
        let FailoverOutcome::Exhausted {
            status, message, ..
        } = out
        else {
            panic!("expected exhausted");
        };
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            message.contains("401"),
            "应返回缓存的上游错误消息: {message}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "确定性失败后不应再发起网络调用"
        );

        // 手动清除后立即恢复探测
        known.clear_all();
        let out = execute_with_failover(&test_client(), &breakers, &known, &chain, &body, false, None).await;
        assert!(matches!(out, FailoverOutcome::Exhausted { .. }));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_failover_400_not_cached_and_still_hard_stops() {
        // 400 不纳入确定性失败：不缓存、不转移（第二个候选不被请求），每次重新请求首选。
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let a = start_behavior_upstream(move |mut s| {
            let h = h.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "400 Bad Request", "{\"error\":\"bad input\"}").await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let known = crate::down::KnownFailures::new();
        let chain = test_chain(&[(&a, "m-a", "id-a"), ("http://127.0.0.1:1", "m-b", "id-b")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        for _ in 0..2 {
            let out = execute_with_failover(&test_client(), &breakers, &known, &chain, &body, false, None).await;
            let FailoverOutcome::Exhausted { status, .. } = out else {
                panic!("expected exhausted");
            };
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "400 不缓存：每次都重新请求首选"
        );
    }

    // ── Responses 上游协议集成测试 ─────────────────────────────────

    /// Responses 候选非流式：断言上游收到 Responses 格式请求体（input 数组、store:false、
    /// 无 messages 字段），URL 是 /v1/responses；上游返回 Responses JSON，
    /// 网关得到 chat completion JSON。
    #[tokio::test]
    async fn test_failover_responses_candidate_non_streaming() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let received_responses_format = Arc::new(AtomicBool::new(false));
        let rrf = received_responses_format.clone();

        let upstream = start_behavior_upstream(move |mut s| {
            let rrf = rrf.clone();
            Box::pin(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap();
                let req_text = String::from_utf8_lossy(&buf[..n]);

                // 验证：URL 包含 /v1/responses
                let first_line = req_text.lines().next().unwrap_or("");
                assert!(
                    first_line.contains("/v1/responses"),
                    "upstream should receive /v1/responses URL, got: {first_line}"
                );

                // 验证：请求体是 Responses 格式（有 input 数组、store:false、无 messages）
                if let Some(body_start) = req_text.find("\r\n\r\n") {
                    let body = &req_text[body_start + 4..];
                    let v: serde_json::Value = serde_json::from_str(body).unwrap();
                    assert!(
                        v.get("input").is_some(),
                        "should have 'input' field (Responses format): {body}"
                    );
                    assert_eq!(v["store"], false, "should have store:false");
                    assert!(
                        v.get("messages").is_none(),
                        "should NOT have 'messages' field: {body}"
                    );
                    rrf.store(true, Ordering::SeqCst);
                }

                // 返回 Responses 格式响应
                let responses_json = serde_json::json!({
                    "id": "resp_abc",
                    "object": "response",
                    "created_at": 1700000000,
                    "model": "gpt-5-codex",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Hello from Responses" }]
                    }],
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "total_tokens": 15
                    }
                });
                let resp_body = responses_json.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                s.write_all(resp.as_bytes()).await.unwrap();
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain_with_protocol(
            &[(&upstream, "gpt-5-codex", "id-resp")],
            crate::router::UpstreamProtocol::Responses,
        );
        let body = serde_json::json!({
            "model": "router",
            "stream": false,
            "messages": [{ "role": "user", "content": "hi" }]
        });

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        let FailoverOutcome::Success {
            resp,
            candidate,
            failed_over,
            upstream_anthropic,
        } = out
        else {
            panic!("expected success");
        };
        assert!(!failed_over);
        assert!(
            !upstream_anthropic,
            "Responses 候选走转换路径，upstream_anthropic 应为 false"
        );
        assert_eq!(candidate.model_id, "id-resp");
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            received_responses_format.load(Ordering::SeqCst),
            "upstream should have received Responses format request"
        );

        // 验证：响应体是 chat completion 格式
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let chat_resp: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(chat_resp["object"], "chat.completion");
        assert_eq!(
            chat_resp["choices"][0]["message"]["content"],
            "Hello from Responses"
        );
        assert_eq!(chat_resp["choices"][0]["finish_reason"], "stop");
        assert_eq!(chat_resp["usage"]["prompt_tokens"], 10);
        assert_eq!(chat_resp["usage"]["completion_tokens"], 5);
    }

    /// Responses 候选流式：上游发 Responses SSE（response.created / output_text.delta /
    /// response.completed 带 usage），断言客户端收到 chat chunk SSE
    ///（delta.content、收尾 finish_reason+usage、[DONE]）。
    #[tokio::test]
    async fn test_failover_responses_candidate_streaming() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let received_responses_format = Arc::new(AtomicBool::new(false));
        let rrf = received_responses_format.clone();

        let upstream = start_behavior_upstream(move |mut s| {
            let rrf = rrf.clone();
            Box::pin(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap();
                let req_text = String::from_utf8_lossy(&buf[..n]);
                let first_line = req_text.lines().next().unwrap_or("");
                assert!(
                    first_line.contains("/v1/responses"),
                    "upstream should receive /v1/responses URL, got: {first_line}"
                );
                rrf.store(true, Ordering::SeqCst);

                // 返回 Responses SSE 流
                let sse = concat!(
                    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_s1\",\"model\":\"gpt-5\",\"created_at\":1700000000,\"output\":[],\"status\":\"in_progress\"}}\n\n",
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n\n",
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n",
                    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_s1\",\"model\":\"gpt-5\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello world\"}]}],\"usage\":{\"input_tokens\":8,\"output_tokens\":3,\"total_tokens\":11}}}\n\n",
                    "data: [DONE]\n\n",
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse.len(),
                    sse
                );
                s.write_all(resp.as_bytes()).await.unwrap();
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain_with_protocol(
            &[(&upstream, "gpt-5", "id-sresp")],
            crate::router::UpstreamProtocol::Responses,
        );
        let body = serde_json::json!({"model": "router", "stream": true, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, true, None).await;
        let FailoverOutcome::Success { resp, .. } = out else {
            panic!("expected success");
        };
        assert!(received_responses_format.load(Ordering::SeqCst));
        assert_eq!(resp.status(), StatusCode::OK);

        // 读取完整 SSE 流
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // 验证 chat chunk SSE 格式
        assert!(
            text.contains("chat.completion.chunk"),
            "should have chat.completion.chunk objects:\n{text}"
        );
        assert!(
            text.contains("\"content\":\"Hello \""),
            "should have content delta:\n{text}"
        );
        assert!(
            text.contains("\"content\":\"world\""),
            "should have second content delta:\n{text}"
        );
        assert!(
            text.contains("\"finish_reason\":\"stop\""),
            "should have finish_reason stop:\n{text}"
        );
        assert!(
            text.contains("\"prompt_tokens\":8"),
            "should have usage:\n{text}"
        );
        assert!(
            text.contains("data: [DONE]"),
            "should end with [DONE]:\n{text}"
        );
    }

    /// 混合候选链故障转移：首选 Responses 候选 500，次选 chat 候选成功——
    /// 断言两次上游调用格式各自正确、最终响应正常。
    #[tokio::test]
    async fn test_failover_mixed_chain_responses_500_then_chat_success() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let responses_hits = Arc::new(AtomicUsize::new(0));
        let chat_hits = Arc::new(AtomicUsize::new(0));
        let rh = responses_hits.clone();
        let ch = chat_hits.clone();

        // 首选：Responses 候选，返回 500
        let responses_up = {
            let rh = rh.clone();
            start_behavior_upstream(move |mut s| {
                let rh = rh.clone();
                Box::pin(async move {
                    rh.fetch_add(1, Ordering::SeqCst);
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let _ = s.read(&mut buf).await;
                    let err_body = r#"{"error":{"message":"internal","type":"server_error"}}"#;
                    let resp = format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        err_body.len(),
                        err_body
                    );
                    s.write_all(resp.as_bytes()).await.unwrap();
                })
            })
            .await
        };

        // 次选：Chat Completions 候选，成功
        let chat_up = {
            let ch = ch.clone();
            start_behavior_upstream(move |mut s| {
                let ch = ch.clone();
                Box::pin(async move {
                    ch.fetch_add(1, Ordering::SeqCst);
                    drain_request(&mut s).await;
                    write_http(&mut s, "200 OK", ok_sse_response_body()).await;
                })
            })
            .await
        };

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain_mixed(&[
            (
                &responses_up,
                "gpt-5-codex",
                "id-resp",
                crate::router::UpstreamProtocol::Responses,
            ),
            (
                &chat_up,
                "gpt-4o",
                "id-chat",
                crate::router::UpstreamProtocol::ChatCompletions,
            ),
        ]);
        let body = serde_json::json!({"model": "router", "stream": true, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, true, None).await;
        let FailoverOutcome::Success {
            resp,
            candidate,
            failed_over,
            upstream_anthropic,
        } = out
        else {
            panic!("expected success via chat fallback");
        };
        assert!(
            failed_over,
            "should have failed over from Responses to Chat"
        );
        assert!(
            !upstream_anthropic,
            "chat 候选走转换路径，upstream_anthropic 应为 false"
        );
        assert_eq!(candidate.model_id, "id-chat");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            responses_hits.load(Ordering::SeqCst),
            1,
            "responses candidate hit once"
        );
        assert_eq!(
            chat_hits.load(Ordering::SeqCst),
            1,
            "chat candidate hit once"
        );

        // 验证最终响应是 chat completion 格式
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            text.contains("hi"),
            "should contain upstream content: {text}"
        );
        assert!(
            text.contains("[DONE]"),
            "should have [DONE] terminator: {text}"
        );
    }

    // ── Anthropic 直通（anthropic_base_url）分支 ──────────────────────────

    /// 快速构造带 anthropic_base_url 的 CandidateChain（测试用）。
    /// 每个 spec 为 `(base_url, model_name, model_id, anthropic_base_url)`。
    fn test_chain_anthropic(
        specs: &[(&str, &str, &str, Option<&str>)],
    ) -> crate::router::CandidateChain {
        use crate::router::{Candidate, CandidateChain};
        use crate::ProviderConfig;
        CandidateChain {
            candidates: specs
                .iter()
                .enumerate()
                .map(|(i, (base, model_name, model_id, anthro))| Candidate {
                    provider: ProviderConfig {
                        id: format!("p{}", i),
                        name: format!("P{}", i),
                        provider_type: "deepseek".into(),
                        base_url: base.to_string(),
                        api_key: "k".into(),
                        extra_config: None,
                        anthropic_base_url: anthro.map(str::to_string),
                        enabled: true,
                        created_at: String::new(),
                        updated_at: String::new(),
                    },
                    model_name: model_name.to_string(),
                    model_id: model_id.to_string(),
                    priority: i as i64,
                    upstream_protocol: crate::router::UpstreamProtocol::ChatCompletions,
                })
                .collect(),
            group_name: Some("g".into()),
        }
    }

    /// 解析 mock 收到的原始 HTTP 请求 → (请求行, JSON body)。
    async fn parse_mock_request(buf: &[u8]) -> (String, serde_json::Value) {
        let text = String::from_utf8_lossy(buf);
        let first_line = text.lines().next().unwrap_or("").to_string();
        let body = if let Some(pos) = text.find("\r\n\r\n") {
            serde_json::from_str(&text[pos + 4..]).unwrap_or_default()
        } else {
            serde_json::Value::Null
        };
        (first_line, body)
    }

    /// 纯函数级：单候选配 anthropic_base_url + 入口带 anthropic_body → 直发 /v1/messages，
    /// 发送体 = 原始 Anthropic body（仅 model 替换为候选真实名），成功返回
    /// `upstream_anthropic=true` 且响应为 Anthropic 格式（不被二次转换）。
    #[tokio::test]
    async fn test_failover_anthropic_passthrough_single_candidate() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let hit_path = Arc::new(AtomicBool::new(false));
        let hp = hit_path.clone();
        let captured = Arc::new(tokio::sync::Mutex::new(serde_json::Value::Null));
        let cap = captured.clone();
        let upstream = start_behavior_upstream(move |mut s| {
            let hp = hp.clone();
            let cap = cap.clone();
            Box::pin(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 16384];
                let n = s.read(&mut buf).await.unwrap();
                let (first_line, body) = parse_mock_request(&buf[..n]).await;
                assert!(
                    first_line.contains("/v1/messages"),
                    "直通候选应请求 /v1/messages: {first_line}"
                );
                hp.store(true, Ordering::SeqCst);
                *cap.lock().await = body;
                let resp_body = serde_json::json!({
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "model": "m-a",
                    "content": [{"type":"text","text":"hi from anthropic"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                s.write_all(resp.as_bytes()).await.unwrap();
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain_anthropic(&[("http://127.0.0.1:1", "m-a", "id-a", Some(&upstream))]);
        let req_body = serde_json::json!({"model": "router", "stream": false, "messages": []});
        let anthropic_body = serde_json::json!({
            "model": "router",
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}]
        });

        let out = execute_with_failover(
            &test_client(),
            &breakers,
            &EMPTY_KNOWN,
            &chain,
            &req_body,
            false,
            Some(&anthropic_body),
        )
        .await;
        let FailoverOutcome::Success {
            resp,
            candidate,
            failed_over,
            upstream_anthropic,
        } = out
        else {
            panic!("expected success");
        };
        assert!(!failed_over);
        assert_eq!(candidate.model_id, "id-a");
        assert!(upstream_anthropic, "直通成功应标记 upstream_anthropic=true");
        assert!(hit_path.load(Ordering::SeqCst), "应命中 /v1/messages");
        assert_eq!(resp.status(), StatusCode::OK);
        // 发送体 = 原始 Anthropic body，仅 model 替换为候选真实名
        let sent = captured.lock().await.clone();
        assert_eq!(
            sent["model"], "m-a",
            "直通 body 的 model 应替换为候选名: {sent}"
        );
        assert_eq!(sent["max_tokens"], 8);
        assert_eq!(sent["messages"][0]["role"], "user");
        // 响应体原样透传（Anthropic 格式，未被二次转换）
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "hi from anthropic");
    }

    /// 纯函数级：组 [A(直通), B(直通)]，首选直通 404（确定性失败 → 记 known-failures）
    /// 自动尝试备选直通成功；第二次请求首选被跳过（TTL 内）直达备选。
    #[tokio::test]
    async fn test_failover_anthropic_passthrough_404_then_backup() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let a_hits = Arc::new(AtomicUsize::new(0));
        let ah = a_hits.clone();
        let a = start_behavior_upstream(move |mut s| {
            let ah = ah.clone();
            Box::pin(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 16384];
                let n = s.read(&mut buf).await.unwrap();
                let (first_line, _) = parse_mock_request(&buf[..n]).await;
                assert!(
                    first_line.contains("/v1/messages"),
                    "首选直通应请求 /v1/messages: {first_line}"
                );
                ah.fetch_add(1, Ordering::SeqCst);
                let err_body =
                    r#"{"type":"error","error":{"type":"not_found_error","message":"model not found"}}"#;
                let resp = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    err_body.len(),
                    err_body
                );
                s.write_all(resp.as_bytes()).await.unwrap();
            })
        })
        .await;

        let b_hits = Arc::new(AtomicUsize::new(0));
        let bh = b_hits.clone();
        let b = start_behavior_upstream(move |mut s| {
            let bh = bh.clone();
            Box::pin(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 16384];
                let n = s.read(&mut buf).await.unwrap();
                let (first_line, body) = parse_mock_request(&buf[..n]).await;
                assert!(
                    first_line.contains("/v1/messages"),
                    "备选直通应请求 /v1/messages: {first_line}"
                );
                assert_eq!(body["model"], "m-b", "备选直通 body 的 model 应为其真实名: {body}");
                bh.fetch_add(1, Ordering::SeqCst);
                let resp_body = serde_json::json!({
                    "id": "msg_b",
                    "type": "message",
                    "role": "assistant",
                    "model": "m-b",
                    "content": [{"type":"text","text":"ok from backup"}],
                    "stop_reason": "end_turn"
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                s.write_all(resp.as_bytes()).await.unwrap();
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let known = crate::down::KnownFailures::new();
        let chain = test_chain_anthropic(&[
            ("http://127.0.0.1:1", "m-a", "id-a", Some(&a)),
            ("http://127.0.0.1:1", "m-b", "id-b", Some(&b)),
        ]);
        let req_body = serde_json::json!({"model": "router", "stream": false, "messages": []});
        let anthropic_body = serde_json::json!({
            "model": "router",
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}]
        });

        // 第一次：首选直通 404 → 记录并转移 → 备选直通成功
        let out = execute_with_failover(
            &test_client(),
            &breakers,
            &known,
            &chain,
            &req_body,
            false,
            Some(&anthropic_body),
        )
        .await;
        let FailoverOutcome::Success {
            resp,
            candidate,
            failed_over,
            upstream_anthropic,
        } = out
        else {
            panic!("expected success via backup");
        };
        assert_eq!(candidate.model_id, "id-b");
        assert!(failed_over, "首选 404 转移到备选应记为 failed_over");
        assert!(
            upstream_anthropic,
            "备选直通成功也应标记 upstream_anthropic=true"
        );
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
        assert_eq!(b_hits.load(Ordering::SeqCst), 1);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["content"][0]["text"], "ok from backup");

        // 第二次：首选被跳过（known 命中），备选再次成功
        let out = execute_with_failover(
            &test_client(),
            &breakers,
            &known,
            &chain,
            &req_body,
            false,
            Some(&anthropic_body),
        )
        .await;
        assert!(matches!(out, FailoverOutcome::Success { .. }));
        assert_eq!(
            a_hits.load(Ordering::SeqCst),
            1,
            "确定性失败后首选不再被请求"
        );
        assert_eq!(b_hits.load(Ordering::SeqCst), 2);
    }

    // ── 上游 200 空/畸形响应的重试 + 故障转移 ──────────────────────────

    /// 非流式 200 空 body：同候选原地重试，第二次返回正常 JSON 后成功。
    #[tokio::test]
    async fn test_failover_nonstream_empty_body_retry_then_success() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let upstream = start_behavior_upstream(move |mut s| {
            let h = h.clone();
            Box::pin(async move {
                let n = h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                if n == 0 {
                    // 第一次：200 + 空 body
                    write_http(&mut s, "200 OK", "").await;
                } else {
                    // 第二次：正常 JSON
                    write_http(&mut s, "200 OK", "{\"choices\":[]}").await;
                }
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&upstream, "m1", "id1")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        let FailoverOutcome::Success {
            resp, failed_over, ..
        } = out
        else {
            panic!("expected success after in-place retry");
        };
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!failed_over);
        assert_eq!(hits.load(Ordering::SeqCst), 2, "首次空 body + 1 次原地重试");
        // 最终成功：熔断计数复位
        assert_eq!(breakers.snapshot("id1").consecutive_failures, 0);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("choices").is_some());
    }

    /// 非流式 200 空 body：首选始终空（尝试 2 次=首次+重试），转移到备选成功。
    #[tokio::test]
    async fn test_failover_nonstream_empty_body_failover() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let bad_hits = Arc::new(AtomicUsize::new(0));
        let bh = bad_hits.clone();
        let bad = start_behavior_upstream(move |mut s| {
            let bh = bh.clone();
            Box::pin(async move {
                bh.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", "").await;
            })
        })
        .await;
        let good = start_behavior_upstream(|mut s| {
            Box::pin(async move {
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", "{\"choices\":[]}").await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad, "m-bad", "id-bad"), (&good, "m-good", "id-good")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        let FailoverOutcome::Success {
            candidate,
            failed_over,
            ..
        } = out
        else {
            panic!("expected success via failover");
        };
        assert_eq!(candidate.model_id, "id-good");
        assert!(failed_over);
        assert_eq!(
            bad_hits.load(Ordering::SeqCst),
            2,
            "首选：首次 + 1 次原地重试后才转移"
        );
        assert_eq!(breakers.snapshot("id-bad").consecutive_failures, 1);
    }

    /// 非流式 200 非 JSON body：首选畸形（重试 1 次仍畸形），转移到备选成功。
    #[tokio::test]
    async fn test_failover_nonstream_malformed_json_failover() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let bad_hits = Arc::new(AtomicUsize::new(0));
        let bh = bad_hits.clone();
        let bad = start_behavior_upstream(move |mut s| {
            let bh = bh.clone();
            Box::pin(async move {
                bh.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", "this is not json").await;
            })
        })
        .await;
        let good = start_behavior_upstream(|mut s| {
            Box::pin(async move {
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", "{\"choices\":[]}").await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad, "m-bad", "id-bad"), (&good, "m-good", "id-good")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        let FailoverOutcome::Success {
            candidate,
            failed_over,
            ..
        } = out
        else {
            panic!("expected success via failover");
        };
        assert_eq!(candidate.model_id, "id-good");
        assert!(failed_over);
        assert_eq!(bad_hits.load(Ordering::SeqCst), 2, "首选：首次 + 1 次原地重试");
    }

    /// 流式链长 2：首选 200 头后立即 EOF（无任何 SSE 事件）→ 守卫返回 502，
    /// 同候选重试 1 次仍空流后转移到备选正常 SSE。
    #[tokio::test]
    async fn test_failover_stream_empty_sse_failover() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let bad_hits = Arc::new(AtomicUsize::new(0));
        let bh = bad_hits.clone();
        let bad = start_behavior_upstream(move |mut s| {
            let bh = bh.clone();
            Box::pin(async move {
                use tokio::io::AsyncWriteExt;
                bh.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                // 200 头 + 立即 EOF：不写任何 SSE data，靠 Connection: close 收尾
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                s.write_all(head.as_bytes()).await.unwrap();
            })
        })
        .await;
        let good = start_behavior_upstream(|mut s| {
            Box::pin(async move {
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", ok_sse_response_body()).await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad, "m-bad", "id-bad"), (&good, "m-good", "id-good")]);
        let body = serde_json::json!({"model": "router", "stream": true, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, true, None).await;
        let FailoverOutcome::Success {
            resp,
            candidate,
            failed_over,
            ..
        } = out
        else {
            panic!("expected success via failover after empty SSE stream");
        };
        assert_eq!(candidate.model_id, "id-good");
        assert!(failed_over);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            bad_hits.load(Ordering::SeqCst),
            2,
            "空流：首次 + 1 次原地重试后才转移"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("[DONE]"));
    }

    /// 单候选链流式：不走守卫（relay 直通），200 空 body 保持既有放行行为。
    #[tokio::test]
    async fn test_failover_single_candidate_stream_empty_no_retry() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let upstream = start_behavior_upstream(move |mut s| {
            let h = h.clone();
            Box::pin(async move {
                use tokio::io::AsyncWriteExt;
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                s.write_all(head.as_bytes()).await.unwrap();
                // 立即 EOF，不写任何 SSE 事件
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&upstream, "m1", "id1")]);
        let body = serde_json::json!({"model": "router", "stream": true, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, true, None).await;
        let FailoverOutcome::Success { resp, failed_over, .. } = out else {
            panic!("单候选链流式不走守卫，空流应保持放行（Ok）");
        };
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!failed_over);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "无守卫无重试，仅请求一次");
    }

    /// 单候选非流式始终空 body：重试 1 次仍空 → Exhausted + 502。
    #[tokio::test]
    async fn test_failover_nonstream_empty_body_exhausted() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let upstream = start_behavior_upstream(move |mut s| {
            let h = h.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", "").await;
            })
        })
        .await;

        let breakers = crate::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&upstream, "m1", "id1")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&test_client(), &breakers, &EMPTY_KNOWN, &chain, &body, false, None).await;
        let FailoverOutcome::Exhausted {
            status, message, ..
        } = out
        else {
            panic!("expected exhausted after retries depleted");
        };
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            message.contains("empty response body"),
            "错误消息应标识空 body: {message}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2, "首次 + 1 次原地重试后放弃");
    }

    /// 单元级：is_malformed_error 的判定覆盖。
    #[test]
    fn test_is_malformed_error() {
        assert!(is_malformed_error("empty response body from upstream"));
        assert!(is_malformed_error("malformed response body (not valid JSON)"));
        assert!(is_malformed_error(
            "empty SSE stream from upstream (no events before EOF)"
        ));
        assert!(is_malformed_error(
            "invalid responses-format upstream body: missing field"
        ));
        // 普通 502（连接失败/上游 5xx 透传）不属于畸形类
        assert!(!is_malformed_error("Upstream connection failed: refused"));
        assert!(!is_malformed_error("Upstream error 500: boom"));
        assert!(!is_malformed_error("Upstream first-byte timeout"));
    }
}
