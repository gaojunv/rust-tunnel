use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::config::ServerConfig;
use crate::control_plane::client_registry::TunnelOpenOutcome;
use rust_tunnel_common::{
    create_server_config, load_or_generate_cert, ControlMessage, TunnelError, TunnelResult,
};

use super::ServerState;

/// 处理单个客户端控制连接（v2 协议，spec §3.2）。
///
/// 替代旧的基于端口的 `handle_control_connection`。
///
/// # Errors
/// 当控制通道读写或注册失败时返回 `TunnelError`。
#[allow(
    clippy::match_same_arms,
    reason = "不同 ControlMessage 变体分发到不同处理逻辑，合并会降低可读性"
)]
#[allow(
    clippy::too_many_lines,
    reason = "控制连接编排含注册、鉴权、读写循环与全量消息分发，共享大量局部状态，拆分反而降低可读性"
)]
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
            // 包装为带 source 的错误（保留 RegisterError 链），不再吞成 String
            return Err(TunnelError::with_source("register failed", err));
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
            let Some(msg) = ControlMessage::read_from_stream(&mut reader).await? else {
                break;
            };
            match msg {
                ControlMessage::Ping {
                    seq,
                    timestamp_micros,
                } => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| {
                            let micros = d.as_micros();
                            u64::try_from(micros).unwrap_or(u64::MAX)
                        });
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
                ControlMessage::AgentExecResponse {
                    request_id, result, ..
                } => {
                    registry
                        .deliver_agent_response(&entry.name, &request_id, result)
                        .await;
                }
                // spawn 协商响应：与 spawn_negotiate 的挂起等待者配对
                m @ (ControlMessage::AgentSpawnResponse { .. }
                | ControlMessage::AgentLlmProxyReady { .. }) => {
                    let session_id = match &m {
                        ControlMessage::AgentSpawnResponse { session_id, .. } => session_id.clone(),
                        ControlMessage::AgentLlmProxyReady { session_id, .. } => session_id.clone(),
                        _ => unreachable!(),
                    };
                    if !registry.resolve_spawn_pending(&session_id, m).await {
                        debug!(
                            "spawn response for unknown session '{session_id}' from '{}'",
                            entry.name
                        );
                    }
                }
                // spawn stdio / LLM 请求代理：转交 AgentState 的 AcpBridge 路由
                // （AgentSpawnData → ACP stdio pump，AgentSpawnExit → 进程退出
                // 标记，AgentLlmProxyRequest → 服务端 LLM 网关转发）。
                m @ (ControlMessage::AgentSpawnData { .. }
                | ControlMessage::AgentSpawnExit { .. }
                | ControlMessage::AgentLlmProxyRequest { .. }) => {
                    if let Some(agent) = &state.agent_state {
                        if let Some(bridge) = &agent.acp_bridge {
                            bridge.handle_client_msg(&entry.name, m).await;
                            continue;
                        }
                    }
                    debug!(
                        "spawn/llm-proxy msg from client '{}' but no acp bridge: {:?}",
                        entry.name, m
                    );
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
                            log_store.send(crate::logs::LogEntry {
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
    // 客户端控制连接断开：先清理该客户端的 ACP 会话（flush 回合缓冲 + 移除条目）。
    // 客户端 agent 进程随连接断开而终止，残留条目只会被 idle reaper 晚回收
    // （30 分钟）；disconnect 只发断线通知、不 await 任何 agent 工作，此处即时
    // 清理避免审批等待/缓冲长时间悬挂。
    if let Some(agent) = &state.agent_state {
        if let Some(bridge) = &agent.acp_bridge {
            bridge.drop_client_sessions(&name_for_cleanup).await;
        }
    }
    cleanup_registry
        .disconnect(&name_for_cleanup, "connection closed")
        .await;
    result
}

/// 启动控制面主服务（监听、TLS 握手与连接分发）。
///
/// # Errors
/// 当监听绑定或 TLS 配置失败时返回 `TunnelError`。
#[allow(
    clippy::match_same_arms,
    reason = "不同 ControlMessage 变体分发到不同处理逻辑，合并会降低可读性"
)]
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
            .map_err(|e| TunnelError::with_source("Failed to load TLS certificates", e))?;
        let tls_config = create_server_config(cert_pair)
            .map_err(|e| TunnelError::with_source("Failed to create TLS config", e))?;
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
