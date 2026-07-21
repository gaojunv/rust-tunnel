use axum::{
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

pub mod acme;
pub mod clients;
pub mod dns;
pub mod dto;
pub mod login;
pub mod logs;
pub mod mesh;
pub mod server_auth;
pub mod shadowsocks;
pub mod static_files;
pub mod stats;
pub mod trojan;

pub use dto::*;

use crate::server::auth::{auth_middleware, AuthConfig};
use crate::server::control::ServerState;
use crate::server::reverse_proxy::ProxyRule;

#[cfg(test)]
mod tests {
    use super::*;
    use super::stats;
    use crate::server::db::Database;
    use chrono::Timelike;

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


    // ── Stats unified API tests ──────────────────────────────────

    #[tokio::test]
    async fn test_stats_summary_empty_is_all_zero() {
        let state = ApiState {
            server_state: ServerState::new(),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let response = stats::get_stats_summary(State(state)).await;
        assert_eq!(response.into_response().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_stats_summary_reflects_recorded_data() {
        let server_state = ServerState::new();
        server_state.stats_collector.record_bytes(
            crate::server::stats::EntityType::Proxy,
            "rule1",
            100,
            200,
        );
        server_state
            .stats_collector
            .incr_conns(crate::server::stats::EntityType::Client, "home-nas");

        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let response = stats::get_stats_summary(State(state)).await;
        assert_eq!(response.into_response().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_stats_summary_empty_returns_default() {
        let server_state = ServerState::new();
        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        // Verify the handler doesn't panic with an empty state
        let response = stats::get_stats_summary(State(state)).await;
        assert_eq!(response.into_response().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_stats_query_requires_start_end() {
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);
        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        // Query with invalid params should return error
        let response = stats::get_stats_query(
            State(state),
            axum_extra::extract::Query(stats::StatsQueryParams {
                entity_type: None,
                entity_id: None,
                start: "not-a-date".to_string(),
                end: "2026-01-01T00:00:00Z".to_string(),
            }),
        )
        .await;
        let resp = response.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_stats_query_range_exceeds_7_days() {
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);
        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let response = stats::get_stats_query(
            State(state),
            axum_extra::extract::Query(stats::StatsQueryParams {
                entity_type: None,
                entity_id: None,
                start: "2026-01-01T00:00:00Z".to_string(),
                end: "2026-01-10T00:00:00Z".to_string(),
            }),
        )
        .await;
        let resp = response.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_stats_query_valid_range_returns_data() {
        let db = Database::new(":memory:").await.unwrap();
        // Insert test data directly
        let now = chrono::Utc::now();
        let ts = now - chrono::Duration::seconds(now.second() as i64);
        sqlx::query(
            "INSERT INTO stats_snapshots (entity_type, entity_id, timestamp, bytes_in, bytes_out, bytes_in_rate, bytes_out_rate, rtt_ms, loss_pct, active_conns) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("shadowsocks")
        .bind("ss:8388")
        .bind(ts)
        .bind(1000_i64)
        .bind(2000_i64)
        .bind(50.0_f64)
        .bind(100.0_f64)
        .bind(Some(12.0_f64))
        .bind(None::<f64>)
        .bind(2_i32)
        .execute(&db.pool)
        .await
        .unwrap();

        let server_state = ServerState::with_db(db);
        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let response = stats::get_stats_query(
            State(state),
            axum_extra::extract::Query(stats::StatsQueryParams {
                entity_type: Some(vec!["shadowsocks".to_string()]),
                entity_id: None,
                start: (ts - chrono::Duration::minutes(1)).to_rfc3339(),
                end: (ts + chrono::Duration::minutes(1)).to_rfc3339(),
            }),
        )
        .await;
        let resp = response.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

/// API state shared across all handlers
#[derive(Clone)]
pub struct ApiState {
    pub server_state: ServerState,
    pub auth_config: Arc<AuthConfig>,
    pub log_store: Option<crate::server::logs::LogStore>,
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

    let id = uuid::Uuid::new_v4().to_string();

    let mut rule = ProxyRule {
        id: id.clone(),
        name: body.name,
        rule_type: rule_type.clone(),
        listen: body.listen,
        domains: body.domains,
        routes: body.routes,
        tls: body.tls,
        enabled: body.enabled,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        cert_status: None,
    };

    if let Err(e) = crate::server::reverse_proxy::validate_rule_for_save(&rule) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    crate::server::reverse_proxy::sanitize_rule(&mut rule);

    let cert_status = crate::server::reverse_proxy::resolve_cert_source_for_rule(
        &rule,
        state.server_state.proxy_state.cert_manager(),
    )
    .await;
    rule.cert_status = Some(cert_status.clone());

    if let Err(e) = state.server_state.proxy_state.save_rule(&rule).await {
        tracing::error!("Failed to save proxy rule: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save proxy rule" })),
        )
            .into_response();
    }

    {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.insert(id.clone(), rule.clone());
    }

    if rule.rule_type == crate::server::reverse_proxy::RuleType::Http {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&rule.listen)
            .await
        {
            tracing::warn!("Reconcile failed on create, rolling back: {}", e);
            let _ = state
                .server_state
                .proxy_state
                .rules
                .lock()
                .await
                .remove(&id);
            if let Err(del_err) = state.server_state.proxy_state.delete_rule(&id).await {
                tracing::error!(
                    "Compensating delete failed after reconcile error: {}",
                    del_err
                );
            }
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("{}", e),
                    "conflicts": conflicts_from_error(&e),
                })),
            )
                .into_response();
        }
    }

    // 反代规则变更可能影响 Trojan 共享/独立模式
    crate::server::trojan_runtime::sync_trojan_mode(&state.server_state).await;

    if cert_status.source == crate::server::reverse_proxy::CertSourceKind::PendingIssuance {
        if let Some(mgr) = state.server_state.proxy_state.cert_manager().cloned() {
            if rule.tls.as_ref().is_some_and(|t| t.acme) {
                let domain = cert_status.covering_domain.clone();
                tokio::spawn(async move {
                    if let Err(e) = mgr.request_acme_certificate(&domain).await {
                        tracing::error!("ACME issuance failed for {}: {}", domain, e);
                    }
                });
            }
        }
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "rule": rule })),
    )
        .into_response()
}

