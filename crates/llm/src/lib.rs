//! LLM 网关：OpenAI / Anthropic / Responses 三协议入口、模型路由与故障转移、
//! 熔断与确定性失败缓存、用量日志、compat 工具调用改写。
//!
//! RAG 注入经 `rag` feature 挂接 `rust-tunnel-rag`；本 crate 不依赖 server
//! 装配层类型（`DynamicConfig`/`ReverseProxyState`），接缝为 [`LlmGatewayRuleInput`]
//! 与 [`LlmState::request_logging`] 原子开关。

// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

/// Anthropic 协议处理器。
pub mod anthropic_handler;
/// 认证与鉴权。
pub mod auth;
/// 熔断器。
pub mod breaker;
/// 兼容性工具调用改写。
pub mod compat;
/// 字段加密。
pub mod crypto;
/// 确定性失败缓存。
pub mod down;
/// 格式转换与透传。
pub mod format;
/// OpenAI 协议处理器。
pub mod openai_handler;
/// 统一流水线与故障转移。
pub mod pipeline;
/// 提供商与模型管理。
pub mod provider;
// RAG 已拆分为独立 crate；`llm::rag` 路径经 re-export 保持兼容
/// RAG 能力重导出（`rust-tunnel-rag`），保持 `llm::rag` 兼容路径。
#[cfg(feature = "rag")]
pub use rust_tunnel_rag as rag;
/// Responses API 类型与转换。
pub mod responses;
/// Responses API 处理器。
pub mod responses_handler;
/// 路由缓存（provider/model/group 快照）。
pub mod route_cache;
/// 路由与故障转移。
pub mod router;
/// 上游 HTTP 客户端与错误映射。
pub mod upstream;
/// 用量采集与落库。
pub mod usage;

#[cfg(feature = "rag")]
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use rust_tunnel_persistence::Database;

// ── Configuration types ───────────────────────────────────────

/// 协议入口标识 — 双域名隔离用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProtocol {
    /// OpenAI 兼容协议入口。
    OpenAI,
    /// Anthropic 原生协议入口。
    Anthropic,
}

/// Gateway-level configuration (persisted as part of the LLM ProxyRule).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LlmGatewayConfig {
    /// 是否启用 LLM 网关。
    pub enabled: bool,
    /// OpenAI 协议入口域名；None/空 = 不开放 OpenAI 入口。
    #[serde(default, alias = "domain", skip_serializing_if = "Option::is_none")]
    pub openai_domain: Option<String>,
    /// Anthropic 协议入口域名；None/空 = 不开放 Anthropic 入口。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_domain: Option<String>,
    /// 监听地址（如 `0.0.0.0:443`）。
    pub listen: String,
    /// 是否启用 TLS。
    pub tls_enabled: bool,
    /// 是否通过 ACME 自动签发证书。
    pub tls_acme: bool,
}

impl LlmGatewayConfig {
    /// 校验配置合法性，返回 None 表示合法。
    /// enabled=true 时至少一个域名非空；两个域名都非空时必须不同。
    #[must_use]
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
    #[must_use]
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
    /// `init_llm_state` 按位置索引还原（`domains[0]` → openai, `domains[1]` → anthropic）。
    #[must_use]
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
    /// 提供商唯一标识。
    pub id: String,
    /// 提供商展示名。
    pub name: String,
    /// 提供商类型标识（如 openai、anthropic）。
    pub provider_type: String,
    /// 上游 API 基地址。
    pub base_url: String,
    /// API key is NEVER returned in API responses. Set via update.
    #[serde(skip_serializing)]
    pub api_key: String,
    /// 额外配置 JSON（加密存储）。
    pub extra_config: Option<String>,
    /// Anthropic Messages API base URL; `None` = 不支持 Anthropic 协议。
    pub anthropic_base_url: Option<String>,
    /// 是否启用。
    pub enabled: bool,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
    /// 更新时间（ISO 8601）。
    pub updated_at: String,
}

