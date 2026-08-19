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
pub mod agent;
pub mod clients;
pub mod dns;
pub mod dto;
pub mod llm;
pub mod login;
pub mod logs;
pub mod mesh;
pub mod preferences;
#[cfg(feature = "rag")]
pub mod rag;
pub mod reverse_proxy;
pub mod server_auth;
pub mod settings;
pub mod shadowsocks;
pub mod static_files;
pub mod stats;
pub mod trojan;

pub use dto::*;

use crate::auth::{auth_middleware, AuthConfig};
use crate::control::ServerState;

#[cfg(test)]
mod tests {
    use super::stats;
    use super::*;
    use crate::db::Database;
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
            crate::stats::EntityType::Proxy,
            "rule1",
            100,
            200,
        );
        server_state
            .stats_collector
            .incr_conns(crate::stats::EntityType::Client, "home-nas");

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
    pub log_store: Option<crate::logs::LogStore>,
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
    #[allow(
        unused_mut,
        reason = "reassigned only by the rag-gated merge block below"
    )]
    let mut public_routes = Router::new()
        .route("/api/login", post(login::login))
        .route("/api/health", get(login::health))
        .route("/api/stats/stream", get(stats::sse_stats_stream))
        .route("/api/logs/stream", get(logs::sse_log_stream))
        .route("/api/agent/ws", get(agent::agent_ws))
        .route("/api/agent/notifications/ws", get(agent::notifications_ws))
        .route("/api/agent/terminal/ws", get(agent::terminal_ws))
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
            "/api/logs/llm-logging",
            get(logs::get_llm_logging).put(logs::put_llm_logging),
        )
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
        // LLM model groups (multi-model failover)
        .route(
            "/api/llm/model-groups",
            get(llm::list_model_groups).post(llm::create_model_group),
        )
        .route(
            "/api/llm/model-groups/:id",
            get(llm::get_model_group)
                .put(llm::update_model_group)
                .delete(llm::delete_model_group),
        )
        .route(
            "/api/llm/model-groups/:id/members",
            put(llm::replace_group_members),
        )
        .route(
            "/api/llm/model-groups/:id/reset-breaker",
            post(llm::reset_group_breaker),
        )
        // Agent workbench endpoints (WebSocket is in public_routes — uses ?token= query param)
        .route(
            "/api/agent/workspaces",
            get(agent::list_workspaces).post(agent::create_workspace),
        )
        .route(
            "/api/agent/workspaces/:id",
            get(agent::get_workspace)
                .put(agent::update_workspace)
                .delete(agent::delete_workspace),
        )
        .route(
            "/api/agent/workspaces/:id/files",
            get(agent::list_workspace_files),
        )
        .route("/api/agent/workspaces/:id/fs/tree", get(agent::get_fs_tree))
        .route(
            "/api/agent/workspaces/:id/fs/file",
            get(agent::get_fs_file).put(agent::put_fs_file),
        )
        .route(
            "/api/agent/workspaces/:id/git/status",
            get(agent::get_git_status),
        )
        .route(
            "/api/agent/workspaces/:id/git/diff",
            get(agent::get_git_diff),
        )
        .route(
            "/api/agent/workspaces/:id/git/branches",
            get(agent::get_git_branches),
        )
        .route(
            "/api/agent/workspaces/:id/git/log",
            get(agent::get_git_log),
        )
        .route(
            "/api/agent/workspaces/:id/git/show",
            get(agent::get_git_show),
        )
        .route(
            "/api/agent/workspaces/:id/git/stash",
            get(agent::get_git_stash).post(agent::post_git_stash_push),
        )
        .route(
            "/api/agent/workspaces/:id/git/stage",
            post(agent::post_git_stage),
        )
        .route(
            "/api/agent/workspaces/:id/git/unstage",
            post(agent::post_git_unstage),
        )
        .route(
            "/api/agent/workspaces/:id/git/commit",
            post(agent::post_git_commit),
        )
        .route(
            "/api/agent/workspaces/:id/git/checkout",
            post(agent::post_git_checkout),
        )
        .route(
            "/api/agent/workspaces/:id/git/branch/delete",
            post(agent::post_git_branch_delete),
        )
        .route(
            "/api/agent/workspaces/:id/git/pull",
            post(agent::post_git_pull),
        )
        .route(
            "/api/agent/workspaces/:id/git/push",
            post(agent::post_git_push),
        )
        .route(
            "/api/agent/workspaces/:id/git/revert",
            post(agent::post_git_revert),
        )
        .route(
            "/api/agent/workspaces/:id/git/reset",
            post(agent::post_git_reset),
        )
        .route(
            "/api/agent/workspaces/:id/git/stash/apply",
            post(agent::post_git_stash_apply),
        )
        .route(
            "/api/agent/workspaces/:id/git/stash/pop",
            post(agent::post_git_stash_pop),
        )
        .route(
            "/api/agent/workspaces/:id/git/stash/drop",
            post(agent::post_git_stash_drop),
        )
        .route(
            "/api/agent/workspaces/:id/github/repo",
            get(agent::get_repo_info),
        )
        .route(
            "/api/agent/workspaces/:id/github/workflows",
            get(agent::list_workflows),
        )
        .route(
            "/api/agent/workspaces/:id/github/runs",
            get(agent::list_workflow_runs),
        )
        .route(
            "/api/agent/workspaces/:id/github/runs/:run_id/jobs",
            get(agent::list_run_jobs),
        )
        .route(
            "/api/agent/workspaces/:id/github/jobs/:job_id/logs",
            get(agent::get_job_logs),
        )
        .route(
            "/api/agent/workspaces/:id/github/workflows/:workflow_id/dispatch",
            post(agent::dispatch_workflow),
        )
        .route(
            "/api/agent/workspaces/:id/github/runs/:run_id/rerun",
            post(agent::rerun_workflow),
        )
        .route(
            "/api/agent/workspaces/:id/github/runs/:run_id/cancel",
            post(agent::cancel_run),
        )
        .route(
            "/api/agent/workspaces/:id/sessions",
            get(agent::list_sessions).post(agent::create_session),
        )
        .route(
            "/api/agent/sessions/:id",
            put(agent::update_session).delete(agent::delete_session),
        )
        .route(
            "/api/agent/sessions/:id/model",
            axum::routing::patch(agent::update_session_model),
        )
        .route(
            "/api/agent/sessions/:id/role",
            axum::routing::patch(agent::update_session_role),
        )
        .route(
            "/api/agent/sessions/:id/archive",
            post(agent::archive_session),
        )
        .route(
            "/api/agent/sessions/:id/messages",
            get(agent::list_messages),
        )
        .route(
            "/api/agent/default-model",
            get(agent::get_default_model).put(agent::put_default_model),
        )
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

    // Agent roles CRUD 路由（无 rag feature 依赖，无条件编译）。
    protected_routes = protected_routes.merge(agent::roles::protected_router());

    // RAG 知识库路由（仅 rag feature 启用时挂载）。必须在 auth middleware 应用
    // 之前 merge 进 protected_routes——axum 的 `.layer()` 只包裹调用时已存在的
    // 路由，之后 merge 进来的路由不会被包裹（否则 /api/llm/kb* 会失去 JWT 保护）。
    #[cfg(feature = "rag")]
    {
        public_routes = public_routes.merge(rag::public_router());
        protected_routes = protected_routes.merge(rag::protected_router());
        // AI 记忆体管理路由（settings / memories CRUD / clear / 手动蒸馏 / SSE）
        protected_routes = protected_routes.merge(agent::memory::protected_router());
        public_routes = public_routes.merge(agent::memory::public_router());
        // Skill 库管理路由（skills CRUD + toggle）
        protected_routes = protected_routes.merge(agent::skills::protected_router());
    }

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
        .route("/*path", get(static_files::serve_static))
        // 前端静态资源 gzip/brotli 压缩（JS/CSS 体积大，压缩收益显著）
        .layer(tower_http::compression::CompressionLayer::new());

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
