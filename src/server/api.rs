use crate::server::quality::{ConnectionQuality, QualitySample};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, post, put},
    Json, Router,
};
use chrono::{DateTime, Timelike, Utc};
#[cfg(feature = "embed-frontend")]
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

use crate::common::DnsRecord;
use crate::server::auth::{auth_middleware, create_token, AuthConfig};
use crate::server::control::ServerState;
use crate::server::db::Database;
use crate::server::reverse_proxy::{ProxyRule, ProxyStats};
use axum::response::sse::{Event, KeepAlive, Sse};
use std::time::Duration;

/// Embedded frontend assets
#[cfg(feature = "embed-frontend")]
#[derive(RustEmbed)]
#[folder = "frontend-dist/"]
struct FrontendAssets;

/// Traffic record for a single time bucket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficBucket {
    pub timestamp: DateTime<Utc>,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// Traffic statistics for a port
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortTraffic {
    pub port: u16,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub buckets: VecDeque<TrafficBucket>,
}

/// Traffic store to track network statistics
#[derive(Clone)]
pub struct TrafficStore {
    inner: Arc<Mutex<std::collections::HashMap<u16, PortTraffic>>>,
    db: Option<Database>,
    /// Ports that have been updated since the last DB flush
    dirty_ports: Arc<std::sync::Mutex<std::collections::HashSet<u16>>>,
}

