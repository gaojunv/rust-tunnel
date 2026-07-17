//! Shared HTTP listener: multiple ProxyRules share one listen port,
//! dispatched by Host header (and by SNI for TLS-terminated listeners).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::{routing::any, Router};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use super::error::ReconcileError;
use super::handler::{handle_proxy_request_unified, RouteSource};
use super::router::RouteTable;
use super::upstream::UpstreamClient;
use super::{ProxyRule, ReverseProxyState, RuleType};
use crate::server::acme::CertificateManager;

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
                        tokio::spawn(async move {
                            handle_one_connection(stream, peer, acceptor, table, upstream_c).await;
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

async fn handle_one_connection(
    stream: tokio::net::TcpStream,
    _peer: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    route_table: Arc<ArcSwap<RouteTable>>,
    upstream: Arc<UpstreamClient>,
) {
    let source = RouteSource(route_table);
    let app: Router = Router::new()
        .fallback(any(handle_proxy_request_unified))
        .with_state((source, upstream));

    match acceptor {
        Some(acc) => {
            let tls_stream = match acc.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    debug!("TLS handshake failed: {}", e);
                    return;
                }
            };
            let io = hyper_util::rt::TokioIo::new(tls_stream);
            let service = hyper_util::service::TowerToHyperService::new(app.into_service());
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                debug!("HTTPS connection error: {}", e);
            }
        }
        None => {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper_util::service::TowerToHyperService::new(app.into_service());
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                debug!("HTTP connection error: {}", e);
            }
        }
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
                .filter(|r| r.enabled && r.rule_type == RuleType::Http && r.listen == listen_addr)
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
        let new = match SharedListener::spawn(
            listen_addr.to_string(),
            tls_enabled,
            new_table,
            self.cert_manager().cloned(),
            active_rule_ids,
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
            if r.rule_type == RuleType::Http {
                set.insert(r.listen.clone());
            }
        }
        set.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::reverse_proxy::{
        Backend, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, ProxyTlsConfig, Route,
        RuleType,
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
                    addr: "127.0.0.1:8080".into(),
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
                    addr: backend.to_string(),
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
        let mgr = Arc::new(crate::server::acme::CertificateManager::new(
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
            crate::server::acme::CertEntry {
                cert_pem,
                key_pem,
                chain_pem: None,
                expires_at: None,
                source: crate::server::acme::CertSource::Manual,
            },
        )
        .await
        .unwrap();

        let cfg_arc: Arc<ServerConfig> = {
            use crate::server::acme::CertificateProvider;
            mgr.get_tls_server_config("localhost").await.unwrap()
        };
        assert_eq!(
            cfg_arc.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }
}
