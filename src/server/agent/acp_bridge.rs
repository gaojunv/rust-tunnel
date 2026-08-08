//! ACP bridge: owns agent sessions over the tunnel, translates between
//! the frontend WebSocket protocol and ACP JSON-RPC.
//!
//! 生命周期：`ensure_session` 惰性 spawn（LLM 回环代理 → agent 进程），
//! 进程 stdio 经控制通道（AgentSpawnData）与 ACP 连接互转；空闲 30 分钟由
//! reaper 回收。
//!
//! ## stdio pump 与 ACP 连接
//!
//! 每个 session 一对 `tokio::io::duplex`：
//! - ACP 端（`agent_io`）交给 `agent-client-protocol` 的 `Client` 作为
//!   `ByteStreams`（入 = 进程 stdout，出 = 进程 stdin）。
//! - pump 端（`pump_io`）由 [`run_stdio_pump`] 驱动：进程 stdout
//!   （`AgentSpawnData{stdin:false}` → 通道）写入 duplex 喂给 ACP crate；
//!   ACP crate 写出的字节读回后以 `AgentSpawnData{stdin:true}` 下发客户端。
//!
//! duplex 本身就是缓冲区：handshake 期间进程早产 stdout 会缓冲在 duplex/通道
//! 里（不会丢），ACP 连接建立后随即消费。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{ByteStreams, Client, ConnectionTo};

use crate::common::ControlMessage;
use crate::server::db::Database;
use crate::server::llm::crypto::LlmCipher;
use crate::server::persistence::db::agent::AgentWorkspaceRecord;

use super::acp_events::map_update;
use super::llm_bridge;
use super::spawner::AgentSpawner;

/// spawn/协商超时：LLM 代理启动与 agent 进程拉起各限 30s。
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);
/// 空闲 30 分钟杀进程（重挂 ACP 连接由客户端 spawn manager 处理）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// reaper 检查间隔。
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// 一个已 spawn 的 ACP agent 会话。
struct SpawnedAgent {
    /// ACP 侧 session id（handshake 成功后填充）。LLM 模型配置不在此缓存——
    /// AgentLlmProxyRequest 按 session 从 DB 解析（session → workspace.llm_model_id），
    /// 保证服务端配置变更即时生效。
    acp_session_id: Option<agent_client_protocol::schema::v1::SessionId>,
    /// ACP 连接上下文（prompt/cancel 发请求）；handshake 成功后填充。
    connection: Option<ConnectionTo<agent_client_protocol::Agent>>,
    /// duplex 的 ACP 端：交 `ByteStreams` 建立 ACP 连接（handshake 时取走）。
    agent_io: Option<tokio::io::DuplexStream>,
    /// 进程 stdout 字节（`AgentSpawnData{stdin:false}`）→ pump 的通道发送端。
    /// handle_spawn_data 转发用；条目移除（kill/重拉）时 drop，pump 随之收尾。
    stdout_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// 来源客户端名（cancel/kill 时经 registry 下发 AgentExecCancel）。
    client_id: String,
    /// WS 事件通道（map_update 映射帧推前端）；ensure_session 时注册/刷新。
    /// 注意：ACP 回合异步执行，事件在 WS 连接关闭后仍可能短暂推送——handler
    /// 退出时 [`AcpBridge::detach_ws_tx`] 清空，避免无消费端长期占位。
    ws_tx: Option<mpsc::Sender<serde_json::Value>>,
    /// 回合进行中标记：prompt 置位、PromptResponse 到达/cancel 清位。防并发
    /// prompt（ACP 单连接不支持并发回合；WS session_lock 只串行化分派，
    /// 不跨异步回合）。
    busy: bool,
    /// 最近活动时间（prompt / cancel / stdio / ACP 通知都会刷新；idle reaper 依据）。
    last_activity: std::time::Instant,
    /// AgentSpawnExit 已到达（进程结束）。
    exited: bool,
}

