use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

use super::ApiState;

// Get Trojan configuration
pub async fn get_trojan_config(State(state): State<ApiState>) -> Json<super::dto::TrojanConfig> {
    let (enabled, port, fallback, domain) = {
        let dc = state.server_state.dynamic_config.read().await;
        match dc.trojan.as_ref() {
            Some(t) => (
                t.enabled,
                Some(t.port),
                Some(t.fallback.clone()),
                Some(t.domain.clone()),
            ),
            None => (false, None, None, None),
        }
    };
    let rt = state.server_state.trojan_runtime.read().await;
    Json(super::dto::TrojanConfig {
        enabled,
        port,
        fallback,
        domain,
        cert_source: rt.cert_source.clone(),
        shared: rt.shared,
    })
}

// Update Trojan configuration (start/stop/modify)
pub async fn update_trojan_config(
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
    let fallback = payload["fallback"].as_str().unwrap_or("127.0.0.1:80");

    // 现有配置：password/domain 留空时保留原值
    let existing = {
        let dc = state.server_state.dynamic_config.read().await;
        dc.trojan.clone()
    };

    // password 可选：留空保留原密码（首次启用必须提供）
    let password = match payload["password"].as_str() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => match existing.as_ref() {
            Some(t) if !t.password.is_empty() => t.password.clone(),
            _ => {
                return (StatusCode::BAD_REQUEST, "Password required").into_response();
            }
        },
    };

    // domain 可选：空串 = 不用 ACME 证书、不参与 SNI 分流
    let domain = match payload["domain"].as_str() {
        Some(d) => d.trim().to_ascii_lowercase(),
        None => existing
            .as_ref()
            .map(|t| t.domain.clone())
            .unwrap_or_default(),
    };
    if !domain.is_empty() {
        if let Err(e) = crate::trojan::validate_trojan_domain(&domain) {
            return (StatusCode::BAD_REQUEST, format!("Invalid domain: {}", e)).into_response();
        }
    }

    // 端口与反代共享的边界规则检查
    if enabled {
        if let Some((listen_addr, tls_enabled)) = state
            .server_state
            .proxy_state
            .http_listen_addr_for_port(port)
            .await
        {
            if !tls_enabled {
                return (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Port {} is used by reverse proxy listener {} without TLS. Trojan requires TLS.",
                        port, listen_addr
                    ),
                )
                    .into_response();
            }
            if domain.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Port {} is used by reverse proxy listener {}. Set a domain to share it via SNI.",
                        port, listen_addr
                    ),
                )
                    .into_response();
            }
        }
    }

    // Save to DB（单份配置语义：整表替换，避免修改端口时残留旧行）
    if let Some(db) = state.server_state.db() {
        if let Err(e) = db
            .replace_trojan_config(port, &password, fallback, enabled, &domain)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    // Update in-memory config（new_cfg 同时供 lifecycle 段复用）
    let new_cfg = crate::dynamic_config::TrojanDynamicConfig {
        enabled,
        port,
        password: password.clone(),
        fallback: fallback.to_string(),
        domain: domain.clone(),
    };
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.trojan = Some(new_cfg.clone());
    }

    // Handle listener lifecycle（证书解析 + 共享/独立模式判定统一在 trojan_runtime）
    if let Err(e) = crate::trojan_runtime::apply_trojan_config(&state.server_state, &new_cfg).await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to apply trojan config: {}", e),
        )
            .into_response();
    }

    Json(serde_json::json!({"status": "ok", "enabled": enabled, "port": port, "domain": domain}))
        .into_response()
}
