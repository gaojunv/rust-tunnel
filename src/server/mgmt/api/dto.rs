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
