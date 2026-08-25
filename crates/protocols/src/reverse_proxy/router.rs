use super::{Backend, LoadBalancing, ProxyRule, Route, RuleType};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Routing table for proxy rules
pub struct RouteTable {
    /// HTTP rules indexed by domain
    domain_rules: HashMap<String, Vec<String>>,
    /// All rules by ID
    rules: HashMap<String, ProxyRule>,
    /// Round-robin counters per route
    rr_counters: Arc<Mutex<HashMap<String, usize>>>,
}

impl RouteTable {
    /// Create a new empty route table
    #[must_use] 
    pub fn new() -> Self {
        Self {
            domain_rules: HashMap::new(),
            rules: HashMap::new(),
            rr_counters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Build route table from a list of rules
    #[must_use] 
    pub fn from_rules(rules: Vec<ProxyRule>) -> Self {
        let mut table = Self::new();
        for rule in rules {
            table.add_rule(rule);
        }
        table
    }

    /// Add a rule to the table
    pub fn add_rule(&mut self, rule: ProxyRule) {
        // Index by domain for HTTP rules
        if rule.rule_type == RuleType::Http {
            for domain in &rule.domains {
                self.domain_rules
                    .entry(domain.clone())
                    .or_default()
                    .push(rule.id.clone());
            }
        }
        self.rules.insert(rule.id.clone(), rule);
    }

    /// Remove a rule from the table
    pub fn remove_rule(&mut self, id: &str) {
        if let Some(rule) = self.rules.remove(id) {
            if rule.rule_type == RuleType::Http {
                for domain in &rule.domains {
                    if let Some(ids) = self.domain_rules.get_mut(domain) {
                        ids.retain(|r| r != id);
                        if ids.is_empty() {
                            self.domain_rules.remove(domain);
                        }
                    }
                }
            }
        }
    }

    /// Find a rule by ID
    #[must_use] 
    pub fn get_rule(&self, id: &str) -> Option<&ProxyRule> {
        self.rules.get(id)
    }

    /// Get all rules
    #[must_use] 
    pub fn get_all_rules(&self) -> Vec<&ProxyRule> {
        self.rules.values().collect()
    }

    /// Match an HTTP request to a backend
    /// Returns (rule, route, backend) if matched
    pub async fn match_http_request(
        &self,
        host: &str,
        path: &str,
    ) -> Option<(&ProxyRule, &Route, &Backend)> {
        // Try exact domain match first
        if let Some(rule_ids) = self.domain_rules.get(host) {
            for rule_id in rule_ids {
                if let Some(rule) = self.rules.get(rule_id) {
                    if !rule.enabled || rule.rule_type != RuleType::Http {
                        continue;
                    }
                    if let Some((route, backend)) = self.match_path(rule, path).await {
                        return Some((rule, route, backend));
                    }
                }
            }
        }

        // Try wildcard domain match (*.example.com)
        if let Some(dot_pos) = host.find('.') {
            let wildcard = format!("*{}", &host[dot_pos..]);
            if let Some(rule_ids) = self.domain_rules.get(&wildcard) {
                for rule_id in rule_ids {
                    if let Some(rule) = self.rules.get(rule_id) {
                        if !rule.enabled || rule.rule_type != RuleType::Http {
                            continue;
                        }
                        if let Some((route, backend)) = self.match_path(rule, path).await {
                            return Some((rule, route, backend));
                        }
                    }
                }
            }
        }

        None
    }

    /// Match a path against a rule's routes
    async fn match_path<'a>(
        &self,
        rule: &'a ProxyRule,
        path: &str,
    ) -> Option<(&'a Route, &'a Backend)> {
        // Sort routes by path length (longest first) for most specific match
        let mut routes: Vec<&Route> = rule.routes.iter().collect();
        routes.sort_by_key(|r| std::cmp::Reverse(r.path.len()));

        for route in routes {
            if path.starts_with(&route.path) || route.path == "/" {
                if let Some(backend) = self.select_backend(&route.path, route).await {
                    return Some((route, backend));
                }
            }
        }

        // Default route (empty path or "/")
        if let Some(route) = rule.routes.first() {
            if let Some(backend) = self.select_backend(&route.path, route).await {
                return Some((route, backend));
            }
        }

        None
    }

    /// Select a backend using the configured load balancing algorithm
    async fn select_backend<'a>(&self, route_key: &str, route: &'a Route) -> Option<&'a Backend> {
        if route.backends.is_empty() {
            return None;
        }

        match route.load_balancing {
            LoadBalancing::RoundRobin => {
                let mut counters = self.rr_counters.lock().await;
                let counter = counters.entry(route_key.to_string()).or_insert(0);
                let idx = *counter % route.backends.len();
                *counter = counter.wrapping_add(1);
                route.backends.get(idx)
            }
            LoadBalancing::WeightedRoundRobin => {
                // Simple weighted round-robin: select based on cumulative weights
                let total_weight: u32 = route.backends.iter().map(|b| b.weight).sum();
                if total_weight == 0 {
                    return route.backends.first();
                }

                let mut counters = self.rr_counters.lock().await;
                let counter = counters.entry(route_key.to_string()).or_insert(0);
                let target = (*counter as u32) % total_weight;
                *counter = counter.wrapping_add(1);

                let mut cumulative = 0;
                for backend in &route.backends {
                    cumulative += backend.weight;
                    if target < cumulative {
                        return Some(backend);
                    }
                }
                route.backends.first()
            }
        }
    }

    /// Find a TCP/UDP rule by listen address
    #[must_use] 
    pub fn match_tcp_rule(&self, listen_addr: &str) -> Option<&ProxyRule> {
        self.rules.values().find(|rule| {
            rule.enabled
                && (rule.rule_type == RuleType::Tcp || rule.rule_type == RuleType::Udp)
                && rule.listen == listen_addr
        })
    }
}

