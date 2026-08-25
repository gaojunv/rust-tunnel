use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

use super::llm_dispatch::LlmDispatcher;
use crate::reverse_proxy::connector;
use crate::reverse_proxy::rules::{
    resolve_cert_source_for_rule, Backend, BackendKind, CertSourceKind, ProxyRule, ProxyTlsConfig,
    RuleCertStatus, RuleType, TrojanSniEntry,
};
use rust_tunnel_persistence::Database;
use rust_tunnel_pki::acme::{CertificateManager, CertificateProvider};
use rust_tunnel_stats::StatsCollector;

/// State for the reverse proxy module
#[derive(Clone)]
pub struct ReverseProxyState {
    /// Active proxy rules (id -> rule)
    pub rules: Arc<Mutex<HashMap<String, ProxyRule>>>,
    /// HTTP shared listeners keyed by listen_addr (e.g. "0.0.0.0:443").
    pub shared_listeners:
        Arc<Mutex<HashMap<String, crate::reverse_proxy::shared_listener::SharedListener>>>,
    /// TCP/UDP per-rule listeners (previously named `listeners`).
    pub tcp_listeners: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// Stats collector reference (set after construction)
    pub stats_collector: StatsCollector,
    /// Per-listen_addr async mutex to serialize reconcile calls.
    pub reconcile_locks: Arc<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Database reference
    db: Option<Database>,
    /// Concrete CertificateManager (needed for SNI resolver and coverage queries).
    cert_manager: Option<Arc<CertificateManager>>,
    /// Direct connector (always available).
    pub direct_connector: Arc<connector::DirectConnector>,
    /// Optional client connector — Some(_) once ClientRegistry is wired in.
    pub client_connector: Arc<tokio::sync::RwLock<Option<Arc<connector::ClientConnector>>>>,
    /// Trojan SNI 分流表项（ArcSwap 热替换）：SNI 命中 domain 时，
    /// 对应 listen_addr 的共享监听器把连接交给 Trojan 处理。None = 独立监听模式。
    pub trojan_sni: Arc<arc_swap::ArcSwap<Option<Arc<TrojanSniEntry>>>>,
    /// LLM Gateway dispatcher (type-erased, injected by assembly layer).
    pub llm_dispatcher: Arc<tokio::sync::RwLock<Option<Arc<dyn LlmDispatcher>>>>,
}

