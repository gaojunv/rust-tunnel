use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tracing::info;

use crate::control_plane::client_registry::ClientRegistry;
use crate::db::Database;
use crate::dns::registry::DnsRegistry;
use crate::dynamic_config::DynamicConfig;
use crate::mesh::MeshManager;
use crate::reverse_proxy::ReverseProxyState;
use rust_tunnel_common::ControlMessage;

use super::acme_config::{AcmeConfigInfo, AcmeFullConfig};
use super::port_info::{ClientInfo, PortInfo as ServerPortInfo, TrojanRuntimeStatus};

/// Sender for control messages - can be shared across tasks
pub type ControlMessageSender = mpsc::Sender<ControlMessage>;

/// ACME 证书生命周期状态（client / 配置 / 证书管理器 / DNS provider）。
/// 领域切片：原 ServerState 的 5 个 acme_* 扁平字段归并于此。
#[derive(Clone, Default)]
pub struct AcmeState {
    /// ACME client for certificate management (set when ACME is enabled)
    pub client: Arc<RwLock<Option<std::sync::Arc<crate::acme::client::AcmeClient>>>>,
    /// ACME configuration info (set when ACME is enabled)
    pub config: Arc<RwLock<Option<AcmeConfigInfo>>>,
    /// Full ACME configuration for API access
    pub full_config: Arc<RwLock<AcmeFullConfig>>,
    /// Certificate manager for TLS certificate lifecycle (set when ACME is enabled)
    pub cert_manager: Option<std::sync::Arc<crate::acme::manager::CertificateManager>>,
    /// DNS provider configuration for ACME DNS-01 challenges
    pub dns_provider: Arc<RwLock<Option<crate::acme::dns::DnsProviderConfig>>>,
}

/// API/控制通道 TLS 只读设置（来自 config，运行期不变）。
/// 领域切片：原 ServerState 的 api_tls/api_domain/tls_cert_path/tls_key_path 归并于此。
#[derive(Debug, Clone)]
pub struct TlsSettings {
    /// Whether API TLS is enabled (read-only, from config)
    pub enabled: bool,
    /// API domain for TLS certificate (read-only, from config)
    pub domain: Option<String>,
    /// 控制通道 TLS 证书路径（Trojan 自签名回退复用，read-only，来自 config）
    pub cert_path: String,
    /// 控制通道 TLS 私钥路径
    pub key_path: String,
}

impl Default for TlsSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            domain: None,
            cert_path: "./data/tls/cert.pem".to_string(),
            key_path: "./data/tls/key.pem".to_string(),
        }
    }
}

/// SS/Trojan 端口表与监听器运行时（端口注册、连接计数、监听生命周期）。
/// 领域切片：原 ServerState 的 3 个私有端口字段 + 3 个监听器字段归并于此。
#[derive(Clone, Default)]
pub struct ProxyPortsState {
    /// Map from port to port info (Shadowsocks or Trojan)
    ports: Arc<Mutex<HashMap<u16, ServerPortInfo>>>,
    /// Active Shadowsocks connections per port
    ss_connections: Arc<Mutex<HashMap<u16, usize>>>,
    /// Active Trojan connections per port
    trojan_connections: Arc<Mutex<HashMap<u16, usize>>>,
    /// Shadowsocks listener abort handle
    pub ss_listener_abort: Arc<RwLock<Option<tokio::sync::watch::Sender<bool>>>>,
    /// Trojan listener abort handle
    pub trojan_listener_abort: Arc<RwLock<Option<tokio::sync::watch::Sender<bool>>>>,
    /// Trojan 运行时状态（证书来源、共享模式）
    pub trojan_runtime: Arc<RwLock<TrojanRuntimeStatus>>,
}

impl ProxyPortsState {
    /// 注册 Shadowsocks 端口；端口已被占用（任一协议）时返回 false
    pub async fn register_shadowsocks(&self, port: u16, cipher: String, password: String) -> bool {
        let mut ports = self.ports.lock().await;
        if ports.contains_key(&port) {
            return false;
        }
        ports.insert(
            port,
            ServerPortInfo::Shadowsocks {
                port,
                cipher,
                password,
                enabled: true,
                created_at: chrono::Utc::now().timestamp(),
            },
        );
        true
    }

    /// 查询端口信息
    pub async fn get_port(&self, port: u16) -> Option<ServerPortInfo> {
        let ports = self.ports.lock().await;
        ports.get(&port).cloned()
    }