impl Default for RouteTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reverse_proxy::{BackendKind, BackendProtocol, BackendScheme};

    fn create_test_rule(id: &str, domains: Vec<&str>, path: &str) -> ProxyRule {
        ProxyRule {
            id: id.to_string(),
            name: format!("Rule {id}"),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".to_string(),
            domains: domains.into_iter().map(String::from).collect(),
            routes: vec![Route {
                path: path.to_string(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: "127.0.0.1:8080".to_string(),
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
        }
    }

    #[tokio::test]
    async fn test_route_table_add_and_get() {
        let mut table = RouteTable::new();
        let rule = create_test_rule("rule1", vec!["example.com"], "/");
        table.add_rule(rule);

        assert!(table.get_rule("rule1").is_some());
        assert!(table.get_rule("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_route_table_domain_match() {
        let mut table = RouteTable::new();
        let rule = create_test_rule("rule1", vec!["example.com", "www.example.com"], "/");
        table.add_rule(rule);

        let result = table.match_http_request("example.com", "/").await;
        assert!(result.is_some());

        let result = table.match_http_request("www.example.com", "/").await;
        assert!(result.is_some());

        let result = table.match_http_request("other.com", "/").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_route_table_remove() {
        let mut table = RouteTable::new();
        let rule = create_test_rule("rule1", vec!["example.com"], "/");
        table.add_rule(rule);

        table.remove_rule("rule1");
        assert!(table.get_rule("rule1").is_none());
        assert!(table.match_http_request("example.com", "/").await.is_none());
    }

    #[tokio::test]
    async fn test_route_table_wildcard_match() {
        let mut table = RouteTable::new();
        let rule = create_test_rule("rule1", vec!["*.example.com"], "/");
        table.add_rule(rule);

        let result = table.match_http_request("api.example.com", "/").await;
        assert!(result.is_some());

        let result = table.match_http_request("example.com", "/").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_route_table_path_matching() {
        let mut table = RouteTable::new();
        let rule = ProxyRule {
            id: "rule1".to_string(),
            name: "Test Rule".to_string(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".to_string(),
            domains: vec!["example.com".to_string()],
            routes: vec![
                Route {
                    path: "/api".to_string(),
                    backends: vec![Backend {
                        kind: BackendKind::Direct,
                        addr: "127.0.0.1:8081".to_string(),
                        client_name: None,
                        weight: 100,
                        protocol: BackendProtocol::Http1,
                        scheme: BackendScheme::Http,
                    }],
                    load_balancing: LoadBalancing::RoundRobin,
                },
                Route {
                    path: "/".to_string(),
                    backends: vec![Backend {
                        kind: BackendKind::Direct,
                        addr: "127.0.0.1:8080".to_string(),
                        client_name: None,
                        weight: 100,
                        protocol: BackendProtocol::Http1,
                        scheme: BackendScheme::Http,
                    }],
                    load_balancing: LoadBalancing::RoundRobin,
                },
            ],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        table.add_rule(rule);

        // /api/users should match /api route
        let result = table.match_http_request("example.com", "/api/users").await;
        assert!(result.is_some());
        let (_, route, backend) = result.unwrap();
        assert_eq!(route.path, "/api");
        assert_eq!(backend.addr, "127.0.0.1:8081");

        // /index.html should match / route
        let result = table.match_http_request("example.com", "/index.html").await;
        assert!(result.is_some());
        let (_, route, backend) = result.unwrap();
        assert_eq!(route.path, "/");
        assert_eq!(backend.addr, "127.0.0.1:8080");
    }

    #[tokio::test]
    async fn test_route_table_disabled_rule() {
        let mut table = RouteTable::new();
        let rule = ProxyRule {
            id: "rule1".to_string(),
            name: "Test Rule".to_string(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".to_string(),
            domains: vec!["example.com".to_string()],
            routes: vec![Route {
                path: "/".to_string(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: "127.0.0.1:8080".to_string(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::RoundRobin,
            }],
            tls: None,
            enabled: false, // Disabled
            created_at: None,
            cert_status: None,
        };
        table.add_rule(rule);

        let result = table.match_http_request("example.com", "/").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_route_table_tcp_rule() {
        let mut table = RouteTable::new();
        let rule = ProxyRule {
            id: "tcp1".to_string(),
            name: "TCP Rule".to_string(),
            rule_type: RuleType::Tcp,
            listen: "0.0.0.0:3306".to_string(),
            domains: vec![],
            routes: vec![Route {
                path: "/".to_string(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: "127.0.0.1:3306".to_string(),
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
        table.add_rule(rule);

        let result = table.match_tcp_rule("0.0.0.0:3306");
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, "tcp1");

        let result = table.match_tcp_rule("0.0.0.0:5432");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_route_table_multiple_domains() {
        let mut table = RouteTable::new();
        let rule = ProxyRule {
            id: "rule1".to_string(),
            name: "Multi Domain Rule".to_string(),
            rule_type: RuleType::Http,
            listen: "0.0.0.0:80".to_string(),
            domains: vec!["example.com".to_string(), "www.example.com".to_string()],
            routes: vec![Route {
                path: "/".to_string(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: "127.0.0.1:8080".to_string(),
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
        table.add_rule(rule);

        assert!(table.match_http_request("example.com", "/").await.is_some());
        assert!(table
            .match_http_request("www.example.com", "/")
            .await
            .is_some());
        assert!(table.match_http_request("other.com", "/").await.is_none());
    }
}
