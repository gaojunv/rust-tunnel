//! Agent workbench REST + WebSocket handlers.
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::server::agent::session::SessionRuntime;
use crate::server::auth::{validate_token, AuthConfig};
use crate::server::llm::ChatMessage;

use super::ApiState;

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub client_id: String,
    pub runtime_type: String, // "host" | "docker"
    pub root_path: String,
    pub docker_image: Option<String>,
    /// Pre-started container to `docker exec` into. MVP: container lifecycle is
    /// out of scope — the user must start the container and supply its id here.
    pub docker_container_id: Option<String>,
    /// ACP 远程 agent 引擎：空串（缺省，向后兼容）为内置 runner；非空取
    /// `gemini` / `claude-code` / `opencode`（见 `spawner::agent_command`）。
    #[serde(default)]
    pub agent_type: String,
    /// ACP agent 可执行文件路径；缺省 None 时依赖 PATH 查找。
    #[serde(default)]
    pub agent_path: Option<String>,
    /// workspace 默认 LLM 模型 id（`llm_models.id`，ACP 会话启动时必需）。
    #[serde(default)]
    pub llm_model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateWorkspaceRequest {
    pub name: String,
    pub root_path: String,
    pub system_prompt: Option<String>,
    pub approval_mode: Option<String>,
    /// ACP 字段，COALESCE 语义：缺省 None 保持原值。`agent_type` 空串表示切回内置
    /// runner；`agent_path`/`llm_model_id` 空串视为忽略（本迭代不支持清空）。
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub agent_path: Option<String>,
    #[serde(default)]
    pub llm_model_id: Option<String>,
}