/// ACP `session/request_permission` → 审批回调。
///
/// `(session_id, tool, summary, args_preview, ws_tx) → 是否批准`。`AcpBridge`
/// 构造时 `AgentState` 尚在构建（循环依赖），由 `AgentState::new` 通过
/// [`Self::with_approval`] 注入真实实现（走 `AgentState::request_approval`，
/// 与 runner 路径共用审批弹层与 pending map）；未注入时默认拒绝。
type ApproveFn = dyn Fn(
        String,
        String,
        String,
        String,
        mpsc::Sender<serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send>>
    + Send
    + Sync;

/// ACP 会话桥：惰性拉起 agent 进程、把 LLM 代理请求路由到服务端网关。
#[derive(Clone)]
pub struct AcpBridge {
    spawner: AgentSpawner,
    db: Database,
    /// 提供商字段解密器（来自 LLM 网关 LlmState；None 时仅支持明文历史值）。
    cipher: Option<LlmCipher>,
    /// ACP 权限请求 → 审批弹层回调（`AgentState::new` 注入；None 时默认拒绝）。
    approval: Arc<ApproveFn>,
    /// 本服务端进程的活跃 ACP 会话表：session_id → SpawnedAgent。
    sessions: Arc<Mutex<HashMap<String, SpawnedAgent>>>,
}

impl AcpBridge {
    pub fn new(spawner: AgentSpawner, db: Database) -> Self {
        let bridge = Self {
            spawner,
            db,
            cipher: None,
            approval: Arc::new(|_, _, _, _, _| Box::pin(async { false })),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        bridge.start_idle_reaper();
        bridge
    }

    /// 注入 LLM 字段解密器（提供商 API Key 落库加密；bin/server.rs 在 LLM
    /// 网关初始化后调用）。None = 无主密钥，仅能读历史明文值。
    #[must_use]
    pub fn with_cipher(mut self, cipher: Option<LlmCipher>) -> Self {
        self.cipher = cipher;
        self
    }

    /// 注入 ACP 权限请求的审批回调（`AgentState::new` 在构造期调用，解决
    /// AcpBridge ↔ AgentState 循环依赖）。`AgentState::request_approval` 与
    /// runner 路径共用 pending map，WS `approval_response` 帧对两者一视同仁。
    #[must_use]
    pub fn with_approval(mut self, approval: Arc<ApproveFn>) -> Self {
        self.approval = approval;
        self
    }

    /// 后台回收空闲 ACP agent：超 `IDLE_TIMEOUT` 未活动即移除会话表条目并
    /// 经 registry 下发进程退出语义（AgentExecCancel request_id = session_id，
    /// 客户端 spawn manager 终止对应进程）。
    fn start_idle_reaper(&self) {
        let sessions = self.sessions.clone();
        let spawner = self.spawner.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAP_INTERVAL).await;
                let stale: Vec<String> = sessions
                    .lock()
                    .await
                    .iter()
                    .filter(|(_, a)| a.last_activity.elapsed() > IDLE_TIMEOUT)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in stale {
                    // 二次锁内复查：收集 stale 到移除之间条目可能被新活动刷新
                    // （prompt/cancel/stdio），此时不应误删。
                    let client_id = {
                        let mut guard = sessions.lock().await;
                        let still_stale = guard
                            .get(&id)
                            .is_some_and(|a| a.last_activity.elapsed() > IDLE_TIMEOUT);
                        if still_stale {
                            let client_id = guard.get(&id).map(|a| a.client_id.clone());
                            guard.remove(&id);
                            tracing::info!(session_id = %id, "evicted idle ACP session");
                            client_id
                        } else {
                            None
                        }
                    };
                    // 真杀进程（锁外发送，避免持锁 await 控制通道）。
                    if let Some(client_id) = client_id {
                        spawner.send_agent_cancel(&client_id, &id).await;
                    }
                }
            }
        });
    }

    /// 惰性确保 session 就绪：stdio pump → LLM 代理 → agent 进程 → ACP handshake。
    ///
    /// 幂等：会话表已有条目则直接返回（同一 session 的多个 WS 连接/多条
    /// 消息不会重复 spawn —— Task 3 评审契约，防重复 AgentLlmProxyStart）。
    /// 幂等命中时刷新条目里的 WS 事件通道（重连/多标签页共用同一进程）。
    ///
    /// `ws_tx` 是 WS 事件通道：handshake 建立后 ACP 事件流经
    /// [`super::acp_events::map_update`] 推回前端。
    pub async fn ensure_session(
        &self,
        session_id: &str,
        workspace: &AgentWorkspaceRecord,
        ws_tx: mpsc::Sender<serde_json::Value>,
    ) -> Result<(), String> {
        // 幂等守卫 + 占位登记 + pump 基础设施一次锁内完成：并发 ensure_session
        // （同一 session 的多个 WS 连接/多条消息）看到占位条目直接短路，杜绝
        // 重复 AgentLlmProxyStart / AgentSpawnRequest（Task 3 评审契约）。
        // 校验也在此锁内做（无 await，持锁开销可忽略）。
        //
        // pump 基础设施（duplex + stdout 通道）在 spawn 之前建好：agent 进程
        // 一经拉起随时可能产出 stdout（AgentSpawnData{stdin:false}），此时
        // handle_spawn_data 能立即转发到 pump 通道 → duplex 缓冲，ACP 连接
        // 建立后消费——handshake 期间早产字节不会丢（Task 6 评审要求）。
        let pump_setup: Option<(tokio::io::DuplexStream, mpsc::Receiver<Vec<u8>>)> = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get(session_id) {
                Some(agent) if !agent.exited => {
                    // 已有活跃进程：仅刷新事件通道（多标签页/重连共用同一进程；
                    // 事件推给最新连接，避免断线后的旧 sender 占位）。
                    if let Some(a) = sessions.get_mut(session_id) {
                        a.ws_tx = Some(ws_tx.clone());
                    }
                    return Ok(());
                }
                // 进程已退出的陈旧条目：视为不存在，移除后走 spawn 路径重拉。
                // 否则死进程会阻塞 respawn 直到 30 分钟 reaper 清掉它，后续
                // prompt 一直报 "agent process has exited"。
                Some(_) => {
                    tracing::info!(session_id, "re-spawning exited ACP session");
                    sessions.remove(session_id);
                }
                None => {}
            }
            if workspace.runtime_type != "host" {
                return Err("ACP 模式暂不支持 docker workspace，请改用 host 模式".into());
            }
            // spawn 前校验模型已配置（不缓存；AgentLlmProxyRequest 按 session
            // 从 DB 解析，保证配置变更即时生效）
            workspace
                .llm_model_id
                .as_deref()
                .ok_or_else(|| "workspace 未配置 LLM 模型（llm_model_id）".to_string())?;
            let (agent_io, pump_io) = tokio::io::duplex(64 * 1024);
            let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
            sessions.insert(
                session_id.to_string(),
                SpawnedAgent {
                    acp_session_id: None,
                    connection: None,
                    agent_io: Some(agent_io),
                    stdout_tx: Some(stdout_tx),
                    client_id: workspace.client_id.clone(),
                    ws_tx: Some(ws_tx.clone()),
                    busy: false,
                    last_activity: std::time::Instant::now(),
                    exited: false,
                },
            );
            Some((pump_io, stdout_rx))
        };

        // 拿锁期不 spawn（避免长时间持锁阻塞 prompt/cancel）。先解析客户端
        // 控制通道并启动 pump（ACP→进程 stdin 方向；进程 stdout 方向已在占位
        // 里就绪）。客户端离线时 pump 不启动——start_llm_proxy 随后报错，
        // 占位被移除（stdout_tx drop → pump 收尾），无资源泄漏。
        let client_id = workspace.client_id.clone();
        let control_tx = self.spawner.client_control_sender(&client_id).await;
        if let (Some((pump_io, stdout_rx)), Some(control_tx)) = (pump_setup, control_tx) {
            let pump_session_id = session_id.to_string();
            tokio::spawn(async move {
                run_stdio_pump(pump_io, stdout_rx, control_tx, pump_session_id).await;
            });
        }

        let agent_type = &workspace.agent_type;
        let outcome = async {
            // 1) 客户端内嵌 LLM 回环代理
            let port = self
                .spawner
                .start_llm_proxy(&client_id, session_id, SPAWN_TIMEOUT)
                .await?;
            // 2) spawn agent 进程（env 注入 LLM 代理地址）
            self.spawner
                .spawn_agent(
                    &client_id,
                    session_id,
                    agent_type,
                    workspace.agent_path.as_deref(),
                    port,
                    &workspace.root_path,
                    SPAWN_TIMEOUT,
                )
                .await?;
            // 3) ACP handshake（stdio pump 已就绪，此步建立 ACP 连接 + WS 接线）
            self.acp_handshake(session_id, &ws_tx).await
        }
        .await;
        if outcome.is_err() {
            // spawn 失败：移除占位，允许后续重试。
            self.sessions.lock().await.remove(session_id);
        }
        outcome
    }

    /// ACP handshake：initialize → session/new（或 session/load 恢复）。
    ///
    /// 从占位条目取走 duplex 的 ACP 端，spawn 一个常驻连接任务（crate 的
    /// `Client` 角色 + `ByteStreams` transport），任务内完成
    /// `initialize` + `session/new`，把 `ConnectionTo<Agent>` 与 ACP session id
    /// 写回会话条目；随后 main_fn 挂起等待 incoming EOF（保持连接存活，
    /// 直到进程退出/会话被杀）。通知（`session/update`）经
    /// [`map_update`] 映射后推 ws_tx；权限请求（`session/request_permission`）
    /// 走审批回调。
    ///
    /// 注意：`agent_client_protocol::Client` 是角色标记（unit struct），并非
    /// 连接句柄；连接句柄是 `ConnectionTo<Agent>`。每 session 一条专用连接，
    /// 通知无需按 session id 过滤。
    async fn acp_handshake(
        &self,
        session_id: &str,
        ws_tx: &mpsc::Sender<serde_json::Value>,
    ) -> Result<(), String> {
        // 取走 duplex 的 ACP 端（占用即移除；后续 kill 不再持有）。
        let agent_io = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .get_mut(session_id)
                .ok_or_else(|| "session not spawned".to_string())?
                .agent_io
                .take()
                .ok_or_else(|| "ACP stdio pump not initialized".to_string())?
        };

        let sid = session_id.to_string();
        let ws_tx = ws_tx.clone();
        let approval = self.approval.clone();
        let sessions = self.sessions.clone();
        let (setup_tx, setup_rx) = oneshot::channel();

        // 常驻连接任务：connect_with 的 main_fn 完成 handshake 后保持挂起，
        // 连接生命周期 = ACP 会话生命周期。
        let sid_log = sid.clone();
        tokio::spawn(async move {
            let result = Client
                .builder()
                .on_receive_notification(
                    {
                        let sid = sid.clone();
                        let ws_tx = ws_tx.clone();
                        let sessions = sessions.clone();
                        // tool_call_id → 工具名 缓存：ACP 的 ToolCallUpdate 常不带
                        // title，而前端 ChatStream 按 tool_result.name === tool_call.name
                        // 匹配卡片——从前序 ToolCall 事件的 title 补名，保证结果能挂上。
                        let mut tool_names: HashMap<String, String> = HashMap::new();
                        async move |notification: SessionNotification, _cx| {
                            // 专用连接：所有通知都属于本 session。先刷新活动
                            // （长回合无 stdout 时不被 idle reaper 误回收）。
                            if let Some(a) = sessions.lock().await.get_mut(&sid) {
                                a.last_activity = std::time::Instant::now();
                            }
                            if let agent_client_protocol::schema::v1::SessionUpdate::ToolCall(tc) =
                                &notification.update
                            {
                                tool_names.insert(
                                    tc.tool_call_id.to_string(),
                                    tc.title.clone(),
                                );
                            }
                            if let Some(mut frame) = map_update(&notification.update) {
                                // ToolCallUpdate 缺 title 时从缓存补 name。
                                if frame["type"] == "tool_result"
                                    && frame.get("name").is_none()
                                {
                                    if let Some(id) =
                                        frame.get("id").and_then(|v| v.as_str())
                                    {
                                        if let Some(name) = tool_names.get(id) {
                                            frame["name"] =
                                                serde_json::Value::String(name.clone());
                                        }
                                    }
                                }
                                // 流式帧 try_send：前端消费跟不上时丢帧（实时流可
                                // 容忍），避免连接已关闭但 ACP 会话存活时通知处理器
                                // 阻塞卡死整个 ACP 连接。
                                if ws_tx.try_send(frame).is_err() {
                                    tracing::trace!(session_id = %sid, "acp event dropped (ws channel full/closed)");
                                }
                            }
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    {
                        let sid = sid.clone();
                        let ws_tx = ws_tx.clone();
                        let approval = approval.clone();
                        async move |request: RequestPermissionRequest, responder, _cx| {
                            let tool_name = request
                                .tool_call
                                .fields
                                .title
                                .clone()
                                .unwrap_or_else(|| "unknown tool".to_string());
                            let args_preview = request
                                .tool_call
                                .fields
                                .raw_input
                                .as_ref()
                                .map(acp_raw_to_string)
                                .unwrap_or_else(|| tool_name.clone());
                            let approved = approval(
                                sid.clone(),
                                tool_name,
                                "ACP 工具调用请求".to_string(),
                                args_preview,
                                ws_tx.clone(),
                            )
                            .await;
                            let outcome = if approved {
                                pick_option(
                                    &request.options,
                                    PermissionOptionKind::AllowAlways,
                                )
                                .or_else(|| {
                                    pick_option(
                                        &request.options,
                                        PermissionOptionKind::AllowOnce,
                                    )
                                })
                            } else {
                                pick_option(
                                    &request.options,
                                    PermissionOptionKind::RejectAlways,
                                )
                                .or_else(|| {
                                    pick_option(
                                        &request.options,
                                        PermissionOptionKind::RejectOnce,
                                    )
                                })
                            };
                            match outcome {
                                Some(option_id) => {
                                    let _ = responder.respond(RequestPermissionResponse::new(
                                        RequestPermissionOutcome::Selected(
                                            SelectedPermissionOutcome::new(option_id),
                                        ),
                                    ));
                                }
                                None => {
                                    let _ = responder.respond(RequestPermissionResponse::new(
                                        RequestPermissionOutcome::Cancelled,
                                    ));
                                }
                            }
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(
                    {
                        // DuplexStream 非 Clone：拆成读写半各包一层 futures 适配。
                        let (agent_rd, agent_wr) = tokio::io::split(agent_io);
                        ByteStreams::new(FuturesIo(agent_wr), FuturesIo(agent_rd))
                    },
                    async move |cx| {
                        // 1) initialize；2) session/new。失败则 setup 报错并关连接。
                        let setup = async {
                            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                                .block_task()
                                .await?;
                            let new_session = cx
                                .send_request(NewSessionRequest::new("."))
                                .block_task()
                                .await?;
                            let acp_session_id = new_session.session_id.clone();
                            // 写回会话条目：连接句柄 + ACP session id 供 prompt/cancel。
                            {
                                let mut map = sessions.lock().await;
                                if let Some(agent) = map.get_mut(&sid) {
                                    agent.connection = Some(cx.clone());
                                    agent.acp_session_id = Some(acp_session_id);
                                    agent.last_activity = std::time::Instant::now();
                                }
                            }
                            Ok::<(), agent_client_protocol::Error>(())
                        };
                        if let Err(e) = setup.await {
                            let _ = setup_tx.send(Err(format!("acp handshake failed: {e}")));
                            return Err(e);
                        }
                        let _ = setup_tx.send(Ok(()));
                        // 保持连接存活：进程 stdout EOF（pump 退出）时返回并关闭。
                        cx.incoming_closed().await;
                        Ok(())
                    },
                )
                .await;
            // setup 结果已由 main_fn 内部分支发送（初始化失败/成功各一次）；
            // 走到这里的是连接在 setup 之后因 EOF/错误收尾，仅记日志。
            if let Err(e) = result {
                tracing::debug!(session_id = %sid_log, "acp connection ended with error: {e}");
            }
        });

        match tokio::time::timeout(SPAWN_TIMEOUT, setup_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err("acp connection task dropped".into()),
            Err(_) => Err("acp handshake timed out".into()),
        }
    }

    /// 向 ACP 会话发送一条 prompt（fire-and-forget）。
    ///
    /// 发送 `session/prompt` 后立即返回；回合内的 `session/update` 通知经
    /// [`map_update`] 推送 ws_tx，`PromptResponse` 到达时回调发 `{"type":"done"}`
    /// 帧。回合进行中重复 prompt 报错（`busy` 守卫；ACP 单连接不支持并发回合）。
    pub async fn prompt(&self, session_id: &str, content: &str) -> Result<(), String> {
        let (connection, acp_session_id, ws_tx) = {
            let mut sessions = self.sessions.lock().await;
            let agent = sessions
                .get_mut(session_id)
                .ok_or_else(|| "session not spawned".to_string())?;
            if agent.exited {
                return Err("agent process has exited".into());
            }
            if agent.busy {
                return Err("ACP 回合进行中，请等待完成或取消后再发送".into());
            }
            // 先校验再置 busy：校验失败（handshake 未完成等）不污染回合状态。
            let connection = agent
                .connection
                .clone()
                .ok_or_else(|| "ACP handshake not complete".to_string())?;
            let acp_session_id = agent
                .acp_session_id
                .clone()
                .ok_or_else(|| "ACP handshake not complete".to_string())?;
            let ws_tx = agent
                .ws_tx
                .clone()
                .ok_or_else(|| "ACP session has no ws channel".to_string())?;
            agent.busy = true;
            agent.last_activity = std::time::Instant::now();
            (connection, acp_session_id, ws_tx)
        };

        let sessions = self.sessions.clone();
        let sid = session_id.to_string();
        let prompt = vec![ContentBlock::Text(TextContent::new(content.to_string()))];
        let send_result = connection
            .send_request_to(
                agent_client_protocol::Agent,
                PromptRequest::new(acp_session_id, prompt),
            )
            .on_receiving_result(async move |result| {
                // 回合结束（成功/失败/取消）：清 busy + 发终态帧。
                if let Some(a) = sessions.lock().await.get_mut(&sid) {
                    a.busy = false;
                }
                match result {
                    Ok(_resp) => {
                        // 终态帧走阻塞发送：低频、必须送达；前端在时通道很快被
                        // push_task 排空，不会长期阻塞。
                        let _ = ws_tx.send(serde_json::json!({"type": "done"})).await;
                    }
                    Err(e) => {
                        let _ = ws_tx
                            .send(serde_json::json!({
                                "type": "error",
                                "message": format!("acp prompt failed: {e}")
                            }))
                            .await;
                    }
                }
                Ok(())
            });
        if let Err(e) = send_result {
            // 回调注册失败（连接关闭等）：清 busy，避免会话永久卡死。
            if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
                a.busy = false;
            }
            return Err(format!("acp prompt send failed: {e}"));
        }
        Ok(())
    }

    /// 取消进行中的回合：ACP session/cancel + 客户端 AgentExecCancel。
    ///
    /// `AgentExecCancel{request_id = session_id}` 的杀进程语义由客户端 spawn
    /// manager 实现（Task 2）：终止内网侧 agent 进程。进程退出后 `exited`
    /// 置位，下一次 prompt 前 ensure_session 自动重拉新进程。
    pub async fn cancel(&self, session_id: &str) {
        tracing::info!(session_id, "ACP cancel requested");
        let (client_id, connection, acp_session_id) = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get_mut(session_id) {
                Some(agent) => {
                    agent.last_activity = std::time::Instant::now();
                    agent.busy = false;
                    (
                        agent.client_id.clone(),
                        agent.connection.clone(),
                        agent.acp_session_id.clone(),
                    )
                }
                None => return,
            }
        };
        // ACP 协议层取消：让 agent 尽快停手（stop_reason = cancelled）。
        if let (Some(cx), Some(sid)) = (connection, acp_session_id) {
            let _ = cx.send_notification(CancelNotification::new(sid));
        }
        // 客户端进程层取消：真杀（AgentExecCancel request_id = session_id）。
        self.spawner.send_agent_cancel(&client_id, session_id).await;
    }

    /// 终结 ACP 会话：杀客户端进程 + 移除会话条目（idle reaper / 会话归档关闭
    /// 用）。与 `cancel` 的区别：不再保留会话，进程死后不重拉。
    pub async fn kill(&self, session_id: &str) {
        let client_id = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .map(|a| a.client_id.clone());
        let Some(client_id) = client_id else {
            return;
        };
        self.spawner.send_agent_cancel(&client_id, session_id).await;
        self.sessions.lock().await.remove(session_id);
        tracing::info!(session_id, "killed ACP session");
    }

    /// 断开/连接关闭时清空条目里的 WS 事件通道：ACP 回合在连接关闭后仍可能
    /// 存活，保留旧 sender 会让通知处理器 try_send 持续成功而事件无人消费
    /// （更严重的是 reaper 据此刷新活动，误以为会话仍然活跃）。
    pub async fn detach_ws_tx(&self, session_id: &str) {
        if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
            a.ws_tx = None;
        }
    }

    /// 路由客户端发来的 spawn/LLM 代理控制消息（server.rs 控制循环转交）。
    ///
    /// `client_name` 是消息来源客户端（AgentLlmProxyRequest 的响应必须回发
    /// 给同一客户端；session_id 用于解析模型配置）。
    pub async fn handle_client_msg(&self, client_name: &str, msg: ControlMessage) {
        match msg {
            ControlMessage::AgentSpawnData {
                session_id,
                data,
                stdin,
            } => {
                self.handle_spawn_data(&session_id, data, stdin).await;
            }
            ControlMessage::AgentSpawnExit { session_id, code } => {
                self.handle_spawn_exit(&session_id, code).await;
            }
            ControlMessage::AgentLlmProxyRequest {
                request_id,
                session_id,
                path,
                body,
            } => {
                self.handle_llm_proxy_request(client_name, request_id, session_id, path, body)
                    .await;
            }
            _ => {
                tracing::debug!("acp_bridge: ignoring control message");
            }
        }
    }

    /// AgentSpawnData（进程 stdio 字节）：进程 stdout（stdin=false）转发到对应
    /// 会话的 ACP stdio pump；stdin=true（服务端→进程方向）理论上不会从客户端
    /// 到达，直接忽略。锁内只取发送端克隆，实际发送在锁外——channel 满时
    /// 背压等待（ACP 侧持续消费；仅在进程卡死/连接中断时积压），不阻塞
    /// prompt/cancel/reaper 对会话表的访问。
    async fn handle_spawn_data(&self, session_id: &str, data: Vec<u8>, stdin: bool) {
        if stdin {
            tracing::trace!(session_id, len = data.len(), "acp spawn data stdin=true ignored");
            return;
        }
        let stdout_tx = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get_mut(session_id) {
                Some(agent) if !agent.exited => {
                    agent.last_activity = std::time::Instant::now();
                    agent.stdout_tx.clone()
                }
                Some(_) => None,
                None => None,
            }
        };
        match stdout_tx {
            Some(tx) => {
                tracing::trace!(session_id, len = data.len(), "acp spawn data routed to pump");
                if tx.send(data).await.is_err() {
                    tracing::debug!(session_id, "spawn data: pump closed, dropped");
                }
            }
            None => tracing::debug!(session_id, "spawn data for missing/exited session, dropped"),
        }
    }

    /// AgentSpawnExit（进程退出）：标记会话已退出；后续 prompt 报错。
    /// 同时 drop pump 发送端：进程 stdout 已全部到达（控制通道保序，exit 在
    /// 最后），pump 排空残余字节后退出 → duplex EOF → ACP 连接随之关闭。
    async fn handle_spawn_exit(&self, session_id: &str, code: Option<i32>) {
        let mut sessions = self.sessions.lock().await;
        match sessions.get_mut(session_id) {
            Some(agent) => {
                agent.exited = true;
                agent.stdout_tx = None;
                agent.last_activity = std::time::Instant::now();
                tracing::info!(session_id, code, "acp agent process exited");
            }
            None => tracing::debug!(session_id, "spawn exit for unknown session"),
        }
    }

    /// AgentLlmProxyRequest：解析模型配置 → 服务端注入 key → 上游流式转发，
    /// 响应块经客户端控制通道回发。**必须**以 done=true chunk 收尾（契约）。
    async fn handle_llm_proxy_request(
        &self,
        client_name: &str,
        request_id: String,
        session_id: String,
        path: String,
        body: Vec<u8>,
    ) {
        // 响应回发给来源客户端（其控制通道）。
        let Some(control_tx) = self.spawner.client_control_sender(client_name).await else {
            tracing::warn!(client_name, %request_id, "llm proxy: client offline, dropping request");
            return;
        };
        // 会话必须已登记（ensure_session 已跑）。未登记（LLM 代理请求先于
        // ensure_session）时无法解析模型，按契约发 502 done chunk。
        if !self.sessions.lock().await.contains_key(&session_id) {
            let _ = control_tx
                .send(ControlMessage::AgentLlmProxyChunk {
                    request_id,
                    data: format!("agent session not ready: {session_id}").into_bytes(),
                    done: true,
                    status: 502,
                })
                .await;
            return;
        }
        let db = self.db.clone();
        let cipher = self.cipher.clone();
        tokio::spawn(async move {
            // forward 内部按 session → workspace.llm_model_id → model → provider
            // 解析，服务端注入 api key 后调上游。
            let stream = llm_bridge::forward(db, cipher, session_id, request_id.clone(), path, body);
            futures_util::pin_mut!(stream);
            while let Some(chunk) = stream.next().await {
                let msg = ControlMessage::AgentLlmProxyChunk {
                    request_id: chunk.request_id,
                    data: chunk.data,
                    done: chunk.done,
                    status: chunk.status,
                };
                if control_tx.send(msg).await.is_err() {
                    break; // 客户端断连：停止转发
                }
            }
        });
    }
}

