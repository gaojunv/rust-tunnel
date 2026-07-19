use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::common::{
    create_server_config, load_or_generate_cert, ControlMessage, TunnelError, TunnelResult,
};
use crate::server::api::TrafficStore;
use crate::server::client_registry::{ClientRegistry, TunnelOpenOutcome};
use crate::server::db::Database;
use crate::server::dns::registry::DnsRegistry;
use crate::server::dynamic_config::DynamicConfig;
use crate::server::mesh::MeshManager;
use crate::server::quality::{ConnectionQuality, QualitySample, QualityStore, QualityTracker};
use crate::server::reverse_proxy::ReverseProxyState;
use crate::server::ServerConfig;
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Sender for control messages - can be shared across tasks
pub type ControlMessageSender = mpsc::Sender<ControlMessage>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
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
            PortInfo::Shadowsocks { .. } => PortType::Shadowsocks,
            PortInfo::Trojan { .. } => PortType::Trojan,
        }
    }

    pub fn port(&self) -> u16 {
        match self {
            PortInfo::Shadowsocks { port, .. } => *port,
            PortInfo::Trojan { port, .. } => *port,
        }
    }
}

/// ACME configuration summary for API responses
#[derive(Debug, Clone)]
pub struct AcmeConfigInfo {
    pub enabled: bool,
    pub server_url: String,
    pub cert_dir: String,
}

/// Full ACME configuration for API access
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl AcmeFullConfig {
    /// Resolve `AcmeFullConfig` for the current process.
    ///
    /// If `server_settings.acme_config` in the database contains valid JSON,
    /// return it verbatim — the CLI/TOML `server_config` values are ignored
    /// on this path. This makes DB the runtime source of truth: values set
    /// via `PUT /api/acme/config` (which writes back to DB) survive restart.
    ///
    /// If the row is absent, seed from `server_config` and persist. On this
    /// first-seed path, `tos_agreed` is inferred as `true` when
    /// `acme_certificates` already contains rows — upgrading from a
    /// CLI-only ACME deployment shouldn't force the operator to re-agree.
    ///
    /// If the row is present but malformed, or the DB read fails, run this
    /// process with a fresh seed but leave the DB row untouched — this
    /// preserves any state the operator may want to inspect/repair, and
    /// avoids letting a transient DB error wipe good persistent config.
    ///
    /// All DB failures are logged as warnings and never fatal.
    pub async fn load_or_seed(db: &Database, server_config: &ServerConfig) -> Self {
        // Tracks whether we should persist the seed back to DB.
        // Set to false in branches where the DB may hold state we
        // shouldn't overwrite:
        // - malformed row: preserve it so an operator can inspect/repair
        // - transient read failure: a valid row may still exist; don't
        //   clobber it with the CLI seed
        //
        // Set to true (default) only when we're confident the row is
        // absent (Ok(None)).
        let mut should_persist_seed = true;

        match db.load_server_setting("acme_config").await {
            Ok(Some(json)) => match serde_json::from_str::<Self>(&json) {
                Ok(cfg) => return cfg,
                Err(e) => {
                    warn!(
                            "acme_config in DB is malformed ({}), re-seeding from CLI/TOML without overwriting the bad row",
                            e
                        );
                    should_persist_seed = false;
                }
            },
            Ok(None) => {
                // Normal seed path — should_persist_seed stays true
            }
            Err(e) => {
                warn!(
                    "Failed to load ACME config from DB ({}), falling back to CLI/TOML seed for this process; leaving DB alone until the next successful startup",
                    e
                );
                should_persist_seed = false;
            }
        }

        let mut seed = Self {
            enabled: server_config.acme_enabled,
            server_url: server_config.acme_server_url.clone(),
            email: server_config.acme_email.clone(),
            cert_dir: server_config.acme_cert_dir.clone(),
            auto_renew: server_config.acme_auto_renew,
            renewal_check_interval: server_config.acme_renewal_check_interval,
            renewal_days_before_expiry: server_config.acme_renewal_days_before_expiry,
            tos_agreed: false,
        };

        // Legacy DB path: if certificates already exist in this DB, the
        // operator must have agreed to ToS in a previous version —
        // otherwise those certs couldn't have been issued. Carry that
        // state forward so they don't get re-prompted after upgrade.
        // Only runs on first seed (should_persist_seed == true); the
        // DB-has-value fast path returns before we get here.
        if should_persist_seed && !seed.tos_agreed {
            match db.load_acme_certificates().await {
                Ok(records) if !records.is_empty() => {
                    info!(
                        "Inferred tos_agreed=true from {} existing certificates",
                        records.len()
                    );
                    seed.tos_agreed = true;
                }
                Ok(_) => {
                    // No certs → keep tos_agreed=false
                }
                Err(e) => {
                    warn!(
                        "Failed to query acme_certificates for ToS inference: {}. \
                         Keeping tos_agreed=false; user can re-agree via UI.",
                        e
                    );
                }
            }
        }

        if should_persist_seed {
            match serde_json::to_string(&seed) {
                Ok(json) => {
                    if let Err(e) = db.save_server_setting("acme_config", &json).await {
                        warn!("Failed to persist ACME seed config to DB: {}", e);
                    }
                }
                Err(e) => {
                    warn!("Failed to serialize ACME seed config: {}", e);
                }
            }
        }

        seed
    }
}