/// 三态反序列化：字段缺失 → `None`；显式 `null` → `Some(None)`；字符串 → `Some(Some(s))`。
/// 裸 `Option<String>` 无法区分前两者（serde 把缺失/null 都映到 None），
/// 导致前端“清除 extra_config”的显式 null 被误当“保留原值”，开关永远关不掉。
#[allow(
    clippy::option_option,
    reason = "三态语义需区分缺失、显式 null 与字符串，Option<Option<String>> 为 serde 已知模式"
)]
fn deserialize_nullable_string<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(<Option<String> as serde::Deserialize>::deserialize(
        d,
    )?))
}

/// 归一 `anthropic_base_url`：trim 后为空视为未配置（`None`）。
///
/// 读写两侧都调用：写侧拒绝空串落库（创建/更新把 `""` 归一成 NULL）；
/// 读侧（route_cache 快照构造 / provider 列表回显）防御性归一，使库里**已存的** `""`
/// 脏数据不再触发 Anthropic 直通——`Some("")` 时 `is_some()` 为 true 会误走直通，
/// 而前端输入框显示空、卡片也不渲染，用户完全看不出已配置。
#[must_use]
pub fn normalize_anthropic_base_url(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 创建/更新提供商的请求体。
#[derive(Debug, serde::Deserialize)]
pub struct ProviderRequest {
    /// 提供商展示名。
    pub name: String,
    /// 提供商类型标识。
    pub provider_type: String,
    /// 上游 API 基地址。
    pub base_url: String,
    /// 上游 API 密钥。
    pub api_key: String,
    /// 三态语义：缺失 = 不修改；`null` = 清除；字符串 = 覆盖。
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub extra_config: Option<Option<String>>,
    /// Anthropic Messages API base URL。
    /// 三态语义（同 extra_config）：字段缺失 = 不修改；显式 `null` = 清除；
    /// 字符串 = 覆盖（trim 后为空按未配置处理）。前端总是发送该字段、清空时为 null。
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub anthropic_base_url: Option<Option<String>>,
}

/// 模型配置（`llm_models` 表的一行）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelConfig {
    /// 模型唯一标识。
    pub id: String,
    /// 所属提供商 id。
    pub provider_id: String,
    /// 上游真实模型名。
    pub model_name: String,
    /// 对外别名（为空则直接暴露 `model_name`）。
    pub alias: String,
    /// 标签列表。
    pub tags: Vec<String>,
    /// per-model 配置（JSON 字符串，如 `{"agent_context_limit":200000}`）。非敏感，不加密。
    pub extra_config: Option<String>,
    /// 是否启用。
    pub enabled: bool,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
    /// 更新时间（ISO 8601）。
    pub updated_at: String,
}

/// 创建/更新模型的请求体。
#[derive(Debug, serde::Deserialize)]
pub struct ModelRequest {
    /// 上游真实模型名。
    pub model_name: String,
    /// 对外别名（为空则直接暴露 `model_name`）。
    pub alias: Option<String>,
    /// 标签列表。
    pub tags: Option<Vec<String>>,
    /// per-model 配置（JSON 字符串）。缺省/`null` = 不设置（或覆盖为无配置）。
    pub extra_config: Option<String>,
}

/// API key 视图（返回给管理界面，不含完整密钥）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiKeyView {
    /// 记录 id。
    pub id: String,
    /// 密钥前缀（用于展示，不含完整密钥）。
    pub key_prefix: String,
    /// 密钥名称。
    pub name: String,
    /// 是否启用。
    pub enabled: bool,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
    /// 最后使用时间（未使用过为 None）。
    pub last_used_at: Option<String>,
    /// 绑定的 RAG 知识库 id（未绑定为 None）。
    #[serde(default)]
    pub kb_id: Option<String>,
}

/// 创建 API key 的请求体。
#[derive(Debug, serde::Deserialize)]
pub struct CreateApiKeyRequest {
    /// 密钥名称。
    pub name: String,
    /// 可选：创建时即绑定 RAG 知识库（不存在则 400）。
    #[serde(default)]
    pub kb_id: Option<String>,
}

