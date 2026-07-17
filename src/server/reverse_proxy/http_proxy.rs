use super::handler::{handle_proxy_request_unified, RouteSource};
use super::router::RouteTable;
use super::upstream::UpstreamClient;
use super::ReverseProxyState;
use crate::server::db::Database;
use arc_swap::ArcSwap;
use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

/// HTTP reverse proxy handler
pub struct HttpProxy {
    state: ReverseProxyState,
    #[allow(dead_code)]
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
        // Look up the rule and snapshot needed pieces atomically.
        let (rule_snapshot, tls_config, fallback_domain) = {
            let rules = self.state.rules.lock().await;
            let rule = rules.get(&rule_id).cloned();
            let tls = rule
                .as_ref()
                .and_then(|r| r.tls.clone())
                .filter(|t| t.enabled);
            let fallback = rule.as_ref().and_then(|r| r.domains.first().cloned());
            (rule, tls, fallback)
        };

        let Some(rule) = rule_snapshot else {
            return Err(format!("rule {rule_id} not found in state").into());
        };

        // Build a single-rule RouteTable snapshot for this listener.
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());

        let app = Router::new()
            .fallback(any(handle_proxy_request_unified))
            .with_state((source, upstream));

        let listener = TcpListener::bind(addr).await?;

        if let Some(tls_cfg) = tls_config {
            // TLS-enabled path
            let domain = tls_cfg.domain.clone().or(fallback_domain);

            let domain = match domain {
                Some(d) => d,
                None => {
                    error!(
                        "TLS enabled for rule {} but no domain configured, falling back to plain HTTP",
                        rule_id
                    );
                    // Fall back to plain HTTP
                    return self.start_plain(listener, app, rule_id).await;
                }
            };

            // Get TLS server config from certificate provider
            let server_config = match self.state.cert_provider() {
                Some(provider) => match provider.get_tls_server_config(&domain).await {
                    Some(config) => config,
                    None => {
                        error!(
                            "No certificate found for domain '{}' on rule {}, falling back to plain HTTP",
                            domain, rule_id
                        );
                        return self.start_plain(listener, app, rule_id).await;
                    }
                },
                None => {
                    warn!(
                        "TLS enabled for rule {} but no certificate provider configured, falling back to plain HTTP",
                        rule_id
                    );
                    return self.start_plain(listener, app, rule_id).await;
                }
            };

            info!(
                "HTTP proxy listening on {} with TLS for domain '{}'",
                addr, domain
            );

            let tls_acceptor = TlsAcceptor::from(server_config);

            let handle = tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((tcp_stream, _peer_addr)) => {
                            let tls_acceptor = tls_acceptor.clone();
                            let app = app.clone();

                            tokio::spawn(async move {
                                let tls_stream = match tls_acceptor.accept(tcp_stream).await {
                                    Ok(s) => s,
                                    Err(e) => {
                                        debug!("TLS handshake failed: {}", e);
                                        return;
                                    }
                                };

                                let io = hyper_util::rt::TokioIo::new(tls_stream);
                                let service = hyper_util::service::TowerToHyperService::new(
                                    app.into_service(),
                                );

                                if let Err(e) = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await
                                {
                                    debug!("HTTPS connection error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Failed to accept connection: {}", e);
                        }
                    }
                }
            });

            let mut listeners = self.state.tcp_listeners.lock().await;
            listeners.insert(rule_id, handle);
        } else {
            // Plain HTTP path (existing behavior)
            return self.start_plain(listener, app, rule_id).await;
        }

        Ok(())
    }

    /// Start plain HTTP listener (no TLS)
    async fn start_plain(
        &self,
        listener: TcpListener,
        app: Router,
        rule_id: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!("HTTP proxy error: {}", e);
            }
        });

        let mut listeners = self.state.tcp_listeners.lock().await;
        listeners.insert(rule_id, handle);

        Ok(())
    }
}

/// Axum handler used by the shared listener.
/// State carries a hot-swappable `RouteTable`; each request loads a snapshot.
pub async fn handle_proxy_request_shared(
    State(table): State<Arc<ArcSwap<RouteTable>>>,
    req: Request<Body>,
) -> impl IntoResponse {
    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.split(':').next().unwrap_or(s).to_string())
        .unwrap_or_default();
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    let table_snap = table.load();
    let matched = table_snap.match_http_request(&host, &path).await;

    let backend_addr = match matched {
        Some((_, _, backend)) => backend.addr.clone(),
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from(format!("No route for host '{host}'")))
                .unwrap();
        }
    };

    let upstream_uri = format!("http://{backend_addr}{path}");
    let client = reqwest::Client::new();
    let mut upstream_req = client.request(method, &upstream_uri);

    for (key, value) in req.headers() {
        if key != "host" {
            upstream_req = upstream_req.header(key, value);
        }
    }
    upstream_req = upstream_req.header("X-Forwarded-For", "127.0.0.1");
    upstream_req = upstream_req.header("X-Real-IP", "127.0.0.1");

    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();

    match upstream_req.body(body_bytes.to_vec()).send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut proxy_response = Response::builder().status(status);
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
                .body(Body::from(format!("Backend error: {e}")))
                .unwrap()
        }
    }
}