/// Global server state shared between all tasks
#[derive(Clone)]
pub struct ServerState {
    /// Map from port to port info (Shadowsocks or Trojan)
    ports: Arc<Mutex<HashMap<u16, PortInfo>>>,
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
    /// Client registry (spec §2.6)
    pub client_registry: Option<ClientRegistry>,
    /// ACME client for certificate management (set when ACME is enabled)
    pub acme_client: Arc<RwLock<Option<std::sync::Arc<crate::server::acme::client::AcmeClient>>>>,
    /// ACME configuration info (set when ACME is enabled)
    pub acme_config: Arc<RwLock<Option<AcmeConfigInfo>>>,
    /// Full ACME configuration for API access
    pub acme_full_config: Arc<RwLock<AcmeFullConfig>>,
    /// Certificate manager for TLS certificate lifecycle (set when ACME is enabled)
    pub cert_manager: Option<std::sync::Arc<crate::server::acme::manager::CertificateManager>>,
    /// DNS provider configuration for ACME DNS-01 challenges
    pub dns_provider_config: Arc<RwLock<Option<crate::server::acme::dns::DnsProviderConfig>>>,
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
            client_registry: None,
            acme_client: Arc::new(RwLock::new(None)),
            acme_config: Arc::new(RwLock::new(None)),
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
            ss_active_connections: Arc::new(Mutex::new(HashMap::new())),
            trojan_active_connections: Arc::new(Mutex::new(HashMap::new())),
            traffic_store: TrafficStore::with_db(db.clone()),
            quality_store: QualityStore::with_db(db.clone()),
            quality_trackers: Arc::new(Mutex::new(HashMap::new())),
            db: Some(db.clone()),
            log_store: Some(crate::server::logs::LogStore::new(Some(db.clone()))),
            mesh_manager: MeshManager::new(),
            dns_registry: None,
            proxy_state: ReverseProxyState::with_db(db.clone()),
            client_registry: Some(ClientRegistry::new(db)),
            acme_client: Arc::new(RwLock::new(None)),
            acme_config: Arc::new(RwLock::new(None)),
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