/// 创建 API key 的响应 — 完整密钥仅展示一次。
#[derive(Debug, serde::Serialize)]
pub struct CreateApiKeyResponse {
    /// 记录 id。
    pub id: String,
    /// 完整密钥（仅此次返回）。
    pub key: String, // full key — show once!
    /// 密钥前缀。
    pub key_prefix: String,
    /// 密钥名称。
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
    /// 角色（`user` / `assistant` / `system` / `tool`）。
    pub role: String,
    /// 文本内容；`None` 表示无文本（如纯工具调用消息）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// DeepSeek 思考模式的历史 `reasoning_content`：思考链开启后，上游（如
    /// Console Go 渠道）要求多轮对话里历史 assistant 消息必须原样携带该字段，
    /// 否则 400「The reasoning_content in the thinking mode must be passed back」。
    /// 由 Anthropic 历史 `thinking` 块映射而来（thinking → reasoning_content）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// 工具调用列表（assistant 发起的调用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    /// 工具调用结果对应的调用 id（`role=tool` 时使用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// 工具名（`role=tool` 时使用）。
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
    /// 请求模型名或别名。
    pub model: String,
    /// 对话消息列表。
    pub messages: Vec<ChatMessage>,
    /// 是否流式响应。
    pub stream: bool,
    /// 最大生成 token 数。
    pub max_tokens: Option<u32>,
    /// 采样温度。
    pub temperature: Option<f32>,
    /// 核采样阈值。
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

/// Chat Completions 单个选项。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatCompletionChoice {
    /// 选项序号。
    pub index: u32,
    /// 选项消息。
    pub message: ChatMessage,
    /// 结束原因（`stop` / `tool_calls` 等）。
    pub finish_reason: Option<String>,
}

/// Chat Completions 用量统计。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatCompletionUsage {
    /// 输入 token 数。
    pub prompt_tokens: u32,
    /// 输出 token 数。
    pub completion_tokens: u32,
    /// 总 token 数。
    pub total_tokens: u32,
}

/// Chat Completions 响应。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatCompletionResponse {
    /// 响应 id。
    pub id: String,
    /// 对象类型（`chat.completion`）。
    pub object: String,
    /// 创建时间戳（秒）。
    pub created: u64,
    /// 实际使用的模型名。
    pub model: String,
    /// 选项列表。
    pub choices: Vec<ChatCompletionChoice>,
    /// 用量统计。
    pub usage: Option<ChatCompletionUsage>,
}

/// OpenAI-compatible error response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmErrorResponse {
    /// 错误详情。
    pub error: LlmErrorDetail,
}

/// OpenAI 兼容错误详情。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmErrorDetail {
    /// 错误信息。
    pub message: String,
    /// 错误类型。
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
    pub cipher: Option<crate::crypto::LlmCipher>,
    /// RAG 向量存储（知识库检索）。
    #[cfg(feature = "rag")]
    pub rag_store: rag::store::VectorStore,
    /// RAG 摄入状态事件通道（SSE 推送给前端）。
    #[cfg(feature = "rag")]
    pub rag_tx: tokio::sync::broadcast::Sender<rag::ingest::KbEvent>,
    /// LLM 请求摘要日志开关（server `DynamicConfig.llm_request_logging` 的热路径投影）。
    /// 默认开启；装配层 init 时注入初值，运行时由 API handler 同步写。
    pub request_logging: Arc<std::sync::atomic::AtomicBool>,
    /// 按模型粒度的熔断器（故障转移时跳过持续失败的候选）。
    pub breakers: breaker::ModelBreakers,
    /// 上游 HTTP 客户端（连接池共享）。默认按 [`upstream::UpstreamClientConfig::default`]
    /// 构建；测试可替换为自定义超时/连接池配置的实例。
    pub upstream_client: reqwest::Client,
    /// 路由实体（provider/model/group）内存缓存，避免请求热路径的 DB 往返。
    pub route_cache: route_cache::RouteCache,
    /// 确定性失败缓存（401/403/404 等"必然失败"的候选，TTL 内跳过网络调用）。
    pub known_failures: down::KnownFailures,
}

