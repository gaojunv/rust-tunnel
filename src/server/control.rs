use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;
use tracing::{info, warn, error, debug};

use crate::common::{ControlMessage, TunnelError, TunnelResult};
use crate::server::{ServerConfig, listener};

/// Information about a connected client
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Remote port that client wants to expose
    pub remote_port: u16,
    /// Sender half of the control channel for sending messages to client
    pub control_stream: Arc<Mutex<TcpStream>>,
}

/// Information about an active connection between user and client
#[derive(Debug)]
struct ActiveConnection {
    /// User TCP stream writer half
    user_writer: Arc<Mutex<OwnedWriteHalf>>,
}

/// Global server state shared between all tasks
#[derive(Clone)]
pub struct ServerState {
    /// Map from remote port to client info
    clients: Arc<Mutex<HashMap<u16, ClientInfo>>>,
    /// Map from connection_id to active connection info
    active_connections: Arc<Mutex<HashMap<u64, ActiveConnection>>>,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            active_connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn register_client(&self, remote_port: u16, control_stream: Arc<Mutex<TcpStream>>) -> bool {
        let mut clients = self.clients.lock().await;
        if clients.contains_key(&remote_port) {
            return false;
        }
        clients.insert(remote_port, ClientInfo {
            remote_port,
            control_stream,
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
            writer.write_all(&data).await?;
            writer.flush().await?;
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
}

/// Handle a single control connection from client
async fn handle_control_connection(state: ServerState, stream: TcpStream) -> TunnelResult<()> {
    let stream_arc = Arc::new(Mutex::new(stream));
    let mut stream_guard = stream_arc.lock().await;

    // Read registration message
    let msg = match ControlMessage::read_from_stream(&mut stream_guard).await {
        Ok(Some(msg)) => msg,
        Ok(None) => {
            return Err(TunnelError::Protocol("Connection closed before registration".into()));
        }
        Err(e) => return Err(e),
    };

    let (remote_port, registered) = match msg {
        ControlMessage::Register { remote_port } => {
            info!("Received registration request for port {}", remote_port);
            (remote_port, state.register_client(remote_port, stream_arc.clone()).await)
        }
        _ => {
            let _ = ControlMessage::RegisterResponse {
                success: false,
                message: "Expected registration message".into(),
            }.write_to_stream(&mut stream_guard).await;
            return Err(TunnelError::Protocol("Expected registration message".into()));
        }
    };

    drop(stream_guard);

    if !registered {
        let mut stream_guard = stream_arc.lock().await;
        let _ = ControlMessage::RegisterResponse {
            success: false,
            message: format!("Port {} already registered", remote_port),
        }.write_to_stream(&mut stream_guard).await;
        return Err(TunnelError::Protocol(format!("Port {} already registered", remote_port)));
    }

    // Send registration success
    let mut stream_guard = stream_arc.lock().await;
    ControlMessage::RegisterResponse {
        success: true,
        message: "Registered successfully".into(),
    }.write_to_stream(&mut stream_guard).await?;
    drop(stream_guard);

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
    loop {
        // Acquire lock before each read, release after reading one message
        // This allows other tasks to use the control channel for sending messages
        let mut stream_guard = stream_arc.lock().await;
        match ControlMessage::read_from_stream(&mut stream_guard).await {
            Ok(Some(msg)) => {
                drop(stream_guard);
                match msg {
                    ControlMessage::Ping => {
                        let mut stream_guard = stream_arc.lock().await;
                        let _ = ControlMessage::Pong.write_to_stream(&mut stream_guard).await;
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
pub async fn run_server(config: ServerConfig) -> TunnelResult<()> {
    let state = ServerState::new();
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
