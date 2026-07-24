pub mod anthropic_handler;
pub mod auth;
pub mod crypto;
pub mod format;
pub mod openai_handler;
pub mod provider;
pub mod router;
pub mod upstream;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::server::db::Database;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
            return Some("at least one of openai_domain or anthropic_domain is required when enabled".into());
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

    /// 收集所有已配置的非空域名（用于 ProxyRule.domains 持久化）。
    pub fn configured_domains(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(ref d) = self.openai_domain {
            if !d.is_empty() {
                v.push(d.clone());
            }
        }
        if let Some(ref d) = self.anthropic_domain {
            if !d.is_empty() {
                v.push(d.clone());
            }
        }
        v
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

/// Request body for creating/updating a provider.
#[derive(Debug, serde::Deserialize)]
pub struct ProviderRequest {
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    pub extra_config: Option<String>,
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
}

/// Request body for creating an API key.
#[derive(Debug, serde::Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
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
    pub cipher: Option<crate::server::llm::crypto::LlmCipher>,
}

impl LlmState {
    pub fn new(
        db: Option<Database>,
        cipher: Option<crate::server::llm::crypto::LlmCipher>,
    ) -> Self {
        Self {
            db,
            gateway_config: Arc::new(RwLock::new(None)),
            cipher,
        }
    }
}
