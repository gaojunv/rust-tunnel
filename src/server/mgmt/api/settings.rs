use axum::{extract::State, response::IntoResponse, Json};

use super::ApiState;

// ── Settings Endpoints ─────────────────────────────────────────────

/// Get all dynamic configuration
pub async fn get_settings(State(state): State<ApiState>) -> impl IntoResponse {
    let dc = state.server_state.dynamic_config.read().await;
    Json(serde_json::json!({
        "log_level": dc.log_level,
        "api_tls": state.server_state.api_tls,
        "api_domain": state.server_state.api_domain,
        "shadowsocks": dc.ss,
        "trojan": dc.trojan,
        "reverse_proxy": dc.reverse_proxy,
        "dns": dc.dns,
    }))
}
