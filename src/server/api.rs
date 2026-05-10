use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, Response, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post, delete},
    Json, Router,
};
use crate::server::quality::{ConnectionQuality, QualitySample};
use chrono::{DateTime, Utc, Timelike};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

use crate::server::auth::{auth_middleware, AuthConfig, create_token};
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
}

impl TrafficStore {
    /// Create a new traffic store without database (for backwards compatibility)
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::collections::HashMap::new())),
            db: None,
        }
    }

    /// Create a new traffic store with database persistence
    pub fn with_db(db: Database) -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::collections::HashMap::new())),
            db: Some(db),
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
        let bucket_time;
        {
            let mut store = self.inner.lock().await;
            let port_traffic = store.entry(port).or_insert_with(|| PortTraffic {
                port,
                total_bytes_in: 0,
                total_bytes_out: 0,
                buckets: VecDeque::new(),
            });
            port_traffic.total_bytes_in += bytes;
            bucket_time = Self::add_to_bucket(port_traffic, bytes, 0);
        }

        // Persist to database asynchronously
        if let Some(db) = &self.db {
            let db = db.clone();
            tokio::spawn(async move {
                let _ = db.upsert_port_traffic(port, bytes, 0).await;
                let _ = db.upsert_traffic_bucket(port, bucket_time, bytes, 0).await;
            });
        }
    }

    /// Record outgoing bytes (from server to user)
    pub async fn record_bytes_out(&self, port: u16, bytes: u64) {
        let bucket_time;
        {
            let mut store = self.inner.lock().await;
            let port_traffic = store.entry(port).or_insert_with(|| PortTraffic {
                port,
                total_bytes_in: 0,
                total_bytes_out: 0,
                buckets: VecDeque::new(),
            });
            port_traffic.total_bytes_out += bytes;
            bucket_time = Self::add_to_bucket(port_traffic, 0, bytes);
        }

        // Persist to database asynchronously
        if let Some(db) = &self.db {
            let db = db.clone();
            tokio::spawn(async move {
                let _ = db.upsert_port_traffic(port, 0, bytes).await;
                let _ = db.upsert_traffic_bucket(port, bucket_time, 0, bytes).await;
            });
        }
    }

    fn add_to_bucket(port_traffic: &mut PortTraffic, bytes_in: u64, bytes_out: u64) -> DateTime<Utc> {
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
        }).into_response();
    }

    if state.auth_config.verify_password(&request.password) {
        match create_token(&state.auth_config.jwt_secret) {
            Ok(token) => Json(LoginResponse {
                token,
                auth_required: true,
            }).into_response(),
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
    let mut response = Vec::with_capacity(clients.len());
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
    match state.server_state.traffic_store.get_port_traffic(port).await {
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
        .route("/api/quality/warnings", get(get_quality_warnings));

    // Only apply auth middleware if password is set
    if auth_config.is_enabled() {
        protected_routes = protected_routes.layer(middleware::from_fn_with_state(
            auth_config.clone(),
            auth_middleware,
        ));
    }

    // Static file service for frontend (embedded)
    let static_routes = Router::new()
        .route("/", get(|| async { serve_static(Path("".to_string())).await }))
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
