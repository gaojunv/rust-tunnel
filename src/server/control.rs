use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use crate::common::{
    create_server_config, load_or_generate_cert, ControlMessage, TunnelError, TunnelResult,
};
use crate::server::api::TrafficStore;
use crate::server::db::Database;
use crate::server::dns::registry::DnsRegistry;
use crate::server::dynamic_config::DynamicConfig;
use crate::server::mesh::MeshManager;
use crate::server::reverse_proxy::ReverseProxyState;
use crate::server::quality::{
    calculate_quality_score, check_warnings, ConnectionQuality, QualitySample, QualityStore,
    QualityThresholds, QualityTracker,
};
use crate::server::{listener, ServerConfig};
use chrono::{Timelike, Utc};

/// Sender for control messages - can be shared across tasks
pub type ControlMessageSender = mpsc::Sender<ControlMessage>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    Tunnel,
    Shadowsocks,
    Trojan,
}

/// Information about a connected client
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Remote port that client wants to expose
    pub remote_port: u16,
    /// Hostname of the client machine (optional)
    pub hostname: Option<String>,
    /// Sender for sending messages to client via control channel
    pub control_sender: ControlMessageSender,
}

#[derive(Debug, Clone)]
pub enum PortInfo {
    Tunnel(ClientInfo),
    Shadowsocks {
        port: u16,
        cipher: String,
        password: String,
        enabled: bool,
        created_at: i64,
    },
    Trojan {
        port: u16,
        password: String,
        fallback: String,
        enabled: bool,
        created_at: i64,
    },
}

impl PortInfo {
    pub fn port_type(&self) -> PortType {
        match self {
            PortInfo::Tunnel(_) => PortType::Tunnel,
            PortInfo::Shadowsocks { .. } => PortType::Shadowsocks,
            PortInfo::Trojan { .. } => PortType::Trojan,
        }
    }

    pub fn port(&self) -> u16 {
        match self {
            PortInfo::Tunnel(info) => info.remote_port,
            PortInfo::Shadowsocks { port, .. } => *port,
            PortInfo::Trojan { port, .. } => *port,
        }
    }
}

/// Information about an active connection between user and client
struct ActiveConnection {
    /// User TCP stream writer half connected to client
    user_writer: Arc<Mutex<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>>,
    /// Remote port this connection belongs to
    remote_port: u16,
}

/// ACME configuration summary for API responses
#[derive(Debug, Clone)]
pub struct AcmeConfigInfo {
    pub enabled: bool,
    pub server_url: String,
    pub cert_dir: String,
}

/// Full ACME configuration for API access
#[derive(Debug, Clone)]
pub struct AcmeFullConfig {
    pub enabled: bool,
    pub server_url: String,
    pub email: Option<String>,
    pub cert_dir: String,
    pub auto_renew: bool,
    pub renewal_check_interval: u64,
    pub renewal_days_before_expiry: u64,
    pub tos_agreed: bool,
}

impl Default for AcmeFullConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            email: None,
            cert_dir: "./data/certs".to_string(),
            auto_renew: true,
            renewal_check_interval: 24,
            renewal_days_before_expiry: 30,
            tos_agreed: false,
        }
    }
}