    /// 注销端口；端口存在时返回 true
    pub async fn unregister_port(&self, port: u16) -> bool {
        let mut ports = self.ports.lock().await;
        ports.remove(&port).is_some()
    }

    /// Get the number of active connections for a specific port (SS/Trojan only)
    pub async fn get_connection_count_for_port(&self, remote_port: u16) -> usize {
        let ss_connections = self.ss_connections.lock().await;
        let ss_count = ss_connections.get(&remote_port).copied().unwrap_or(0);

        let trojan_connections = self.trojan_connections.lock().await;
        let trojan_count = trojan_connections.get(&remote_port).copied().unwrap_or(0);

        ss_count + trojan_count
    }

    /// Increment active Shadowsocks connections for a port
    pub async fn increment_ss_connections(&self, port: u16) {
        let mut ss_connections = self.ss_connections.lock().await;
        *ss_connections.entry(port).or_insert(0) += 1;
    }

    /// Decrement active Shadowsocks connections for a port
    pub async fn decrement_ss_connections(&self, port: u16) {
        let mut ss_connections = self.ss_connections.lock().await;
        if let Some(count) = ss_connections.get_mut(&port) {
            if *count > 0 {
                *count -= 1;
            }
        }
    }

    /// 注册 Trojan 端口；端口已被占用（任一协议）时返回 false
    pub async fn register_trojan(&self, port: u16, password: String, fallback: String) -> bool {
        let mut ports = self.ports.lock().await;
        if ports.contains_key(&port) {
            return false;
        }
        ports.insert(
            port,
            ServerPortInfo::Trojan {
                port,
                password,
                fallback,
                enabled: true,
                created_at: chrono::Utc::now().timestamp(),
            },
        );
        true
    }

    /// Get all Trojan ports
    pub async fn get_trojan_ports(&self) -> Vec<u16> {
        let ports = self.ports.lock().await;
        ports
            .iter()
            .filter_map(|(port, info)| match info {
                ServerPortInfo::Trojan { .. } => Some(*port),
                _ => None,
            })
            .collect()
    }

    /// Check if a port is a Trojan port
    pub async fn is_trojan_port(&self, port: u16) -> bool {
        let ports = self.ports.lock().await;
        matches!(ports.get(&port), Some(ServerPortInfo::Trojan { .. }))
    }

    /// Increment active Trojan connections for a port
    pub async fn increment_trojan_connections(&self, port: u16) {
        let mut trojan_connections = self.trojan_connections.lock().await;
        *trojan_connections.entry(port).or_insert(0) += 1;
    }

    /// Decrement active Trojan connections for a port
    pub async fn decrement_trojan_connections(&self, port: u16) {
        let mut trojan_connections = self.trojan_connections.lock().await;
        if let Some(count) = trojan_connections.get_mut(&port) {
            if *count > 0 {
                *count -= 1;
            }
        }
    }

    /// Get total active connection count (SS + Trojan only)
    pub async fn get_active_connection_count(&self) -> usize {
        let ss_connections = self.ss_connections.lock().await;
        let trojan_connections = self.trojan_connections.lock().await;
        ss_connections.values().sum::<usize>() + trojan_connections.values().sum::<usize>()
    }

    /// Get all Shadowsocks ports
    pub async fn get_shadowsocks_ports(&self) -> Vec<u16> {
        let ports = self.ports.lock().await;
        ports
            .iter()
            .filter_map(|(port, info)| match info {
                ServerPortInfo::Shadowsocks { .. } => Some(*port),
                _ => None,
            })
            .collect()
    }

    /// Check if a port is a Shadowsocks port
    pub async fn is_shadowsocks_port(&self, port: u16) -> bool {
        let ports = self.ports.lock().await;
        matches!(ports.get(&port), Some(ServerPortInfo::Shadowsocks { .. }))
    }
}

