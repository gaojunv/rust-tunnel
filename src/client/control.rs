use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time;
use tracing::{info, debug, warn, error};

use crate::common::{ControlMessage, TunnelError, TunnelResult};
use crate::client::{ClientConfig, proxy};

use tokio::net::tcp::OwnedWriteHalf;

/// Information about an active local connection
struct ActiveLocalConnection {
    /// Local TCP stream writer half connected to target service
    local_writer: Arc<Mutex<OwnedWriteHalf>>,
}

/// Client state shared between all tasks
#[derive(Clone)]
pub struct ClientState {
    pub config: ClientConfig,
    pub control_stream: Arc<Mutex<TcpStream>>,
    pub forwards: Vec<ForwardRule>,
    active_connections: Arc<Mutex<HashMap<u64, ActiveLocalConnection>>>,
}

impl ClientState {
    fn new(config: ClientConfig, control_stream: TcpStream, forwards: Vec<ForwardRule>) -> Self {
        Self {
            config,
            control_stream: Arc::new(Mutex::new(control_stream)),
            forwards,
            active_connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn add_connection(&self, connection_id: u64, stream: Arc<Mutex<OwnedWriteHalf>>) {
        let mut conns = self.active_connections.lock().await;
        conns.insert(connection_id, ActiveLocalConnection {
            local_writer: stream,
        });
    }

    pub async fn remove_connection(&self, connection_id: u64) {
        let mut conns = self.active_connections.lock().await;
        conns.remove(&connection_id);
    }

    pub async fn deliver_data(&self, connection_id: u64, data: Vec<u8>) -> TunnelResult<()> {
        let conns = self.active_connections.lock().await;
        if let Some(conn) = conns.get(&connection_id) {
            let mut writer = conn.local_writer.lock().await;
            writer.write_all(&data).await?;
            writer.flush().await?;
            Ok(())
        } else {
            debug!("No active local connection found for id {}", connection_id);
            Ok(())
        }
    }

    pub async fn close_connection(&self, connection_id: u64) {
        self.remove_connection(connection_id).await;
    }
}

/// Start the heartbeat task that sends periodic ping to keep connection alive
async fn start_heartbeat(state: ClientState) {
    let mut interval = time::interval(time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        let mut control_guard = state.control_stream.lock().await;
        if let Err(e) = ControlMessage::Ping.write_to_stream(&mut control_guard).await {
            warn!("Failed to send ping: {}", e);
            break;
        }
        debug!("Sent heartbeat ping");
    }
}

/// Process messages from server on control channel
async fn process_control_messages(state: ClientState) -> TunnelResult<()> {
    let mut control_guard = state.control_stream.lock().await;
    loop {
        match ControlMessage::read_from_stream(&mut control_guard).await {
            Ok(Some(msg)) => {
                drop(control_guard);
                match msg {
                    ControlMessage::Pong => {
                        debug!("Received heartbeat pong");
                    }
                    ControlMessage::NewConnection { connection_id, remote_port } => {
                        info!("New connection request id {} for remote port {}", connection_id, remote_port);
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = proxy::handle_new_connection(state_clone, connection_id, remote_port).await {
                                warn!("Failed to handle new connection {}: {}", connection_id, e);
                            }
                        });
                    }
                    ControlMessage::Data { connection_id, data } => {
                        if let Err(e) = state.deliver_data(connection_id, data).await {
                            warn!("Failed to deliver data to connection {}: {}", connection_id, e);
                        }
                    }
                    ControlMessage::Close { connection_id } => {
                        info!("Connection {} closed by server", connection_id);
                        state.close_connection(connection_id).await;
                    }
                    _ => {
                        warn!("Unexpected message from server: {:?}", msg);
                    }
                }
                control_guard = state.control_stream.lock().await;
            }
            Ok(None) => {
                info!("Server closed control connection");
                break;
            }
            Err(e) => {
                error!("Error reading from control connection: {}", e);
                break;
            }
        }
    }
    Ok(())
}

use crate::client::ForwardRule;

/// Main client entry point
pub async fn run_client(config: ClientConfig, forwards: Vec<ForwardRule>) -> TunnelResult<()> {
    // Connect to server
    let mut stream = TcpStream::connect(&config.server).await?;
    info!("Connected to server at {}", config.server);

    // Register all forward rules
    for rule in &forwards {
        ControlMessage::Register {
            remote_port: rule.remote_port,
        }.write_to_stream(&mut stream).await?;

        // Read registration response
        let resp = match ControlMessage::read_from_stream(&mut stream).await {
            Ok(Some(ControlMessage::RegisterResponse { success, message })) => {
                (success, message)
            }
            Ok(Some(_)) => {
                return Err(TunnelError::Protocol("Expected registration response".into()));
            }
            Ok(None) => {
                return Err(TunnelError::Protocol("Connection closed during registration".into()));
            }
            Err(e) => return Err(e),
        };

        if !resp.0 {
            return Err(TunnelError::ControlChannel(format!(
                "Registration failed for port {}: {}",
                rule.remote_port, resp.1
            )));
        }

        info!("Registration successful for remote port {} -> {}",
              rule.remote_port, rule.local_addr);
    }

    let state = ClientState::new(config, stream, forwards);

    // Start heartbeat task
    let heartbeat_state = state.clone();
    tokio::spawn(start_heartbeat(heartbeat_state));

    // Process incoming messages from server
    process_control_messages(state).await?;

    warn!("Control connection terminated");
    Ok(())
}