    /// Wire up the ClientConnector to ReverseProxyState after the server
    /// has started. Called from `bin/server.rs` after `with_db()`.
    pub async fn wire_up_client_connector(&self) {
        if let Some(registry) = &self.client_registry {
            let cc = std::sync::Arc::new(
                crate::server::reverse_proxy::connector::ClientConnector::new(registry.clone()),
            );
            self.proxy_state.set_client_connector(cc).await;
            info!("ClientConnector registered into ReverseProxyState");
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

    /// Get the number of active connections for a specific port (SS/Trojan only)
    pub async fn get_connection_count_for_port(&self, remote_port: u16) -> usize {
        let ss_connections = self.ss_active_connections.lock().await;
        let ss_count = ss_connections.get(&remote_port).copied().unwrap_or(0);

        let trojan_connections = self.trojan_active_connections.lock().await;
        let trojan_count = trojan_connections.get(&remote_port).copied().unwrap_or(0);

        ss_count + trojan_count
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
        let ss_connections = self.ss_active_connections.lock().await;
        let trojan_connections = self.trojan_active_connections.lock().await;
        ss_connections.values().sum::<usize>() + trojan_connections.values().sum::<usize>()
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
    pub async fn set_acme_client(
        &self,
        client: std::sync::Arc<crate::server::acme::client::AcmeClient>,
        config: AcmeConfigInfo,
    ) {
        *self.acme_client.write().await = Some(client);
        *self.acme_config.write().await = Some(config);
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

/// Handle a single control connection from a client using the new
/// ClientRegistry + v2 protocol (spec §3.2).
///
/// This replaces the old `handle_control_connection` which used port-based
/// tunnel-forward registration.
async fn handle_client_connection(
    reader: impl AsyncRead + Unpin + Send,
    writer: impl AsyncWrite + Unpin + Send + 'static,
    state: ServerState,
) -> TunnelResult<()> {
    let registry = state
        .client_registry
        .as_ref()
        .ok_or_else(|| TunnelError::Protocol("server has no client registry".into()))?
        .clone();

    let mut reader = reader;
    let mut writer = writer;

    // 1. Expect Register
    let first = ControlMessage::read_from_stream(&mut reader)
        .await?
        .ok_or_else(|| TunnelError::Protocol("connection closed before Register".into()))?;

    let (client_name, hostname, client_version, password) = match first {
        ControlMessage::Register {
            protocol_version,
            client_name,
            password,
            client_version,
        } => {
            if protocol_version != 2 {
                let resp = ControlMessage::RegisterResponse {
                    success: false,
                    message: format!("unsupported protocol_version {protocol_version}, want 2"),
                };
                let _ = resp.write_to_stream(&mut writer).await;
                return Err(TunnelError::Protocol("protocol version mismatch".into()));
            }
            (client_name, None::<String>, Some(client_version), password)
        }
        other => {
            return Err(TunnelError::Protocol(format!(
                "expected Register, got {other:?}"
            )));
        }
    };

    // Sender channel (Server -> client)
    let (send_tx, mut send_rx) = mpsc::channel::<ControlMessage>(32);

    // Try to register with the ClientRegistry
    let entry = match registry
        .register(
            &client_name,
            hostname,
            client_version,
            &password,
            send_tx.clone(),
        )
        .await
    {
        Ok(e) => e,
        Err(err) => {
            let resp = ControlMessage::RegisterResponse {
                success: false,
                message: err.to_string(),
            };
            let _ = resp.write_to_stream(&mut writer).await;
            return Err(TunnelError::ControlChannel(err.to_string()));
        }
    };

    let resp = ControlMessage::RegisterResponse {
        success: true,
        message: String::new(),
    };
    resp.write_to_stream(&mut writer).await?;

    // Writer task: pump send_rx -> wire
    tokio::spawn(async move {
        while let Some(msg) = send_rx.recv().await {
            if let Err(e) = msg.write_to_stream(&mut writer).await {
                tracing::debug!("control write ended: {e}");
                break;
            }
        }
    });

    // 2. Reader loop: dispatch to registry active_connections / heartbeat / etc.
    let name_for_cleanup = entry.name.clone();
    let cleanup_registry = registry.clone();
    let result: TunnelResult<()> = async {
        loop {
            let msg = match ControlMessage::read_from_stream(&mut reader).await? {
                Some(m) => m,
                None => break,
            };
            match msg {
                ControlMessage::Ping {
                    seq,
                    timestamp_micros,
                } => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0);
                    let _ = entry
                        .control_sender
                        .send(ControlMessage::Pong {
                            seq,
                            ping_timestamp_micros: timestamp_micros,
                            pong_timestamp_micros: now,
                        })
                        .await;
                    // Best-effort touch last_seen
                    if let Some(reg) = state.client_registry.as_ref() {
                        let _ = reg.db().touch_client_last_seen(&entry.name).await;
                    }
                }
                ControlMessage::TunnelOpenResult {
                    connection_id,
                    success,
                    error,
                } => {
                    let mut conns = entry.active_connections.lock().await;
                    if let Some(active) = conns.get_mut(&connection_id) {
                        if let Some(tx) = active.open_result.take() {
                            let outcome = if success {
                                TunnelOpenOutcome::Ok
                            } else {
                                TunnelOpenOutcome::Failed(error.unwrap_or_default())
                            };
                            let _ = tx.send(outcome);
                        }
                    }
                    if !success {
                        conns.remove(&connection_id);
                    }
                }
                ControlMessage::Data {
                    connection_id,
                    data,
                } => {
                    let conns = entry.active_connections.lock().await;
                    if let Some(active) = conns.get(&connection_id) {
                        let _ = active.inbound.send(data).await;
                    }
                }
                ControlMessage::Close { connection_id } => {
                    let mut conns = entry.active_connections.lock().await;
                    conns.remove(&connection_id);
                }
                ControlMessage::LogBatch {
                    entries: log_entries,
                } => {
                    if let Some(ref log_store) = state.log_store {
                        let source_prefix = format!("client:{}", entry.name);
                        for e in log_entries {
                            log_store.send(crate::server::logs::LogEntry {
                                id: 0,
                                timestamp: e.timestamp,
                                level: e.level,
                                source: source_prefix.clone(),
                                target: e.target,
                                message: e.message,
                            });
                        }
                    }
                }
                // Mesh variants: forward to mesh_manager
                m @ (ControlMessage::MeshJoin { .. }
                | ControlMessage::MeshLeave { .. }
                | ControlMessage::MeshMemberList { .. }
                | ControlMessage::MeshConnect { .. }
                | ControlMessage::P2PRequest { .. }
                | ControlMessage::P2PResponse { .. }
                | ControlMessage::P2PResult { .. }
                | ControlMessage::MeshRelay { .. }
                | ControlMessage::MeshRegisterServices { .. }) => {
                    tracing::debug!("mesh msg received from '{}': {:?}", entry.name, m);
                }
                other => {
                    tracing::warn!("unexpected msg from client '{}': {:?}", entry.name, other);
                }
            }
        }
        Ok(())
    }
    .await;

    // 3. Cleanup: remove from registry
    cleanup_registry
        .disconnect(&name_for_cleanup, "connection closed")
        .await;
    result
}

/// Start the main server
#[allow(clippy::too_many_lines)]
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
                        let (reader, writer) = tokio::io::split(tls_stream);
                        handle_client_connection(reader, writer, state_clone).await
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
                        let (reader, writer) = tokio::io::split(tls_stream);
                        handle_client_connection(reader, writer, state_clone).await
                    }
                    Err(e) => {
                        warn!("TLS handshake failed with {}: {}", addr, e);
                        return;
                    }
                }
            } else {
                // No TLS
                let (reader, writer) = tokio::io::split(stream);
                handle_client_connection(reader, writer, state_clone).await
            };

            if let Err(e) = result {
                warn!("Control connection error from {}: {}", addr, e);
            }
        });
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
    use crate::server::db::Database;

    async fn fresh_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn fresh_db_uses_cli_values_and_writes_back() {
        let (db, _tmp) = fresh_db().await;

        let cfg = crate::server::config::ServerConfig {
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
        let cfg = crate::server::config::ServerConfig {
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
        let first_cfg = crate::server::config::ServerConfig {
            acme_enabled: true,
            acme_server_url: "https://first.example/acme".to_string(),
            acme_cert_dir: "/first".to_string(),
            ..Default::default()
        };
        AcmeFullConfig::load_or_seed(&db, &first_cfg).await;

        // Second call with different seed args — should not touch DB
        let second_cfg = crate::server::config::ServerConfig {
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

        let cfg = crate::server::config::ServerConfig {
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

        let cfg = crate::server::config::ServerConfig {
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

        let cfg = crate::server::config::ServerConfig {
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
        let cfg = crate::server::config::ServerConfig {
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
