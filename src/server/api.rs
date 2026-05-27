use crate::server::quality::{ConnectionQuality, QualitySample};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, Response, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Timelike, Utc};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

use crate::server::auth::{auth_middleware, create_token, AuthConfig};
use crate::server::control::ServerState;
use crate::server::db::Database;

/// Embedded frontend assets
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

/// API state shared across all handlers
#[derive(Clone)]
pub struct ApiState {
    pub server_state: ServerState,
    pub auth_config: Arc<AuthConfig>,
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
async fn serve_static(Path(path): Path<String>) -> impl IntoResponse {
    let path = if path.is_empty() { "index.html" } else { &path };

    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data))
                .unwrap()
        }
        None => {
            // Fallback to index.html for SPA routing
            if let Some(index) = FrontendAssets::get("index.html") {
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::from(index.data))
                    .unwrap()
            } else {
                Response::builder()
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

/// Create and run the API server
pub async fn run_api_server(
    api_addr: String,
    server_state: ServerState,
    auth_config: AuthConfig,
) -> Result<(), std::io::Error> {
    let auth_config = Arc::new(auth_config);

    let state = ApiState {
        server_state,
        auth_config: auth_config.clone(),
    };

    // CORS layer
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Public routes (no auth required)
    let public_routes = Router::new()
        .route("/api/login", post(login))
        .route("/api/health", get(health));

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
        .route("/api/trojan/quality", get(get_trojan_quality));

    // Only apply auth middleware if password is set
    if auth_config.is_enabled() {
        protected_routes = protected_routes.layer(middleware::from_fn_with_state(
            auth_config.clone(),
            auth_middleware,
        ));
    }

    // Static file service for frontend (embedded)
    let static_routes = Router::new()
        .route(
            "/",
            get(|| async { serve_static(Path("".to_string())).await }),
        )
        .route("/*path", get(serve_static));

    let app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .merge(static_routes)
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&api_addr).await?;
    tracing::info!("API server listening on {}", api_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
