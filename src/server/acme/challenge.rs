use super::AcmeState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::{info, warn};

/// HTTP-01 challenge server
pub struct ChallengeServer {
    state: AcmeState,
    port: u16,
}

impl ChallengeServer {
    /// Create a new challenge server
    pub fn new(state: AcmeState, port: u16) -> Self {
        Self { state, port }
    }

    /// Start the challenge server
    pub async fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        let state = self.state.clone();

        let app = Router::new()
            .route("/.well-known/acme-challenge/{token}", get(handle_challenge))
            .with_state(state);

        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        let listener = TcpListener::bind(addr).await?;

        info!("ACME challenge server listening on {}", addr);

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("Challenge server error: {}", e);
            }
        });

        Ok(())
    }
}

/// Handle ACME challenge request
async fn handle_challenge(
    State(state): State<AcmeState>,
    Path(token): Path<String>,
) -> impl IntoResponse {
    match state.get_challenge(&token).await {
        Some(authorization) => {
            info!("Serving ACME challenge for token: {}", token);
            (StatusCode::OK, authorization)
        }
        None => {
            warn!("ACME challenge not found for token: {}", token);
            (StatusCode::NOT_FOUND, "Challenge not found".to_string())
        }
    }
}