impl ReverseProxyState {
    /// Create a new reverse proxy state without database
    #[must_use]
    pub fn new() -> Self {
        Self {
            rules: Arc::new(Mutex::new(HashMap::new())),
            shared_listeners: Arc::new(Mutex::new(HashMap::new())),
            tcp_listeners: Arc::new(Mutex::new(HashMap::new())),
            reconcile_locks: Arc::new(StdMutex::new(HashMap::new())),
            stats_collector: StatsCollector::new(None),
            db: None,
            cert_manager: None,
            direct_connector: Arc::new(connector::DirectConnector),
            client_connector: Arc::new(tokio::sync::RwLock::new(None)),
            trojan_sni: Arc::new(arc_swap::ArcSwap::from_pointee(None)),
            llm_dispatcher: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Create a new reverse proxy state with database
    #[must_use]
    pub fn with_db(db: Database) -> Self {
        Self {
            rules: Arc::new(Mutex::new(HashMap::new())),
            shared_listeners: Arc::new(Mutex::new(HashMap::new())),
            tcp_listeners: Arc::new(Mutex::new(HashMap::new())),
            reconcile_locks: Arc::new(StdMutex::new(HashMap::new())),
            stats_collector: StatsCollector::new(None),
            db: Some(db),
            cert_manager: None,
            direct_connector: Arc::new(connector::DirectConnector),
            client_connector: Arc::new(tokio::sync::RwLock::new(None)),
            trojan_sni: Arc::new(arc_swap::ArcSwap::from_pointee(None)),
            llm_dispatcher: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Set the concrete certificate manager for TLS termination and coverage queries.
    pub fn set_cert_manager(&mut self, manager: Arc<CertificateManager>) {
        self.cert_manager = Some(manager);
    }

    /// 挂上统一统计采集器（与 `ServerState.stats_collector` 共享同一实例）。
    /// 默认构造时是独立的 `StatsCollector::new(None)` 占位，不注入则埋点数据
    /// 不会出现在 `/api/stats/*` 中。
    pub fn set_stats_collector(&mut self, sc: StatsCollector) {
        self.stats_collector = sc;
    }

    /// Get the concrete certificate manager.
    #[must_use]
    pub fn cert_manager(&self) -> Option<&Arc<CertificateManager>> {
        self.cert_manager.as_ref()
    }

    /// 设置/清除 Trojan SNI 分流表项（热替换，无需重建监听器）。
    pub fn set_trojan_sni(&self, entry: Option<TrojanSniEntry>) {
        self.trojan_sni.store(Arc::new(entry.map(Arc::new)));
    }

    /// 取该监听地址生效的 Trojan 分流表项。
    #[must_use]
    pub fn trojan_sni_entry(&self, listen_addr: &str) -> Option<Arc<TrojanSniEntry>> {
        let snap = self.trojan_sni.load();
        match snap.as_ref() {
            Some(e) if e.listen_addr == listen_addr => Some(e.clone()),
            _ => None,
        }
    }

    /// 查找指定端口上启用的 HTTP 反代规则对应的 listen 地址与 TLS 状态。
    /// 返回 `Some((listen_addr, tls_enabled))`；该端口无任何启用的 HTTP 规则时返回 `None`。
    pub async fn http_listen_addr_for_port(&self, port: u16) -> Option<(String, bool)> {
        let rules = self.rules.lock().await;
        rules
            .values()
            .filter(|r| r.enabled && r.rule_type == RuleType::Http)
            .find(|r| {
                r.listen
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.parse::<u16>().ok())
                    == Some(port)
            })
            .map(|r| (r.listen.clone(), r.tls.as_ref().is_some_and(|t| t.enabled)))
    }

    /// 判断指定端口是否仍被反代共享监听器占用，返回占用的 listen 地址。
    /// 用于 Trojan 独立监听回退前的端口冲突检测（规则已降级/删除但 listener 仍绑定端口）。
    pub async fn shared_listener_addr_for_port(&self, port: u16) -> Option<String> {
        let listeners = self.shared_listeners.lock().await;
        listeners
            .keys()
            .find(|addr| addr.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) == Some(port))
            .cloned()
    }

    /// Backwards-compatible accessor returning the manager as a trait object.
    /// TCP proxy and legacy HTTP paths still use `CertificateProvider`.
    #[must_use]
    pub fn cert_provider(&self) -> Option<Arc<dyn CertificateProvider>> {
        self.cert_manager
            .as_ref()
            .map(|m| m.clone() as Arc<dyn CertificateProvider>)
    }

    /// Set the ClientConnector once ClientRegistry is available.
    pub async fn set_client_connector(&self, cc: Arc<connector::ClientConnector>) {
        *self.client_connector.write().await = Some(cc);
    }

    /// Pick the correct connector for a backend. Returns Err if kind=Client
    /// but no ClientConnector is registered yet.
    pub async fn connector_for(
        &self,
        backend: &Backend,
    ) -> std::io::Result<Arc<dyn connector::Connector>> {
        match backend.kind {
            BackendKind::Direct => {
                Ok(self.direct_connector.clone() as Arc<dyn connector::Connector>)
            }
            BackendKind::Client => {
                let guard = self.client_connector.read().await;
                guard
                    .as_ref()
                    .map(|c| c.clone() as Arc<dyn connector::Connector>)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "client backend used before ClientConnector was registered",
                        )
                    })
            }
        }
    }

    /// Load rules from database
    pub async fn load_from_db(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(db) = &self.db {
            let records = db.load_proxy_rules().await?;
            let mut rules = self.rules.lock().await;

            for record in records {
                // Reconstruct cert_status if all three DB columns are present.
                let cert_status = match (
                    record.cert_source.as_deref(),
                    record.cert_covering_domain.clone(),
                    record.cert_status_updated_at,
                ) {
                    (Some(src), Some(covering), Some(ts)) => {
                        let source = match src {
                            "exact" => Some(CertSourceKind::Exact),
                            "wildcard_reuse" => Some(CertSourceKind::WildcardReuse),
                            "pending_issuance" => Some(CertSourceKind::PendingIssuance),
                            "none" => Some(CertSourceKind::None),
                            other => {
                                tracing::warn!("Unknown cert_source '{}' in DB, dropping", other);
                                None
                            }
                        };
                        source.map(|s| RuleCertStatus {
                            source: s,
                            covering_domain: covering,
                            last_updated: ts,
                        })
                    }
                    _ => None, // Partial state / pre-migration row → refresh on next event
                };

                let rule = ProxyRule {
                    id: record.id,
                    name: record.name,
                    rule_type: match record.rule_type.as_str() {
                        "http" => RuleType::Http,
                        "tcp" => RuleType::Tcp,
                        "udp" => RuleType::Udp,
                        "llm" => RuleType::Llm,
                        _ => continue,
                    },
                    listen: record.listen_addr,
                    domains: record
                        .domains
                        .as_deref()
                        .and_then(|d| serde_json::from_str(d).ok())
                        .unwrap_or_default(),
                    routes: record
                        .routes
                        .as_deref()
                        .and_then(|r| serde_json::from_str(r).ok())
                        .unwrap_or_default(),
                    tls: if record.tls_enabled != 0 {
                        Some(ProxyTlsConfig {
                            enabled: true,
                            acme: record.tls_acme != 0,
                            domain: record.tls_domain,
                        })
                    } else {
                        None
                    },
                    enabled: record.enabled != 0,
                    created_at: Some(record.created_at.to_rfc3339()),
                    cert_status,
                };
                rules.insert(rule.id.clone(), rule);
            }
        }
        Ok(())
    }

    /// Save a rule to database
    pub async fn save_rule(&self, rule: &ProxyRule) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(db) = &self.db {
            let domains_json = if rule.domains.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&rule.domains)?)
            };
            let routes_json = if rule.routes.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&rule.routes)?)
            };
            let tls = rule.tls.as_ref();

            let cert_source_str: Option<&'static str> =
                rule.cert_status.as_ref().map(|s| match s.source {
                    CertSourceKind::Exact => "exact",
                    CertSourceKind::WildcardReuse => "wildcard_reuse",
                    CertSourceKind::PendingIssuance => "pending_issuance",
                    CertSourceKind::None => "none",
                });
            let cert_covering = rule
                .cert_status
                .as_ref()
                .map(|s| s.covering_domain.as_str());
            let cert_updated = rule.cert_status.as_ref().map(|s| &s.last_updated);

            db.save_proxy_rule(
                &rule.id,
                &rule.name,
                &rule.rule_type.to_string(),
                &rule.listen,
                domains_json.as_deref(),
                routes_json.as_deref(),
                tls.is_some_and(|t| t.enabled),
                tls.is_some_and(|t| t.acme),
                tls.and_then(|t| t.domain.as_deref()),
                rule.enabled,
                cert_source_str,
                cert_covering,
                cert_updated,
            )
            .await?;
        }
        Ok(())
    }

    /// Delete a rule from database
    pub async fn delete_rule(&self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(db) = &self.db {
            db.delete_proxy_rule(id).await?;
        }
        Ok(())
    }

    /// Recompute cert_status for every TLS-enabled rule. Called from the
    /// cert_event_reactor after CertEvent::Issued/Renewed/Expired.
    pub async fn refresh_all_cert_status(&self, cert_manager: &Arc<CertificateManager>) {
        // Snapshot rules to compute outside the lock
        let snapshot: Vec<ProxyRule> = {
            let rules = self.rules.lock().await;
            rules.values().cloned().collect()
        };

        let mut updates: Vec<(String, RuleCertStatus)> = Vec::new();
        for rule in &snapshot {
            if rule.tls.as_ref().is_some_and(|t| t.enabled) {
                let new = resolve_cert_source_for_rule(rule, Some(cert_manager)).await;
                let changed = match &rule.cert_status {
                    None => true,
                    Some(prev) => {
                        prev.source != new.source || prev.covering_domain != new.covering_domain
                    }
                };
                if changed {
                    updates.push((rule.id.clone(), new));
                }
            }
        }

        if updates.is_empty() {
            return;
        }

        // Apply in-memory + persist
        {
            let mut rules = self.rules.lock().await;
            for (id, status) in &updates {
                if let Some(r) = rules.get_mut(id) {
                    r.cert_status = Some(status.clone());
                }
            }
        }
        // Persist (fire-and-forget on failure — logged)
        for (id, _) in &updates {
            let rule = {
                let rules = self.rules.lock().await;
                rules.get(id).cloned()
            };
            if let Some(r) = rule {
                if let Err(e) = self.save_rule(&r).await {
                    tracing::error!("Failed to persist refreshed cert_status for {}: {}", id, e);
                }
            }
        }
    }
}

