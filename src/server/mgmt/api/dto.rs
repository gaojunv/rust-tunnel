use serde::{Deserialize, Serialize};

/// Log entry response
#[derive(Debug, Serialize)]
pub struct LogEntryResponse {
    pub id: i64,
    pub timestamp: i64,
    pub level: String,
    pub source: String,
    pub target: String,
    pub message: String,
}

/// Query parameters for GET /api/logs
#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub level: Option<String>,
    pub source: Option<String>,
    pub search: Option<String>,
    pub limit: Option<u32>,
    pub before_id: Option<i64>,
}

/// Request body for PUT /api/logs/level
#[derive(Debug, Deserialize)]
pub struct SetLevelRequest {
    pub level: String,
}

/// Request body for PUT /api/logs/llm-logging
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct SetLlmLoggingRequest {
    pub enabled: bool,
}

/// SSE query params (for token-based auth)
#[derive(Debug, Deserialize)]
pub struct SseQuery {
    pub level: Option<String>,
    pub source: Option<String>,
    pub token: Option<String>,
}

/// Login request
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

/// Login response
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub auth_required: bool,
}

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

/// Shadowsocks configuration
#[derive(Debug, Serialize)]
pub struct ShadowsocksConfig {
    pub enabled: bool,
    pub port: Option<u16>,
    pub cipher: Option<String>,
}

/// Trojan configuration
#[derive(Debug, Serialize)]
pub struct TrojanConfig {
    pub enabled: bool,
    pub port: Option<u16>,
    pub fallback: Option<String>,
    pub domain: Option<String>,
    /// 证书来源："acme_exact" | "acme_wildcard" | "self_signed"；未运行时为 null
    pub cert_source: Option<String>,
    /// true = 与反代共享端口（SNI 分流）；false = 独立监听
    pub shared: bool,
}

/// Mesh network info response
#[derive(Debug, Serialize)]
pub struct MeshNetworkResponse {
    pub id: String,
    pub members: Vec<MeshMemberResponse>,
    pub services: Vec<MeshServiceResponse>,
}

#[derive(Debug, Serialize)]
pub struct MeshMemberResponse {
    pub client_name: String,
    pub public_addr: Option<String>,
    pub p2p_available: bool,
    pub online: bool,
}

#[derive(Debug, Serialize)]
pub struct MeshServiceResponse {
    pub service_name: String,
    pub protocol: String,
    pub local_addr: String,
    pub client_name: String,
}

/// DNS record response
#[derive(Debug, Serialize)]
pub struct DnsRecordResponse {
    pub name: String,
    pub record_type: String,
    pub value: String,
}

/// Request to add a manual DNS record
#[derive(Debug, Deserialize)]
pub struct AddDnsRecordRequest {
    pub name: String,
    pub record_type: String,
    pub value: String,
    pub port: Option<u16>,
}

// ── LLM 模型组（多模型故障转移）──────────────────────────────────

/// 模型组视图（列表用）。
#[derive(Debug, Serialize)]
pub struct ModelGroupView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub member_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// 创建/更新模型组请求。
#[derive(Debug, Deserialize)]
pub struct ModelGroupRequest {
    pub name: String,
    pub enabled: Option<bool>,
}

/// 组成员输入。
#[derive(Debug, Deserialize)]
pub struct GroupMemberInput {
    pub model_id: String,
    pub priority: i32,
}

/// 整体替换成员请求。
#[derive(Debug, Deserialize)]
pub struct ReplaceMembersRequest {
    pub members: Vec<GroupMemberInput>,
}

/// 熔断快照视图。
#[derive(Debug, Serialize)]
pub struct BreakerSnapshotView {
    pub state: String,
    pub consecutive_failures: u32,
    pub cooldown_remaining_secs: u64,
}

/// 组成员视图（详情用）。
#[derive(Debug, Serialize)]
pub struct GroupMemberView {
    pub model_id: String,
    pub priority: i32,
    pub model_name: String,
    pub alias: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model_enabled: bool,
    pub breaker: BreakerSnapshotView,
}

/// 模型组详情视图。
#[derive(Debug, Serialize)]
pub struct ModelGroupDetailView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
    pub members: Vec<GroupMemberView>,
}