impl Default for TrafficStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TrafficStore {
    /// Create a new traffic store without database (for backwards compatibility)
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::collections::HashMap::new())),
            db: None,
            dirty_ports: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Create a new traffic store with database persistence
    pub fn with_db(db: Database) -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::collections::HashMap::new())),
            db: Some(db),
            dirty_ports: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Load traffic data from database
    pub async fn load_from_db(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(db) = &self.db {
            let mut store = self.inner.lock().await;

            // Load port traffic aggregates
            let port_records = db.load_port_traffic().await?;
            let bucket_records = db.load_recent_buckets(24).await?;

            // Build port traffic entries
            for record in port_records {
                let port = record.port as u16;
                let port_traffic = PortTraffic {
                    port,
                    total_bytes_in: record.total_bytes_in as u64,
                    total_bytes_out: record.total_bytes_out as u64,
                    buckets: VecDeque::new(),
                };
                store.insert(port, port_traffic);
            }

            // Add buckets to respective ports
            for bucket in bucket_records {
                let port = bucket.port as u16;
                if let Some(port_traffic) = store.get_mut(&port) {
                    port_traffic.buckets.push_back(TrafficBucket {
                        timestamp: bucket.timestamp,
                        bytes_in: bucket.bytes_in as u64,
                        bytes_out: bucket.bytes_out as u64,
                    });
                }
            }

            // Ensure buckets are within 24h limit for each port
            for port_traffic in store.values_mut() {
                while port_traffic.buckets.len() > 1440 {
                    port_traffic.buckets.pop_front();
                }
            }
        }
        Ok(())
    }

    /// Record incoming bytes (from user to server)
    pub async fn record_bytes_in(&self, port: u16, bytes: u64) {
        {
            let mut store = self.inner.lock().await;
            let port_traffic = store.entry(port).or_insert_with(|| PortTraffic {
                port,
                total_bytes_in: 0,
                total_bytes_out: 0,
                buckets: VecDeque::new(),
            });
            port_traffic.total_bytes_in += bytes;
            Self::add_to_bucket(port_traffic, bytes, 0);
        }
        // Mark port as dirty for periodic batch flush (no immediate DB write)
        if let Ok(mut dirty) = self.dirty_ports.lock() {
            dirty.insert(port);
        }
    }

    /// Record outgoing bytes (from server to user)
    pub async fn record_bytes_out(&self, port: u16, bytes: u64) {
        {
            let mut store = self.inner.lock().await;
            let port_traffic = store.entry(port).or_insert_with(|| PortTraffic {
                port,
                total_bytes_in: 0,
                total_bytes_out: 0,
                buckets: VecDeque::new(),
            });
            port_traffic.total_bytes_out += bytes;
            Self::add_to_bucket(port_traffic, 0, bytes);
        }
        // Mark port as dirty for periodic batch flush
        if let Ok(mut dirty) = self.dirty_ports.lock() {
            dirty.insert(port);
        }
    }

    /// Flush dirty traffic data to the database.
    /// Called periodically by the background flush task and on graceful shutdown.
    pub async fn flush_to_db(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let db = match &self.db {
            Some(db) => db.clone(),
            None => return Ok(()),
        };

        // Atomically take the set of dirty ports
        let dirty_ports: std::collections::HashSet<u16> = {
            let mut dirty = self.dirty_ports.lock().unwrap();
            std::mem::take(&mut *dirty)
        };

        if dirty_ports.is_empty() {
            return Ok(());
        }

        // Snapshot in-memory data for dirty ports (brief lock)
        let snapshots: Vec<(u16, PortTraffic)> = {
            let store = self.inner.lock().await;
            dirty_ports
                .iter()
                .filter_map(|&port| store.get(&port).map(|pt| (port, pt.clone())))
                .collect()
        };

        // Write snapshots to DB without holding the in-memory lock
        for (port, port_traffic) in snapshots {
            if let Err(e) = db
                .replace_port_traffic(
                    port,
                    port_traffic.total_bytes_in,
                    port_traffic.total_bytes_out,
                )
                .await
            {
                tracing::warn!("Failed to flush port_traffic for port {}: {}", port, e);
            }
            for bucket in &port_traffic.buckets {
                if let Err(e) = db
                    .replace_traffic_bucket(
                        port,
                        bucket.timestamp,
                        bucket.bytes_in,
                        bucket.bytes_out,
                    )
                    .await
                {
                    tracing::warn!("Failed to flush traffic_bucket for port {}: {}", port, e);
                }
            }
        }

        Ok(())
    }

    /// Start the background task that periodically flushes traffic data to the database.
    pub fn start_flush_task(&self) {
        let store = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                if let Err(e) = store.flush_to_db().await {
                    tracing::warn!("Failed to flush traffic data to DB: {}", e);
                }
            }
        });
    }

    fn add_to_bucket(
        port_traffic: &mut PortTraffic,
        bytes_in: u64,
        bytes_out: u64,
    ) -> DateTime<Utc> {
        let now = Utc::now();
        // Truncate to minute
        let bucket_time = now - chrono::Duration::seconds(now.second() as i64);

        // Check if we have a bucket for this minute
        if let Some(last) = port_traffic.buckets.back_mut() {
            if last.timestamp == bucket_time {
                last.bytes_in += bytes_in;
                last.bytes_out += bytes_out;
                return bucket_time;
            }
        }

        // Add new bucket
        port_traffic.buckets.push_back(TrafficBucket {
            timestamp: bucket_time,
            bytes_in,
            bytes_out,
        });

        // Keep only last 24 hours (1440 buckets)
        while port_traffic.buckets.len() > 1440 {
            port_traffic.buckets.pop_front();
        }

        bucket_time
    }

    /// Get traffic for all ports
    pub async fn get_all_traffic(&self) -> Vec<PortTraffic> {
        let store = self.inner.lock().await;
        store.values().cloned().collect()
    }

    /// Get traffic for specific port
    pub async fn get_port_traffic(&self, port: u16) -> Option<PortTraffic> {
        let store = self.inner.lock().await;
        store.get(&port).cloned()
    }

    /// Remove traffic data for a port (when client disconnects)
    /// Note: This only removes from in-memory cache, database history is preserved
    pub async fn remove_port(&self, port: u16) {
        let mut store = self.inner.lock().await;
        store.remove(&port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_traffic_store_new() {
        let store = TrafficStore::new();
        let traffic = store.get_all_traffic().await;
        assert!(traffic.is_empty());
    }

    #[tokio::test]
    async fn test_record_bytes_in() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.port, 8080);
        assert_eq!(traffic.total_bytes_in, 100);
        assert_eq!(traffic.total_bytes_out, 0);
        assert_eq!(traffic.buckets.len(), 1);
    }

    #[tokio::test]
    async fn test_record_bytes_out() {
        let store = TrafficStore::new();
        store.record_bytes_out(8080, 200).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.port, 8080);
        assert_eq!(traffic.total_bytes_in, 0);
        assert_eq!(traffic.total_bytes_out, 200);
        assert_eq!(traffic.buckets.len(), 1);
    }

    #[tokio::test]
    async fn test_record_multiple_ports() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;
        store.record_bytes_out(9000, 200).await;

        let all_traffic = store.get_all_traffic().await;
        assert_eq!(all_traffic.len(), 2);

        let traffic_8080 = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic_8080.total_bytes_in, 100);

        let traffic_9000 = store.get_port_traffic(9000).await.unwrap();
        assert_eq!(traffic_9000.total_bytes_out, 200);
    }

    #[tokio::test]
    async fn test_record_accumulates() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;
        store.record_bytes_in(8080, 200).await;
        store.record_bytes_out(8080, 50).await;
        store.record_bytes_out(8080, 75).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.total_bytes_in, 300);
        assert_eq!(traffic.total_bytes_out, 125);
    }

    #[tokio::test]
    async fn test_get_nonexistent_port() {
        let store = TrafficStore::new();
        let traffic = store.get_port_traffic(9999).await;
        assert!(traffic.is_none());
    }

    #[tokio::test]
    async fn test_remove_port() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;
        assert!(store.get_port_traffic(8080).await.is_some());

        store.remove_port(8080).await;
        assert!(store.get_port_traffic(8080).await.is_none());
    }

    #[tokio::test]
    async fn test_remove_nonexistent_port() {
        let store = TrafficStore::new();
        // Should not panic
        store.remove_port(9999).await;
    }

    #[tokio::test]
    async fn test_bucket_creation() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.buckets.len(), 1);

        let bucket = &traffic.buckets[0];
        assert_eq!(bucket.bytes_in, 100);
        assert_eq!(bucket.bytes_out, 0);
    }

    #[tokio::test]
    async fn test_traffic_store_clone() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;

        let cloned = store.clone();
        let traffic = cloned.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.total_bytes_in, 100);
    }

    #[tokio::test]
    async fn test_traffic_store_with_db() {
        let db = Database::new(":memory:").await.unwrap();
        let store = TrafficStore::with_db(db);

        store.record_bytes_in(8080, 100).await;
        store.record_bytes_out(8080, 200).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.total_bytes_in, 100);
        assert_eq!(traffic.total_bytes_out, 200);
    }

    #[tokio::test]
    async fn test_traffic_store_load_from_db() {
        let db = Database::new(":memory:").await.unwrap();

        // Pre-populate database
        db.upsert_port_traffic(8080, 500, 1000).await.unwrap();
        db.upsert_port_traffic(9000, 200, 400).await.unwrap();

        // Create store and load from DB
        let store = TrafficStore::with_db(db);
        store.load_from_db().await.unwrap();

        let traffic_8080 = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic_8080.total_bytes_in, 500);
        assert_eq!(traffic_8080.total_bytes_out, 1000);

        let traffic_9000 = store.get_port_traffic(9000).await.unwrap();
        assert_eq!(traffic_9000.total_bytes_in, 200);
        assert_eq!(traffic_9000.total_bytes_out, 400);
    }

    #[tokio::test]
    async fn test_traffic_store_bucket_time_truncation() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;
        store.record_bytes_in(8080, 50).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        // Both should typically be in the same minute bucket
        // Total should always be correct regardless
        assert_eq!(traffic.total_bytes_in, 150);
        // If both landed in the same bucket, there should be 1 bucket with 150 bytes
        // If they landed in different buckets (second boundary), there could be 2
        let total_bucket_bytes: u64 = traffic.buckets.iter().map(|b| b.bytes_in).sum();
        assert_eq!(total_bucket_bytes, 150);
    }

    #[tokio::test]
    async fn test_traffic_store_zero_bytes() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 0).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.total_bytes_in, 0);
        assert_eq!(traffic.buckets.len(), 1);
    }

    #[tokio::test]
    async fn test_health_response() {
        let response = HealthResponse { status: "ok" };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("ok"));
    }

    #[test]
    fn test_login_request_deserialize() {
        let json = r#"{"password":"secret"}"#;
        let req: LoginRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.password, "secret");
    }

    #[test]
    fn test_server_metrics_serialize() {
        let metrics = ServerMetrics {
            client_count: 5,
            active_connection_count: 10,
            total_bytes_in: 1000,
            total_bytes_out: 2000,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        assert!(json.contains("client_count"));
        assert!(json.contains("1000"));
    }

    #[test]
    fn test_client_response_serialize() {
        let response = ClientResponse {
            port: 8080,
            hostname: Some("test-host".into()),
            connection_count: 3,
            quality: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("8080"));
        assert!(json.contains("test-host"));
    }

    #[test]
    fn test_shadowsocks_config_serialize() {
        let config = ShadowsocksConfig {
            enabled: true,
            port: Some(8388),
            cipher: Some("aes-256-gcm".into()),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("8388"));
        assert!(json.contains("aes-256-gcm"));
    }

    #[test]
    fn test_shadowsocks_stats_serialize() {
        let stats = ShadowsocksStats {
            enabled: true,
            port: Some(8388),
            total_bytes_in: 1000,
            total_bytes_out: 2000,
            active_connections: 5,
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("active_connections"));
    }
}

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

