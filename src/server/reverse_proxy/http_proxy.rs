use super::ReverseProxyState;
use crate::server::db::Database;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

/// HTTP reverse proxy handler
pub struct HttpProxy {
    state: ReverseProxyState,
    db: Option<Database>,
}

impl HttpProxy {
    /// Create a new HTTP proxy
    pub fn new(state: ReverseProxyState, db: Option<Database>) -> Self {
        Self { state, db }
    }

    /// Start HTTP proxy listener on the given address
    pub async fn start(
        &self,
        addr: SocketAddr,
        rule_id: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = self.state.clone();
        let db = self.db.clone();

        let app = Router::new()
            .fallback(any(handle_proxy_request))
            .with_state((state, rule_id.clone()));

        let listener = TcpListener::bind(addr).await?;
        info!("HTTP proxy listening on {}", addr);

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!("HTTP proxy error: {}", e);
            }
        });

        // Store the listener handle
        let mut listeners = self.state.listeners.lock().await;
        listeners.insert(rule_id, handle);

        Ok(())
    }
}

/// Handle proxy request
async fn handle_proxy_request(
    State((state, rule_id)): State<(ReverseProxyState, String)>,
    req: Request<Body>,
) -> impl IntoResponse {
    // Extract host from request
    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // Match route
    let route_match = state
        .rules
        .lock()
        .await
        .get(&rule_id)
        .map(|rule| rule.routes.clone());

    let Some(routes) = route_match else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("No matching route"))
            .unwrap();
    };

    // Find matching route and backend
    let backend_addr = find_backend(&routes, &path);

    let Some(backend) = backend_addr else {
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("No backend available"))
            .unwrap();
    };

    // Build upstream URL
    let upstream_uri = format!("http://{}{}", backend, path);

    // Create HTTP client
    let client = reqwest::Client::new();

    // Build upstream request
    let mut upstream_req = client.request(method, &upstream_uri);

    // Copy headers
    for (key, value) in req.headers() {
        if key != "host" {
            upstream_req = upstream_req.header(key, value);
        }
    }

    // Add proxy headers
    upstream_req = upstream_req.header("X-Forwarded-For", "127.0.0.1");
    upstream_req = upstream_req.header("X-Real-IP", "127.0.0.1");

    // Get request body
    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();

    // Send to backend
    match upstream_req.body(body_bytes.to_vec()).send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

            let mut proxy_response = Response::builder().status(status);

            // Copy response headers
            for (key, value) in response.headers() {
                proxy_response = proxy_response.header(key, value);
            }

            let body = response.bytes().await.unwrap_or_default();
            proxy_response.body(Body::from(body)).unwrap()
        }
        Err(e) => {
            error!("Backend request failed: {}", e);
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("Backend error: {}", e)))
                .unwrap()
        }
    }
}

/// Find a backend address from routes
fn find_backend(routes: &[super::Route], path: &str) -> Option<String> {
    // Sort routes by path length (longest first)
    let mut sorted_routes: Vec<&super::Route> = routes.iter().collect();
    sorted_routes.sort_by(|a, b| b.path.len().cmp(&a.path.len()));

    for route in sorted_routes {
        if path.starts_with(&route.path) || route.path == "/" {
            if let Some(backend) = route.backends.first() {
                return Some(backend.addr.clone());
            }
        }
    }

    // Default: first route's first backend
    routes
        .first()
        .and_then(|r| r.backends.first())
        .map(|b| b.addr.clone())
}
