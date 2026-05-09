use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn, error, debug};

use crate::common::{ControlMessage, TunnelError, TunnelResult, load_or_generate_cert, create_server_config};
use crate::server::{ServerConfig, listener};
use crate::server::api::TrafficStore;
use crate::server::db::Database;

/// Sender for control messages - can be shared across tasks
pub type ControlMessageSender = mpsc::Sender<ControlMessage>;

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

/// Information about an active connection between user and client
struct ActiveConnection {
    /// User TCP stream writer half connected to client
    user_writer: Arc<Mutex<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>>,
    /// Remote port this connection belongs to
    remote_port: u16,
}

/// Global server state shared between all tasks
#[derive(Clone)]
pub struct ServerState {
    /// Map from remote port to client info
    clients: Arc<Mutex<HashMap<u16, ClientInfo>>>,
    /// Map from connection_id to active connection info
    active_connections: Arc<Mutex<HashMap<u64, ActiveConnection>>>,
    /// Traffic statistics store
    pub traffic_store: TrafficStore,
    /// Database connection (optional)
    db: Option<Database>,
}

impl ServerState {
    /// Create a new server state without database (for backwards compatibility)
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            active_connections: Arc::new(Mutex::new(HashMap::new())),
            traffic_store: TrafficStore::new(),
            db: None,
        }
    }

    /// Create a new server state with database
    pub fn with_db(db: Database) -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            active_connections: Arc::new(Mutex::new(HashMap::new())),
            traffic_store: TrafficStore::with_db(db.clone()),
            db: Some(db),
        }
    }

    pub async fn register_client(&self, remote_port: u16, hostname: Option<String>, control_sender: ControlMessageSender) -> bool {
        let hostname_clone = hostname.clone();
        let mut clients = self.clients.lock().await;
        if clients.contains_key(&remote_port) {
            return false;
        }
        clients.insert(remote_port, ClientInfo {
            remote_port,
            hostname,
            control_sender,
        });

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
        let clients = self.clients.lock().await;
        clients.get(&remote_port).cloned()
    }

    pub async fn remove_client(&self, remote_port: u16) {
        let mut clients = self.clients.lock().await;
        clients.remove(&remote_port);

        // Record client disconnection in database
        if let Some(db) = &self.db {
            let db = db.clone();
            tokio::spawn(async move {
                let _ = db.record_client_disconnect(remote_port).await;
            });
        }
    }

    pub async fn add_active_connection(&self, connection_id: u64, remote_port: u16, user_writer: Arc<Mutex<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>>) {
        let mut active_connections = self.active_connections.lock().await;
        active_connections.insert(connection_id, ActiveConnection {
            user_writer,
            remote_port,
        });
    }

    /// Get the number of active connections for a specific port
    pub async fn get_connection_count_for_port(&self, remote_port: u16) -> usize {
        let active_connections = self.active_connections.lock().await;
        active_connections
            .values()
            .filter(|conn| conn.remote_port == remote_port)
            .count()
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
            self.traffic_store.record_bytes_out(remote_port, bytes as u64).await;
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
        let clients = self.clients.lock().await;
        clients.iter().map(|(port, info)| (*port, info.clone())).collect()
    }

    pub async fn get_client_count(&self) -> usize {
        let clients = self.clients.lock().await;
        clients.len()
    }

    pub async fn get_active_connection_count(&self) -> usize {
        let active_connections = self.active_connections.lock().await;
        active_connections.len()
    }

    pub async fn disconnect_client(&self, remote_port: u16) -> bool {
        let clients = self.clients.lock().await;
        if let Some(client) = clients.get(&remote_port) {
            // Send Disconnect message to client via the channel
            let _ = client.control_sender.send(ControlMessage::Disconnect).await;
            true
        } else {
            false
        }
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

/// Handle a single control connection from client (supports both plain TCP and TLS)
async fn handle_control_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    config: ServerConfig,
    state: ServerState,
    stream: S,
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

    // Process registration phase - client may send multiple Register messages
    info!("Waiting for client registration...");

    loop {
        let msg = match ControlMessage::read_from_stream(&mut reader).await {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                if registered_ports.is_empty() {
                    return Err(TunnelError::Protocol("Connection closed before registration".into()));
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
            ControlMessage::Register { remote_port, hostname, auth_token: client_auth_token } => {
                info!("Received registration request for port {} from hostname {:?}", remote_port, hostname);

                // Validate authentication token if server requires it
                if let Some(ref expected_token) = config.client_auth_token {
                    match client_auth_token {
                        Some(ref token) if token == expected_token => {
                            debug!("Client authentication successful");
                        }
                        Some(_) => {
                            warn!("Client authentication failed: invalid token");
                            let _ = sender.send(ControlMessage::RegisterResponse {
                                success: false,
                                message: "Invalid authentication token".into(),
                            }).await;
                            continue;
                        }
                        None => {
                            warn!("Client authentication failed: token required but not provided");
                            let _ = sender.send(ControlMessage::RegisterResponse {
                                success: false,
                                message: "Authentication token required".into(),
                            }).await;
                            continue;
                        }
                    }
                }

                // First, remove any existing client on this port (cleanup from previous connection)
                state.remove_client(remote_port).await;

                // Now register the new client (clone sender for each registration)
                let registered = state.register_client(remote_port, hostname.clone(), sender.clone()).await;

                if !registered {
                    let _ = sender.send(ControlMessage::RegisterResponse {
                        success: false,
                        message: format!("Port {} already registered", remote_port),
                    }).await;
                    continue;
                }

                // Send registration success
                sender.send(ControlMessage::RegisterResponse {
                    success: true,
                    message: "Registered successfully".into(),
                }).await.map_err(|_| TunnelError::Protocol("Failed to send registration response".into()))?;

                info!("Client registered for port {}", remote_port);
                registered_ports.push(remote_port);

                // Spawn the listener task for the remote port
                let state_clone = state.clone();
                let state_for_remove = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = listener::run_listener(state_clone, remote_port).await {
                        error!("Listener for port {} failed: {}", remote_port, e);
                    }
                    state_for_remove.remove_client(remote_port).await;
                    info!("Client unregistered from port {}", remote_port);
                });
            }
            _ => {
                // If we have registered ports, this is the end of registration phase
                if !registered_ports.is_empty() {
                    info!("Registration phase complete, {} ports registered", registered_ports.len());
                    break;
                } else {
                    let _ = sender.send(ControlMessage::RegisterResponse {
                        success: false,
                        message: "Expected registration message".into(),
                    }).await;
                    return Err(TunnelError::Protocol("Expected registration message".into()));
                }
            }
        }
    }

    // Main loop: keep connection alive and process messages (heartbeats, data routing)
    let result = loop {
        match ControlMessage::read_from_stream(&mut reader).await {
            Ok(Some(msg)) => {
                match msg {
                    ControlMessage::Register { remote_port, hostname, auth_token: client_auth_token } => {
                        // Handle late registration (client might send more Register messages later)
                        info!("Received late registration request for port {} from hostname {:?}", remote_port, hostname);

                        // Validate authentication token if server requires it
                        if let Some(ref expected_token) = config.client_auth_token {
                            match client_auth_token {
                                Some(ref token) if token == expected_token => {
                                    debug!("Client authentication successful");
                                }
                                Some(_) => {
                                    warn!("Client authentication failed: invalid token");
                                    let _ = sender.send(ControlMessage::RegisterResponse {
                                        success: false,
                                        message: "Invalid authentication token".into(),
                                    }).await;
                                    continue;
                                }
                                None => {
                                    warn!("Client authentication failed: token required but not provided");
                                    let _ = sender.send(ControlMessage::RegisterResponse {
                                        success: false,
                                        message: "Authentication token required".into(),
                                    }).await;
                                    continue;
                                }
                            }
                        }

                        // First, remove any existing client on this port
                        state.remove_client(remote_port).await;

                        // Now register the new client
                        let registered = state.register_client(remote_port, hostname.clone(), sender.clone()).await;

                        if !registered {
                            let _ = sender.send(ControlMessage::RegisterResponse {
                                success: false,
                                message: format!("Port {} already registered", remote_port),
                            }).await;
                        } else {
                            // Send registration success
                            sender.send(ControlMessage::RegisterResponse {
                                success: true,
                                message: "Registered successfully".into(),
                            }).await.map_err(|_| TunnelError::Protocol("Failed to send registration response".into()))?;

                            info!("Client registered for port {}", remote_port);
                            registered_ports.push(remote_port);

                            // Spawn the listener task
                            let state_clone = state.clone();
                            let state_for_remove = state.clone();
                            tokio::spawn(async move {
                                if let Err(e) = listener::run_listener(state_clone, remote_port).await {
                                    error!("Listener for port {} failed: {}", remote_port, e);
                                }
                                state_for_remove.remove_client(remote_port).await;
                                info!("Client unregistered from port {}", remote_port);
                            });
                        }
                    }
                    ControlMessage::Ping => {
                        // Send pong response
                        let _ = sender.send(ControlMessage::Pong).await;
                    }
                    ControlMessage::Pong => {
                        // Ignore, pong is only server -> client
                    }
                    ControlMessage::Data { connection_id, data } => {
                        // Deliver data from client to user connection
                        if let Err(e) = state.deliver_data(connection_id, data).await {
                            warn!("Failed to deliver data to connection {}: {}", connection_id, e);
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
                    ControlMessage::RegisterResponse { .. } => {
                        warn!("Received unexpected RegisterResponse from client");
                    }
                    ControlMessage::NewConnection { .. } => {
                        warn!("Received unexpected NewConnection from client");
                    }
                }
            }
            Ok(None) => {
                // Connection closed
                break Ok(());
            }
            Err(e) => {
                error!("Error reading from control channel: {}", e);
                break Err(e);
            }
        }
    };

    // Cleanup: remove all registered clients when control connection closes
    for &remote_port in &registered_ports {
        state.remove_client(remote_port).await;
        info!("Client unregistered from port {} (control connection closed)", remote_port);
    }

    result
}

/// Start the main server
pub async fn run_server(config: ServerConfig, state: ServerState) -> TunnelResult<()> {
    // Set up TLS if enabled
    let tls_acceptor = if config.tls {
        info!("TLS ENABLED - generating/loading TLS certificates");
        let cert_pair = load_or_generate_cert(&config.tls_cert, &config.tls_key)
            .map_err(|e| TunnelError::Tls(format!("Failed to load TLS certificates: {}", e)))?;
        let tls_config = create_server_config(cert_pair)
            .map_err(|e| TunnelError::Tls(format!("Failed to create TLS config: {}", e)))?;
        Some(TlsAcceptor::from(tls_config))
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

        tracing::debug!("New control connection from {}", addr);

        tokio::spawn(async move {
            // Wrap TCP stream with TLS if enabled
            let result = match tls_acceptor_clone {
                Some(acceptor) => {
                    debug!("Performing TLS handshake with {}", addr);
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            debug!("TLS handshake successful with {}", addr);
                            handle_control_connection(config_clone, state_clone, tls_stream).await
                        }
                        Err(e) => {
                            warn!("TLS handshake failed with {}: {}", addr, e);
                            return;
                        }
                    }
                }
                None => {
                    handle_control_connection(config_clone, state_clone, stream).await
                }
            };

            if let Err(e) = result {
                warn!("Control connection error: {}", e);
            }
        });
    }
}