/// API state shared across all handlers
#[derive(Clone)]
pub struct ApiState {
    pub server_state: ServerState,
    pub auth_config: Arc<AuthConfig>,
    pub log_store: Option<crate::server::logs::LogStore>,
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

/// Client response for API
#[derive(Debug, Serialize)]
pub struct ClientResponse {
    pub port: u16,
    pub hostname: Option<String>,
    pub connection_count: usize,
    pub quality: Option<ConnectionQuality>,
}

/// Server metrics
#[derive(Debug, Serialize)]
pub struct ServerMetrics {
    pub client_count: usize,
    pub active_connection_count: usize,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
}

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

/// Client with quality data
#[derive(Debug, Serialize)]
pub struct ClientWithQuality {
    pub port: u16,
    pub hostname: Option<String>,
    pub quality: ConnectionQuality,
}

/// Port quality response with history
#[derive(Debug, Serialize)]
pub struct PortQualityResponse {
    pub current: ConnectionQuality,
    pub history: Vec<QualitySample>,
}

/// Quality warning
#[derive(Debug, Serialize)]
pub struct QualityWarning {
    pub port: u16,
    pub hostname: Option<String>,
    pub quality: ConnectionQuality,
    pub warning_type: String,
}

/// Query parameters for history
#[derive(Debug, Deserialize)]
pub struct QualityHistoryQuery {
    pub start: Option<String>,
    pub end: Option<String>,
}

/// Shadowsocks configuration
#[derive(Debug, Serialize)]
pub struct ShadowsocksConfig {
    pub enabled: bool,
    pub port: Option<u16>,
    pub cipher: Option<String>,
}

/// Shadowsocks statistics
#[derive(Debug, Serialize)]
pub struct ShadowsocksStats {
    pub enabled: bool,
    pub port: Option<u16>,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub active_connections: usize,
}

/// Shadowsocks quality response
#[derive(Debug, Serialize)]
pub struct ShadowsocksQuality {
    pub port: u16,
    pub quality: ConnectionQuality,
    pub history: Vec<QualitySample>,
}

/// Trojan configuration
#[derive(Debug, Serialize)]
pub struct TrojanConfig {
    pub enabled: bool,
    pub port: Option<u16>,
    pub fallback: Option<String>,
}

/// Trojan statistics
#[derive(Debug, Serialize)]
pub struct TrojanStats {
    pub enabled: bool,
    pub port: Option<u16>,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub active_connections: usize,
}

/// Trojan quality response
#[derive(Debug, Serialize)]
pub struct TrojanQuality {
    pub port: u16,
    pub quality: ConnectionQuality,
    pub history: Vec<QualitySample>,
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

/// Request body for PUT /api/acme/config
#[derive(Debug, Deserialize)]
struct UpdateAcmeConfigRequest {
    enabled: Option<bool>,
    server_url: Option<String>,
    email: Option<String>,
    auto_renew: Option<bool>,
    renewal_check_interval: Option<u64>,
    renewal_days_before_expiry: Option<u64>,
    tos_agreed: Option<bool>,
}

// Login handler
async fn login(
    State(state): State<ApiState>,
    Json(request): Json<LoginRequest>,
) -> impl IntoResponse {
    if !state.auth_config.is_enabled() {
        let token = create_token(&state.auth_config.jwt_secret)
            .unwrap_or_else(|_| "dummy-token".to_string());
        return Json(LoginResponse {
            token,
            auth_required: false,
        })
        .into_response();
    }

    if state.auth_config.verify_password(&request.password) {
        match create_token(&state.auth_config.jwt_secret) {
            Ok(token) => Json(LoginResponse {
                token,
                auth_required: true,
            })
            .into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create token").into_response(),
        }
    } else {
        (StatusCode::UNAUTHORIZED, "Invalid password").into_response()
    }
}

// Logout handler (client just discards token)
async fn logout() -> impl IntoResponse {
    StatusCode::OK
}

// List all clients
async fn list_clients(State(state): State<ApiState>) -> Json<Vec<ClientResponse>> {
    let clients = state.server_state.get_all_clients().await;
    let mut response = Vec::with_capacity(clients.len() + 1);

    // Tunnel clients
    for (port, info) in clients {
        let connection_count = state.server_state.get_connection_count_for_port(port).await;
        let quality = state.server_state.quality_store.get_quality(port).await;
        response.push(ClientResponse {
            port,
            hostname: info.hostname,
            connection_count,
            quality,
        });
    }

    // Shadowsocks ports — show them in the client list so SS activity is visible
    let ss_ports = state.server_state.get_shadowsocks_ports().await;
    for port in ss_ports {
        let connection_count = state.server_state.get_connection_count_for_port(port).await;
        let quality = state.server_state.quality_store.get_quality(port).await;
        response.push(ClientResponse {
            port,
            hostname: Some("[Shadowsocks]".to_string()),
            connection_count,
            quality,
        });
    }

    // Trojan ports — show them in the client list
    let trojan_ports = state.server_state.get_trojan_ports().await;
    for port in trojan_ports {
        let connection_count = state.server_state.get_connection_count_for_port(port).await;
        let quality = state.server_state.quality_store.get_quality(port).await;
        response.push(ClientResponse {
            port,
            hostname: Some("[Trojan]".to_string()),
            connection_count,
            quality,
        });
    }

    Json(response)
}

// Disconnect client
async fn disconnect_client(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
) -> impl IntoResponse {
    let success = state.server_state.disconnect_client(port).await;
    if success {
        state.server_state.traffic_store.remove_port(port).await;
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

// Get traffic for all clients
async fn get_traffic(State(state): State<ApiState>) -> Json<Vec<PortTraffic>> {
    Json(state.server_state.traffic_store.get_all_traffic().await)
}

// Get traffic for specific port
async fn get_port_traffic(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
) -> impl IntoResponse {
    match state
        .server_state
        .traffic_store
        .get_port_traffic(port)
        .await
    {
        Some(traffic) => Json(traffic).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// Health check
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

// Get server metrics
async fn get_metrics(State(state): State<ApiState>) -> Json<ServerMetrics> {
    let client_count = state.server_state.get_client_count().await;
    let active_connection_count = state.server_state.get_active_connection_count().await;

    let traffic = state.server_state.traffic_store.get_all_traffic().await;
    let total_bytes_in = traffic.iter().map(|t| t.total_bytes_in).sum();
    let total_bytes_out = traffic.iter().map(|t| t.total_bytes_out).sum();

    Json(ServerMetrics {
        client_count,
        active_connection_count,
        total_bytes_in,
        total_bytes_out,
    })
}

/// Serve embedded static files for frontend
#[cfg(feature = "embed-frontend")]
async fn serve_static(Path(path): Path<String>) -> impl IntoResponse {
    let path = if path.is_empty() { "index.html" } else { &path };

    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            axum::http::Response::builder()
                .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data))
                .unwrap()
        }
        None => {
            // Fallback to index.html for SPA routing
            if let Some(index) = FrontendAssets::get("index.html") {
                axum::http::Response::builder()
                    .header(axum::http::header::CONTENT_TYPE, "text/html")
                    .body(Body::from(index.data))
                    .unwrap()
            } else {
                axum::http::Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("Not found"))
                    .unwrap()
            }
        }
    }
}

// Get all clients with quality data
async fn get_all_quality(State(state): State<ApiState>) -> Json<Vec<ClientWithQuality>> {
    let clients = state.server_state.get_all_clients().await;
    let mut result = Vec::with_capacity(clients.len());

    for (port, info) in clients {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            result.push(ClientWithQuality {
                port,
                hostname: info.hostname,
                quality,
            });
        }
    }

