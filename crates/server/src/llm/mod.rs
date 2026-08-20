pub mod anthropic_handler;
pub mod auth;
pub mod breaker;
pub mod compat;
pub mod crypto;
pub mod down;
pub mod format;
pub mod openai_handler;
pub mod pipeline;
pub mod provider;
#[cfg(feature = "rag")]
pub mod rag;
pub mod responses;
pub mod responses_handler;
pub mod route_cache;
pub mod router;
pub mod upstream;
pub mod usage;

#[cfg(feature = "rag")]
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::db::Database;

// ── Configuration types ───────────────────────────────────────

/// 协议入口标识 — 双域名隔离用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProtocol {
    OpenAI,
    Anthropic,
}

/// Gateway-level configuration (persisted as part of the LLM ProxyRule).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LlmGatewayConfig {
    pub enabled: bool,
    /// OpenAI 协议入口域名；None/空 = 不开放 OpenAI 入口。
    #[serde(default, alias = "domain", skip_serializing_if = "Option::is_none")]
    pub openai_domain: Option<String>,
    /// Anthropic 协议入口域名；None/空 = 不开放 Anthropic 入口。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_domain: Option<String>,
    pub listen: String,
    pub tls_enabled: bool,
    pub tls_acme: bool,
}

impl LlmGatewayConfig {
    /// 校验配置合法性，返回 None 表示合法。
    /// enabled=true 时至少一个域名非空；两个域名都非空时必须不同。
    pub fn validate(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let oa = self.openai_domain.as_deref().unwrap_or("");
        let an = self.anthropic_domain.as_deref().unwrap_or("");
        if oa.is_empty() && an.is_empty() {
            return Some(
                "at least one of openai_domain or anthropic_domain is required when enabled".into(),
            );
        }
        if !oa.is_empty() && !an.is_empty() && oa == an {
            return Some("openai_domain and anthropic_domain cannot be the same".into());
        }
        None
    }

    /// 根据 host 匹配命中的协议入口；都不匹配返回 None。
    pub fn match_protocol(&self, host: &str) -> Option<LlmProtocol> {
        if !self.enabled || host.is_empty() {
            return None;
        }
        if let Some(ref d) = self.openai_domain {
            if !d.is_empty() && host == d.as_str() {
                return Some(LlmProtocol::OpenAI);
            }
        }
        if let Some(ref d) = self.anthropic_domain {
            if !d.is_empty() && host == d.as_str() {
                return Some(LlmProtocol::Anthropic);
            }
        }
        None
    }

    /// 收集所有已配置的域名（用于 ProxyRule.domains 持久化）。
    /// 始终返回两个元素：`[openai_domain, anthropic_domain]`，未配置的为空字符串。
    /// `init_llm_state` 按位置索引还原（domains[0] → openai, domains[1] → anthropic）。
    pub fn configured_domains(&self) -> Vec<String> {
        vec![
            self.openai_domain.clone().unwrap_or_default(),
            self.anthropic_domain.clone().unwrap_or_default(),
        ]
    }
}

/// Provider configuration (mirrors LlmProviderRecord for API responses).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    /// API key is NEVER returned in API responses. Set via update.
    #[serde(skip_serializing)]
    pub api_key: String,
    pub extra_config: Option<String>,
    /// Anthropic Messages API base URL; `None` = 不支持 Anthropic 协议。
    pub anthropic_base_url: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// 三态反序列化：字段缺失 → `None`；显式 `null` → `Some(None)`；字符串 → `Some(Some(s))`。
/// 裸 `Option<String>` 无法区分前两者（serde 把缺失/null 都映到 None），
/// 导致前端“清除 extra_config”的显式 null 被误当“保留原值”，开关永远关不掉。
fn deserialize_nullable_string<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(<Option<String> as serde::Deserialize>::deserialize(
        d,
    )?))
}

/// Request body for creating/updating a provider.
#[derive(Debug, serde::Deserialize)]
pub struct ProviderRequest {
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    /// 三态语义：缺失 = 不修改；`null` = 清除；字符串 = 覆盖。
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub extra_config: Option<Option<String>>,
    /// Anthropic Messages API base URL; 留空或 null 表示不支持。
    pub anthropic_base_url: Option<String>,
}