/// Global server state shared between all tasks
#[derive(Clone)]
pub struct ServerState {
    /// Map from port to port info (Tunnel or Shadowsocks)
    ports: Arc<Mutex<HashMap<u16, PortInfo>>>,
    /// Map from connection_id to active tunnel connection info
    active_connections: Arc<Mutex<HashMap<u64, ActiveConnection>>>,
    /// Active Shadowsocks connections per port
    ss_active_connections: Arc<Mutex<HashMap<u16, usize>>>,
    /// Active Trojan connections per port
    trojan_active_connections: Arc<Mutex<HashMap<u16, usize>>>,
    /// Traffic statistics store
    pub traffic_store: TrafficStore,
    /// Database connection (optional)
    db: Option<Database>,
    /// Connection quality store
    pub quality_store: QualityStore,
    /// Quality trackers per port
    quality_trackers: Arc<Mutex<HashMap<u16, QualityTracker>>>,
    /// Log store for capturing and broadcasting logs
    pub log_store: Option<crate::server::logs::LogStore>,
    /// Mesh network manager
    pub mesh_manager: MeshManager,
    /// DNS registry (set when DNS server is enabled)
    pub dns_registry: Option<DnsRegistry>,
    /// Reverse proxy state
    pub proxy_state: ReverseProxyState,
    /// Active listener tasks (keyed by port) — aborted on client disconnect
    listener_tasks: Arc<Mutex<HashMap<u16, tokio::task::JoinHandle<()>>>>,
    /// ACME client for certificate management (set when ACME is enabled)
    pub acme_client: Option<std::sync::Arc<crate::server::acme::client::AcmeClient>>,
    /// ACME configuration info (set when ACME is enabled)
    pub acme_config: Option<AcmeConfigInfo>,
    /// Full ACME configuration for API access
    pub acme_full_config: Arc<RwLock<AcmeFullConfig>>,
    /// Certificate manager for TLS certificate lifecycle (set when ACME is enabled)
    pub cert_manager: Option<std::sync::Arc<crate::server::acme::manager::CertificateManager>>,
    /// DNS provider configuration for ACME DNS-01 challenges
    pub dns_provider_config:
        Arc<RwLock<Option<crate::server::acme::dns::DnsProviderConfig>>>,
    /// Dynamic configuration (DB-backed, runtime-changeable)
    pub dynamic_config: Arc<RwLock<DynamicConfig>>,
    /// Shadowsocks listener abort handle
    pub ss_listener_abort: Arc<RwLock<Option<tokio::sync::watch::Sender<bool>>>>,
    /// Trojan listener abort handle
    pub trojan_listener_abort: Arc<RwLock<Option<tokio::sync::watch::Sender<bool>>>>,
    /// Whether API TLS is enabled (read-only, from config)
    pub api_tls: bool,
    /// API domain for TLS certificate (read-only, from config)
    pub api_domain: Option<String>,
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerState {
    /// Create a new server state without database (for backwards compatibility)
    pub fn new() -> Self {
        Self {
            ports: Arc::new(Mutex::new(HashMap::new())),
            active_connections: Arc::new(Mutex::new(HashMap::new())),
            ss_active_connections: Arc::new(Mutex::new(HashMap::new())),
            trojan_active_connections: Arc::new(Mutex::new(HashMap::new())),
            traffic_store: TrafficStore::new(),
            quality_store: QualityStore::new(),
            quality_trackers: Arc::new(Mutex::new(HashMap::new())),
            db: None,
            log_store: None,
            mesh_manager: MeshManager::new(),
            dns_registry: None,
            proxy_state: ReverseProxyState::new(),
            listener_tasks: Arc::new(Mutex::new(HashMap::new())),
            acme_client: None,
            acme_config: None,
            acme_full_config: Arc::new(RwLock::new(AcmeFullConfig::default())),
            cert_manager: None,
            dns_provider_config: Arc::new(RwLock::new(None)),
            dynamic_config: Arc::new(RwLock::new(DynamicConfig {
                log_level: "info".to_string(),
                ss: None,
                trojan: None,
                reverse_proxy: crate::server::dynamic_config::ReverseProxySettings {
                    max_connections: 10000,
                    connection_timeout_secs: 30,
                    buffer_size: 8192,
                },
                dns: crate::server::dynamic_config::DnsSettings {
                    tunnel_domain: "tunnel.local".to_string(),
                    mesh_domain: "mesh.local".to_string(),
                },
            })),
            ss_listener_abort: Arc::new(RwLock::new(None)),
            trojan_listener_abort: Arc::new(RwLock::new(None)),
            api_tls: false,
            api_domain: None,
        }
    }

    /// Create a new server state with database
    pub fn with_db(db: Database) -> Self {
        Self {
            ports: Arc::new(Mutex::new(HashMap::new())),
            listener_tasks: Arc::new(Mutex::new(HashMap::new())),
            active_connections: Arc::new(Mutex::new(HashMap::new())),
            ss_active_connections: Arc::new(Mutex::new(HashMap::new())),
            trojan_active_connections: Arc::new(Mutex::new(HashMap::new())),
            traffic_store: TrafficStore::with_db(db.clone()),
            quality_store: QualityStore::with_db(db.clone()),
            quality_trackers: Arc::new(Mutex::new(HashMap::new())),
            db: Some(db.clone()),
            log_store: Some(crate::server::logs::LogStore::new(Some(db.clone()))),
            mesh_manager: MeshManager::new(),
            dns_registry: None,
            proxy_state: ReverseProxyState::with_db(db),
            acme_client: None,
            acme_config: None,
            acme_full_config: Arc::new(RwLock::new(AcmeFullConfig::default())),
            cert_manager: None,
            dns_provider_config: Arc::new(RwLock::new(None)),
            dynamic_config: Arc::new(RwLock::new(DynamicConfig {
                log_level: "info".to_string(),
                ss: None,
                trojan: None,
                reverse_proxy: crate::server::dynamic_config::ReverseProxySettings {
                    max_connections: 10000,
                    connection_timeout_secs: 30,
                    buffer_size: 8192,
                },
                dns: crate::server::dynamic_config::DnsSettings {
                    tunnel_domain: "tunnel.local".to_string(),
                    mesh_domain: "mesh.local".to_string(),
                },
            })),
            ss_listener_abort: Arc::new(RwLock::new(None)),
            trojan_listener_abort: Arc::new(RwLock::new(None)),
            api_tls: false,
            api_domain: None,
        }
    }

    pub async fn register_client(
        &self,
        remote_port: u16,
        hostname: Option<String>,
        control_sender: ControlMessageSender,
    ) -> bool {
        let hostname_clone = hostname.clone();
        let mut ports = self.ports.lock().await;
        if ports.contains_key(&remote_port) {
            return false;
        }
        ports.insert(
            remote_port,
            PortInfo::Tunnel(ClientInfo {
                remote_port,
                hostname,
                control_sender,
            }),
        );

        // Record client connection in database
        if let Some(db) = &self.db {
            let db = db.clone();
            tokio::spawn(async move {
                let _ = db.record_client_connect(remote_port, hostname_clone).await;
            });
        }

        true
    }

    pub async fn get_client(&self, remote_port: u16) -> Option<ClientInfo> {
        let ports = self.ports.lock().await;
        match ports.get(&remote_port) {
            Some(PortInfo::Tunnel(info)) => Some(info.clone()),
            _ => None,
        }
    }

    pub async fn remove_client(&self, remote_port: u16) {
        // Abort the listener task so the port is freed
        let task = {
            let mut tasks = self.listener_tasks.lock().await;
            tasks.remove(&remote_port)
        };
        if let Some(handle) = task {
            handle.abort();
            info!("Aborted listener task for port {}", remote_port);
        }

        let mut ports = self.ports.lock().await;
        ports.remove(&remote_port);

        // Also clean up quality data when client is removed
        self.quality_store.remove_port(remote_port).await;
        self.remove_quality_tracker(remote_port).await;

        // Record client disconnection in database
        if let Some(db) = &self.db {
            let db = db.clone();
            tokio::spawn(async move {
                let _ = db.record_client_disconnect(remote_port).await;
            });
        }
    }

    pub async fn register_shadowsocks(&self, port: u16, cipher: String, password: String) -> bool {
        let mut ports = self.ports.lock().await;
        if ports.contains_key(&port) {
            return false;
        }
        ports.insert(
            port,
            PortInfo::Shadowsocks {
                port,
                cipher,
                password,
                enabled: true,
                created_at: chrono::Utc::now().timestamp(),
            },
        );
        true
    }

    pub async fn get_port(&self, port: u16) -> Option<PortInfo> {
        let ports = self.ports.lock().await;
        ports.get(&port).cloned()
    }

    pub async fn unregister_port(&self, port: u16) -> bool {
        let mut ports = self.ports.lock().await;
        ports.remove(&port).is_some()
    }

    pub async fn add_active_connection(
        &self,
        connection_id: u64,
        remote_port: u16,
        user_writer: Arc<Mutex<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>>,
    ) {
        let mut active_connections = self.active_connections.lock().await;
        active_connections.insert(
            connection_id,
            ActiveConnection {
                user_writer,
                remote_port,
            },
        );
    }

    /// Get the number of active connections for a specific port
    pub async fn get_connection_count_for_port(&self, remote_port: u16) -> usize {
        let active_connections = self.active_connections.lock().await;
        let tunnel_count = active_connections
            .values()
            .filter(|conn| conn.remote_port == remote_port)
            .count();

        let ss_connections = self.ss_active_connections.lock().await;
        let ss_count = ss_connections.get(&remote_port).copied().unwrap_or(0);

        let trojan_connections = self.trojan_active_connections.lock().await;
        let trojan_count = trojan_connections.get(&remote_port).copied().unwrap_or(0);

        tunnel_count + ss_count + trojan_count
    }

    /// Increment active Shadowsocks connections for a port
    pub async fn increment_ss_connections(&self, port: u16) {
        let mut ss_connections = self.ss_active_connections.lock().await;
        *ss_connections.entry(port).or_insert(0) += 1;
    }

    /// Decrement active Shadowsocks connections for a port
    pub async fn decrement_ss_connections(&self, port: u16) {
        let mut ss_connections = self.ss_active_connections.lock().await;
        if let Some(count) = ss_connections.get_mut(&port) {
            if *count > 0 {
                *count -= 1;
            }
        }
    }

    pub async fn register_trojan(&self, port: u16, password: String, fallback: String) -> bool {
        let mut ports = self.ports.lock().await;
        if ports.contains_key(&port) {
            return false;
        }
        ports.insert(
            port,
            PortInfo::Trojan {
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
                PortInfo::Trojan { .. } => Some(*port),
                _ => None,
            })
            .collect()
    }

    /// Check if a port is a Trojan port
    pub async fn is_trojan_port(&self, port: u16) -> bool {
        let ports = self.ports.lock().await;
        matches!(ports.get(&port), Some(PortInfo::Trojan { .. }))
    }

    /// Increment active Trojan connections for a port
    pub async fn increment_trojan_connections(&self, port: u16) {
        let mut trojan_connections = self.trojan_active_connections.lock().await;
        *trojan_connections.entry(port).or_insert(0) += 1;
    }

    /// Decrement active Trojan connections for a port
    pub async fn decrement_trojan_connections(&self, port: u16) {
        let mut trojan_connections = self.trojan_active_connections.lock().await;
        if let Some(count) = trojan_connections.get_mut(&port) {
            if *count > 0 {
                *count -= 1;
            }
        }
    }

    pub async fn remove_active_connection(&self, connection_id: u64) {
        let mut active_connections = self.active_connections.lock().await;
        active_connections.remove(&connection_id);
    }

    pub async fn deliver_data(&self, connection_id: u64, data: Vec<u8>) -> TunnelResult<()> {
        let active_connections = self.active_connections.lock().await;
        if let Some(conn) = active_connections.get(&connection_id) {
            let mut writer = conn.user_writer.lock().await;
            let bytes = data.len();
            let remote_port = conn.remote_port;
            writer.write_all(&data).await?;
            writer.flush().await?;
            drop(writer);
            drop(active_connections);

            // Record traffic to the correct port
            self.traffic_store
                .record_bytes_out(remote_port, bytes as u64)
                .await;
            Ok(())
        } else {
            debug!("No active connection found for id {}", connection_id);
            Ok(())
        }
    }

    pub async fn close_connection(&self, connection_id: u64) {
        let mut active_connections = self.active_connections.lock().await;
        active_connections.remove(&connection_id);
    }

    // API helper methods
    pub async fn get_all_clients(&self) -> Vec<(u16, ClientInfo)> {
        let ports = self.ports.lock().await;
        ports
            .iter()
            .filter_map(|(port, info)| match info {
                PortInfo::Tunnel(client_info) => Some((*port, client_info.clone())),
                _ => None,
            })
            .collect()
    }

    pub async fn get_client_count(&self) -> usize {
        let ports = self.ports.lock().await;
        ports
            .values()
            .filter(|p| matches!(p, PortInfo::Tunnel(_)))
            .count()
    }

    pub async fn get_active_connection_count(&self) -> usize {
        let active_connections = self.active_connections.lock().await;
        let ss_connections = self.ss_active_connections.lock().await;
        let trojan_connections = self.trojan_active_connections.lock().await;
        active_connections.len()
            + ss_connections.values().sum::<usize>()
            + trojan_connections.values().sum::<usize>()
    }

    pub async fn disconnect_client(&self, remote_port: u16) -> bool {
        let ports = self.ports.lock().await;
        if let Some(PortInfo::Tunnel(client)) = ports.get(&remote_port) {
            // Send Disconnect message to client via the channel
            let _ = client.control_sender.send(ControlMessage::Disconnect).await;
            true
        } else {
            false
        }
    }

    /// Get or create quality tracker for a port
    pub async fn get_or_create_quality_tracker(&self, port: u16) -> QualityTracker {
        let mut trackers = self.quality_trackers.lock().await;
        trackers.entry(port).or_default().clone()
    }

    /// Update quality tracker for a port
    pub async fn update_quality_tracker(&self, port: u16, tracker: QualityTracker) {
        let mut trackers = self.quality_trackers.lock().await;
        trackers.insert(port, tracker);
    }

    /// Remove quality tracker for a port (client disconnect)
    pub async fn remove_quality_tracker(&self, port: u16) {
        let mut trackers = self.quality_trackers.lock().await;
        trackers.remove(&port);
    }

    /// Get all Shadowsocks ports
    pub async fn get_shadowsocks_ports(&self) -> Vec<u16> {
        let ports = self.ports.lock().await;
        ports
            .iter()
            .filter_map(|(port, info)| match info {
                PortInfo::Shadowsocks { .. } => Some(*port),
                _ => None,
            })
            .collect()
    }

    /// Check if a port is a Shadowsocks port
    pub async fn is_shadowsocks_port(&self, port: u16) -> bool {
        let ports = self.ports.lock().await;
        matches!(ports.get(&port), Some(PortInfo::Shadowsocks { .. }))
    }

    /// Sample quality for proxy ports (Shadowsocks / Trojan) based on traffic throughput.
    /// Called periodically — reads recent traffic buckets and generates quality samples.
    pub async fn sample_proxy_quality(&self) {
        let now = Utc::now();
        let ss_ports = self.get_shadowsocks_ports().await;
        let trojan_ports = self.get_trojan_ports().await;

        for port in ss_ports.iter().chain(trojan_ports.iter()) {
            let (bytes_in_per_sec, bytes_out_per_sec) =
                if let Some(traffic) = self.traffic_store.get_port_traffic(*port).await {
                    // Compute throughput from the most recent bucket
                    let (recent_in, recent_out) = traffic
                        .buckets
                        .back()
                        .map(|b| (b.bytes_in as f64, b.bytes_out as f64))
                        .unwrap_or((0.0, 0.0));
                    // Approximate per-second rate: bucket covers ~60 seconds
                    (recent_in / 60.0, recent_out / 60.0)
                } else {
                    (0.0, 0.0)
                };

            // Quality score: based on whether there's active traffic
            let active = bytes_in_per_sec > 0.0 || bytes_out_per_sec > 0.0;
            let quality_score: u8 = if active { 100 } else { 50 };

            let quality = ConnectionQuality {
                last_rtt_ms: 0.0,
                avg_rtt_ms: 0.0,
                min_rtt_ms: 0.0,
                max_rtt_ms: 0.0,
                loss_rate: 0.0,
                consecutive_losses: 0,
                bytes_in_per_sec,
                bytes_out_per_sec,
                quality_score,
                last_update: now,
                is_warning: false,
                is_critical: false,
            };

            self.quality_store.update_quality(*port, quality).await;

            // Add historical sample once per minute
            let sample = QualitySample {
                timestamp: now,
                avg_rtt_ms: 0.0,
                loss_rate: 0.0,
                bytes_in_per_sec,
                bytes_out_per_sec,
                quality_score,
            };
            self.quality_store.add_sample(*port, sample).await;
        }
    }

    /// Set the ACME client for this server state
    pub fn set_acme_client(
        &mut self,
        client: std::sync::Arc<crate::server::acme::client::AcmeClient>,
        config: AcmeConfigInfo,
    ) {
        self.acme_client = Some(client);
        self.acme_config = Some(config);
    }

    /// Set the certificate manager for this server state
    pub fn set_cert_manager(
        &mut self,
        manager: std::sync::Arc<crate::server::acme::manager::CertificateManager>,
    ) {
        self.cert_manager = Some(manager);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_server_state_new() {
        let state = ServerState::new();
        assert_eq!(state.get_client_count().await, 0);
        assert_eq!(state.get_active_connection_count().await, 0);
    }

    // Helper to create a test message sender
    fn create_test_sender() -> ControlMessageSender {
        let (sender, _) = mpsc::channel(32);
        sender
    }

    #[tokio::test]
    async fn test_register_and_get_client() {
        let state = ServerState::new();

        let sender = create_test_sender();

        // Register client
        let registered = state.register_client(8080, None, sender.clone()).await;
        assert!(registered);
        assert_eq!(state.get_client_count().await, 1);

        // Get client
        let client = state.get_client(8080).await;
        assert!(client.is_some());

        // Register same port again should fail
        let registered = state.register_client(8080, None, sender).await;
        assert!(!registered);
    }

    #[tokio::test]
    async fn test_remove_client() {
        let state = ServerState::new();

        let sender = create_test_sender();
        state.register_client(8080, None, sender).await;
        assert_eq!(state.get_client_count().await, 1);

        state.remove_client(8080).await;
        assert_eq!(state.get_client_count().await, 0);
        assert!(state.get_client(8080).await.is_none());
    }

    #[tokio::test]
    async fn test_remove_nonexistent_client() {
        let state = ServerState::new();
        // Should not panic
        state.remove_client(9999).await;
    }

    #[tokio::test]
    async fn test_get_all_clients() {
        let state = ServerState::new();

        let sender = create_test_sender();

        state.register_client(8080, None, sender.clone()).await;
        state.register_client(9000, None, sender).await;

        let clients = state.get_all_clients().await;
        assert_eq!(clients.len(), 2);
        let ports: Vec<u16> = clients.iter().map(|(p, _)| *p).collect();
        assert!(ports.contains(&8080));
        assert!(ports.contains(&9000));
    }

    #[tokio::test]
    async fn test_active_connections() {
        let state = ServerState::new();

        // For active connections, we still need a boxed writer - use a vec as mock
        let mock_writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send> = Box::new(Vec::new());
        let writer_arc = Arc::new(Mutex::new(mock_writer));

        state.add_active_connection(42, 8080, writer_arc).await;
        assert_eq!(state.get_active_connection_count().await, 1);
        assert_eq!(state.get_connection_count_for_port(8080).await, 1);

        state.remove_active_connection(42).await;
        assert_eq!(state.get_active_connection_count().await, 0);
        assert_eq!(state.get_connection_count_for_port(8080).await, 0);
    }

    #[tokio::test]
    async fn test_connection_count_for_nonexistent_port() {
        let state = ServerState::new();
        assert_eq!(state.get_connection_count_for_port(9999).await, 0);
    }

    #[tokio::test]
    async fn test_server_state_clone() {
        let state = ServerState::new();

        let sender = create_test_sender();
        state.register_client(8080, None, sender).await;

        let cloned = state.clone();
        assert_eq!(cloned.get_client_count().await, 1);
    }

    #[tokio::test]
    async fn test_remove_nonexistent_connection() {
        let state = ServerState::new();
        // Should not panic
        state.remove_active_connection(9999).await;
        state.close_connection(9999).await;
    }

    #[tokio::test]
    async fn test_disconnect_nonexistent_client() {
        let state = ServerState::new();
        let result = state.disconnect_client(9999).await;
        assert!(!result);
    }
}

#[cfg(test)]
mod ss_integration_tests {
    use super::*;

    #[tokio::test]
    async fn test_can_register_both_tunnel_and_shadowsocks() {
        let state = ServerState::new();

        // Register a tunnel port
        let (sender, _) = tokio::sync::mpsc::channel(1);
        assert!(
            state
                .register_client(8080, Some("test-host".to_string()), sender.clone())
                .await
        );

        // Register a shadowsocks port
        assert!(
            state
                .register_shadowsocks(8388, "aes-256-gcm".to_string(), "test-pass".to_string())
                .await
        );

        // Verify both are registered
        let clients = state.get_all_clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].0, 8080);

        let ss_ports = state.get_shadowsocks_ports().await;
        assert_eq!(ss_ports.len(), 1);
        assert_eq!(ss_ports[0], 8388);

        // Can get port info for both
        assert!(state.get_port(8080).await.is_some());
        assert!(state.get_port(8388).await.is_some());

        // Check port types
        assert!(!state.is_shadowsocks_port(8080).await);
        assert!(state.is_shadowsocks_port(8388).await);
    }