    Json(result)
}

// Get quality data for a single port
async fn get_port_quality(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
) -> impl IntoResponse {
    let current = state.server_state.quality_store.get_quality(port).await;
    let history = state.server_state.quality_store.get_samples(port).await;

    match current {
        Some(current) => Json(PortQualityResponse { current, history }).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// Get quality history for a port (with optional time range)
async fn get_quality_history(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
    Query(_params): Query<QualityHistoryQuery>,
) -> Json<Vec<QualitySample>> {
    // For now, just return in-memory samples (last 60 minutes)
    // Future: support database queries for longer time ranges
    let samples = state.server_state.quality_store.get_samples(port).await;
    Json(samples)
}

// Get current quality warnings
async fn get_quality_warnings(State(state): State<ApiState>) -> Json<Vec<QualityWarning>> {
    let clients = state.server_state.get_all_clients().await;
    let mut warnings = Vec::new();

    for (port, info) in clients {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            let warning_type = if quality.is_critical {
                Some("critical".to_string())
            } else if quality.is_warning {
                Some("warning".to_string())
            } else {
                None
            };

            if let Some(warning_type) = warning_type {
                warnings.push(QualityWarning {
                    port,
                    hostname: info.hostname,
                    quality,
                    warning_type,
                });
            }
        }
    }

    Json(warnings)
}

// Get Shadowsocks configuration
async fn get_shadowsocks_config(State(state): State<ApiState>) -> Json<ShadowsocksConfig> {
    // Get all SS ports
    let ss_ports = state.server_state.get_shadowsocks_ports().await;

    let (port, cipher) = if !ss_ports.is_empty() {
        // For now, return the first SS port info with default cipher
        // In future multi-port support, this would return all
        (Some(ss_ports[0]), Some("aes-256-gcm".to_string()))
    } else {
        (None, None)
    };

    Json(ShadowsocksConfig {
        enabled: !ss_ports.is_empty(),
        port,
        cipher,
    })
}

// Get Shadowsocks traffic statistics
async fn get_shadowsocks_stats(State(state): State<ApiState>) -> Json<ShadowsocksStats> {
    let ss_ports = state.server_state.get_shadowsocks_ports().await;

    let mut total_bytes_in = 0;
    let mut total_bytes_out = 0;
    let mut active_connections = 0;

    for &port in &ss_ports {
        if let Some(traffic) = state
            .server_state
            .traffic_store
            .get_port_traffic(port)
            .await
        {
            total_bytes_in += traffic.total_bytes_in;
            total_bytes_out += traffic.total_bytes_out;
        }
        active_connections += state.server_state.get_connection_count_for_port(port).await;
    }

    Json(ShadowsocksStats {
        enabled: !ss_ports.is_empty(),
        port: ss_ports.first().copied(),
        total_bytes_in,
        total_bytes_out,
        active_connections,
    })
}

// Get Shadowsocks quality data
async fn get_shadowsocks_quality(State(state): State<ApiState>) -> Json<Vec<ShadowsocksQuality>> {
    let ss_ports = state.server_state.get_shadowsocks_ports().await;
    let mut result = Vec::with_capacity(ss_ports.len());

    for port in ss_ports {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            let history = state.server_state.quality_store.get_samples(port).await;
            result.push(ShadowsocksQuality {
                port,
                quality,
                history,
            });
        }
    }

    Json(result)
}

// Update Shadowsocks configuration (placeholder for dynamic config)
async fn update_shadowsocks_config(State(_state): State<ApiState>) -> impl IntoResponse {
    // For now, return not implemented since we don't support dynamic reconfiguration yet
    // In future: support enabling/disabling SS, changing port/cipher/password
    (
        StatusCode::NOT_IMPLEMENTED,
        "Dynamic configuration not implemented yet",
    )
}

// Get Trojan configuration
async fn get_trojan_config(State(state): State<ApiState>) -> Json<TrojanConfig> {
    let trojan_ports = state.server_state.get_trojan_ports().await;

    let (port, fallback) = if !trojan_ports.is_empty() {
        // Get fallback from port info
        let port_info = state.server_state.get_port(trojan_ports[0]).await;
        let fallback = port_info.and_then(|info| {
            if let crate::server::control::PortInfo::Trojan { fallback, .. } = info {
                Some(fallback)
            } else {
                None
            }
        });
        (Some(trojan_ports[0]), fallback)
    } else {
        (None, None)
    };

    Json(TrojanConfig {
        enabled: !trojan_ports.is_empty(),
        port,
        fallback,
    })
}

// Get Trojan traffic statistics
async fn get_trojan_stats(State(state): State<ApiState>) -> Json<TrojanStats> {
    let trojan_ports = state.server_state.get_trojan_ports().await;

    let mut total_bytes_in = 0;
    let mut total_bytes_out = 0;
    let mut active_connections = 0;

    for &port in &trojan_ports {
        if let Some(traffic) = state
            .server_state
            .traffic_store
            .get_port_traffic(port)
            .await
        {
            total_bytes_in += traffic.total_bytes_in;
            total_bytes_out += traffic.total_bytes_out;
        }
        active_connections += state.server_state.get_connection_count_for_port(port).await;
    }

    Json(TrojanStats {
        enabled: !trojan_ports.is_empty(),
        port: trojan_ports.first().copied(),
        total_bytes_in,
        total_bytes_out,
        active_connections,
    })
}

// Get Trojan quality data
async fn get_trojan_quality(State(state): State<ApiState>) -> Json<Vec<TrojanQuality>> {
    let trojan_ports = state.server_state.get_trojan_ports().await;
    let mut result = Vec::with_capacity(trojan_ports.len());

    for port in trojan_ports {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            let history = state.server_state.quality_store.get_samples(port).await;
            result.push(TrojanQuality {
                port,
                quality,
                history,
            });
        }
    }

    Json(result)
}

// Update Trojan configuration (placeholder for dynamic config)
async fn update_trojan_config(State(_state): State<ApiState>) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "Dynamic configuration not implemented yet",
    )
}

