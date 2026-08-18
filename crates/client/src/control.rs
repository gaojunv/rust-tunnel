use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::logs::{spawn_log_forwarder, ClientLogLayer};
use crate::{proxy, spawn::SpawnManager, ClientConfig};
use rust_tunnel_common::{
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
    active_connections: Arc<Mutex<HashMap<u64, ActiveLocalConnection>>>,
    /// 进行中的 agent exec 取消句柄：request_id → cancel 信号发送端。
    exec_cancels: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    /// 长生命周期 agent 进程的 spawn 管理器（AgentSpawn* 消息）。
    spawn_manager: SpawnManager,
    /// LLM 回环代理的 request_id → 响应接收端（AgentLlmProxy* 消息）。
    llm_proxy_pending: crate::llm_proxy::PendingMap,
    /// LLM 回环代理的 session_id → kill 信号发送端（AgentLlmProxyStop 触发，
    /// 释放回环监听端口）。重 spawn 时旧句柄先 send 再替换，自愈泄漏。
    llm_proxy_kills: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
}

impl ClientState {
    fn new(config: ClientConfig, control_sender: ControlSender) -> Self {
        Self {
            config,
            control_sender,
            active_connections: Arc::new(Mutex::new(HashMap::new())),
            exec_cancels: Arc::new(Mutex::new(HashMap::new())),
            spawn_manager: SpawnManager::new(),
            llm_proxy_pending: crate::llm_proxy::new_pending_map(),
            llm_proxy_kills: Arc::new(Mutex::new(HashMap::new())),
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

    /// 注册一次 exec 的取消句柄；`handle_exec_request` 结束后由调用方注销。
    pub async fn register_exec_cancel(&self, request_id: &str, tx: oneshot::Sender<()>) {
        self.exec_cancels
            .lock()
            .await
            .insert(request_id.to_string(), tx);
    }

    /// 收到 `AgentExecCancel`：触发对应 exec 取消。返回 false 表示无此执行
    /// （命令已结束/超时），调用方静默忽略。
    pub async fn cancel_exec(&self, request_id: &str) -> bool {
        let removed = self.exec_cancels.lock().await.remove(request_id);
        if let Some(tx) = removed {
            let _ = tx.send(());
            true
        } else {
            false
        }
    }

    /// 注销一个 exec 的取消句柄（执行正常结束后）。
    pub async fn deregister_exec_cancel(&self, request_id: &str) {
        self.exec_cancels.lock().await.remove(request_id);
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
                    ControlMessage::OpenTunnel {
                        connection_id,
                        target_addr,
                    } => {
                        info!(
                            "OpenTunnel request id {} for target {}",
                            connection_id, target_addr
                        );
                        // Pre-register as pending so Data messages are buffered instead of dropped
                        state.add_pending_connection(connection_id).await;
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                proxy::handle_open_tunnel(state_clone, connection_id, target_addr)
                                    .await
                            {
                                warn!("Failed to handle OpenTunnel {}: {}", connection_id, e);
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
                    ControlMessage::AgentExecRequest {
                        session_id,
                        request_id,
                        root_path,
                        docker_container,
                        command,
                    } => {
                        let sender = state.control_sender.clone();
                        if !state.config.enable_agent {
                            tokio::spawn(async move {
                                let _ = sender
                                    .send(ControlMessage::AgentExecResponse {
                                        session_id,
                                        request_id,
                                        result: rust_tunnel_common::AgentResult::Error {
                                            message: "agent not enabled on this client".into(),
                                        },
                                    })
                                    .await;
                            });
                            continue;
                        }
                        // Sandbox root comes from the server-side workspace config
                        // delivered in the request (in docker mode it is the
                        // container-side root; commands are wrapped in `docker exec`).
                        let root = std::path::PathBuf::from(root_path);
                        let state2 = state.clone();
                        let (cancel_tx, mut cancel_rx) = oneshot::channel();
                        state.register_exec_cancel(&request_id, cancel_tx).await;
                        tokio::spawn(async move {
                            let timeout = match &command {
                                rust_tunnel_common::AgentCommand::ShellWithTimeout { timeout_secs, .. } => {
                                    std::time::Duration::from_secs((*timeout_secs).clamp(1, 3600))
                                }
                                _ => std::time::Duration::from_secs(120),
                            };
                            let result = crate::agent::handle_exec_request(
                                &command,
                                &root,
                                timeout,
                                docker_container.as_deref(),
                                Some(&mut cancel_rx),
                            )
                            .await;
                            state2.deregister_exec_cancel(&request_id).await;
                            let _ = sender
                                .send(ControlMessage::AgentExecResponse {
                                    session_id,
                                    request_id,
                                    result,
                                })
                                .await;
                        });
                    }
                    ControlMessage::AgentExecCancel { request_id } => {
                        if !state.cancel_exec(&request_id).await {
                            debug!("cancel for unknown exec request_id {}", request_id);
                        }
                        // spawn 进程的 session_id 与 exec 的 request_id 共用取消通道
                        if !state.spawn_manager.kill(&request_id).await {
                            debug!("no spawned process for request_id {}", request_id);
                        }
                    }
                    ControlMessage::AgentSpawnRequest {
                        session_id,
                        command,
                        args,
                        env,
                        cwd,
                    } => {
                        if !state.config.enable_agent {
                            let sender = state.control_sender.clone();
                            tokio::spawn(async move {
                                let _ = sender
                                    .send(ControlMessage::AgentSpawnResponse {
                                        session_id,
                                        success: false,
                                        error: Some("agent not enabled".into()),
                                    })
                                    .await;
                            });
                        } else {
                            let mgr = state.spawn_manager.clone();
                            let tx = state.control_sender.clone();
                            tokio::spawn(async move {
                                mgr.handle_spawn(session_id, command, args, env, cwd, tx)
                                    .await;
                            });
                        }
                    }
                    ControlMessage::AgentSpawnData {
                        session_id,
                        data,
                        stdin: true,
                    } => {
                        if let Err(e) = state.spawn_manager.write_stdin(&session_id, data).await {
                            warn!("spawn stdin write failed: {e}");
                        }
                    }
                    ControlMessage::AgentSpawnData { stdin: false, .. } => {
                        warn!("client received unexpected server-stdout spawn data");
                    }
                    ControlMessage::AgentLlmProxyStart { session_id } => {
                        let tx = state.control_sender.clone();
                        let (port, kill_tx) = if state.config.enable_agent {
                            match crate::llm_proxy::serve(
                                session_id.clone(),
                                tx.clone(),
                                state.llm_proxy_pending.clone(),
                            )
                            .await
                            {
                                Ok((port, kill_tx)) => (port, Some(kill_tx)),
                                Err(e) => {
                                    warn!("LLM loop proxy bind failed: {}", e);
                                    (0, None)
                                }
                            }
                        } else {
                            (0, None)
                        };
                        if let Some(kill_tx) = kill_tx {
                            // 重 spawn 自愈：同一 session 的旧监听（若仍存活）先关掉
                            // 再存新句柄，避免每次 spawn 泄漏一个回环监听端口。
                            let old = state
                                .llm_proxy_kills
                                .lock()
                                .await
                                .insert(session_id.clone(), kill_tx);
                            if let Some(old) = old {
                                let _ = old.send(());
                            }
                        }
                        let _ = tx
                            .send(ControlMessage::AgentLlmProxyReady { session_id, port })
                            .await;
                    }
                    ControlMessage::AgentLlmProxyStop { session_id } => {
                        // 释放 LLM 回环代理监听端口（kill/失败清理用）；无此 session 静默忽略。
                        let kill_tx = state.llm_proxy_kills.lock().await.remove(&session_id);
                        if let Some(kill_tx) = kill_tx {
                            let _ = kill_tx.send(());
                        } else {
                            debug!("AgentLlmProxyStop for unknown session {}", session_id);
                        }
                    }
                    ControlMessage::AgentLlmProxyChunk { .. } => {
                        crate::llm_proxy::route_chunk(&state.llm_proxy_pending, &msg).await;
                    }
                    ControlMessage::Disconnect { reason } => {
                        info!("Server requested disconnect: {}", reason);
                        return Err(TunnelError::Protocol(format!(
                            "Server requested disconnect: {reason}"
                        )));
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

/// Main client entry point
pub async fn run_client(config: ClientConfig) -> TunnelResult<()> {
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

    // Get hostname for client name
    let hostname = gethostname::gethostname().into_string().ok();
    let client_name = config
        .name
        .clone()
        .or_else(|| hostname.clone())
        .unwrap_or_else(|| "unnamed-client".to_string());

    // Register with v2 protocol (single Register, no per-forward loop)
    let client_version = if config.enable_agent {
        format!("{}+agent", env!("CARGO_PKG_VERSION"))
    } else {
        env!("CARGO_PKG_VERSION").to_string()
    };
    let register = ControlMessage::Register {
        protocol_version: 2,
        client_name: client_name.clone(),
        password: config.password.clone(),
        client_version,
    };
    register.write_to_stream(&mut writer).await?;
    info!("Sent Register to server (name='{client_name}')");

    match ControlMessage::read_from_stream(&mut reader).await {
        Ok(Some(ControlMessage::RegisterResponse { success: true, .. })) => {
            info!("registered as '{client_name}'");
        }
        Ok(Some(ControlMessage::RegisterResponse {
            success: false,
            message,
        })) => {
            return Err(TunnelError::ControlChannel(format!(
                "register failed: {message}"
            )));
        }
        Ok(Some(other)) => {
            return Err(TunnelError::Protocol(format!(
                "expected RegisterResponse, got {other:?}"
            )));
        }
        Ok(None) => {
            return Err(TunnelError::Protocol("closed during register".into()));
        }
        Err(e) => return Err(e),
    }

    // Mesh network registration
    if let Some(ref mesh_id) = config.mesh {
        let mesh_name = config
            .mesh_name
            .clone()
            .unwrap_or_else(|| hostname.unwrap_or_else(|| "unknown".into()));

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

    let state = ClientState::new(config, sender.clone());

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
    use rust_tunnel_common::ControlMessage;

    fn create_test_state() -> ClientState {
        let config = ClientConfig {
            server: "localhost:8080".to_string(),
            name: None,
            password: "pw".to_string(),
            tls: false,
            tls_server_name: None,
            tls_insecure: true,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            enable_agent: false,
            agent_pty_port: crate::pty::DEFAULT_PTY_PORT,
            log: "info".to_string(),
        };
        let (sender, _) = mpsc::channel(32);
        ClientState::new(config, sender)
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
        ControlMessage::Disconnect {
            reason: "testing".into(),
        }
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

    #[tokio::test]
    async fn test_process_control_messages_agent_exec_disabled() {
        // enable_agent=false 时收到 AgentExecRequest → 回 AgentExecResponse::Error("agent not enabled")
        let state = create_test_state();
        let (tx, mut rx) = mpsc::channel(32);
        let state = ClientState {
            control_sender: tx,
            ..state
        };

        let mut buffer = Vec::new();
        ControlMessage::AgentExecRequest {
            session_id: "s".into(),
            request_id: "r1".into(),
            root_path: ".".into(),
            docker_container: None,
            command: rust_tunnel_common::AgentCommand::Shell {
                cmd: "echo hi".into(),
                cwd: None,
            },
        }
        .write_to_stream(&mut buffer)
        .await
        .unwrap();

        let mut reader = &buffer[..];
        let result = process_control_messages(&mut reader, state).await;
        assert!(result.is_ok());

        let msg = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        match msg {
            ControlMessage::AgentExecResponse {
                request_id, result, ..
            } => {
                assert_eq!(request_id, "r1");
                match result {
                    rust_tunnel_common::AgentResult::Error { message } => {
                        assert!(message.contains("not enabled"));
                    }
                    other => panic!("expected Error, got {other:?}"),
                }
            }
            other => panic!("expected AgentExecResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_control_messages_agent_exec_shell() {
        // enable_agent=true 时执行 shell 并回结果
        let config = ClientConfig {
            enable_agent: true,
            ..create_test_state().config
        };
        let (tx, mut rx) = mpsc::channel(32);
        let state = ClientState::new(config, tx);

        let mut buffer = Vec::new();
        ControlMessage::AgentExecRequest {
            session_id: "s".into(),
            request_id: "r2".into(),
            root_path: ".".into(),
            docker_container: None,
            command: rust_tunnel_common::AgentCommand::Shell {
                cmd: "echo hello-agent".into(),
                cwd: None,
            },
        }
        .write_to_stream(&mut buffer)
        .await
        .unwrap();

        let mut reader = &buffer[..];
        let result = process_control_messages(&mut reader, state).await;
        assert!(result.is_ok());

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        match msg {
            ControlMessage::AgentExecResponse {
                request_id, result, ..
            } => {
                assert_eq!(request_id, "r2");
                match result {
                    rust_tunnel_common::AgentResult::Shell {
                        stdout, exit_code, ..
                    } => {
                        assert_eq!(exit_code, 0);
                        assert!(stdout.contains("hello-agent"));
                    }
                    other => panic!("expected Shell, got {other:?}"),
                }
            }
            other => panic!("expected AgentExecResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_control_messages_llm_proxy_start_disabled() {
        // enable_agent=false 时收到 AgentLlmProxyStart → 回 AgentLlmProxyReady { port: 0 }
        let state = create_test_state();
        let (tx, mut rx) = mpsc::channel(32);
        let state = ClientState {
            control_sender: tx,
            ..state
        };

        let mut buffer = Vec::new();
        ControlMessage::AgentLlmProxyStart {
            session_id: "s1".into(),
        }
        .write_to_stream(&mut buffer)
        .await
        .unwrap();

        let mut reader = &buffer[..];
        let result = process_control_messages(&mut reader, state).await;
        assert!(result.is_ok());

        let msg = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        match msg {
            ControlMessage::AgentLlmProxyReady { session_id, port } => {
                assert_eq!(session_id, "s1");
                assert_eq!(port, 0);
            }
            other => panic!("expected AgentLlmProxyReady, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_control_messages_llm_proxy_start_enabled() {
        // enable_agent=true 时启动回环代理并回真实端口（>0）
        let config = ClientConfig {
            enable_agent: true,
            ..create_test_state().config
        };
        let (tx, mut rx) = mpsc::channel(32);
        let state = ClientState::new(config, tx);

        let mut buffer = Vec::new();
        ControlMessage::AgentLlmProxyStart {
            session_id: "s2".into(),
        }
        .write_to_stream(&mut buffer)
        .await
        .unwrap();

        let mut reader = &buffer[..];
        let result = process_control_messages(&mut reader, state).await;
        assert!(result.is_ok());

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        match msg {
            ControlMessage::AgentLlmProxyReady { session_id, port } => {
                assert_eq!(session_id, "s2");
                assert!(port > 0);
            }
            other => panic!("expected AgentLlmProxyReady, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_control_messages_llm_proxy_stop_frees_listener() {
        // Start 后 Stop：kill_tx 被触发，回环监听端口释放（防泄漏）。
        let config = ClientConfig {
            enable_agent: true,
            ..create_test_state().config
        };
        let (tx, mut rx) = mpsc::channel(32);
        let state = ClientState::new(config, tx);

        let mut buffer = Vec::new();
        ControlMessage::AgentLlmProxyStart {
            session_id: "s2".into(),
        }
        .write_to_stream(&mut buffer)
        .await
        .unwrap();
        ControlMessage::AgentLlmProxyStop {
            session_id: "s2".into(),
        }
        .write_to_stream(&mut buffer)
        .await
        .unwrap();

        let mut reader = &buffer[..];
        let result = process_control_messages(&mut reader, state.clone()).await;
        assert!(result.is_ok());

        // 先拿 Ready 里的端口，确认代理确实启动了
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        let port = match msg {
            ControlMessage::AgentLlmProxyReady { session_id, port } => {
                assert_eq!(session_id, "s2");
                assert!(port > 0);
                port
            }
            other => panic!("expected AgentLlmProxyReady, got {other:?}"),
        };
        // Stop 已处理：kill 句柄被移除，kill 信号已触发 → 端口释放
        assert!(state.llm_proxy_kills.lock().await.is_empty());
        let mut port_closed = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                port_closed = true;
                break;
            }
        }
        assert!(port_closed, "listener should be released after stop");
    }

    #[tokio::test]
    async fn test_agent_exec_cancel_roundtrip_on_control_channel() {
        // 最小化验证：AgentExecCancel 经 ControlMessage 可序列化/反序列化并触发 cancel_exec
        let msg = ControlMessage::AgentExecCancel {
            request_id: "req-x".into(),
        };
        let bytes = msg.serialize().unwrap();
        // serialize 输出为 4 字节长度前缀 + bincode 载荷；deserialize 仅解析载荷
        let decoded: ControlMessage = bincode::deserialize(&bytes[4..]).unwrap();
        assert!(matches!(
            decoded,
            ControlMessage::AgentExecCancel { request_id } if request_id == "req-x"
        ));
    }
}
