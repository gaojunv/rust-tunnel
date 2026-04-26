use axum::{
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post, delete},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

use crate::server::auth::{auth_middleware, Auth, AuthConfig, create_token};
use crate::server::control::{ClientInfo, ServerState};

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
}

impl TrafficStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Record incoming bytes (from user to server)
    pub async fn record_bytes_in(&self, port: u16, bytes: u64) {
        let mut store = self.inner.lock().await;
        let port_traffic = store.entry(port).or_insert_with(|| PortTraffic {
            port,
            total_bytes_in: 0,
            total_bytes_out: 0,
            buckets: VecDeque::new(),
        });
        port_traffic.total_bytes_in += bytes;
        self.add_to_bucket(port_traffic, bytes, 0).await;
    }

    /// Record outgoing bytes (from server to user)
    pub async fn record_bytes_out(&self, port: u16, bytes: u64) {
        let mut store = self.inner.lock().await;
        let port_traffic = store.entry(port).or_insert_with(|| PortTraffic {
            port,
            total_bytes_in: 0,
            total_bytes_out: 0,
            buckets: VecDeque::new(),
        });
        port_traffic.total_bytes_out += bytes;
        self.add_to_bucket(port_traffic, 0, bytes).await;
    }

    async fn add_to_bucket(&self, port_traffic: &mut PortTraffic, bytes_in: u64, bytes_out: u64) {
        let now = Utc::now();
        // Truncate to minute
        let bucket_time = now - chrono::Duration::seconds(now.second() as i64);

        // Check if we have a bucket for this minute
        if let Some(last) = port_traffic.buckets.back_mut() {
            if last.timestamp == bucket_time {
                last.bytes_in += bytes_in;
                last.bytes_out += bytes_out;
                return;
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
    pub async fn remove_port(&self, port: u16) {
        let mut store = self.inner.lock().await;
        store.remove(&port);
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
    pub connection_count: usize,
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
        });
    }

    if state.auth_config.verify_password(&request.password) {
        match create_token(&state.auth_config.jwt_secret) {
            Ok(token) => Json(LoginResponse {
                token,
                auth_required: true,
            }),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create token").into_response(),
        }
    } else {
        (StatusCode::UNAUTHORIZED, "Invalid password").into_response()
    }
}

// Logout handler (client just discards token)
async fn logout(_auth: Auth) -> impl IntoResponse {
    StatusCode::OK
}

// List all clients
async fn list_clients(State(state): State<ApiState>, _auth: Auth) -> Json<Vec<ClientResponse>> {
    let clients = state.server_state.get_all_clients().await;
    Json(
        clients
            .into_iter()
            .map(|(port, _info)| ClientResponse {
                port,
                connection_count: 0, // Will track later
            })
            .collect(),
    )
}

// Disconnect client
async fn disconnect_client(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
    _auth: Auth,
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
async fn get_traffic(State(state): State<ApiState>, _auth: Auth) -> Json<Vec<PortTraffic>> {
    Json(state.server_state.traffic_store.get_all_traffic().await)
}

// Get traffic for specific port
async fn get_port_traffic(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
    _auth: Auth,
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
async fn get_metrics(State(state): State<ApiState>, _auth: Auth) -> Json<ServerMetrics> {
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

    // Protected routes (require auth)
    let protected_routes = Router::new()
        .route("/api/logout", post(logout))
        .route("/api/clients", get(list_clients))
        .route("/api/clients/:port", delete(disconnect_client))
        .route("/api/traffic", get(get_traffic))
        .route("/api/traffic/:port", get(get_port_traffic))
        .route("/api/metrics", get(get_metrics))
        .layer(middleware::from_fn_with_state(
            auth_config.clone(),
            auth_middleware,
        ));

    // Static file service for frontend
    let serve_dir = ServeDir::new("frontend-dist");

    let app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .fallback_service(serve_dir)
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&api_addr).await?;
    tracing::info!("API server listening on {}", api_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