impl Default for ReverseProxyState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_all_cert_status_updates_pending_to_wildcard() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(CertificateManager::new(temp_dir.path().to_str().unwrap()));

        let state = ReverseProxyState::new();
        // Insert a rule referencing api.example.com, TLS enabled but no cert yet -> PendingIssuance
        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:443".into(),
            domains: vec!["api.example.com".into()],
            routes: vec![],
            tls: Some(ProxyTlsConfig {
                enabled: true,
                acme: true,
                domain: None,
            }),
            enabled: true,
            created_at: None,
            cert_status: Some(RuleCertStatus {
                source: CertSourceKind::PendingIssuance,
                covering_domain: "api.example.com".into(),
                last_updated: chrono::Utc::now(),
            }),
        };
        state.rules.lock().await.insert("r1".into(), rule);

        // Add wildcard cert to manager (rcgen 0.14 API, matches gen_self_signed_pem_for in manager tests)
        let (cert_pem, key_pem) = {
            use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
            let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
            let params = CertificateParams::new(vec!["*.example.com".to_string()]).unwrap();
            let cert = params.self_signed(&key_pair).unwrap();
            (cert.pem(), key_pair.serialize_pem())
        };
        mgr.add_certificate(
            "*.example.com",
            rust_tunnel_pki::acme::CertEntry {
                cert_pem,
                key_pem,
                chain_pem: None,
                expires_at: None,
                source: rust_tunnel_pki::acme::CertSource::Manual,
            },
        )
        .await
        .unwrap();

        state.refresh_all_cert_status(&mgr).await;
        let rules = state.rules.lock().await;
        let r = rules.get("r1").unwrap();
        assert_eq!(
            r.cert_status.as_ref().unwrap().source,
            CertSourceKind::WildcardReuse
        );
    }

    fn make_entry(domain: &str, listen_addr: &str) -> TrojanSniEntry {
        // 构造一个最小的 ServerConfig 占位（tls_config_rx 的类型是 Receiver<Arc<ServerConfig>>）
        let cfg = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(
                    crate::reverse_proxy::sni_resolver::SniCertResolver::new(Arc::new(
                        rust_tunnel_pki::acme::CertificateManager::new("/tmp/nonexistent-test-dir"),
                    )),
                )),
        );
        let (_tx, rx) = tokio::sync::watch::channel(cfg);
        TrojanSniEntry {
            domain: domain.to_string(),
            listen_addr: listen_addr.to_string(),
            trojan_port: 443,
            password: "p".to_string(),
            fallback: "127.0.0.1:80".to_string(),
            tls_config_rx: rx,
            registry: std::sync::Arc::new(crate::port_registry::MockPortRegistry::new()),
            stats: rust_tunnel_stats::StatsCollector::new(None),
        }
    }

    #[test]
    fn trojan_sni_entry_matches_listen_addr() {
        let state = ReverseProxyState::new();
        assert!(state.trojan_sni_entry("0.0.0.0:443").is_none());

        state.set_trojan_sni(Some(make_entry("trojan.example.com", "0.0.0.0:443")));
        let hit = state.trojan_sni_entry("0.0.0.0:443").expect("should hit");
        assert_eq!(hit.domain, "trojan.example.com");
        // 其他监听地址不命中
        assert!(state.trojan_sni_entry("0.0.0.0:8443").is_none());

        state.set_trojan_sni(None);
        assert!(state.trojan_sni_entry("0.0.0.0:443").is_none());
    }

    #[tokio::test]
    async fn http_listen_addr_for_port_reports_tls() {
        let state = ReverseProxyState::new();
        assert!(state.http_listen_addr_for_port(443).await.is_none());

        let tls_rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:443".into(),
            domains: vec!["a.example.com".into()],
            routes: vec![],
            tls: Some(ProxyTlsConfig {
                enabled: true,
                acme: false,
                domain: Some("a.example.com".into()),
            }),
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        state.rules.lock().await.insert("r1".into(), tls_rule);
        assert_eq!(
            state.http_listen_addr_for_port(443).await,
            Some(("0.0.0.0:443".to_string(), true))
        );
        assert!(state.http_listen_addr_for_port(8443).await.is_none());
    }
}
