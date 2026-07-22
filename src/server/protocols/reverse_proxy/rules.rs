use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::server::acme::{CertCoverage, CertificateManager};
use crate::server::control::ServerState;

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

/// Kind of backend endpoint.
///
/// - `Direct`: `addr` is an external `host:port` reachable from the server.
/// - `Client`: `addr` is a `host:port` reachable from the client named
///   `client_name` (dial goes through the control channel tunnel).
///
/// Missing on deserialize → `Direct` (backward compat with pre-2026-07 rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    #[default]
    Direct,
    Client,
}

/// Backend server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backend {
    /// Backend endpoint kind (Direct = external, Client = via tunnel).
    /// Missing on deserialize → Direct.
    #[serde(default)]
    pub kind: BackendKind,
    /// Backend server address (`host:port`). For `kind = Client` this is a
    /// host:port reachable from the named client's own network.
    pub addr: String,
    /// Required when `kind = Client`; None for `kind = Direct` (sanitized on save).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
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

/// Trojan SNI 分流表项：反代共享监听器嗅探到匹配的 SNI 时，
/// 不终止该连接的 TLS，直接交给 `listener::handle_trojan_connection` 处理。
pub struct TrojanSniEntry {
    /// 匹配的 SNI 域名（小写）
    pub domain: String,
    /// 生效的反代共享监听地址（如 "0.0.0.0:443"）
    pub listen_addr: String,
    /// Trojan 端口（共享模式下不真实 bind，仅用于连接统计/流量记账）
    pub trojan_port: u16,
    pub password: String,
    pub fallback: String,
    /// Trojan 的 TLS 配置 watch channel（证书热更新）
    pub tls_config_rx: tokio::sync::watch::Receiver<Arc<rustls::ServerConfig>>,
    pub state: ServerState,
}