// ── Mesh Network Endpoints ─────────────────────────────────────────

// GET /api/mesh — list all meshes
async fn list_meshes(State(state): State<ApiState>) -> impl IntoResponse {
    let networks = state.server_state.mesh_manager.list_networks().await;
    let response: Vec<MeshNetworkResponse> = networks
        .into_iter()
        .map(|(id, members)| {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();

            MeshNetworkResponse {
                id,
                members: members
                    .iter()
                    .map(|m| MeshMemberResponse {
                        client_name: m.client_name.clone(),
                        public_addr: m.public_addr.clone(),
                        p2p_available: m.p2p_available,
                        online: true,
                    })
                    .collect(),
                services,
            }
        })
        .collect();
    Json(response)
}

// GET /api/mesh/:id — mesh detail
async fn get_mesh(State(state): State<ApiState>, Path(mesh_id): Path<String>) -> impl IntoResponse {
    match state.server_state.mesh_manager.get_mesh(&mesh_id).await {
        Some(members) => {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();

            Json(MeshNetworkResponse {
                id: mesh_id,
                members: members
                    .iter()
                    .map(|m| MeshMemberResponse {
                        client_name: m.client_name.clone(),
                        public_addr: m.public_addr.clone(),
                        p2p_available: m.p2p_available,
                        online: true,
                    })
                    .collect(),
                services,
            })
            .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// GET /api/mesh/:id/services — mesh services
async fn get_mesh_services(
    State(state): State<ApiState>,
    Path(mesh_id): Path<String>,
) -> impl IntoResponse {
    match state.server_state.mesh_manager.get_mesh(&mesh_id).await {
        Some(members) => {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();
            Json(services).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── DNS Management Endpoints ───────────────────────────────────────

// GET /api/dns/records — list all DNS records
async fn get_dns_records(State(state): State<ApiState>) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    let records = dns_registry.list_records().await;
    let response: Vec<DnsRecordResponse> = records
        .iter()
        .map(|r| DnsRecordResponse {
            name: r.name().to_string(),
            record_type: r.record_type().to_string(),
            value: match r {
                DnsRecord::TunnelA {
                    target_ip, port, ..
                } => format!("{} (port {})", target_ip, port),
                DnsRecord::MeshA { target_ip, .. } => target_ip.clone(),
                DnsRecord::TunnelSrv { target, port, .. } => format!("{}:{}", target, port),
                DnsRecord::MeshSrv { target, port, .. } => format!("{}:{}", target, port),
                DnsRecord::Txt { text, .. } => text.clone(),
            },
        })
        .collect();

    Json(response).into_response()
}

// POST /api/dns/records — add manual DNS record
async fn add_dns_record(
    State(state): State<ApiState>,
    Json(body): Json<AddDnsRecordRequest>,
) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    let record = match body.record_type.as_str() {
        "A" => DnsRecord::TunnelA {
            name: body.name.clone(),
            target_ip: body.value.clone(),
            port: body.port.unwrap_or(80),
        },
        _ => return (StatusCode::BAD_REQUEST, "Unsupported record type").into_response(),
    };

    dns_registry.add_manual_record(record).await;
    StatusCode::CREATED.into_response()
}

// DELETE /api/dns/records/:name — delete DNS record
async fn delete_dns_record(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    dns_registry.remove_record(&name).await;
    StatusCode::OK.into_response()
}

// ── Log Viewer Endpoints ──────────────────────────────────────────

async fn sse_log_stream(
    State(state): State<ApiState>,
    Query(params): Query<SseQuery>,
) -> impl IntoResponse {
    // Check auth for SSE
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");

        let is_valid = if !token.is_empty() {
            crate::server::auth::validate_token(token, &state.auth_config.jwt_secret).is_ok()
        } else {
            false
        };

        if !is_valid {
            return axum::response::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from("Unauthorized"))
                .unwrap();
        }
    }

    let log_store = match &state.log_store {
        Some(store) => store.clone(),
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let min_level = params.level.as_deref().unwrap_or("info");
    let min_level_u8 = match min_level {
        "error" => 4u8,
        "warn" => 3,
        "info" => 2,
        "debug" => 1,
        "trace" => 0,
        _ => 2,
    };
    let source_filter = params.source.clone();

    let mut rx = log_store.tx.subscribe();
    let stream = async_stream::stream! {
        loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Ok(entry)) => {
                    // Apply filters
                    let entry_level = match entry.level.as_str() {
                        "TRACE" => 0, "DEBUG" => 1, "INFO" => 2, "WARN" => 3, "ERROR" => 4,
                        _ => 2,
                    };
                    if entry_level < min_level_u8 {
                        continue;
                    }
                    if let Some(ref src) = source_filter {
                        if !entry.source.starts_with(src) {
                            continue;
                        }
                    }

                    let json = serde_json::to_string(&LogEntryResponse {
                        id: entry.id,
                        timestamp: entry.timestamp,
                        level: entry.level.clone(),
                        source: entry.source.clone(),
                        target: entry.target.clone(),
                        message: entry.message.clone(),
                    })
                    .unwrap_or_default();

                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("log").data(json),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default()
                            .event("sync")
                            .data(format!(r#"{{"lagged":{}}}"#, n)),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    break;
                }
                Err(_) => {
                    // Timeout — send ping to keep connection alive
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("ping").data(""),
                    );
                }
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response()
}

async fn get_logs(
    State(state): State<ApiState>,
    Query(params): Query<LogsQuery>,
) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let limit = params.limit.unwrap_or(200).min(1000) as usize;

    // When before_id is specified, query DB directly for correct pagination
    // (in-memory entries have id=0, so DB pagination is the only correct path)
    if params.before_id.is_some() {
        let db_entries = log_store
            .query_db(
                params.level.as_deref(),
                params.source.as_deref(),
                params.search.as_deref(),
                limit as u32,
                params.before_id,
            )
            .await;

        let response: Vec<LogEntryResponse> = db_entries
            .into_iter()
            .map(|e| LogEntryResponse {
                id: e.id,
                timestamp: e.timestamp,
                level: e.level,
                source: e.source,
                target: e.target,
                message: e.message,
            })
            .collect();

        return Json(response).into_response();
    }

    // Query in-memory buffer first (fast path, no DB round-trip)
    let mem_entries = log_store
        .query(
            params.level.as_deref(),
            params.source.as_deref(),
            params.search.as_deref(),
            limit,
        )
        .await;

    // If in-memory buffer doesn't have enough entries, supplement from DB
    if mem_entries.len() < limit {
        let db_limit = (limit - mem_entries.len()) as u32;
        let db_entries = log_store
            .query_db(
                params.level.as_deref(),
                params.source.as_deref(),
                params.search.as_deref(),
                db_limit,
                None,
            )
            .await;

        // Merge: DB entries (older) first, then in-memory (newer)
        // Deduplicate by id for entries that were flushed to DB
        let mem_ids: std::collections::HashSet<i64> = mem_entries
            .iter()
            .filter_map(|e| if e.id > 0 { Some(e.id) } else { None })
            .collect();

        let mut all_entries: Vec<LogEntryResponse> = db_entries
            .into_iter()
            .filter(|e| !mem_ids.contains(&e.id))
            .map(|e| LogEntryResponse {
                id: e.id,
                timestamp: e.timestamp,
                level: e.level,
                source: e.source,
                target: e.target,
                message: e.message,
            })
            .collect();

        all_entries.extend(mem_entries.into_iter().map(|e| LogEntryResponse {
            id: e.id,
            timestamp: e.timestamp,
            level: e.level,
            source: e.source,
            target: e.target,
            message: e.message,
        }));

        return Json(all_entries).into_response();
    }

    let response: Vec<LogEntryResponse> = mem_entries
        .into_iter()
        .map(|e| LogEntryResponse {
            id: e.id,
            timestamp: e.timestamp,
            level: e.level,
            source: e.source,
            target: e.target,
            message: e.message,
        })
        .collect();

    Json(response).into_response()
}

async fn get_logs_level(State(state): State<ApiState>) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let level_u8 = log_store.level.load(std::sync::atomic::Ordering::Relaxed);
    let level_str = match level_u8 {
        0 => "trace",
        1 => "debug",
        2 => "info",
        3 => "warn",
        4 => "error",
        _ => "info",
    };

    Json(serde_json::json!({ "level": level_str })).into_response()
}

async fn put_logs_level(
    State(state): State<ApiState>,
    Json(body): Json<SetLevelRequest>,
) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let level_u8 = match body.level.to_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" => 3,
        "error" => 4,
        _ => {
            return axum::response::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(
                    "Invalid level. Use: trace, debug, info, warn, error",
                ))
                .unwrap();
        }
    };

    log_store
        .level
        .store(level_u8, std::sync::atomic::Ordering::Relaxed);
    tracing::info!("Log level changed to {}", body.level.to_lowercase());

    Json(serde_json::json!({ "level": body.level.to_lowercase() })).into_response()
}

