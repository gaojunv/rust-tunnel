pub mod config;
pub mod error;
pub mod http_proxy;
pub mod router;
pub mod sni_resolver;
pub mod tcp_proxy;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::server::acme::CertificateProvider;
use crate::server::db::Database;

/// Load balancing algorithm
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancing {
    RoundRobin,
    WeightedRoundRobin,
}

impl Default for LoadBalancing {
    fn default() -> Self {
        Self::RoundRobin
    }
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

/// Backend server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backend {
    /// Backend server address (host:port)
    pub addr: String,
    /// Weight for load balancing (default: 100)
    #[serde(default = "default_weight")]
    pub weight: u32,
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

/// State for the reverse proxy module
#[derive(Clone)]
pub struct ReverseProxyState {
    /// Active proxy rules (id -> rule)
    pub rules: Arc<Mutex<HashMap<String, ProxyRule>>>,
    /// Active listener tasks (rule_id -> handle)
    pub listeners: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// Connection counters per rule
    pub connection_counts: Arc<Mutex<HashMap<String, u64>>>,
    /// Database reference
    db: Option<Database>,
    /// Certificate provider for TLS termination
    cert_provider: Option<Arc<dyn CertificateProvider>>,
}

impl ReverseProxyState {
    /// Create a new reverse proxy state without database
    pub fn new() -> Self {
        Self {
            rules: Arc::new(Mutex::new(HashMap::new())),
            listeners: Arc::new(Mutex::new(HashMap::new())),
            connection_counts: Arc::new(Mutex::new(HashMap::new())),
            db: None,
            cert_provider: None,
        }
    }

    /// Create a new reverse proxy state with database
    pub fn with_db(db: Database) -> Self {
        Self {
            rules: Arc::new(Mutex::new(HashMap::new())),
            listeners: Arc::new(Mutex::new(HashMap::new())),
            connection_counts: Arc::new(Mutex::new(HashMap::new())),
            db: Some(db),
            cert_provider: None,
        }
    }

    /// Set the certificate provider for TLS termination
    pub fn set_cert_provider(&mut self, provider: Arc<dyn CertificateProvider>) {
        self.cert_provider = Some(provider);
    }

    /// Get a reference to the certificate provider
    pub fn cert_provider(&self) -> Option<&Arc<dyn CertificateProvider>> {
        self.cert_provider.as_ref()
    }

    /// Load rules from database
    pub async fn load_from_db(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(db) = &self.db {
            let records = db.load_proxy_rules().await?;
            let mut rules = self.rules.lock().await;

            for record in records {
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
                    cert_status: None,
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

            let cert_source_str: Option<&'static str> = rule.cert_status.as_ref().map(|s| match s.source {
                CertSourceKind::Exact => "exact",
                CertSourceKind::WildcardReuse => "wildcard_reuse",
                CertSourceKind::PendingIssuance => "pending_issuance",
                CertSourceKind::None => "none",
            });
            let cert_covering = rule.cert_status.as_ref().map(|s| s.covering_domain.as_str());
            let cert_updated = rule.cert_status.as_ref().map(|s| &s.last_updated);

            db.save_proxy_rule(
                &rule.id,
                &rule.name,
                &rule.rule_type.to_string(),
                &rule.listen,
                domains_json.as_deref(),
                routes_json.as_deref(),
                tls.map_or(false, |t| t.enabled),
                tls.map_or(false, |t| t.acme),
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
        assert!(rule.cert_status.is_none(), "cert_status should be skipped on deserialize");
    }
}