/// Model configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelConfig {
    pub id: String,
    pub provider_id: String,
    pub model_name: String,
    pub alias: String,
    pub tags: Vec<String>,
    /// per-model 配置（JSON 字符串，如 `{"agent_context_limit":200000}`）。非敏感，不加密。
    pub extra_config: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Request body for creating/updating a model.
#[derive(Debug, serde::Deserialize)]
pub struct ModelRequest {
    pub model_name: String,
    pub alias: Option<String>,
    pub tags: Option<Vec<String>>,
    /// per-model 配置（JSON 字符串）。缺省/`null` = 不设置（或覆盖为无配置）。
    pub extra_config: Option<String>,
}

/// API key view (returned to management UI).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiKeyView {
    pub id: String,
    pub key_prefix: String,
    pub name: String,
    pub enabled: bool,
    pub created_at: String,
    pub last_used_at: Option<String>,
    /// 绑定的 RAG 知识库 id（未绑定为 None）。
    #[serde(default)]
    pub kb_id: Option<String>,
}

/// Request body for creating an API key.
#[derive(Debug, serde::Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    /// 可选：创建时即绑定 RAG 知识库（不存在则 400）。
    #[serde(default)]
    pub kb_id: Option<String>,
}

/// Response for API key creation — full key shown only once.
#[derive(Debug, serde::Serialize)]
pub struct CreateApiKeyResponse {
    pub id: String,
    pub key: String, // full key — show once!
    pub key_prefix: String,
    pub name: String,
}

// ── Chat types (internal representation) ──────────────────────

/// Unified chat message (both OpenAI and Anthropic map to this).
///
/// 序列化形式对齐 OpenAI Chat Completions 消息：
/// - `content` 允许缺省（assistant 的 tool_calls 消息通常 content=null）；
/// - `tool_calls` 承载 assistant 的工具调用请求；
/// - `tool_call_id` / `name` 用于 `role="tool"` 的结果消息。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// DeepSeek 思考模式的历史 `reasoning_content`：思考链开启后，上游（如
    /// Console Go 渠道）要求多轮对话里历史 assistant 消息必须原样携带该字段，
    /// 否则 400「The reasoning_content in the thinking mode must be passed back」。
    /// 由 Anthropic 历史 `thinking` 块映射而来（thinking → reasoning_content）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    /// 构造仅含文本的消息（保留旧调用点的便利）。
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
}

/// Unified chat completion request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// OpenAI functions 格式的工具声明；透传给上游。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    /// OpenAI `tool_choice`：`"auto"` / `"required"` / `{type:"function", function:{name}}`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    /// OpenAI 协议入口透传模式：保留原始请求体。
    /// 有值则 build_upstream_body 以此为基底定点覆盖 model/stream_options，
    /// 其余字段原样上行；None（anthropic 转换路径）走原有重建逻辑。
    #[serde(skip)]
    pub raw_body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatCompletionUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub usage: Option<ChatCompletionUsage>,
}

/// OpenAI-compatible error response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmErrorResponse {
    pub error: LlmErrorDetail,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
}

// ── Runtime state ─────────────────────────────────────────────

/// In-memory cache of providers + models for fast routing lookups.
#[derive(Debug, Clone)]
pub struct LlmState {
    /// Database reference for persisting changes.
    pub db: Option<Database>,
    /// Gateway config (domain, TLS settings).
    pub gateway_config: Arc<RwLock<Option<LlmGatewayConfig>>>,
    /// 字段加密器（提供商 API Key 等敏感字段的落库加密）；None 表示未配置主密钥。
    pub cipher: Option<crate::llm::crypto::LlmCipher>,
    /// RAG 向量存储（知识库检索）。
    #[cfg(feature = "rag")]
    pub rag_store: rag::store::VectorStore,
    /// RAG 摄入状态事件通道（SSE 推送给前端）。
    #[cfg(feature = "rag")]
    pub rag_tx: tokio::sync::broadcast::Sender<rag::ingest::KbEvent>,
    /// 动态配置引用（LLM 请求日志开关等）。默认开启，生产路径由 init_llm_state 注入真实实例。
    pub dynamic_config: Arc<RwLock<crate::dynamic_config::DynamicConfig>>,
    /// 按模型粒度的熔断器（故障转移时跳过持续失败的候选）。
    pub breakers: breaker::ModelBreakers,
    /// 路由实体（provider/model/group）内存缓存，避免请求热路径的 DB 往返。
    pub route_cache: route_cache::RouteCache,
    /// 确定性失败缓存（401/403/404 等"必然失败"的候选，TTL 内跳过网络调用）。
    pub known_failures: down::KnownFailures,
}

