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
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigOptionValue, SessionConfigValueId, SessionNotification,
    SetSessionConfigOptionRequest, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{ByteStreams, Client, ConnectionTo};

use rust_tunnel_common::ControlMessage;
use crate::server::db::Database;
use crate::server::llm::crypto::LlmCipher;
use crate::server::persistence::db::agent::AgentWorkspaceRecord;

use super::acp_events::map_update;
use super::llm_bridge::{self, LlmGatewayEndpoint};
use super::spawner::AgentSpawner;

/// spawn/协商超时：LLM 代理启动与 agent 进程拉起各限 30s。
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);
/// config option 切换超时：agent 无响应时让 WS 连接及时拿到 error 帧回滚，
/// 而非无限阻塞（回放挂起同样受此约束）。
const CONFIG_OPTION_TIMEOUT: Duration = Duration::from_secs(15);
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
    /// 连接任务的通知/请求处理器**每次事件**都经 [`current_ws_tx`] 动态读此
    /// 字段，重连（dedup 刷新）后流式帧自动切到新连接。
    ws_tx: Option<mpsc::Sender<serde_json::Value>>,
    /// 回合进行中标记：prompt 置位、PromptResponse 到达/cancel 清位。防并发
    /// prompt（ACP 单连接不支持并发回合；WS session_lock 只串行化分派，
    /// 不跨异步回合）。
    busy: bool,
    /// 已被取消的回合代数集合：`cancel` 时记录当前回合代数。终态回调只对
    /// 与自身代数匹配的取消做抑制（避免跨回合误吞 done/误发 error）。
    /// 不用 exited 判定——进程自行崩溃（非用户取消）时 exited 也会置位，
    /// 此时仍须把错误上报前端。
    cancelled_turns: std::collections::HashSet<u64>,
    /// 回合代数计数器：每次 prompt 递增，与 cancelled_turns 配合区分
    /// "哪个回合被取消"。
    turn_generation: u64,
    /// 最近活动时间（prompt / cancel / stdio / ACP 通知都会刷新；idle reaper 依据）。
    last_activity: std::time::Instant,
    /// AgentSpawnExit 已到达（进程结束）。
    exited: bool,
    /// 回合内 assistant 文本缓冲（chunk 逐条到达，终态合并落一行库）。
    text_buf: String,
    /// 回合内 thought 文本缓冲（终态落 `name='thought'` 行）。
    thought_buf: String,
    /// ACP 会话配置选项快照（handshake 捕获 + config_option_update 全量替换）。
    /// 空 Vec 且 handshake 未完成 = 尚无状态；agent 不上报时保持空。
    config_options: Vec<SessionConfigOption>,
    /// 握手完成信号（false → true）。连接预 spawn（后台任务）仍在握手时，
    /// 首条 user_message 的 `wait_ready` 经 `subscribe` + `wait_for` 等待它，
    /// 避免 `prompt` 报 "ACP handshake not complete"。条目被移除（spawn 失败/
    /// kill）时 Sender drop，wait_for 以 RecvError 返回。
    spawn_ready: watch::Sender<bool>,
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
    /// LLM 网关入口（内部 HTTP 回环调用）；未注入时 LLM 代理请求全部 502。
    gateway: Option<LlmGatewayEndpoint>,
}

