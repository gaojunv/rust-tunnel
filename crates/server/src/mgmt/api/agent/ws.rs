//! Agent workbench WebSocket handlers：主回合流 + 终端桥接。
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::agent::session::{DEFAULT_MODEL_KEY, SessionRuntime};
use crate::auth::{validate_token, AuthConfig};
use crate::llm::ChatMessage;
use crate::mgmt::api::ApiState;

use super::dto::{AgentWsQuery, NotificationsWsQuery, TerminalWsQuery};

/// 应用层心跳间隔：浏览器 JS 观测不到协议层 Ping 帧，探活必须用文本帧；
/// 心跳流量同时保活 NAT/代理的空闲映射（典型空闲超时 60s+，25s 足够密集）。
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(25);

/// 应用层心跳帧：前端看门狗以「任意帧到达」判定连接存活（含本帧）。
fn heartbeat_frame() -> serde_json::Value {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    serde_json::json!({"type": "heartbeat", "ts": ts})
}

/// GET /api/agent/ws?session_id=xxx&token=<jwt>
/// Public route; JWT validated from query param (browser WebSocket can't set headers).
pub async fn agent_ws(
    State(state): State<ApiState>,
    Query(params): Query<AgentWsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");
        if token.is_empty() || validate_token(token, &state.auth_config.jwt_secret).is_err() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_agent_socket(state, socket, params.session_id))
        .into_response()
}

/// GET /api/agent/notifications/ws?token=<jwt>
/// Public route; JWT validated from query param（同 `agent_ws`）。全局工作台通知：
/// 订阅 `AgentState` 的通知广播，任务完成/出错/需用户干预时向浏览器推送
/// [`crate::agent::notify::AgentNotification`] 帧（标签闪动 + 系统通知用）。
pub async fn notifications_ws(
    State(state): State<ApiState>,
    Query(params): Query<NotificationsWsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");
        if token.is_empty() || validate_token(token, &state.auth_config.jwt_secret).is_err() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_notifications_socket(state, socket))
        .into_response()
}

/// 通知 WS 连接生命周期：订阅广播 → 逐条下发文本帧。对端断开（send 失败）即退出。
/// 广播 sender 常驻（挂 `AgentState`），无事件时 `recv()` 挂起即可——连接关闭由
/// send 返回 Err 暴露，无需额外探测。
async fn handle_notifications_socket(state: ApiState, socket: WebSocket) {
    let (mut ws_sink, _ws_stream) = socket.split();
    let Some(agent) = state.server_state.agent_state else {
        return;
    };
    let mut rx = agent.subscribe_notifications();
    // 心跳分支：通知连接同样可能长空闲被中间设备掐断（无事件时 recv 挂起、
    // 无任何出站帧），按 HEARTBEAT_INTERVAL 下发文本心跳帧探活，send 失败即退出。
    let mut hb = tokio::time::interval(HEARTBEAT_INTERVAL);
    hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            n = rx.recv() => {
                // 广播 sender 关闭/Lagged 时 recv 返回 Err：保持原 `while let Ok`
                // 的退出语义（任何 Err 即 return），不顺手改。
                let Ok(n) = n else { return };
                let text = match serde_json::to_string(&n) {
                    Ok(t) => t,
                    Err(_) => continue, // 序列化失败理论上不可达，跳过不中断
                };
                if ws_sink.send(Message::Text(text)).await.is_err() {
                    return; // 对端断开
                }
            }
            _ = hb.tick() => {
                let text = serde_json::to_string(&heartbeat_frame()).unwrap_or_default();
                if ws_sink.send(Message::Text(text)).await.is_err() {
                    return; // 对端断开
                }
            }
        }
    }
}

/// GET `/api/agent/terminal/ws?workspace_id=xxx&cols=..&rows=..&token=<jwt>`
/// Public route; JWT validated from query param（同 `agent_ws`，浏览器 WebSocket
/// 无法带 Authorization header）。
pub async fn terminal_ws(
    State(state): State<ApiState>,
    Query(params): Query<TerminalWsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // 与 agent_ws 相同的 JWT 校验；抽成纯函数以便单测——WebSocketUpgrade 无法在
    // 单元测试中构造（内部字段私有、依赖真实握手），见 tests::test_terminal_ws_auth_status。
    if let Some(status) = terminal_ws_auth_status(&state.auth_config, params.token.as_deref()) {
        return status.into_response();
    }
    ws.on_upgrade(move |socket| handle_terminal_socket(state, socket, params))
        .into_response()
}

/// WS 公共路由的 JWT 校验：auth 未启用 → None（放行）；启用且 token 缺失/非法
/// → Some(401)。与 `agent_ws` 的内联 4 行逻辑等价，抽出来只为让拒绝路径可单测。
fn terminal_ws_auth_status(auth_config: &AuthConfig, token: Option<&str>) -> Option<StatusCode> {
    if auth_config.is_enabled() {
        let token = token.unwrap_or("");
        if token.is_empty() || validate_token(token, &auth_config.jwt_secret).is_err() {
            return Some(StatusCode::UNAUTHORIZED);
        }
    }
    None
}

/// PTY 协商帧：首行 JSON，`\n` 结尾（`client::pty` 服务端协议约定）。shell 为
/// None（host runtime）时不带该字段，客户端回退系统默认 shell。
/// `id` 为终端会话唯一标识（UUID v4），用于后续 resize 重协商定位 PTY 实例。
/// 旧客户端无 `id` 字段时 serde 默认忽略（Negotiation 无 deny_unknown_fields）。
#[derive(serde::Serialize)]
struct PtyNegotiation<'a> {
    rows: u16,
    cols: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
}

/// 前端 resize 帧：`{"type":"resize","cols":N,"rows":M}`。
#[derive(serde::Deserialize)]
struct ResizeFrame {
    cols: u16,
    rows: u16,
}

/// 向 WebSocket 下发一个文本错误帧（终端协议错误上报方式：握手完成后无 HTTP
/// 状态码，错误以文本帧传达后由调用方关闭连接）。
async fn send_ws_error<S>(sink: &mut S, message: &str)
where
    S: futures_util::Sink<Message> + Unpin,
{
    let _ = sink.send(Message::Text(message.to_string())).await;
}

/// 浏览器 ↔ WebSocket ↔ `OpenTunnel` 字节流 ↔ 客户端回环 PTY 服务的桥接入口。
/// 握手已完成，错误无 HTTP 状态码可用，统一以文本错误帧下发后关闭连接。
async fn handle_terminal_socket(state: ApiState, socket: WebSocket, params: TerminalWsQuery) {
    // 终端尺寸钳制到 1..=500，防止畸形 query 让客户端分配超大 PTY。
    let cols = params.cols.unwrap_or(80).clamp(1, 500);
    let rows = params.rows.unwrap_or(24).clamp(1, 500);

    let (mut ws_sink, ws_stream) = socket.split();

    // 1. agent 工作台未初始化（配置未启用）→ 错误帧
    let Some(agent) = state.server_state.agent_state else {
        send_ws_error(&mut ws_sink, "agent workbench not initialized").await;
        return;
    };

    // 2. workspace 记录加载；不存在 → 错误帧
    let ws = match agent.db.agent_get_workspace(&params.workspace_id).await {
        Ok(Some(ws)) => ws,
        Ok(None) => {
            send_ws_error(&mut ws_sink, "workspace not found").await;
            return;
        }
        Err(e) => {
            tracing::warn!("terminal ws: load workspace failed: {e}");
            send_ws_error(&mut ws_sink, "failed to load workspace").await;
            return;
        }
    };

    // docker 运行时必须有 container_id，否则没有 shell 可 exec
    if ws.runtime_type == "docker" && ws.docker_container_id.is_none() {
        send_ws_error(&mut ws_sink, "docker container not started").await;
        return;
    }

    // 3. 版本门控：离线（get 返回 None，无版本信息）与过旧（< 0.3.0）都视为
    //    不支持——老客户端没有 PTY 服务，建隧道只会连到客户端上不存在的端口。
    let entry = agent.registry.client_handle(&ws.client_id).await;
    let version = entry.as_ref().and_then(|e| e.client_version.clone());
    if !crate::agent::runner::client_supports_terminal(version.as_deref()) {
        let message = if entry.is_none() {
            "client offline or too old (requires >= 0.3.0)"
        } else {
            "terminal requires client >= 0.3.0; please upgrade the client"
        };
        send_ws_error(&mut ws_sink, message).await;
        return;
    }

    // 4. 协商帧的 shell 字段：host 用系统默认 shell（缺省）；docker 用整串
    //    `docker exec -it <ctr> sh`。container_id 来自用户配置，理论上可含空格/
    //    引号——但这是 JSON 字符串而非 shell 拼接，客户端 `sh -c` 自行处理，
    //    服务端原样透传即可。
    let shell: Option<String> = match ws.runtime_type.as_str() {
        "docker" => Some(format!(
            "docker exec -it {} sh",
            ws.docker_container_id
                .as_deref()
                .expect("docker container id checked above")
        )),
        _ => None,
    };

    // 5. 建立到客户端回环 PTY 服务的隧道。MVP：客户端 `--agent-pty-port` 可覆盖
    //    监听端口，但服务端无从得知覆盖值，只能硬编码默认端口常量。
    let target = format!("127.0.0.1:{}", rust_tunnel_common::pty::DEFAULT_PTY_PORT);
    let mut tunnel = match agent.registry.open_tunnel(&ws.client_id, &target).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("terminal ws: open tunnel to {target} failed: {e}");
            send_ws_error(
                &mut ws_sink,
                &format!("failed to open tunnel to client PTY service: {e}"),
            )
            .await;
            return;
        }
    };

    // 6. 写首行协商帧（JSON + '\n'）。写失败说明隧道已断，直接返回；
    //    tunnel drop 会自动向客户端发 Close 释放对端 PTY 连接。
    //    生成 UUID 作为终端会话 id：后续 resize 帧经此 id 定位客户端 PTY 实例。
    let terminal_id = uuid::Uuid::new_v4().to_string();
    let negotiation = PtyNegotiation {
        rows,
        cols,
        shell: shell.as_deref(),
        id: Some(&terminal_id),
    };
    let mut frame = serde_json::to_vec(&negotiation)
        .expect("serde_json::to_vec on a flat struct is infallible");
    frame.push(b'\n');
    if let Err(e) = tunnel.write_all(&frame).await {
        tracing::warn!("terminal ws: write PTY negotiation frame failed: {e}");
        return;
    }

    // 7. 双向桥接：WS binary ↔ 隧道字节流（含 resize 帧处理）。
    bridge_terminal(ws_sink, ws_stream, tunnel, &agent, &ws, &terminal_id).await;
}