impl LlmState {
    /// 便捷构造：rag_store 指向系统临时目录，仅用于测试/演示。
    /// 生产初始化请用 [`Self::new_with_rag`] 指定数据目录（仅 `rag` feature 启用时可用）。
    pub fn new(db: Option<Database>, cipher: Option<crate::llm::crypto::LlmCipher>) -> Self {
        Self {
            db,
            gateway_config: Arc::new(RwLock::new(None)),
            cipher,
            #[cfg(feature = "rag")]
            rag_store: rag::store::VectorStore::new(Path::new(&std::env::temp_dir())),
            #[cfg(feature = "rag")]
            rag_tx: tokio::sync::broadcast::channel(256).0,
            dynamic_config: Arc::new(RwLock::new(
                crate::dynamic_config::DynamicConfig::default_for_llm(),
            )),
            breakers: breaker::ModelBreakers::new(),
            route_cache: route_cache::RouteCache::new(),
            known_failures: down::KnownFailures::new(),
        }
    }

    /// 指定 RAG 数据目录构造（知识库向量 shard 位于 `<rag_data_dir>/rag/<kb_id>/`）。
    #[cfg(feature = "rag")]
    pub fn new_with_rag(
        db: Option<Database>,
        cipher: Option<crate::llm::crypto::LlmCipher>,
        rag_data_dir: &Path,
    ) -> Self {
        Self {
            db,
            gateway_config: Arc::new(RwLock::new(None)),
            cipher,
            rag_store: rag::store::VectorStore::new(rag_data_dir),
            rag_tx: tokio::sync::broadcast::channel(256).0,
            dynamic_config: Arc::new(RwLock::new(
                crate::dynamic_config::DynamicConfig::default_for_llm(),
            )),
            breakers: breaker::ModelBreakers::new(),
            route_cache: route_cache::RouteCache::new(),
            known_failures: down::KnownFailures::new(),
        }
    }
}

/// LLM 请求日志中单个字符串字段的截断上限（字符）。
///
/// 完整对话正文（可能含用户 secrets）原样落盘是安全风险；日志保留结构
/// （role/name/tools 等）与正文前段，超长文本省略并标注截断字符数。
const MAX_LOG_STRING_CHARS: usize = 4000;

/// 脱敏请求体用于日志：递归截断超长字符串字段，避免完整用户内容（含可能的
/// secrets）原样落盘。结构保留，只对超长文本做省略。修改不影响内存中的原始
/// 请求体（clone 后处理）。
pub fn sanitize_request_body(body: &serde_json::Value) -> serde_json::Value {
    fn truncate(s: &mut String) {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= MAX_LOG_STRING_CHARS {
            return;
        }
        let head: String = chars.iter().take(MAX_LOG_STRING_CHARS).collect();
        *s = format!("{head}…[truncated {} chars]", chars.len() - MAX_LOG_STRING_CHARS);
    }
    fn walk(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::String(s) => truncate(s),
            serde_json::Value::Array(items) => {
                for item in items.iter_mut() {
                    walk(item);
                }
            }
            serde_json::Value::Object(map) => {
                for val in map.values_mut() {
                    walk(val);
                }
            }
            _ => {}
        }
    }
    let mut out = body.clone();
    walk(&mut out);
    out
}