/// 校验 agent_type：空串（内置 runner）或受支持的 ACP 引擎。
fn validate_agent_type(agent_type: &str) -> bool {
    matches!(agent_type, "" | "gemini" | "claude-code" | "opencode")
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub title: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSessionModelRequest {
    /// 空串表示清除会话模型，回退到默认解析。
    pub model: String,
}

// 共享常量：session.rs 的 resolve_effective_model / has_any_model_config 同用。
use crate::server::agent::session::DEFAULT_MODEL_KEY;

#[derive(Debug, serde::Serialize)]
pub struct DefaultModelResponse {
    pub model: String,
}

/// GET /api/agent/default-model — 读全局默认模型（未设置返回空串）。
pub async fn get_default_model(State(state): State<ApiState>) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.load_server_setting(DEFAULT_MODEL_KEY).await {
        Ok(Some(m)) => Json(DefaultModelResponse { model: m }).into_response(),
        Ok(None) => Json(DefaultModelResponse {
            model: String::new(),
        })
        .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// PUT /api/agent/default-model — 写全局默认模型（空串清除）。
pub async fn put_default_model(
    State(state): State<ApiState>,
    Json(body): Json<UpdateSessionModelRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent
        .db
        .save_server_setting(DEFAULT_MODEL_KEY, body.model.trim())
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateSessionRequest {
    pub title: String,
}

/// Generate a random hex id (32 hex chars, 128-bit).
fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

#[derive(Debug, Deserialize)]
pub struct AgentWsQuery {
    pub session_id: String,
    pub token: Option<String>,
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

#[derive(Debug, Deserialize)]
pub struct TerminalWsQuery {
    pub workspace_id: String,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub token: Option<String>,
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
#[derive(serde::Serialize)]
struct PtyNegotiation<'a> {
    rows: u16,
    cols: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell: Option<&'a str>,
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
    let entry = agent.registry.get(&ws.client_id).await;
    let version = entry.as_ref().and_then(|e| e.client_version.clone());
    if !crate::server::agent::runner::client_supports_terminal(version.as_deref()) {
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
    let target = format!("127.0.0.1:{}", crate::client::pty::DEFAULT_PTY_PORT);
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
    let negotiation = PtyNegotiation {
        rows,
        cols,
        shell: shell.as_deref(),
    };
    let mut frame = serde_json::to_vec(&negotiation)
        .expect("serde_json::to_vec on a flat struct is infallible");
    frame.push(b'\n');
    if let Err(e) = tunnel.write_all(&frame).await {
        tracing::warn!("terminal ws: write PTY negotiation frame failed: {e}");
        return;
    }

    // 7. 双向桥接：WS binary ↔ 隧道字节流。
    bridge_terminal(ws_sink, ws_stream, tunnel).await;
}

/// 双向桥接：WS binary ↔ 隧道字节流。任一方向结束即整体退出（tunnel drop 发
/// Close 给客户端，WS 连接随之关闭）。`tokio::io::split` 把隧道拆成读/写两半，
/// 供 select! 两个分支同时借用。
async fn bridge_terminal(
    mut ws_sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut ws_stream: futures_util::stream::SplitStream<WebSocket>,
    tunnel: crate::server::tunnel_stream::ClientTunnelStream,
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
        // 方向二：WebSocket → 隧道。Close/EOF/错误即结束；Text/Ping/Pong 忽略
        // （协议只用 Binary；浏览器对服务端 ping 自动回 pong，且自身从不发 ping）。
        res = async {
            loop {
                match ws_stream.next().await {
                    Some(Ok(Message::Binary(data))) => {
                        if tunnel_wr.write_all(&data).await.is_err() {
                            break Err("tunnel write failed".to_string());
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break Ok(()),
                    // Text/Ping/Pong 忽略：协议只用 Binary。浏览器对服务端 ping 自动
                    // 回 pong、且自身从不发 ping，服务端无需处理。
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

/// 会话模型「下一条消息生效」：每轮从 DB 重读 `session.model`，若已设置（非空）
/// 且与运行时当前模型不同则覆盖。`session.model` 为 `None` 表示回退默认——保持
/// `SessionRuntime::load` 的加载路径语义（此时 `rt.model` 已在 load/首轮解析为
/// 默认或第一个可用模型），不据此覆盖，避免把已解析的模型改写为空串。
async fn refresh_session_model(
    db: &crate::server::db::Database,
    session_id: &str,
    rt_model: &mut String,
) {
    let Ok(Some(session)) = db.agent_get_session(session_id).await else {
        return;
    };
    if let Some(model) = session.model {
        let model = model.trim();
        if !model.is_empty() && model != rt_model {
            *rt_model = model.to_string();
        }
    }
}

/// `WS` 客户端帧分类：`user_message` / `cancel` / `approval_response` / 其他（忽略）。
enum WsFrame {
    /// 用户消息：content + 可选 @引用文件路径列表
    UserMessage { content: String, refs: Vec<String> },
    Cancel,
    /// 审批响应：`request_id`、是否批准、是否本会话记住该类工具
    ApprovalResponse {
        request_id: String,
        approved: bool,
        remember: bool,
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
            let content = body.get("content").and_then(|c| c.as_str()).map(str::to_string);
            let refs = body
                .get("refs")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .take(crate::server::agent::runner::MAX_REFS)
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
                remember: matches!(
                    body.get("remember").and_then(|v| v.as_str()),
                    Some("session")
                ),
            }
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
fn use_acp_path(workspace: &crate::server::persistence::db::agent::AgentWorkspaceRecord) -> bool {
    !workspace.agent_type.is_empty()
}

/// 按 session 加载其 workspace 记录（分派 ACP 路径用）。
/// `Ok(None)` = session/workspace 不存在（可回退自研 runner 路径，其内部会再报错）；
/// `Err` = 读库失败（瞬态 DB 错误）。调用方对 `Err` 应发 error 帧并跳过本回合，
/// 不能静默回退——否则 ACP 配置的 workspace 会落到自研 runner（用错引擎）。
async fn load_workspace_for_session(
    db: &crate::server::db::Database,
    session_id: &str,
) -> Result<Option<crate::server::persistence::db::agent::AgentWorkspaceRecord>, String> {
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
    agent: &crate::server::agent::AgentState,
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
        if total >= crate::server::agent::runner::MAX_REFS_TOTAL_BYTES {
            ref_files.push((path.clone(), Err("refs total size limit".to_string())));
            continue;
        }
        let result = crate::server::agent::executor::exec_on_client(
            agent,
            workspace_id,
            client_id,
            root_path,
            docker_container,
            crate::common::AgentCommand::ReadFile { path: path.clone() },
        )
        .await;
        match result {
            crate::common::AgentResult::FileContent { content: c } => {
                total += c.len();
                ref_files.push((path.clone(), Ok(c)));
            }
            _ => ref_files.push((path.clone(), Err("read failed".to_string()))),
        }
    }
    crate::server::agent::runner::compose_user_message(content, &ref_files)
}

async fn handle_agent_socket(state: ApiState, socket: WebSocket, session_id: String) {
    let (mut ws_sink, mut ws_stream) = socket.split();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);

    // 推送任务：event_rx → WebSocket。对端断开导致 send 失败时不再 break，而是
    // 继续 drain event_rx——runner 内部仍是阻塞式 send().await，但只要接收端持续
    // 消费，64 槽 channel 就不会填满，runner 永不阻塞。所有发送方 drop 后
    // recv() 返回 None，任务自然结束；外层循环退出后仍由 push_task.abort() 兜底。
    let push_task = tokio::spawn(async move {
        let mut sink_alive = true;
        while let Some(ev) = event_rx.recv().await {
            let text = serde_json::to_string(&ev).unwrap_or_default();
            if sink_alive && ws_sink.send(Message::Text(text)).await.is_err() {
                sink_alive = false;
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

    loop {
        // 优先消费缓冲的 pending 消息；否则从 socket 读取下一条。
        let (content, refs) = if let Some(p) = pending.take() {
            p
        } else {
            let msg = match ws_stream.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(m)) => m,
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
                                let _ = event_tx
                                    .send(serde_json::json!({"type": "stopped"}))
                                    .await;
                            }
                        }
                    }
                    continue;
                }
                WsFrame::ApprovalResponse {
                    request_id,
                    approved,
                    remember,
                } => {
                    if acp_active {
                        if let Some(agent) = state.server_state.agent_state.as_ref() {
                            agent
                                .resolve_approval(&session_id, &request_id, approved, remember)
                                .await;
                        }
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
                .proxy_state
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
                    .ensure_session(&session_id, &acp_workspace, event_tx.clone())
                    .await
                {
                    let _ = event_tx
                        .send(serde_json::json!({"type": "error", "message": e}))
                        .await;
                    continue;
                }
                if let Err(e) = bridge.prompt(&session_id, &content).await {
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
                // 会话模型「下一条消息生效」：PATCH 仅落库，每轮从 DB 重读
                // session.model 并覆盖 rt.model（非空时），无需重连 WS 即生效。
                refresh_session_model(&agent.db, &session_id, &mut rt.model).await;
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
                    if let Ok(models) =
                        crate::server::llm::router::list_available_models(&llm).await
                    {
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
        let turn = crate::server::agent::runner::run_agent_turn(
            agent.clone(),
            llm.clone(),
            rt,
            event_tx.clone(),
        );
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
                                remember,
                            } => {
                                // 唤醒挂起的审批（跨 runner future 边界，靠 AgentState
                                // 的 pending map 可达）；未知 request_id 静默忽略。
                                agent
                                    .resolve_approval(
                                        &session_id,
                                        &request_id,
                                        approved,
                                        remember,
                                    )
                                    .await;
                            }
                            WsFrame::Other => {}
                        },
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
                    tokio::spawn(crate::server::agent::title::maybe_generate_title(
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
                bridge.detach_ws_tx(&session_id).await;
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
    agent: &crate::server::agent::AgentState,
    workspace_id: &str,
    client_id: &str,
    _event_tx: &tokio::sync::mpsc::Sender<serde_json::Value>,
) {
    let Some(request_id) = agent.inflight_take(workspace_id).await else {
        return;
    };
    let version = agent
        .registry
        .get(client_id)
        .await
        .and_then(|e| e.client_version.clone());
    if !crate::server::agent::runner::client_supports_cancel(version.as_deref()) {
        tracing::debug!("client {} does not support cancel, skipping", client_id);
        return;
    }
    if !agent.registry.send_agent_cancel(client_id, &request_id).await {
        tracing::debug!("send_agent_cancel failed for client {}", client_id);
    }
}

pub async fn list_workspaces(State(state): State<ApiState>) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_list_workspaces().await {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn create_workspace(
    State(state): State<ApiState>,
    Json(body): Json<CreateWorkspaceRequest>,
) -> impl IntoResponse {
    if body.runtime_type != "host" && body.runtime_type != "docker" {
        return (
            StatusCode::BAD_REQUEST,
            "runtime_type must be 'host' or 'docker'",
        )
            .into_response();
    }
    if body.runtime_type == "docker" && body.docker_image.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "docker_image required for docker runtime",
        )
            .into_response();
    }
    if !validate_agent_type(&body.agent_type) {
        return (
            StatusCode::BAD_REQUEST,
            "agent_type must be '' | gemini | claude-code | opencode",
        )
            .into_response();
    }
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 可选 ACP 字段的空串归一化为 None：存储保持 NULL 而非空串。
    let agent_path = body.agent_path.as_deref().filter(|s| !s.is_empty());
    let llm_model_id = body.llm_model_id.as_deref().filter(|s| !s.is_empty());
    let id = new_id();
    match agent
        .db
        .agent_create_workspace(
            &id,
            &body.name,
            &body.client_id,
            &body.runtime_type,
            &body.root_path,
            body.docker_image.as_deref(),
            body.docker_container_id.as_deref(),
            &body.agent_type,
            agent_path,
            llm_model_id,
        )
        .await
    {
        Ok(()) => match agent.db.agent_get_workspace(&id).await {
            Ok(Some(ws)) => Json(ws).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn get_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_get_workspace(&id).await {
        Ok(Some(ws)) => Json(ws).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn update_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateWorkspaceRequest>,
) -> impl IntoResponse {
    // approval_mode 校验：非法值拒绝（而不是静默落库）
    if let Some(m) = body.approval_mode.as_deref() {
        if !matches!(m, "safe" | "auto_write" | "full_auto") {
            return (
                StatusCode::BAD_REQUEST,
                "approval_mode must be safe|auto_write|full_auto",
            )
                .into_response();
        }
    }
    // agent_type 校验：空串合法（切回内置 runner），非法引擎拒绝
    if let Some(t) = body.agent_type.as_deref() {
        if !validate_agent_type(t) {
            return (
                StatusCode::BAD_REQUEST,
                "agent_type must be '' | gemini | claude-code | opencode",
            )
                .into_response();
        }
    }
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 空串归一化为 None（保持字段语义：未设置 ≠ 空串）
    let system_prompt = body
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // ACP 字段 COALESCE 语义：None 保持原值；agent_type 空串合法（回到内置 runner）；
    // agent_path/llm_model_id 空串归一化为 None（本迭代不支持清空，见 Task 8 brief）。
    let agent_path = body.agent_path.as_deref().filter(|s| !s.is_empty());
    let llm_model_id = body.llm_model_id.as_deref().filter(|s| !s.is_empty());
    match agent
        .db
        .agent_update_workspace(
            &id,
            &body.name,
            &body.root_path,
            system_prompt,
            body.approval_mode.as_deref(),
            body.agent_type.as_deref(),
            agent_path,
            llm_model_id,
        )
        .await
    {
        Ok(()) => get_workspace(State(state), Path(id)).await.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn delete_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_delete_workspace(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceFilesQuery {
    pub q: String,
    pub limit: Option<usize>,
}

/// 面板执行辅助：加载 workspace、docker container 存在性检查、经隧道执行命令。
/// 错误一律映射为 HTTP 响应（404/503），与 `list_workspace_files` 的语义一致：
/// 客户端离线/隧道失败/exec 错误 → 503（前端区分「离线」与「空结果」）。
async fn workspace_exec(
    state: &ApiState,
    workspace_id: &str,
    command: crate::common::AgentCommand,
) -> Result<crate::common::AgentResult, axum::response::Response> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    };
    let ws = match agent.db.agent_get_workspace(workspace_id).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return Err(StatusCode::NOT_FOUND.into_response()),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    };
    if ws.runtime_type == "docker" && ws.docker_container_id.is_none() {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    let result = crate::server::agent::executor::exec_on_client(
        agent,
        &ws.id,
        &ws.client_id,
        &ws.root_path,
        ws.docker_container_id.as_deref(),
        command,
    )
    .await;
    match result {
        crate::common::AgentResult::Error { .. } => {
            Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
        ok => Ok(ok),
    }
}

#[derive(Debug, Deserialize)]
pub struct FsPathQuery {
    /// 相对工作区根的路径；tree 默认 "."，file 必填。
    pub path: Option<String>,
}

/// GET /api/agent/workspaces/:id/fs/tree?path=<rel>
/// FilesPanel 目录树数据源：ListDir 输出（目录以 '/' 结尾）解析为结构化 JSON。
pub async fn get_fs_tree(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<FsPathQuery>,
) -> impl IntoResponse {
    let path = params.path.unwrap_or_else(|| ".".to_string());
    let result = match workspace_exec(&state, &id, crate::common::AgentCommand::ListDir { path })
        .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let crate::common::AgentResult::FileContent { content } = result else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let entries: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let is_dir = l.ends_with('/');
            let name = l.strip_suffix('/').unwrap_or(l);
            serde_json::json!({ "name": name, "is_dir": is_dir })
        })
        .collect();
    Json(serde_json::json!({ "entries": entries })).into_response()
}

/// GET /api/agent/workspaces/:id/fs/file?path=<rel>
/// FilesPanel 文件预览：返回内容与截断标记（客户端 100KB 截断惯例 `[truncated]`）。
pub async fn get_fs_file(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<FsPathQuery>,
) -> impl IntoResponse {
    let Some(path) = params.path.filter(|p| !p.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "path is required").into_response();
    };
    let result = match workspace_exec(&state, &id, crate::common::AgentCommand::ReadFile { path })
        .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let crate::common::AgentResult::FileContent { content } = result else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let truncated = content.contains("[truncated]");
    Json(serde_json::json!({ "content": content, "truncated": truncated })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct PutFsFileRequest {
    pub path: String,
    pub content: String,
    /// 前端确认后重发携带：跳过审批检查（用户已在面板确认）。
    pub approved: Option<bool>,
}

/// PUT /api/agent/workspaces/:id/fs/file
/// FilesPanel 文件保存。按 workspace approval_mode 判定：需审批且未确认 → 409
/// `{needs_approval:true}`，前端弹确认后带 `approved:true` 重发。
pub async fn put_fs_file(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<PutFsFileRequest>,
) -> impl IntoResponse {
    if body.path.is_empty() {
        return (StatusCode::BAD_REQUEST, "path is required").into_response();
    }
    // 审批判定需要 approval_mode：先取 workspace（workspace_exec 内部还会再取一次，
    // 多一次 DB 读换取 helper 复用，面板低频操作可接受）。
    let approval_mode = match &state.server_state.agent_state {
        Some(agent) => match agent.db.agent_get_workspace(&id).await {
            Ok(Some(ws)) => ws.approval_mode,
            Ok(None) => return StatusCode::NOT_FOUND.into_response(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let command = crate::common::AgentCommand::WriteFile {
        path: body.path,
        content: body.content,
    };
    if !body.approved.unwrap_or(false)
        && crate::server::agent::approval::needs_approval(&approval_mode, &command)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "needs_approval": true })),
        )
            .into_response();
    }
    match workspace_exec(&state, &id, command).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(resp) => resp,
    }
}

/// GET /api/agent/workspaces/:id/git/status
/// GitPanel 数据源：`git status --porcelain=v1 -b` 原文（解析放前端）。
pub async fn get_git_status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let result = match workspace_exec(
        &state,
        &id,
        crate::common::AgentCommand::Shell {
            cmd: "git status --porcelain=v1 -b".to_string(),
            cwd: None,
        },
    )
    .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let crate::common::AgentResult::Shell { stdout, stderr, .. } = result else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    Json(serde_json::json!({ "status": stdout, "stderr": stderr })).into_response()
}

/// GET /api/agent/workspaces/:id/git/diff?path=<rel>
/// GitPanel 文件 diff：path 为空时返回整个工作区 diff。
pub async fn get_git_diff(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<FsPathQuery>,
) -> impl IntoResponse {
    let result = match workspace_exec(
        &state,
        &id,
        crate::common::AgentCommand::GitDiff {
            path: params.path.filter(|p| !p.is_empty()),
        },
    )
    .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let crate::common::AgentResult::FileContent { content } = result else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    Json(serde_json::json!({ "diff": content })).into_response()
}

/// 单引号 shell 转义：' → '\''（标准做法），包裹后任意输入安全。
fn shell_escape_q(q: &str) -> String {
    format!("'{}'", q.replace('\'', r"'\''"))
}

/// GET /api/agent/workspaces/:id/files?q=<前缀>&limit=<n>
/// @补全数据源：经隧道在沙箱内 find+grep 过滤文件路径。Windows 客户端无 find/grep
/// 时 grep 报错 → 返回空列表（前端降级手输路径），不视为错误。
pub async fn list_workspace_files(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<WorkspaceFilesQuery>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let ws = match agent.db.agent_get_workspace(&id).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if ws.runtime_type == "docker" && ws.docker_container_id.is_none() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 50);
    let q = params.q.trim();
    let cmd = if q.is_empty() {
        format!(
            "find . -path ./.git -prune -o -type f -print | head -{}",
            limit
        )
    } else {
        format!(
            "find . -path ./.git -prune -o -type f -print | grep -i -F -- {} | head -{}",
            shell_escape_q(q),
            limit
        )
    };
    let result = crate::server::agent::executor::exec_on_client(
        agent,
        &ws.id,
        &ws.client_id,
        &ws.root_path,
        ws.docker_container_id.as_deref(),
        crate::common::AgentCommand::Shell { cmd, cwd: None },
    )
    .await;
    let files: Vec<String> = match result {
        // grep 无命中 / Windows 无 grep 报错（走 stderr，stdout 为空）→ 空列表 200，降级语义保留
        crate::common::AgentResult::Shell { stdout, .. } => stdout
            .lines()
            .map(|l| l.strip_prefix("./").unwrap_or(l).to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        // 客户端离线/隧道失败/exec 错误 → 503，前端据此区分「离线」与「无匹配」。
        // Windows 无 sh 时 spawn 失败也归入此分支（规格内取舍：503 对前端同样是降级）。
        _ => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    Json(serde_json::json!({ "files": files })).into_response()
}

pub async fn list_sessions(
    State(state): State<ApiState>,
    Path(workspace_id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_list_sessions(&workspace_id).await {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn create_session(
    State(state): State<ApiState>,
    Path(workspace_id): Path<String>,
    Json(body): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_get_workspace(&workspace_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let id = new_id();
    match agent
        .db
        .agent_create_session(
            &id,
            &workspace_id,
            body.title.as_deref(),
            body.model.as_deref(),
        )
        .await
    {
        Ok(()) => match agent.db.agent_get_session(&id).await {
            Ok(Some(s)) => Json(s).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn update_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSessionRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_update_session_title(&id, &body.title).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn update_session_model(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSessionModelRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 会话不存在返回 404
    match agent.db.agent_get_session(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let model = body.model.trim();
    let model = if model.is_empty() { None } else { Some(model) };
    match agent.db.agent_update_session_model(&id, model).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn archive_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 归档即终结：杀掉该 session 的 ACP agent 进程（不存在则 no-op）。
    if let Some(bridge) = agent.acp_bridge.as_ref() {
        bridge.kill(&id).await;
    }
    match agent.db.agent_archive_session(&id).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn delete_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 删除即终结：杀掉该 session 的 ACP agent 进程（不存在则 no-op）。
    if let Some(bridge) = agent.acp_bridge.as_ref() {
        bridge.kill(&id).await;
    }
    match agent.db.agent_delete_session(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn list_messages(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_get_session(&session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    match agent.db.agent_list_messages(&session_id).await {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::AuthConfig;
    use crate::server::control::ServerState;
    use crate::server::db::Database;
    use std::sync::Arc;

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
                remember: true
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

        // 缺 request_id → Other
        let bad = parse_ws_frame(Message::Text(
            r#"{"type":"approval_response","approved":true}"#.into(),
        ));
        assert!(matches!(bad, WsFrame::Other));
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

    fn ws_record() -> crate::server::persistence::db::agent::AgentWorkspaceRecord {
        crate::server::persistence::db::agent::AgentWorkspaceRecord {
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
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
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
        assert!(load_workspace_for_session(&db, "ghost").await.unwrap().is_none());
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
    async fn test_create_and_list_workspaces() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state.clone()),
            Json(CreateWorkspaceRequest {
                name: "proj".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/home/u/proj".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: String::new(),
                agent_path: None,
                llm_model_id: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = list_workspaces(State(state)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_create_workspace_rejects_bad_runtime_type() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "x".into(),
                client_id: "nas".into(),
                runtime_type: "kubernetes".into(),
                root_path: "/p".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: String::new(),
                agent_path: None,
                llm_model_id: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_docker_workspace_persists_container_id() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "dproj".into(),
                client_id: "nas".into(),
                runtime_type: "docker".into(),
                root_path: "/container/work".into(),
                docker_image: Some("node:20".into()),
                docker_container_id: Some("dev-ctr".into()),
                agent_type: String::new(),
                agent_path: None,
                llm_model_id: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        // 响应体包含持久化的 container_id
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["runtime_type"], "docker");
        assert_eq!(json["docker_container_id"], "dev-ctr");
    }

    #[test]
    fn test_create_workspace_request_missing_acp_fields_defaults() {
        // 向后兼容：旧前端不发送 ACP 字段 → serde default 填充 agent_type=''、
        // agent_path/llm_model_id=None（自研 runner 路径）。
        let body: CreateWorkspaceRequest = serde_json::from_str(
            r#"{"name":"p","client_id":"nas","runtime_type":"host","root_path":"/p"}"#,
        )
        .unwrap();
        assert_eq!(body.agent_type, "");
        assert_eq!(body.agent_path, None);
        assert_eq!(body.llm_model_id, None);
    }

    #[tokio::test]
    async fn test_create_workspace_persists_acp_fields() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "acp-proj".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/p".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: "gemini".into(),
                agent_path: Some("/opt/gemini".into()),
                llm_model_id: Some("model-1".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        // 响应体回读 ACP 字段
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["agent_type"], "gemini");
        assert_eq!(json["agent_path"], "/opt/gemini");
        assert_eq!(json["llm_model_id"], "model-1");
    }

    #[tokio::test]
    async fn test_create_workspace_rejects_invalid_agent_type() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "x".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/p".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: "cursor".into(),
                agent_path: None,
                llm_model_id: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_workspace_acp_fields() {
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(UpdateWorkspaceRequest {
                name: "p".into(),
                root_path: "/p".into(),
                system_prompt: None,
                approval_mode: None,
                agent_type: Some("gemini".into()),
                agent_path: Some("/opt/gemini".into()),
                llm_model_id: Some("model-1".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "gemini");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/gemini"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("model-1"));
    }

    #[tokio::test]
    async fn test_update_workspace_absent_acp_fields_keep_existing() {
        // COALESCE 语义：缺省 ACP 字段（None）保持原值；agent_path/llm_model_id
        // 空串归一化为 None → 同样保持原值（本迭代不支持清空，见 brief）。
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "gemini", Some("/opt/gemini"),
            Some("model-1"),
        )
        .await
        .unwrap();
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(UpdateWorkspaceRequest {
                name: "p".into(),
                root_path: "/p".into(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: Some("".into()),
                llm_model_id: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "gemini");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/gemini"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("model-1"));
    }

    #[tokio::test]
    async fn test_update_workspace_clears_agent_type_to_builtin() {
        // agent_type 空串合法：从 ACP 引擎切回内置 runner（与 path/model 不同，可清空）。
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "gemini", None, None)
            .await
            .unwrap();
        let resp = update_workspace(
            State(state),
            Path("w1".to_string()),
            Json(UpdateWorkspaceRequest {
                name: "p".into(),
                root_path: "/p".into(),
                system_prompt: None,
                approval_mode: None,
                agent_type: Some("".into()),
                agent_path: None,
                llm_model_id: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "");
    }

    #[tokio::test]
    async fn test_session_lifecycle() {
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();

        let resp = create_session(
            State(state.clone()),
            Path("w1".to_string()),
            Json(CreateSessionRequest {
                title: Some("t".into()),
                model: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = list_sessions(State(state.clone()), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = list_messages(State(state), Path("s-nonexistent".to_string()))
            .await
            .into_response();
        // 消息列表对不存在的会话返回 404
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_update_session_model_endpoint() {
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 设置模型
        let resp = update_session_model(
            State(state.clone()),
            Path("s1".to_string()),
            Json(UpdateSessionModelRequest {
                model: "claude-opus-5".into(),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-opus-5"));

        // 空串清除
        let resp = update_session_model(
            State(state.clone()),
            Path("s1".to_string()),
            Json(UpdateSessionModelRequest { model: "  ".into() }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(db
            .agent_get_session("s1")
            .await
            .unwrap()
            .unwrap()
            .model
            .is_none());

        // 不存在的会话 → 404
        let resp = update_session_model(
            State(state),
            Path("nope".to_string()),
            Json(UpdateSessionModelRequest { model: "x".into() }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_refresh_session_model_applies_patched_model() {
        let (_state, db) = test_state().await;
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();

        // 会话模型已在 load 时解析为 gpt-4o；PATCH 落库新模型后，下一轮重读应覆盖。
        let mut rt_model = "gpt-4o".to_string();
        db.agent_update_session_model("s1", Some("claude-opus-5"))
            .await
            .unwrap();
        refresh_session_model(&db, "s1", &mut rt_model).await;
        assert_eq!(rt_model, "claude-opus-5");

        // 模型值相同 → 保持原值，无多余写。
        refresh_session_model(&db, "s1", &mut rt_model).await;
        assert_eq!(rt_model, "claude-opus-5");

        // 清除（None）→ 回退默认语义：保持加载路径已解析的模型，不覆盖为空串。
        db.agent_update_session_model("s1", None).await.unwrap();
        refresh_session_model(&db, "s1", &mut rt_model).await;
        assert_eq!(rt_model, "claude-opus-5");

        // 不存在的会话 → 静默保持原模型。
        let mut other = "keep".to_string();
        refresh_session_model(&db, "ghost", &mut other).await;
        assert_eq!(other, "keep");
    }

    #[tokio::test]
    async fn test_list_workspace_files_workspace_not_found() {
        let (state, _db) = test_state().await;
        let resp = list_workspace_files(
            State(state),
            Path("ghost".to_string()),
            Query(WorkspaceFilesQuery { q: "main".into(), limit: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_workspace_files_client_offline_returns_503() {
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "proj", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        // 客户端不在线（未注册任何客户端到 registry）：exec_on_client 隧道层
        // 立即返回 AgentResult::Error，handler 应回 503 供前端区分「离线」与「无匹配」。
        let resp = list_workspace_files(
            State(state),
            Path("w1".to_string()),
            Query(WorkspaceFilesQuery { q: "main".into(), limit: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_fs_endpoints_workspace_not_found() {
        let (state, _db) = test_state().await;
        let resp = get_fs_tree(
            State(state.clone()),
            Path("ghost".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = get_fs_file(
            State(state.clone()),
            Path("ghost".to_string()),
            Query(FsPathQuery {
                path: Some("a.rs".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = get_git_status(State(state.clone()), Path("ghost".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = get_git_diff(
            State(state),
            Path("ghost".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_fs_endpoints_client_offline_returns_503() {
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "proj", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        // 客户端离线：所有面板端点统一 503（前端据此显示「客户端离线」而非空态）。
        let resp = get_fs_tree(
            State(state.clone()),
            Path("w1".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resp = get_fs_file(
            State(state.clone()),
            Path("w1".to_string()),
            Query(FsPathQuery {
                path: Some("a.rs".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resp = get_git_status(State(state.clone()), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resp = get_git_diff(
            State(state),
            Path("w1".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_fs_file_requires_path() {
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "proj", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        let resp = get_fs_file(
            State(state),
            Path("w1".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_put_fs_file_safe_mode_needs_approval_409() {
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "proj", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        // 默认 approval_mode = safe：WriteFile 需确认。未确认 → 409 needs_approval，
        // 且不会触碰隧道（客户端离线也不会 503）。
        let resp = put_fs_file(
            State(state.clone()),
            Path("w1".to_string()),
            Json(PutFsFileRequest {
                path: "a.rs".into(),
                content: "fn main() {}".into(),
                approved: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["needs_approval"], true);

        // 确认后重发 → 通过审批层，走到隧道（客户端离线 → 503）。
        let resp = put_fs_file(
            State(state),
            Path("w1".to_string()),
            Json(PutFsFileRequest {
                path: "a.rs".into(),
                content: "fn main() {}".into(),
                approved: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_put_fs_file_full_auto_skips_approval() {
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "proj", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        db.agent_update_workspace("w1", "proj", "/p", None, Some("full_auto"), None, None, None)
            .await
            .unwrap();
        // full_auto：未确认也直接放行 → 客户端离线 503（而非 409）。
        let resp = put_fs_file(
            State(state),
            Path("w1".to_string()),
            Json(PutFsFileRequest {
                path: "a.rs".into(),
                content: "x".into(),
                approved: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_put_fs_file_workspace_not_found() {
        let (state, _db) = test_state().await;
        let resp = put_fs_file(
            State(state),
            Path("ghost".to_string()),
            Json(PutFsFileRequest {
                path: "a.rs".into(),
                content: "x".into(),
                approved: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_shell_escape_q() {
        assert_eq!(shell_escape_q("main"), "'main'");
        assert_eq!(shell_escape_q("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_escape_q("a';b|rm"), r#"'a'\'';b|rm'"#); // 单引号转义后特殊字符在引号内安全
    }

    #[tokio::test]
    async fn test_default_model_roundtrip() {
        let (state, _db) = test_state().await;

        // 未设置 → 空串
        let resp = get_default_model(State(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "");

        // 写入
        let resp = put_default_model(
            State(state.clone()),
            Json(UpdateSessionModelRequest {
                model: "deepseek-chat".into(),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        // 读回
        let resp = get_default_model(State(state.clone()))
            .await
            .into_response();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "deepseek-chat");

        // 空串清除 → 读回空串
        let resp = put_default_model(
            State(state.clone()),
            Json(UpdateSessionModelRequest { model: "".into() }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = get_default_model(State(state.clone()))
            .await
            .into_response();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "");
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
        let token = crate::server::auth::create_token("secret").unwrap();
        assert_eq!(terminal_ws_auth_status(&auth, Some(token.as_str())), None);
        // auth 未启用 → 一律放行（与受保护路由中间件的语义一致）
        let disabled = AuthConfig::new(None, None);
        assert_eq!(terminal_ws_auth_status(&disabled, None), None);
        assert_eq!(
            terminal_ws_auth_status(&disabled, Some("anything")),
            None
        );
    }

    #[test]
    fn test_terminal_negotiation_frame_omits_shell_when_none() {
        // host runtime：shell 字段不出现在协商帧中（客户端回退系统默认 shell）
        let frame = serde_json::to_vec(&PtyNegotiation {
            rows: 24,
            cols: 80,
            shell: None,
        })
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(json["rows"], 24);
        assert_eq!(json["cols"], 80);
        assert!(json.get("shell").is_none());

        // docker runtime：shell 原样透传
        let frame = serde_json::to_vec(&PtyNegotiation {
            rows: 40,
            cols: 120,
            shell: Some("docker exec -it dev-ctr sh"),
        })
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(json["shell"], "docker exec -it dev-ctr sh");
    }
}