impl LlmState {
    /// 便捷构造：rag_store 指向系统临时目录，仅用于测试/演示。
    /// 生产初始化请用 `Self::new_with_rag` 指定数据目录（仅 `rag` feature 启用时可用）。
    #[must_use]
    pub fn new(db: Option<Database>, cipher: Option<crate::crypto::LlmCipher>) -> Self {
        Self {
            db,
            gateway_config: Arc::new(RwLock::new(None)),
            cipher,
            #[cfg(feature = "rag")]
            rag_store: rag::store::VectorStore::new(Path::new(&std::env::temp_dir())),
            #[cfg(feature = "rag")]
            rag_tx: tokio::sync::broadcast::channel(256).0,
            request_logging: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            breakers: breaker::ModelBreakers::new(),
            upstream_client: upstream::UpstreamClientConfig::default().build_client(),
            route_cache: route_cache::RouteCache::new(),
            known_failures: down::KnownFailures::new(),
        }
    }

    /// 指定 RAG 数据目录构造（知识库向量 shard 位于 `<rag_data_dir>/rag/<kb_id>/`）。
    #[cfg(feature = "rag")]
    #[must_use]
    pub fn new_with_rag(
        db: Option<Database>,
        cipher: Option<crate::crypto::LlmCipher>,
        rag_data_dir: &Path,
    ) -> Self {
        Self {
            db,
            gateway_config: Arc::new(RwLock::new(None)),
            cipher,
            rag_store: rag::store::VectorStore::new(rag_data_dir),
            rag_tx: tokio::sync::broadcast::channel(256).0,
            request_logging: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            breakers: breaker::ModelBreakers::new(),
            upstream_client: upstream::UpstreamClientConfig::default().build_client(),
            route_cache: route_cache::RouteCache::new(),
            known_failures: down::KnownFailures::new(),
        }
    }
}

/// LLM Gateway 规则输入（装配层从反代 `ProxyRule` 提取转换，llm 不依赖 protocols 类型）。
#[derive(Debug, Clone)]
pub struct LlmGatewayRuleInput {
    /// 是否启用网关。
    pub enabled: bool,
    /// 已配置域名列表（`[openai_domain, anthropic_domain]`，空字符串表示未配置）。
    pub domains: Vec<String>,
    /// 监听地址。
    pub listen: String,
    /// 是否启用 TLS。
    pub tls_enabled: bool,
    /// 是否通过 ACME 自动签发证书。
    pub tls_acme: bool,
}

/// 从 gateway 规则输入构造 LlmState（装配层函数，替代原 ReverseProxyState::init_llm_state）。
pub async fn init_llm_state(
    gateway_rule: Option<LlmGatewayRuleInput>,
    db: Option<Database>,
    master_key: Option<[u8; 32]>,
    #[cfg_attr(not(feature = "rag"), allow(unused_variables))] rag_data_dir: &std::path::Path,
    request_logging_enabled: bool,
) -> Arc<LlmState> {
    let cipher = master_key.map(crate::crypto::LlmCipher::from_master_key);
    #[cfg(feature = "rag")]
    let llm = LlmState::new_with_rag(db, cipher, rag_data_dir);
    #[cfg(not(feature = "rag"))]
    let llm = LlmState::new(db, cipher);
    llm.request_logging.store(
        request_logging_enabled,
        std::sync::atomic::Ordering::Relaxed,
    );

    if let Some(rule) = gateway_rule {
        let (openai_domain, anthropic_domain) = if rule.domains.len() >= 2 {
            (
                Some(rule.domains[0].clone()).filter(|d| !d.is_empty()),
                Some(rule.domains[1].clone()).filter(|d| !d.is_empty()),
            )
        } else {
            let old = rule.domains.first().cloned().filter(|d| !d.is_empty());
            (old, None)
        };
        let config = LlmGatewayConfig {
            enabled: rule.enabled,
            openai_domain,
            anthropic_domain,
            listen: rule.listen.clone(),
            tls_enabled: rule.tls_enabled,
            tls_acme: rule.tls_acme,
        };
        *llm.gateway_config.write().await = Some(config);
    } else {
        *llm.gateway_config.write().await = Some(LlmGatewayConfig {
            enabled: false,
            openai_domain: None,
            anthropic_domain: None,
            listen: "0.0.0.0:443".to_string(),
            tls_enabled: false,
            tls_acme: false,
        });
    }

    Arc::new(llm)
}

