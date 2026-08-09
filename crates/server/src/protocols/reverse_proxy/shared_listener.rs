//! Shared HTTP listener: multiple ProxyRules share one listen port,
//! dispatched by Host header (and by SNI for TLS-terminated listeners).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::{
    body::Body,
    extract::State,
    http::{Method, Request, StatusCode},
    response::Response,
    routing::any,
    Json, Router,
};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use super::error::ReconcileError;
use super::handler::{handle_proxy_request_unified, ProxyState, RouteSource};
use super::router::RouteTable;
use super::sni_sniff;
use super::upstream::UpstreamClient;
use super::{ProxyRule, ReverseProxyState, RuleType, TrojanSniEntry};
use crate::acme::CertificateManager;

pub struct SharedListener {
    pub listen_addr: String,
    pub tls_enabled: bool,
    pub route_table: Arc<ArcSwap<RouteTable>>,
    pub active_rule_ids: HashSet<String>,
    handle: JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
}

impl SharedListener {
    /// Spawn a new shared listener bound to `listen_addr`.
    pub async fn spawn(
        listen_addr: String,
        tls_enabled: bool,
        initial_table: RouteTable,
        cert_manager: Option<Arc<CertificateManager>>,
        active_rule_ids: HashSet<String>,
        proxy_state: Arc<ReverseProxyState>,
    ) -> Result<Self, ReconcileError> {
        if tls_enabled && cert_manager.is_none() {
            return Err(ReconcileError::NoCertManager {
                listen_addr: listen_addr.clone(),
            });
        }

        let addr: SocketAddr = listen_addr.parse().map_err(|e: std::net::AddrParseError| {
            ReconcileError::BindFailed {
                listen_addr: listen_addr.clone(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, e),
            }
        })?;
        let listener =
            TcpListener::bind(addr)
                .await
                .map_err(|source| ReconcileError::BindFailed {
                    listen_addr: listen_addr.clone(),
                    source,
                })?;

        let route_table = Arc::new(ArcSwap::from_pointee(initial_table));
        let upstream_for_task = Arc::new(UpstreamClient::new());
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        let tls_acceptor = if tls_enabled {
            let mgr = cert_manager.expect("checked above");
            let resolver = mgr.sni_resolver();
            let mut cfg = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(resolver);
            cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            Some(TlsAcceptor::from(Arc::new(cfg)))
        } else {
            None
        };

        let route_table_for_task = route_table.clone();
        let listen_addr_for_log = listen_addr.clone();
        let listen_addr_for_match = listen_addr.clone();

        let handle = tokio::spawn(async move {
            info!(
                "shared listener up: {} (tls={})",
                listen_addr_for_log, tls_enabled
            );
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() {
                            info!("shared listener shutting down (sender dropped): {}", listen_addr_for_log);
                            break;
                        }
                        if *shutdown_rx.borrow() {
                            info!("shared listener shutting down: {}", listen_addr_for_log);
                            break;
                        }
                    }
                    accept = listener.accept() => {
                        let (stream, peer) = match accept {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("accept error on {}: {}", listen_addr_for_log, e);
                                continue;
                            }
                        };
                        let table = route_table_for_task.clone();
                        let acceptor = tls_acceptor.clone();
                        let upstream_c = upstream_for_task.clone();
                        let ps = proxy_state.clone();
                        // 每连接取一次分流表项快照（ArcSwap 热替换，无需重建监听器）
                        let trojan_entry = proxy_state.trojan_sni_entry(&listen_addr_for_match);
                        tokio::spawn(async move {
                            handle_one_connection(stream, peer, acceptor, table, upstream_c, ps, trojan_entry)
                                .await;
                        });
                    }
                }
            }
        });

        Ok(SharedListener {
            listen_addr,
            tls_enabled,
            route_table,
            active_rule_ids,
            handle,
            shutdown_tx,
        })
    }

    /// Signal the listener task to stop. Non-blocking.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Send shutdown, then await the accept task so the `TcpListener` inside
    /// it is dropped and the OS socket is fully released.
    pub async fn stop_and_wait(self, timeout: std::time::Duration) {
        let SharedListener {
            handle,
            shutdown_tx,
            listen_addr,
            ..
        } = self;
        let abort_handle = handle.abort_handle();
        let _ = shutdown_tx.send(true);
        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(join_err)) => {
                warn!(
                    "shared listener {} task panicked while stopping: {}",
                    listen_addr, join_err
                );
            }
            Err(_) => {
                warn!(
                    "shared listener {} did not exit within {:?}; aborting",
                    listen_addr, timeout
                );
                abort_handle.abort();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_one_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    route_table: Arc<ArcSwap<RouteTable>>,
    upstream: Arc<UpstreamClient>,
    proxy_state: Arc<ReverseProxyState>,
    trojan_entry: Option<Arc<TrojanSniEntry>>,
) {
    // Trojan SNI 分流：仅在 TLS 监听器上嗅探。命中 trojan 域名时本监听器
    // 不终止 TLS，直接交给 Trojan 处理（Trojan 自己终止 TLS）；
    // 解析失败/非 TLS/未命中一律走原有 HTTP 路径。
    if let (Some(entry), Some(_)) = (&trojan_entry, &acceptor) {
        if let Some(sni) = sni_sniff::sniff_sni(&stream).await {
            if sni == entry.domain {
                debug!(peer = %peer, sni = %sni, "SNI 命中 trojan 域名，分流给 Trojan");
                crate::listener::handle_trojan_connection(
                    stream,
                    peer,
                    entry.trojan_port,
                    entry.password.clone(),
                    entry.fallback.clone(),
                    entry.tls_config_rx.clone(),
                    entry.state.clone(),
                )
                .await;
                return;
            }
        }
    }

    let source = RouteSource(route_table);
    let app: Router = Router::new()
        .fallback(any(llm_aware_proxy_dispatch))
        .with_state((source, upstream, proxy_state));

    match acceptor {
        Some(acc) => {
            let tls_stream = match acc.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    debug!("TLS handshake failed: {}", e);
                    return;
                }
            };
            let alpn = tls_stream
                .get_ref()
                .1
                .alpn_protocol()
                .map(|p| String::from_utf8_lossy(p).into_owned());
            debug!(peer = %peer, alpn = ?alpn, "tls handshake ok");

            let io = hyper_util::rt::TokioIo::new(tls_stream);
            let service = hyper_util::service::TowerToHyperService::new(app.into_service());
            if let Err(e) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(io, service)
                    .await
            {
                debug!("HTTPS connection error: {}", e);
            }
        }
        None => {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper_util::service::TowerToHyperService::new(app.into_service());
            if let Err(e) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(io, service)
                    .await
            {
                debug!("HTTP connection error: {}", e);
            }
        }
    }
}

