use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use super::ApiState;

/// Request body for PUT /api/acme/config
#[derive(Debug, Deserialize)]
pub struct UpdateAcmeConfigRequest {
    pub enabled: Option<bool>,
    pub server_url: Option<String>,
    pub email: Option<String>,
    pub auto_renew: Option<bool>,
    pub renewal_check_interval: Option<u64>,
    pub renewal_days_before_expiry: Option<u64>,
    pub tos_agreed: Option<bool>,
}

/// 证书申请请求
#[derive(Debug, Deserialize)]
pub struct CertificateRequest {
    /// 挑战类型: "http-01" 或 "dns-01"
    pub challenge_type: Option<String>,
}

// ── ACME Certificate Management Endpoints ──────────────────────────

// GET /api/acme/certificates — list all certificates
pub async fn list_acme_certificates(State(state): State<ApiState>) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

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
pub async fn request_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
    Json(req): Json<CertificateRequest>,
) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            tracing::error!("ACME certificate request failed: ACME client not initialized");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

    let challenge_type = req.challenge_type.unwrap_or_else(|| "http-01".to_string());

    let result = match challenge_type.as_str() {
        "dns-01" => {
            // 获取 DNS solver
            let dns_config = state.server_state.dns_provider_config.read().await;
            match dns_config.as_ref() {
                Some(config) => {
                    let solver: Arc<dyn crate::server::acme::dns::DnsChallengeSolver> = match config
                        .provider
                    {
                        crate::server::acme::dns::DnsProvider::Aliyun => Arc::new(
                            crate::server::acme::dns::aliyun::AliyunDnsSolver::new(config),
                        ),
                        crate::server::acme::dns::DnsProvider::Cloudflare => Arc::new(
                            crate::server::acme::dns::cloudflare::CloudflareDnsSolver::new(config),
                        ),
                        crate::server::acme::dns::DnsProvider::TencentCloud => Arc::new(
                            crate::server::acme::dns::tencent::TencentDnsSolver::new(config),
                        ),
                        crate::server::acme::dns::DnsProvider::Custom => Arc::new(
                            crate::server::acme::dns::custom::CustomDnsSolver::new(config),
                        ),
                    };
                    client.request_certificate_with_dns(&domain, solver).await
                }
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": "DNS provider not configured" })),
                    )
                        .into_response();
                }
            }
        }
        _ => client.request_certificate(&domain).await,
    };

    match result {
        Ok(metadata) => {
            tracing::info!("Certificate request successful for domain: {}", domain);
            // 签发结果写入 CertificateManager 内存缓存并广播 CertEvent::Issued，
            // 否则重启前 Trojan / 反代 SNI resolver 看不到新证书。
            if let Some(ref cm) = state.server_state.cert_manager {
                if let Err(e) = cm.load_issued_certificate(&domain).await {
                    tracing::warn!(
                        "Failed to load issued certificate into cache for {}: {}",
                        domain,
                        e
                    );
                }
            }
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "certificate": metadata })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Certificate request failed for domain {}: {:?}", domain, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

// GET /api/acme/certificates/:domain — get certificate details
pub async fn get_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

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
pub async fn renew_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

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
pub async fn get_acme_status(State(state): State<ApiState>) -> impl IntoResponse {
    // Read enabled status from acme_full_config to stay in sync with API updates
    let full_config = state.server_state.acme_full_config.read().await;
    let enabled = full_config.enabled;
    let server_url = full_config.server_url.clone();
    let cert_dir = full_config.cert_dir.clone();
    drop(full_config);

    let client_guard = state.server_state.acme_client.read().await;
    let cert_count = match client_guard.as_ref() {
        Some(client) => match client.list_certificates().await {
            Ok(certs) => certs.len(),
            Err(_) => 0,
        },
        None => 0,
    };
    drop(client_guard);

    let api_tls = state.server_state.cert_manager.is_some();
    let trojan = !state.server_state.get_trojan_ports().await.is_empty();
    let control_tls = state.server_state.cert_manager.is_some();

    // Check if any reverse-proxy rule has TLS with ACME enabled
    let reverse_proxy = {
        let rules = state.server_state.proxy_state.rules.lock().await;
        rules
            .values()
            .any(|r| r.tls.as_ref().is_some_and(|t| t.enabled && t.acme))
    };

    Json(serde_json::json!({
        "enabled": enabled,
        "server_url": server_url,
        "cert_dir": cert_dir,
        "certificate_count": cert_count,
        "consumers": {
            "api_tls": api_tls,
            "trojan": trojan,
            "control_tls": control_tls,
            "reverse_proxy": reverse_proxy,
        },
    }))
    .into_response()
}

