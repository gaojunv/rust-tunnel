use chrono::{DateTime, Utc};
use sqlx::FromRow;

/// Shadowsocks config record from database
#[derive(FromRow, Debug)]
pub struct ShadowsocksConfigRecord {
    pub id: i32,
    pub port: i32,
    pub cipher: String,
    pub password: String,
    pub enabled: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A log entry row from the database
#[derive(FromRow, Debug, Clone)]
pub struct DbLogEntry {
    pub id: i64,
    pub timestamp: i64,
    pub level: String,
    pub source: String,
    pub target: String,
    pub message: String,
}

/// Trojan config record from database
#[derive(FromRow, Debug)]
pub struct TrojanConfigRecord {
    pub id: i32,
    pub port: i32,
    pub password: String,
    pub fallback: String,
    pub enabled: i32,
    pub domain: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Mesh network record from database
#[derive(FromRow, Debug)]
pub struct MeshNetworkRecord {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub description: Option<String>,
}

/// Mesh service record from database
#[derive(FromRow, Debug)]
pub struct MeshServiceRecord {
    pub id: i32,
    pub mesh_id: String,
    pub client_name: String,
    pub service_name: String,
    pub protocol: String,
    pub local_addr: String,
    pub dns_record: String,
}

/// Proxy rule record from database
#[derive(FromRow, Debug, Clone)]
pub struct ProxyRuleRecord {
    pub id: String,
    pub name: String,
    #[sqlx(rename = "type")]
    pub rule_type: String,
    pub listen_addr: String,
    pub domains: Option<String>,
    pub routes: Option<String>,
    pub tls_enabled: i32,
    pub tls_acme: i32,
    pub tls_domain: Option<String>,
    pub enabled: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cert_source: Option<String>,
    pub cert_covering_domain: Option<String>,
    pub cert_status_updated_at: Option<DateTime<Utc>>,
}

/// DB row mirroring stats_snapshots
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct StatsSnapshotRow {
    pub entity_type: String,
    pub entity_id: String,
    pub timestamp: DateTime<Utc>,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub bytes_in_rate: f64,
    pub bytes_out_rate: f64,
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub active_conns: i32,
}

/// ACME certificate record from database
#[derive(FromRow, Debug, Clone)]
pub struct AcmeCertificateRecord {
    pub domain: String,
    pub status: String,
    pub cert_pem: Option<String>,
    pub key_pem: Option<String>,
    pub chain_pem: Option<String>,
    pub issued_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub auto_renew: i32,
    pub last_renewal_attempt: Option<DateTime<Utc>>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// ACME challenge record from database
#[derive(FromRow, Debug)]
pub struct AcmeChallengeRecord {
    pub token: String,
    pub domain: String,
    pub authorization: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Reverse proxy config record
#[derive(Debug, Clone, FromRow)]
pub struct ReverseProxyConfigRecord {
    pub max_connections: i64,
    pub connection_timeout_secs: i64,
    pub buffer_size: i64,
}

/// DNS config record
#[derive(Debug, Clone, FromRow)]
pub struct DnsConfigRecord {
    pub tunnel_domain: String,
    pub mesh_domain: String,
}

/// Client registry record from database
#[derive(Debug, Clone, FromRow)]
pub struct ClientRecord {
    pub name: String,
    pub hostname: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub note: Option<String>,
}