/// stdio pump：进程 stdout（`stdout_rx`，即 `AgentSpawnData{stdin:false}` 转来）
/// → 写 duplex 喂 ACP crate；ACP crate 写出的字节从 duplex 读回 → 以
/// `AgentSpawnData{stdin:true}` 下发客户端进程 stdin。
///
/// 任一端关闭即整体退出：`stdout_rx` 通道关闭（进程 exit / 会话被移除）时先
/// 排空再退出；ACP 端 EOF（duplex 对端 drop）也退出。
async fn run_stdio_pump(
    pump_io: tokio::io::DuplexStream,
    mut stdout_rx: mpsc::Receiver<Vec<u8>>,
    control_tx: mpsc::Sender<ControlMessage>,
    session_id: String,
) {
    let (mut pump_rd, mut pump_wr) = tokio::io::split(pump_io);
    let mut buf = vec![0u8; 8192];
    loop {
        tokio::select! {
            data = stdout_rx.recv() => {
                match data {
                    // 进程 stdout → ACP。写失败 = ACP 端已关闭，收尾。
                    Some(data) => {
                        if pump_wr.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    // 通道关闭（exit/会话移除）：排空已完成，退出。
                    None => break,
                }
            }
            // ACP → 进程 stdin。
            n = pump_rd.read(&mut buf) => {
                match n {
                    Ok(0) => break, // ACP 连接已关闭
                    Ok(n) => {
                        let msg = ControlMessage::AgentSpawnData {
                            session_id: session_id.clone(),
                            data: buf[..n].to_vec(),
                            stdin: true,
                        };
                        if control_tx.send(msg).await.is_err() {
                            break; // 客户端断连
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    tracing::debug!(session_id, "acp stdio pump exited");
}

/// 从权限选项里挑指定 kind 的 option id（approve→AllowAlways/AllowOnce，
/// deny→RejectAlways/RejectOnce；优先 Always 对齐"记住本会话"语义）。
fn pick_option(
    options: &[PermissionOption],
    kind: PermissionOptionKind,
) -> Option<PermissionOptionId> {
    options
        .iter()
        .find(|o| o.kind == kind)
        .map(|o| o.option_id.clone())
}

/// 把 ACP 权限请求的 raw 输入编码成审批弹层的 args_preview 字符串。
/// 字符串直传；对象序列化为 JSON 文本（与 acp_events 的 encode_raw 同语义）。
fn acp_raw_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// 把 tokio 的 `AsyncRead`/`AsyncWrite` 适配成 futures 的——ACP crate 的
/// `ByteStreams` 走 futures-rs trait，而我们的 stdio duplex 是 tokio 类型。
/// 由 [`tokio::io::split`] 出的读写半各自包一层，保持 `Unpin`。
struct FuturesIo<T>(T);

impl<T: tokio::io::AsyncRead + Unpin> futures_util::io::AsyncRead for FuturesIo<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // tokio 1.x 的 poll_read 走 ReadBuf；futures 的直接给 &mut [u8]。
        let mut rb = tokio::io::ReadBuf::new(buf);
        match std::pin::Pin::new(&mut self.0).poll_read(cx, &mut rb) {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(rb.filled().len())),
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> futures_util::io::AsyncWrite for FuturesIo<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// 构造 workspace 记录（agent_type 已配置 + host 运行时）。
    fn acp_workspace() -> AgentWorkspaceRecord {
        AgentWorkspaceRecord {
            id: "w1".into(),
            name: "proj".into(),
            client_id: "nas".into(),
            runtime_type: "host".into(),
            root_path: "/workspace".into(),
            docker_image: None,
            docker_container_id: None,
            approval_mode: "safe".into(),
            system_prompt: None,
            agent_type: "gemini".into(),
            agent_path: None,
            llm_model_id: Some("model-1".into()),
            created_at: "t".into(),
            updated_at: "t".into(),
        }
    }

    fn docker_workspace() -> AgentWorkspaceRecord {
        let mut ws = acp_workspace();
        ws.runtime_type = "docker".into();
        ws
    }

    /// 测试用的会话条目默认值（connection/duplex 未建立）。
    fn spawned_agent() -> SpawnedAgent {
        SpawnedAgent {
            acp_session_id: None,
            connection: None,
            agent_io: None,
            stdout_tx: None,
            client_id: "nas".into(),
            ws_tx: None,
            busy: false,
            last_activity: std::time::Instant::now(),
            exited: false,
        }
    }

    /// 构造一个注册了模拟客户端 + 自动应答协商请求的 bridge。
    /// `respond` 闭包把收到的请求转成响应消息。
    async fn mock_bridge<F>(respond: F) -> AcpBridge
    where
        F: Fn(ControlMessage) -> ControlMessage + Send + 'static,
    {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let (tx, mut rx) = mpsc::channel(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let registry2 = registry.clone();
        tokio::spawn(async move {
            loop {
                let Some(req) = rx.recv().await else {
                    return;
                };
                let sid = match &req {
                    ControlMessage::AgentSpawnRequest { session_id, .. } => session_id.clone(),
                    ControlMessage::AgentLlmProxyStart { session_id } => session_id.clone(),
                    other => panic!("unexpected request: {other:?}"),
                };
                registry2.resolve_spawn_pending(&sid, respond(req)).await;
            }
        });
        AcpBridge::new(AgentSpawner::new(registry), db)
    }

    #[tokio::test]
    async fn test_ensure_session_docker_rejected() {
        let bridge = mock_bridge(|_| unreachable!("docker rejection should not spawn")).await;
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &docker_workspace(), ws_tx)
            .await
            .expect_err("docker workspace should be rejected");
        assert!(err.contains("docker"), "err: {err}");
    }

    #[tokio::test]
    async fn test_ensure_session_missing_model_id_rejected() {
        let bridge = mock_bridge(|_| unreachable!("missing model should not spawn")).await;
        let mut ws = acp_workspace();
        ws.llm_model_id = None;
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx)
            .await
            .expect_err("workspace without llm_model_id should be rejected");
        assert!(err.contains("llm_model_id"), "err: {err}");
    }

    #[tokio::test]
    async fn test_ensure_session_offline_client_fails() {
        // 客户端未注册 → start_llm_proxy 报 NotConnected，before spawn_agent
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut ws = acp_workspace();
        ws.client_id = "ghost".into();
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx)
            .await
            .expect_err("offline client should fail");
        assert!(err.contains("llm proxy start failed"), "err: {err}");
    }

    #[tokio::test]
    async fn test_ensure_session_unsupported_agent_type() {
        // 模拟客户端应答 LLM 代理端口；agent_type 非法在本地报错（spawn 前）
        let bridge = mock_bridge(|req| match req {
            ControlMessage::AgentLlmProxyStart { session_id } => {
                ControlMessage::AgentLlmProxyReady {
                    session_id,
                    port: 45678,
                }
            }
            other => panic!("unexpected request: {other:?}"),
        })
        .await;
        let mut ws = acp_workspace();
        ws.agent_type = "cursor".into();
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx)
            .await
            .expect_err("unsupported agent type should fail locally");
        assert!(err.contains("unsupported agent type"), "err: {err}");
    }

    #[tokio::test]
    async fn test_ensure_session_dedup_skips_respawn() {
        // 会话已登记 → ensure_session 直接 Ok，不发起任何 spawn
        let bridge = mock_bridge(|_| unreachable!("dedup should not spawn")).await;
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), spawned_agent());
        let (ws_tx, _rx) = mpsc::channel(16);
        bridge
            .ensure_session("sess-1", &acp_workspace(), ws_tx)
            .await
            .expect("dedup should return Ok");
    }

    #[tokio::test]
    async fn test_ensure_session_exited_entry_respawns() {
        // 回归：exited=true 的陈旧条目不能短路 dedup。用离线客户端让 spawn
        // 路径快速失败——若 ensure_session 直接 Ok（bug 行为）则 expect_err panic。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut exited = spawned_agent();
        exited.exited = true;
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), exited);
        let mut ws = acp_workspace();
        ws.client_id = "ghost".into();
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx)
            .await
            .expect_err("exited entry must attempt respawn, not short-circuit Ok");
        assert!(err.contains("llm proxy start failed"), "err: {err}");
        // 陈旧条目已被移除（spawn 失败后不留占位），再次 ensure_session 仍可重试
        assert!(
            !bridge.sessions.lock().await.contains_key("sess-1"),
            "stale exited entry should be gone"
        );
    }

    #[tokio::test]
    async fn test_prompt_unknown_session_errors() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let err = bridge
            .prompt("no-such-session", "hi")
            .await
            .expect_err("unknown session should error");
        assert!(err.contains("not spawned"), "err: {err}");
    }

    #[tokio::test]
    async fn test_prompt_after_exit_errors() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut exited = spawned_agent();
        exited.exited = true;
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), exited);
        let err = bridge
            .prompt("sess-1", "hi")
            .await
            .expect_err("exited session should error");
        assert!(err.contains("exited"), "err: {err}");
    }

    #[tokio::test]
    async fn test_handle_spawn_exit_marks_exited() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), spawned_agent());
        bridge
            .handle_client_msg(
                "nas",
                ControlMessage::AgentSpawnExit {
                    session_id: "sess-1".into(),
                    code: Some(1),
                },
            )
            .await;
        assert!(
            bridge.sessions.lock().await.get("sess-1").unwrap().exited,
            "exit should be recorded"
        );
    }

    #[tokio::test]
    async fn test_handle_llm_proxy_request_unregistered_session_502() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let (tx, mut rx) = mpsc::channel(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        bridge
            .handle_client_msg(
                "nas",
                ControlMessage::AgentLlmProxyRequest {
                    request_id: "req-1".into(),
                    session_id: "sess-unknown".into(),
                    path: "/v1/chat/completions".into(),
                    body: br#"{"stream":true}"#.to_vec(),
                },
            )
            .await;
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for chunk")
            .expect("channel closed");
        match msg {
            ControlMessage::AgentLlmProxyChunk {
                request_id,
                done: true,
                status: 502,
                ..
            } => assert_eq!(request_id, "req-1"),
            other => panic!("expected 502 done chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_llm_proxy_request_offline_client_dropped() {
        // 来源客户端离线：无法回发，请求被丢弃（不 panic、不 hang）
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge
            .handle_client_msg(
                "ghost",
                ControlMessage::AgentLlmProxyRequest {
                    request_id: "req-1".into(),
                    session_id: "sess-1".into(),
                    path: "/v1/chat/completions".into(),
                    body: br#"{"stream":true}"#.to_vec(),
                },
            )
            .await;
    }

    // ── stdio pump ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_stdio_pump_process_stdout_to_agent() {
        // 进程 stdout（stdout_tx）→ pump → duplex → ACP 端可读
        let (mut agent_io, pump_io) = tokio::io::duplex(64 * 1024);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(16);
        let (control_tx, _control_rx) = mpsc::channel::<ControlMessage>(16);
        tokio::spawn(run_stdio_pump(pump_io, stdout_rx, control_tx, "sess-1".into()));

        stdout_tx.send(b"hello".to_vec()).await.unwrap();
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), agent_io.read(&mut buf))
            .await
            .expect("timed out reading agent_io")
            .expect("read failed");
        assert_eq!(&buf[..n], b"hello");
    }

    #[tokio::test]
    async fn test_stdio_pump_agent_to_process_stdin() {
        // ACP 端写入（模拟 ACP crate 输出到进程 stdin）→ AgentSpawnData(stdin=true)
        let (mut agent_io, pump_io) = tokio::io::duplex(64 * 1024);
        let (_stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(16);
        let (control_tx, mut control_rx) = mpsc::channel::<ControlMessage>(16);
        tokio::spawn(run_stdio_pump(pump_io, stdout_rx, control_tx, "sess-1".into()));

        agent_io.write_all(b"world").await.unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), control_rx.recv())
            .await
            .expect("timed out waiting for stdin data")
            .expect("channel closed");
        match msg {
            ControlMessage::AgentSpawnData {
                session_id,
                data,
                stdin: true,
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(data, b"world");
            }
            other => panic!("expected AgentSpawnData(stdin=true), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_stdio_pump_exits_when_stdout_sender_dropped() {
        // 进程退出/会话移除 → stdout_tx drop → pump 收尾（排空后退出，不泄漏）
        let (mut agent_io, pump_io) = tokio::io::duplex(64 * 1024);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(16);
        let (control_tx, _control_rx) = mpsc::channel::<ControlMessage>(16);
        let task = tokio::spawn(run_stdio_pump(pump_io, stdout_rx, control_tx, "sess-1".into()));
        // 先投递一条残余字节再 drop 发送端：pump 应转发后再退出（不丢数据）
        stdout_tx.send(b"tail".to_vec()).await.unwrap();
        drop(stdout_tx);
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), agent_io.read(&mut buf))
            .await
            .expect("timed out reading agent_io")
            .expect("read failed");
        assert_eq!(&buf[..n], b"tail");
        // pump 任务应自行结束
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("pump task did not exit")
            .expect("pump task panicked");
    }

    // ── busy 守卫 ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_prompt_busy_guard_rejects_concurrent() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut agent = spawned_agent();
        agent.busy = true;
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), agent);
        let err = bridge
            .prompt("sess-1", "hi")
            .await
            .expect_err("busy turn should reject a concurrent prompt");
        assert!(err.contains("回合"), "err: {err}");
    }

    #[tokio::test]
    async fn test_prompt_without_connection_errors() {
        // busy=false 但 handshake 未完成（connection 缺失）→ 报错且 busy 复位
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), spawned_agent());
        let err = bridge
            .prompt("sess-1", "hi")
            .await
            .expect_err("no connection yet should error");
        assert!(err.contains("handshake"), "err: {err}");
        // busy 已复位，不会卡死后续 prompt
        assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
    }

    #[tokio::test]
    async fn test_cancel_unknown_session_is_noop() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge.cancel("no-such-session").await; // 不 panic
    }

    #[tokio::test]
    async fn test_kill_removes_entry() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), spawned_agent());
        bridge.kill("sess-1").await;
        assert!(!bridge.sessions.lock().await.contains_key("sess-1"));
        bridge.kill("sess-1").await; // 幂等：不存在时静默 no-op
    }

    // ── ACP handshake + prompt 集成（mock JSON-RPC agent）────────

    /// 模拟 ACP agent：newline-delimited JSON-RPC。从 `stdin_rx`（pump 的
    /// AgentSpawnData{stdin:true} 转来）读请求行，把响应/通知写到 `stdout_tx`
    /// （→ pump → ACP crate）。
    async fn mock_acp_agent(
        mut stdin_rx: mpsc::Receiver<ControlMessage>,
        stdout_tx: mpsc::Sender<Vec<u8>>,
    ) {
        let mut buf = String::new();
        while let Some(msg) = stdin_rx.recv().await {
            let ControlMessage::AgentSpawnData { data, stdin: true, .. } = msg else {
                continue;
            };
            buf.push_str(&String::from_utf8_lossy(&data));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let method = json
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = json.get("id").cloned().unwrap_or(serde_json::Value::Null);
                // 单条请求可能产出多条输出行（prompt：通知 + 响应）。
                let mut out_lines: Vec<serde_json::Value> = Vec::new();
                match method.as_str() {
                    "initialize" => {
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "protocolVersion": 1 }
                        }));
                    }
                    "session/new" => {
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "sessionId": "acp-1" }
                        }));
                    }
                    "session/prompt" => {
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": "hello from mock" } } }
                        }));
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "tool_call", "toolCallId": "call_1",
                                    "title": "shell", "status": "completed",
                                    "rawInput": { "cmd": "ls" } } }
                        }));
                        // ToolCallUpdate 不带 title：验证 name 从前序 ToolCall 缓存补
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "tool_call_update",
                                    "toolCallId": "call_1", "status": "completed",
                                    "rawOutput": "a.rs" } }
                        }));
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "stopReason": "end_turn" }
                        }));
                    }
                    other => {
                        tracing::debug!("mock agent: unknown method {other}");
                        continue;
                    }
                }
                for line_value in out_lines {
                    let mut bytes = serde_json::to_vec(&line_value).unwrap();
                    bytes.push(b'\n');
                    if stdout_tx.send(bytes).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn test_acp_handshake_prompt_streams_events() {
        // 端到端：duplex → pump → mock agent（JSON-RPC）→ 事件流经 ws_tx。
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(16);
        let (agent_io, pump_io) = tokio::io::duplex(64 * 1024);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
        let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(32);

        // 注册会话条目（含 duplex ACP 端 + pump 通道）
        let mut agent = spawned_agent();
        agent.agent_io = Some(agent_io);
        agent.stdout_tx = Some(stdout_tx.clone());
        agent.ws_tx = Some(ws_tx.clone());
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), agent);

        // pump + mock agent
        tokio::spawn(run_stdio_pump(
            pump_io,
            stdout_rx,
            control_tx,
            "sess-1".into(),
        ));
        tokio::spawn(mock_acp_agent(control_rx, stdout_tx));

        // ACP handshake：initialize → session/new
        bridge
            .acp_handshake("sess-1", &ws_tx)
            .await
            .expect("handshake should complete");
        // 写回连接句柄与 ACP session id
        {
            let s = bridge.sessions.lock().await;
            let agent = s.get("sess-1").unwrap();
            assert!(agent.connection.is_some(), "connection should be stored");
            assert_eq!(
                agent.acp_session_id.as_ref().unwrap().0.as_ref(),
                "acp-1"
            );
        }

        // prompt：异步回合，事件流经 ws_tx
        bridge
            .prompt("sess-1", "hello")
            .await
            .expect("prompt should send");

        // 事件序列：assistant_chunk → tool_call → tool_result(名从缓存补) → done
        let mut events = Vec::new();
        for _ in 0..4 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv())
                .await
                .expect("timed out waiting for ws event")
                .expect("ws channel closed");
            events.push(ev);
        }
        assert_eq!(events[0]["type"], "assistant_chunk");
        assert_eq!(events[0]["content"], "hello from mock");
        assert_eq!(events[1]["type"], "tool_call");
        assert_eq!(events[1]["name"], "shell");
        assert_eq!(events[2]["type"], "tool_result");
        assert_eq!(events[2]["name"], "shell", "name should be cached from ToolCall");
        assert_eq!(events[2]["result"], "a.rs");
        assert_eq!(events[3]["type"], "done");
        // 回合结束：busy 复位，可再次 prompt
        assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
    }
}