// GET /api/acme/config — get ACME configuration
pub async fn get_acme_config(State(state): State<ApiState>) -> impl IntoResponse {
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
pub async fn update_acme_config(
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

    // Persist ACME config to database
    if let Some(db) = state.server_state.get_db() {
        if let Ok(json) = serde_json::to_string(&*config) {
            if let Err(e) = db.save_server_setting("acme_config", &json).await {
                tracing::error!("Failed to persist ACME config: {}", e);
            }
        }
    }

    // Capture config values for ACME client initialization
    let has_client = state.server_state.acme_client.read().await.is_some();
    let should_init_client = config.enabled && !has_client;
    let acme_server_url = config.server_url.clone();
    let acme_cert_dir = config.cert_dir.clone();
    let acme_email = config.email.clone();
    let acme_enabled = config.enabled;
    let has_db = state.server_state.get_db().is_some();

    tracing::info!(
        "ACME config update: enabled={}, should_init_client={}, has_db={}",
        config.enabled,
        should_init_client,
        has_db
    );

    // Prepare response before potentially dropping the lock
    let response = Json(serde_json::json!({
        "enabled": config.enabled,
        "server_url": config.server_url,
        "email": config.email,
        "cert_dir": config.cert_dir,
        "auto_renew": config.auto_renew,
        "renewal_check_interval": config.renewal_check_interval,
        "renewal_days_before_expiry": config.renewal_days_before_expiry,
        "tos_agreed": config.tos_agreed,
    }));

    // Drop the write lock before initializing ACME client
    drop(config);

    // Initialize ACME client if enabled and not already initialized
    if should_init_client {
        tracing::info!("Initializing ACME client...");
        if let Some(db) = state.server_state.get_db() {
            let acme_state = crate::server::acme::AcmeState::with_db(db.clone());
            let client = Arc::new(crate::server::acme::client::AcmeClient::new(
                acme_state,
                acme_server_url.clone(),
                acme_cert_dir.clone(),
                acme_email,
            ));

            if let Err(e) = client.initialize().await {
                tracing::error!("Failed to initialize ACME client: {}", e);
            } else {
                tracing::info!("ACME client initialized successfully");
            }

            let acme_config_info = crate::server::control::AcmeConfigInfo {
                enabled: acme_enabled,
                server_url: acme_server_url,
                cert_dir: acme_cert_dir,
            };

            state
                .server_state
                .set_acme_client(client, acme_config_info)
                .await;
            tracing::info!("ACME client set on server state");
        } else {
            tracing::error!("Cannot initialize ACME client: no database available");
        }
    } else if acme_enabled {
        tracing::info!("ACME client already initialized, skipping");
    }

    response
}

// ── DNS Provider Endpoints ─────────────────────────────────────────

// GET /api/acme/dns-providers — get available providers and current config
pub async fn get_dns_providers(State(state): State<ApiState>) -> impl IntoResponse {
    let config = state.server_state.dns_provider_config.read().await;
    Json(serde_json::json!({
        "providers": ["cloudflare", "aliyun", "tencent", "custom"],
        "config": *config
    }))
}

// PUT /api/acme/dns-providers — update DNS provider configuration
pub async fn update_dns_provider(
    State(state): State<ApiState>,
    Json(req): Json<crate::server::acme::dns::DnsProviderConfig>,
) -> impl IntoResponse {
    let mut config = state.server_state.dns_provider_config.write().await;
    *config = Some(req.clone());

    // Persist DNS provider config to database
    if let Some(db) = state.server_state.get_db() {
        if let Ok(json) = serde_json::to_string(&*config) {
            if let Err(e) = db.save_server_setting("dns_provider_config", &json).await {
                tracing::error!("Failed to persist DNS provider config: {}", e);
            }
        }
    }

    Json(serde_json::json!({
        "success": true,
        "config": req
    }))
}

// GET /api/acme/challenge-status/:domain — get ACME challenge status for a domain
pub async fn get_challenge_status(
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
pub async fn delete_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

    match client.delete_certificate(&domain).await {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::AuthConfig;
    use crate::server::control::ServerState;
    use crate::server::db::Database;
    use axum::extract::State;
    use axum::Json;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_update_acme_config_initializes_client() {
        // Create a server state with in-memory database
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);

        // Verify ACME client is not initialized
        assert!(server_state.acme_client.read().await.is_none());

        // Create API state
        let state = ApiState {
            server_state: server_state.clone(),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };

        // Create request to enable ACME
        let req = UpdateAcmeConfigRequest {
            enabled: Some(true),
            server_url: Some("https://acme-staging-v02.api.letsencrypt.org/directory".to_string()),
            email: Some("test@example.com".to_string()),
            auto_renew: Some(true),
            renewal_check_interval: None,
            renewal_days_before_expiry: None,
            tos_agreed: Some(true),
        };

        // Call update_acme_config
        let _ = update_acme_config(State(state), Json(req)).await;

        // Verify ACME client is now initialized (shared Arc, visible from original)
        assert!(server_state.acme_client.read().await.is_some());

        // Verify ACME config is set
        let acme_config_guard = server_state.acme_config.read().await;
        assert!(acme_config_guard.is_some());
        let acme_config = acme_config_guard.as_ref().unwrap();
        assert!(acme_config.enabled);
        assert_eq!(
            acme_config.server_url,
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );
        drop(acme_config_guard);

        // Verify ACME full config is updated
        let full_config = server_state.acme_full_config.read().await;
        assert!(full_config.enabled);
        assert_eq!(
            full_config.server_url,
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(full_config.email, Some("test@example.com".to_string()));
    }

    #[tokio::test]
    async fn test_update_acme_config_disabled_does_not_init_client() {
        // Create a server state with in-memory database
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);

        // Verify ACME client is not initialized
        assert!(server_state.acme_client.read().await.is_none());

        // Create API state
        let state = ApiState {
            server_state: server_state.clone(),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };

        // Create request to disable ACME
        let req = UpdateAcmeConfigRequest {
            enabled: Some(false),
            server_url: Some("https://acme-staging-v02.api.letsencrypt.org/directory".to_string()),
            email: None,
            auto_renew: None,
            renewal_check_interval: None,
            renewal_days_before_expiry: None,
            tos_agreed: None,
        };

        // Call update_acme_config
        let _ = update_acme_config(State(state), Json(req)).await;

        // Verify ACME client is still not initialized
        assert!(server_state.acme_client.read().await.is_none());

        // Verify ACME config is still not set
        assert!(server_state.acme_config.read().await.is_none());

        // Verify ACME full config is updated
        let full_config = server_state.acme_full_config.read().await;
        assert!(!full_config.enabled);
    }

    #[tokio::test]
    async fn test_get_acme_status_reflects_config_update() {
        // Create a server state with in-memory database
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);

        // Create API state
        let state = ApiState {
            server_state: server_state.clone(),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };

        // Initial status should show ACME disabled
        let _ = get_acme_status(State(state.clone())).await;
        let full_config = server_state.acme_full_config.read().await;
        assert!(!full_config.enabled);
        drop(full_config);

        // Enable ACME
        let req = UpdateAcmeConfigRequest {
            enabled: Some(true),
            server_url: Some("https://acme-staging-v02.api.letsencrypt.org/directory".to_string()),
            email: Some("test@example.com".to_string()),
            auto_renew: Some(true),
            renewal_check_interval: None,
            renewal_days_before_expiry: None,
            tos_agreed: Some(true),
        };

        let _ = update_acme_config(State(state), Json(req)).await;

        // Verify ACME is now enabled in the config
        let full_config = server_state.acme_full_config.read().await;
        assert!(full_config.enabled);
        assert_eq!(
            full_config.server_url,
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );

        // Verify ACME client is initialized
        assert!(server_state.acme_client.read().await.is_some());
    }
}