/// Global server state shared between all tasks
#[derive(Clone)]
pub struct ServerState {
    /// SS/Trojan 端口与监听器运行时（领域切片）
    pub proxy_ports: ProxyPortsState,
    /// Unified statistics collector (traffic / connections for all entity types)
    pub stats_collector: crate::stats::StatsCollector,
    /// Database connection (optional)
    db: Option<Database>,
    /// Log store for capturing and broadcasting logs
    pub log_store: Option<crate::logs::LogStore>,
    /// Mesh network manager
    pub mesh_manager: MeshManager,
    /// DNS registry (set when DNS server is enabled)
    pub dns_registry: Option<DnsRegistry>,
    /// Reverse proxy state
    pub proxy_state: ReverseProxyState,
    /// LLM Gateway state (set when LLM is configured). 由装配层 init_llm_state 注入。
    pub llm_state: std::sync::Arc<tokio::sync::RwLock<Option<std::sync::Arc<crate::llm::LlmState>>>>,
    /// Client registry (spec §2.6)
    pub client_registry: Option<ClientRegistry>,
    /// AI agent workbench state (workspace execution locks, DB access)
    pub agent_state: Option<crate::agent::AgentState>,
    /// ACME 证书生命周期（领域切片）
    pub acme: AcmeState,
    /// Dynamic configuration (DB-backed, runtime-changeable)
    pub dynamic_config: Arc<RwLock<DynamicConfig>>,
    /// API/控制通道 TLS 只读设置（领域切片）
    pub tls: TlsSettings,
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerState {
    /// Create a new server state without database (for backwards compatibility)
    pub fn new() -> Self {
        let mut state = Self {
            proxy_ports: ProxyPortsState::default(),
            stats_collector: crate::stats::StatsCollector::new(None),
            db: None,
            log_store: None,
            mesh_manager: MeshManager::new(),
            dns_registry: None,
            proxy_state: ReverseProxyState::new(),
            llm_state: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            client_registry: None,
            agent_state: None,
            acme: AcmeState::default(),
            dynamic_config: Arc::new(RwLock::new(DynamicConfig {
                log_level: "info".to_string(),
                llm_request_logging: true,
                ss: None,
                trojan: None,
                reverse_proxy: crate::dynamic_config::ReverseProxySettings {
                    max_connections: 10000,
                    connection_timeout_secs: 30,
                    buffer_size: 8192,
                },
                dns: crate::dynamic_config::DnsSettings {
                    tunnel_domain: "tunnel.local".to_string(),
                    mesh_domain: "mesh.local".to_string(),
                },
            })),
            tls: TlsSettings::default(),
        };
        // 反代状态挂上同一个统计采集器：proxy 埋点才能汇总到 /api/stats/*
        state
            .proxy_state
            .set_stats_collector(state.stats_collector.clone());
        state
    }

    /// Create a new server state with database
    pub fn with_db(db: Database) -> Self {
        let registry = ClientRegistry::new(db.clone());
        let mut state = Self {
            proxy_ports: ProxyPortsState::default(),
            stats_collector: crate::stats::StatsCollector::new(Some(db.clone())),
            db: Some(db.clone()),
            log_store: Some(crate::logs::LogStore::new(Some(db.clone()))),
            mesh_manager: MeshManager::new(),
            dns_registry: None,
            proxy_state: ReverseProxyState::with_db(db.clone()),
            llm_state: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            client_registry: Some(registry.clone()),
            agent_state: Some(crate::agent::AgentState::new(std::sync::Arc::new(registry), db)),
            acme: AcmeState::default(),
            dynamic_config: Arc::new(RwLock::new(DynamicConfig {
                log_level: "info".to_string(),
                llm_request_logging: true,
                ss: None,
                trojan: None,
                reverse_proxy: crate::dynamic_config::ReverseProxySettings {
                    max_connections: 10000,
                    connection_timeout_secs: 30,
                    buffer_size: 8192,
                },
                dns: crate::dynamic_config::DnsSettings {
                    tunnel_domain: "tunnel.local".to_string(),
                    mesh_domain: "mesh.local".to_string(),
                },
            })),
            tls: TlsSettings::default(),
        };
        // ClientRegistry 挂上统计采集器：open_tunnel 的连接数/流量埋点依赖它
        if let Some(registry) = state.client_registry.as_mut() {
            registry.set_stats_collector(state.stats_collector.clone());
        }
        // 反代状态挂上同一个统计采集器：proxy 埋点才能汇总到 /api/stats/*
        state
            .proxy_state
            .set_stats_collector(state.stats_collector.clone());
        state
    }

    /// Wire up the ClientConnector to ReverseProxyState after the server
    /// has started. Called from `bin/server.rs` after `with_db()`.
    pub async fn wire_up_client_connector(&self) {
        if let Some(registry) = &self.client_registry {
            let opener: std::sync::Arc<dyn rust_tunnel_protocols::TunnelOpener> = std::sync::Arc::new(registry.clone());
            let cc = std::sync::Arc::new(crate::reverse_proxy::connector::ClientConnector::new(
                opener,
            ));
            self.proxy_state.set_client_connector(cc).await;
            info!("ClientConnector registered into ReverseProxyState");
        }
    }

    /// Initialize the LlmState from the in-memory `__llm_gateway__` rule (if any).
    /// Called during server startup after rules are loaded from DB.
    pub async fn init_llm_state(
        &self,
        db: Option<Database>,
        master_key: Option<[u8; 32]>,
        #[cfg_attr(not(feature = "rag"), allow(unused_variables))] rag_data_dir: &std::path::Path,
        dynamic_config: std::sync::Arc<tokio::sync::RwLock<DynamicConfig>>,
    ) {
        // 装配层职责：反代规则 → llm 自有输入类型；DynamicConfig → 热路径开关初值
        let gateway_rule = {
            let rules = self.proxy_state.rules.lock().await;
            rules.get("__llm_gateway__").cloned()
        }
        .map(|rule| crate::llm::LlmGatewayRuleInput {
            enabled: rule.enabled,
            domains: rule.domains.clone(),
            listen: rule.listen.clone(),
            tls_enabled: rule.tls.as_ref().is_some_and(|t| t.enabled),
            tls_acme: rule.tls.as_ref().is_some_and(|t| t.acme),
        });
        let request_logging = dynamic_config.read().await.llm_request_logging;
        let llm = crate::llm::init_llm_state(
            gateway_rule,
            db,
            master_key,
            rag_data_dir,
            request_logging,
        )
        .await;
        *self.llm_state.write().await = Some(llm.clone());
        // 同时注入反代分流器
        *self.proxy_state.llm_dispatcher.write().await =
            Some(std::sync::Arc::new(LlmDispatcherAdapter::new(llm)));
    }

    pub async fn register_shadowsocks(&self, port: u16, cipher: String, password: String) -> bool {
        self.proxy_ports.register_shadowsocks(port, cipher, password).await
    }

    pub async fn get_port(&self, port: u16) -> Option<ServerPortInfo> {
        self.proxy_ports.get_port(port).await
    }

    pub async fn unregister_port(&self, port: u16) -> bool {
        self.proxy_ports.unregister_port(port).await
    }

    /// Get the number of active connections for a specific port (SS/Trojan only)
    pub async fn get_connection_count_for_port(&self, remote_port: u16) -> usize {
        self.proxy_ports.get_connection_count_for_port(remote_port).await
    }

    /// Increment active Shadowsocks connections for a port
    pub async fn increment_ss_connections(&self, port: u16) {
        self.proxy_ports.increment_ss_connections(port).await;
    }

    /// Decrement active Shadowsocks connections for a port
    pub async fn decrement_ss_connections(&self, port: u16) {
        self.proxy_ports.decrement_ss_connections(port).await;
    }

    pub async fn register_trojan(&self, port: u16, password: String, fallback: String) -> bool {
        self.proxy_ports.register_trojan(port, password, fallback).await
    }

    /// Get all Trojan ports
    pub async fn get_trojan_ports(&self) -> Vec<u16> {
        self.proxy_ports.get_trojan_ports().await
    }

    /// Check if a port is a Trojan port
    pub async fn is_trojan_port(&self, port: u16) -> bool {
        self.proxy_ports.is_trojan_port(port).await
    }

    /// Increment active Trojan connections for a port
    pub async fn increment_trojan_connections(&self, port: u16) {
        self.proxy_ports.increment_trojan_connections(port).await;
    }

    /// Decrement active Trojan connections for a port
    pub async fn decrement_trojan_connections(&self, port: u16) {
        self.proxy_ports.decrement_trojan_connections(port).await;
    }

    // API helper methods — kept for backward compat, return empty data since
    // tunnel-forward clients are now managed via ClientRegistry.

    /// Get all tunnel-forward clients (deprecated, returns empty)
    pub async fn get_all_clients(&self) -> Vec<(u16, ClientInfo)> {
        Vec::new()
    }

    /// Get tunnel-forward client count (deprecated, returns 0)
    pub async fn get_client_count(&self) -> usize {
        if let Some(ref registry) = self.client_registry {
            registry.list_online().await.len()
        } else {
            0
        }
    }

    /// Get total active connection count (SS + Trojan only)
    pub async fn get_active_connection_count(&self) -> usize {
        self.proxy_ports.get_active_connection_count().await
    }

    /// Get all Shadowsocks ports
    pub async fn get_shadowsocks_ports(&self) -> Vec<u16> {
        self.proxy_ports.get_shadowsocks_ports().await
    }

    /// Check if a port is a Shadowsocks port
    pub async fn is_shadowsocks_port(&self, port: u16) -> bool {
        self.proxy_ports.is_shadowsocks_port(port).await
    }

    /// Set the ACME client for this server state
    pub async fn set_acme_client(
        &self,
        client: std::sync::Arc<crate::acme::client::AcmeClient>,
        config: AcmeConfigInfo,
    ) {
        *self.acme.client.write().await = Some(client);
        *self.acme.config.write().await = Some(config);
    }

    /// Set the certificate manager for this server state
    pub fn set_cert_manager(
        &mut self,
        manager: std::sync::Arc<crate::acme::manager::CertificateManager>,
    ) {
        self.acme.cert_manager = Some(manager);
    }

    /// Set the DNS registry for this server state
    pub fn set_dns_registry(&mut self, registry: DnsRegistry) {
        self.dns_registry = Some(registry);
    }

    /// Get a reference to the database (if available)
    pub fn get_db(&self) -> Option<&Database> {
        self.db.as_ref()
    }

    /// Get database reference (if available)
    pub fn db(&self) -> Option<&Database> {
        self.db.as_ref()
    }

    /// Set the dynamic config (called after DB initialization)
    pub async fn set_dynamic_config(&self, config: DynamicConfig) {
        let mut dc = self.dynamic_config.write().await;
        *dc = config;
    }
}

// ── PortRegistry impl — bridges ServerState to protocols crate ──
#[async_trait::async_trait]
impl rust_tunnel_protocols::PortRegistry for ServerState {
    async fn register_shadowsocks(&self, port: u16, cipher: String, password: String) -> bool {
        ServerState::register_shadowsocks(self, port, cipher, password).await
    }
    async fn register_trojan(&self, port: u16, password: String, fallback: String) -> bool {
        ServerState::register_trojan(self, port, password, fallback).await
    }
    async fn get_port(&self, port: u16) -> Option<rust_tunnel_protocols::PortInfo> {
        let ports = self.proxy_ports.ports.lock().await;
        let p = ports.get(&port)?.clone();
        Some(match p {
            crate::control_plane::PortInfo::Shadowsocks { port, cipher, password, enabled, created_at } => rust_tunnel_protocols::PortInfo::Shadowsocks { port, cipher, password, enabled, created_at },
            crate::control_plane::PortInfo::Trojan { port, password, fallback, enabled, created_at } => rust_tunnel_protocols::PortInfo::Trojan { port, password, fallback, enabled, created_at },
        })
    }
    async fn unregister_port(&self, port: u16) -> bool { ServerState::unregister_port(self, port).await }
    async fn get_connection_count_for_port(&self, remote_port: u16) -> usize { ServerState::get_connection_count_for_port(self, remote_port).await }
    async fn increment_ss_connections(&self, port: u16) { ServerState::increment_ss_connections(self, port).await }
    async fn decrement_ss_connections(&self, port: u16) { ServerState::decrement_ss_connections(self, port).await }
    async fn increment_trojan_connections(&self, port: u16) { ServerState::increment_trojan_connections(self, port).await }
    async fn decrement_trojan_connections(&self, port: u16) { ServerState::decrement_trojan_connections(self, port).await }
}

impl std::fmt::Debug for ServerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerState").finish_non_exhaustive()
    }
}