/// 双向桥接：WS binary ↔ 隧道字节流。任一方向结束即整体退出（tunnel drop 发
/// Close 给客户端，WS 连接随之关闭）。`tokio::io::split` 把隧道拆成读/写两半，
/// 供 select! 两个分支同时借用。
///
/// WS Text 帧中的 resize 消息 `{"type":"resize","cols":N,"rows":M}` 触发重协商：
/// 新开一条到客户端 PTY 端口的隧道，发送 resize_for 协商帧后关闭，不影响现有
/// 数据通道。
async fn bridge_terminal(
    mut ws_sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut ws_stream: futures_util::stream::SplitStream<WebSocket>,
    tunnel: crate::agent::TunnelByteStream,
    agent: &crate::agent::AgentState,
    workspace: &crate::db::agent::AgentWorkspaceRecord,
    terminal_id: &str,
) {
    let (mut tunnel_rd, mut tunnel_wr) = tokio::io::split(tunnel);
    let mut buf = vec![0u8; 4096];
    tokio::select! {
        // 方向一：隧道 → WebSocket。隧道 EOF（读 0）或任一侧出错即结束。
        res = async {
            loop {
                match tunnel_rd.read(&mut buf).await {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        if ws_sink.send(Message::Binary(buf[..n].to_vec())).await.is_err() {
                            break Err("ws send failed".to_string());
                        }
                    }
                    Err(e) => break Err(format!("tunnel read failed: {e}")),
                }
            }
        } => {
            match res {
                Ok(()) => tracing::debug!("terminal ws: tunnel EOF, closing"),
                Err(e) => tracing::warn!("terminal ws: tunnel→ws ended: {e}"),
            }
        }
        // 方向二：WebSocket → 隧道。Binary 转发到隧道；Text 帧解析 resize 指令；
        // Close/EOF/错误即结束。
        res = async {
            loop {
                match ws_stream.next().await {
                    Some(Ok(Message::Binary(data))) => {
                        if tunnel_wr.write_all(&data).await.is_err() {
                            break Err("tunnel write failed".to_string());
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        // resize 帧：新开隧道发 resize_for 协商帧，不影响数据通道。
                        if let Ok(frame) = serde_json::from_str::<ResizeFrame>(&text) {
                            let cols = frame.cols.clamp(1, 500);
                            let rows = frame.rows.clamp(1, 500);
                            let target = format!(
                                "127.0.0.1:{}",
                                rust_tunnel_common::pty::DEFAULT_PTY_PORT
                            );
                            match agent.registry.open_tunnel(&workspace.client_id, &target).await {
                                Ok(mut resize_tunnel) => {
                                    let resize_frame = format!(
                                        "{{\"resize_for\":\"{}\",\"rows\":{},\"cols\":{}}}\n",
                                        terminal_id, rows, cols
                                    );
                                    if let Err(e) = resize_tunnel.write_all(resize_frame.as_bytes()).await {
                                        tracing::warn!("terminal ws: resize tunnel write failed: {e}");
                                    }
                                    // resize_tunnel drop 自动 Close 释放连接
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "terminal ws: resize tunnel open failed: {e} (id={terminal_id})"
                                    );
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break Ok(()),
                    // Ping/Pong 忽略：浏览器对服务端 ping 自动回 pong。
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break Err("ws stream error".to_string()),
                }
            }
        } => {
            match res {
                Ok(()) => tracing::debug!("terminal ws: ws closed by peer, ending"),
                Err(e) => tracing::warn!("terminal ws: ws→tunnel ended: {e}"),
            }
        }
    }
}

/// 会话角色/模型「下一条消息生效」：每条消息从 DB 重读 `session.role_id` 与
/// `session.model`，刷新 `rt.active_role` 并按 `SessionRuntime::load` 的优先级链
/// 重算 `rt.model`（session.model → 角色 model_override → workspace.llm_model_id →
/// 全局默认 → 保持现状）。同时兼作 `@role` 临时角色的复位点：`@role` 只改内存态
/// 不落库，下一条消息在此被重置回 DB 基线，不会泄漏到后续回合。
/// 返回值：`session.model` 是否显式设置（`@role` 的 model_override 仅在其为空时生效）。
async fn refresh_session_state(
    db: &crate::db::Database,
    session_id: &str,
    rt_model: &mut String,
    active_role: &mut Option<crate::db::roles::AgentRoleRecord>,
) -> bool {
    let Ok(Some(session)) = db.agent_get_session(session_id).await else {
        return false;
    };
    *active_role = match session.role_id.as_deref() {
        Some(role_id) => db
            .role_get_by_id(role_id)
            .await
            .ok()
            .flatten()
            .filter(|r| r.enabled == 1 && (r.mode == "primary" || r.mode == "all")),
        None => None,
    };
    if let Some(m) = session.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if m != rt_model {
            *rt_model = m.to_string();
        }
        return true;
    }
    if let Some(rm) = active_role
        .as_ref()
        .and_then(|r| r.model_override.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if rm != rt_model {
            *rt_model = rm.to_string();
        }
        return false;
    }
    if let Ok(Some(ws)) = db.agent_get_workspace(&session.workspace_id).await {
        if let Ok(Some(r)) =
            crate::agent::session::resolve_workspace_model_ref(db, ws.llm_model_id.as_deref()).await
        {
            if r != *rt_model {
                *rt_model = r;
            }
            return false;
        }
    }
    // 全链路皆空 → 回退全局默认（load 时的「第一个可用模型」兜底在此无法复现，
    // 默认设置也为空则保持现状）。
    if let Ok(Some(d)) = db.load_server_setting(DEFAULT_MODEL_KEY).await {
        let d = d.trim();
        if !d.is_empty() && d != rt_model {
            *rt_model = d.to_string();
        }
    }
    false
}

/// 会话审批模式「下一条消息生效」：每轮从 DB 重读 workspace.approval_mode，
/// 若与运行时当前值不同则覆盖（set_mode 中途切换后下一轮自动生效）。
async fn refresh_approval_mode(db: &crate::db::Database, session_id: &str, rt_mode: &mut String) {
    let Ok(Some(session)) = db.agent_get_session(session_id).await else {
        return;
    };
    let Ok(Some(workspace)) = db.agent_get_workspace(&session.workspace_id).await else {
        return;
    };
    if workspace.approval_mode != *rt_mode {
        *rt_mode = workspace.approval_mode;
    }
}

/// `WS` 客户端帧分类：`user_message` / `cancel` / `approval_response` / 其他（忽略）。
enum WsFrame {
    /// 用户消息：content + 可选 @引用文件路径列表
    UserMessage {
        content: String,
        refs: Vec<String>,
    },
    Cancel,
    /// 审批响应：`request_id`、是否批准、是否本会话记住该类工具；
    /// `option_id` 为 ACP options 透传路径（用户选中具体选项）时可缺省。
    ApprovalResponse {
        request_id: String,
        approved: bool,
        option_id: Option<String>,
        remember: bool,
    },
    /// elicitation 响应（AskUserQuestion 表单）：`action` ∈ accept/decline/cancel；
    /// accept 时 `content` 为字段值对象（与 requested_schema 匹配）。
    ElicitationResponse {
        request_id: String,
        action: String,
        content: Option<serde_json::Value>,
    },
    /// ACP 会话配置切换（session/set_config_option 透传）
    SetConfigOption {
        config_id: String,
        value: String,
    },
    /// Runner 路径审批模式切换（plan/execute）：更新 workspace.approval_mode
    /// 并同步当前连接的 SessionRuntime。
    SetMode {
        mode: String,
    },
    /// 主会话角色切换：更新 session.role_id（DB）+ rt.active_role。
    /// role_id 为空串 = 清除角色。
    SetRole {
        role_id: String,
    },
    Other,
}

fn parse_ws_frame(msg: Message) -> WsFrame {
    let Message::Text(text) = msg else {
        return WsFrame::Other;
    };
    let Ok(body) = serde_json::from_str::<serde_json::Value>(&text) else {
        return WsFrame::Other;
    };
    match body.get("type").and_then(|t| t.as_str()) {
        Some("user_message") => {
            let content = body
                .get("content")
                .and_then(|c| c.as_str())
                .map(str::to_string);
            let refs = body
                .get("refs")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .take(crate::agent::runner::MAX_REFS)
                        .collect()
                })
                .unwrap_or_default();
            match content {
                Some(c) => WsFrame::UserMessage { content: c, refs },
                None => WsFrame::Other,
            }
        }
        Some("cancel") => WsFrame::Cancel,
        Some("approval_response") => {
            let request_id = body
                .get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if request_id.is_empty() {
                return WsFrame::Other;
            }
            WsFrame::ApprovalResponse {
                request_id,
                approved: body
                    .get("approved")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                option_id: body
                    .get("option_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                remember: matches!(
                    body.get("remember").and_then(|v| v.as_str()),
                    Some("session")
                ),
            }
        }
        Some("elicitation_response") => {
            let request_id = body
                .get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let action = body
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // request_id 缺省或 action 非法 → Other（与 approval_response 同款
            // 容错：非法帧静默忽略，不 panic）。
            if request_id.is_empty() || !["accept", "decline", "cancel"].contains(&action.as_str()) {
                return WsFrame::Other;
            }
            WsFrame::ElicitationResponse {
                request_id,
                action,
                content: body.get("content").cloned(),
            }
        }
        Some("set_config_option") => {
            let config_id = body
                .get("config_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let value = body
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if config_id.is_empty() || value.is_empty() {
                return WsFrame::Other;
            }
            WsFrame::SetConfigOption { config_id, value }
        }
        Some("set_mode") => {
            let mode = body
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // 只接受已知的 approval_mode 值
            if !matches!(mode.as_str(), "safe" | "auto_write" | "full_auto" | "plan") {
                return WsFrame::Other;
            }
            WsFrame::SetMode { mode }
        }
        Some("set_role") => {
            let role_id = body
                .get("role_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            WsFrame::SetRole { role_id }
        }
        _ => WsFrame::Other,
    }
}

/// 单个 agent turn 的终态。取消（cancel 帧）与断连需与正常完成/失败区分：
/// - 取消的回合不算"成功"：不发 error 帧、不生成标题，且内存 rt_cache 作废
///   （turn 可能已把无配对 tool 结果的 assistant tool_calls 写进 rt.messages，
///   内存路径没有 sanitize_tool_pairs 兜底，直接复用会把非法序列发给 LLM 导致
///   400；置 None 后下一条消息从 DB 重建，DB 重放路径的占位补齐保证序列合法）。
/// - 断连（对端关闭/错误）：退出外层循环。
enum TurnOutcome {
    /// 回合正常结束（成功或失败）
    Completed(Result<(), String>),
    /// 收到 cancel 帧，turn future 被丢弃
    Cancelled,
    /// 对端断开/协议错误，需退出外层循环
    Disconnected,
}

/// 选择回合执行路径：workspace 配置了 `agent_type`（非空）走 ACP 远程 agent，
/// 否则走自研 runner（保留）。
fn use_acp_path(workspace: &crate::db::agent::AgentWorkspaceRecord) -> bool {
    !workspace.agent_type.is_empty()
}

/// 按 session 加载其 workspace 记录（分派 ACP 路径用）。
/// `Ok(None)` = session/workspace 不存在（可回退自研 runner 路径，其内部会再报错）；
/// `Err` = 读库失败（瞬态 DB 错误）。调用方对 `Err` 应发 error 帧并跳过本回合，
/// 不能静默回退——否则 ACP 配置的 workspace 会落到自研 runner（用错引擎）。
async fn load_workspace_for_session(
    db: &crate::db::Database,
    session_id: &str,
) -> Result<Option<crate::db::agent::AgentWorkspaceRecord>, String> {
    let session = match db.agent_get_session(session_id).await {
        Ok(session) => session,
        Err(e) => return Err(format!("load session failed: {e}")),
    };
    let Some(session) = session else {
        return Ok(None);
    };
    db.agent_get_workspace(&session.workspace_id)
        .await
        .map_err(|e| format!("load workspace failed: {e}"))
}

/// 读取 @引用文件并合成完整 user 消息（自研 runner 与 ACP 两条路径共用）。
/// 超总量限制的 refs 标注拒绝；读失败的标注 `[无法读取]`，均不阻断回合。
/// refs 为空时原样返回 content。
async fn inject_refs(
    agent: &crate::agent::AgentState,
    workspace_id: &str,
    client_id: &str,
    root_path: &str,
    docker_container: Option<&str>,
    content: &str,
    refs: &[String],
) -> String {
    if refs.is_empty() {
        return content.to_string();
    }
    let mut ref_files: Vec<(String, Result<String, String>)> = Vec::new();
    let mut total = 0usize;
    for path in refs {
        if total >= crate::agent::runner::MAX_REFS_TOTAL_BYTES {
            ref_files.push((path.clone(), Err("refs total size limit".to_string())));
            continue;
        }
        let result = crate::agent::executor::exec_on_client(
            agent,
            workspace_id,
            client_id,
            root_path,
            docker_container,
            rust_tunnel_common::AgentCommand::ReadFile { path: path.clone() },
        )
        .await;
        match result {
            rust_tunnel_common::AgentResult::FileContent { content: c } => {
                total += c.len();
                ref_files.push((path.clone(), Ok(c)));
            }
            _ => ref_files.push((path.clone(), Err("read failed".to_string()))),
        }
    }
    crate::agent::runner::compose_user_message(content, &ref_files)
}

async fn handle_agent_socket(state: ApiState, socket: WebSocket, session_id: String) {
    let (mut ws_sink, mut ws_stream) = socket.split();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
    // 本连接唯一标识：ensure_session 注册/刷新 ws_tx 时记录，teardown 时
    // detach_ws_tx 按它判断「是否仍是我注册的通道」。刷新竞态下旧连接 close
    // 晚于新连接注册，若无身份判断会误清新连接的通道（tool_result/done 全丢）。
    let conn_id = rand::random::<u64>();
    // 死连接传导：push_task 判定 sink 死亡（发送失败/超时）后 notify 一次，
    // 主循环（外层 select / 内层 turn select）任一正在等待的 `notified()` 立即
    // 完成 → teardown。notify_one 无等待者时 permit 留存，后到的 notified() 也会
    // 立刻返回，正覆盖「push_task 先死、主循环后到」的时序。
    let dead_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let dead_notify_clone = dead_notify.clone();

    // 推送任务：event_rx → WebSocket。对端断开导致 send 失败时不再 break，而是
    // 继续 drain event_rx——runner 内部仍是阻塞式 send().await，但只要接收端持续
    // 消费，64 槽 channel 就不会填满，runner 永不阻塞。所有发送方 drop 后
    // recv() 返回 None，任务自然结束；外层循环退出后仍由 push_task.abort() 兜底。
    //
    // ws_sink.send 必须加超时：慢客户端（TCP 零窗口、对端存活但不读）会让
    // send 无限阻塞 → push_task 不再 drain event_rx（64 槽）→ 所有生产方
    // send().await 阻塞 → runner 回合、ACP 通知/审批、stdio pump 一并冻结。
    // 完全断线能自愈（send 返回 Err），但"慢而不死"的客户端会永久停摆该会话。
    // 5 秒超时足够覆盖正常网络抖动；超时即视为 sink 死亡，后续事件只 drain 不发。
    const WS_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    // 通知广播：本连接所属 workspace 在此解析一次（session 缺失/读库失败为 None，
    // 跳过通知）。出站帧（runner 与 ACP 的 done/error、审批/elicitation 请求）唯
    // 一流经 push_task，在此翻译成全局通知广播——未查看该会话的标签页经
    // /api/agent/notifications/ws 收到提醒。
    let workspace_id = match &state.server_state.agent_state {
        Some(agent) => agent
            .db
            .agent_get_session(&session_id)
            .await
            .ok()
            .flatten()
            .map(|s| s.workspace_id),
        None => None,
    };
    let notify_agent = state.server_state.agent_state.clone();
    let notify_session_id = session_id.clone();
    let push_task = tokio::spawn(async move {
        let mut sink_alive = true;
        // 心跳与事件合流：长任务期间 WS 长时间无帧，中间设备会静默掐断空闲 TCP，
        // 服务端永远发现不了半开死连接。以文本心跳帧（HEARTBEAT_INTERVAL 节拍）
        // 制造持续出站流量——send 失败/超时即判定 sink 死亡并通知主循环 teardown。
        let mut hb = tokio::time::interval(HEARTBEAT_INTERVAL);
        hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                ev = event_rx.recv() => {
                    let Some(ev) = ev else { break }; // 所有发送方 drop，连接收尾
                    if let Some(ws_id) = &workspace_id {
                        if let Some(agent) = notify_agent.as_ref() {
                            if let Some(n) =
                                crate::agent::notify::notification_from_frame(&ev, &notify_session_id, ws_id)
                            {
                                agent.notify(n);
                            }
                        }
                    }
                    let text = serde_json::to_string(&ev).unwrap_or_default();
                    if !sink_alive {
                        continue;
                    }
                    let send_result =
                        tokio::time::timeout(WS_SEND_TIMEOUT, ws_sink.send(Message::Text(text))).await;
                    match send_result {
                        Ok(Ok(())) => {} // 发送成功
                        Ok(Err(_)) | Err(_) => {
                            // 发送失败或超时：标记 sink 死亡，后续事件继续 drain 但不再发
                            sink_alive = false;
                            // sink 已死，主循环应立即 teardown，不必等下一次心跳。
                            dead_notify_clone.notify_one();
                        }
                    }
                }
                _ = hb.tick() => {
                    // 心跳帧不经过 notification_from_frame 翻译（直接发送）。
                    // sink 已判定死亡时只 drain 不发，心跳分支同样跳过。
                    if !sink_alive {
                        continue;
                    }
                    let text = serde_json::to_string(&heartbeat_frame()).unwrap_or_default();
                    let send_result =
                        tokio::time::timeout(WS_SEND_TIMEOUT, ws_sink.send(Message::Text(text))).await;
                    if matches!(send_result, Ok(Err(_)) | Err(_)) {
                        sink_alive = false;
                        dead_notify_clone.notify_one(); // 见上：主循环立即 teardown
                    }
                }
            }
        }
    });

    // Live per-connection runtime cache. Reloading from the DB between turns is
    // protocol-invalid: assistant tool_calls messages are never persisted and
    // tool rows lack tool_call_id, so a DB replay yields a message sequence the
    // OpenAI API rejects (400). Instead, load once on the first user message
    // (which is persisted first), then append user text onto the in-memory
    // messages for later turns — the runner's in-memory path is valid. A fresh
    // WebSocket connection does reload, which may lose prior tool-call
    // structure; that is the accepted MVP tradeoff.
    let mut rt_cache: Option<SessionRuntime> = None;
    // 回合进行中对端又发来的用户消息：最多缓冲一条，当前 turn 结束后优先处理。
    let mut pending: Option<(String, Vec<String>)> = None;
    // 本连接是否已分派到 ACP 路径（workspace 配置了 agent_type）。ACP 回合
    // 异步执行，两轮之间的 cancel/approval_response 帧也要在此处理。
    let mut acp_active = false;

    // ACP 会话已就绪（重连/多标签页场景）：立即推送配置快照，前端设置菜单
    // 无需等下一次 config_option_update。session 尚未 spawn 时返回 None 跳过。
    // 能返回 Some 说明该 session 已有活跃 ACP 进程（workspace 必然配置了
    // agent_type）——同时置 acp_active，让后续 set_config_option/cancel/
    // approval_response 帧正常分派（否则刷新/新标签页连接上这些帧被静默丢弃）。
    if let Some(agent) = state.server_state.agent_state.as_ref() {
        if let Some(bridge) = agent.acp_bridge.as_ref() {
            if let Some(options) = bridge.session_config_options(&session_id).await {
                acp_active = true;
                let _ = event_tx
                    .send(serde_json::json!({"type": "session_state", "options": options}))
                    .await;
            }
        }
    }

    // 预 spawn（后台、不阻塞连接循环）：WS 一建立即拉起 ACP agent，mode/effort
    // 快捷按钮无需等首条 user_message。session 已 spawn（重连/多标签页）时
    // ensure_session 幂等只刷新 ws_tx；失败静默——离线客户端/无模型配置等报错
    // 交给首次 user_message 路径（再次 ensure_session 会重试）。成功后
    // ensure_session 内部已在 handshake 后推送 session_state（acp_bridge），
    // 前端对话前即收到 config options。
    if let Some(agent) = state.server_state.agent_state.clone() {
        if let Some(bridge) = agent.acp_bridge.clone() {
            let sid = session_id.clone();
            let ws_tx = event_tx.clone();
            tokio::spawn(async move {
                let Ok(Some(workspace)) = load_workspace_for_session(&agent.db, &sid).await else {
                    return;
                };
                if use_acp_path(&workspace) {
                    if let Err(e) = bridge.ensure_session(&sid, &workspace, ws_tx, conn_id).await {
                        // info 级：预 spawn 失败此前只在 debug 可见，用户首条
                        // 消息只能拿到 wait_ready 的泛化错误，排查困难。
                        tracing::info!(session_id = %sid, "pre-spawn acp agent failed: {e}");
                    }
                }
            });
        }
    }

    loop {
        // 优先消费缓冲的 pending 消息；否则从 socket 读取下一条。
        let (content, refs) = if let Some(p) = pending.take() {
            p
        } else {
            let msg = tokio::select! {
                msg = ws_stream.next() => match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(m)) => m,
                },
                // push_task 判定 sink 死亡（半开连接/慢客户端）：无需等对端 close，
                // 直接 teardown——释放会话锁、detach ACP ws_tx、abort push_task。
                _ = dead_notify.notified() => break,
            };
            match parse_ws_frame(msg) {
                WsFrame::UserMessage { content, refs } => (content, refs),
                // ACP 路径：回合在 bridge 里异步跑，cancel/审批响应在「两轮之间」
                // 到达，必须在这里处理（runner 路径的审批响应仍在 turn 内循环处理，
                // 二者共用 AgentState 的 pending map；未知 request_id 幂等忽略）。
                WsFrame::Cancel => {
                    if acp_active {
                        if let Some(agent) = state.server_state.agent_state.as_ref() {
                            if let Some(bridge) = agent.acp_bridge.as_ref() {
                                bridge.cancel(&session_id).await;
                                let _ = event_tx.send(serde_json::json!({"type": "stopped"})).await;
                            }
                        }
                    }
                    continue;
                }
                WsFrame::ApprovalResponse {
                    request_id,
                    approved,
                    option_id,
                    remember,
                } => {
                    if acp_active {
                        if let Some(agent) = state.server_state.agent_state.as_ref() {
                            agent
                                .resolve_approval(
                                    &session_id,
                                    &request_id,
                                    approved,
                                    option_id,
                                    remember,
                                )
                                .await;
                        }
                    }
                    continue;
                }
                WsFrame::ElicitationResponse {
                    request_id,
                    action,
                    content,
                } => {
                    // elicitation 仅 ACP 路径产生（runner 无表单概念）；未知
                    // request_id 幂等忽略（可能已超时清除 / 不属本进程）。
                    if acp_active {
                        if let Some(agent) = state.server_state.agent_state.as_ref() {
                            agent
                                .resolve_elicitation(&session_id, &request_id, &action, content)
                                .await;
                        }
                    }
                    continue;
                }
                WsFrame::SetConfigOption { config_id, value } => {
                    // 门控：acp_active（已走 ACP 路径）或会话已被预 spawn（对话前
                    // 按钮已出现、config_options 已就绪）。预 spawn 成功后 acp_active
                    // 仍是 false（只在 user_message 分派时置位），不加 session_spawned
                    // 会导致按钮出现但 set_config_option 帧被静默丢弃（点击无响应）。
                    let spawned = match state
                        .server_state
                        .agent_state
                        .as_ref()
                        .and_then(|a| a.acp_bridge.as_ref())
                    {
                        Some(bridge) => bridge.session_spawned(&session_id).await,
                        None => false,
                    };
                    if acp_active || spawned {
                        if let Some(agent) = state.server_state.agent_state.as_ref() {
                            if let Some(bridge) = agent.acp_bridge.as_ref() {
                                match bridge
                                    .set_config_option(&session_id, &config_id, &value)
                                    .await
                                {
                                    Ok(()) => {
                                        // 持久化（best-effort）：重连/重开时握手回放。
                                        if let Err(e) = agent
                                            .db
                                            .agent_update_session_config_state(
                                                &session_id,
                                                &config_id,
                                                Some(&value),
                                            )
                                            .await
                                        {
                                            tracing::warn!(session_id = %session_id, "persist config_state failed: {e}");
                                        }
                                    }
                                    Err(e) => {
                                        let _ = event_tx
                                            .send(serde_json::json!({
                                                "type": "error",
                                                "message": format!("设置失败: {e}")
                                            }))
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }
                WsFrame::SetMode { mode } => {
                    // Runner 路径审批模式切换：更新 workspace.approval_mode（DB）+ 当前连接 rt（内存态）。
                    // rt 在首次 user_message 时才创建；若尚无 rt，仅更新 DB（下一条消息时 load 读取）。
                    if let Some(agent) = state.server_state.agent_state.as_ref() {
                        let ws = match load_workspace_for_session(&agent.db, &session_id).await {
                            Ok(Some(ws)) => ws,
                            _ => {
                                let _ = event_tx.send(serde_json::json!({"type": "error", "message": "workspace not found"})).await;
                                continue;
                            }
                        };
                        if let Err(e) = agent.db.agent_update_workspace(
                            &ws.id, &ws.name, &ws.root_path,
                            None, Some(&mode), None, None, None, None, None, false, false,
                        ).await {
                            let _ = event_tx.send(serde_json::json!({"type": "error", "message": format!("set mode failed: {e}")})).await;
                            continue;
                        }
                    }
                    // 同步当前连接的 rt（若已创建）
                    if let Some(rt) = rt_cache.as_mut() {
                        rt.approval_mode = mode.clone();
                    }
                    let _ = event_tx.send(serde_json::json!({"type": "mode_updated", "mode": &mode})).await;
                    continue;
                }
                WsFrame::SetRole { role_id } => {
                    // 角色切换：更新 session.role_id（DB）+ rt.active_role（内存态）。
                    // role_id 空串 = 清除角色。
                    if let Some(agent) = state.server_state.agent_state.as_ref() {
                        // 更新 DB（空串 → None）
                        let db_role_id = if role_id.is_empty() { None } else { Some(role_id.as_str()) };
                        if let Err(e) = agent.db.agent_update_session_role(&session_id, db_role_id).await {
                            let _ = event_tx.send(serde_json::json!({"type": "error", "message": format!("set role failed: {e}")})).await;
                            continue;
                        }
                        // 同步 rt.active_role（若已创建）
                        if let Some(rt) = rt_cache.as_mut() {
                            rt.active_role = if role_id.is_empty() {
                                None
                            } else {
                                match agent.db.role_get_by_id(&role_id).await {
                                    Ok(Some(r)) if r.enabled == 1 && (r.mode == "primary" || r.mode == "all") => Some(r),
                                    _ => None,
                                }
                            };
                        }
                        let _ = event_tx.send(serde_json::json!({"type": "role_updated", "role_id": &role_id})).await;
                    }
                    continue;
                }
                WsFrame::Other => continue,
            }
        };

        let (agent, llm) = match (
            state.server_state.agent_state.clone(),
            state
                .server_state
                .llm_state
                .read()
                .await
                .as_ref()
                .cloned(),
        ) {
            (Some(a), Some(l)) => (a, l),
            _ => {
                let _ = event_tx
                    .send(serde_json::json!({"type": "error", "message": "agent or LLM gateway not initialized"}))
                    .await;
                continue;
            }
        };

        // 会话级互斥：同一 session 的并发连接（多标签页/重连叠旧连接）各自跑
        // turn 会并发写库、消息交错、tool_call_id 失配。user 落库 + rt 重建 +
        // 整回合全程持锁，把并发 turn 串行化。等待中的连接在锁释放后从 DB 重载
        // （rt_cache 仍是 None），能看到前者写入的全部消息。
        let session_lock = agent.session_lock(&session_id).await;
        let _session_guard = session_lock.lock().await;

        // ACP 分派：workspace 配置了 agent_type（非空）→ 走 ACP 远程 agent 路径
        // （agent 进程经隧道 spawn，事件经 acp_events::map_update 推前端）；
        // 否则保持下方自研 runner 路径。workspace 记录在此按需加载——runner
        // 路径的 SessionRuntime::load 内部还会再读一次，多一次低频 DB 读换取
        // 分派正确性可接受。
        match load_workspace_for_session(&agent.db, &session_id).await {
            Ok(Some(acp_workspace)) if use_acp_path(&acp_workspace) => {
                acp_active = true;
                // @引用注入（与 runner 路径共用语义）：refs 内容进 prompt，不静默
                // 丢弃（评审补充项）。读取经隧道在客户端进行，同 runner 路径。
                let content = inject_refs(
                    &agent,
                    &acp_workspace.id,
                    &acp_workspace.client_id,
                    &acp_workspace.root_path,
                    acp_workspace.docker_container_id.as_deref(),
                    &content,
                    &refs,
                )
                .await;
                let Some(bridge) = agent.acp_bridge.clone() else {
                    let _ = event_tx
                        .send(serde_json::json!({
                            "type": "error",
                            "message": "ACP bridge not initialized"
                        }))
                        .await;
                    continue;
                };
                if let Err(e) = bridge
                    .ensure_session(&session_id, &acp_workspace, event_tx.clone(), conn_id)
                    .await
                {
                    let _ = event_tx
                        .send(serde_json::json!({"type": "error", "message": e}))
                        .await;
                    continue;
                }
                // 预 spawn（连接打开时后台触发）可能仍在握手：等待就绪再 prompt，
                // 否则 connection 未建立会报 "ACP handshake not complete"。
                if let Err(e) = bridge.wait_ready(&session_id).await {
                    let _ = event_tx
                        .send(serde_json::json!({"type": "error", "message": e}))
                        .await;
                    continue;
                }
                // AI 记忆注入（ACP）：每会话检索一次并缓存到 SpawnedAgent（对齐
                // runner 路径的 rt.memory_block 缓存先例）。prompt_inner 发送前把
                // <memory> 块 prepend 到发给 agent 的 user content 头部——只进 agent
                // 侧上下文、不落 DB（持久化/蒸馏保持干净，无回环）。
                #[cfg(feature = "rag")]
                if let Some(memory) = agent.memory.as_ref() {
                    if bridge.cached_memory_block(&session_id).await.is_none() {
                        let block = crate::agent::memory::inject::retrieve_for_session(
                            memory,
                            &acp_workspace.client_id,
                            &acp_workspace.id,
                            &content,
                        )
                        .await
                        .unwrap_or_default();
                        bridge.set_memory_block(&session_id, Some(block)).await;
                    }
                    // Skill 清单注入（ACP）：与记忆同模式缓存到 SpawnedAgent，纯 SQL
                    // 零 embedding 依赖。prompt_inner 与 <memory> 块一并 prepend。
                    if bridge.cached_skill_list_block(&session_id).await.is_none() {
                        let block = crate::agent::skill::retrieve_skill_list_for_session(
                            memory,
                            &acp_workspace.client_id,
                            &acp_workspace.id,
                        )
                        .await
                        .unwrap_or_default();
                        bridge.set_skill_list_block(&session_id, Some(block)).await;
                    }
                    // Wiki 清单注入（ACP）：同 skill 模式缓存到 SpawnedAgent，纯 SQL
                    // （FTS5）零 embedding 依赖。prompt_inner 与前两块一并 prepend。
                    if let Some(wiki) = agent.wiki.as_ref() {
                        if bridge.cached_wiki_list_block(&session_id).await.is_none() {
                            let block = crate::agent::wiki::retrieve_wiki_list_for_session(
                                wiki,
                                &acp_workspace.client_id,
                                &acp_workspace.id,
                            )
                            .await
                            .unwrap_or_default();
                            bridge.set_wiki_list_block(&session_id, Some(block)).await;
                        }
                    }
                }
                // 持久化 user 消息（与 runner 路径同款）：落的是注入后的 content，
                // DB 中就是一条完整的 user 消息，前端刷新后对话不丢。排队消息同样
                // 立即落库——在 submit_prompt 之前，无论本轮是直接跑还是排队。
                let msg_id = format!("{:032x}", rand::random::<u128>());
                if let Err(e) = agent
                    .db
                    .agent_add_message(&msg_id, &session_id, "user", &content, None)
                    .await
                {
                    tracing::warn!(session_id = %session_id, "persist acp user message failed: {e}");
                }
                // submit_prompt：空闲直接跑；进行中回合排队（回合连续，排空才发
                // done）。返回 Err（会话不存在/已退出/排队满）时以 error 帧回发。
                if let Err(e) = bridge.submit_prompt(&session_id, &content, refs).await {
                    let _ = event_tx
                        .send(serde_json::json!({"type": "error", "message": e}))
                        .await;
                }
                continue;
            }
            // workspace 未配置 ACP / session 缺失：落入下方自研 runner 路径。
            Ok(_) => {}
            // 读库失败：不静默回退自研 runner（ACP workspace 用错引擎），
            // 发 error 帧并跳过本回合（评审 Finding 3）。
            Err(e) => {
                let _ = event_tx
                    .send(serde_json::json!({"type": "error", "message": e}))
                    .await;
                continue;
            }
        }

        // 首个用户消息：从 DB 重建运行时；后续消息直接追加到内存 messages。
        // user 消息的落库与内存追加统一放在 refs 注入之后（见下），加载阶段
        // 不再写库——先落原始 content 再注入会造成 DB 与内存内容不一致。
        let rt = match rt_cache.as_mut() {
            Some(rt) => {
                // 会话角色/模型「下一条消息生效」：PATCH/SetRole 仅落库，每条消息
                // 从 DB 重读并覆盖 rt（含 @role 临时角色复位），无需重连 WS 即生效。
                refresh_session_state(&agent.db, &session_id, &mut rt.model, &mut rt.active_role)
                    .await;
                refresh_approval_mode(&agent.db, &session_id, &mut rt.approval_mode).await;
                rt
            }
            None => {
                // 全局默认模型：session.model 为空时优先于「第一个可用模型」。
                let default_model = agent
                    .db
                    .load_server_setting(DEFAULT_MODEL_KEY)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let mut loaded =
                    match SessionRuntime::load(&agent.db, &session_id, &default_model).await {
                        Ok(rt) => rt,
                        Err(e) => {
                            let _ = event_tx
                                .send(serde_json::json!({"type": "error", "message": e}))
                                .await;
                            continue;
                        }
                    };
                // 模型为空 → 取 LLM 网关第一个可用模型
                if loaded.model.is_empty() {
                    if let Ok(models) = crate::llm::router::list_available_models(&llm).await {
                        if let Some(first) = models.first() {
                            if let Some(name) = first.get("id").and_then(|v| v.as_str()) {
                                loaded.model = name.to_string();
                            }
                        }
                    }
                    if loaded.model.is_empty() {
                        let _ = event_tx
                            .send(serde_json::json!({"type": "error", "message": "no LLM model configured"}))
                            .await;
                        continue;
                    }
                }
                rt_cache = Some(loaded);
                rt_cache.as_mut().expect("rt_cache just assigned")
            }
        };

        // @role 前缀解析：content 以 @<name> 开头且 name 匹配可见角色（mode 含 primary/all）
        // → 设置 rt.active_role（本回合生效）+ 剥离前缀；未命中走原 @文件引用逻辑。
        let visible_roles = agent.db.role_list_visible(
            &rt.client_id,
            &rt.workspace_id,
            Some("primary"),
        ).await.unwrap_or_default();
        let content = if let Some((role_name, stripped)) = crate::agent::roles::parse_at_role_prefix(&content, &visible_roles) {
            // 直接在可见角色列表中取记录（含 client/workspace 作用域）——不可按
            // global 作用域重查，否则非全局角色匹配上前缀却取不到记录。
            // parse_at_role_prefix 的名字来自同一列表，find 必然命中。
            let role = visible_roles
                .iter()
                .find(|r| r.name.eq_ignore_ascii_case(&role_name))
                .cloned();
            // @role 的 model_override 本回合生效，仅在 session.model 未显式设置时
            // （同 load 的优先级链）；下一条消息由 refresh_session_state 复位。
            if let Some(ref r) = role {
                let session_model_set = agent
                    .db
                    .agent_get_session(&session_id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.model)
                    .is_some_and(|m| !m.trim().is_empty());
                if !session_model_set {
                    if let Some(rm) = r
                        .model_override
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        rt.model = rm.to_string();
                    }
                }
            }
            rt.active_role = role;
            tracing::debug!(session_id = %session_id, role = %role_name, "@role prefix matched");
            stripped.to_string()
        } else {
            content
        };

        // @引用注入：逐个经隧道 ReadFile，合成完整 user 消息后落库 + 进上下文。
        // 超总量限制的 refs 标注拒绝；读失败的标注 [无法读取]，均不阻断回合。
        let content = inject_refs(
            &agent,
            &rt.workspace_id,
            &rt.client_id,
            &rt.root_path,
            rt.docker_container.as_deref(),
            &content,
            &refs,
        )
        .await;

        // 持久化 user 消息（保持会话历史完整，供 Web 端与重连后的首轮恢复）。
        // 落的是注入后的 content——DB 中就是一条完整的 user 消息。
        let msg_id = format!("{:032x}", rand::random::<u128>());
        let _ = agent
            .db
            .agent_add_message(&msg_id, &session_id, "user", &content, None)
            .await;
        // 内存上下文追加的同样是注入后的 content。
        rt.messages.push(ChatMessage::text("user", content));

        // 每个用户消息串行运行一个 agent turn。turn 期间持续观察 ws_stream：
        // 对端断开则丢弃 turn future（取消该回合）并退出外层循环，避免连接任务
        // 永久挂起（read 循环不再 poll ws_stream 导致 close 永远不可见）；若 turn
        // 期间对端又发来 user_message，缓冲到 pending，turn 结束后优先处理。
        // rt 是 &mut 借用且被移入 turn future——回合成功后的标题生成需要会话模型，
        // 故在此先 clone（借用仍在期，turn 结束后无法再访问 rt）。
        let turn_model = rt.model.clone();
        // select 循环内 rt 被 turn future 独占借用，cancel/断连分支要下发取消信号，
        // 此处先 clone workspace/client 标识备用（仅两个 String，代价可忽略）。
        let cancel_workspace_id = rt.workspace_id.clone();
        let cancel_client_id = rt.client_id.clone();
        let turn =
            crate::agent::runner::run_agent_turn(agent.clone(), llm.clone(), rt, event_tx.clone());
        // turn future 独占 rt 的可变借用：把 select 循环放进独立块，块结束时
        // turn 被 drop、借用随之结束——取消分支需置空 rt_cache，必须在块外赋值。
        let outcome = {
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    r = &mut turn => break TurnOutcome::Completed(r),
                    msg = ws_stream.next() => match msg {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {
                            // 断连：若有进行中的 exec，把取消信号下发到客户端，避免
                            // 内网机器上的命令失去监督仍在运行。
                            send_cancel_to_client(&agent, &cancel_workspace_id, &cancel_client_id, &event_tx).await;
                            break TurnOutcome::Disconnected;
                        }
                        Some(Ok(m)) => match parse_ws_frame(m) {
                            WsFrame::UserMessage { content, refs } => {
                                pending.get_or_insert((content, refs));
                            }
                            // 中断式取消：先下发真取消信号（支持新协议的客户端），
                            // 再 drop turn future（与断连路径一致），但连接保留，
                            // 回发 stopped 帧后继续外层循环等下一条消息。
                            WsFrame::Cancel => {
                                // 真取消：先把进行中的 exec 请求 id 下发到客户端
                                // （仅支持新协议的客户端；老客户端退化为停止等待）。
                                send_cancel_to_client(&agent, &cancel_workspace_id, &cancel_client_id, &event_tx).await;
                                let _ = event_tx
                                    .send(serde_json::json!({"type": "stopped"}))
                                    .await;
                                // 停止的意图是"都停下"：清空已缓冲的排队消息，避免
                                // 下一轮外层循环继续消费它们。
                                pending = None;
                                break TurnOutcome::Cancelled;
                            }
                            WsFrame::ApprovalResponse {
                                request_id,
                                approved,
                                option_id,
                                remember,
                            } => {
                                // 唤醒挂起的审批（跨 runner future 边界，靠 AgentState
                                // 的 pending map 可达）；未知 request_id 静默忽略。
                                agent
                                    .resolve_approval(
                                        &session_id,
                                        &request_id,
                                        approved,
                                        option_id,
                                        remember,
                                    )
                                    .await;
                            }
                            // 配置切换是 ACP 会话概念：本内层循环只跑自研 runner
                            // 路径（ACP 回合在外层循环处理），无会话配置可切换，忽略。
                            WsFrame::SetConfigOption { .. } => {}
                            // Runner 审批模式切换：回合内仅持久化 DB（rt 被 turn future 借用，
                            // 不可修改）；下一轮 LLM 调用从 rt.approval_mode 读取最新值
                            // （ws.rs 外层 loop 下一帧处理时 rt_cache 已更新）。
                            WsFrame::SetMode { mode } => {
                                if let Some(agent) = state.server_state.agent_state.as_ref() {
                                    if let Ok(Some(ws)) = load_workspace_for_session(&agent.db, &session_id).await {
                                        let _ = agent.db.agent_update_workspace(
                                            &ws.id, &ws.name, &ws.root_path,
                                            None, Some(&mode), None, None, None, None, None, false, false,
                                        ).await;
                                    }
                                }
                                let _ = event_tx.send(serde_json::json!({"type": "mode_updated", "mode": &mode})).await;
                            }
                            // 角色切换：回合内仅持久化 DB；下一条消息时 rt_cache 重建时加载。
                            WsFrame::SetRole { role_id } => {
                                if let Some(agent) = state.server_state.agent_state.as_ref() {
                                    let db_role_id = if role_id.is_empty() { None } else { Some(role_id.as_str()) };
                                    let _ = agent.db.agent_update_session_role(&session_id, db_role_id).await;
                                }
                                let _ = event_tx.send(serde_json::json!({"type": "role_updated", "role_id": &role_id})).await;
                            }
                            // elicitation 仅 ACP 路径产生：runner 回合内不可能有
                            // pending 表单，忽略（resolve_elicitation 对未知 id
                            // 是 no-op，ACP 帧只在外层处理，保持最小改动）。
                            WsFrame::ElicitationResponse { .. } => {}
                            WsFrame::Other => {}
                        },
                    },
                    // 死连接：与 None/Err/Close 同款断连处理（turn 内先下发 exec
                    // 取消信号再丢弃 turn future），区别是触发方为 push_task 的
                    // 心跳判定而非对端帧——半开连接不会发 close，只能靠出站探活。
                    _ = dead_notify.notified() => {
                        send_cancel_to_client(&agent, &cancel_workspace_id, &cancel_client_id, &event_tx).await;
                        break TurnOutcome::Disconnected;
                    },
                }
            }
        };
        // 取消的回合把运行时作废：turn 可能已把无配对 tool 结果的 assistant
        // tool_calls 推进 rt.messages（handle_tool_calls 先落库/入内存再逐条执行），
        // 内存路径没有 sanitize_tool_pairs 兜底，直接复用下一轮会把非法序列发给
        // LLM（400）。置 None 后下一条消息从 DB 重建——DB 重放路径的占位补齐
        // 保证序列合法。取消语义统一："取消后运行时状态作废，从 DB 对齐"。
        if matches!(outcome, TurnOutcome::Cancelled) {
            rt_cache = None;
        }
        match outcome {
            // 对端断开/协议错误：退出外层循环（原 break None 语义，保持不变）。
            TurnOutcome::Disconnected => break,
            // 取消：stopped 帧已回发，无 error 帧、无标题生成，继续等下一条消息。
            TurnOutcome::Cancelled => {}
            TurnOutcome::Completed(Err(e)) => {
                // 仅 Exhausted 场景 runner 会发终态 error 帧；其余 Err 路径需要在此兜底，
                // 否则浏览器会一直等待。Exhausted 场景会重复一个 error 帧，MVP 下无害。
                let _ = event_tx
                    .send(serde_json::json!({"type": "error", "message": e}))
                    .await;
            }
            TurnOutcome::Completed(Ok(())) => {
                // 回合成功结束：title 为空时异步生成（内部有非空竞态守卫）
                let needs_title = agent
                    .db
                    .agent_get_session(&session_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|s| s.title.as_deref().is_none_or(|t| t.trim().is_empty()));
                if needs_title {
                    // 标题帧只发到触发连接（event_tx 归本连接的 push_task 消费）：
                    // 不广播同 session 的其他标签页——需广播则要在 AgentState 维护
                    // 连接表，YAGNI 不做；其他标签页的 SessionBar 在下次 refetch 时自愈。
                    tokio::spawn(crate::agent::title::maybe_generate_title(
                        agent.clone(),
                        llm.clone(),
                        session_id.clone(),
                        turn_model,
                        Some(event_tx.clone()),
                    ));
                }
            }
        }
    }

    // 连接关闭：若本连接持有 ACP 会话的 WS 事件通道，清空之——ACP 回合在
    // 断连后仍可能存活（服务端 agent 继续跑），保留旧 sender 会让通知
    // try_send 持续成功而事件无人消费、reaper 据此误刷新活动。
    if acp_active {
        if let Some(agent) = state.server_state.agent_state.as_ref() {
            if let Some(bridge) = agent.acp_bridge.as_ref() {
                // 只清本连接自己的通道：刷新时旧连接 teardown 晚于新连接注册，
                // 无条件清空会误清新连接的 ws_tx（tool_result/done 全部丢失）。
                bridge.detach_ws_tx(&session_id, conn_id).await;
            }
        }
    }

    push_task.abort();
}

