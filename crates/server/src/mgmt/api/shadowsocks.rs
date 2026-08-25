use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

use super::{dto::ShadowsocksConfig, ApiState};

// Get Shadowsocks configuration
pub async fn get_shadowsocks_config(State(state): State<ApiState>) -> Json<ShadowsocksConfig> {
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

// Update Shadowsocks configuration (start/stop/modify)
pub async fn update_shadowsocks_config(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let enabled = payload["enabled"].as_bool().unwrap_or(false);
    let port = match payload["port"].as_u64() {
        Some(p) if p > 0 && p <= 65535 => p as u16,
        _ => {
            return (StatusCode::BAD_REQUEST, "Invalid or missing port").into_response();
        }
    };
    let cipher = match payload["cipher"].as_str() {
        Some(c @ "aes-256-gcm") | Some(c @ "chacha20-ietf-poly1305") => c,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Invalid cipher. Supported: aes-256-gcm, chacha20-ietf-poly1305",
            )
                .into_response();
        }
    };
    let password = match payload["password"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => {
            return (StatusCode::BAD_REQUEST, "Password required").into_response();
        }
    };

    // Save to DB（单份配置语义：整表替换，避免修改端口时残留旧行）
    if let Some(db) = state.server_state.db() {
        if let Err(e) = db
            .replace_shadowsocks_config(port, cipher, password, enabled)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    // Update in-memory config
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.ss = Some(crate::dynamic_config::ShadowsocksDynamicConfig {
            enabled,
            port,
            cipher: cipher.to_string(),
            password: password.to_string(),
        });
    }

    // Handle listener lifecycle
    {
        let mut abort = state
            .server_state
            .proxy_ports
            .ss_listener_abort
            .write()
            .await;
        // Stop existing listener if any
        if let Some(tx) = abort.take() {
            let _ = tx.send(true);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        if enabled {
            let (abort_tx, abort_rx) = tokio::sync::watch::channel(false);
            *abort = Some(abort_tx);
            let state_clone = state.server_state.clone();
            let ss_port = port;
            let ss_cipher = cipher.to_string();
            let ss_password = password.to_string();
            tokio::spawn(async move {
                let stats = state_clone.stats_collector.clone();
                let registry: std::sync::Arc<dyn rust_tunnel_protocols::PortRegistry> =
                    std::sync::Arc::new(state_clone);
                if let Err(e) = crate::listener::start_shadowsocks_listener_with_abort(
                    registry,
                    stats,
                    ss_port,
                    ss_cipher,
                    ss_password,
                    abort_rx,
                )
                .await
                {
                    tracing::error!("SS listener error: {}", e);
                }
            });
        }
    }

    Json(serde_json::json!({"status": "ok", "enabled": enabled, "port": port})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