fn conflicts_from_error(
    e: &crate::server::reverse_proxy::error::ReconcileError,
) -> Vec<serde_json::Value> {
    use crate::server::reverse_proxy::error::ReconcileError as E;
    match e {
        E::DomainConflict {
            domain,
            other_rule_id,
            ..
        } => vec![serde_json::json!({
            "rule_id": other_rule_id,
            "reason": format!("domain {} already claimed", domain),
        })],
        E::TlsMismatch {
            existing_tls,
            new_tls,
            ..
        } => vec![serde_json::json!({
            "reason": format!("tls mismatch: existing={} new={}", existing_tls, new_tls),
        })],
        E::BindFailed { source, .. } => vec![serde_json::json!({
            "reason": format!("bind failed: {}", source),
        })],
        E::NoCertManager { .. } => vec![serde_json::json!({
            "reason": "TLS enabled but no certificate manager configured",
        })],
    }
}

// PUT /api/proxy/rules/:id — update a proxy rule
async fn update_proxy_rule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateProxyRuleRequest>,
) -> impl IntoResponse {
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

    let previous = {
        let rules = state.server_state.proxy_state.rules.lock().await;
        rules.get(&id).cloned()
    };
    let previous = match previous {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Rule not found" })),
            )
                .into_response();
        }
    };

    let mut rule = ProxyRule {
        id: id.clone(),
        name: body.name,
        rule_type: rule_type.clone(),
        listen: body.listen,
        domains: body.domains,
        routes: body.routes,
        tls: body.tls,
        enabled: body.enabled,
        created_at: previous.created_at.clone(),
        cert_status: None,
    };

    if let Err(e) = crate::server::reverse_proxy::validate_rule_for_save(&rule) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    crate::server::reverse_proxy::sanitize_rule(&mut rule);

    let cert_status = crate::server::reverse_proxy::resolve_cert_source_for_rule(
        &rule,
        state.server_state.proxy_state.cert_manager(),
    )
    .await;
    rule.cert_status = Some(cert_status.clone());

    if let Err(e) = state.server_state.proxy_state.save_rule(&rule).await {
        tracing::error!("Failed to save proxy rule: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save proxy rule" })),
        )
            .into_response();
    }

    {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.insert(id.clone(), rule.clone());
    }

    let old_listen = previous.listen.clone();
    let new_listen = rule.listen.clone();

    if rule.rule_type == crate::server::reverse_proxy::RuleType::Http {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&new_listen)
            .await
        {
            tracing::warn!("Reconcile failed on update, rolling back: {}", e);
            {
                let mut rules = state.server_state.proxy_state.rules.lock().await;
                rules.insert(id.clone(), previous.clone());
            }
            let _ = state.server_state.proxy_state.save_rule(&previous).await;

            let ports: std::collections::HashSet<&str> = [old_listen.as_str(), new_listen.as_str()]
                .into_iter()
                .collect();
            for port in ports {
                if let Err(rb) = state
                    .server_state
                    .proxy_state
                    .reconcile_http_listener(port)
                    .await
                {
                    tracing::error!("Rollback reconcile failed for {}: {}", port, rb);
                }
            }
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("{}", e),
                    "conflicts": conflicts_from_error(&e),
                })),
            )
                .into_response();
        }
    }
    if previous.rule_type == crate::server::reverse_proxy::RuleType::Http
        && old_listen != new_listen
    {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&old_listen)
            .await
        {
            tracing::warn!("Old-port reconcile failed after update: {}", e);
        }
    }

    // 新旧端口 reconcile 都完成后，检查 Trojan 共享/独立模式是否需要切换
    crate::server::trojan_runtime::sync_trojan_mode(&state.server_state).await;

    if cert_status.source == crate::server::reverse_proxy::CertSourceKind::PendingIssuance {
        if let Some(mgr) = state.server_state.proxy_state.cert_manager().cloned() {
            if rule.tls.as_ref().is_some_and(|t| t.acme) {
                let domain = cert_status.covering_domain.clone();
                tokio::spawn(async move {
                    if let Err(e) = mgr.request_acme_certificate(&domain).await {
                        tracing::error!("ACME issuance failed for {}: {}", domain, e);
                    }
                });
            }
        }
    }

    Json(serde_json::json!({ "rule": rule })).into_response()
}