    #[tokio::test]
    async fn test_port_type_detection() {
        let state = ServerState::new();

        // Test empty
        assert!(!state.is_shadowsocks_port(9999).await);

        // Register tunnel
        let (sender, _) = tokio::sync::mpsc::channel(1);
        state.register_client(9000, None, sender.clone()).await;
        assert!(!state.is_shadowsocks_port(9000).await);

        // Register SS
        state
            .register_shadowsocks(9001, "aes-256-gcm".to_string(), "pass".to_string())
            .await;
        assert!(state.is_shadowsocks_port(9001).await);
    }

    #[tokio::test]
    async fn test_unregister_port() {
        let state = ServerState::new();

        // Register both
        let (sender, _) = tokio::sync::mpsc::channel(1);
        state.register_client(9002, None, sender.clone()).await;
        state
            .register_shadowsocks(9003, "aes-256-gcm".to_string(), "pass".to_string())
            .await;

        // Both exist
        assert!(state.get_port(9002).await.is_some());
        assert!(state.get_port(9003).await.is_some());

        // Unregister tunnel
        assert!(state.unregister_port(9002).await);
        assert!(state.get_port(9002).await.is_none());

        // Unregister SS
        assert!(state.unregister_port(9003).await);
        assert!(state.get_port(9003).await.is_none());

        // Unregister non-existent
        assert!(!state.unregister_port(9999).await);
    }

