use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::agent::runner::client_supports_mapping_summary;
use crate::config::ServerConfig;
use crate::control_plane::client_registry::TunnelOpenOutcome;
use rust_tunnel_common::{
    create_server_config, load_or_generate_cert, BackendSummary, ControlMessage, MappingSummary,
    RouteSummary, RuleSummary, TunnelError, TunnelResult,
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

    // 首包映射摘要推送（仅对支持的客户端版本下发，避免老客户端收到未知变体断连）。
    if client_supports_mapping_summary(entry.client_version.as_deref()) {
        if let Some(summary) = build_mapping_summary(&state, &entry).await {
            let _ = entry
                .control_sender
                .send(ControlMessage::ClientMappingSummary { summary })
                .await;
        }
    }

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

// ── 映射摘要推送 ─────────────────────────────────────────────────

const MAX_CONTROL_MSG_BYTES: usize = 1024 * 1024;

fn rule_references_client(
    rule: &rust_tunnel_protocols::reverse_proxy::ProxyRule,
    client_name: &str,
) -> bool {
    for route in &rule.routes {
        for backend in &route.backends {
            if backend.kind == rust_tunnel_protocols::reverse_proxy::BackendKind::Client
                && backend.client_name.as_deref() == Some(client_name)
            {
                return true;
            }
        }
    }
    false
}

fn truncate_summary_for_limit(mut summary: MappingSummary) -> Option<MappingSummary> {
    let Ok(bytes) =
        bincode::serialize(&ControlMessage::ClientMappingSummary { summary: summary.clone() })
    else {
        return Some(summary);
    };
    if bytes.len() + 4 <= MAX_CONTROL_MSG_BYTES {
        return Some(summary);
    }
    let original_len = summary.rules.len();
    let mut lo = 0usize;
    let mut hi = original_len;
    let mut best = 0usize;
    while lo <= hi {
        let mid = usize::midpoint(lo, hi);
        let mut probe = summary.clone();
        probe.rules.truncate(mid);
        probe.truncated = mid < original_len;
        let Ok(b) = bincode::serialize(&ControlMessage::ClientMappingSummary { summary: probe })
        else {
            break;
        };
        if b.len() + 4 <= MAX_CONTROL_MSG_BYTES {
            best = mid;
            if mid == hi {
                break;
            }
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    summary.rules.truncate(best);
    summary.truncated = true;
    if let Ok(b) = bincode::serialize(&ControlMessage::ClientMappingSummary {
        summary: summary.clone(),
    }) {
        if b.len() + 4 > MAX_CONTROL_MSG_BYTES {
            return None;
        }
    }
    Some(summary)
}

/// 构造映射摘要：过滤 `kind == Client && client_name == X` 的规则，拼 `RuleSummary`，
/// 并填充 `connected_at`/`active_tunnels`/`rtt_ms`/`truncated`。
#[allow(clippy::too_many_lines, reason = "映射摘要组装含规则过滤+摘要拼装+1MB截断，共享局部状态拆分反而降低可读性")]
async fn build_mapping_summary(
    state: &ServerState,
    entry: &crate::control_plane::client_registry::ClientEntry,
) -> Option<MappingSummary> {
    let client_name = entry.name.as_str();
    let rules_snapshot: Vec<rust_tunnel_protocols::reverse_proxy::ProxyRule> = {
        let rules = state.proxy_state.rules.lock().await;
        rules.values().cloned().collect()
    };

    let mut rule_summaries: Vec<RuleSummary> = Vec::new();
    for rule in &rules_snapshot {
        if !rule_references_client(rule, client_name) {
            continue;
        }
        let route_summaries: Vec<RouteSummary> = rule
            .routes
            .iter()
            .map(|r| RouteSummary {
                path: r.path.clone(),
                backends: r
                    .backends
                    .iter()
                    .map(|b| BackendSummary {
                        kind: match b.kind {
                            rust_tunnel_protocols::reverse_proxy::BackendKind::Client => {
                                "client".to_string()
                            }
                            rust_tunnel_protocols::reverse_proxy::BackendKind::Direct => {
                                "direct".to_string()
                            }
                        },
                        addr: b.addr.clone(),
                        client_name: b.client_name.clone(),
                        weight: b.weight,
                    })
                    .collect(),
            })
            .collect();
        rule_summaries.push(RuleSummary {
            id: rule.id.clone(),
            name: rule.name.clone(),
            listen: rule.listen.clone(),
            domains: rule.domains.clone(),
            tls_enabled: rule.tls.as_ref().is_some_and(|t| t.enabled),
            routes: route_summaries,
        });
    }

    let connected_at = u64::try_from(entry.connected_at.timestamp_micros()).unwrap_or(0);
    let active_tunnels = u32::try_from(entry.active_tunnel_count().await).unwrap_or(u32::MAX);

    let summary = MappingSummary {
        connected_at: Some(connected_at),
        active_tunnels,
        rtt_ms: None, // TODO: 从质量监控/心跳 stats 读取真实 RTT
        rules: rule_summaries,
        truncated: false,
    };

    truncate_summary_for_limit(summary)
}

/// 向单个在线客户端增量推送最新映射摘要（供反代规则变更后调用）。
/// 内部做版本门控与 1MB 截断；客户端离线或不支持则静默返回 false。
pub async fn push_mapping_summary_to_client(
    state: &ServerState,
    client_name: &str,
) -> bool {
    let Some(registry) = state.client_registry.as_ref() else {
        return false;
    };
    let Some(entry) = registry.get(client_name).await else {
        return false;
    };
    if !client_supports_mapping_summary(entry.client_version.as_deref()) {
        return false;
    }
    let Some(summary) = build_mapping_summary(state, &entry).await else {
        return false;
    };
    entry
        .control_sender
        .send(ControlMessage::ClientMappingSummary { summary })
        .await
        .is_ok()
}

/// 向所有受 `rule` 影响的客户端推送映射摘要（增量推送入口，规则保存/删除后调用）。
/// 从 `rule` 的 routes 中提取所有 `kind == Client` 的 `client_name` 去重后逐个推送。
pub async fn push_mapping_summary_for_rule(
    state: &ServerState,
    rule: &rust_tunnel_protocols::reverse_proxy::ProxyRule,
) {
    use std::collections::HashSet;
    let mut affected: HashSet<String> = HashSet::new();
    for route in &rule.routes {
        for backend in &route.backends {
            if backend.kind == rust_tunnel_protocols::reverse_proxy::BackendKind::Client {
                if let Some(name) = backend.client_name.as_deref() {
                    if !name.is_empty() {
                        affected.insert(name.to_string());
                    }
                }
            }
        }
    }
    for name in affected {
        let _ = push_mapping_summary_to_client(state, &name).await;
    }
}

/// 向旧规则曾关联、但新规则不再关联的客户端也推送一次（用于更新/删除时清理其摘要）。
pub async fn push_mapping_summary_for_rule_pair(
    state: &ServerState,
    old_rule: Option<&rust_tunnel_protocols::reverse_proxy::ProxyRule>,
    new_rule: Option<&rust_tunnel_protocols::reverse_proxy::ProxyRule>,
) {
    use std::collections::HashSet;
    let mut affected: HashSet<String> = HashSet::new();
    for rule in old_rule.into_iter().chain(new_rule) {
        for route in &rule.routes {
            for backend in &route.backends {
                if backend.kind == rust_tunnel_protocols::reverse_proxy::BackendKind::Client {
                    if let Some(name) = backend.client_name.as_deref() {
                        if !name.is_empty() {
                            affected.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }
    for name in affected {
        let _ = push_mapping_summary_to_client(state, &name).await;
    }
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
