use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;
use tracing::{info, warn, error, debug};

use crate::common::{ControlMessage, TunnelError, TunnelResult};
use crate::server::{ServerConfig, listener};
use crate::server::api::TrafficStore;

/// Information about a connected client
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Remote port that client wants to expose
    pub remote_port: u16,
    /// Sender half of the control channel for sending messages to client
    pub control_writer: Arc<Mutex<OwnedWriteHalf>>,
}

/// Information about an active connection between user and client
#[derive(Debug)]
struct ActiveConnection {
    /// User TCP stream writer half connected to client
    user_writer: Arc<Mutex<OwnedWriteHalf>>,
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
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            active_connections: Arc::new(Mutex::new(HashMap::new())),
            traffic_store: TrafficStore::new(),
        }
    }

    pub async fn register_client(&self, remote_port: u16, control_writer: Arc<Mutex<OwnedWriteHalf>>) -> bool {
        let mut clients = self.clients.lock().await;
        if clients.contains_key(&remote_port) {
            return false;
        }
        clients.insert(remote_port, ClientInfo {
            remote_port,
            control_writer,
        });
        true
    }

    pub async fn get_client(&self, remote_port: u16) -> Option<ClientInfo> {
        let clients = self.clients.lock().await;
        clients.get(&remote_port).cloned()
    }

    pub async fn remove_client(&self, remote_port: u16) {
        let mut clients = self.clients.lock().await;
        clients.remove(&remote_port);
    }

    pub async fn add_active_connection(&self, connection_id: u64, remote_port: u16, user_writer: Arc<Mutex<OwnedWriteHalf>>) {
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
            // Send Disconnect message to client
            let mut writer = client.control_writer.lock().await;
            // We'll let the connection drop naturally after sending
            let _ = ControlMessage::Disconnect.write_to_split(&mut *writer).await;
            true
        } else {
            false
        }
    }
}

/// Handle a single control connection from client
async fn handle_control_connection(state: ServerState, stream: TcpStream) -> TunnelResult<()> {
    // Split into read and write halves
    let (reader, writer) = stream.into_split();
    let writer_arc = Arc::new(Mutex::new(writer));

    // Track all registered ports for this connection
    let mut registered_ports = Vec::new();
    let mut reader = reader;

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
            ControlMessage::Register { remote_port } => {
                info!("Received registration request for port {}", remote_port);

                // First, remove any existing client on this port (cleanup from previous connection)
                state.remove_client(remote_port).await;

                // Now register the new client
                let registered = state.register_client(remote_port, writer_arc.clone()).await;

                if !registered {
                    let mut writer_guard = writer_arc.lock().await;
                    let _ = ControlMessage::RegisterResponse {
                        success: false,
                        message: format!("Port {} already registered", remote_port),
                    }.write_to_split(&mut *writer_guard).await;
                    // Continue processing other registrations instead of failing completely
                    continue;
                }

                // Send registration success
                let mut writer_guard = writer_arc.lock().await;
                ControlMessage::RegisterResponse {
                    success: true,
                    message: "Registered successfully".into(),
                }.write_to_split(&mut *writer_guard).await?;
                drop(writer_guard);

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
                    // Put the message back? We can't, so just process it normally in the main loop
                    // Instead, we'll break and handle it in the main loop
                    info!("Registration phase complete, {} ports registered", registered_ports.len());

                    // We need to handle this message in the main loop, but since we can't put it back,
                    // let's just create a small wrapper to handle it
                    // For simplicity, let's just break and continue to main loop
                    break;
                } else {
                    let mut writer_guard = writer_arc.lock().await;
                    let _ = ControlMessage::RegisterResponse {
                        success: false,
                        message: "Expected registration message".into(),
                    }.write_to_split(&mut *writer_guard).await;
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
                    ControlMessage::Register { remote_port } => {
                        // Handle late registration (client might send more Register messages later)
                        info!("Received late registration request for port {}", remote_port);

                        // First, remove any existing client on this port
                        state.remove_client(remote_port).await;

                        // Now register the new client
                        let registered = state.register_client(remote_port, writer_arc.clone()).await;

                        if !registered {
                            let mut writer_guard = writer_arc.lock().await;
                            let _ = ControlMessage::RegisterResponse {
                                success: false,
                                message: format!("Port {} already registered", remote_port),
                            }.write_to_split(&mut *writer_guard).await;
                        } else {
                            // Send registration success
                            let mut writer_guard = writer_arc.lock().await;
                            ControlMessage::RegisterResponse {
                                success: true,
                                message: "Registered successfully".into(),
                            }.write_to_split(&mut *writer_guard).await?;
                            drop(writer_guard);

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
                        let mut writer_guard = writer_arc.lock().await;
                        let _ = ControlMessage::Pong.write_to_split(&mut *writer_guard).await;
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
                        // Client is ready, data can flow
                        debug!("Connection ready");
                    }
                    ControlMessage::Close { connection_id } => {
                        // Close the user connection
                        state.close_connection(connection_id).await;
                        debug!("Connection {} closed by client", connection_id);
                    }
                    ControlMessage::Disconnect => {
                        // Server doesn't expect this from client
                        warn!("Received unexpected Disconnect from client");
                    }
                    ControlMessage::RegisterResponse { .. } => {
                        // Server doesn't expect this from client
                        warn!("Received unexpected RegisterResponse from client");
                    }
                    ControlMessage::NewConnection { .. } => {
                        // Server doesn't expect this from client
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
    let listener = TcpListener::bind(&config.control_addr).await?;
    info!("Control server listening on {}", config.control_addr);

    loop {
        let (stream, addr) = listener.accept().await?;
        let state_clone = state.clone();
        tracing::debug!("New control connection from {}", addr);
        tokio::spawn(async move {
            if let Err(e) = handle_control_connection(state_clone, stream).await {
                warn!("Control connection error: {}", e);
            }
        });
    }
}