    #[tokio::test]
    async fn test_cannot_register_duplicate_port_both_types() {
        let state = ServerState::new();
        let (sender, _) = tokio::sync::mpsc::channel(1);

        // Register tunnel first
        assert!(state.register_client(8080, None, sender.clone()).await);
        // Cannot register SS on same port
        assert!(
            !state
                .register_shadowsocks(8080, "aes-256-gcm".into(), "pass".into())
                .await
        );

        // Create new state, reverse order
        let state2 = ServerState::new();
        assert!(
            state2
                .register_shadowsocks(8081, "aes-256-gcm".into(), "pass".into())
                .await
        );
        // Cannot register tunnel on same port
        assert!(!state2.register_client(8081, None, sender.clone()).await);
    }
}

/// Handle a single control connection from client (supports both plain TCP and TLS)
async fn handle_control_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    config: ServerConfig,
    state: ServerState,
    stream: S,
    peer_addr: std::net::SocketAddr,
) -> TunnelResult<()> {
    // Split into read and write halves
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Create message channel for sending messages to client
    // The writer task receives messages and writes them to the stream
    let (sender, mut receiver) = mpsc::channel::<ControlMessage>(32);

    // Spawn writer task - handles all message sending
    tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            if let Err(e) = msg.write_to_stream(&mut writer).await {
                debug!("Failed to write control message: {}", e);
                break;
            }
        }
    });

    // Track all registered ports for this connection
    let mut registered_ports = Vec::new();
    let mut deferred_msg: Option<ControlMessage> = None;

    // Process registration phase - client may send multiple Register messages
    info!("Waiting for client registration...");

    loop {
        let msg = match ControlMessage::read_from_stream(&mut reader).await {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                if registered_ports.is_empty() {
                    return Err(TunnelError::Protocol(
                        "Connection closed before registration".into(),
                    ));
                } else {
                    break;
                }
            }
            Err(e) => {
                if registered_ports.is_empty() {
                    return Err(e);
                } else {
                    error!("Error during registration phase: {}", e);
                    break;
                }
            }
        };

        match msg {
            ControlMessage::Register {
                remote_port,
                hostname,
                auth_token: client_auth_token,
            } => {
                info!(
                    "Received registration request for port {} from hostname {:?}",
                    remote_port, hostname
                );

                // Validate authentication token if server requires it
                if let Some(ref expected_token) = config.client_auth_token {
                    match client_auth_token {
                        Some(ref token) if token == expected_token => {
                            debug!("Client authentication successful");
                        }
                        Some(_) => {
                            warn!(
                                "Client authentication failed from {}: invalid token",
                                peer_addr
                            );
                            let _ = sender
                                .send(ControlMessage::RegisterResponse {
                                    success: false,
                                    message: "Invalid authentication token".into(),
                                })
                                .await;
                            continue;
                        }
                        None => {
                            warn!("Client authentication failed from {}: token required but not provided", peer_addr);
                            let _ = sender
                                .send(ControlMessage::RegisterResponse {
                                    success: false,
                                    message: "Authentication token required".into(),
                                })
                                .await;
                            continue;
                        }
                    }
                }

                // First, remove any existing client on this port (cleanup from previous connection)
                state.remove_client(remote_port).await;

                // Now register the new client (clone sender for each registration)
                let registered = state
                    .register_client(remote_port, hostname.clone(), sender.clone())
                    .await;

                if !registered {
                    let _ = sender
                        .send(ControlMessage::RegisterResponse {
                            success: false,
                            message: format!("Port {} already registered", remote_port),
                        })
                        .await;
                    continue;
                }

                // Send registration success
                sender
                    .send(ControlMessage::RegisterResponse {
                        success: true,
                        message: "Registered successfully".into(),
                    })
                    .await
                    .map_err(|_| {
                        TunnelError::Protocol("Failed to send registration response".into())
                    })?;

                info!("Client registered for port {}", remote_port);
                registered_ports.push(remote_port);

                // Auto-register DNS record for tunnel port if DNS is enabled
                if let Some(ref dns_registry) = state.dns_registry {
                    let fqdn = dns_registry
                        .register_tunnel_default(remote_port, None)
                        .await;
                    info!("DNS auto-registered: {}", fqdn);
                }

                // Spawn the listener task for the remote port
                let state_clone = state.clone();
                let state_for_remove = state.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) = listener::run_listener(state_clone, remote_port).await {
                        error!("Listener for port {} failed: {}", remote_port, e);
                    }
                    state_for_remove.remove_client(remote_port).await;
                    info!("Client unregistered from port {}", remote_port);
                });
                // Store the handle so we can abort it on disconnect
                {
                    let mut tasks = state.listener_tasks.lock().await;
                    tasks.insert(remote_port, handle);
                }
            }
            ControlMessage::Ping {
                seq,
                timestamp_micros,
            } => {
                // Ping received during registration phase
                if !registered_ports.is_empty() {
                    // Process quality update (same as main-loop Ping handling)
                    let now = Utc::now();
                    let now_micros = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0);
                    let rtt_ms = if now_micros > timestamp_micros {
                        (now_micros - timestamp_micros) as f32 / 1000.0
                    } else {
                        0.0
                    };

                    for &port in &registered_ports {
                        let mut tracker = state.get_or_create_quality_tracker(port).await;
                        let (lost, loss_rate) = tracker.record_ping(seq);
                        tracker.record_rtt(rtt_ms);

                        let avg_rtt = tracker.get_avg_rtt();
                        let min_rtt = tracker.get_min_rtt();
                        let max_rtt = tracker.get_max_rtt();
                        let quality_score = calculate_quality_score(avg_rtt, loss_rate);
                        let thresholds = QualityThresholds::default();
                        let (is_warning, is_critical) =
                            check_warnings(avg_rtt, loss_rate, &thresholds);

                        let quality = ConnectionQuality {
                            last_rtt_ms: rtt_ms,
                            avg_rtt_ms: avg_rtt,
                            min_rtt_ms: min_rtt,
                            max_rtt_ms: max_rtt,
                            loss_rate,
                            consecutive_losses: lost,
                            bytes_in_per_sec: 0.0,
                            bytes_out_per_sec: 0.0,
                            quality_score,
                            last_update: now,
                            is_warning,
                            is_critical,
                        };

                        state.quality_store.update_quality(port, quality).await;

                        // Add historical sample on first ping (minute boundary check)
                        let current_minute = now.minute();
                        if tracker.last_sample_minute != current_minute {
                            let sample = QualitySample {
                                timestamp: now,
                                avg_rtt_ms: avg_rtt,
                                loss_rate,
                                bytes_in_per_sec: 0.0,
                                bytes_out_per_sec: 0.0,
                                quality_score,
                            };
                            state.quality_store.add_sample(port, sample).await;
                            tracker.last_sample_minute = current_minute;
                        }

                        state.update_quality_tracker(port, tracker).await;
                    }

                    // Send pong response
                    let pong_timestamp_micros = now_micros;
                    let _ = sender
                        .send(ControlMessage::Pong {
                            seq,
                            ping_timestamp_micros: timestamp_micros,
                            pong_timestamp_micros,
                        })
                        .await;

                    info!(
                        "Registration phase complete (received Ping), {} ports registered",
                        registered_ports.len()
                    );
                    break;
                } else {
                    // No ports registered yet, just ignore and continue waiting
                    continue;
                }
            }
            ControlMessage::LogBatch { .. } => {
                // Log batches during registration phase are silently dropped
                // (no ports registered yet, so no source context)
            }
            _ => {
                // If we have registered ports, this is the end of registration phase
                if !registered_ports.is_empty() {
                    info!(
                        "Registration phase complete, {} ports registered",
                        registered_ports.len()
                    );
                    deferred_msg = Some(msg);
                    break;
                } else {
                    let _ = sender
                        .send(ControlMessage::RegisterResponse {
                            success: false,
                            message: "Expected registration message".into(),
                        })
                        .await;
                    return Err(TunnelError::Protocol(
                        "Expected registration message".into(),
                    ));
                }
            }
        }
    }

    // Derive client name from the first registered port's hostname
    let client_name = if let Some(&port) = registered_ports.first() {
        state
            .get_client(port)
            .await
            .and_then(|c| c.hostname)
            .unwrap_or_else(|| format!("port-{}", port))
    } else {
        "unknown".to_string()
    };
    let mut mesh_client_name = client_name.clone();

    // Main loop: keep connection alive and process messages (heartbeats, data routing)
    let result = loop {
        let msg = if let Some(deferred) = deferred_msg.take() {
            deferred
        } else {
            match ControlMessage::read_from_stream(&mut reader).await {
                Ok(Some(msg)) => msg,
                Ok(None) => {
                    break Ok(());
                }
                Err(e) => {
                    error!("Error reading from control channel: {}", e);
                    break Err(e);
                }
            }
        };

        match msg {
            ControlMessage::Register {
                remote_port,
                hostname,
                auth_token: client_auth_token,
            } => {
                // Handle late registration (client might send more Register messages later)
                info!(
                    "Received late registration request for port {} from hostname {:?}",
                    remote_port, hostname
                );

                // Validate authentication token if server requires it
                if let Some(ref expected_token) = config.client_auth_token {
                    match client_auth_token {
                        Some(ref token) if token == expected_token => {
                            debug!("Client authentication successful");
                        }
                        Some(_) => {
                            warn!(
                                "Client authentication failed from {}: invalid token",
                                peer_addr
                            );
                            let _ = sender
                                .send(ControlMessage::RegisterResponse {
                                    success: false,
                                    message: "Invalid authentication token".into(),
                                })
                                .await;
                            continue;
                        }
                        None => {
                            warn!("Client authentication failed from {}: token required but not provided", peer_addr);
                            let _ = sender
                                .send(ControlMessage::RegisterResponse {
                                    success: false,
                                    message: "Authentication token required".into(),
                                })
                                .await;
                            continue;
                        }
                    }
                }

                // First, remove any existing client on this port
                state.remove_client(remote_port).await;

                // Now register the new client
                let registered = state
                    .register_client(remote_port, hostname.clone(), sender.clone())
                    .await;

                if !registered {
                    let _ = sender
                        .send(ControlMessage::RegisterResponse {
                            success: false,
                            message: format!("Port {} already registered", remote_port),
                        })
                        .await;
                } else {
                    // Send registration success
                    sender
                        .send(ControlMessage::RegisterResponse {
                            success: true,
                            message: "Registered successfully".into(),
                        })
                        .await
                        .map_err(|_| {
                            TunnelError::Protocol("Failed to send registration response".into())
                        })?;

                    info!("Client registered for port {}", remote_port);
                    registered_ports.push(remote_port);

                    // Auto-register DNS record for tunnel port if DNS is enabled
                    if let Some(ref dns_registry) = state.dns_registry {
                        let fqdn = dns_registry
                            .register_tunnel_default(remote_port, None)
                            .await;
                        info!("DNS auto-registered: {}", fqdn);
                    }

                    // Spawn the listener task
                    let state_clone = state.clone();
                    let state_for_remove = state.clone();
                    let handle = tokio::spawn(async move {
                        if let Err(e) = listener::run_listener(state_clone, remote_port).await {
                            error!("Listener for port {} failed: {}", remote_port, e);
                        }
                        state_for_remove.remove_client(remote_port).await;
                        info!("Client unregistered from port {}", remote_port);
                    });
                    // Store handle to allow abort on disconnect
                    {
                        let mut tasks = state.listener_tasks.lock().await;
                        tasks.insert(remote_port, handle);
                    }
                }
            }
            ControlMessage::Ping {
                seq,
                timestamp_micros,
            } => {
                // Calculate RTT using server time
                let now = Utc::now();
                let now_micros = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0);
                let rtt_ms = if now_micros > timestamp_micros {
                    (now_micros - timestamp_micros) as f32 / 1000.0
                } else {
                    0.0 // Clock went backwards, ignore
                };

                tracing::debug!(
                    "Processing Ping seq={} for {} ports, rtt={:.1}ms, minute={}",
                    seq,
                    registered_ports.len(),
                    rtt_ms,
                    now.minute()
                );

                // Iterate all registered ports for this connection and update quality
                for &port in &registered_ports {
                    // Get or create quality tracker
                    let mut tracker = state.get_or_create_quality_tracker(port).await;

                    // Record ping and calculate loss
                    let (lost, loss_rate) = tracker.record_ping(seq);

                    // Record RTT
                    tracker.record_rtt(rtt_ms);

                    // Calculate statistics
                    let avg_rtt = tracker.get_avg_rtt();
                    let min_rtt = tracker.get_min_rtt();
                    let max_rtt = tracker.get_max_rtt();

                    // Calculate quality score
                    let quality_score = calculate_quality_score(avg_rtt, loss_rate);

                    // Check warnings
                    let thresholds = QualityThresholds::default();
                    let (is_warning, is_critical) = check_warnings(avg_rtt, loss_rate, &thresholds);

                    // Get throughput from TrafficStore (simplified)
                    // For now we use 0 since we don't have per-second calculation yet
                    let (bytes_in_per_sec, bytes_out_per_sec) = (0.0, 0.0);

                    // Update real-time quality data
                    let quality = ConnectionQuality {
                        last_rtt_ms: rtt_ms,
                        avg_rtt_ms: avg_rtt,
                        min_rtt_ms: min_rtt,
                        max_rtt_ms: max_rtt,
                        loss_rate,
                        consecutive_losses: lost,
                        bytes_in_per_sec,
                        bytes_out_per_sec,
                        quality_score,
                        last_update: now,
                        is_warning,
                        is_critical,
                    };

                    state.quality_store.update_quality(port, quality).await;

                    // Add historical sample once per minute
                    let current_minute = now.minute();
                    if tracker.last_sample_minute != current_minute {
                        tracing::info!(
                                    "Adding quality sample for port {}: last_sample_minute={}, current_minute={}",
                                    port, tracker.last_sample_minute, current_minute
                                );
                        let sample = QualitySample {
                            timestamp: now,
                            avg_rtt_ms: avg_rtt,
                            loss_rate,
                            bytes_in_per_sec,
                            bytes_out_per_sec,
                            quality_score,
                        };
                        state.quality_store.add_sample(port, sample).await;
                        tracker.last_sample_minute = current_minute;
                    }

                    state.update_quality_tracker(port, tracker).await;
                }

                // Send pong response
                let pong_timestamp_micros = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0);
                let _ = sender
                    .send(ControlMessage::Pong {
                        seq,
                        ping_timestamp_micros: timestamp_micros,
                        pong_timestamp_micros,
                    })
                    .await;
            }
            ControlMessage::Pong { .. } => {
                // Ignore, pong is only server -> client
            }
            ControlMessage::LogBatch { entries } => {
                if let Some(ref log_store) = state.log_store {
                    // Find hostname from first registered port
                    let hostname = if let Some(&port) = registered_ports.first() {
                        state.get_client(port).await.map(|c| c.hostname).flatten()
                    } else {
                        None
                    };
                    let source_prefix = format!(
                        "client:{}:{}",
                        hostname.as_deref().unwrap_or("unknown"),
                        registered_ports.first().copied().unwrap_or(0)
                    );

                    for entry in entries {
                        log_store.send(crate::server::logs::LogEntry {
                            id: 0,
                            timestamp: entry.timestamp,
                            level: entry.level,
                            source: source_prefix.clone(),
                            target: entry.target,
                            message: entry.message,
                        });
                    }
                }
            }
            ControlMessage::Data {
                connection_id,
                data,
            } => {
                // Deliver data from client to user connection
                if let Err(e) = state.deliver_data(connection_id, data).await {
                    warn!(
                        "Failed to deliver data to connection {}: {}",
                        connection_id, e
                    );
                }
            }
            ControlMessage::ConnectionReady { .. } => {
                debug!("Connection ready");
            }
            ControlMessage::Close { connection_id } => {
                state.close_connection(connection_id).await;
                debug!("Connection {} closed by client", connection_id);
            }
            ControlMessage::Disconnect => {
                warn!("Received unexpected Disconnect from client");
            }
            ControlMessage::MeshJoin {
                mesh_id,
                client_name: msg_client_name,
            } => {
                mesh_client_name = msg_client_name;
                info!("Client '{}' joining mesh '{}'", mesh_client_name, mesh_id);
                // Register client for relay
                state
                    .mesh_manager
                    .register_client(&mesh_client_name, sender.clone())
                    .await;
                // Join the mesh
                let members = state
                    .mesh_manager
                    .join_mesh(&mesh_id, &mesh_client_name)
                    .await;
                // Send member list back to requester
                let _ = sender
                    .send(ControlMessage::MeshMemberList {
                        mesh_id: mesh_id.clone(),
                        members,
                    })
                    .await;
                // Notify other members of new joiner (re-fetch updated list)
                let updated = state
                    .mesh_manager
                    .join_mesh(&mesh_id, &mesh_client_name)
                    .await;
                let notify_msg = ControlMessage::MeshMemberList {
                    mesh_id: mesh_id.clone(),
                    members: updated,
                };
                state
                    .mesh_manager
                    .broadcast_to_mesh(&mesh_id, notify_msg, Some(&mesh_client_name))
                    .await;
            }
            ControlMessage::MeshLeave { mesh_id } => {
                info!("Client '{}' leaving mesh '{}'", mesh_client_name, mesh_id);
                let members = state
                    .mesh_manager
                    .leave_mesh(&mesh_id, &mesh_client_name)
                    .await;
                // Notify remaining members
                let notify_msg = ControlMessage::MeshMemberList {
                    mesh_id: mesh_id.clone(),
                    members,
                };
                state
                    .mesh_manager
                    .broadcast_to_mesh(&mesh_id, notify_msg, None)
                    .await;
                // Cleanup DNS records for this mesh
                if let Some(ref dns_registry) = state.dns_registry {
                    dns_registry.unregister_mesh_client(&mesh_id).await;
                }
            }
            ControlMessage::MeshConnect {
                target_client,
                service_name,
            } => {
                debug!(
                    "Mesh connect request: {} -> {} (service: {})",
                    mesh_client_name, target_client, service_name
                );
                // Forward to target
                if state
                    .mesh_manager
                    .send_to_client(
                        &target_client,
                        ControlMessage::MeshConnect {
                            target_client: mesh_client_name.clone(),
                            service_name: service_name.clone(),
                        },
                    )
                    .await
                {
                    debug!("Forwarded mesh connect request to {}", target_client);
                } else {
                    warn!(
                        "Target client '{}' not reachable for mesh connect",
                        target_client
                    );
                }
            }
            ControlMessage::MeshMemberList { .. } => {
                warn!(
                    "Received unexpected MeshMemberList from client '{}' (server->client message)",
                    mesh_client_name
                );
            }
            ControlMessage::P2PRequest {
                target_client,
                local_addr,
            } => {
                debug!(
                    "P2P request: {} -> {} (addr: {})",
                    mesh_client_name, target_client, local_addr
                );
                // Forward P2P request to target with requester's address
                state
                    .mesh_manager
                    .send_to_client(
                        &target_client,
                        ControlMessage::P2PResponse {
                            target_client: mesh_client_name.clone(),
                            remote_addr: local_addr,
                        },
                    )
                    .await;
            }
            ControlMessage::P2PResponse { .. } => {
                warn!(
                    "Received unexpected P2PResponse from client '{}' (server->client message)",
                    mesh_client_name
                );
            }
            ControlMessage::P2PResult {
                target_client,
                success,
            } => {
                if success {
                    info!(
                        "P2P connection established between '{}' and '{}'",
                        mesh_client_name, target_client
                    );
                } else {
                    info!(
                        "P2P hole punch failed between '{}' and '{}', will use relay",
                        mesh_client_name, target_client
                    );
                }
            }
            ControlMessage::MeshRelay {
                target_client,
                data,
            } => {
                if let Err(e) = state
                    .mesh_manager
                    .relay
                    .relay_data(&mesh_client_name, &target_client, data)
                    .await
                {
                    warn!(
                        "Mesh relay failed from '{}' to '{}': {}",
                        mesh_client_name, target_client, e
                    );
                }
            }
            ControlMessage::MeshRegisterServices { mesh_id, services } => {
                info!(
                    "Registering {} services for client '{}' in mesh '{}'",
                    services.len(),
                    mesh_client_name,
                    mesh_id
                );
                let mesh_services: Vec<crate::common::MeshService> = services
                    .iter()
                    .map(|s| crate::common::MeshService {
                        name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                    })
                    .collect();
                state
                    .mesh_manager
                    .register_services(&mesh_id, &mesh_client_name, mesh_services)
                    .await;

                // Auto-register DNS records for mesh services if DNS is enabled
                if let Some(ref dns_registry) = state.dns_registry {
                    for s in &services {
                        // Parse port from local_addr (e.g., "localhost:3306" → 3306)
                        if let Some(port_str) = s.local_addr.rsplit(':').next() {
                            if let Ok(port) = port_str.parse::<u16>() {
                                dns_registry
                                    .register_mesh_service(
                                        &mesh_id,
                                        &s.name,
                                        &s.protocol,
                                        &peer_addr.ip().to_string(),
                                        port,
                                    )
                                    .await;
                            }
                        }
                    }
                }
            }
            ControlMessage::RegisterResponse { .. } => {
                warn!("Received unexpected RegisterResponse from client");
            }
            ControlMessage::NewConnection { .. } => {
                warn!("Received unexpected NewConnection from client");
            }
        }
    };

    // Cleanup: remove all registered clients when control connection closes
    for &remote_port in &registered_ports {
        state.remove_client(remote_port).await;
        state.remove_quality_tracker(remote_port).await;
        info!(
            "Client unregistered from port {} (control connection closed)",
            remote_port
        );
    }

    // Cleanup mesh: leave each mesh with broadcast to remaining members
    let meshes = state
        .mesh_manager
        .router
        .lock()
        .await
        .get_client_meshes(&mesh_client_name);
    for mesh_id in &meshes {
        let members = state
            .mesh_manager
            .leave_mesh(mesh_id, &mesh_client_name)
            .await;
        // Notify remaining members of the departure
        let notify_msg = ControlMessage::MeshMemberList {
            mesh_id: mesh_id.clone(),
            members,
        };
        state
            .mesh_manager
            .broadcast_to_mesh(mesh_id, notify_msg, None)
            .await;
        info!(
            "Client '{}' removed from mesh '{}' (disconnected)",
            mesh_client_name, mesh_id
        );
    }
    // Unregister from relay and clients map (router already cleaned by leave_mesh above)
    state
        .mesh_manager
        .unregister_client(&mesh_client_name)
        .await;

    // Cleanup DNS records
    if let Some(ref dns_registry) = state.dns_registry {
        for &remote_port in &registered_ports {
            let dns_name = format!("port-{}", remote_port);
            dns_registry.unregister_tunnel(&dns_name, remote_port).await;
        }
        for mesh_id in &meshes {
            dns_registry.unregister_mesh_client(mesh_id).await;
        }
    }

    result
}

