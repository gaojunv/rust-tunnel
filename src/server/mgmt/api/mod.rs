use axum::{
    body::Body,
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, patch, post, put},
    Router,
};

#[cfg(test)]
use axum::extract::State;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

pub mod acme;
pub mod clients;
pub mod dns;
pub mod dto;
pub mod llm;
pub mod login;
pub mod logs;
pub mod mesh;
pub mod preferences;
pub mod reverse_proxy;
pub mod server_auth;
pub mod settings;
pub mod shadowsocks;
pub mod static_files;
pub mod stats;
pub mod trojan;

pub use dto::*;

use crate::server::auth::{auth_middleware, AuthConfig};
use crate::server::control::ServerState;

#[cfg(test)]
mod tests {
    use super::stats;
    use super::*;
    use crate::server::db::Database;
    use chrono::Timelike;

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
        .route("/api/logs/stream", get(logs::sse_log_stream))
        .route("/api/preferences", get(preferences::get_preferences));

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
        .route(
            "/api/logs/level",
            get(logs::get_logs_level).put(logs::put_logs_level),
        )
        // Proxy rules management endpoints
        .route(
            "/api/proxy/rules",
            get(reverse_proxy::list_proxy_rules).post(reverse_proxy::create_proxy_rule),
        )
        .route(
            "/api/proxy/rules/:id",
            put(reverse_proxy::update_proxy_rule).delete(reverse_proxy::delete_proxy_rule),
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
        // LLM Gateway management endpoints
        .route(
            "/api/llm/gateway",
            get(llm::get_gateway_config).put(llm::update_gateway_config),
        )
        .route(
            "/api/llm/providers",
            get(llm::list_providers).post(llm::create_provider),
        )
        .route(
            "/api/llm/providers/:id",
            put(llm::update_provider)
                .patch(llm::toggle_provider)
                .delete(llm::delete_provider),
        )
        .route(
            "/api/llm/providers/:provider_id/models",
            get(llm::list_provider_models).post(llm::add_model),
        )
        .route("/api/llm/models", get(llm::list_all_models))
        .route(
            "/api/llm/models/:id",
            put(llm::update_model).delete(llm::delete_model),
        )
        .route(
            "/api/llm/api-keys",
            get(llm::list_api_keys).post(llm::create_api_key),
        )
        .route(
            "/api/llm/api-keys/:id",
            patch(llm::toggle_api_key).delete(llm::delete_api_key),
        )
        // LLM usage statistics
        .route("/api/llm/usage/summary", get(llm::get_usage_summary))
        .route("/api/llm/usage/aggregate", get(llm::get_usage_aggregate))
        .route("/api/llm/usage/logs", get(llm::get_usage_logs))
        // User preferences
        .route("/api/preferences", put(preferences::put_preferences))
        // Settings endpoints
        .route("/api/settings", get(settings::get_settings))
        .route(
            "/api/settings/reverse-proxy",
            get(reverse_proxy::get_reverse_proxy_config)
                .put(reverse_proxy::update_reverse_proxy_config),
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
    use axum::extract::Path;
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