/// Compute the cert coverage source for a rule at save time.
///
/// Selection of the "primary domain":
/// 1. `rule.tls.domain` if set
/// 2. `rule.domains.first()`
/// 3. otherwise: source = None
pub async fn resolve_cert_source_for_rule(
    rule: &ProxyRule,
    cert_manager: Option<&Arc<CertificateManager>>,
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
        Some(CertCoverage::Exact) => RuleCertStatus {
            source: CertSourceKind::Exact,
            covering_domain: domain,
            last_updated: now,
        },
        Some(CertCoverage::Wildcard(w)) => RuleCertStatus {
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

/// Validate a rule about to be persisted. See spec §2.5.
/// Returns Err(message) on the first failing constraint.
pub fn validate_rule_for_save(rule: &ProxyRule) -> Result<(), String> {
    for route in &rule.routes {
        for b in &route.backends {
            match b.kind {
                BackendKind::Client => {
                    let name = b.client_name.as_deref().unwrap_or("");
                    if name.trim().is_empty() {
                        return Err("client backend requires non-empty client_name".into());
                    }
                    // parse host:port
                    if b.addr
                        .rsplit_once(':')
                        .and_then(|(_, p)| p.parse::<u16>().ok())
                        .is_none()
                    {
                        return Err(format!(
                            "client backend addr '{}' is not a valid host:port",
                            b.addr
                        ));
                    }
                    if rule.rule_type == RuleType::Udp {
                        return Err("UDP rules cannot use client backends".into());
                    }
                    if b.protocol == BackendProtocol::Http2 {
                        return Err("HTTP/2 to client backend not yet supported".into());
                    }
                }
                BackendKind::Direct => {
                    // client_name must be None for direct; harmless if not,
                    // but sanitize_rule() will zero it out.
                }
            }
        }
    }
    Ok(())
}

/// Zero out `client_name` on `Direct` backends before persisting.
pub fn sanitize_rule(rule: &mut ProxyRule) {
    for route in &mut rule.routes {
        for b in &mut route.backends {
            if b.kind == BackendKind::Direct {
                b.client_name = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
        let mgr = Arc::new(CertificateManager::new(
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
            kind: BackendKind::Direct,
            addr: "10.0.0.1:8080".to_string(),
            client_name: None,
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
    fn backend_kind_defaults_to_direct_on_missing() {
        let json = r#"{"addr":"10.0.0.1:80","weight":100,"protocol":"http1","scheme":"http"}"#;
        let b: Backend = serde_json::from_str(json).unwrap();
        assert_eq!(b.kind, BackendKind::Direct);
        assert!(b.client_name.is_none());
    }

    #[test]
    fn backend_client_roundtrip() {
        let b = Backend {
            kind: BackendKind::Client,
            addr: "localhost:80".into(),
            client_name: Some("home-nas".into()),
            weight: 100,
            protocol: BackendProtocol::Http1,
            scheme: BackendScheme::Http,
        };
        let s = serde_json::to_string(&b).unwrap();
        let back: Backend = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind, BackendKind::Client);
        assert_eq!(back.client_name.as_deref(), Some("home-nas"));
    }

    #[test]
    fn backend_direct_omits_client_name_on_serialize() {
        let b = Backend {
            kind: BackendKind::Direct,
            addr: "10.0.0.1:80".into(),
            client_name: None,
            weight: 100,
            protocol: BackendProtocol::Http1,
            scheme: BackendScheme::Http,
        };
        let s = serde_json::to_string(&b).unwrap();
        assert!(!s.contains("client_name"), "serialized: {s}");
    }

    #[test]
    fn backend_kind_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&BackendKind::Direct).unwrap(),
            "\"direct\""
        );
        assert_eq!(
            serde_json::to_string(&BackendKind::Client).unwrap(),
            "\"client\""
        );
    }

    #[test]
    fn validate_rule_rejects_client_backend_missing_name() {
        let rule = ProxyRule {
            id: "r".into(),
            name: "r".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".into(),
            domains: vec!["x.com".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Client,
                    addr: "localhost:80".into(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let err = validate_rule_for_save(&rule).expect_err("should reject");
        assert!(err.contains("client_name"));
    }

    #[test]
    fn validate_rule_rejects_client_backend_bad_addr() {
        let rule = ProxyRule {
            id: "r".into(),
            name: "r".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".into(),
            domains: vec!["x.com".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Client,
                    addr: "no-port".into(),
                    client_name: Some("home-nas".into()),
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let err = validate_rule_for_save(&rule).expect_err("should reject bad addr");
        assert!(err.contains("not a valid host:port"));
    }

    #[test]
    fn validate_rule_rejects_udp_client_backend() {
        let rule = ProxyRule {
            id: "r".into(),
            name: "r".into(),
            rule_type: RuleType::Udp,
            listen: "0.0.0.0:53".into(),
            domains: vec![],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Client,
                    addr: "1.1.1.1:53".into(),
                    client_name: Some("home-nas".into()),
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let err = validate_rule_for_save(&rule).expect_err("should reject UDP client");
        assert!(err.contains("UDP"));
    }

    #[test]
    fn validate_rule_rejects_http2_client_backend() {
        let rule = ProxyRule {
            id: "r".into(),
            name: "r".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".into(),
            domains: vec!["x.com".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Client,
                    addr: "localhost:80".into(),
                    client_name: Some("home-nas".into()),
                    weight: 100,
                    protocol: BackendProtocol::Http2,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let err = validate_rule_for_save(&rule).expect_err("should reject HTTP/2 client");
        assert!(err.contains("HTTP/2"));
    }

    #[test]
    fn validate_rule_direct_backend_ok() {
        let rule = ProxyRule {
            id: "r".into(),
            name: "r".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".into(),
            domains: vec!["x.com".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: "10.0.0.1:80".into(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        assert!(validate_rule_for_save(&rule).is_ok());
    }

    #[test]
    fn sanitize_rule_zeroes_client_name_on_direct() {
        let mut rule = ProxyRule {
            id: "r".into(),
            name: "r".into(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".into(),
            domains: vec!["x.com".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: "10.0.0.1:80".into(),
                    client_name: Some("stray".into()),
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        sanitize_rule(&mut rule);
        assert!(rule.routes[0].backends[0].client_name.is_none());
    }
}