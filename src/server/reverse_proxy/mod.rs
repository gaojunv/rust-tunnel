pub mod config;
pub mod error;
pub mod handler;
pub mod router;
pub mod shared_listener;
pub mod sni_resolver;
pub mod tcp_proxy;
pub mod upstream;

#[cfg(test)]
mod http2_test;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

use crate::server::db::Database;

/// Load balancing algorithm
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum LoadBalancing {
    #[default]
    RoundRobin,
    WeightedRoundRobin,
}

/// Proxy rule type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RuleType {
    Http,
    Tcp,
    Udp,
}

impl std::fmt::Display for RuleType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http => write!(f, "http"),
            Self::Tcp => write!(f, "tcp"),
            Self::Udp => write!(f, "udp"),
        }
    }
}

/// Application-layer protocol on the connection to the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendProtocol {
    #[default]
    Http1,
    /// h2 over TLS (ALPN-negotiated) or h2c prior-knowledge over plain TCP.
    Http2,
}

/// Transport scheme on the connection to the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendScheme {
    #[default]
    Http,
    Https,
}

/// Backend server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backend {
    /// Backend server address (host:port)
    pub addr: String,
    /// Weight for load balancing (default: 100)
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Application-layer protocol; defaults to Http1 for backward compat.
    #[serde(default)]
    pub protocol: BackendProtocol,
    /// Transport scheme; defaults to Http.
    #[serde(default)]
    pub scheme: BackendScheme,
}

fn default_weight() -> u32 {
    100
}

/// Route configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Path prefix to match (e.g., "/api")
    pub path: String,
    /// Backend servers
    pub backends: Vec<Backend>,
    /// Load balancing algorithm
    #[serde(default)]
    pub load_balancing: LoadBalancing,
}

/// TLS configuration for proxy rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyTlsConfig {
    /// Enable TLS
    pub enabled: bool,
    /// Use ACME for certificate
    #[serde(default)]
    pub acme: bool,
    /// Domain for ACME certificate
    pub domain: Option<String>,
}

/// Kind of certificate coverage for a proxy rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertSourceKind {
    /// Exact-match certificate is available for the rule's primary domain.
    Exact,
    /// A wildcard certificate covers the rule's primary domain.
    WildcardReuse,
    /// TLS is enabled but no covering certificate exists yet
    /// (ACME issuance in progress or not yet triggered).
    PendingIssuance,
    /// TLS is not enabled for this rule.
    None,
}

/// Runtime certificate status attached to a proxy rule.
///
/// Read-only: populated by the server on rule save and cert events,
/// never accepted from client input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleCertStatus {
    pub source: CertSourceKind,
    pub covering_domain: String,
    pub last_updated: chrono::DateTime<chrono::Utc>,
}

/// Proxy rule definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRule {
    /// Unique identifier
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Rule type (http, tcp, udp)
    #[serde(rename = "type")]
    pub rule_type: RuleType,
    /// Listen address (host:port)
    pub listen: String,
    /// Domain names (for HTTP rules)
    #[serde(default)]
    pub domains: Vec<String>,
    /// Routes (for HTTP rules)
    #[serde(default)]
    pub routes: Vec<Route>,
    /// TLS configuration
    pub tls: Option<ProxyTlsConfig>,
    /// Whether the rule is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Creation timestamp
    pub created_at: Option<String>,
    /// Runtime cert coverage status. Never accepted from client input.
    #[serde(default, skip_deserializing)]
    pub cert_status: Option<RuleCertStatus>,
}

fn default_enabled() -> bool {
    true
}

/// Statistics for a proxy rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRuleStats {
    pub total_connections: u64,
    pub active_connections: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// Global proxy statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyStats {
    pub total_rules: i64,
    pub active_rules: i64,
    pub total_connections: i64,
    pub bytes_in: i64,
    pub bytes_out: i64,
}

/// Accumulated traffic for one rule, pending flush to the database.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrafficDelta {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub connections: u64,
}

/// In-memory traffic accumulator shared between the proxy handlers and the
/// periodic DB flush task. Uses a std mutex so it can be updated from sync
/// contexts (e.g. body-stream `inspect` callbacks) without an async runtime.
pub type TrafficPending = Arc<StdMutex<HashMap<String, TrafficDelta>>>;