/// LLM-aware fallback：请求 Host 匹配已启用的 LLM Gateway 域名时，一律交给
/// LLM handler（未识别的路径由网关返回 OpenAI 风格 404）；其余请求走普通反代。
/// 这样 LLM 规则与普通 HTTP 规则可以共存于同一监听端口，互不抢占。
async fn llm_aware_proxy_dispatch(
    State((source, upstream, proxy_state)): State<ProxyState>,
    req: Request<Body>,
) -> Response {
    // 在任何 await 之前同步提取判定所需的数据：
    // `&Request<Body>` 不是 Send（Body 不 Sync），不能跨 await 持有。
    // h2 的 authority 在 URI 中；h1 可能在 URI（absolute-form）或 Host 头。
    let host = req
        .uri()
        .host()
        .map(str::to_string)
        .or_else(|| {
            req.headers()
                .get("host")
                .and_then(|h| h.to_str().ok())
                .map(|s| s.split(':').next().unwrap_or(s).to_string())
        })
        .unwrap_or_default();

    if let Some((llm, protocol)) = match_llm_gateway(&proxy_state, &host).await {
        return llm_handle(llm, protocol, req).await;
    }
    handle_proxy_request_unified(State((source, upstream, proxy_state)), req).await
}

/// 判断指向 `host` 的请求是否应由 LLM Gateway 处理；是则返回对应的 `LlmState` 和命中的协议。
async fn match_llm_gateway(
    proxy_state: &Arc<ReverseProxyState>,
    host: &str,
) -> Option<(
    Arc<crate::llm::LlmState>,
    crate::llm::LlmProtocol,
)> {
    let llm_guard = proxy_state.llm_state.read().await;
    let llm = llm_guard.as_ref()?;
    let cfg = llm.gateway_config.read().await;
    let cfg = cfg.as_ref()?;
    cfg.match_protocol(host).map(|proto| (llm.clone(), proto))
}