/// 把当前 workspace 进行中的 exec 取消信号下发到客户端（版本门控）。断连/取消
/// 共用。执行已结束（inflight 为空）或无匹配 id 时静默 no-op。
/// 入参用 workspace_id/client_id 而非 &SessionRuntime：select 循环内 rt 已被
/// turn future 独占借用，调用方在移入 turn 前 clone 这两个标识。
async fn send_cancel_to_client(
    agent: &crate::agent::AgentState,
    workspace_id: &str,
    client_id: &str,
    _event_tx: &tokio::sync::mpsc::Sender<serde_json::Value>,
) {
    let request_ids = agent.inflight_take(workspace_id).await;
    if request_ids.is_empty() {
        return;
    }
    let version = agent
        .registry
        .client_handle(client_id)
        .await
        .and_then(|e| e.client_version.clone());
    if !crate::agent::runner::client_supports_cancel(version.as_deref()) {
        tracing::debug!("client {} does not support cancel, skipping", client_id);
        return;
    }
    // 只读并发组可能多条在途：逐条下发取消。
    for request_id in request_ids {
        if !agent
            .registry
            .send_agent_cancel(client_id, &request_id)
            .await
        {
            tracing::debug!("send_agent_cancel failed for client {}", client_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthConfig;
    use crate::control::ServerState;
    use crate::db::Database;
    use std::sync::Arc;

    #[test]
    fn test_heartbeat_frame_shape() {
        // 前端看门狗以「任意帧到达」判定存活，只要求 type 与自增时间戳两个字段
        // 稳定可解析；ts 必须为正整数（u64 秒），前端据此可计算最后活动时间。
        let frame = heartbeat_frame();
        assert_eq!(frame["type"], "heartbeat");
        let ts = frame["ts"].as_u64().expect("ts must be a u64");
        assert!(ts > 0, "ts should be a positive unix timestamp, got {ts}");
    }

    #[test]
    fn test_turn_outcome_cancel_is_not_success() {
        // 回归：取消此前被编码为 break Some(Ok(()))，会落入 `Ok` 分支触发
        // 标题生成（浪费一次 LLM 调用并可能生成误导性标题）。改为可区分的
        // 终态后，取消/断连/失败都不算回合成功，仅 Completed(Ok) 才进入
        // 标题生成路径。
        assert!(matches!(
            TurnOutcome::Completed(Ok(())),
            TurnOutcome::Completed(Ok(()))
        ));
        assert!(!matches!(
            TurnOutcome::Cancelled,
            TurnOutcome::Completed(Ok(()))
        ));
        assert!(!matches!(
            TurnOutcome::Disconnected,
            TurnOutcome::Completed(Ok(()))
        ));
        assert!(!matches!(
            TurnOutcome::Completed(Err("boom".into())),
            TurnOutcome::Completed(Ok(()))
        ));
    }

    #[test]
    fn test_parse_ws_frame_variants() {
        let user = parse_ws_frame(Message::Text(
            r#"{"type":"user_message","content":"hi"}"#.into(),
        ));
        assert!(matches!(
            user,
            WsFrame::UserMessage { content, refs } if content == "hi" && refs.is_empty()
        ));

        // @引用：refs 数组解析；无 refs 字段默认空列表
        let user_refs = parse_ws_frame(Message::Text(
            r#"{"type":"user_message","content":"看下","refs":["src/main.rs","a/b.rs"]}"#.into(),
        ));
        assert!(matches!(
            user_refs,
            WsFrame::UserMessage { content, refs } if content == "看下" && refs == ["src/main.rs", "a/b.rs"]
        ));

        let cancel = parse_ws_frame(Message::Text(r#"{"type":"cancel"}"#.into()));
        assert!(matches!(cancel, WsFrame::Cancel));

        let unknown = parse_ws_frame(Message::Text(r#"{"type":"ping"}"#.into()));
        assert!(matches!(unknown, WsFrame::Other));

        let binary = parse_ws_frame(Message::Binary(vec![1, 2, 3]));
        assert!(matches!(binary, WsFrame::Other));

        let malformed = parse_ws_frame(Message::Text("not json".into()));
        assert!(matches!(malformed, WsFrame::Other));

        let approve = parse_ws_frame(Message::Text(
            r#"{"type":"approval_response","request_id":"r1","approved":true,"remember":"session"}"#
                .into(),
        ));
        assert!(matches!(
            approve,
            WsFrame::ApprovalResponse {
                request_id,
                approved: true,
                remember: true,
                option_id: None
            } if request_id == "r1"
        ));

        let deny = parse_ws_frame(Message::Text(
            r#"{"type":"approval_response","request_id":"r2","approved":false}"#.into(),
        ));
        assert!(matches!(
            deny,
            WsFrame::ApprovalResponse {
                approved: false,
                remember: false,
                ..
            }
        ));

        // ACP options 透传：option_id 解析（优先于 approved）
        let select = parse_ws_frame(Message::Text(
            r#"{"type":"approval_response","request_id":"r3","option_id":"allow_always"}"#.into(),
        ));
        assert!(matches!(
            select,
            WsFrame::ApprovalResponse {
                request_id,
                approved: false,
                option_id: Some(id),
                remember: false,
                ..
            } if request_id == "r3" && id == "allow_always"
        ));

        // option_id 空串视为缺省（避免脏数据构造 Selected("")）
        let empty_opt = parse_ws_frame(Message::Text(
            r#"{"type":"approval_response","request_id":"r4","option_id":""}"#.into(),
        ));
        assert!(matches!(
            empty_opt,
            WsFrame::ApprovalResponse {
                option_id: None,
                ..
            }
        ));

        // 缺 request_id → Other
        let bad = parse_ws_frame(Message::Text(
            r#"{"type":"approval_response","approved":true}"#.into(),
        ));
        assert!(matches!(bad, WsFrame::Other));
    }

    #[test]
    fn test_parse_ws_frame_elicitation_response() {
        // accept + content（字段值对象）
        let accept = parse_ws_frame(Message::Text(
            r#"{"type":"elicitation_response","request_id":"r1","action":"accept","content":{"name":"Alice","age":3}}"#.into(),
        ));
        match accept {
            WsFrame::ElicitationResponse {
                request_id,
                action,
                content,
            } => {
                assert_eq!(request_id, "r1");
                assert_eq!(action, "accept");
                let content = content.expect("accept should carry content");
                assert_eq!(content["name"], "Alice");
                assert_eq!(content["age"], 3);
            }
            _ => panic!("expected ElicitationResponse(accept)"),
        }

        // decline / cancel：无 content
        for action in ["decline", "cancel"] {
            let frame = parse_ws_frame(Message::Text(format!(
                r#"{{"type":"elicitation_response","request_id":"r2","action":"{action}"}}"#
            )));
            assert!(
                matches!(
                    frame,
                    WsFrame::ElicitationResponse { action: a, content: None, .. } if a == action
                ),
                "action={action} frame should parse"
            );
        }

        // 缺 request_id → Other
        let no_id = parse_ws_frame(Message::Text(
            r#"{"type":"elicitation_response","action":"accept"}"#.into(),
        ));
        assert!(matches!(no_id, WsFrame::Other));

        // 非法 action → Other
        let bad_action = parse_ws_frame(Message::Text(
            r#"{"type":"elicitation_response","request_id":"r3","action":"maybe"}"#.into(),
        ));
        assert!(matches!(bad_action, WsFrame::Other));
    }

    #[test]
    fn test_parse_set_config_option() {
        let frame = parse_ws_frame(Message::Text(
            r#"{"type":"set_config_option","config_id":"mode","value":"plan"}"#.into(),
        ));
        match frame {
            WsFrame::SetConfigOption { config_id, value } => {
                assert_eq!(config_id, "mode");
                assert_eq!(value, "plan");
            }
            _ => panic!("expected SetConfigOption"),
        }
    }

    #[test]
    fn test_parse_set_config_option_missing_fields() {
        let frame = parse_ws_frame(Message::Text(
            r#"{"type":"set_config_option","config_id":""}"#.into(),
        ));
        assert!(matches!(frame, WsFrame::Other));
    }

    #[test]
    fn test_parse_set_role() {
        let frame = parse_ws_frame(Message::Text(
            r#"{"type":"set_role","role_id":"r1"}"#.into(),
        ));
        match frame {
            WsFrame::SetRole { role_id } => assert_eq!(role_id, "r1"),
            _ => panic!("expected SetRole"),
        }
        // 空 role_id = 清除角色
        let clear = parse_ws_frame(Message::Text(
            r#"{"type":"set_role","role_id":""}"#.into(),
        ));
        match clear {
            WsFrame::SetRole { role_id } => assert!(role_id.is_empty()),
            _ => panic!("expected SetRole with empty role_id"),
        }
    }

    async fn test_state() -> (ApiState, Database) {
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db.clone());
        (
            ApiState {
                server_state,
                auth_config: Arc::new(AuthConfig::new(None, None)),
                log_store: None,
            },
            db,
        )
    }

    fn ws_record() -> crate::db::agent::AgentWorkspaceRecord {
        crate::db::agent::AgentWorkspaceRecord {
            id: "w1".into(),
            name: "proj".into(),
            client_id: "nas".into(),
            runtime_type: "host".into(),
            root_path: "/p".into(),
            docker_image: None,
            docker_container_id: None,
            approval_mode: "safe".into(),
            system_prompt: None,
            agent_type: String::new(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
            github_token: None,
            github_owner: None,
            github_repo: None,
            created_at: "t".into(),
            updated_at: "t".into(),
        }
    }

    #[test]
    fn test_use_acp_path() {
        // agent_type 为空 → 自研 runner 路径；非空 → ACP 路径
        assert!(!use_acp_path(&ws_record()));
        let mut ws = ws_record();
        ws.agent_type = "gemini".into();
        assert!(use_acp_path(&ws));
        ws.agent_type = "claude-code".into();
        assert!(use_acp_path(&ws));
    }

    #[tokio::test]
    async fn test_load_workspace_for_session_not_found_is_ok_none() {
        // 评审 Finding 3：session/workspace「不存在」是 Ok(None)（可回退 runner），
        // 与读库错误 Err 区分开——后者不应静默落到自研 runner（用错引擎）。
        let (_state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        // 存在 → Some(workspace)
        let ws = load_workspace_for_session(&db, "s1")
            .await
            .expect("load ok")
            .expect("workspace exists");
        assert_eq!(ws.id, "w1");
        // session 不存在 → Ok(None)，不 panic
        assert!(load_workspace_for_session(&db, "ghost")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_inject_refs_empty_passthrough() {
        // 无 @引用：原样返回，不触碰隧道（exec_on_client 也不被调用）。
        let (state, _db) = test_state().await;
        let agent = state.server_state.agent_state.as_ref().unwrap().clone();
        let out = inject_refs(&agent, "w1", "nas", "/p", None, "hello", &[]).await;
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn test_refresh_session_state_applies_patched_model() {
        let (_state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();

        // 会话模型已在 load 时解析为 gpt-4o；PATCH 落库新模型后，下一轮重读应覆盖。
        let mut rt_model = "gpt-4o".to_string();
        let mut role = None;
        db.agent_update_session_model("s1", Some("claude-opus-5"))
            .await
            .unwrap();
        let explicit = refresh_session_state(&db, "s1", &mut rt_model, &mut role).await;
        assert_eq!(rt_model, "claude-opus-5");
        assert!(explicit);

        // 模型值相同 → 保持原值，无多余写。
        refresh_session_state(&db, "s1", &mut rt_model, &mut role).await;
        assert_eq!(rt_model, "claude-opus-5");

        // 清除（None）→ 无角色/无 workspace 覆盖时回退全局默认；默认也为空则保持现状。
        db.agent_update_session_model("s1", None).await.unwrap();
        let explicit = refresh_session_state(&db, "s1", &mut rt_model, &mut role).await;
        assert!(!explicit);
        assert_eq!(rt_model, "claude-opus-5");

        // 不存在的会话 → 静默保持原模型。
        let mut other = "keep".to_string();
        let mut other_role = None;
        refresh_session_state(&db, "ghost", &mut other, &mut other_role).await;
        assert_eq!(other, "keep");
    }

    #[tokio::test]
    async fn test_refresh_session_state_role_and_model_override() {
        let (_state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None).await.unwrap();
        db.role_insert(
            "r1",
            "reviewer",
            "代码评审",
            "prompt",
            None,
            None,
            Some("role-model-x"),
            "primary",
            "global",
            "",
            "",
        )
        .await
        .unwrap();

        // 绑定角色 + session.model 为空 → active_role 同步、角色 model_override 生效。
        db.agent_update_session_role("s1", Some("r1")).await.unwrap();
        let mut rt_model = "default-model".to_string();
        let mut role = None;
        let explicit = refresh_session_state(&db, "s1", &mut rt_model, &mut role).await;
        assert!(!explicit);
        assert_eq!(rt_model, "role-model-x");
        assert_eq!(role.as_ref().map(|r| r.name.as_str()), Some("reviewer"));

        // session.model 显式设置 → 优先于角色 model_override。
        db.agent_update_session_model("s1", Some("gpt-4o")).await.unwrap();
        let explicit = refresh_session_state(&db, "s1", &mut rt_model, &mut role).await;
        assert!(explicit);
        assert_eq!(rt_model, "gpt-4o");

        // 解绑角色 → active_role 清空、模型回退（session.model 仍显式）。
        db.agent_update_session_role("s1", None).await.unwrap();
        refresh_session_state(&db, "s1", &mut rt_model, &mut role).await;
        assert!(role.is_none());

        // 禁用角色 → 视为无角色。
        db.agent_update_session_role("s1", Some("r1")).await.unwrap();
        db.role_toggle_enabled("r1").await.unwrap();
        db.agent_update_session_model("s1", None).await.unwrap();
        let mut rt_model2 = "default-model".to_string();
        let mut role2 = None;
        refresh_session_state(&db, "s1", &mut rt_model2, &mut role2).await;
        assert!(role2.is_none());
        assert_eq!(rt_model2, "default-model");
    }

    #[test]
    fn test_terminal_ws_auth_status_requires_token() {
        // `terminal_ws` 的 JWT 拒绝路径。端到端 WS 握手测试需要真实 upgrade
        // （WebSocketUpgrade 字段私有、OnUpgrade 扩展仅真实连接存在），这里跳过；
        // 直接测 handler 使用的纯函数 `terminal_ws_auth_status`，覆盖
        // 「auth enabled 且无 token → 401」的拒绝分支（与 agent_ws 同款校验）。
        let auth = AuthConfig::new(Some("pw".into()), Some("secret".into()));
        // auth enabled：无 token / 空 token / 非法 token → 401
        assert_eq!(
            terminal_ws_auth_status(&auth, None),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            terminal_ws_auth_status(&auth, Some("")),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            terminal_ws_auth_status(&auth, Some("bogus-token")),
            Some(StatusCode::UNAUTHORIZED)
        );
        // 合法 token → 放行
        let token = crate::auth::create_token("secret").unwrap();
        assert_eq!(terminal_ws_auth_status(&auth, Some(token.as_str())), None);
        // auth 未启用 → 一律放行（与受保护路由中间件的语义一致）
        let disabled = AuthConfig::new(None, None);
        assert_eq!(terminal_ws_auth_status(&disabled, None), None);
        assert_eq!(terminal_ws_auth_status(&disabled, Some("anything")), None);
    }

    #[test]
    fn test_terminal_negotiation_frame_omits_shell_when_none() {
        // host runtime：shell 字段不出现在协商帧中（客户端回退系统默认 shell）
        let frame = serde_json::to_vec(&PtyNegotiation {
            rows: 24,
            cols: 80,
            shell: None,
            id: None,
        })
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(json["rows"], 24);
        assert_eq!(json["cols"], 80);
        assert!(json.get("shell").is_none());
        assert!(json.get("id").is_none());

        // docker runtime：shell 原样透传
        let frame = serde_json::to_vec(&PtyNegotiation {
            rows: 40,
            cols: 120,
            shell: Some("docker exec -it dev-ctr sh"),
            id: None,
        })
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(json["shell"], "docker exec -it dev-ctr sh");
    }
}
