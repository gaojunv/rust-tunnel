use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::client::logs::{spawn_log_forwarder, ClientLogLayer};
use crate::client::{proxy, ClientConfig};
use crate::common::{
    connect_tls_insecure, init_logging_with_layer, ClientLogEntry, ControlMessage, MeshServiceDef,
    TunnelError, TunnelResult,
};

/// Stores the global log layer so it can be reused across reconnections.
/// On reconnect the inner sender is hot-swapped via [`ClientLogLayer::set_sender`].
static LOG_LAYER: std::sync::OnceLock<ClientLogLayer> = std::sync::OnceLock::new();

/// Type alias for the control message sender
pub type ControlSender = mpsc::Sender<ControlMessage>;

/// State of a local connection being established
enum LocalConnectionState {
    /// Waiting for local connection to be established; incoming data is buffered
    Pending(Vec<Vec<u8>>),
    /// Active connection with a writer to the local service
    Active(Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>),
}

/// Information about a local connection (pending or active)
struct ActiveLocalConnection {
    state: LocalConnectionState,
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
    fn new(
        config: ClientConfig,
        control_sender: ControlSender,
        forwards: Vec<ForwardRule>,
    ) -> Self {
        Self {
            config,
            control_sender,
            forwards,
            active_connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn add_pending_connection(&self, connection_id: u64) {
        let mut conns = self.active_connections.lock().await;
        conns.insert(
            connection_id,
            ActiveLocalConnection {
                state: LocalConnectionState::Pending(Vec::new()),
            },
        );
    }

    /// Transition a pending connection to active, flushing any buffered data.
    /// Returns false if the connection was removed (e.g., Close received while connecting).
    pub async fn activate_connection(
        &self,
        connection_id: u64,
        stream: Box<dyn AsyncWrite + Unpin + Send>,
    ) -> bool {
        let mut conns = self.active_connections.lock().await;
        match conns.get_mut(&connection_id) {
            Some(conn) => {
                match std::mem::replace(
                    &mut conn.state,
                    LocalConnectionState::Active(Arc::new(Mutex::new(stream))),
                ) {
                    LocalConnectionState::Pending(buffered) => {
                        if !buffered.is_empty() {
                            if let LocalConnectionState::Active(writer) = &conn.state {
                                let mut writer = writer.lock().await;
                                for data in buffered {
                                    let _ = writer.write_all(&data).await;
                                }
                                let _ = writer.flush().await;
                            }
                        }
                        true
                    }
                    LocalConnectionState::Active(_) => {
                        debug!(
                            "Connection {} already active, ignoring duplicate activation",
                            connection_id
                        );
                        false
                    }
                }
            }
            None => {
                debug!(
                    "Connection {} not found during activation (may have been closed)",
                    connection_id
                );
                false
            }
        }
    }

    pub async fn remove_connection(&self, connection_id: u64) {
        let mut conns = self.active_connections.lock().await;
        conns.remove(&connection_id);
    }

    pub async fn deliver_data(&self, connection_id: u64, data: Vec<u8>) -> TunnelResult<()> {
        let mut conns = self.active_connections.lock().await;
        if let Some(conn) = conns.get_mut(&connection_id) {
            match &mut conn.state {
                LocalConnectionState::Pending(buffer) => {
                    buffer.push(data);
                    Ok(())
                }
                LocalConnectionState::Active(writer) => {
                    let mut writer = writer.lock().await;
                    writer.write_all(&data).await?;
                    writer.flush().await?;
                    Ok(())
                }
            }
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
        // Skip first tick - send ping immediately on connection
        if seq > 0 {
            interval.tick().await;
        }
        let timestamp_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        seq = seq.wrapping_add(1);
        if let Err(e) = sender
            .send(ControlMessage::Ping {
                seq,
                timestamp_micros,
            })
            .await
        {
            warn!("Failed to send ping: {}", e);
            break;
        }
        debug!("Sent heartbeat ping seq={}", seq);
    }
}

/// Process messages from server on control channel
async fn process_control_messages<R: AsyncRead + Unpin>(
    reader: &mut R,
    state: ClientState,
) -> TunnelResult<()> {
    loop {
        match ControlMessage::read_from_stream(reader).await {
            Ok(Some(msg)) => {
                match msg {
                    ControlMessage::Pong {
                        seq,
                        ping_timestamp_micros,
                        pong_timestamp_micros,
                    } => {
                        let client_rtt_micros = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_micros() as u64)
                            .unwrap_or(0)
                            .wrapping_sub(ping_timestamp_micros);
                        let server_processing_time =
                            pong_timestamp_micros.wrapping_sub(ping_timestamp_micros);
                        debug!(
                            "Received heartbeat pong seq={} rtt={}us server_processing={}us",
                            seq, client_rtt_micros, server_processing_time
                        );
                    }
                    ControlMessage::NewConnection {
                        connection_id,
                        remote_port,
                    } => {
                        info!(
                            "New connection request id {} for remote port {}",
                            connection_id, remote_port
                        );
                        // Pre-register as pending so Data messages are buffered instead of dropped
                        state.add_pending_connection(connection_id).await;
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = proxy::handle_new_connection(
                                state_clone,
                                connection_id,
                                remote_port,
                            )
                            .await
                            {
                                warn!("Failed to handle new connection {}: {}", connection_id, e);
                            }
                        });
                    }
                    ControlMessage::Data {
                        connection_id,
                        data,
                    } => {
                        if let Err(e) = state.deliver_data(connection_id, data).await {
                            warn!(
                                "Failed to deliver data to connection {}: {}",
                                connection_id, e
                            );
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
                    ControlMessage::MeshMemberList { mesh_id, members } => {
                        debug!(
                            "Mesh '{}' member list updated: {} member(s)",
                            mesh_id,
                            members.len()
                        );
                    }
                    ControlMessage::MeshConnect {
                        target_client: _,
                        service_name,
                    } => {
                        info!(
                            "Mesh connect request for service '{}' (not yet implemented)",
                            service_name
                        );
                    }
                    ControlMessage::P2PResponse {
                        target_client: _,
                        remote_addr,
                    } => {
                        debug!("P2P response with remote addr: {}", remote_addr);
                    }
                    ControlMessage::MeshRelay {
                        target_client: _,
                        data: _,
                    } => {
                        debug!("Received mesh relay data");
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
    let (mut reader, mut writer): (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) = if config.tls {
        info!(
            "Connecting to server {} with TLS (insecure mode - accepting self-signed certs)",
            config.server
        );

        // Extract hostname for TLS SNI
        let tls_server_name = config
            .tls_server_name
            .as_ref()
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
    let hostname = gethostname::gethostname().into_string().ok();

    // Register all forward rules
    for rule in &forwards {
        ControlMessage::Register {
            remote_port: rule.remote_port,
            hostname: hostname.clone(),
            auth_token: config.auth_token.clone(),
        }
        .write_to_stream(&mut writer)
        .await?;

        // Read registration response
        let resp = match ControlMessage::read_from_stream(&mut reader).await {
            Ok(Some(ControlMessage::RegisterResponse { success, message })) => (success, message),
            Ok(Some(_)) => {
                return Err(TunnelError::Protocol(
                    "Expected registration response".into(),
                ));
            }
            Ok(None) => {
                return Err(TunnelError::Protocol(
                    "Connection closed during registration".into(),
                ));
            }
            Err(e) => return Err(e),
        };

        if !resp.0 {
            return Err(TunnelError::ControlChannel(format!(
                "Registration failed for port {}: {}",
                rule.remote_port, resp.1
            )));
        }

        info!(
            "Registration successful for remote port {} -> {}",
            rule.remote_port, rule.local_addr
        );
    }

    // Mesh network registration
    if let Some(ref mesh_id) = config.mesh {
        let mesh_name = config
            .mesh_name
            .clone()
            .unwrap_or_else(|| hostname.clone().unwrap_or_else(|| "unknown".into()));

        ControlMessage::MeshJoin {
            mesh_id: mesh_id.clone(),
            client_name: mesh_name,
        }
        .write_to_stream(&mut writer)
        .await?;

        info!("Joined mesh network: {}", mesh_id);

        // Register mesh services
        let services: Vec<MeshServiceDef> = config
            .mesh_services
            .iter()
            .filter_map(|s| {
                let mut parts = s.splitn(3, ':');
                Some(MeshServiceDef {
                    name: parts.next()?.to_string(),
                    protocol: parts.next()?.to_string(),
                    local_addr: parts.next()?.to_string(),
                })
            })
            .collect();

        if !services.is_empty() {
            ControlMessage::MeshRegisterServices {
                mesh_id: mesh_id.clone(),
                services,
            }
            .write_to_stream(&mut writer)
            .await?;

            info!("Registered {} mesh service(s)", config.mesh_services.len());
        }
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

    // --- Client log capture setup ---
    let log_ctrl_sender = sender.clone();
    let (log_tx, log_rx) = mpsc::unbounded_channel::<ClientLogEntry>();

    let layer = ClientLogLayer::new();
    if LOG_LAYER.set(layer).is_ok() {
        // First connection: the layer was stored; now initialise logging with it.
        let stored = LOG_LAYER.get().unwrap();
        init_logging_with_layer(&config.log, stored.clone());
    }

    if let Some(stored) = LOG_LAYER.get() {
        stored.set_sender(log_tx);
        spawn_log_forwarder(log_rx, log_ctrl_sender);
    }
    // --- End log capture setup ---

    let state = ClientState::new(config, sender.clone(), forwards);

    // Start heartbeat task
    tokio::spawn(start_heartbeat(sender));

    // Process incoming messages from server
    process_control_messages(&mut reader, state).await?;

    warn!("Control connection terminated");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ForwardRule;
    use crate::common::ControlMessage;

    fn create_test_state() -> ClientState {
        let config = ClientConfig {
            server: "localhost:8080".to_string(),
            forwards: vec!["8080:localhost:80".to_string()],
            auth_token: None,
            tls: false,
            tls_server_name: None,
            tls_insecure: true,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            log: "info".to_string(),
        };
        let (sender, _) = mpsc::channel(32);
        let forwards = vec![ForwardRule {
            remote_port: 8080,
            local_addr: "localhost:80".to_string(),
            dns_name: None,
        }];
        ClientState::new(config, sender, forwards)
    }

    #[tokio::test]
    async fn test_client_state_add_and_remove_connection() {
        let state = create_test_state();

        state.add_pending_connection(42).await;
        let mock_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(Vec::new());
        assert!(state.activate_connection(42, mock_writer).await);

        // Connection should exist - verify by trying to deliver data
        let result = state.deliver_data(42, vec![1, 2, 3]).await;
        assert!(result.is_ok());

        state.remove_connection(42).await;

        // After removal, deliver_data should still return Ok (just no-op)
        let result = state.deliver_data(42, vec![1, 2, 3]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_client_state_deliver_data_nonexistent() {
        let state = create_test_state();

        // Delivering to a non-existent connection should return Ok (no-op)
        let result = state.deliver_data(9999, vec![1, 2, 3]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_client_state_close_connection() {
        let state = create_test_state();

        state.add_pending_connection(42).await;
        let mock_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(Vec::new());
        assert!(state.activate_connection(42, mock_writer).await);

        state.close_connection(42).await;

        // After close, deliver_data should return Ok (no-op)
        let result = state.deliver_data(42, vec![1, 2, 3]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_client_state_close_nonexistent_connection() {
        let state = create_test_state();
        // Should not panic
        state.close_connection(9999).await;
    }

    #[tokio::test]
    async fn test_client_state_clone() {
        let state = create_test_state();
        let cloned = state.clone();

        cloned.add_pending_connection(100).await;
        let mock_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(Vec::new());
        assert!(cloned.activate_connection(100, mock_writer).await);

        // Should be visible from original (shared state)
        let result = state.deliver_data(100, vec![1, 2, 3]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_client_state_multiple_connections() {
        let state = create_test_state();

        for i in 0..5 {
            state.add_pending_connection(i).await;
            let mock_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(Vec::new());
            assert!(state.activate_connection(i, mock_writer).await);
        }

        // All should be deliverable
        for i in 0..5 {
            let result = state.deliver_data(i, vec![1]).await;
            assert!(result.is_ok());
        }

        // Remove one
        state.remove_connection(2).await;
        let result = state.deliver_data(2, vec![1]).await;
        assert!(result.is_ok()); // Returns Ok but no-op
    }

    #[tokio::test]
    async fn test_pending_connection_buffers_data() {
        let state = create_test_state();

        // Add pending connection
        state.add_pending_connection(42).await;

        // Deliver data while pending - should buffer, not error
        let result = state.deliver_data(42, vec![1, 2, 3]).await;
        assert!(result.is_ok());
        let result = state.deliver_data(42, vec![4, 5, 6]).await;
        assert!(result.is_ok());

        // Activate - buffered data should be flushed
        let mock_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(Vec::new());
        assert!(state.activate_connection(42, mock_writer).await);

        // Now data should go directly to writer
        let result = state.deliver_data(42, vec![7, 8, 9]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_activate_removed_connection() {
        let state = create_test_state();

        state.add_pending_connection(42).await;
        state.remove_connection(42).await;

        let mock_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(Vec::new());
        assert!(!state.activate_connection(42, mock_writer).await);
    }

    #[tokio::test]
    async fn test_process_control_messages_close() {
        let state = create_test_state();

        // Simulate server sending Close message
        let mut buffer = Vec::new();
        ControlMessage::Close { connection_id: 42 }
            .write_to_stream(&mut buffer)
            .await
            .unwrap();

        // Add connection first so close has something to close
        state.add_pending_connection(42).await;
        let mock_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(Vec::new());
        assert!(state.activate_connection(42, mock_writer).await);

        let mut reader = &buffer[..];
        let result = process_control_messages(&mut reader, state.clone()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_process_control_messages_disconnect() {
        let state = create_test_state();

        let mut buffer = Vec::new();
        ControlMessage::Disconnect
            .write_to_stream(&mut buffer)
            .await
            .unwrap();

        let mut reader = &buffer[..];
        let result = process_control_messages(&mut reader, state).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            TunnelError::Protocol(msg) => assert!(msg.contains("disconnect")),
            _ => panic!("Expected Protocol error"),
        }
    }

    #[tokio::test]
    async fn test_process_control_messages_eof() {
        let state = create_test_state();
        let mut reader = &[] as &[u8];
        let result = process_control_messages(&mut reader, state).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_process_control_messages_pong() {
        let state = create_test_state();

        let mut buffer = Vec::new();
        ControlMessage::Pong {
            seq: 1,
            ping_timestamp_micros: 1000,
            pong_timestamp_micros: 2000,
        }
        .write_to_stream(&mut buffer)
        .await
        .unwrap();

        let mut reader = &buffer[..];
        let result = process_control_messages(&mut reader, state).await;
        assert!(result.is_ok());
    }
}