/// 把已匹配 LLM Gateway 的请求分发给对应的 handler。
/// 按命中的协议入口严格限制接受的路径，跨协议路径返回协议各自的 404 风格。
async fn llm_handle(
    llm: Arc<crate::llm::LlmState>,
    protocol: crate::llm::LlmProtocol,
    req: Request<Body>,
) -> Response {
    use crate::llm::LlmProtocol;
    use crate::llm::{anthropic_handler, openai_handler, upstream};

    let state = openai_handler::LlmHandlerState {
        llm,
        protocol: Some(protocol),
    };
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let is_models = method == Method::GET && path == "/v1/models";
    let is_messages = method == Method::POST && path == "/v1/messages";
    let is_chat_completions = method == Method::POST && path == "/v1/chat/completions";
    let (parts, body) = req.into_parts();
    let mut headers = parts.headers;

    // HTTP/2 请求没有 Host 头，authority 由 hyper 映射到 URI；
    // 下游 handler 的 validate_host 只认 Host 头，这里按 h1 语义补齐。
    if !headers.contains_key(axum::http::header::HOST) {
        if let Some(authority) = parts.uri.authority() {
            if let Ok(value) = axum::http::HeaderValue::from_str(authority.as_str()) {
                headers.insert(axum::http::header::HOST, value);
            }
        }
    }

    // 严格协议隔离：按入口协议判定允许的路径
    let allowed = match protocol {
        LlmProtocol::OpenAI => is_models || is_chat_completions,
        LlmProtocol::Anthropic => is_models || is_messages,
    };

    if !allowed {
        let etype = "invalid_request_error";
        let msg = "Not found".to_string();
        return match protocol {
            LlmProtocol::OpenAI => upstream::error_response(StatusCode::NOT_FOUND, msg, etype),
            LlmProtocol::Anthropic => {
                upstream::error_response_anthropic(StatusCode::NOT_FOUND, msg, etype)
            }
        };
    }

    if is_models {
        return openai_handler::handle_list_models(State(state), headers).await;
    }

    let bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("failed to read request body: {}", e);
            return match protocol {
                LlmProtocol::OpenAI => {
                    upstream::error_response(StatusCode::BAD_REQUEST, msg, "invalid_request_error")
                }
                LlmProtocol::Anthropic => upstream::error_response_anthropic(
                    StatusCode::BAD_REQUEST,
                    msg,
                    "invalid_request_error",
                ),
            };
        }
    };
    if std::str::from_utf8(&bytes).is_err() {
        let msg = "request body is not valid UTF-8; JSON must be UTF-8 encoded (inline non-ASCII text in terminals like Windows cmd is often not UTF-8 — use \\uXXXX escapes or a UTF-8 file)".into();
        return match protocol {
            LlmProtocol::OpenAI => {
                upstream::error_response(StatusCode::BAD_REQUEST, msg, "invalid_request_error")
            }
            LlmProtocol::Anthropic => upstream::error_response_anthropic(
                StatusCode::BAD_REQUEST,
                msg,
                "invalid_request_error",
            ),
        };
    }
    let json: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("invalid JSON body: {}", e);
            return match protocol {
                LlmProtocol::OpenAI => {
                    upstream::error_response(StatusCode::BAD_REQUEST, msg, "invalid_request_error")
                }
                LlmProtocol::Anthropic => upstream::error_response_anthropic(
                    StatusCode::BAD_REQUEST,
                    msg,
                    "invalid_request_error",
                ),
            };
        }
    };

    if is_messages {
        anthropic_handler::handle_messages(State(state), headers, Json(json)).await
    } else {
        openai_handler::handle_chat_completions(State(state), headers, Json(json)).await
    }
}

/// Validate the set of enabled HTTP rules on one port.
///
/// - Domain uniqueness: no two rules on the same port may claim the same domain string.
/// - TLS consistency: all rules on the same port must have identical `tls.enabled`.
///
/// The `listen_addr` parameter is only used for error reporting.
pub fn validate_rules_for_port(
    listen_addr: &str,
    rules: &[ProxyRule],
) -> Result<bool /* tls_enabled */, ReconcileError> {
    let mut tls_state: Option<bool> = None;
    let mut seen_domains: HashMap<String, String> = HashMap::new();

    for rule in rules {
        let this_tls = rule.tls.as_ref().is_some_and(|t| t.enabled);
        match tls_state {
            None => tls_state = Some(this_tls),
            Some(prev) if prev != this_tls => {
                return Err(ReconcileError::TlsMismatch {
                    listen_addr: listen_addr.to_string(),
                    existing_tls: prev,
                    new_tls: this_tls,
                });
            }
            _ => {}
        }
        for domain in &rule.domains {
            if let Some(other_id) = seen_domains.insert(domain.clone(), rule.id.clone()) {
                if other_id != rule.id {
                    return Err(ReconcileError::DomainConflict {
                        listen_addr: listen_addr.to_string(),
                        domain: domain.clone(),
                        other_rule_id: other_id,
                    });
                }
            }
        }
    }

    Ok(tls_state.unwrap_or(false))
}