/// Add bytes / connections to the pending accumulator for a rule.
pub fn record_traffic(
    pending: &TrafficPending,
    rule_id: &str,
    bytes_in: u64,
    bytes_out: u64,
    connections: u64,
) {
    if bytes_in == 0 && bytes_out == 0 && connections == 0 {
        return;
    }
    let mut map = pending.lock().unwrap();
    let entry = map.entry(rule_id.to_string()).or_default();
    entry.bytes_in += bytes_in;
    entry.bytes_out += bytes_out;
    entry.connections += connections;
}

/// State for the reverse proxy module
#[derive(Clone)]
pub struct ReverseProxyState {
    /// Active proxy rules (id -> rule)
    pub rules: Arc<Mutex<HashMap<String, ProxyRule>>>,
    /// HTTP shared listeners keyed by listen_addr (e.g. "0.0.0.0:443").
    pub shared_listeners:
        Arc<Mutex<HashMap<String, crate::server::reverse_proxy::shared_listener::SharedListener>>>,
    /// TCP/UDP per-rule listeners (previously named `listeners`).
    pub tcp_listeners: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// Connection counters per rule
    pub connection_counts: Arc<Mutex<HashMap<String, u64>>>,
    /// Traffic accumulator per rule, flushed to DB periodically
    pub traffic_pending: TrafficPending,
    /// Per-listen_addr async mutex to serialize reconcile calls.
    pub reconcile_locks: Arc<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Database reference
    db: Option<Database>,
    /// Concrete CertificateManager (needed for SNI resolver and coverage queries).
    cert_manager: Option<Arc<crate::server::acme::CertificateManager>>,
}

/// Compute the cert coverage source for a rule at save time.
///
/// Selection of the "primary domain":
/// 1. `rule.tls.domain` if set
/// 2. `rule.domains.first()`
/// 3. otherwise: source = None
pub async fn resolve_cert_source_for_rule(
    rule: &ProxyRule,
    cert_manager: Option<&Arc<crate::server::acme::CertificateManager>>,
) -> RuleCertStatus {
    let now = chrono::Utc::now();

    let Some(tls) = rule.tls.as_ref().filter(|t| t.enabled) else {
        return RuleCertStatus {
            source: CertSourceKind::None,
            covering_domain: String::new(),
            last_updated: now,
        };
    };
    let primary = tls
        .domain
        .clone()
        .or_else(|| rule.domains.first().cloned())
        // Normalize so it matches the storage key inside CertificateManager
        // (which lowercases on add) and the SNI value delivered by rustls.
        .map(|d| d.to_ascii_lowercase());
    let Some(domain) = primary else {
        return RuleCertStatus {
            source: CertSourceKind::None,
            covering_domain: String::new(),
            last_updated: now,
        };
    };

    let Some(mgr) = cert_manager else {
        return RuleCertStatus {
            source: CertSourceKind::PendingIssuance,
            covering_domain: domain,
            last_updated: now,
        };
    };

    match mgr.find_covering_cert(&domain).await {
        Some(crate::server::acme::CertCoverage::Exact) => RuleCertStatus {
            source: CertSourceKind::Exact,
            covering_domain: domain,
            last_updated: now,
        },
        Some(crate::server::acme::CertCoverage::Wildcard(w)) => RuleCertStatus {
            source: CertSourceKind::WildcardReuse,
            covering_domain: w,
            last_updated: now,
        },
        None => RuleCertStatus {
            source: CertSourceKind::PendingIssuance,
            covering_domain: domain,
            last_updated: now,
        },
    }
}

impl ReverseProxyState {
    /// Create a new reverse proxy state without database
    pub fn new() -> Self {
        Self {
            rules: Arc::new(Mutex::new(HashMap::new())),
            shared_listeners: Arc::new(Mutex::new(HashMap::new())),
            tcp_listeners: Arc::new(Mutex::new(HashMap::new())),
            connection_counts: Arc::new(Mutex::new(HashMap::new())),
            traffic_pending: Arc::new(StdMutex::new(HashMap::new())),
            reconcile_locks: Arc::new(StdMutex::new(HashMap::new())),
            db: None,
            cert_manager: None,
        }
    }

    /// Create a new reverse proxy state with database
    pub fn with_db(db: Database) -> Self {
        Self {
            rules: Arc::new(Mutex::new(HashMap::new())),
            shared_listeners: Arc::new(Mutex::new(HashMap::new())),
            tcp_listeners: Arc::new(Mutex::new(HashMap::new())),
            connection_counts: Arc::new(Mutex::new(HashMap::new())),
            traffic_pending: Arc::new(StdMutex::new(HashMap::new())),
            reconcile_locks: Arc::new(StdMutex::new(HashMap::new())),
            db: Some(db),
            cert_manager: None,
        }
    }