/// LLM Gateway 分流适配器：protocols 的 [`LlmDispatcher`] trait ← llm crate 的
/// `llm_handle` 自由函数。装配层拥有此适配，llm 不依赖 protocols 类型。
struct LlmDispatcherAdapter {
    llm: std::sync::Arc<crate::llm::LlmState>,
}

impl LlmDispatcherAdapter {
    fn new(llm: std::sync::Arc<crate::llm::LlmState>) -> Self {
        Self { llm }
    }
}

impl crate::reverse_proxy::llm_dispatch::LlmDispatcher for LlmDispatcherAdapter {
    fn try_handle(
        self: std::sync::Arc<Self>,
        host: String,
        req: axum::http::Request<axum::body::Body>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<axum::response::Response, axum::http::Request<axum::body::Body>>,
                > + Send,
        >,
    > {
        Box::pin(async move {
            let protocol = {
                let cfg = self.llm.gateway_config.read().await;
                cfg.as_ref().and_then(|c| c.match_protocol(&host))
            };
            let Some(protocol) = protocol else {
                return Err(req);
            };
            Ok(crate::llm::llm_handle(self.llm.clone(), protocol, req).await)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_state_new() {
        let state = ServerState::new();
        assert_eq!(state.get_client_count().await, 0);
        assert_eq!(state.get_active_connection_count().await, 0);
    }

    #[tokio::test]
    async fn test_server_state_clone() {
        let state = ServerState::new();
        let cloned = state.clone();
        assert_eq!(cloned.get_client_count().await, 0);
    }
}

#[cfg(test)]
mod ss_integration_tests {
    use super::*;

    #[tokio::test]
    async fn test_register_shadowsocks_and_trojan() {
        let state = ServerState::new();

        // Register a shadowsocks port
        assert!(
            state
                .register_shadowsocks(8388, "aes-256-gcm".to_string(), "test-pass".to_string())
                .await
        );

        // Verify SS registered
        let ss_ports = state.get_shadowsocks_ports().await;
        assert_eq!(ss_ports.len(), 1);
        assert_eq!(ss_ports[0], 8388);
        assert!(state.is_shadowsocks_port(8388).await);

        // Register a Trojan port
        assert!(
            state
                .register_trojan(443, "tj-pass".to_string(), "127.0.0.1:80".to_string())
                .await
        );

        let trojan_ports = state.get_trojan_ports().await;
        assert_eq!(trojan_ports.len(), 1);
        assert_eq!(trojan_ports[0], 443);
        assert!(state.is_trojan_port(443).await);

        // Both ports should be accessible via get_port
        assert!(state.get_port(8388).await.is_some());
        assert!(state.get_port(443).await.is_some());

        // Port not registered
        assert!(!state.is_shadowsocks_port(443).await);
        assert!(!state.is_trojan_port(8388).await);
    }

    #[tokio::test]
    async fn test_unregister_port() {
        let state = ServerState::new();

        state
            .register_shadowsocks(9003, "aes-256-gcm".to_string(), "pass".to_string())
            .await;

        assert!(state.get_port(9003).await.is_some());

        assert!(state.unregister_port(9003).await);
        assert!(state.get_port(9003).await.is_none());

        // Unregister non-existent
        assert!(!state.unregister_port(9999).await);
    }

    #[tokio::test]
    async fn test_cannot_register_duplicate_shadowsocks() {
        let state = ServerState::new();

        assert!(
            state
                .register_shadowsocks(8080, "aes-256-gcm".into(), "pass".into())
                .await
        );
        // Cannot register SS on same port
        assert!(
            !state
                .register_shadowsocks(8080, "chacha20-ietf-poly1305".into(), "other".into())
                .await
        );
    }

    #[tokio::test]
    async fn test_connection_count_for_port() {
        let state = ServerState::new();
        assert_eq!(state.get_connection_count_for_port(9999).await, 0);
    }

    #[tokio::test]
    async fn test_ss_trojan_connection_counts() {
        let state = ServerState::new();

        state.increment_ss_connections(8388).await;
        state.increment_ss_connections(8388).await;
        assert_eq!(state.get_connection_count_for_port(8388).await, 2);

        state.increment_trojan_connections(443).await;
        assert_eq!(state.get_connection_count_for_port(443).await, 1);

        state.decrement_ss_connections(8388).await;
        assert_eq!(state.get_connection_count_for_port(8388).await, 1);

        state.decrement_ss_connections(8388).await;
        assert_eq!(state.get_connection_count_for_port(8388).await, 0);

        // Decrement below zero should be safe
        state.decrement_ss_connections(8388).await;
        assert_eq!(state.get_connection_count_for_port(8388).await, 0);
    }

    #[tokio::test]
    async fn test_active_connection_count() {
        let state = ServerState::new();
        assert_eq!(state.get_active_connection_count().await, 0);

        state.increment_ss_connections(8388).await;
        state.increment_trojan_connections(443).await;
        state.increment_trojan_connections(443).await;
        assert_eq!(state.get_active_connection_count().await, 3);
    }
}

#[cfg(test)]
mod acme_config_load_or_seed_tests {
    use super::*;
    use crate::db::Database;

    async fn fresh_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn fresh_db_uses_cli_values_and_writes_back() {
        let (db, _tmp) = fresh_db().await;

        let cfg = crate::config::ServerConfig {
            acme_enabled: true,
            acme_server_url: "https://example.test/acme".to_string(),
            acme_email: Some("op@example.test".to_string()),
            acme_cert_dir: "/tmp/certs".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 12,
            acme_renewal_days_before_expiry: 15,
            ..Default::default()
        };

        let out = AcmeFullConfig::load_or_seed(&db, &cfg).await;

        // Returned value comes from seed args
        assert!(out.enabled);
        assert_eq!(out.server_url, "https://example.test/acme");
        assert_eq!(out.email.as_deref(), Some("op@example.test"));
        assert_eq!(out.cert_dir, "/tmp/certs");
        assert!(out.auto_renew);
        assert_eq!(out.renewal_check_interval, 12);
        assert_eq!(out.renewal_days_before_expiry, 15);
        assert!(!out.tos_agreed); // no certs in fresh DB → no inference

        // DB row now contains the seed
        let json = db
            .load_server_setting("acme_config")
            .await
            .unwrap()
            .expect("acme_config row should exist after seed");
        let stored: AcmeFullConfig = serde_json::from_str(&json).unwrap();
        assert!(stored.enabled);
        assert_eq!(stored.server_url, "https://example.test/acme");
        assert!(!stored.tos_agreed);
    }

    #[tokio::test]
    async fn existing_db_returns_stored_values_ignoring_seed() {
        let (db, _tmp) = fresh_db().await;

        // Pre-populate DB with a distinctive config
        let stored = AcmeFullConfig {
            enabled: true,
            server_url: "https://stored.example/acme".to_string(),
            email: Some("stored@example.test".to_string()),
            cert_dir: "/stored/certs".to_string(),
            auto_renew: false,
            renewal_check_interval: 99,
            renewal_days_before_expiry: 77,
            tos_agreed: true,
        };
        db.save_server_setting("acme_config", &serde_json::to_string(&stored).unwrap())
            .await
            .unwrap();

        // Call with completely different seed args — none of them should leak through
        let cfg = crate::config::ServerConfig {
            acme_enabled: false, // different from stored
            acme_server_url: "https://cli.example/acme".to_string(),
            acme_email: Some("cli@example.test".to_string()),
            acme_cert_dir: "/cli/certs".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 1,
            acme_renewal_days_before_expiry: 2,
            ..Default::default()
        };

        let out = AcmeFullConfig::load_or_seed(&db, &cfg).await;

        assert!(out.enabled);
        assert_eq!(out.server_url, "https://stored.example/acme");
        assert_eq!(out.email.as_deref(), Some("stored@example.test"));
        assert_eq!(out.cert_dir, "/stored/certs");
        assert!(!out.auto_renew);
        assert_eq!(out.renewal_check_interval, 99);
        assert_eq!(out.renewal_days_before_expiry, 77);
        assert!(out.tos_agreed);
    }

    #[tokio::test]
    async fn seed_never_overwrites_existing_db() {
        let (db, _tmp) = fresh_db().await;

        // First call: seed DB with initial values
        let first_cfg = crate::config::ServerConfig {
            acme_enabled: true,
            acme_server_url: "https://first.example/acme".to_string(),
            acme_cert_dir: "/first".to_string(),
            ..Default::default()
        };
        AcmeFullConfig::load_or_seed(&db, &first_cfg).await;

        // Second call with different seed args — should not touch DB
        let second_cfg = crate::config::ServerConfig {
            acme_enabled: false,
            acme_server_url: "https://second.example/acme".to_string(),
            acme_email: Some("x@y".to_string()),
            acme_cert_dir: "/second".to_string(),
            acme_auto_renew: false,
            acme_renewal_check_interval: 1,
            acme_renewal_days_before_expiry: 1,
            ..Default::default()
        };
        AcmeFullConfig::load_or_seed(&db, &second_cfg).await;

        // DB row must still be the first seed
        let json = db
            .load_server_setting("acme_config")
            .await
            .unwrap()
            .unwrap();
        let stored: AcmeFullConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(stored.server_url, "https://first.example/acme");
        assert_eq!(stored.cert_dir, "/first");
    }

    #[tokio::test]
    async fn malformed_json_falls_back_to_seed_without_overwriting_db() {
        let (db, _tmp) = fresh_db().await;

        // Poison the row with unparseable JSON
        db.save_server_setting("acme_config", "not valid json {[")
            .await
            .unwrap();

        let cfg = crate::config::ServerConfig {
            acme_enabled: true,
            acme_server_url: "https://seed.example/acme".to_string(),
            acme_cert_dir: "/seed".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 24,
            acme_renewal_days_before_expiry: 30,
            ..Default::default()
        };

        let out = AcmeFullConfig::load_or_seed(&db, &cfg).await;

        // Returned value comes from seed args
        assert!(out.enabled);
        assert_eq!(out.server_url, "https://seed.example/acme");
        assert_eq!(out.cert_dir, "/seed");

        // But DB row is untouched — operator can inspect / repair it
        let stored = db
            .load_server_setting("acme_config")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, "not valid json {[");
    }

    #[tokio::test]
    async fn tos_agreed_inferred_from_existing_certs_on_first_seed() {
        let (db, _tmp) = fresh_db().await;

        // Simulate an upgrade path: certs exist in DB (from a previous
        // version that only used CLI-based ACME) but acme_config row
        // was never written.
        db.save_acme_certificate(
            "example.test",
            "active",
            Some("-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----"),
            Some("-----BEGIN PRIVATE KEY-----\nfake\n-----END PRIVATE KEY-----"),
            None,
            Some(chrono::Utc::now()),
            Some(chrono::Utc::now() + chrono::Duration::days(90)),
            true,
        )
        .await
        .unwrap();

        let cfg = crate::config::ServerConfig {
            acme_enabled: true,
            acme_server_url: "https://seed.example/acme".to_string(),
            acme_cert_dir: "/seed".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 24,
            acme_renewal_days_before_expiry: 30,
            ..Default::default()
        };

        let out = AcmeFullConfig::load_or_seed(&db, &cfg).await;

        assert!(
            out.tos_agreed,
            "existing certs should imply ToS was previously agreed"
        );

        // And the persisted seed carries it forward
        let stored: AcmeFullConfig = serde_json::from_str(
            &db.load_server_setting("acme_config")
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(stored.tos_agreed);
    }

    #[tokio::test]
    async fn tos_agreed_stays_false_when_no_certs_exist() {
        let (db, _tmp) = fresh_db().await;

        let cfg = crate::config::ServerConfig {
            acme_enabled: true,
            acme_server_url: "https://seed.example/acme".to_string(),
            acme_cert_dir: "/seed".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 24,
            acme_renewal_days_before_expiry: 30,
            ..Default::default()
        };

        let out = AcmeFullConfig::load_or_seed(&db, &cfg).await;

        assert!(!out.tos_agreed, "empty DB should not infer ToS agreement");

        // And the persisted seed also has tos_agreed=false
        let stored: AcmeFullConfig = serde_json::from_str(
            &db.load_server_setting("acme_config")
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(!stored.tos_agreed);
    }

    #[tokio::test]
    async fn tos_inference_does_not_run_when_db_already_has_config() {
        let (db, _tmp) = fresh_db().await;

        // First call: seed with no certs → tos_agreed=false persisted
        let cfg = crate::config::ServerConfig {
            acme_enabled: false,
            acme_server_url: "https://a.example/acme".to_string(),
            acme_cert_dir: "/a".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 24,
            acme_renewal_days_before_expiry: 30,
            ..Default::default()
        };

        let first = AcmeFullConfig::load_or_seed(&db, &cfg).await;
        assert!(!first.tos_agreed);

        // Now add certs after the first seed. If inference re-ran on the
        // second call, it would flip tos_agreed to true.
        db.save_acme_certificate(
            "example.test",
            "active",
            Some("cert"),
            Some("key"),
            None,
            Some(chrono::Utc::now()),
            Some(chrono::Utc::now() + chrono::Duration::days(90)),
            true,
        )
        .await
        .unwrap();

        // Second call should read the persisted config (tos_agreed=false)
        // via the DB-hit fast path and NOT re-run inference.
        let second = AcmeFullConfig::load_or_seed(&db, &cfg).await;
        assert!(
            !second.tos_agreed,
            "inference must not overwrite persisted tos_agreed=false — DB row is authoritative"
        );
    }
}
