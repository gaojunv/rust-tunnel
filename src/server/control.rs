use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
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

    pub async fn add_active_connection(&self, connection_id: u64, user_writer: Arc<Mutex<OwnedWriteHalf>>) {
        let mut active_connections = self.active_connections.lock().await;
        active_connections.insert(connection_id, ActiveConnection {
            user_writer,
        });
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
            writer.write_all(&data).await?;
            writer.flush().await?;
            drop(writer);
            drop(active_connections);

            // Find which port this connection belongs to and record traffic
            let clients = self.clients.lock().await;
            for (&port, _) in clients.iter() {
                // We don't track which connection is on which port, so just record to all for now
                // This is imperfect but better than nothing
                self.traffic_store.record_bytes_out(port, bytes as u64).await;
            }
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

    // Read registration message
    let mut reader = reader;
    let msg = match ControlMessage::read_from_stream(&mut reader).await {
        Ok(Some(msg)) => msg,
        Ok(None) => {
            return Err(TunnelError::Protocol("Connection closed before registration".into()));
        }
        Err(e) => return Err(e),
    };

    let (remote_port, registered) = match msg {
        ControlMessage::Register { remote_port } => {
            info!("Received registration request for port {}", remote_port);
            (remote_port, state.register_client(remote_port, writer_arc.clone()).await)
        }
        _ => {
            let mut writer_guard = writer_arc.lock().await;
            let _ = ControlMessage::RegisterResponse {
                success: false,
                message: "Expected registration message".into(),
            }.write_to_split(&mut *writer_guard).await;
            return Err(TunnelError::Protocol("Expected registration message".into()));
        }
    };

    if !registered {
        let mut writer_guard = writer_arc.lock().await;
        let _ = ControlMessage::RegisterResponse {
            success: false,
            message: format!("Port {} already registered", remote_port),
        }.write_to_split(&mut *writer_guard).await;
        return Err(TunnelError::Protocol(format!("Port {} already registered", remote_port)));
    }

    // Send registration success
    let mut writer_guard = writer_arc.lock().await;
    ControlMessage::RegisterResponse {
        success: true,
        message: "Registered successfully".into(),
    }.write_to_split(&mut *writer_guard).await?;
    drop(writer_guard);

    info!("Client registered for port {}", remote_port);

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

    // Keep connection alive and process messages (heartbeats, data routing)
    // Only reading happens here, writing goes through the shared writer_arc
    loop {
        match ControlMessage::read_from_stream(&mut reader).await {
            Ok(Some(msg)) => {
                match msg {
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
                    _ => {
                        warn!("Unexpected message on control channel: {:?}", msg);
                    }
                }
            }
            Ok(None) => {
                // Connection closed
                break;
            }
            Err(e) => {
                error!("Error reading from control channel: {}", e);
                break;
            }
        }
    }

    Ok(())
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