// ── Proxy Rules Endpoints ─────────────────────────────────────────

/// Request body for creating a proxy rule
#[derive(Debug, Deserialize)]
pub struct CreateProxyRuleRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub rule_type: String,
    pub listen: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub routes: Vec<crate::server::reverse_proxy::Route>,
    pub tls: Option<crate::server::reverse_proxy::ProxyTlsConfig>,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
}

/// Request body for updating a proxy rule
#[derive(Debug, Deserialize)]
pub struct UpdateProxyRuleRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub rule_type: String,
    pub listen: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub routes: Vec<crate::server::reverse_proxy::Route>,
    pub tls: Option<crate::server::reverse_proxy::ProxyTlsConfig>,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
}

fn default_rule_enabled() -> bool {
    true
}

// GET /api/proxy/rules — list all proxy rules
async fn list_proxy_rules(State(state): State<ApiState>) -> impl IntoResponse {
    let rules = state.server_state.proxy_state.rules.lock().await;
    let rules_vec: Vec<&ProxyRule> = rules.values().collect();
    Json(serde_json::json!({ "rules": rules_vec }))
}

// POST /api/proxy/rules — create a new proxy rule
async fn create_proxy_rule(
    State(state): State<ApiState>,
    Json(body): Json<CreateProxyRuleRequest>,
) -> impl IntoResponse {
    // Parse rule type
    let rule_type = match body.rule_type.to_lowercase().as_str() {
        "http" => crate::server::reverse_proxy::RuleType::Http,
        "tcp" => crate::server::reverse_proxy::RuleType::Tcp,
        "udp" => crate::server::reverse_proxy::RuleType::Udp,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid rule type. Use: http, tcp, udp" })),
            )
                .into_response();
        }
    };

    // Generate a unique ID
    let id = uuid::Uuid::new_v4().to_string();

    let rule = ProxyRule {
        id: id.clone(),
        name: body.name,
        rule_type,
        listen: body.listen,
        domains: body.domains,
        routes: body.routes,
        tls: body.tls,
        enabled: body.enabled,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
    };

    // Save to database
    if let Err(e) = state.server_state.proxy_state.save_rule(&rule).await {
        tracing::error!("Failed to save proxy rule: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save proxy rule" })),
        )
            .into_response();
    }

    // Add to in-memory state
    let mut rules = state.server_state.proxy_state.rules.lock().await;
    rules.insert(id.clone(), rule.clone());

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "rule": rule })),
    )
        .into_response()
}

// PUT /api/proxy/rules/:id — update a proxy rule
async fn update_proxy_rule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateProxyRuleRequest>,
) -> impl IntoResponse {
    // Parse rule type
    let rule_type = match body.rule_type.to_lowercase().as_str() {
        "http" => crate::server::reverse_proxy::RuleType::Http,
        "tcp" => crate::server::reverse_proxy::RuleType::Tcp,
        "udp" => crate::server::reverse_proxy::RuleType::Udp,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid rule type. Use: http, tcp, udp" })),
            )
                .into_response();
        }
    };

    // Check if rule exists
    let existing = {
        let rules = state.server_state.proxy_state.rules.lock().await;
        rules.get(&id).cloned()
    };

    let existing = match existing {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Rule not found" })),
            )
                .into_response();
        }
    };

    let rule = ProxyRule {
        id: id.clone(),
        name: body.name,
        rule_type,
        listen: body.listen,
        domains: body.domains,
        routes: body.routes,
        tls: body.tls,
        enabled: body.enabled,
        created_at: existing.created_at,
    };

    // Save to database
    if let Err(e) = state.server_state.proxy_state.save_rule(&rule).await {
        tracing::error!("Failed to save proxy rule: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save proxy rule" })),
        )
            .into_response();
    }

    // Update in-memory state
    let mut rules = state.server_state.proxy_state.rules.lock().await;
    rules.insert(id, rule.clone());

    Json(serde_json::json!({ "rule": rule })).into_response()
}

// DELETE /api/proxy/rules/:id — delete a proxy rule
async fn delete_proxy_rule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Check if rule exists
    let exists = {
        let rules = state.server_state.proxy_state.rules.lock().await;
        rules.contains_key(&id)
    };

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Rule not found" })),
        )
            .into_response();
    }

    // Stop the listener if it's running
    let listener_handle = {
        let mut listeners = state.server_state.proxy_state.listeners.lock().await;
        listeners.remove(&id)
    };
    if let Some(handle) = listener_handle {
        handle.abort();
    }

    // Delete from database
    if let Err(e) = state.server_state.proxy_state.delete_rule(&id).await {
        tracing::error!("Failed to delete proxy rule from database: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to delete proxy rule" })),
        )
            .into_response();
    }

    // Remove from in-memory state
    {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.remove(&id);
    }

    StatusCode::OK.into_response()
}