// DELETE /api/proxy/rules/:id — delete a proxy rule
async fn delete_proxy_rule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let deleted = {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.remove(&id)
    };
    let deleted = match deleted {
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Rule not found" })),
            )
                .into_response();
        }
        Some(r) => r,
    };

    if deleted.rule_type != crate::server::reverse_proxy::RuleType::Http {
        let listener_handle = {
            let mut listeners = state.server_state.proxy_state.tcp_listeners.lock().await;
            listeners.remove(&id)
        };
        if let Some(handle) = listener_handle {
            handle.abort();
        }
    }

    if let Err(e) = state.server_state.proxy_state.delete_rule(&id).await {
        tracing::error!("Failed to delete proxy rule from database: {}", e);
    }

    if deleted.rule_type == crate::server::reverse_proxy::RuleType::Http {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&deleted.listen)
            .await
        {
            tracing::warn!("Reconcile failed on delete: {}", e);
        }
    }

    // 共享 listener 可能已删除/降级，检查 Trojan 是否需要回退独立监听
    crate::server::trojan_runtime::sync_trojan_mode(&state.server_state).await;

    Json(serde_json::json!({ "deleted": id })).into_response()
}

// ── Settings Endpoints ─────────────────────────────────────────────

/// Get all dynamic configuration
async fn get_settings(State(state): State<ApiState>) -> impl IntoResponse {
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

/// Get reverse proxy config
async fn get_reverse_proxy_config(State(state): State<ApiState>) -> impl IntoResponse {
    let dc = state.server_state.dynamic_config.read().await;
    Json(serde_json::json!(dc.reverse_proxy))
}

/// Update reverse proxy config
async fn update_reverse_proxy_config(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let max_conn = payload["max_connections"].as_u64().unwrap_or(10000) as u32;
    let timeout = payload["connection_timeout_secs"].as_u64().unwrap_or(30);
    let buffer = payload["buffer_size"].as_u64().unwrap_or(8192) as usize;

    // Save to DB
    if let Some(db) = state.server_state.db() {
        if let Err(e) = db
            .save_reverse_proxy_config(max_conn, timeout, buffer)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    // Update in-memory
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.reverse_proxy = crate::server::dynamic_config::ReverseProxySettings {
            max_connections: max_conn,
            connection_timeout_secs: timeout,
            buffer_size: buffer,
        };
    }

    StatusCode::OK.into_response()
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
        .route("/api/login", post(login::login))
        .route("/api/health", get(login::health))
        .route("/api/stats/stream", get(stats::sse_stats_stream))
        .route("/api/logs/stream", get(logs::sse_log_stream));

    // Protected routes (require auth only when password is set)
    let mut protected_routes = Router::new()
        .route("/api/logout", post(login::logout))
        // Stats query endpoints (SSE stream is in public_routes — uses ?token= query param)
        .route("/api/stats/query", get(stats::get_stats_query))
        .route("/api/stats/summary", get(stats::get_stats_summary))
        .route("/api/clients", get(clients::list_clients))
        .route(
            "/api/clients/:name",
            patch(clients::patch_client_note).delete(clients::delete_client),
        )
        .route("/api/clients/:name/kick", post(clients::kick_client))
        // Server auth token management
        .route(
            "/api/server-auth",
            get(server_auth::get_auth).put(server_auth::put_auth),
        )
        .route("/api/server-auth/rotate", post(server_auth::rotate_auth))
        // Shadowsocks management endpoints
        .route(
            "/api/shadowsocks",
            get(shadowsocks::get_shadowsocks_config).post(shadowsocks::update_shadowsocks_config),
        )
        // Trojan management endpoints
        .route(
            "/api/trojan",
            get(trojan::get_trojan_config).post(trojan::update_trojan_config),
        )
        // Mesh network endpoints
        .route("/api/mesh", get(mesh::list_meshes))
        .route("/api/mesh/:id", get(mesh::get_mesh))
        .route("/api/mesh/:id/services", get(mesh::get_mesh_services))
        // DNS management endpoints
        .route(
            "/api/dns/records",
            get(dns::get_dns_records).post(dns::add_dns_record),
        )
        .route("/api/dns/records/:name", delete(dns::delete_dns_record))
        // Log viewer endpoints (SSE stream is in public_routes — uses ?token= query param)
        .route("/api/logs", get(logs::get_logs))
        .route("/api/logs/level", get(logs::get_logs_level).put(logs::put_logs_level))
        // Proxy rules management endpoints
        .route(
            "/api/proxy/rules",
            get(list_proxy_rules).post(create_proxy_rule),
        )
        .route(
            "/api/proxy/rules/:id",
            put(update_proxy_rule).delete(delete_proxy_rule),
        )
        // ACME certificate management endpoints
        .route("/api/acme/status", get(acme::get_acme_status))
        .route(
            "/api/acme/config",
            get(acme::get_acme_config).put(acme::update_acme_config),
        )
        .route("/api/acme/certificates", get(acme::list_acme_certificates))
        .route(
            "/api/acme/certificates/:domain",
            get(acme::get_acme_certificate)
                .post(acme::request_acme_certificate)
                .delete(acme::delete_acme_certificate),
        )
        .route(
            "/api/acme/certificates/:domain/renew",
            post(acme::renew_acme_certificate),
        )
        // DNS provider endpoints
        .route(
            "/api/acme/dns-providers",
            get(acme::get_dns_providers).put(acme::update_dns_provider),
        )
        .route(
            "/api/acme/challenge-status/:domain",
            get(acme::get_challenge_status),
        )
        // Settings endpoints
        .route("/api/settings", get(get_settings))
        .route(
            "/api/settings/reverse-proxy",
            get(get_reverse_proxy_config).put(update_reverse_proxy_config),
        )
        .route(
            "/api/settings/dns",
            get(dns::get_dns_config).put(dns::update_dns_config),
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
            get(|| async { static_files::serve_static(Path("".to_string())).await }),
        )
        .route("/*path", get(static_files::serve_static));

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
                    "0.0.0.0:80".to_string()
                }
            };

            // Start HTTP redirect server on port 80
            let http_app =
                axum::Router::new().fallback(|req: axum::http::Request<Body>| async move {
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
                    let service = hyper_util::service::TowerToHyperService::new(app.into_service());

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