/// 记录 LLM 请求日志（受 dynamic_config.llm_request_logging 开关控制）。
///
/// 在 LLM 网关入口调用（compat/RAG 改写后、上游调用前后）。
/// `request_body` 是发往上游的完整请求体，原样落地、不截断不简化——
/// 该日志用于排查上游兼容问题，看不到正文就没有意义。
/// 4xx/5xx 详细错误日志由 upstream.rs 的 llm_upstream/llm_upstream_debug 负责，不受此开关影响。
#[allow(clippy::too_many_arguments)]
pub async fn log_llm_request(
    llm: &LlmState,
    protocol: &str,
    model: &str,
    message_count: usize,
    has_tools: bool,
    stream: bool,
    status: Option<u16>,
    error: Option<&str>,
    elapsed_ms: u128,
    request_body: &serde_json::Value,
) {
    if !llm.dynamic_config.read().await.llm_request_logging {
        return;
    }
    // 全部字段用 %（record_str）输出：LogLayer 只把 record_str 字段拼进 message，
    // 裸字段走 record_debug 会被丢弃（之前日志里只剩 protocol/model/error 就是这个原因）。
    tracing::info!(
        target: "llm_request",
        protocol = %protocol,
        model = %model,
        message_count = %message_count,
        has_tools = %has_tools,
        stream = %stream,
        status = %status.map_or(0, i64::from),
        error = %error.unwrap_or(""),
        elapsed_ms = %elapsed_ms,
        request_body = %sanitize_request_body(request_body),
        "LLM request"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_legacy_domain_field() {
        // 旧持久化数据使用 "domain" 字段，应反序列化为 openai_domain
        let json = r#"{"enabled":true,"domain":"llm.example.com","listen":"0.0.0.0:443","tls_enabled":false,"tls_acme":false}"#;
        let cfg: LlmGatewayConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.openai_domain.as_deref(), Some("llm.example.com"));
        assert!(cfg.anthropic_domain.is_none());
    }

    #[test]
    fn test_deserialize_new_openai_domain_field() {
        let json = r#"{"enabled":true,"openai_domain":"oa.example.com","anthropic_domain":"an.example.com","listen":"0.0.0.0:443","tls_enabled":true,"tls_acme":true}"#;
        let cfg: LlmGatewayConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.openai_domain.as_deref(), Some("oa.example.com"));
        assert_eq!(cfg.anthropic_domain.as_deref(), Some("an.example.com"));
    }

    #[test]
    fn test_dynamic_config_default_enabled() {
        // 默认构造：llm_request_logging 必须开启（生产路径由 init_llm_state 注入真实实例覆盖）
        let state = LlmState::new(None, None);
        assert!(
            state.dynamic_config.blocking_read().llm_request_logging,
            "default dynamic_config should enable llm request logging"
        );
    }

    #[tokio::test]
    async fn test_log_llm_request_respects_disabled_flag() {
        let state = LlmState::new(None, None);
        // 关闭开关
        state.dynamic_config.write().await.llm_request_logging = false;
        // 开关关闭时应直接返回，不 panic
        let body = serde_json::json!({"model": "gpt-4", "messages": []});
        log_llm_request(
            &state,
            "openai",
            "gpt-4",
            1,
            false,
            false,
            Some(200),
            None,
            10,
            &body,
        )
        .await;
    }

    #[test]
    fn test_sanitize_request_body_truncates_long_strings() {
        let long = "x".repeat(10_000);
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [
                {"role": "user", "content": long},
                {"role": "assistant", "content": "短文本"},
            ],
            "tools": [{"type": "function", "function": {"name": "Bash", "description": "run"}}],
            "max_tokens": 100,
            "nested": {"arr": ["a".repeat(5000), 42, null]}
        });
        let out = sanitize_request_body(&body);

        // 超长字符串被截断且带省略标记
        let c0 = out["messages"][0]["content"].as_str().unwrap();
        assert!(
            c0.ends_with("[truncated 6000 chars]"),
            "应有截断标记: {c0:?}"
        );
        assert!(c0.contains('…'), "应含省略号: {c0:?}");
        assert!(c0.starts_with(&"x".repeat(MAX_LOG_STRING_CHARS)));
        // 短字符串原样保留
        assert_eq!(out["messages"][1]["content"], "短文本");
        // 非字符串字段不变
        assert_eq!(out["max_tokens"], 100);
        assert_eq!(out["tools"][0]["function"]["name"], "Bash");
        // 嵌套数组/对象也递归截断
        assert!(out["nested"]["arr"][0]
            .as_str()
            .unwrap()
            .ends_with("[truncated 1000 chars]"));
        // 原始 body 不受影响
        assert_eq!(body["messages"][0]["content"].as_str().unwrap().len(), 10_000);
    }

    #[test]
    fn test_sanitize_request_body_leaves_small_body_intact() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });
        assert_eq!(sanitize_request_body(&body), body);
    }

    #[tokio::test]
    async fn test_log_llm_request_default_enabled() {
        let state = LlmState::new(None, None);
        // 默认开启，调用不应 panic
        let body =
            serde_json::json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});
        log_llm_request(
            &state,
            "openai",
            "gpt-4",
            1,
            false,
            false,
            Some(200),
            None,
            10,
            &body,
        )
        .await;
    }

    /// 回归测试：日志页面曾只显示 "LLM request" 几个字。
    /// 原因是 LogLayer 的 FieldVisitor 只把 record_str 字段拼进 message，
    /// 裸字段（record_debug）除 message 外全部丢弃。
    /// 这里端到端走真实 LogLayer，断言所有字段都出现在最终 message 中。
    #[tokio::test]
    async fn test_log_llm_request_fields_reach_log_message() {
        use crate::mgmt::logs::{LogLayer, LogStore};
        use tracing_subscriber::layer::SubscriberExt;

        let store = LogStore::new_in_memory();
        let subscriber = tracing_subscriber::registry().with(LogLayer::new(store.clone()));

        let state = LlmState::new(None, None);
        let body =
            serde_json::json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]});

        // set_default guard 覆盖整个 await（with_default 对 async 块的注册语义不可靠）
        let _guard = tracing::subscriber::set_default(subscriber);
        log_llm_request(
            &state,
            "openai",
            "gpt-4",
            3,
            true,
            false,
            Some(200),
            None,
            42,
            &body,
        )
        .await;

        // send → ring buffer 由后台 task 转发，轮询等待落地
        let mut msg = String::new();
        for _ in 0..50 {
            if let Some(entry) = store
                .get_all()
                .await
                .into_iter()
                .find(|e| e.target == "llm_request")
            {
                msg = entry.message;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!msg.is_empty(), "no llm_request log entry captured");
        assert!(msg.contains("LLM request"), "msg={msg}");
        assert!(msg.contains("protocol=openai"), "msg={msg}");
        assert!(msg.contains("model=gpt-4"), "msg={msg}");
        assert!(msg.contains("message_count=3"), "msg={msg}");
        assert!(msg.contains("has_tools=true"), "msg={msg}");
        assert!(msg.contains("status=200"), "msg={msg}");
        assert!(msg.contains("elapsed_ms=42"), "msg={msg}");
        assert!(msg.contains("request_body="), "msg={msg}");
        assert!(msg.contains("\"content\":\"hi\""), "msg={msg}");
    }

    #[test]
    fn test_validate_rejects_both_empty_when_enabled() {
        let cfg = LlmGatewayConfig {
            enabled: true,
            openai_domain: None,
            anthropic_domain: None,
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        };
        assert!(cfg.validate().is_some());
    }

    #[test]
    fn test_validate_accepts_one_domain() {
        let cfg = LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("oa.local".into()),
            anthropic_domain: None,
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        };
        assert!(cfg.validate().is_none());
    }

    #[test]
    fn test_validate_rejects_duplicate_domains() {
        let cfg = LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("same.local".into()),
            anthropic_domain: Some("same.local".into()),
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        };
        assert!(cfg.validate().is_some());
    }

    #[test]
    fn test_configured_domains_always_returns_two_elements() {
        // OpenAI only
        let cfg = LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("oa.local".into()),
            anthropic_domain: None,
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        };
        assert_eq!(cfg.configured_domains(), vec!["oa.local", ""]);

        // Anthropic only
        let cfg = LlmGatewayConfig {
            enabled: true,
            openai_domain: None,
            anthropic_domain: Some("an.local".into()),
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        };
        assert_eq!(cfg.configured_domains(), vec!["", "an.local"]);

        // Both
        let cfg = LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("oa.local".into()),
            anthropic_domain: Some("an.local".into()),
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        };
        assert_eq!(cfg.configured_domains(), vec!["oa.local", "an.local"]);
    }

    #[test]
    fn test_match_protocol_routing() {
        let cfg = LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("oa.local".into()),
            anthropic_domain: Some("an.local".into()),
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        };
        assert_eq!(cfg.match_protocol("oa.local"), Some(LlmProtocol::OpenAI));
        assert_eq!(cfg.match_protocol("an.local"), Some(LlmProtocol::Anthropic));
        assert_eq!(cfg.match_protocol("other.local"), None);
        assert_eq!(cfg.match_protocol(""), None);
    }
}