// GET /api/proxy/stats — get proxy statistics
async fn get_proxy_stats(State(state): State<ApiState>) -> impl IntoResponse {
    // Try to get stats from database if available
    if let Some(db) = state.server_state.get_db() {
        match db.get_proxy_stats().await {
            Ok((total_rules, active_rules, total_connections, bytes_in, bytes_out)) => {
                return Json(ProxyStats {
                    total_rules,
                    active_rules,
                    total_connections,
                    bytes_in,
                    bytes_out,
                })
                .into_response();
            }
            Err(e) => {
                tracing::warn!("Failed to get proxy stats from database: {}", e);
            }
        }
    }

    // Fallback to in-memory stats
    let rules = state.server_state.proxy_state.rules.lock().await;
    let total_rules = rules.len() as i64;
    let active_rules = rules.values().filter(|r| r.enabled).count() as i64;

    let connection_counts = state.server_state.proxy_state.connection_counts.lock().await;
    let active_connections: u64 = connection_counts.values().sum();

    Json(ProxyStats {
        total_rules,
        active_rules,
        total_connections: active_connections as i64,
        bytes_in: 0,
        bytes_out: 0,
    })
    .into_response()
}

// ── ACME Certificate Management Endpoints ──────────────────────────

// GET /api/acme/certificates — list all certificates
async fn list_acme_certificates(State(state): State<ApiState>) -> impl IntoResponse {
    let client = match &state.server_state.acme_client {
        Some(c) => c,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };

    match client.list_certificates().await {
        Ok(certs) => Json(serde_json::json!({ "certificates": certs })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// POST /api/acme/certificates/:domain — request a new certificate
async fn request_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client = match &state.server_state.acme_client {
        Some(c) => c,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };

    match client.request_certificate(&domain).await {
        Ok(metadata) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "certificate": metadata })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// GET /api/acme/certificates/:domain — get certificate details
async fn get_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client = match &state.server_state.acme_client {
        Some(c) => c,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };

    match client.get_certificate_metadata(&domain).await {
        Ok(Some(metadata)) => Json(serde_json::json!({ "certificate": metadata })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Certificate not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// POST /api/acme/certificates/:domain/renew — manual renewal
async fn renew_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client = match &state.server_state.acme_client {
        Some(c) => c,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };

    match client.renew_certificate(&domain).await {
        Ok(metadata) => Json(serde_json::json!({ "certificate": metadata })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// GET /api/acme/status — get ACME status
async fn get_acme_status(State(state): State<ApiState>) -> impl IntoResponse {
    let config = match &state.server_state.acme_config {
        Some(c) => c,
        None => {
            return Json(serde_json::json!({
                "enabled": false,
                "server_url": null,
                "cert_dir": null,
                "consumers": {
                    "api_tls": false,
                    "trojan": false,
                    "control_tls": true,
                    "reverse_proxy": false,
                },
            }))
            .into_response();
        }
    };

    let cert_count = match &state.server_state.acme_client {
        Some(client) => match client.list_certificates().await {
            Ok(certs) => certs.len(),
            Err(_) => 0,
        },
        None => 0,
    };

    let api_tls = state.server_state.cert_manager.is_some();
    let trojan = !state.server_state.get_trojan_ports().await.is_empty();

    Json(serde_json::json!({
        "enabled": config.enabled,
        "server_url": config.server_url,
        "cert_dir": config.cert_dir,
        "certificate_count": cert_count,
        "consumers": {
            "api_tls": api_tls,
            "trojan": trojan,
            "control_tls": true,
            "reverse_proxy": false,
        },
    }))
    .into_response()
}

// GET /api/acme/config — get ACME configuration
async fn get_acme_config(State(state): State<ApiState>) -> impl IntoResponse {
    let config = state.server_state.acme_full_config.read().await;
    Json(serde_json::json!({
        "enabled": config.enabled,
        "server_url": config.server_url,
        "email": config.email,
        "cert_dir": config.cert_dir,
        "auto_renew": config.auto_renew,
        "renewal_check_interval": config.renewal_check_interval,
        "renewal_days_before_expiry": config.renewal_days_before_expiry,
        "tos_agreed": config.tos_agreed,
    }))
}

// PUT /api/acme/config — update ACME configuration
async fn update_acme_config(
    State(state): State<ApiState>,
    Json(req): Json<UpdateAcmeConfigRequest>,
) -> impl IntoResponse {
    let mut config = state.server_state.acme_full_config.write().await;
    if let Some(v) = req.enabled {
        config.enabled = v;
    }
    if let Some(v) = req.server_url {
        config.server_url = v;
    }
    if let Some(v) = req.email {
        config.email = Some(v);
    }
    if let Some(v) = req.auto_renew {
        config.auto_renew = v;
    }
    if let Some(v) = req.renewal_check_interval {
        config.renewal_check_interval = v;
    }
    if let Some(v) = req.renewal_days_before_expiry {
        config.renewal_days_before_expiry = v;
    }
    if let Some(v) = req.tos_agreed {
        config.tos_agreed = v;
    }

    Json(serde_json::json!({
        "enabled": config.enabled,
        "server_url": config.server_url,
        "email": config.email,
        "cert_dir": config.cert_dir,
        "auto_renew": config.auto_renew,
        "renewal_check_interval": config.renewal_check_interval,
        "renewal_days_before_expiry": config.renewal_days_before_expiry,
        "tos_agreed": config.tos_agreed,
    }))
}

// ── DNS Provider Endpoints ─────────────────────────────────────────

// GET /api/acme/dns-providers — get available providers and current config
async fn get_dns_providers(State(state): State<ApiState>) -> impl IntoResponse {
    let config = state.server_state.dns_provider_config.read().await;
    Json(serde_json::json!({
        "providers": ["cloudflare", "aliyun", "tencent", "custom"],
        "config": *config
    }))
}

// PUT /api/acme/dns-providers — update DNS provider configuration
async fn update_dns_provider(
    State(state): State<ApiState>,
    Json(req): Json<crate::server::acme::dns::DnsProviderConfig>,
) -> impl IntoResponse {
    let mut config = state.server_state.dns_provider_config.write().await;
    *config = Some(req.clone());
    Json(serde_json::json!({
        "success": true,
        "config": req
    }))
}

// GET /api/acme/challenge-status/:domain — get ACME challenge status for a domain
async fn get_challenge_status(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let _ = state;
    Json(serde_json::json!({
        "domain": domain,
        "status": "pending"
    }))
}

// DELETE /api/acme/certificates/:domain — delete a certificate
async fn delete_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client = match &state.server_state.acme_client {
        Some(c) => c,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };

    match client.delete_certificate(&domain).await {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Create and run the API server
pub async fn run_api_server(
    api_addr: String,
    server_state: ServerState,
    auth_config: AuthConfig,
    tls_config: Option<Arc<rustls::server::ServerConfig>>,
) -> Result<(), std::io::Error> {
    let auth_config = Arc::new(auth_config);

    let log_store = server_state.log_store.clone();

    let state = ApiState {
        server_state,
        auth_config: auth_config.clone(),
        log_store,
    };

    // CORS layer
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Public routes (no auth required) — SSE uses ?token= query param for auth
    let public_routes = Router::new()
        .route("/api/login", post(login))
        .route("/api/health", get(health))
        .route("/api/logs/stream", get(sse_log_stream));

    // Protected routes (require auth only when password is set)
    let mut protected_routes = Router::new()
        .route("/api/logout", post(logout))
        .route("/api/clients", get(list_clients))
        .route("/api/clients/:port", delete(disconnect_client))
        .route("/api/traffic", get(get_traffic))
        .route("/api/traffic/:port", get(get_port_traffic))
        .route("/api/metrics", get(get_metrics))
        // Quality monitoring endpoints
        .route("/api/quality/all", get(get_all_quality))
        .route("/api/quality/:port", get(get_port_quality))
        .route("/api/quality/:port/history", get(get_quality_history))
        .route("/api/quality/warnings", get(get_quality_warnings))
        // Shadowsocks management endpoints
        .route(
            "/api/shadowsocks",
            get(get_shadowsocks_config).post(update_shadowsocks_config),
        )
        .route("/api/shadowsocks/stats", get(get_shadowsocks_stats))
        .route("/api/shadowsocks/quality", get(get_shadowsocks_quality))
        // Trojan management endpoints
        .route(
            "/api/trojan",
            get(get_trojan_config).post(update_trojan_config),
        )
        .route("/api/trojan/stats", get(get_trojan_stats))
        .route("/api/trojan/quality", get(get_trojan_quality))
        // Mesh network endpoints
        .route("/api/mesh", get(list_meshes))
        .route("/api/mesh/:id", get(get_mesh))
        .route("/api/mesh/:id/services", get(get_mesh_services))
        // DNS management endpoints
        .route(
            "/api/dns/records",
            get(get_dns_records).post(add_dns_record),
        )
        .route("/api/dns/records/:name", delete(delete_dns_record))
        // Log viewer endpoints (SSE stream is in public_routes — uses ?token= query param)
        .route("/api/logs", get(get_logs))
        .route("/api/logs/level", get(get_logs_level).put(put_logs_level))
        // Proxy rules management endpoints
        .route(
            "/api/proxy/rules",
            get(list_proxy_rules).post(create_proxy_rule),
        )
        .route(
            "/api/proxy/rules/:id",
            put(update_proxy_rule).delete(delete_proxy_rule),
        )
        .route("/api/proxy/stats", get(get_proxy_stats))
        // ACME certificate management endpoints
        .route("/api/acme/status", get(get_acme_status))
        .route(
            "/api/acme/config",
            get(get_acme_config).put(update_acme_config),
        )
        .route(
            "/api/acme/certificates",
            get(list_acme_certificates),
        )
        .route(
            "/api/acme/certificates/:domain",
            get(get_acme_certificate)
                .post(request_acme_certificate)
                .delete(delete_acme_certificate),
        )
        .route(
            "/api/acme/certificates/:domain/renew",
            post(renew_acme_certificate),
        )
        // DNS provider endpoints
        .route(
            "/api/acme/dns-providers",
            get(get_dns_providers).put(update_dns_provider),
        )
        .route(
            "/api/acme/challenge-status/:domain",
            get(get_challenge_status),
        );

    // Only apply auth middleware if password is set
    if auth_config.is_enabled() {
        protected_routes = protected_routes.layer(middleware::from_fn_with_state(
            auth_config.clone(),
            auth_middleware,
        ));
    }

    // Static file service for frontend (embedded)
    #[cfg(feature = "embed-frontend")]
    let static_routes = Router::new()
        .route(
            "/",
            get(|| async { serve_static(Path("".to_string())).await }),
        )
        .route("/*path", get(serve_static));

    let app = Router::new().merge(public_routes).merge(protected_routes);

    #[cfg(feature = "embed-frontend")]
    let app = app.merge(static_routes);

    let app = app.layer(cors).with_state(state);

    match tls_config {
        Some(tls_config) => {
            // Extract port 80 address from api_addr for HTTP redirect
            let http_addr = {
                let parts: Vec<&str> = api_addr.split(':').collect();
                if parts.len() == 2 {
                    format!("{}:80", parts[0])
                } else {
                    format!("0.0.0.0:80")
                }
            };

            // Start HTTP redirect server on port 80
            let http_app = axum::Router::new()
                .fallback(|req: axum::http::Request<Body>| async move {
                    let uri = req.uri();
                    let host = uri.host().unwrap_or("localhost").to_string();
                    let path = format!(
                        "https://{host}{}",
                        uri.path_and_query().map(|p| p.as_str()).unwrap_or("/")
                    );
                    (
                        StatusCode::MOVED_PERMANENTLY,
                        [(axum::http::header::LOCATION, path)],
                    )
                        .into_response()
                });

            tokio::spawn(async move {
                let http_listener = match tokio::net::TcpListener::bind(&http_addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!(
                            "Failed to bind HTTP redirect server on {}: {}",
                            http_addr,
                            e
                        );
                        return;
                    }
                };
                tracing::info!("HTTP redirect server listening on {}", http_addr);
                if let Err(e) = axum::serve(http_listener, http_app).await {
                    tracing::error!("HTTP redirect server error: {}", e);
                }
            });

            // Start HTTPS server on api_addr
            let tls_acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
            let listener = tokio::net::TcpListener::bind(&api_addr).await?;
            tracing::info!("HTTPS API server listening on {}", api_addr);

            loop {
                let (tcp_stream, _remote_addr) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::error!("Failed to accept TLS connection: {}", e);
                        continue;
                    }
                };

                let tls_acceptor = tls_acceptor.clone();
                let app = app.clone();

                tokio::spawn(async move {
                    let tls_stream = match tls_acceptor.accept(tcp_stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!("TLS handshake failed: {}", e);
                            return;
                        }
                    };

                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let service =
                        hyper_util::service::TowerToHyperService::new(app.into_service());

                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        tracing::error!("HTTPS connection error: {}", e);
                    }
                });
            }
        }
        None => {
            // Plain HTTP — original behavior
            let listener = tokio::net::TcpListener::bind(&api_addr).await?;
            tracing::info!("API server listening on {}", api_addr);
            axum::serve(listener, app).await?;
            Ok(())
        }
    }
}