impl ReverseProxyState {
    /// Acquire the per-port reconcile lock.
    async fn acquire_reconcile_lock(&self, listen_addr: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let m = {
            let mut map = self.reconcile_locks.lock().unwrap();
            map.entry(listen_addr.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        m.lock_owned().await
    }

    /// Reconcile the shared HTTP listener for `listen_addr` with the current
    /// set of enabled HTTP rules. Idempotent.
    pub async fn reconcile_http_listener(&self, listen_addr: &str) -> Result<(), ReconcileError> {
        let _guard = self.acquire_reconcile_lock(listen_addr).await;

        let rules_snapshot: Vec<ProxyRule> = {
            let rules = self.rules.lock().await;
            rules
                .values()
                .filter(|r| {
                    r.enabled
                        && (r.rule_type == RuleType::Http || r.rule_type == RuleType::Llm)
                        && r.listen == listen_addr
                })
                .cloned()
                .collect()
        };

        if rules_snapshot.is_empty() {
            let removed = {
                let mut listeners = self.shared_listeners.lock().await;
                listeners.remove(listen_addr)
            };
            if let Some(l) = removed {
                l.stop_and_wait(std::time::Duration::from_secs(5)).await;
            }
            return Ok(());
        }

        let tls_enabled = validate_rules_for_port(listen_addr, &rules_snapshot)?;
        let active_rule_ids: HashSet<String> =
            rules_snapshot.iter().map(|r| r.id.clone()).collect();
        let new_table = RouteTable::from_rules(rules_snapshot.clone());

        // Fast path: existing listener with matching TLS mode → hot-swap.
        {
            let mut listeners = self.shared_listeners.lock().await;
            if let Some(existing) = listeners.get_mut(listen_addr) {
                if existing.tls_enabled == tls_enabled {
                    existing.route_table.store(Arc::new(new_table));
                    existing.active_rule_ids = active_rule_ids;
                    return Ok(());
                }
            }
        }

        // Slow path: rebuild listener (TLS toggle).
        // Step A: remove old
        let old = {
            let mut listeners = self.shared_listeners.lock().await;
            listeners.remove(listen_addr)
        };
        // Step B: stop old first to release port
        if let Some(old_listener) = old {
            old_listener
                .stop_and_wait(std::time::Duration::from_secs(5))
                .await;
        }
        // Step C: spawn new
        let rp_state = Arc::new(self.clone());
        let new = match SharedListener::spawn(
            listen_addr.to_string(),
            tls_enabled,
            new_table,
            self.cert_manager().cloned(),
            active_rule_ids,
            rp_state,
        )
        .await
        {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(
                    "Failed to rebuild shared listener on {} after TLS toggle: {}. \
                     Previous listener has already been stopped; port is temporarily offline.",
                    listen_addr,
                    e
                );
                return Err(e);
            }
        };
        {
            let mut listeners = self.shared_listeners.lock().await;
            listeners.insert(listen_addr.to_string(), new);
        }
        Ok(())
    }

    /// Return all distinct HTTP listen addresses currently in the rules map.
    pub async fn distinct_http_listen_addrs(&self) -> Vec<String> {
        let rules = self.rules.lock().await;
        let mut set: HashSet<String> = HashSet::new();
        for r in rules.values() {
            if r.rule_type == RuleType::Http || r.rule_type == RuleType::Llm {
                set.insert(r.listen.clone());
            }
        }
        set.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reverse_proxy::{
        Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule,
        ProxyTlsConfig, Route, RuleType,
    };

    fn http_rule(id: &str, listen: &str, domain: &str, tls: bool) -> ProxyRule {
        ProxyRule {
            id: id.into(),
            name: id.into(),
            rule_type: RuleType::Http,
            listen: listen.into(),
            domains: vec![domain.into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: "127.0.0.1:8080".into(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::RoundRobin,
            }],
            tls: if tls {
                Some(ProxyTlsConfig {
                    enabled: true,
                    acme: false,
                    domain: Some(domain.into()),
                })
            } else {
                None
            },
            enabled: true,
            created_at: None,
            cert_status: None,
        }
    }

    #[tokio::test]
    async fn validate_no_conflicts_returns_ok() {
        let rules = vec![
            http_rule("r1", "0.0.0.0:8081", "a.example.com", false),
            http_rule("r2", "0.0.0.0:8081", "b.example.com", false),
        ];
        assert!(validate_rules_for_port("0.0.0.0:8081", &rules).is_ok());
    }

    #[tokio::test]
    async fn validate_domain_conflict() {
        let rules = vec![
            http_rule("r1", "0.0.0.0:8081", "a.example.com", false),
            http_rule("r2", "0.0.0.0:8081", "a.example.com", false),
        ];
        let err = validate_rules_for_port("0.0.0.0:8081", &rules).unwrap_err();
        assert!(matches!(err, ReconcileError::DomainConflict { .. }));
    }

    #[tokio::test]
    async fn validate_tls_mismatch() {
        let rules = vec![
            http_rule("r1", "0.0.0.0:8081", "a.example.com", true),
            http_rule("r2", "0.0.0.0:8081", "b.example.com", false),
        ];
        let err = validate_rules_for_port("0.0.0.0:8081", &rules).unwrap_err();
        assert!(matches!(err, ReconcileError::TlsMismatch { .. }));
    }

    #[tokio::test]
    async fn reconcile_empty_removes_listener() {
        let state = ReverseProxyState::new();
        // 无规则、无 listener — 只应 no-op 返回 Ok。
        state.reconcile_http_listener("127.0.0.1:0").await.unwrap();
        assert!(state.shared_listeners.lock().await.is_empty());
    }

    /// End-to-end: two HTTP rules on the same port dispatch to different mock backends.
    #[tokio::test]
    async fn e2e_two_rules_same_port_dispatched_by_host() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Mock backend A: replies "A\n"
        let backend_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = backend_a.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = backend_a.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nA\n")
                        .await;
                });
            }
        });

        // Mock backend B: replies "B\n"
        let backend_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b_addr = backend_b.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = backend_b.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nB\n")
                        .await;
                });
            }
        });

        // Pick a free listen port
        let listen_probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_port = listen_probe.local_addr().unwrap().port();
        drop(listen_probe);
        let listen_addr = format!("127.0.0.1:{listen_port}");

        let state = ReverseProxyState::new();
        let mk = |id: &str, host: &str, backend: SocketAddr| ProxyRule {
            id: id.into(),
            name: id.into(),
            rule_type: RuleType::Http,
            listen: listen_addr.clone(),
            domains: vec![host.into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: backend.to_string(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::RoundRobin,
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        state
            .rules
            .lock()
            .await
            .insert("a".into(), mk("a", "a.local", a_addr));
        state
            .rules
            .lock()
            .await
            .insert("b".into(), mk("b", "b.local", b_addr));
        state.reconcile_http_listener(&listen_addr).await.unwrap();

        // Small delay to let the listener spawn
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Client hits port, alternating Host header
        let url_a = format!("http://{listen_addr}/x");
        let url_b = format!("http://{listen_addr}/x");
        let client = reqwest::Client::new();
        let resp_a = client
            .get(&url_a)
            .header("Host", "a.local")
            .send()
            .await
            .unwrap();
        assert!(resp_a.text().await.unwrap().starts_with('A'));
        let resp_b = client
            .get(&url_b)
            .header("Host", "b.local")
            .send()
            .await
            .unwrap();
        assert!(resp_b.text().await.unwrap().starts_with('B'));

        // Cleanup: drop rules FIRST (release the lock) before reconcile,
        // otherwise reconcile_http_listener would deadlock trying to
        // re-acquire the rules mutex.
        {
            state.rules.lock().await.clear();
        }
        state.reconcile_http_listener(&listen_addr).await.unwrap();
    }

    fn llm_rule(id: &str, listen: &str, domain: &str, tls: bool) -> ProxyRule {
        ProxyRule {
            id: id.into(),
            name: id.into(),
            rule_type: RuleType::Llm,
            listen: listen.into(),
            domains: vec![domain.into()],
            routes: vec![],
            tls: if tls {
                Some(ProxyTlsConfig {
                    enabled: true,
                    acme: false,
                    domain: Some(domain.into()),
                })
            } else {
                None
            },
            enabled: true,
            created_at: None,
            cert_status: None,
        }
    }

    /// LLM gateway 启用后，同监听器上其他域名的 /v1/* 请求仍应走反代后端，
    /// 只有 LLM 域名自己的 /v1/* 请求才交给 LLM handler。
    #[tokio::test]
    async fn llm_gateway_does_not_hijack_other_domains() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Mock backend A: replies "A\n" to anything (including /v1/chat/completions)
        let backend_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = backend_a.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = backend_a.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nA\n")
                        .await;
                });
            }
        });

        let listen_probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_port = listen_probe.local_addr().unwrap().port();
        drop(listen_probe);
        let listen_addr = format!("127.0.0.1:{listen_port}");

        let state = ReverseProxyState::new();
        // HTTP rule: a.local → backend A
        let mut rule_a = http_rule("a", &listen_addr, "a.local", false);
        rule_a.routes[0].backends[0].addr = a_addr.to_string();
        state.rules.lock().await.insert("a".into(), rule_a);
        // LLM rule on the same port for llm.local
        state.rules.lock().await.insert(
            "__llm_gateway__".into(),
            llm_rule("__llm_gateway__", &listen_addr, "llm.local", false),
        );

        // Enable the LLM gateway for domain llm.local
        let llm = crate::llm::LlmState::new(None, None);
        *llm.gateway_config.write().await = Some(crate::llm::LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("llm.local".into()),
            anthropic_domain: None,
            listen: listen_addr.clone(),
            tls_enabled: false,
            tls_acme: false,
        });
        *state.llm_state.write().await = Some(Arc::new(llm));

        state.reconcile_http_listener(&listen_addr).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        // 1) Other domain's /v1/chat/completions must reach the proxy backend,
        //    not be intercepted by the LLM gateway.
        let resp = client
            .post(format!("http://{listen_addr}/v1/chat/completions"))
            .header("Host", "a.local")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(
            resp.text().await.unwrap().starts_with('A'),
            "request to a.local should hit proxy backend, not LLM gateway"
        );

        // 2) The LLM domain itself IS handled by the gateway
        //    (no API key configured → 401 from the LLM handler).
        let resp = client
            .post(format!("http://{listen_addr}/v1/chat/completions"))
            .header("Host", "llm.local")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // 3) Requests to OTHER domains are never intercepted by the gateway,
        //    even when the path resembles an LLM endpoint.
        let resp = client
            .post(format!("http://{listen_addr}/v1/models"))
            .header("Host", "a.local")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Cleanup
        {
            state.rules.lock().await.clear();
        }
        state.reconcile_http_listener(&listen_addr).await.unwrap();
    }

    /// HTTP/2 请求把 authority 放在 `:authority`（hyper 映射到 URI），不带 Host 头。
    /// 网关必须同样能识别 LLM 域名，而不是因为缺少 Host 头返回 404。
    #[tokio::test]
    async fn llm_gateway_accepts_h2_authority_without_host_header() {
        let state = ReverseProxyState::new();
        let llm = crate::llm::LlmState::new(None, None);
        *llm.gateway_config.write().await = Some(crate::llm::LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("llm.local".into()),
            anthropic_domain: None,
            listen: "127.0.0.1:1".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        *state.llm_state.write().await = Some(Arc::new(llm));

        let source = RouteSource(Arc::new(ArcSwap::from_pointee(RouteTable::from_rules(
            vec![],
        ))));
        let upstream = Arc::new(UpstreamClient::new());

        // h2 风格请求：authority 在 URI 中，没有 Host 头。
        // 未配置 API key，host 匹配成功时应返回 401 而不是 404。
        let req = Request::builder()
            .method(Method::GET)
            .uri("https://llm.local/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = llm_aware_proxy_dispatch(State((source, upstream, Arc::new(state))), req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Host 匹配 LLM 网关域名的请求全部交给网关处理；未识别的路径
    /// 返回 OpenAI 风格的 404 JSON，而不是反代的 "No route for host"。
    #[tokio::test]
    async fn llm_gateway_unknown_path_returns_openai_404() {
        let state = ReverseProxyState::new();
        let llm = crate::llm::LlmState::new(None, None);
        *llm.gateway_config.write().await = Some(crate::llm::LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("llm.local".into()),
            anthropic_domain: None,
            listen: "127.0.0.1:1".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        *state.llm_state.write().await = Some(Arc::new(llm));

        let source = RouteSource(Arc::new(ArcSwap::from_pointee(RouteTable::from_rules(
            vec![],
        ))));
        let upstream = Arc::new(UpstreamClient::new());

        let req = Request::builder()
            .method(Method::GET)
            .uri("https://llm.local/")
            .body(Body::empty())
            .unwrap();
        let resp = llm_aware_proxy_dispatch(State((source, upstream, Arc::new(state))), req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("\"error\""),
            "expected OpenAI-style error JSON, got: {text}"
        );
    }

    /// 非 UTF-8 的请求体（如 Windows cmd 内联中文被编成 GBK）应返回
    /// 明确指出编码问题的 400，而不是 serde_json 的 "invalid unicode code point"。
    #[tokio::test]
    async fn llm_gateway_non_utf8_body_gets_clear_error() {
        let state = ReverseProxyState::new();
        let llm = crate::llm::LlmState::new(None, None);
        *llm.gateway_config.write().await = Some(crate::llm::LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("llm.local".into()),
            anthropic_domain: None,
            listen: "127.0.0.1:1".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        *state.llm_state.write().await = Some(Arc::new(llm));

        let source = RouteSource(Arc::new(ArcSwap::from_pointee(RouteTable::from_rules(
            vec![],
        ))));
        let upstream = Arc::new(UpstreamClient::new());

        // "你好" 的 GBK 编码字节
        let body = b"{\"model\":\"x\",\"messages\":[{\"role\":\"user\",\"content\":\"\xc4\xe3\xba\xc3\"}]}";
        let req = Request::builder()
            .method(Method::POST)
            .uri("https://llm.local/v1/chat/completions")
            .body(Body::from(&body[..]))
            .unwrap();
        let resp = llm_aware_proxy_dispatch(State((source, upstream, Arc::new(state))), req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            text.contains("UTF-8"),
            "error message should mention UTF-8 encoding, got: {text}"
        );
    }

    #[tokio::test]
    async fn reconcile_llm_rule_starts_listener() {
        let state = ReverseProxyState::new();

        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let listen_addr = format!("127.0.0.1:{port}");
        let rule = llm_rule("llm1", &listen_addr, "llm.example.com", false);

        state.rules.lock().await.insert("llm1".into(), rule);
        state.reconcile_http_listener(&listen_addr).await.unwrap();
        assert!(state
            .shared_listeners
            .lock()
            .await
            .contains_key(&listen_addr));

        // Cleanup
        state.rules.lock().await.remove("llm1");
        state.reconcile_http_listener(&listen_addr).await.unwrap();
        assert!(!state
            .shared_listeners
            .lock()
            .await
            .contains_key(&listen_addr));
    }

    #[tokio::test]
    async fn distinct_listen_addrs_includes_llm_rules() {
        let state = ReverseProxyState::new();

        let http_rule = http_rule("h1", "0.0.0.0:8443", "api.example.com", false);
        let llm_rule = llm_rule("l1", "0.0.0.0:8444", "llm.example.com", false);

        state.rules.lock().await.insert("h1".into(), http_rule);
        state.rules.lock().await.insert("l1".into(), llm_rule);

        let addrs = state.distinct_http_listen_addrs().await;
        assert!(
            addrs.contains(&"0.0.0.0:8443".to_string()),
            "HTTP rule address should be present"
        );
        assert!(
            addrs.contains(&"0.0.0.0:8444".to_string()),
            "LLM rule address should be present"
        );
    }

    #[tokio::test]
    async fn reconcile_llm_and_http_on_same_port() {
        let state = ReverseProxyState::new();

        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let listen_addr = format!("127.0.0.1:{port}");

        let http_rule = http_rule("h1", &listen_addr, "api.example.com", false);
        let llm_rule = llm_rule("l1", &listen_addr, "llm.example.com", false);

        state.rules.lock().await.insert("h1".into(), http_rule);
        state.rules.lock().await.insert("l1".into(), llm_rule);

        // Both rules on same port with same TLS mode should work
        state.reconcile_http_listener(&listen_addr).await.unwrap();
        assert!(state
            .shared_listeners
            .lock()
            .await
            .contains_key(&listen_addr));

        // Cleanup
        state.rules.lock().await.clear();
        state.reconcile_http_listener(&listen_addr).await.unwrap();
    }

    #[tokio::test]
    async fn reconcile_starts_and_stops_listener() {
        let state = ReverseProxyState::new();

        // Discover a free port by binding :0.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let listen_addr = format!("127.0.0.1:{port}");
        let rule = http_rule("r1", &listen_addr, "a.example.com", false);

        state.rules.lock().await.insert("r1".into(), rule);
        state.reconcile_http_listener(&listen_addr).await.unwrap();
        assert!(state
            .shared_listeners
            .lock()
            .await
            .contains_key(&listen_addr));

        // Remove rule and reconcile again — listener should be removed.
        state.rules.lock().await.remove("r1");
        state.reconcile_http_listener(&listen_addr).await.unwrap();
        assert!(!state
            .shared_listeners
            .lock()
            .await
            .contains_key(&listen_addr));
    }

    /// openai 域名不收 Anthropic /v1/messages → 返回 OpenAI 风格 404。
    #[tokio::test]
    async fn llm_openai_domain_rejects_anthropic_messages() {
        let state = ReverseProxyState::new();
        let llm = crate::llm::LlmState::new(None, None);
        *llm.gateway_config.write().await = Some(crate::llm::LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("openai.local".into()),
            anthropic_domain: None,
            listen: "127.0.0.1:1".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        *state.llm_state.write().await = Some(Arc::new(llm));

        let source = RouteSource(Arc::new(ArcSwap::from_pointee(RouteTable::from_rules(
            vec![],
        ))));
        let upstream = Arc::new(UpstreamClient::new());

        let req = Request::builder()
            .method(Method::POST)
            .uri("https://openai.local/v1/messages")
            .body(Body::from("{}"))
            .unwrap();
        let resp = llm_aware_proxy_dispatch(State((source, upstream, Arc::new(state))), req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // OpenAI 风格 404：顶层有 error 字段
        assert!(
            v.get("error").is_some(),
            "expected OpenAI error format, got: {v}"
        );
    }

    /// anthropic 域名不收 OpenAI /v1/chat/completions → 返回 Anthropic 风格 404。
    #[tokio::test]
    async fn llm_anthropic_domain_rejects_openai_chat_completions() {
        let state = ReverseProxyState::new();
        let llm = crate::llm::LlmState::new(None, None);
        *llm.gateway_config.write().await = Some(crate::llm::LlmGatewayConfig {
            enabled: true,
            openai_domain: None,
            anthropic_domain: Some("anthropic.local".into()),
            listen: "127.0.0.1:1".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        *state.llm_state.write().await = Some(Arc::new(llm));

        let source = RouteSource(Arc::new(ArcSwap::from_pointee(RouteTable::from_rules(
            vec![],
        ))));
        let upstream = Arc::new(UpstreamClient::new());

        let req = Request::builder()
            .method(Method::POST)
            .uri("https://anthropic.local/v1/chat/completions")
            .body(Body::from("{}"))
            .unwrap();
        let resp = llm_aware_proxy_dispatch(State((source, upstream, Arc::new(state))), req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Anthropic 风格 404：顶层有 type: "error"
        assert_eq!(
            v["type"], "error",
            "expected Anthropic error format, got: {v}"
        );
    }

    /// 两个域名都配置时各走各的协议。
    #[tokio::test]
    async fn llm_dual_domain_both_routes_correctly() {
        let state = ReverseProxyState::new();
        let llm = crate::llm::LlmState::new(None, None);
        *llm.gateway_config.write().await = Some(crate::llm::LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("oa.local".into()),
            anthropic_domain: Some("an.local".into()),
            listen: "127.0.0.1:1".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        *state.llm_state.write().await = Some(Arc::new(llm));

        let source = RouteSource(Arc::new(ArcSwap::from_pointee(RouteTable::from_rules(
            vec![],
        ))));
        let upstream = Arc::new(UpstreamClient::new());

        // OpenAI 域名 → /v1/chat/completions 接受（无 API key → 401）
        let req = Request::builder()
            .method(Method::POST)
            .uri("https://oa.local/v1/chat/completions")
            .body(Body::from("{}"))
            .unwrap();
        let resp = llm_aware_proxy_dispatch(
            State((source.clone(), upstream.clone(), Arc::new(state.clone()))),
            req,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Anthropic 域名 → /v1/messages 接受（无 API key → 401）
        let req = Request::builder()
            .method(Method::POST)
            .uri("https://an.local/v1/messages")
            .body(Body::from("{}"))
            .unwrap();
        let resp = llm_aware_proxy_dispatch(State((source, upstream, Arc::new(state))), req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// 配置不存在的域名不命中 LLM gateway。
    #[tokio::test]
    async fn llm_unconfigured_domain_not_matched() {
        let state = ReverseProxyState::new();
        let llm = crate::llm::LlmState::new(None, None);
        *llm.gateway_config.write().await = Some(crate::llm::LlmGatewayConfig {
            enabled: true,
            openai_domain: Some("oa.local".into()),
            anthropic_domain: None,
            listen: "127.0.0.1:1".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        *state.llm_state.write().await = Some(Arc::new(llm));

        let source = RouteSource(Arc::new(ArcSwap::from_pointee(RouteTable::from_rules(
            vec![],
        ))));
        let upstream = Arc::new(UpstreamClient::new());

        // 未配 anthropic_domain → "/v1/messages" 也不会命中 LLM，走反代
        let req = Request::builder()
            .method(Method::POST)
            .uri("https://an.local/v1/messages")
            .body(Body::from("{}"))
            .unwrap();
        let resp = llm_aware_proxy_dispatch(State((source, upstream, Arc::new(state))), req).await;
        // 普通反代没有路由 → No route for host
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
mod alpn_tests {
    use rustls::server::ServerConfig;
    use std::sync::Arc;

    /// Sanity check: the ServerConfig built by CertificateManager for a domain
    /// has h2 and http/1.1 in its ALPN list. Full ALPN negotiation is
    /// exercised end-to-end by Task 2.6's E2E test.
    #[tokio::test]
    async fn manager_built_server_config_has_alpn() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(crate::acme::CertificateManager::new(
            temp_dir.path().to_str().unwrap(),
        ));

        // Generate a self-signed cert for "localhost".
        let (cert_pem, key_pem) = {
            use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
            let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
            let params = CertificateParams::new(vec!["localhost".into()]).unwrap();
            let cert = params.self_signed(&kp).unwrap();
            (cert.pem(), kp.serialize_pem())
        };
        mgr.add_certificate(
            "localhost",
            crate::acme::CertEntry {
                cert_pem,
                key_pem,
                chain_pem: None,
                expires_at: None,
                source: crate::acme::CertSource::Manual,
            },
        )
        .await
        .unwrap();

        let cfg_arc: Arc<ServerConfig> = {
            use crate::acme::CertificateProvider;
            mgr.get_tls_server_config("localhost").await.unwrap()
        };
        assert_eq!(
            cfg_arc.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }
}