/// Start the main server
pub async fn run_server(
    config: ServerConfig,
    state: ServerState,
    tls_config_rx: Option<watch::Receiver<Arc<rustls::server::ServerConfig>>>,
) -> TunnelResult<()> {
    // Set up TLS if enabled (fallback when no watch channel is provided)
    let tls_acceptor = if tls_config_rx.is_none() && config.tls {
        info!("TLS ENABLED - generating/loading TLS certificates (static mode)");
        let cert_pair = load_or_generate_cert(&config.tls_cert, &config.tls_key)
            .map_err(|e| TunnelError::Tls(format!("Failed to load TLS certificates: {}", e)))?;
        let tls_config = create_server_config(cert_pair)
            .map_err(|e| TunnelError::Tls(format!("Failed to create TLS config: {}", e)))?;
        Some(TlsAcceptor::from(tls_config))
    } else if config.tls {
        info!("TLS ENABLED - using dynamic certificate watch channel");
        None
    } else {
        info!("TLS DISABLED - using plain TCP connections");
        None
    };

    let listener = TcpListener::bind(&config.control_addr).await?;
    info!("Control server listening on {}", config.control_addr);

    // Log whether client authentication is enabled
    if config.client_auth_token.is_some() {
        info!("Client authentication ENABLED - clients must provide valid tokens");
    } else {
        info!("Client authentication DISABLED - any client can connect");
    }

    loop {
        let (stream, addr) = listener.accept().await?;
        let config_clone = config.clone();
        let state_clone = state.clone();
        let tls_acceptor_clone = tls_acceptor.clone();
        let tls_config_rx_clone = tls_config_rx.clone();

        tracing::debug!("New control connection from {}", addr);

        tokio::spawn(async move {
            // Wrap TCP stream with TLS if enabled
            let result = if let Some(acceptor) = tls_acceptor_clone {
                // Static TLS mode
                debug!("Performing TLS handshake with {}", addr);
                match acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        debug!("TLS handshake successful with {}", addr);
                        handle_control_connection(config_clone, state_clone, tls_stream, addr)
                            .await
                    }
                    Err(e) => {
                        warn!("TLS handshake failed with {}: {}", addr, e);
                        return;
                    }
                }
            } else if let Some(rx) = tls_config_rx_clone {
                // Dynamic TLS mode - read latest config from watch channel
                let current_config = rx.borrow().clone();
                let tls_acceptor = TlsAcceptor::from(current_config);
                debug!("Performing TLS handshake with {} (dynamic cert)", addr);
                match tls_acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        debug!("TLS handshake successful with {}", addr);
                        handle_control_connection(config_clone, state_clone, tls_stream, addr)
                            .await
                    }
                    Err(e) => {
                        warn!("TLS handshake failed with {}: {}", addr, e);
                        return;
                    }
                }
            } else {
                // No TLS
                handle_control_connection(config_clone, state_clone, stream, addr).await
            };

            if let Err(e) = result {
                warn!("Control connection error from {}: {}", addr, e);
            }
        });
    }
}
