use chrono::{DateTime, Utc};
use sqlx::FromRow;

/// Shadowsocks config record from database
#[derive(FromRow, Debug)]
pub struct ShadowsocksConfigRecord {
    /// 主键 id。
    pub id: i32,
    /// 监听端口。
    pub port: i32,
    /// 加密方式。
    pub cipher: String,
    /// 连接密码。
    pub password: String,
    /// 是否启用（1 启用，0 禁用）。
    pub enabled: i32,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 更新时间。
    pub updated_at: DateTime<Utc>,
}

/// A log entry row from the database
#[derive(FromRow, Debug, Clone)]
pub struct DbLogEntry {
    /// 主键 id。
    pub id: i64,
    /// 日志时间戳（毫秒）。
    pub timestamp: i64,
    /// 日志级别。
    pub level: String,
    /// 日志来源模块。
    pub source: String,
    /// 日志目标（tracing target）。
    pub target: String,
    /// 日志内容。
    pub message: String,
}

/// Trojan config record from database
#[derive(FromRow, Debug)]
pub struct TrojanConfigRecord {
    /// 主键 id。
    pub id: i32,
    /// 监听端口。
    pub port: i32,
    /// 连接密码。
    pub password: String,
    /// 认证失败回退地址。
    pub fallback: String,
    /// 是否启用（1 启用，0 禁用）。
    pub enabled: i32,
    /// 关联域名。
    pub domain: String,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 更新时间。
    pub updated_at: DateTime<Utc>,
}

/// Mesh network record from database
#[derive(FromRow, Debug)]
pub struct MeshNetworkRecord {
    /// 网络 id。
    pub id: String,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 网络描述，未填写为 None。
    pub description: Option<String>,
}

/// Mesh service record from database
#[derive(FromRow, Debug)]
pub struct MeshServiceRecord {
    /// 主键 id。
    pub id: i32,
    /// 所属 mesh 网络 id。
    pub mesh_id: String,
    /// 归属客户端名称。
    pub client_name: String,
    /// 服务名称。
    pub service_name: String,
    /// 协议类型。
    pub protocol: String,
    /// 本地服务地址。
    pub local_addr: String,
    /// DNS 记录。
    pub dns_record: String,
}

/// Proxy rule record from database
#[derive(FromRow, Debug, Clone)]
pub struct ProxyRuleRecord {
    /// 规则 id。
    pub id: String,
    /// 规则名称。
    pub name: String,
    /// 规则类型（http/tcp 等）。
    #[sqlx(rename = "type")]
    pub rule_type: String,
    /// 监听地址。
    pub listen_addr: String,
    /// 关联域名列表（JSON），未配置为 None。
    pub domains: Option<String>,
    /// 路由配置（JSON），未配置为 None。
    pub routes: Option<String>,
    /// 是否启用 TLS（1 启用，0 禁用）。
    pub tls_enabled: i32,
    /// 是否启用 ACME 自动证书（1 启用，0 禁用）。
    pub tls_acme: i32,
    /// TLS 证书域名，未配置为 None。
    pub tls_domain: Option<String>,
    /// 是否启用该规则（1 启用，0 禁用）。
    pub enabled: i32,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 更新时间。
    pub updated_at: DateTime<Utc>,
    /// 证书来源，未配置为 None。
    pub cert_source: Option<String>,
    /// 证书覆盖域名，未配置为 None。
    pub cert_covering_domain: Option<String>,
    /// 证书状态更新时间，未更新为 None。
    pub cert_status_updated_at: Option<DateTime<Utc>>,
}

/// DB row mirroring stats_snapshots
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct StatsSnapshotRow {
    /// 实体类型。
    pub entity_type: String,
    /// 实体 id。
    pub entity_id: String,
    /// 快照时间戳。
    pub timestamp: DateTime<Utc>,
    /// 入站字节数。
    pub bytes_in: i64,
    /// 出站字节数。
    pub bytes_out: i64,
    /// 入站速率（字节/秒）。
    pub bytes_in_rate: f64,
    /// 出站速率（字节/秒）。
    pub bytes_out_rate: f64,
    /// 往返时延（毫秒），未采集为 None。
    pub rtt_ms: Option<f64>,
    /// 丢包率（百分比），未采集为 None。
    pub loss_pct: Option<f64>,
    /// 活跃连接数。
    pub active_conns: i32,
}

/// ACME certificate record from database
#[derive(FromRow, Debug, Clone)]
pub struct AcmeCertificateRecord {
    /// 关联域名。
    pub domain: String,
    /// 证书状态。
    pub status: String,
    /// 证书 PEM，未签发为 None。
    pub cert_pem: Option<String>,
    /// 私钥 PEM，未签发为 None。
    pub key_pem: Option<String>,
    /// 证书链 PEM，未配置为 None。
    pub chain_pem: Option<String>,
    /// 签发时间，未签发为 None。
    pub issued_at: Option<DateTime<Utc>>,
    /// 过期时间，未签发为 None。
    pub expires_at: Option<DateTime<Utc>>,
    /// 是否自动续期（1 是，0 否）。
    pub auto_renew: i32,
    /// 上次续期尝试时间，未尝试为 None。
    pub last_renewal_attempt: Option<DateTime<Utc>>,
    /// 错误信息，无错误为 None。
    pub error_message: Option<String>,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
}

/// ACME challenge record from database
#[derive(FromRow, Debug)]
pub struct AcmeChallengeRecord {
    /// 挑战 token。
    pub token: String,
    /// 关联域名。
    pub domain: String,
    /// 授权标识。
    pub authorization: String,
    /// 挑战状态。
    pub status: String,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 过期时间，未设置过期为 None。
    pub expires_at: Option<DateTime<Utc>>,
}

/// Reverse proxy config record
#[derive(Debug, Clone, FromRow)]
pub struct ReverseProxyConfigRecord {
    /// 最大并发连接数。
    pub max_connections: i64,
    /// 连接超时时间（秒）。
    pub connection_timeout_secs: i64,
    /// 缓冲区大小（字节）。
    pub buffer_size: i64,
}

/// DNS config record
#[derive(Debug, Clone, FromRow)]
pub struct DnsConfigRecord {
    /// 隧道域名。
    pub tunnel_domain: String,
    /// Mesh 网络域名。
    pub mesh_domain: String,
}

/// Client registry record from database
#[derive(Debug, Clone, FromRow)]
pub struct ClientRecord {
    /// 客户端名称（主键）。
    pub name: String,
    /// 客户端主机名，未上报为 None。
    pub hostname: Option<String>,
    /// 首次出现时间。
    pub first_seen_at: DateTime<Utc>,
    /// 最后出现时间。
    pub last_seen_at: DateTime<Utc>,
    /// 备注，未填写为 None。
    pub note: Option<String>,
}
