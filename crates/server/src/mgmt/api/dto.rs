use serde::{Deserialize, Serialize};

/// 日志条目响应（单条日志）。
#[derive(Debug, Serialize)]
pub struct LogEntryResponse {
    /// 日志条目 ID（自增主键）。
    pub id: i64,
    /// 时间戳（Unix 毫秒）。
    pub timestamp: i64,
    /// 日志级别（trace/debug/info/warn/error）。
    pub level: String,
    /// 日志来源（模块/组件名）。
    pub source: String,
    /// tracing target。
    pub target: String,
    /// 日志消息正文。
    pub message: String,
}

/// Query parameters for GET /api/logs
#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    /// 按日志级别过滤。
    pub level: Option<String>,
    /// 按来源模块过滤。
    pub source: Option<String>,
    /// 关键词搜索（消息/来源/target）。
    pub search: Option<String>,
    /// 返回条数上限。
    pub limit: Option<u32>,
    /// 分页游标：仅返回 ID 小于该值的记录。
    pub before_id: Option<i64>,
}

/// Request body for PUT /api/logs/level
#[derive(Debug, Deserialize)]
pub struct SetLevelRequest {
    /// 目标日志级别。
    pub level: String,
}

/// Request body for PUT /api/logs/llm-logging
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct SetLlmLoggingRequest {
    /// 是否启用 LLM 请求日志。
    pub enabled: bool,
}

/// SSE query params (for token-based auth)
#[derive(Debug, Deserialize)]
pub struct SseQuery {
    /// 日志级别过滤（SSE 流）。
    pub level: Option<String>,
    /// 来源过滤（SSE 流）。
    pub source: Option<String>,
    /// 基于 URL 的认证 token（SSE 无法携带 Header 时使用）。
    pub token: Option<String>,
}

/// Login request
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// 登录密码。
    pub password: String,
}

/// Login response
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// 认证通过后签发的 JWT。
    pub token: String,
    /// 是否需要认证（未设置密码时为 false）。
    pub auth_required: bool,
}

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// 服务状态（固定为 "ok"）。
    pub status: &'static str,
}

/// Shadowsocks configuration
#[derive(Debug, Serialize)]
pub struct ShadowsocksConfig {
    /// 是否启用 Shadowsocks。
    pub enabled: bool,
    /// 监听端口（未启用时为 None）。
    pub port: Option<u16>,
    /// 加密方式（未启用时为 None）。
    pub cipher: Option<String>,
}

/// Trojan configuration
#[derive(Debug, Serialize)]
pub struct TrojanConfig {
    /// 是否启用 Trojan。
    pub enabled: bool,
    /// 监听端口（未启用时为 None）。
    pub port: Option<u16>,
    /// 认证失败时的回落地址（未配置时为 None）。
    pub fallback: Option<String>,
    /// 绑定的域名（用于 ACME/SNI，未配置时为 None）。
    pub domain: Option<String>,
    /// 证书来源："acme_exact" | "acme_wildcard" | "self_signed"；未运行时为 null
    pub cert_source: Option<String>,
    /// true = 与反代共享端口（SNI 分流）；false = 独立监听
    pub shared: bool,
}

/// Mesh 网络信息响应。
#[derive(Debug, Serialize)]
pub struct MeshNetworkResponse {
    /// Mesh 网络 ID。
    pub id: String,
    /// 成员列表。
    pub members: Vec<MeshMemberResponse>,
    /// 已注册服务列表。
    pub services: Vec<MeshServiceResponse>,
}

/// Mesh 成员视图。
#[derive(Debug, Serialize)]
pub struct MeshMemberResponse {
    /// 客户端名称。
    pub client_name: String,
    /// 公网地址（未探测到时为 None）。
    pub public_addr: Option<String>,
    /// 是否可建立 P2P 直连。
    pub p2p_available: bool,
    /// 是否在线。
    pub online: bool,
}

/// Mesh 服务视图。
#[derive(Debug, Serialize)]
pub struct MeshServiceResponse {
    /// 服务名称。
    pub service_name: String,
    /// 协议（tcp/udp/http）。
    pub protocol: String,
    /// 本地监听地址。
    pub local_addr: String,
    /// 所属客户端名称。
    pub client_name: String,
}

/// DNS record response
#[derive(Debug, Serialize)]
pub struct DnsRecordResponse {
    /// 记录名。
    pub name: String,
    /// 记录类型（A/CNAME 等）。
    pub record_type: String,
    /// 记录值。
    pub value: String,
}

/// Request to add a manual DNS record
#[derive(Debug, Deserialize)]
pub struct AddDnsRecordRequest {
    /// 记录名。
    pub name: String,
    /// 记录类型。
    pub record_type: String,
    /// 记录值。
    pub value: String,
    /// 端口（SRV 等需要端口的记录类型）。
    pub port: Option<u16>,
}

// ── LLM 模型组（多模型故障转移）──────────────────────────────────

/// 模型组视图（列表用）。
#[derive(Debug, Serialize)]
pub struct ModelGroupView {
    /// 组 ID。
    pub id: String,
    /// 组名。
    pub name: String,
    /// 是否启用。
    pub enabled: bool,
    /// 成员数量。
    pub member_count: i64,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间（RFC3339）。
    pub updated_at: String,
}

/// 创建/更新模型组请求。
#[derive(Debug, Deserialize)]
pub struct ModelGroupRequest {
    /// 组名。
    pub name: String,
    /// 是否启用（None 表示保持不变/默认启用）。
    pub enabled: Option<bool>,
}

/// 组成员输入。
#[derive(Debug, Deserialize)]
pub struct GroupMemberInput {
    /// 模型 ID。
    pub model_id: String,
    /// 优先级（越小越优先）。
    pub priority: i32,
}

/// 整体替换成员请求。
#[derive(Debug, Deserialize)]
pub struct ReplaceMembersRequest {
    /// 新成员列表（整体替换）。
    pub members: Vec<GroupMemberInput>,
}

/// 熔断快照视图。
#[derive(Debug, Serialize)]
pub struct BreakerSnapshotView {
    /// 熔断状态（closed/open/half_open）。
    pub state: String,
    /// 连续失败次数。
    pub consecutive_failures: u32,
    /// 剩余冷却时间（秒）。
    pub cooldown_remaining_secs: u64,
}

/// 组成员视图（详情用）。
#[derive(Debug, Serialize)]
pub struct GroupMemberView {
    /// 模型 ID。
    pub model_id: String,
    /// 优先级。
    pub priority: i32,
    /// 模型名。
    pub model_name: String,
    /// 别名。
    pub alias: String,
    /// 所属 provider ID。
    pub provider_id: String,
    /// 所属 provider 名称。
    pub provider_name: String,
    /// 模型是否启用。
    pub model_enabled: bool,
    /// 熔断快照。
    pub breaker: BreakerSnapshotView,
}

/// 模型组详情视图。
#[derive(Debug, Serialize)]
pub struct ModelGroupDetailView {
    /// 组 ID。
    pub id: String,
    /// 组名。
    pub name: String,
    /// 是否启用。
    pub enabled: bool,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间（RFC3339）。
    pub updated_at: String,
    /// 成员列表。
    pub members: Vec<GroupMemberView>,
}