    /// Set the concrete certificate manager for TLS termination and coverage queries.
    pub fn set_cert_manager(&mut self, manager: Arc<crate::server::acme::CertificateManager>) {
        self.cert_manager = Some(manager);
    }

    /// Get the concrete certificate manager.
    #[must_use]
    pub fn cert_manager(&self) -> Option<&Arc<crate::server::acme::CertificateManager>> {
        self.cert_manager.as_ref()
    }

    /// Backwards-compatible accessor returning the manager as a trait object.
    /// TCP proxy and legacy HTTP paths still use `CertificateProvider`.
    #[must_use]
    pub fn cert_provider(&self) -> Option<Arc<dyn crate::server::acme::CertificateProvider>> {
        self.cert_manager
            .as_ref()
            .map(|m| m.clone() as Arc<dyn crate::server::acme::CertificateProvider>)
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

    /// Increment connection count for a rule
    pub async fn increment_connections(&self, rule_id: &str) {
        let mut counts = self.connection_counts.lock().await;
        *counts.entry(rule_id.to_string()).or_insert(0) += 1;
    }

    /// Decrement connection count for a rule
    pub async fn decrement_connections(&self, rule_id: &str) {
        let mut counts = self.connection_counts.lock().await;
        if let Some(count) = counts.get_mut(rule_id) {
            if *count > 0 {
                *count -= 1;
            }
        }
    }

    /// Get connection count for a rule
    pub async fn get_connection_count(&self, rule_id: &str) -> u64 {
        let counts = self.connection_counts.lock().await;
        counts.get(rule_id).copied().unwrap_or(0)
    }

    /// Accumulate traffic for a rule in the pending map (flushed to DB periodically).
    pub fn record_traffic(&self, rule_id: &str, bytes_in: u64, bytes_out: u64, connections: u64) {
        record_traffic(
            &self.traffic_pending,
            rule_id,
            bytes_in,
            bytes_out,
            connections,
        );
    }

    /// Drain the pending traffic accumulator into the `proxy_traffic` table.
    /// Called periodically by the background flush task.
    pub async fn flush_traffic_to_db(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(db) = &self.db else {
            return Ok(());
        };

        // Atomically take the accumulated deltas (brief lock)
        let deltas: HashMap<String, TrafficDelta> = {
            let mut pending = self.traffic_pending.lock().unwrap();
            std::mem::take(&mut *pending)
        };

        for (rule_id, delta) in deltas {
            // Note: if the rule was deleted since the delta was recorded the
            // FK constraint rejects the insert; the delta is dropped rather
            // than re-queued so a stale rule id can't poison every flush.
            if let Err(e) = db
                .insert_proxy_traffic(
                    &rule_id,
                    delta.bytes_in,
                    delta.bytes_out,
                    delta.connections as i32,
                )
                .await
            {
                tracing::warn!("Failed to flush proxy_traffic for rule {}: {}", rule_id, e);
            }
        }
        Ok(())
    }

    /// Start the background task that periodically flushes proxy traffic to the database.
    pub fn start_traffic_flush_task(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                if let Err(e) = state.flush_traffic_to_db().await {
                    tracing::warn!("Failed to flush proxy traffic to DB: {}", e);
                }
            }
        });
    }

    /// Recompute cert_status for every TLS-enabled rule. Called from the
    /// cert_event_reactor after CertEvent::Issued/Renewed/Expired.
    pub async fn refresh_all_cert_status(
        &self,
        cert_manager: &Arc<crate::server::acme::CertificateManager>,
    ) {
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
mod mod_tests {
    use super::*;

    #[test]
    fn cert_status_default_is_none_optional() {
        let rule = ProxyRule {
            id: "id-1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:443".into(),
            domains: vec!["a.example.com".into()],
            routes: vec![],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        assert!(rule.cert_status.is_none());
    }

    #[test]
    fn cert_source_kind_serializes_lowercase() {
        let s = serde_json::to_string(&CertSourceKind::WildcardReuse).unwrap();
        assert_eq!(s, "\"wildcard_reuse\"");
    }

    #[test]
    fn cert_status_skip_deserializing_ignored_from_input() {
        // ProxyRule.tls is Option<ProxyTlsConfig>; serde treats missing
        // Option fields as None so the test JSON does not strictly need
        // "tls": null. Included here for clarity — the important assertion
        // is that cert_status is dropped regardless of what the client sends.
        let json = r#"{
            "id": "id-1",
            "name": "r1",
            "type": "http",
            "listen": "0.0.0.0:443",
            "tls": null,
            "cert_status": { "source": "exact", "covering_domain": "x", "last_updated": "2026-01-01T00:00:00Z" }
        }"#;
        let rule: ProxyRule = serde_json::from_str(json).unwrap();
        assert!(
            rule.cert_status.is_none(),
            "cert_status should be skipped on deserialize"
        );
    }

    #[tokio::test]
    async fn resolve_cert_source_none_when_tls_disabled() {
        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".into(),
            domains: vec!["a.example.com".into()],
            routes: vec![],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let out = resolve_cert_source_for_rule(&rule, None).await;
        assert_eq!(out.source, CertSourceKind::None);
    }

    #[tokio::test]
    async fn resolve_cert_source_pending_when_no_cert() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(crate::server::acme::CertificateManager::new(
            temp_dir.path().to_str().unwrap(),
        ));

        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:443".into(),
            domains: vec!["nope.example.com".into()],
            routes: vec![],
            tls: Some(ProxyTlsConfig {
                enabled: true,
                acme: true,
                domain: None,
            }),
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let out = resolve_cert_source_for_rule(&rule, Some(&mgr)).await;
        assert_eq!(out.source, CertSourceKind::PendingIssuance);
        assert_eq!(out.covering_domain, "nope.example.com");
    }

    #[tokio::test]
    async fn refresh_all_cert_status_updates_pending_to_wildcard() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(crate::server::acme::CertificateManager::new(
            temp_dir.path().to_str().unwrap(),
        ));

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

        state.refresh_all_cert_status(&mgr).await;
        let rules = state.rules.lock().await;
        let r = rules.get("r1").unwrap();
        assert_eq!(
            r.cert_status.as_ref().unwrap().source,
            CertSourceKind::WildcardReuse
        );
    }

    #[test]
    fn backend_deserializes_with_defaults_for_missing_fields() {
        let json = r#"{"addr":"127.0.0.1:80","weight":100}"#;
        let backend: Backend = serde_json::from_str(json).unwrap();
        assert_eq!(backend.protocol, BackendProtocol::Http1);
        assert_eq!(backend.scheme, BackendScheme::Http);
    }

    #[test]
    fn backend_roundtrip_with_explicit_fields() {
        let backend = Backend {
            addr: "10.0.0.1:8080".to_string(),
            weight: 100,
            protocol: BackendProtocol::Http2,
            scheme: BackendScheme::Https,
        };
        let s = serde_json::to_string(&backend).unwrap();
        let parsed: Backend = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.protocol, BackendProtocol::Http2);
        assert_eq!(parsed.scheme, BackendScheme::Https);
    }

    #[test]
    fn record_traffic_accumulates_per_rule() {
        let pending = TrafficPending::default();
        record_traffic(&pending, "r1", 100, 200, 1);
        record_traffic(&pending, "r1", 50, 0, 0);
        record_traffic(&pending, "r2", 0, 0, 2);

        let map = pending.lock().unwrap();
        let r1 = map.get("r1").unwrap();
        assert_eq!(r1.bytes_in, 150);
        assert_eq!(r1.bytes_out, 200);
        assert_eq!(r1.connections, 1);
        assert_eq!(map.get("r2").unwrap().connections, 2);
    }

    #[tokio::test]
    async fn flush_traffic_to_db_persists_and_drains() {
        let db = crate::server::db::Database::new(":memory:").await.unwrap();
        let state = ReverseProxyState::with_db(db.clone());

        // proxy_traffic has a FK to proxy_rules — the rule must exist first
        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".into(),
            domains: vec![],
            routes: vec![],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        state.save_rule(&rule).await.unwrap();

        state.record_traffic("r1", 100, 200, 2);
        state.record_traffic("r1", 10, 20, 0);
        state.flush_traffic_to_db().await.unwrap();

        // Pending map must be drained after a successful flush
        assert!(state.traffic_pending.lock().unwrap().is_empty());

        let (_, _, total_connections, bytes_in, bytes_out) = db.get_proxy_stats().await.unwrap();
        assert_eq!(total_connections, 2);
        assert_eq!(bytes_in, 110);
        assert_eq!(bytes_out, 220);

        // A second flush within the same minute merges into the same bucket row
        state.record_traffic("r1", 5, 5, 1);
        state.flush_traffic_to_db().await.unwrap();
        let (_, _, total_connections, bytes_in, bytes_out) = db.get_proxy_stats().await.unwrap();
        assert_eq!(total_connections, 3);
        assert_eq!(bytes_in, 115);
        assert_eq!(bytes_out, 225);
    }
}