/// 把已匹配 LLM Gateway 的请求分发给对应的 handler。
/// 按命中的协议入口严格限制接受的路径，跨协议路径返回协议各自的 404 风格。
///
/// 供 server 装配层的 `LlmDispatcher` 适配器调用。
pub async fn llm_handle(
    llm: Arc<LlmState>,
    protocol: LlmProtocol,
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::extract::State;
    use axum::http::{Method, StatusCode};
    use axum::Json;

    let state = openai_handler::LlmHandlerState {
        llm,
        protocol: Some(protocol),
    };
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let is_models = method == Method::GET && path == "/v1/models";
    let is_messages = method == Method::POST && path == "/v1/messages";
    let is_chat_completions = method == Method::POST && path == "/v1/chat/completions";
    let is_responses = method == Method::POST && path == "/v1/responses";
    let (parts, body) = req.into_parts();
    let mut headers = parts.headers;

    // HTTP/2 请求没有 Host 头，authority 由 hyper 映射到 URI；
    // 下游 handler 的 validate_host 只认 Host 头，这里按 h1 语义补齐。
    if !headers.contains_key(axum::http::header::HOST) {
        if let Some(authority) = parts.uri.authority() {
            if let Ok(value) = axum::http::HeaderValue::from_str(authority.as_str()) {
                headers.insert(axum::http::header::HOST, value);
            }
        }
    }

    // 严格协议隔离：按入口协议判定允许的路径
    let allowed = match protocol {
        LlmProtocol::OpenAI => is_models || is_chat_completions || is_responses,
        LlmProtocol::Anthropic => is_models || is_messages,
    };

    if !allowed {
        let etype = "invalid_request_error";
        let msg = "Not found".to_string();
        return match protocol {
            LlmProtocol::OpenAI => upstream::error_response(StatusCode::NOT_FOUND, msg, etype),
            LlmProtocol::Anthropic => {
                upstream::error_response_anthropic(StatusCode::NOT_FOUND, msg, etype)
            }
        };
    }

    if is_models {
        return openai_handler::handle_list_models(State(state), headers).await;
    }

    let bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("failed to read request body: {e}");
            return match protocol {
                LlmProtocol::OpenAI => {
                    upstream::error_response(StatusCode::BAD_REQUEST, msg, "invalid_request_error")
                }
                LlmProtocol::Anthropic => upstream::error_response_anthropic(
                    StatusCode::BAD_REQUEST,
                    msg,
                    "invalid_request_error",
                ),
            };
        }
    };
    if std::str::from_utf8(&bytes).is_err() {
        let msg = "request body is not valid UTF-8; JSON must be UTF-8 encoded (inline non-ASCII text in terminals like Windows cmd is often not UTF-8 — use \\uXXXX escapes or a UTF-8 file)".into();
        return match protocol {
            LlmProtocol::OpenAI => {
                upstream::error_response(StatusCode::BAD_REQUEST, msg, "invalid_request_error")
            }
            LlmProtocol::Anthropic => upstream::error_response_anthropic(
                StatusCode::BAD_REQUEST,
                msg,
                "invalid_request_error",
            ),
        };
    }
    let json: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("invalid JSON body: {e}");
            return match protocol {
                LlmProtocol::OpenAI => {
                    upstream::error_response(StatusCode::BAD_REQUEST, msg, "invalid_request_error")
                }
                LlmProtocol::Anthropic => upstream::error_response_anthropic(
                    StatusCode::BAD_REQUEST,
                    msg,
                    "invalid_request_error",
                ),
            };
        }
    };

    if is_messages {
        anthropic_handler::handle_messages(State(state), headers, Json(json)).await
    } else if is_responses {
        responses_handler::handle_responses(State(state), headers, Json(json)).await
    } else {
        openai_handler::handle_chat_completions(State(state), headers, Json(json)).await
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
#[must_use]
pub fn sanitize_request_body(body: &serde_json::Value) -> serde_json::Value {
    fn truncate(s: &mut String) {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= MAX_LOG_STRING_CHARS {
            return;
        }
        let head: String = chars.iter().take(MAX_LOG_STRING_CHARS).collect();
        *s = format!(
            "{head}…[truncated {} chars]",
            chars.len() - MAX_LOG_STRING_CHARS
        );
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
/// `log_llm_request` 参数包（批次 9d 收敛 too_many_arguments）：纯数据参数。
pub struct LogLlmRequestOpts {
    /// 协议标识（`openai` / `anthropic`）。
    pub protocol: String,
    /// 请求模型名。
    pub model: String,
    /// 消息条数。
    pub message_count: usize,
    /// 是否携带工具声明。
    pub has_tools: bool,
    /// 是否流式请求。
    pub stream: bool,
    /// 上游响应状态码。
    pub status: Option<u16>,
    /// 错误信息。
    pub error: Option<String>,
    /// 耗时（毫秒）。
    pub elapsed_ms: u128,
    /// 发往上游的请求体。
    pub request_body: serde_json::Value,
}

/// 记录一条 LLM 请求日志（受 `LlmState::request_logging` 开关控制，关闭时直接返回）。
#[allow(
    clippy::unused_async,
    reason = "与落库路径保持 async 调用统一，开关关闭时也需 await"
)]
pub async fn log_llm_request(llm: &LlmState, opts: &LogLlmRequestOpts) {
    if !llm
        .request_logging
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    // 全部字段用 %（record_str）输出：LogLayer 只把 record_str 字段拼进 message，
    // 裸字段走 record_debug 会被丢弃（之前日志里只剩 protocol/model/error 就是这个原因）。
    tracing::info!(
        target: "llm_request",
        protocol = %opts.protocol,
        model = %opts.model,
        message_count = %opts.message_count,
        has_tools = %opts.has_tools,
        stream = %opts.stream,
        status = %opts.status.map_or(0, i64::from),
        error = %opts.error.as_deref().unwrap_or(""),
        elapsed_ms = %opts.elapsed_ms,
        request_body = %sanitize_request_body(&opts.request_body),
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
    fn test_request_logging_default_enabled() {
        // 默认构造：request_logging 必须开启（生产路径由 init_llm_state 注入真实值覆盖）
        let state = LlmState::new(None, None);
        assert!(
            state
                .request_logging
                .load(std::sync::atomic::Ordering::Relaxed),
            "default request_logging should be enabled"
        );
    }

    #[tokio::test]
    async fn test_log_llm_request_respects_disabled_flag() {
        let state = LlmState::new(None, None);
        // 关闭开关
        state
            .request_logging
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // 开关关闭时应直接返回，不 panic
        let body = serde_json::json!({"model": "gpt-4", "messages": []});
        log_llm_request(
            &state,
            &LogLlmRequestOpts {
                protocol: "openai".to_owned(),
                model: "gpt-4".to_owned(),
                message_count: 1,
                has_tools: false,
                stream: false,
                status: Some(200),
                error: None,
                elapsed_ms: 10,
                request_body: body,
            },
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
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap().len(),
            10_000
        );
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
            &LogLlmRequestOpts {
                protocol: "openai".to_owned(),
                model: "gpt-4".to_owned(),
                message_count: 1,
                has_tools: false,
                stream: false,
                status: Some(200),
                error: None,
                elapsed_ms: 10,
                request_body: body,
            },
        )
        .await;
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
