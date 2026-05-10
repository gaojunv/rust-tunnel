use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time;
use tracing::{info, debug, warn, error};

use crate::common::{ControlMessage, TunnelError, TunnelResult, connect_tls_insecure};
use crate::client::{ClientConfig, proxy};

/// Type alias for the control message sender
pub type ControlSender = mpsc::Sender<ControlMessage>;

/// Information about an active local connection
struct ActiveLocalConnection {
    /// Local TCP stream writer half connected to target service
    local_writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
}

/// Client state shared between all tasks
#[derive(Clone)]
pub struct ClientState {
    pub config: ClientConfig,
    /// Sender for control messages to server
    pub control_sender: ControlSender,
    pub forwards: Vec<ForwardRule>,
    active_connections: Arc<Mutex<HashMap<u64, ActiveLocalConnection>>>,
}

impl ClientState {
    fn new(config: ClientConfig, control_sender: ControlSender, forwards: Vec<ForwardRule>) -> Self {
        Self {
            config,
            control_sender,
            forwards,
            active_connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn add_connection(&self, connection_id: u64, stream: Box<dyn AsyncWrite + Unpin + Send>) {
        let mut conns = self.active_connections.lock().await;
        conns.insert(connection_id, ActiveLocalConnection {
            local_writer: Arc::new(Mutex::new(stream)),
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
async fn start_heartbeat(sender: ControlSender) {
    let mut interval = time::interval(time::Duration::from_secs(30));
    let mut seq = 0u32;
    loop {
        interval.tick().await;
        let timestamp_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        seq = seq.wrapping_add(1);
        if let Err(e) = sender.send(ControlMessage::Ping { seq, timestamp_micros }).await {
            warn!("Failed to send ping: {}", e);
            break;
        }
        debug!("Sent heartbeat ping seq={}", seq);
    }
}

/// Process messages from server on control channel
async fn process_control_messages<R: AsyncRead + Unpin>(reader: &mut R, state: ClientState) -> TunnelResult<()> {
    loop {
        match ControlMessage::read_from_stream(reader).await {
            Ok(Some(msg)) => {
                match msg {
                    ControlMessage::Pong { seq, ping_timestamp_micros, pong_timestamp_micros } => {
                        let client_rtt_micros = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_micros() as u64)
                            .unwrap_or(0)
                            .wrapping_sub(ping_timestamp_micros);
                        let server_processing_time = pong_timestamp_micros.wrapping_sub(ping_timestamp_micros);
                        debug!("Received heartbeat pong seq={} rtt={}us server_processing={}us",
                               seq, client_rtt_micros, server_processing_time);
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
                    ControlMessage::Disconnect => {
                        info!("Server requested disconnect. Shutting down...");
                        return Err(TunnelError::Protocol("Server requested disconnect".into()));
                    }
                    _ => {
                        warn!("Unexpected message from server: {:?}", msg);
                    }
                }
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
    // Connect to server with or without TLS
    let (mut reader, mut writer): (Box<dyn AsyncRead + Unpin + Send>, Box<dyn AsyncWrite + Unpin + Send>) = if config.tls {
        info!("Connecting to server {} with TLS (insecure mode - accepting self-signed certs)", config.server);

        // Extract hostname for TLS SNI
        let tls_server_name = config.tls_server_name.as_ref()
            .unwrap_or(&config.server)
            .split(':')
            .next()
            .unwrap_or("localhost");

        let tls_stream = connect_tls_insecure(&config.server, tls_server_name).await?;
        info!("TLS connection established successfully");
        let (r, w) = tokio::io::split(tls_stream);
        (Box::new(r), Box::new(w))
    } else {
        info!("Connecting to server {} without TLS", config.server);
        let stream = TcpStream::connect(&config.server).await?;
        info!("TCP connection established");
        let (r, w) = tokio::io::split(stream);
        (Box::new(r), Box::new(w))
    };

    // Get hostname
    let hostname = gethostname::gethostname()
        .into_string()
        .ok();

    // Register all forward rules
    for rule in &forwards {
        ControlMessage::Register {
            remote_port: rule.remote_port,
            hostname: hostname.clone(),
            auth_token: config.auth_token.clone(),
        }.write_to_stream(&mut writer).await?;

        // Read registration response
        let resp = match ControlMessage::read_from_stream(&mut reader).await {
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

    // Create message channel for sending messages to server
    let (sender, mut receiver) = mpsc::channel::<ControlMessage>(32);

    // Spawn writer task
    tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            if let Err(e) = msg.write_to_stream(&mut writer).await {
                debug!("Failed to write control message: {}", e);
                break;
            }
        }
    });

    let state = ClientState::new(config, sender.clone(), forwards);

    // Start heartbeat task
    tokio::spawn(start_heartbeat(sender));

    // Process incoming messages from server
    process_control_messages(&mut reader, state).await?;

    warn!("Control connection terminated");
    Ok(())
}