impl AcpBridge {
    pub fn new(spawner: AgentSpawner, db: Database) -> Self {
        let bridge = Self {
            spawner,
            db,
            cipher: None,
            approval: Arc::new(|_, _, _, _, _| Box::pin(async { false })),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            gateway: None,
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

    /// 注入 LLM 网关入口（内部 HTTP 回环地址 + API key + 双协议域名）。
    #[must_use]
    pub fn with_llm_gateway(mut self, gateway: LlmGatewayEndpoint) -> Self {
        self.gateway = Some(gateway);
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
                    cancelled_turns: std::collections::HashSet::new(),
                    turn_generation: 0,
                    last_activity: std::time::Instant::now(),
                    exited: false,
                    text_buf: String::new(),
                    thought_buf: String::new(),
                    config_options: Vec::new(),
                    spawn_ready: watch::channel(false).0,
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
            // 0) 模型配置门禁：session.model / workspace.llm_model_id / 全局默认
            //    任一即可。实际 LLM 请求按 session 从 DB 解析（resolve_effective_model，
            //    含「第一个可用」兜底），此处只防 spawn 后才发现无模型。校验失败走
            //    通用错误路径（outcome Err → 占位被移除，允许重试）。
            if !super::session::has_any_model_config(
                &self.db,
                session_id,
                workspace.llm_model_id.as_deref(),
            )
            .await?
            {
                return Err("workspace 与 session 均未配置 LLM 模型".into());
            }
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
            // 3) ACP handshake（stdio pump 已就绪，此步建立 ACP 连接 + WS 接线；
            // ws_tx 由连接任务的处理器每次事件从会话条目动态解析，无需传入）
            let root_path = workspace.root_path.clone();
            self.acp_handshake(session_id, &root_path).await
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
    /// [`map_update`] 映射后推会话条目当前的 ws_tx——处理器每次事件动态解析，
    /// 重连自动切到新连接；权限请求（`session/request_permission`）走审批回调。
    ///
    /// 注意：`agent_client_protocol::Client` 是角色标记（unit struct），并非
    /// 连接句柄；连接句柄是 `ConnectionTo<Agent>`。每 session 一条专用连接，
    /// 通知无需按 session id 过滤。
    async fn acp_handshake(&self, session_id: &str, cwd: &str) -> Result<(), String> {
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
        let cwd = cwd.to_string();
        let approval = self.approval.clone();
        let sessions = self.sessions.clone();
        let db = self.db.clone();
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
                        let sessions = sessions.clone();
                        let db = db.clone();
                        // tool_call_id → 工具名 缓存：ACP 的 ToolCallUpdate 常不带
                        // title，而前端 ChatStream 按 tool_result.name === tool_call.name
                        // 匹配卡片——从前序 ToolCall 事件的 title 补名，保证结果能挂上。
                        let mut tool_names: HashMap<String, String> = HashMap::new();
                        // tool_call_id → 已落库 args 缓存：claude-code 的 ToolCall 首帧
                        // rawInput 常是 {}（参数尚未到达），真正的命令/路径经后续
                        // ToolCallUpdate.rawInput 到达。若仅推送 WS 而不回填 DB，
                        // 重载后 tool_result 卡片从 tool_calls 行取到的仍是空 args
                        // （历史卡片无操作内容）。缓存已落值，后续含新 rawInput 的帧
                        // UPDATE 原 tool_calls 行补齐。
                        let mut tool_args: HashMap<String, String> = HashMap::new();
                        async move |notification: SessionNotification, _cx| {
                            // 专用连接：所有通知都属于本 session。tool_call 名缓存
                            // 先填（会话 detached 期间也可累积，重连后 tool_result
                            // 仍能补名）。
                            if let agent_client_protocol::schema::v1::SessionUpdate::ToolCall(tc) =
                                &notification.update
                            {
                                tool_names.insert(
                                    tc.tool_call_id.to_string(),
                                    tc.title.clone(),
                                );
                            }
                            // 会话存活守卫（评审修复：persist 移出 ws_tx guard）：
                            // touch_activity 只刷新 last_activity、不读 ws_tx——断线
                            // （detach_ws_tx 置 None）期间到达的事件必须继续落库，
                            // 后台跑完的回合同样可追溯。条目被 kill/reaper 回收后
                            // 返回 false，本帧放弃（不落库、不推送）。
                            if !touch_activity(&sessions, &sid).await {
                                return Ok(());
                            }
                            // 状态快照维护：config_option_update 全量替换；
                            // current_mode_update 只改写 mode 项的 current_value
                            // （claude-code-acp 改 mode 时两种通知都会发，顺序不定，
                            // 两处幂等保证最终一致）。只维护内存快照——帧推送已由
                            // map_update 完成（Task 2），这里不重复推。
                            match &notification.update {
                                agent_client_protocol::schema::v1::SessionUpdate::ConfigOptionUpdate(
                                    upd,
                                ) => {
                                    if let Some(a) = sessions.lock().await.get_mut(&sid) {
                                        a.config_options = upd.config_options.clone();
                                    }
                                }
                                agent_client_protocol::schema::v1::SessionUpdate::CurrentModeUpdate(
                                    mode,
                                ) => {
                                    if let Some(a) = sessions.lock().await.get_mut(&sid) {
                                        for o in &mut a.config_options {
                                            if matches!(
                                                &o.category,
                                                Some(SessionConfigOptionCategory::Mode)
                                            ) {
                                                if let SessionConfigKind::Select(sel) =
                                                    &mut o.kind
                                                {
                                                    sel.current_value = SessionConfigValueId::new(
                                                        mode.current_mode_id.0.clone(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                            if let Some(mut frame) = map_update(&notification.update) {
                                // owned 拷贝：后面要对 frame 赋值，不能持 &str 借用
                                let frame_id = frame
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string);
                                // ToolCallUpdate 缺 title 时从缓存补 name；缺 args 时
                                // 从已落库值补（最终 rawOutput 帧常只带结果不带参数，
                                // 卡片展开详情需要 args）。
                                if frame["type"] == "tool_result" {
                                    if let Some(id) = &frame_id {
                                        if frame.get("name").is_none() {
                                            if let Some(name) = tool_names.get(id) {
                                                frame["name"] =
                                                    serde_json::Value::String(name.clone());
                                            }
                                        }
                                        if frame.get("args").is_none() {
                                            if let Some(args) = tool_args.get(id) {
                                                frame["args"] =
                                                    serde_json::Value::String(args.clone());
                                            }
                                        }
                                    }
                                }
                                // 落库（best-effort，不依赖 WS 存活）：tool/plan 直接
                                // 落；文本/thought 缓冲到终态合并落一行。断线期间
                                // 到达的帧同样落库——落库在推送之前、与推送解耦。
                                persist_acp_frame(&db, &sessions, &sid, &frame).await;
                                // args 回填：tool_call 帧登记已落库 args；tool_result 帧
                                // 携带了新 rawInput（与已落库值不同）时 UPDATE 原
                                // tool_calls 行补齐——重载后历史卡片才能看到操作内容。
                                match (frame["type"].as_str().unwrap_or(""), frame_id) {
                                    ("tool_call", Some(id)) => {
                                        if let Some(a) =
                                            frame.get("args").and_then(|v| v.as_str())
                                        {
                                            tool_args.insert(id.to_string(), a.to_string());
                                        }
                                    }
                                    ("tool_result", Some(id)) => {
                                        if let Some(a) =
                                            frame.get("args").and_then(|v| v.as_str())
                                        {
                                            // 空对象占位（"{}"）不算新信息：ToolCall 首帧
                                            // 的 rawInput={} 不能覆盖此前已回填的真参数。
                                            let meaningful = a.trim() != "{}";
                                            let persisted =
                                                tool_args.get(&id).map(String::as_str);
                                            if meaningful && persisted != Some(a) {
                                                if let Err(e) = db
                                                    .agent_update_tool_call_args(&id, a)
                                                    .await
                                                {
                                                    tracing::warn!(session_id = %sid,
                                                        "backfill tool_call args failed: {e}");
                                                }
                                                tool_args.insert(id.clone(), a.to_string());
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                                // 推送：每次事件动态解析当前 WS 通道（评审 Finding 1）——
                                // handshake 时捕获的 ws_tx 在重连后会过时，ensure_session
                                // 的 dedup 刷新与 detach_ws_tx 清空只改条目里的 ws_tx，
                                // 必须读最新值，否则流式帧会推给已断开的旧连接（try_send
                                // 静默失败，新连接只能看到 done）。断线（None）则跳过推送，
                                // 不影响已完成的落库。try_send 丢帧（前端消费跟不上）是
                                // 实时流可容忍的，避免阻塞卡死整个 ACP 连接。
                                if let Some(ws_tx) = current_ws_tx(&sessions, &sid).await {
                                    if ws_tx.try_send(frame).is_err() {
                                        tracing::trace!(session_id = %sid, "acp event dropped (ws channel full/closed)");
                                    }
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
                        let approval = approval.clone();
                        let sessions = sessions.clone();
                        async move |request: RequestPermissionRequest, responder, _cx| {
                            // 动态解析当前 WS 通道（同 notification，评审 Finding 1）：
                            // 审批弹层要推给最新连接，而非 handshake 时捕获的旧通道。
                            let ws_tx = match current_ws_tx(&sessions, &sid).await {
                                Some(tx) => tx,
                                // 会话已回收 / WS 已断开：构造一个立即失效的通道传给
                                // 审批回调，request_approval 发帧失败即按拒绝短路返回
                                // （评审 Finding 2：避免 5 分钟超时占用连接任务，阻塞
                                // agent 下一个工具调用）。
                                None => {
                                    let (tx, _rx) = mpsc::channel::<serde_json::Value>(1);
                                    tx
                                }
                            };
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
                                ws_tx,
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
                                .send_request(NewSessionRequest::new(&cwd))
                                .block_task()
                                .await?;
                            let acp_session_id = new_session.session_id.clone();
                            let config_options =
                                new_session.config_options.clone().unwrap_or_default();
                            // 写回会话条目：连接句柄 + ACP session id 供 prompt/cancel；
                            // config_options 捕获后供 set_config_option / session_state 帧。
                            {
                                let mut map = sessions.lock().await;
                                if let Some(agent) = map.get_mut(&sid) {
                                    agent.connection = Some(cx.clone());
                                    agent.acp_session_id = Some(acp_session_id);
                                    agent.config_options = config_options;
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
            Ok(Ok(Ok(()))) => {
                self.replay_config_state(session_id).await;
                // 握手完成：通知 wait_ready 等待方（连接预 spawn 期间首条消息）。
                // 置于 session_state 推送前——connection 在 setup 闭包里已先于
                // setup_rx Ok 写入，wait_ready 的「connection.is_some()」快路径
                // 与订阅的 watch 双保险，不会错过就绪。
                if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
                    let _ = a.spawn_ready.send(true);
                }
                // 回放完成后把最终快照推给当前 WS 连接（重连场景前端立即可见）。
                if let Some(frame) = self.session_state_frame(session_id).await {
                    if let Some(ws_tx) = current_ws_tx(&self.sessions, session_id).await {
                        let _ = ws_tx.try_send(frame);
                    }
                }
                Ok(())
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err("acp connection task dropped".into()),
            Err(_) => Err("acp handshake timed out".into()),
        }
    }

    /// 向 ACP 会话发送一条 prompt（fire-and-forget）。
    ///
    /// 发送 `session/prompt` 后立即返回；回合内的 `session/update` 通知经
    /// [`map_update`] 推送会话条目当前的 ws_tx，`PromptResponse` 到达时回调发
    /// `{"type":"done"}` 帧。回合进行中重复 prompt 报错（`busy` 守卫；ACP 单连接
    /// 不支持并发回合）。取消/杀进程后的终态帧被抑制（stopped 帧已由 WS handler
    /// 回发，再补 error/done 会造成误导）。
    pub async fn prompt(&self, session_id: &str, content: &str) -> Result<(), String> {
        let (connection, acp_session_id, turn_gen) = {
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
            agent.busy = true;
            agent.last_activity = std::time::Instant::now();
            // 为本回合分配递增代数：cancel 时记录，终态回调据此判断是否抑制。
            // 解决单布尔跨回合共享导致 cancel 后立即重发 prompt 时误吞 done/
            // 误发 error 的竞态（cancelled 布尔无法区分"哪个回合被取消"）。
            agent.turn_generation += 1;
            let turn_gen = agent.turn_generation;
            (connection, acp_session_id, turn_gen)
        };

        let sessions = self.sessions.clone();
        let db = self.db.clone();
        let sid = session_id.to_string();
        let prompt = vec![ContentBlock::Text(TextContent::new(content.to_string()))];
        let send_result = connection
            .send_request_to(
                agent_client_protocol::Agent,
                PromptRequest::new(acp_session_id, prompt),
            )
            .on_receiving_result(async move |result| {
                // 终态落库先行：缓冲文本/thought 合并落库必须在 done 帧之前完成，
                // 否则前端 done 后 invalidate 的历史 refetch 可能读不到本回合并本。
                flush_acp_turn_buffers(&db, &sessions, &sid).await;
                // 回合终态：清 busy + 取当前 WS 通道（重连后 done/error 推给最新
                // 连接，与通知处理器同语义）。若本回合被取消（cancelled_turns 含
                // 本代数）或会话已被杀（条目移除），抑制终态帧——取消路径的
                // stopped 帧已由 WS handler 回发，再补 error/done 会造成误导。
                // 注意抑制条件按代数匹配而非全局布尔：cancel 后立即重发 prompt
                // 时，新回合的终态回调不会被旧回合的取消标记误吞（评审 Finding）。
                let ws_tx = {
                    let mut map = sessions.lock().await;
                    match map.get_mut(&sid) {
                        Some(a) => {
                            a.busy = false;
                            if a.cancelled_turns.remove(&turn_gen) {
                                return Ok(()); // 本回合被取消：抑制终态帧
                            }
                            a.ws_tx.clone()
                        }
                        None => return Ok(()), // 会话已 kill/回收：不再发终态帧
                    }
                };
                let Some(ws_tx) = ws_tx else {
                    return Ok(()); // 前端已断开：无消费端，终态帧不发
                };
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
                    // 记录当前回合代数为已取消：杀进程后 on_receiving_result 会
                    // 以 Err 到达，终态回调据此抑制 error 帧（stopped 帧已由 WS
                    // handler 回发）。用代数而非布尔：cancel 后立即重发 prompt
                    // 时，新回合分配新代数，不会被本条取消标记误伤。
                    agent.cancelled_turns.insert(agent.turn_generation);
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

    /// 构造全量 session_state 帧；无状态（未握手/agent 不上报）返回 None。
    async fn session_state_frame(&self, session_id: &str) -> Option<serde_json::Value> {
        let sessions = self.sessions.lock().await;
        let agent = sessions.get(session_id)?;
        if agent.acp_session_id.is_none() || agent.config_options.is_empty() {
            return None;
        }
        Some(serde_json::json!({
            "type": "session_state",
            "options": agent.config_options,
        }))
    }

    /// 握手成功后回放 DB 中持久化的配置（mode 优先：agent 侧 model 切换会
    /// 重建 effort 列表，mode 先行保证其余项在最终列表上生效）。单条失败
    /// （如新版 agent 移除某取值）跳过并 warn，不阻断其余。
    async fn replay_config_state(&self, session_id: &str) {
        let saved = match self.db.agent_get_session(session_id).await {
            Ok(Some(record)) => record.config_state,
            _ => None,
        };
        let Some(saved) = saved else { return };
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&saved)
        else {
            return;
        };
        let mut entries: Vec<(String, String)> = map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect();
        entries.sort_by_key(|(k, _)| if k == "mode" { 0 } else { 1 });
        for (config_id, value) in entries {
            if let Err(e) = self.set_config_option(session_id, &config_id, &value).await {
                tracing::warn!(session_id, config_id, "replay config_state skipped: {e}");
            }
        }
    }

    /// 切换 ACP 会话配置项：校验 config_id 在当前 options 中 → 发
    /// `session/set_config_option`。value 对 select 是 value-id 字符串，
    /// 对 boolean 是 "true"/"false"。成功后的状态更新以 agent 回推的
    /// config_option_update 为准（通知处理器全量替换快照）。
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<(), String> {
        let (connection, acp_session_id, is_boolean) = {
            let mut sessions = self.sessions.lock().await;
            let agent = sessions
                .get_mut(session_id)
                .ok_or_else(|| "session not spawned".to_string())?;
            if agent.exited {
                return Err("agent process has exited".into());
            }
            let option = agent
                .config_options
                .iter()
                .find(|o| o.id.0.as_ref() == config_id)
                .ok_or_else(|| format!("unknown config option: {config_id}"))?;
            let is_boolean = matches!(&option.kind, SessionConfigKind::Boolean(_));
            let connection = agent
                .connection
                .clone()
                .ok_or_else(|| "ACP handshake not complete".to_string())?;
            let acp_session_id = agent
                .acp_session_id
                .clone()
                .ok_or_else(|| "ACP handshake not complete".to_string())?;
            agent.last_activity = std::time::Instant::now();
            (connection, acp_session_id, is_boolean)
        };
        let typed_value = if is_boolean {
            SessionConfigOptionValue::boolean(value == "true")
        } else {
            // schema 的 id 新类型只派生了 From<&'static str>；非静态 &str 经
            // SessionConfigValueId::new（内部 Into<Arc<str>> 走 std From<&str>）。
            SessionConfigOptionValue::value_id(SessionConfigValueId::new(value))
        };
        match tokio::time::timeout(
            CONFIG_OPTION_TIMEOUT,
            connection
                .send_request_to(
                    agent_client_protocol::Agent,
                    SetSessionConfigOptionRequest::new(
                        acp_session_id,
                        SessionConfigId::new(config_id),
                        typed_value,
                    ),
                )
                .block_task(),
        )
        .await
        {
            Err(_) => Err(format!("set_config_option timed out: {config_id}")),
            Ok(inner) => {
                inner.map_err(|e| format!("set_config_option failed: {e}"))?;
                Ok(())
            }
        }
    }

    /// 当前会话的配置快照（WS 连接建立后主动推送用）；未就绪返回 None。
    pub async fn session_config_options(
        &self,
        session_id: &str,
    ) -> Option<Vec<SessionConfigOption>> {
        let sessions = self.sessions.lock().await;
        let agent = sessions.get(session_id)?;
        if agent.acp_session_id.is_none() || agent.config_options.is_empty() {
            return None;
        }
        Some(agent.config_options.clone())
    }

    /// 会话是否已在 ACP 桥登记且进程存活（预 spawn 成功后、首条消息前，
    /// `set_config_option` 等帧也要能分派）。exited 的陈旧条目视同未就绪，
    /// 让调用方走错误路径而非静默丢弃。
    pub async fn session_spawned(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .is_some_and(|a| !a.exited)
    }

    /// 等待会话的 ACP 握手完成（连接预 spawn 可能在后台进行）。已就绪立即
    /// 返回；超时或会话被移除（spawn 失败/Sender drop）返回 Err。
    pub async fn wait_ready(&self, session_id: &str) -> Result<(), String> {
        let mut rx = {
            let sessions = self.sessions.lock().await;
            let agent = sessions
                .get(session_id)
                .ok_or_else(|| "session not spawned".to_string())?;
            // connection 已写入 = 握手完成，快路径直接返回
            if agent.connection.is_some() {
                return Ok(());
            }
            agent.spawn_ready.subscribe()
        };
        // 订阅后才检查当前值：避免「subscribe 前已 send(true)」的窗口漏等。
        // spawn_ready 只在 connection 写入后变 true，此分支实际不可达，作双保险。
        if *rx.borrow() {
            return Ok(());
        }
        // wait_for 的返回值借用 rx（Ref<bool>）：先绑局部变量强制在 rx drop 前
        // 释放该借用，避免尾表达式临时值拖到块结束才 drop（E0597）。
        let ready = tokio::time::timeout(SPAWN_TIMEOUT, rx.wait_for(|r| *r)).await;
        match ready {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err("session not spawned".to_string()),
            Err(_) => Err("wait for ACP handshake timed out".to_string()),
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

    /// AgentLlmProxyRequest：经内部 HTTP 回环调 LLM 网关入口（`/v1/messages`
    /// 或 `/v1/chat/completions`），网关自动完成模型组故障转移、格式转换、
    /// 用量统计等全管线。响应块经客户端控制通道流式回发。
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
        // 会话必须已登记（ensure_session 已跑）。未登记时无法解析模型，按契约发 502 done chunk。
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
        // 网关未注入（生产启动应在 init_llm_state 后注入）：全部 502。
        let Some(gateway) = self.gateway.clone() else {
            let _ = control_tx
                .send(ControlMessage::AgentLlmProxyChunk {
                    request_id,
                    data: b"llm gateway not configured (missing inject after init)".to_vec(),
                    done: true,
                    status: 502,
                })
                .await;
            return;
        };
        let db = self.db.clone();
        tokio::spawn(async move {
            let stream = llm_bridge::forward(db, session_id, request_id.clone(), gateway, path, body);
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

/// 动态解析会话当前的 WS 事件通道：重连/多标签页时 `ensure_session` 的 dedup
/// 刷新、连接关闭时 [`AcpBridge::detach_ws_tx`] 清空，都会改动条目里的
/// `ws_tx`。连接任务的通知/请求处理器**每次事件**都读最新值，避免流式帧/审批
/// 弹层推给已断开的旧连接（旧 sender 的 try_send 会静默失败，前端只看到 done）。
/// 顺带刷新 last_activity——长回合无 stdout 时不被 idle reaper 误回收。
/// 会话不存在返回 None。
async fn current_ws_tx(
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) -> Option<mpsc::Sender<serde_json::Value>> {
    let mut map = sessions.lock().await;
    let agent = map.get_mut(sid)?;
    agent.last_activity = std::time::Instant::now();
    agent.ws_tx.clone()
}

/// 刷新会话活动时间并返回条目是否存在。与 [`current_ws_tx`] 的锁内刷新
/// 语义一致，但不读 `ws_tx`——通知处理器用它对会话做存活守卫：条目在
/// （即使断线 `ws_tx=None`）就继续落库，条目被 kill/reaper 回收后返回
/// false 放弃本帧（落库与推送都依赖会话条目，回收后两者都无意义）。
/// 断线期间不依赖 WS 通道存活即落库，是「断线期间后台跑完的回合同样可
/// 追溯」的前提（评审修复：persist 移出 ws_tx guard 之前）。
async fn touch_activity(
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) -> bool {
    let mut map = sessions.lock().await;
    match map.get_mut(sid) {
        Some(a) => {
            a.last_activity = std::time::Instant::now();
            true
        }
        None => false,
    }
}

/// 把规范化 WS 帧落库（best-effort：失败仅记日志，不影响实时推送）。
///
/// - 文本/thought：按 session 缓冲在 `SpawnedAgent`，终态回调统一落一行；
/// - tool_call/tool_result/plan：到达即落；session_title 写回 sessions 表。
///
/// 落库不依赖 WS 连接存活——断线期间后台跑完的回合同样可追溯。
async fn persist_acp_frame(
    db: &Database,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
    frame: &serde_json::Value,
) {
    match frame["type"].as_str().unwrap_or("") {
        "assistant_chunk" => {
            let mut map = sessions.lock().await;
            if let Some(a) = map.get_mut(sid) {
                let content = frame["content"].as_str().unwrap_or("");
                if frame["thought"].as_bool().unwrap_or(false) {
                    a.thought_buf.push_str(content);
                } else {
                    a.text_buf.push_str(content);
                }
            }
        }
        "tool_call" => {
            let call = serde_json::json!([{
                "id": frame["id"],
                "name": frame["name"],
                "arguments": frame.get("args").cloned().unwrap_or(serde_json::Value::Null),
                "tool_kind": frame["tool_kind"],
                "diffs": frame.get("diffs").cloned().unwrap_or(serde_json::Value::Null),
                "locations": frame.get("locations").cloned().unwrap_or(serde_json::Value::Null),
            }]);
            let msg_id = format!("{:032x}", rand::random::<u128>());
            if let Err(e) = db
                .agent_add_message_v2(
                    &msg_id,
                    sid,
                    "assistant",
                    "",
                    Some(&call.to_string()),
                    frame["id"].as_str(),
                    frame["name"].as_str(),
                    "tool_calls",
                )
                .await
            {
                tracing::warn!(session_id = %sid, "persist tool_call failed: {e}");
            }
        }
        "tool_result" => {
            let msg_id = format!("{:032x}", rand::random::<u128>());
            if let Err(e) = db
                .agent_add_message_v2(
                    &msg_id,
                    sid,
                    "assistant",
                    frame["result"].as_str().unwrap_or(""),
                    None,
                    frame["id"].as_str(),
                    frame["name"].as_str(),
                    "tool_result",
                )
                .await
            {
                tracing::warn!(session_id = %sid, "persist tool_result failed: {e}");
            }
        }
        "plan" => {
            let msg_id = format!("{:032x}", rand::random::<u128>());
            let entries = frame["entries"].to_string();
            if let Err(e) = db
                .agent_add_message_v2(
                    &msg_id, sid, "assistant", &entries, None, None, Some("plan"), "message",
                )
                .await
            {
                tracing::warn!(session_id = %sid, "persist plan failed: {e}");
            }
        }
        "session_title" => {
            if let Some(title) = frame["title"].as_str() {
                if let Err(e) = db.agent_update_session_title(sid, title).await {
                    tracing::warn!(session_id = %sid, "persist session title failed: {e}");
                }
            }
        }
        _ => {} // usage 等：仅实时推送，不落库
    }
}

/// 回合终态：把缓冲的 assistant 文本 / thought 各落一行并清空缓冲。
/// 取消/错误/断线终态同样落已有缓冲（用户能看到的那部分回合过程可追溯）。
async fn flush_acp_turn_buffers(
    db: &Database,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) {
    let (text, thought) = {
        let mut map = sessions.lock().await;
        let Some(a) = map.get_mut(sid) else {
            return;
        };
        (std::mem::take(&mut a.text_buf), std::mem::take(&mut a.thought_buf))
    };
    for (name, content) in [(None, text), (Some("thought"), thought)] {
        if content.is_empty() {
            continue;
        }
        let msg_id = format!("{:032x}", rand::random::<u128>());
        if let Err(e) = db
            .agent_add_message_v2(
                &msg_id, sid, "assistant", &content, None, None, name, "message",
            )
            .await
        {
            tracing::warn!(session_id = %sid, "persist turn text failed: {e}");
        }
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
            cancelled_turns: std::collections::HashSet::new(),
            turn_generation: 0,
            last_activity: std::time::Instant::now(),
            exited: false,
            text_buf: String::new(),
            thought_buf: String::new(),
            config_options: Vec::new(),
            spawn_ready: watch::channel(false).0,
        }
    }

    /// 装配 mock agent（duplex → pump → mock_acp_agent）并完成 ACP handshake。
    /// `ws_tx` 注册为会话条目的初始事件通道；连接任务的通知处理器此后每次事件
    /// 从条目动态解析通道（见 `current_ws_tx`）。
    async fn setup_handshake(
        bridge: &AcpBridge,
        ws_tx: mpsc::Sender<serde_json::Value>,
    ) {
        let (agent_io, pump_io) = tokio::io::duplex(64 * 1024);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
        let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(32);

        let mut agent = spawned_agent();
        agent.agent_io = Some(agent_io);
        agent.stdout_tx = Some(stdout_tx.clone());
        agent.ws_tx = Some(ws_tx.clone());
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), agent);

        tokio::spawn(run_stdio_pump(
            pump_io,
            stdout_rx,
            control_tx,
            "sess-1".into(),
        ));
        tokio::spawn(mock_acp_agent(control_rx, stdout_tx));

        bridge
            .acp_handshake("sess-1", "/mock")
            .await
            .expect("handshake should complete");
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
    async fn test_ensure_session_missing_model_config_rejected() {
        // workspace/session/全局默认均未配置模型 → spawn 前门禁拦截（不发起任何请求）
        let bridge = mock_bridge(|_| unreachable!("missing model should not spawn")).await;
        let mut ws = acp_workspace();
        ws.llm_model_id = None;
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx)
            .await
            .expect_err("no model config should be rejected");
        assert!(err.contains("未配置"), "err: {err}");
    }

    #[tokio::test]
    async fn test_session_spawned_reflects_registry() {
        // 未登记 → false；登记存活条目 → true；exited 陈旧条目 → false
        let bridge = mock_bridge(|_| unreachable!("no requests expected")).await;
        assert!(!bridge.session_spawned("sess-1").await);
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), spawned_agent());
        assert!(bridge.session_spawned("sess-1").await);
        let mut exited = spawned_agent();
        exited.exited = true;
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), exited);
        assert!(!bridge.session_spawned("sess-1").await);
    }

    #[tokio::test]
    async fn test_ensure_session_session_model_passes_gate() {
        // session.model 已配置、workspace 未配 llm_model_id → 门禁放行，进入 spawn
        // 路径（此处 LLM 代理绑定失败 → 错误是 bind，而非「未配置」）。
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/workspace", None, None, "gemini", None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let (tx, mut rx) = mpsc::channel(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let registry2 = registry.clone();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let sid = match &req {
                    ControlMessage::AgentSpawnRequest { session_id, .. } => session_id.clone(),
                    ControlMessage::AgentLlmProxyStart { session_id } => session_id.clone(),
                    other => panic!("unexpected request: {other:?}"),
                };
                let resp = match &req {
                    ControlMessage::AgentLlmProxyStart { session_id } => {
                        ControlMessage::AgentLlmProxyReady {
                            session_id: session_id.clone(),
                            port: 0, // 绑定失败：spawn 快速失败，验证门禁已过
                        }
                    }
                    other => panic!("unexpected request: {other:?}"),
                };
                registry2.resolve_spawn_pending(&sid, resp).await;
            }
        });
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut ws = acp_workspace();
        ws.llm_model_id = None;
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx)
            .await
            .expect_err("spawn should be attempted past the model gate");
        assert!(
            err.contains("failed to bind"),
            "error should be from spawn, not model gate: {err}"
        );
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
                        // plan + thought：验证新事件类型的落库
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "agent_thought_chunk",
                                    "content": { "type": "text", "text": "思考一下" } } }
                        }));
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "plan",
                                    "entries": [ { "content": "步骤一", "priority": "high",
                                        "status": "in_progress" } ] } }
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
        setup_handshake(&bridge, ws_tx.clone()).await;
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

        // 事件序列：assistant_chunk → tool_call → tool_result(名从缓存补)
        // → assistant_chunk(thought) → plan → done
        let mut events = Vec::new();
        for _ in 0..6 {
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
        assert_eq!(events[3]["type"], "assistant_chunk");
        assert_eq!(events[3]["thought"], true);
        assert_eq!(events[3]["content"], "思考一下");
        assert_eq!(events[4]["type"], "plan");
        assert_eq!(events[4]["entries"][0]["content"], "步骤一");
        assert_eq!(events[5]["type"], "done");
        // 回合结束：busy 复位，可再次 prompt
        assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
    }

    #[tokio::test]
    async fn test_reconnect_swaps_ws_channel_for_streaming() {
        // 回归（评审 Finding 1）：连接任务的通知处理器在 handshake 时捕获过一次
        // ws_tx，重连后 ensure_session 的 dedup 只刷新条目里的 ws_tx——旧捕获会把
        // 流式帧推给已断开的旧连接（try_send 静默失败，新连接只能看到 done）。
        // 修复后处理器每次事件动态解析，流式帧应全部到达新连接。
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        // 旧连接 A：handshake 建立常驻连接任务。
        let (ws_tx_a, mut ws_rx_a) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake(&bridge, ws_tx_a).await;

        // 新连接 B：重连 → ensure_session dedup 把条目里的 ws_tx 刷新到 B。
        let (ws_tx_b, mut ws_rx_b) = mpsc::channel::<serde_json::Value>(16);
        bridge
            .ensure_session("sess-1", &acp_workspace(), ws_tx_b)
            .await
            .expect("reconnect dedup should refresh ws_tx");

        bridge
            .prompt("sess-1", "hello")
            .await
            .expect("prompt should send");

        // 流式帧（assistant_chunk / tool_call / tool_result / thought / plan）+
        // done 应全部到达 B。
        for expected in [
            "assistant_chunk",
            "tool_call",
            "tool_result",
            "assistant_chunk",
            "plan",
            "done",
        ] {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx_b.recv())
                .await
                .expect("timed out waiting for ws event on new channel")
                .expect("ws channel closed");
            assert_eq!(ev["type"], expected, "event on new channel: {ev}");
        }
        // 旧连接 A 不应收到任何帧（handshake 后捕获的旧 sender 已不再被使用）。
        // 注意：swap 后 A 通道所有 sender 都已 drop，recv 会以 Ok(None)（通道关闭）
        // 或 Err（超时）返回——两者都表示"没有帧"，只有 Ok(Some(..)) 才是泄漏。
        let stale = tokio::time::timeout(std::time::Duration::from_millis(200), ws_rx_a.recv())
            .await;
        assert!(
            !matches!(stale, Ok(Some(_))),
            "old channel should receive nothing after reconnect: {stale:?}"
        );
    }

    #[tokio::test]
    async fn test_cancel_suppresses_terminal_frame() {
        // 回归（评审 Finding 4）：取消/杀进程后 PromptResponse 才到达时，
        // on_receiving_result 不应再发 done/error 终态帧（stopped 帧已由 WS
        // handler 回发；kill 后回调以 Err 到达，不抑制会再补一条误导性 error）。
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
        setup_handshake(&bridge, ws_tx.clone()).await;

        // 模拟 cancel 已执行：prompt 会分配 turn_generation=1，cancel 把该
        // 代数记入 cancelled_turns（真实路径由 cancel() 在 busy 时插入当前代数）。
        // 这里直接预置，等价于 cancel 发生在 prompt 之后但终态回调之前。
        bridge
            .sessions
            .lock()
            .await
            .get_mut("sess-1")
            .unwrap()
            .cancelled_turns
            .insert(1);

        bridge
            .prompt("sess-1", "hello")
            .await
            .expect("prompt should send");

        // 流式通知不受取消抑制（mock agent 仍在回话：assistant_chunk / tool_call /
        // tool_result / thought / plan）；终态 done 应被抑制。
        for expected in [
            "assistant_chunk",
            "tool_call",
            "tool_result",
            "assistant_chunk",
            "plan",
        ] {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv())
                .await
                .expect("timed out waiting for streamed event")
                .expect("ws channel closed");
            assert_eq!(ev["type"], expected, "event: {ev}");
        }
        let terminal = tokio::time::timeout(std::time::Duration::from_millis(300), ws_rx.recv())
            .await;
        assert!(
            terminal.is_err(),
            "cancelled turn must not emit a terminal frame"
        );
        // busy 已复位（回合状态不被卡死），且取消标记已消费（供下一回合从干净态开始）
        {
            let s = bridge.sessions.lock().await;
            assert!(!s.get("sess-1").unwrap().busy);
            assert!(s.get("sess-1").unwrap().cancelled_turns.is_empty());
        }
    }

    #[tokio::test]
    async fn test_cancel_then_immediate_new_prompt_not_suppressed() {
        // 回归（P0-5）：cancel 后立即重发 prompt，新回合的终态回调不得被旧回合
        // 的取消标记误吞（单布尔时代会错误抑制新回合的 done 帧）。
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
        setup_handshake(&bridge, ws_tx.clone()).await;

        // 模拟旧回合（turn_generation=1）已跑完且被取消：预置计数器到 1，
        // 并把代数 1 记入 cancelled_turns。接下来的 prompt 会分配代数 2，
        // 其终态回调不应被代数 1 的取消标记抑制。
        {
            let mut sessions = bridge.sessions.lock().await;
            let agent = sessions.get_mut("sess-1").unwrap();
            agent.turn_generation = 1;
            agent.cancelled_turns.insert(1);
        }

        bridge
            .prompt("sess-1", "hello")
            .await
            .expect("prompt should send");

        // 新回合应正常收到 done 帧（不被旧回合的取消标记抑制）
        let mut got_done = false;
        let mut events = Vec::new();
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv()).await {
                Ok(Some(ev)) if ev["type"] == "done" => {
                    got_done = true;
                    break;
                }
                Ok(Some(ev)) => {
                    events.push(ev);
                    continue;
                } // 流式事件，继续等终态
                Ok(None) => panic!("ws channel closed unexpectedly, events so far: {events:?}"),
                Err(_) => panic!("timed out waiting for done frame, events so far: {events:?}"),
            }
        }
        assert!(
            got_done,
            "new turn must emit done frame, not suppressed by old cancel. events: {events:?}"
        );
        // busy 已复位，旧取消标记仍残留（未被本回合消费）
        {
            let s = bridge.sessions.lock().await;
            assert!(!s.get("sess-1").unwrap().busy);
            assert!(s.get("sess-1").unwrap().cancelled_turns.contains(&1));
        }
    }

    #[tokio::test]
    async fn test_acp_events_persisted_to_db() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/workspace", None, None, "gemini", None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db.clone());

        let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake(&bridge, ws_tx.clone()).await;
        bridge.prompt("sess-1", "hello").await.expect("prompt");
        // 收完终态帧：此时终态回调的落库已完成（done 帧在落库之后发送）
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv())
                .await
                .expect("timed out")
                .expect("closed");
            if ev["type"] == "done" {
                break;
            }
        }

        let rows = db.agent_list_messages("sess-1").await.unwrap();
        // assistant 文本（缓冲到终态落一行）
        let texts: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == "message" && r.name.is_none())
            .collect();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].content, "hello from mock");
        // thought 行
        let thoughts: Vec<_> = rows
            .iter()
            .filter(|r| r.name.as_deref() == Some("thought"))
            .collect();
        assert_eq!(thoughts.len(), 1);
        assert_eq!(thoughts[0].content, "思考一下");
        // plan 行（entries JSON）
        let plans: Vec<_> = rows
            .iter()
            .filter(|r| r.name.as_deref() == Some("plan"))
            .collect();
        assert_eq!(plans.len(), 1);
        assert!(plans[0].content.contains("步骤一"));
        // tool_calls 行：tool_calls JSON 含 tool_kind
        let calls: Vec<_> = rows.iter().filter(|r| r.kind == "tool_calls").collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_call_id.as_deref(), Some("call_1"));
        let call_json: serde_json::Value =
            serde_json::from_str(calls[0].tool_calls.as_deref().unwrap()).unwrap();
        assert_eq!(call_json[0]["tool_kind"], "other"); // mock 未带 kind → 默认
        assert_eq!(call_json[0]["arguments"], "{\"cmd\":\"ls\"}");
        // tool_result 行
        let results: Vec<_> = rows.iter().filter(|r| r.kind == "tool_result").collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(results[0].content, "a.rs");
    }

    #[tokio::test]
    async fn test_acp_persistence_survives_ws_disconnect() {
        // 断线（detach_ws_tx 置 ws_tx=None）期间回合跑完：消息仍落库。
        // 用真实断线路径 detach_ws_tx——区别于 drop(ws_rx) 只关接收端
        // （后者 ws_tx 仍占位，通知处理器仍能过 current_ws_tx 守卫，落库
        // 本来就发生）；detach 后条目 ws_tx=None，验证 persist 已移出 ws_tx
        // guard 之前（评审修复）：断线点之后到达的帧同样落库。
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/workspace", None, None, "gemini", None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db.clone());

        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake(&bridge, ws_tx).await;
        bridge.detach_ws_tx("sess-1").await; // 真实断线：清空条目 WS 通道
        bridge.prompt("sess-1", "hello").await.expect("prompt");
        // 等终态回调落库完成：轮询 DB（断线下无终态帧，只能轮询落库结果）。
        // break 条件要求 tool_result 与终态 flush 的文本行（kind='message' 且
        // name=None）都已落库，避免「tool_result 已落、终态 flush 未完成」的
        // 理论 flake 窗口。
        let mut rows = Vec::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            rows = db.agent_list_messages("sess-1").await.unwrap();
            if rows.iter().any(|r| r.kind == "tool_result")
                && rows.iter().any(|r| r.kind == "message" && r.name.is_none())
            {
                break;
            }
        }
        assert!(
            rows.iter().any(|r| r.kind == "tool_calls"),
            "tool_call should persist without ws consumer: {rows:?}"
        );
        assert!(rows.iter().any(|r| r.kind == "tool_result"));
        assert!(
            rows.iter().any(|r| r.kind == "message" && r.name.is_none()),
            "assistant text should persist without ws consumer"
        );
    }
}
