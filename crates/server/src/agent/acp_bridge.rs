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

use futures_util::StreamExt;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, DeleteSessionRequest, InitializeRequest,
    NewSessionRequest, PermissionOption, PermissionOptionId, PermissionOptionKind,
    PromptRequest, ReadTextFileRequest, ReadTextFileResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigValueId, SessionId,
    SessionNotification, SetSessionConfigOptionRequest, TextContent, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{ByteStreams, Client, ConnectionTo};

use crate::db::Database;
use crate::llm::crypto::LlmCipher;
use crate::persistence::db::agent::AgentWorkspaceRecord;
use rust_tunnel_common::ControlMessage;

use super::acp_events::map_update;
use super::llm_bridge::{self, LlmGatewayEndpoint};
use super::spawner::AgentSpawner;
use super::{ApprovalOption, ApprovalResult};

/// spawn/协商超时：LLM 代理启动与 agent 进程拉起各限 30s。
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);
/// `wait_ready` 等待预 spawn 就绪的预算：必须覆盖 spawn 流水线最坏耗时
/// （LLM 代理协商 + spawn 协商 + ACP handshake 各 `SPAWN_TIMEOUT`，另加
/// 配置注入若干 `CONFIG_OPTION_TIMEOUT`）。若与 `SPAWN_TIMEOUT` 相同，
/// agent 冷启动慢/隧道 RTT 大时，首条消息会在后台 spawn 成功前误报
/// "wait for ACP handshake timed out"（重试即可成功，误导用户）。
const READY_TIMEOUT: Duration = Duration::from_secs(150);
/// config option 切换超时：agent 无响应时让 WS 连接及时拿到 error 帧回滚，
/// 而非无限阻塞（回放挂起同样受此约束）。
const CONFIG_OPTION_TIMEOUT: Duration = Duration::from_secs(15);
/// 空闲 30 分钟杀进程（重挂 ACP 连接由客户端 spawn manager 处理）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// reaper 检查间隔。
const REAP_INTERVAL: Duration = Duration::from_secs(60);
/// 排队消息上限：进行中回合期间 submit_prompt 入队；超出上限报错（不静默丢弃，
/// 用户能立刻知道消息未被接受）。
const MAX_PENDING_PROMPTS: usize = 20;
/// 取消宽限期默认值：`cancel()` 发出 session/cancel 后，agent 未在此时限内响应
/// PromptResponse（终态回调未清 busy）则兜底杀客户端进程。
const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(10);

/// 排队等待发送的用户 prompt（进行中回合时经 [`AcpBridge::submit_prompt`] 暂存）。
/// `content` 是注入 @引用后的完整消息（mgmt/api/agent.rs 分派前已 `inject_refs`，
/// refs 内容已内联）；`refs` 原样留存备查。FIFO：终态回调逐个取出续跑，队列排空才
/// 发 done。
#[derive(Clone)]
struct PendingPrompt {
    content: String,
    /// @引用路径列表，随消息原样留存备查（调用方在分派前已 inject_refs，
    /// refs 内容已内联进 `content`，本字段仅作记录不参与运行）。
    #[allow(dead_code)]
    refs: Vec<String>,
}

/// 回合内一段已到达但尚未落库的 assistant 输出（正文或思考）。流式 chunk 按
/// 到达顺序 push，正文/思考交错时各自独立成段——flush 时按此顺序落库，保证
/// DB rowid 顺序 = 对话顺序（思考先于其后的正文）。旧实现用 `text_buf`/`thought_buf`
/// 两个独立缓冲，flush 时硬编码先正文后思考，刷新后顺序反了（见 flush）。
#[derive(Debug, Default)]
struct TurnSegment {
    /// true = 落 `name='thought'` 行（思考）；false = 普通 assistant 正文行。
    thought: bool,
    content: String,
}

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
    /// 当前注册 WS 通道所属的连接唯一标识：`detach_ws_tx` 按它判断「这个
    /// teardown 是不是注册方本人」。刷新竞态下旧连接 teardown 晚于新连接注册，
    /// 若无条件清空会误清新连接通道（tool_result/done 全丢）。
    ws_conn_id: u64,
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
    /// 回合内 assistant 输出片段缓冲（chunk 按到达顺序 append，同类型相邻合并）。
    /// flush 时按顺序逐段落库——正文/思考交错顺序与对话一致，刷新后历史不乱。
    turn_segments: Vec<TurnSegment>,
    /// ACP 会话配置选项快照（handshake 捕获 + config_option_update 全量替换）。
    /// 空 Vec 且 handshake 未完成 = 尚无状态；agent 不上报时保持空。
    config_options: Vec<SessionConfigOption>,
    /// 握手 + 配置注入完成信号（false → true）。连接预 spawn（后台任务）仍在
    /// 握手/注入 overrides 时，首条 user_message 的 `wait_ready` 经 `subscribe`
    /// 与 `wait_for` 等待它，避免 `prompt` 报 "ACP handshake not complete" 或在
    /// config 注入完成前首回合开跑。条目被移除（spawn 失败/kill）时 Sender drop，
    /// wait_for 以 RecvError 返回。
    spawn_ready: watch::Sender<bool>,
    /// 进行中回合期间排队等待发送的 prompt（FIFO）。兜底杀进程后重拉新进程时
    /// （ensure_session 移除 exited 条目）迁移到新条目，排队消息不丢。
    pending_prompts: VecDeque<PendingPrompt>,
    /// 回合终态唤醒信号：`prompt()` 终态回调清 busy 后 notify_waiters，取消的
    /// 兜底任务（见 [`AcpBridge::cancel`]）据此走优雅路径提前退出，不再等宽限期。
    cancel_notify: Arc<tokio::sync::Notify>,
}

/// ACP `session/request_permission` → 审批回调。
///
/// `(session_id, tool, summary, args_preview, options, ws_tx) → 审批结果`。`options`
/// 是 agent 给出的权限选项透传（用户可选中具体 option_id）；无选项时回调返回
/// `Approved`/`Denied`。`AcpBridge` 构造时 `AgentState` 尚在构建（循环依赖），
/// 由 `AgentState::new` 通过 [`Self::with_approval`] 注入真实实现（走
/// `AgentState::request_approval`，与 runner 路径共用审批弹层与 pending map）；
/// 未注入时默认拒绝。
type ApproveFn = dyn Fn(
        String,
        String,
        String,
        String,
        Vec<ApprovalOption>,
        mpsc::Sender<serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = ApprovalResult> + Send>>
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
    /// 最近一次 spawn 失败的真实原因（session_id → 错误）。预 spawn 失败会
    /// 移除占位条目，无此缓存时已在等待的 `wait_ready` 只能报误导性的
    /// "session not spawned"，真实原因（binary not found / handshake failed
    /// 等）被吞。新一轮 spawn 尝试（占位插入）时清除，只在会话不在场且无
    /// 在途尝试时被消费（见 [`Self::spawn_failure`]）。
    spawn_errors: Arc<Mutex<HashMap<String, String>>>,
    /// LLM 网关入口（内部 HTTP 回环调用）；未注入时 LLM 代理请求全部 502。
    gateway: Option<LlmGatewayEndpoint>,
    /// 取消宽限期：`cancel()` 发出 session/cancel 后 agent 未在此时限内响应
    /// PromptResponse 则兜底杀进程。
    cancel_grace: Duration,
}

impl AcpBridge {
    pub fn new(spawner: AgentSpawner, db: Database) -> Self {
        let bridge = Self {
            spawner,
            db,
            cipher: None,
            approval: Arc::new(|_, _, _, _, _, _| Box::pin(async { ApprovalResult::Denied })),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            spawn_errors: Arc::new(Mutex::new(HashMap::new())),
            gateway: None,
            cancel_grace: DEFAULT_CANCEL_GRACE,
        };
        bridge.start_idle_reaper();
        bridge
    }

    /// 注入取消宽限期（仅测试：验证兜底杀进程路径；生产用默认 10s）。
    #[cfg(test)]
    #[must_use]
    pub fn with_cancel_grace(mut self, grace: Duration) -> Self {
        self.cancel_grace = grace;
        self
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
        conn_id: u64,
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
            // 兜底杀进程后重拉：旧（exited）条目里排队等待的 prompt 迁移到新条目，
            // 避免 cancel 期间提交、进程死时尚未消费的等待消息在重拉后丢失。
            let mut migrated_prompts = VecDeque::new();
            match sessions.get(session_id) {
                Some(agent) if !agent.exited => {
                    // 已有活跃进程：仅刷新事件通道（多标签页/重连共用同一进程；
                    // 事件推给最新连接，避免断线后的旧 sender 占位）。同时记录
                    // 本连接的 conn_id，供 detach 按身份清空。
                    if let Some(a) = sessions.get_mut(session_id) {
                        a.ws_tx = Some(ws_tx.clone());
                        a.ws_conn_id = conn_id;
                    }
                    return Ok(());
                }
                // 进程已退出的陈旧条目：视为不存在，移除后走 spawn 路径重拉。
                // 否则死进程会阻塞 respawn 直到 30 分钟 reaper 清掉它，后续
                // prompt 一直报 "agent process has exited"。
                Some(_) => {
                    tracing::info!(session_id, "re-spawning exited ACP session");
                    migrated_prompts = sessions
                        .get(session_id)
                        .map(|a| a.pending_prompts.clone())
                        .unwrap_or_default();
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
                    ws_conn_id: conn_id,
                    busy: false,
                    cancelled_turns: std::collections::HashSet::new(),
                    turn_generation: 0,
                    last_activity: std::time::Instant::now(),
                    exited: false,
                    turn_segments: Vec::new(),
                    config_options: Vec::new(),
                    spawn_ready: watch::channel(false).0,
                    pending_prompts: migrated_prompts,
                    cancel_notify: Arc::new(tokio::sync::Notify::new()),
                },
            );
            Some((pump_io, stdout_rx))
        };
        if pump_setup.is_some() {
            // 新一轮 spawn 尝试：清除上一次的失败缓存，避免 wait_ready 在新
            // 尝试在途时消费到陈旧错误（见 spawn_failure 的消费条件）。
            self.spawn_errors.lock().await.remove(session_id);
        }

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
        // 阶段耗时打点：wait_ready 超时只能看到「spawn 仍在进行」，各阶段
        // （协商/握手/配置注入）的耗时分布是定位卡点（冷启动慢 vs 隧道 RTT
        // vs agent 不响应 set_config_option）的关键证据。
        let pipeline_start = std::time::Instant::now();
        let outcome: Result<(), String> = async {
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
            tracing::info!(
                session_id,
                elapsed_ms = pipeline_start.elapsed().as_millis() as u64,
                "acp spawn stage: llm proxy ready (port {port})"
            );
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
            tracing::info!(
                session_id,
                elapsed_ms = pipeline_start.elapsed().as_millis() as u64,
                "acp spawn stage: agent process spawned"
            );
            // 3) ACP handshake（stdio pump 已就绪，此步建立 ACP 连接 + WS 接线；
            // ws_tx 由连接任务的处理器每次事件从会话条目动态解析，无需传入）。
            // 重拉（断连过久/reaper 杀进程后）时从 DB 取持久化的 ACP session id，
            // 交 handshake 优先 session/resume 恢复上下文；读失败视为无（重拉走全新）。
            let persisted_acp_session_id = self
                .db
                .agent_get_session(session_id)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.acp_session_id);
            let root_path = workspace.root_path.clone();
            self.acp_handshake(session_id, &root_path, persisted_acp_session_id)
                .await?;
            tracing::info!(
                session_id,
                elapsed_ms = pipeline_start.elapsed().as_millis() as u64,
                "acp spawn stage: handshake complete"
            );
            Ok(())
        }
        .await;
        if let Err(e) = &outcome {
            // spawn 失败：移除占位，允许后续重试；同时缓存真实原因——已在
            // wait_ready 等待的并发调用（预 spawn 在途时发了首条消息）在
            // Sender drop 后能拿到它，而非误导性的 "session not spawned"。
            self.sessions.lock().await.remove(session_id);
            // start_llm_proxy 成功但 spawn_agent/handshake 失败时，代理端口仍
            // 在客户端监听：下发 AgentLlmProxyStop 释放（best-effort）。
            self.spawner.stop_llm_proxy(&client_id, session_id).await;
            self.spawn_errors
                .lock()
                .await
                .insert(session_id.to_string(), e.clone());
            return outcome;
        }
        // 握手成功：workspace 级 overrides 注入先于 session 级 config_state 回放
        // ——用户显式选择（config_state）覆盖 workspace 默认值。在 acp_handshake
        // 返回后才执行：该函数只收 session_id/root_path，此处持有 workspace 记录。
        self.apply_config_overrides(session_id, workspace).await;
        self.replay_config_state(session_id).await;
        tracing::info!(
            session_id,
            elapsed_ms = pipeline_start.elapsed().as_millis() as u64,
            "acp spawn stage: config injection complete"
        );
        // 配置注入完成后才放行 wait_ready：连接预 spawn（后台任务）场景下，
        // 用户路径的 wait_ready 经 watch 通道等待此信号——必须延后到 overrides/
        // config_state 已生效，首条 prompt 才不与在途 set_config_option 竞态
        // （恢复旧顺序：replay 先于 spawn_ready）。
        // 必须用 send_modify 而非 send：spawn_ready 建通道时只存了 Sender
        // （Receiver 当场 drop），预 spawn 在首条消息（首个 subscribe）之前
        // 完成时 receiver_count==0，`send` 会静默失败且**不写入新值**——
        // wait_ready 将永远看到 false，空等整个 READY_TIMEOUT 后误报
        // 「spawn 仍在进行」。send_modify 无接收者也更新值，恰好匹配
        // 「就绪状态置位」语义。
        if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
            a.spawn_ready.send_modify(|ready| *ready = true);
        }
        // 回放完成后把最终快照推给当前 WS 连接（重连场景前端立即可见）。
        if let Some(frame) = self.session_state_frame(session_id).await {
            if let Some(ws_tx) = current_ws_tx(&self.sessions, session_id).await {
                let _ = ws_tx.try_send(frame);
            }
        }
        outcome
    }

    /// ACP handshake：initialize → 会话建立（session/new，或带持久化 id 时优先
    /// session/resume 恢复上下文）。
    ///
    /// 从占位条目取走 duplex 的 ACP 端，spawn 一个常驻连接任务（crate 的
    /// `Client` 角色 + `ByteStreams` transport），任务内完成
    /// `initialize` + 会话建立（`session/resume` 或 `session/new`），把
    /// `ConnectionTo<Agent>` 与 ACP session id 写回会话条目；随后 main_fn 挂起
    /// 等待 incoming EOF（保持连接存活，直到进程退出/会话被杀）。通知
    /// （`session/update`）经 [`map_update`] 映射后推会话条目当前的 ws_tx——
    /// 处理器每次事件动态解析，重连自动切到新连接；权限请求
    /// （`session/request_permission`）走审批回调。
    ///
    /// `persisted_acp_session_id` 为上次会话建立落库的 ACP session id（断线过久
    /// 重拉时从 DB 取）。agent 声明支持 `session/resume` 时优先 resume（凭 id 从
    /// 客户端磁盘恢复 agent 侧对话上下文），失败/不支持回退 `session/new`。
    /// 最终生效的 session id 落库（best-effort），供下次重拉继续 resume。
    ///
    /// 注意：`agent_client_protocol::Client` 是角色标记（unit struct），并非
    /// 连接句柄；连接句柄是 `ConnectionTo<Agent>`。每 session 一条专用连接，
    /// 通知无需按 session id 过滤。
    async fn acp_handshake(
        &self,
        session_id: &str,
        cwd: &str,
        persisted_acp_session_id: Option<String>,
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
        let cwd = cwd.to_string();
        let approval = self.approval.clone();
        let sessions = self.sessions.clone();
        let db = self.db.clone();
        let spawner = self.spawner.clone();
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
                            // 透传 agent 给出的权限选项：用户可从中选具体选项（如
                            // AskUserQuestion / plan 审批），而非服务端硬编码挑选。
                            // options 为空时审批卡片保持 approve/deny 二元按钮。
                            let options: Vec<ApprovalOption> = request
                                .options
                                .iter()
                                .map(permission_option_to_approval)
                                .collect();
                            let result = approval(
                                sid.clone(),
                                tool_name,
                                "ACP 工具调用请求".to_string(),
                                args_preview,
                                options,
                                ws_tx,
                            )
                            .await;
                            let outcome = match result {
                                // 用户选中了具体选项：原样回传 option_id。
                                ApprovalResult::Selected(id) => Some(
                                    PermissionOptionId::from(id),
                                ),
                                // 无选项路径的批准/拒绝：fallback 到服务端挑默认选项
                                // （AllowAlways→AllowOnce / RejectAlways→RejectOnce）。
                                ApprovalResult::Approved => pick_option(
                                    &request.options,
                                    PermissionOptionKind::AllowAlways,
                                )
                                .or_else(|| {
                                    pick_option(
                                        &request.options,
                                        PermissionOptionKind::AllowOnce,
                                    )
                                }),
                                ApprovalResult::Denied => pick_option(
                                    &request.options,
                                    PermissionOptionKind::RejectAlways,
                                )
                                .or_else(|| {
                                    pick_option(
                                        &request.options,
                                        PermissionOptionKind::RejectOnce,
                                    )
                                }),
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
                .on_receive_request(
                    {
                        // fs/read_text_file：绝对路径 → 工作区相对路径 → 经隧道到
                        // 客户端沙箱读取，内容回包给 agent（claude-code 据此读项目文件）。
                        let sid = sid.clone();
                        let db = db.clone();
                        let sessions = sessions.clone();
                        let spawner = spawner.clone();
                        async move |request: ReadTextFileRequest, responder, _cx| {
                            let outcome = exec_fs_read(
                                &db,
                                &spawner,
                                &sessions,
                                &sid,
                                &request.path.to_string_lossy(),
                            )
                            .await;
                            match outcome {
                                Ok(content) => {
                                    let _ = responder.respond(ReadTextFileResponse::new(content));
                                }
                                Err(e) => {
                                    let _ = responder.respond_with_error(
                                        agent_client_protocol::util::internal_error(
                                            format!("fs/read_text_file failed: {e}"),
                                        ),
                                    );
                                }
                            }
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        // fs/write_text_file：同 read，写文件到客户端沙箱。
                        let sid = sid.clone();
                        let db = db.clone();
                        let sessions = sessions.clone();
                        let spawner = spawner.clone();
                        async move |request: WriteTextFileRequest, responder, _cx| {
                            let outcome = exec_fs_write(
                                &db,
                                &spawner,
                                &sessions,
                                &sid,
                                &request.path.to_string_lossy(),
                                &request.content,
                            )
                            .await;
                            match outcome {
                                Ok(()) => {
                                    let _ = responder.respond(WriteTextFileResponse::new());
                                }
                                Err(e) => {
                                    let _ = responder.respond_with_error(
                                        agent_client_protocol::util::internal_error(
                                            format!("fs/write_text_file failed: {e}"),
                                        ),
                                    );
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
                        // 1) initialize；2) 会话建立（session/resume 或 session/new）。
                        // 失败则 setup 报错并关连接。
                        let setup = async {
                            // 声明客户端 fs 能力：agent 才会通过 fs/read_text_file 与
                            // fs/write_text_file 请求读文件（服务端经隧道转发到客户端
                            // 沙箱执行）。不声明则 agent 静默降级（如报「不支持」）。
                            let capabilities = client_capabilities();
                            let init_resp = cx
                                .send_request(
                                    InitializeRequest::new(ProtocolVersion::V1)
                                        .client_capabilities(capabilities),
                                )
                                .block_task()
                                .await?;
                            // agent 声明 session/resume 能力 + 持有多余的持久化 id →
                            // 优先 resume（凭 id 从客户端磁盘恢复 agent 侧对话上下文，
                            // 解决断线过久/进程被杀后上下文丢失）。
                            let resume_capable = init_resp
                                .agent_capabilities
                                .session_capabilities
                                .resume
                                .is_some();
                            // 会话建立：resume 成功复用旧 id；失败/无 id/不支持 →
                            // session/new（全新会话，原行为）。
                            let (acp_session_id, config_options): (
                                SessionId,
                                Vec<SessionConfigOption>,
                            ) = match persisted_acp_session_id.as_deref() {
                                Some(persisted) if resume_capable => {
                                    match cx
                                        .send_request(ResumeSessionRequest::new(
                                            SessionId::new(persisted),
                                            &cwd,
                                        ))
                                        .block_task()
                                        .await
                                    {
                                        Ok(resp) => {
                                            tracing::info!(
                                                session_id = %sid,
                                                acp_session_id = %persisted,
                                                "acp session resumed"
                                            );
                                            (
                                                SessionId::new(persisted),
                                                resp.config_options.clone().unwrap_or_default(),
                                            )
                                        }
                                        Err(e) => {
                                            // resume 失败（会话文件缺失/已清理等）
                                            // → 回退全新会话，不阻断建立。
                                            tracing::warn!(
                                                session_id = %sid,
                                                "acp resume failed, fall back to new session: {e}"
                                            );
                                            let new_session = cx
                                                .send_request(NewSessionRequest::new(&cwd))
                                                .block_task()
                                                .await?;
                                            (
                                                new_session.session_id.clone(),
                                                new_session
                                                    .config_options
                                                    .clone()
                                                    .unwrap_or_default(),
                                            )
                                        }
                                    }
                                }
                                _ => {
                                    let new_session = cx
                                        .send_request(NewSessionRequest::new(&cwd))
                                        .block_task()
                                        .await?;
                                    (
                                        new_session.session_id.clone(),
                                        new_session.config_options.clone().unwrap_or_default(),
                                    )
                                }
                            };
                            // 落库（best-effort）：断线重拉时凭它 session/resume
                            // 恢复上下文。失败仅 warn，不阻断会话建立。
                            if let Err(e) = db
                                .agent_set_acp_session_id(&sid, Some(acp_session_id.0.as_ref()))
                                .await
                            {
                                tracing::warn!(session_id = %sid, "persist acp_session_id failed: {e}");
                            }
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
                // 握手成功即返回。wait_ready 的放行（spawn_ready.send(true)）由
                // ensure_session 在 apply_config_overrides + replay_config_state
                // 完成后统一执行——acp_handshake 只收 session_id/root_path，不持有
                // workspace 记录；把放行延后到配置注入完成，首条 prompt 才不与
                // 在途 set_config_option 竞态（恢复旧顺序：replay 先于 spawn_ready）。
                // 失败分支（下文）不发送 spawn_ready，wait_ready 以超时/Sender drop
                // 返回，语义与移动前一致。
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
    /// [`map_update`] 推送会话条目当前的 ws_tx，`PromptResponse` 到达时终态回调
    /// 处理。回合进行中重复 prompt 报错（`busy` 守卫；ACP 单连接不支持并发回合）
    /// ——用户路径请用 [`Self::submit_prompt`]（进行中自动排队）。
    ///
    /// 终态回调同时承担队列 drain：清 busy、唤醒取消兜底任务后，若 `pending_prompts`
    /// 非空则取队首异步续跑下一条（回合连续，不发 done），排空才发 done；本回合被
    /// 取消（代数命中）时抑制生产者终态帧但仍 drain 队列（停止后排队消息自动发送）。
    /// 兜底杀进程/进程崩溃（exited）后不 drain——排队消息在 ensure_session 重拉新
    /// 进程时迁移，避免往死进程发请求丢失。
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

        let bridge = self.clone();
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
                // 终态：清 busy + 唤醒取消兜底任务 + 取当前 WS 通道 + 排空队列
                // （若会话存活）。取消/杀进程后的终态帧抑制按代数匹配而非全局
                // 布尔：cancel 后立即重发 prompt 时，新回合的终态回调不会被旧回合
                // 的取消标记误吞（评审 Finding）。
                let (ws_tx, next, cancelled, alive) = {
                    let mut map = sessions.lock().await;
                    match map.get_mut(&sid) {
                        Some(a) => {
                            a.busy = false;
                            // 唤醒取消兜底任务走优雅路径。用 notify_waiters 而非
                            // notify_one：notify_one 在无等待者时会暂存一个许可，
                            // 某正常回合的终态若先于兜底任务开始等待时调用，后续的
                            // 兜底任务会误消费陈旧许可而直接跳过杀进程（agent 真卡
                            // 死时进程无人杀）。
                            a.cancel_notify.notify_waiters();
                            let cancelled = a.cancelled_turns.remove(&turn_gen);
                            // 兜底杀进程/进程崩溃后不 drain：排队消息在
                            // ensure_session 重拉新进程时迁移（见 ensure_session），
                            // 避免往死进程发请求丢失。
                            let alive = !a.exited;
                            let next = if alive {
                                a.pending_prompts.pop_front()
                            } else {
                                None
                            };
                            (a.ws_tx.clone(), next, cancelled, alive)
                        }
                        // 会话已 kill/回收：条目移除，不再发终态帧。
                        None => return Ok(()),
                    }
                };
                // 队列非空：不发 done（回合连续），异步发起下一条 prompt。不在
                // 持锁状态 send_request（prompt 内部自己取锁）；spawn 避免同步
                // 递归 async 的深度风险（20 条排队 = 至多 20 层同步调用栈）。
                if let Some(next) = next {
                    // 抽成独立 sync fn 发起下一条（不在 async 闭包里直接
                    // tokio::spawn(bridge.prompt(...))——闭包捕获环境会让 prompt
                    // future 被判定非 Send；独立函数里是普通 owned 数据，编译通过）。
                    spawn_drain_next(bridge.clone(), sid.clone(), next);
                    return Ok(());
                }
                // 队列排空：被取消或会话已死的回合不发生产者终态帧（stopped 帧
                // 已由 WS handler 回发、cancel_fallback 由兜底任务回发，再补
                // error/done 会造成误导）。
                if cancelled || !alive {
                    return Ok(());
                }
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

    /// 提交一条用户消息到 ACP 会话：空闲直接跑（走 [`Self::prompt`]），进行中回合
    /// 排队等待（推 `{"type":"queued"}` 帧通知前端）。回合连续：终态回调逐条续跑
    /// 队列，排空才发 done。
    ///
    /// `content` 为注入 @引用后的完整消息（调用方 mgmt/api/agent.rs 在分派前已
    /// `inject_refs`）；`refs` 原样随 PendingPrompt 存储备查。排队消息同样已由调用
    /// 方立即落库（user 落库在 submit_prompt 之前），刷新/重连后历史完整。
    ///
    /// 返回 Err 的场景：会话不存在 / 进程已退出 / 排队已达 `MAX_PENDING_PROMPTS`
    /// 上限。调用方应把错误以 error 帧回发前端。
    pub async fn submit_prompt(
        &self,
        session_id: &str,
        content: &str,
        refs: Vec<String>,
    ) -> Result<(), String> {
        // 锁内决策：busy 入队；空闲跑现有 prompt()。队列非空但空闲（兜底杀进程后
        // 重拉迁移的旧消息）时，本条排到队尾、先跑队首——保持 FIFO 顺序。
        let run_content = {
            let mut sessions = self.sessions.lock().await;
            let Some(a) = sessions.get_mut(session_id) else {
                return Err("session not spawned".to_string());
            };
            if a.exited {
                return Err("agent process has exited".into());
            }
            if a.busy {
                if a.pending_prompts.len() >= MAX_PENDING_PROMPTS {
                    return Err(format!("排队消息已达上限（{MAX_PENDING_PROMPTS} 条）"));
                }
                a.pending_prompts.push_back(PendingPrompt {
                    content: content.to_string(),
                    refs,
                });
                // queued 帧：状态提示，try_send 丢帧可接受（与通知处理器同语义）。
                if let Some(ws_tx) = a.ws_tx.clone() {
                    let _ = ws_tx.try_send(serde_json::json!({"type": "queued"}));
                }
                return Ok(());
            }
            if a.pending_prompts.is_empty() {
                Some(content.to_string())
            } else {
                // 空闲但队列非空：本条排到队尾（FIFO），先跑队首旧消息。
                a.pending_prompts.push_back(PendingPrompt {
                    content: content.to_string(),
                    refs,
                });
                if let Some(ws_tx) = a.ws_tx.clone() {
                    let _ = ws_tx.try_send(serde_json::json!({"type": "queued"}));
                }
                a.pending_prompts
                    .pop_front()
                    .map(|p| p.content)
            }
        };
        match run_content {
            Some(c) => self.prompt(session_id, &c).await,
            None => Ok(()),
        }
    }

    /// 优雅取消进行中的回合：发 ACP `session/cancel` 通知（**保留进程**），等待
    /// agent 在 `cancel_grace` 内响应 PromptResponse（终态回调清 busy）；超时未
    /// 响应则兜底杀客户端进程（`AgentExecCancel{request_id = session_id}`，客户端
    /// spawn manager 终止内网侧 agent）并推 `{"type":"cancel_fallback"}` 帧。
    ///
    /// 与旧实现的区别：不再立即杀进程——直接杀会丢会话上下文（下次 prompt 走
    /// NewSessionRequest 建空会话）。busy 保持到 PromptResponse 到达才复位，期间
    /// 新消息经 [`Self::submit_prompt`] 排队；兜底杀进程后 `exited` 置位，下一次
    /// ensure_session 自动重拉新进程并迁移排队消息。
    pub async fn cancel(&self, session_id: &str) {
        tracing::info!(session_id, "ACP cancel requested");
        // 仅取消进行中的回合：非 busy 短路返回，防止无在途回合时把代数记入
        // cancelled_turns 后永不消费（泄漏）。
        let (client_id, connection, acp_session_id, turn_gen, cancel_notify) = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get_mut(session_id) {
                Some(agent) if agent.busy => {
                    agent.last_activity = std::time::Instant::now();
                    // 记录当前回合代数为已取消：终态回调据此抑制生产者终态帧。
                    // 用代数而非布尔：cancel 后立即重发 prompt 时，新回合分配新
                    // 代数，不会被本条取消标记误伤。注意不清 busy——回合保持到
                    // PromptResponse 到达（终态回调清位）或兜底杀进程。
                    agent.cancelled_turns.insert(agent.turn_generation);
                    let turn_gen = agent.turn_generation;
                    (
                        agent.client_id.clone(),
                        agent.connection.clone(),
                        agent.acp_session_id.clone(),
                        turn_gen,
                        agent.cancel_notify.clone(),
                    )
                }
                _ => return, // 无进行中回合（或会话不存在）：无事可取消
            }
        };
        // ACP 协议层取消：让 agent 尽快停手（stop_reason = cancelled），进程保留。
        if let (Some(cx), Some(sid)) = (connection, acp_session_id) {
            let _ = cx.send_notification(CancelNotification::new(sid));
        }
        // 兜底任务：agent 未在 cancel_grace 内响应（终态回调未清 busy）则真杀。
        // 捕获的均为克隆（session_id / 代数 / Notify），锁只在二次确认时短暂持有。
        let sessions = self.sessions.clone();
        let spawner = self.spawner.clone();
        let sid = session_id.to_string();
        let grace = self.cancel_grace;
        tokio::spawn(async move {
            tokio::select! {
                // 优雅路径：终态回调清 busy 后 notify_waiters 唤醒，兜底不做任何事。
                _ = cancel_notify.notified() => {}
                // 超时：二次确认（仍 busy 且本代数仍被取消）才杀进程——避免误杀
                // 已恢复的回合 / 终态回调已清 busy 的正常路径。
                _ = tokio::time::sleep(grace) => {
                    let (should_kill, ws_tx) = {
                        let mut map = sessions.lock().await;
                        match map.get_mut(&sid) {
                            Some(a) if a.busy && a.cancelled_turns.contains(&turn_gen) => {
                                a.busy = false;
                                a.cancelled_turns.remove(&turn_gen);
                                (true, a.ws_tx.clone())
                            }
                            _ => (false, None),
                        }
                    };
                    if should_kill {
                        tracing::warn!(session_id = %sid, "ACP agent did not respond to cancel within grace; killing process");
                        spawner.send_agent_cancel(&client_id, &sid).await;
                        if let Some(ws_tx) = ws_tx {
                            let _ = ws_tx.try_send(serde_json::json!({"type": "cancel_fallback"}));
                        }
                    }
                }
            }
        });
    }

    /// 终结 ACP 会话：杀客户端进程 + 移除会话条目（idle reaper / 会话归档关闭
    /// 用）。与 `cancel` 的区别：不再保留会话，进程死后不重拉。
    ///
    /// **保留** agent 侧持久化会话数据：归档后重开会话可 `session/resume`
    /// 恢复上下文。需要连客户端数据一起清理请用 [`Self::kill_and_delete`]。
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
        // 终结会话：一并释放 LLM 回环代理监听端口（防泄漏）。
        self.spawner.stop_llm_proxy(&client_id, session_id).await;
        self.sessions.lock().await.remove(session_id);
        tracing::info!(session_id, "killed ACP session");
    }

    /// 终结 ACP 会话并清理客户端持久化会话数据（**会话删除**用）：先发 ACP
    /// `session/delete` 让 agent 删除其持久化会话文件，再走 [`Self::kill`] 杀进程
    /// 移除条目。
    ///
    /// `session/delete` 是 best-effort（5s 超时防卡死）：连接已断/进程已死时忽略，
    /// 不影响终结。与 [`Self::kill`]（归档用，保留数据）和 idle reaper 的
    /// `send_agent_cancel`（回收空闲进程，保留数据供 resume）区别——只有用户
    /// 显式删除会话才清理 agent 侧数据。
    pub async fn kill_and_delete(&self, session_id: &str) {
        let (connection, acp_sid) = {
            let guard = self.sessions.lock().await;
            let Some(a) = guard.get(session_id) else {
                return;
            };
            (a.connection.clone(), a.acp_session_id.clone())
        };
        // 先让 agent 删除其持久化会话文件（best-effort，5s 超时防卡死）。
        if let (Some(cx), Some(sid)) = (connection, acp_sid) {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                cx.send_request(DeleteSessionRequest::new(sid)).block_task(),
            )
            .await;
        }
        self.kill(session_id).await;
    }

    /// 断开/连接关闭时清空条目里的 WS 事件通道：ACP 回合在连接关闭后仍可能
    /// 存活，保留旧 sender 会让通知处理器 try_send 持续成功而事件无人消费
    /// （更严重的是 reaper 据此刷新活动，误以为会话仍然活跃）。
    ///
    /// 只清本连接自己注册的通道（按 `conn_id` 匹配）：刷新/重连时旧连接的
    /// close 检测可能晚于新连接注册（`ensure_session` 已把 `ws_tx` 换成新连接
    /// 的通道），旧连接 teardown 若无条件置 None 会把新连接的通道一起清掉
    /// → 后续 tool_result/done 帧全部丢弃、前端 running 卡死。
    pub async fn detach_ws_tx(&self, session_id: &str, my_conn_id: u64) {
        if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
            if a.ws_conn_id == my_conn_id {
                a.ws_tx = None;
            }
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

    /// 握手成功后注入 workspace 级 ACP 引擎选项覆盖（`agent_config_overrides`，
    /// JSON map：config_id → value）。先于 [`Self::replay_config_state`] 执行——
    /// session 级 config_state（用户显式选择）回放覆盖 workspace 默认。
    /// config_id 按字典序（`mode` 提前，与回放一致）逐项 set；agent 未暴露的
    /// config_id 或单条失败仅 warn 跳过，不阻断会话建立与其余项注入。
    async fn apply_config_overrides(&self, session_id: &str, workspace: &AgentWorkspaceRecord) {
        let Some(raw) = workspace.agent_config_overrides.as_deref() else {
            return;
        };
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(raw)
        else {
            tracing::warn!(
                session_id,
                "agent_config_overrides not a JSON object, skipped"
            );
            return;
        };
        let mut entries: Vec<(String, String)> = map
            .into_iter()
            .filter_map(|(k, v)| match v.as_str() {
                Some(s) => Some((k, s.to_string())),
                None => {
                    tracing::warn!(
                        session_id,
                        config_id = %k,
                        "agent_config_overrides value not a string, skipped"
                    );
                    None
                }
            })
            .collect();
        entries.sort_by_key(|(k, _)| (if k == "mode" { 0 } else { 1 }, k.clone()));
        for (config_id, value) in entries {
            if let Err(e) = self.set_config_option(session_id, &config_id, &value).await {
                tracing::warn!(session_id, config_id, "apply config override skipped: {e}");
            }
        }
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

    /// 等待会话的 ACP 握手 + 配置注入完成（连接预 spawn 可能在后台进行）。
    /// 已就绪立即返回；超时、会话被移除（spawn 失败/Sender drop）返回 Err
    /// （spawn 失败时透出缓存的真实原因）。
    ///
    /// 统一以 `spawn_ready` watch 为准，不放行于 connection 已写入的瞬时状态：
    /// `connection` 在握手完成时即写回会话条目，而 `spawn_ready` 在
    /// `apply_config_overrides` + `replay_config_state` 全部完成后才置 true。
    /// 若以 connection 存在与否做快路径放行，首条 prompt 会与在途的
    /// `set_config_option` 竞态（workspace overrides/用户 config_state 尚未注入）。
    /// watch 为 true 即「握手 + 配置注入」均已完成的最终状态。
    ///
    /// 超时预算用 `READY_TIMEOUT` 而非 `SPAWN_TIMEOUT`：预 spawn 流水线
    /// （LLM 代理协商 → spawn 协商 → handshake → 配置注入）最坏耗时远超
    /// 30s，等待方必须覆盖整个在途尝试，否则冷启动慢时误报超时。
    pub async fn wait_ready(&self, session_id: &str) -> Result<(), String> {
        let mut rx = {
            let sessions = self.sessions.lock().await;
            match sessions.get(session_id) {
                Some(agent) => agent.spawn_ready.subscribe(),
                None => return Err(self.spawn_failure(session_id).await),
            }
        };
        // 订阅后才检查当前值：避免「subscribe 前已 send(true)」的窗口漏等
        // （重连/多标签页下条目已就绪，subscribe 即取到当前 true 值）。
        if *rx.borrow() {
            return Ok(());
        }
        // wait_for 的返回值借用 rx（Ref<bool>，非 Send）：直接 match 临时值
        // 并在语句结束即 drop，不持借用跨下方 await（否则 WS handler 的
        // future 非 Send）；也不能落局部变量——通配模式不 move，会活到块尾。
        let sender_dropped = match tokio::time::timeout(READY_TIMEOUT, rx.wait_for(|r| *r)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(_)) => true,
            Err(_) => false,
        };
        if sender_dropped {
            Err(self.spawn_failure(session_id).await)
        } else {
            Err(
                "等待 ACP agent 就绪超时：spawn 仍在进行（agent 冷启动或网络较慢），请稍后重试"
                    .to_string(),
            )
        }
    }

    /// 会话不在 spawn 表时的错误描述：优先取最近一次 spawn 失败的真实原因
    /// （预 spawn 失败会移除占位条目并缓存原因；新一轮尝试开始前已清除旧值，
    /// 故此处读到的必属于最近一次已结束的尝试）。
    async fn spawn_failure(&self, session_id: &str) -> String {
        match self.spawn_errors.lock().await.get(session_id) {
            Some(e) => format!("agent spawn failed: {e}"),
            None => "session not spawned".to_string(),
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
            tracing::trace!(
                session_id,
                len = data.len(),
                "acp spawn data stdin=true ignored"
            );
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
                tracing::trace!(
                    session_id,
                    len = data.len(),
                    "acp spawn data routed to pump"
                );
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
            let stream =
                llm_bridge::forward(db, session_id, request_id.clone(), gateway, path, body);
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

/// 从终态回调发起下一条排队 prompt（fire-and-forget，队列 drain）。
///
/// 抽成独立 sync fn：在 async 闭包（`on_receiving_result` 回调）里直接
/// `tokio::spawn(bridge.prompt(...))` 会让 `prompt()` 的 opaque future 因闭包
/// 捕获环境被判定非 Send（编译错误）；独立函数上下文里 `bridge`/`sid`/`next` 是
/// 普通 owned 数据，`tokio::spawn` 正常编译。`prompt()` 本身 fire-and-forget
/// （发完即返回），spawn 后本函数立即返回，不构成同步递归。
fn spawn_drain_next(bridge: AcpBridge, sid: String, next: PendingPrompt) {
    tokio::spawn(async move {
        if let Err(e) = bridge.prompt(&sid, &next.content).await {
            tracing::warn!(session_id = %sid, "drain queued prompt failed: {e}");
        }
    });
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
async fn touch_activity(sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>, sid: &str) -> bool {
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
                let is_thought = frame["thought"].as_bool().unwrap_or(false);
                // 同类型相邻 chunk 合并进当前段（流式分段到达）；正文↔思考切换时
                // 开新段，保住交错顺序——flush 按此落库，刷新后顺序才与对话一致。
                let appended = match a.turn_segments.last_mut() {
                    Some(last) if last.thought == is_thought => {
                        last.content.push_str(content);
                        true
                    }
                    _ => false,
                };
                if !appended {
                    a.turn_segments.push(TurnSegment {
                        thought: is_thought,
                        content: content.to_string(),
                    });
                }
            }
        }
        "tool_call" => {
            // 工具边界 flush：把此前缓冲的 assistant 文本/thought 先落库，再落
            // tool_call 行——DB rowid 顺序 = 对话顺序（文本出现在其调用的工具
            // 之前）。否则中途刷新时 DB 里缺当前工具之前的文本段，前端历史里
            // 这段文本消失（顺序乱）。终态 flush 只冲最后一段，行为不变。
            flush_acp_turn_buffers(db, sessions, sid).await;
            let call = serde_json::json!([{
                "id": frame["id"],
                "name": frame["name"],
                "arguments": frame.get("args").cloned().unwrap_or(serde_json::Value::Null),
                "tool_kind": frame["tool_kind"],
                "diffs": frame.get("diffs").cloned().unwrap_or(serde_json::Value::Null),
                "locations": frame.get("locations").cloned().unwrap_or(serde_json::Value::Null),
            }]);
            let msg_id = format!("{:032x}", rand::random::<u128>());
            // upsert：同一 (session_id, tool_call_id) 收敛为一行，避免每个事件
            // 纯 INSERT 造成刷新后重复卡片。
            if let Err(e) = db
                .agent_upsert_tool_call(
                    &msg_id,
                    sid,
                    frame["id"].as_str().unwrap_or_default(),
                    frame["name"].as_str(),
                    &call.to_string(),
                )
                .await
            {
                tracing::warn!(session_id = %sid, "persist tool_call failed: {e}");
            }
        }
        "tool_result" => {
            let msg_id = format!("{:032x}", rand::random::<u128>());
            // upsert：ToolCallUpdate 中间态（空 result）与终态按同一
            // (session_id, tool_call_id) 收敛，终态覆盖中间态空占位。
            if let Err(e) = db
                .agent_upsert_tool_result(
                    &msg_id,
                    sid,
                    frame["id"].as_str().unwrap_or_default(),
                    frame["name"].as_str(),
                    frame["result"].as_str().unwrap_or(""),
                )
                .await
            {
                tracing::warn!(session_id = %sid, "persist tool_result failed: {e}");
            }
        }
        "plan" => {
            // 同 tool_call：plan 前若有已缓冲文本（ACP 常先出 plan 再出正文，
            // 但顺序不定），先落库保证边界前文本不丢。
            flush_acp_turn_buffers(db, sessions, sid).await;
            let msg_id = format!("{:032x}", rand::random::<u128>());
            let entries = frame["entries"].to_string();
            if let Err(e) = db
                .agent_add_message_v2(
                    &msg_id,
                    sid,
                    "assistant",
                    &entries,
                    None,
                    None,
                    Some("plan"),
                    "message",
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

/// 回合终态：把缓冲的 assistant 输出片段按到达顺序各落一行并清空缓冲。
/// 取消/错误/断线终态同样落已有缓冲（用户能看到的那部分回合过程可追溯）。
/// 注意：必须按 `turn_segments` 顺序落库（思考 → 其后正文），不可先正文后思考
/// ——否则 DB rowid 顺序反了，刷新后历史里思考卡与回复顺序颠倒。
async fn flush_acp_turn_buffers(
    db: &Database,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) {
    let segments = {
        let mut map = sessions.lock().await;
        let Some(a) = map.get_mut(sid) else {
            return;
        };
        std::mem::take(&mut a.turn_segments)
    };
    for seg in segments {
        if seg.content.is_empty() {
            continue;
        }
        let name = seg.thought.then_some("thought");
        let msg_id = format!("{:032x}", rand::random::<u128>());
        if let Err(e) = db
            .agent_add_message_v2(
                &msg_id,
                sid,
                "assistant",
                &seg.content,
                None,
                None,
                name,
                "message",
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

/// ACP `PermissionOption` → 审批卡片透传的轻量 `ApprovalOption`（kind 归一为
/// snake_case 字符串，前端据此渲染按钮样式 / 决定 remember 语义）。
fn permission_option_to_approval(o: &PermissionOption) -> ApprovalOption {
    ApprovalOption {
        id: o.option_id.to_string(),
        label: o.name.clone(),
        kind: match o.kind {
            PermissionOptionKind::AllowOnce => "allow_once".to_string(),
            PermissionOptionKind::AllowAlways => "allow_always".to_string(),
            PermissionOptionKind::RejectOnce => "reject_once".to_string(),
            PermissionOptionKind::RejectAlways => "reject_always".to_string(),
            // non_exhaustive：未来新增 kind 按自定义选项渲染（中性样式）。
            _ => "custom".to_string(),
        },
    }
}

/// 本服务端声明的 ACP 客户端能力：fs 读写经隧道转发到客户端沙箱执行。
/// 不声明则 agent 静默降级（如报「不支持」）。
fn client_capabilities() -> agent_client_protocol::schema::v1::ClientCapabilities {
    agent_client_protocol::schema::v1::ClientCapabilities::new().fs(
        agent_client_protocol::schema::v1::FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(true),
    )
}

/// 把 ACP 的绝对路径转成工作区相对路径。客户端 `resolve_sandboxed` 只接受相对
/// 路径（拒绝绝对路径、拒绝逃逸工作区）；ACP `Read/WriteTextFileRequest.path`
/// 约定为绝对路径，这里剥掉 root_path 前缀。路径在工作区外 → Err。
fn to_workspace_relative(root_path: &str, abs_path: &str) -> Result<String, String> {
    let root = Path::new(root_path);
    let abs = Path::new(abs_path);
    if !abs.is_absolute() {
        return Err(format!("fs request path must be absolute: {abs_path}"));
    }
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| format!("fs request path is outside workspace root: {abs_path}"))?;
    if rel.as_os_str().is_empty() {
        return Err("fs request path is the workspace root itself".into());
    }
    Ok(rel.to_string_lossy().to_string())
}

/// fs 请求的公共上下文：session → workspace（root_path / docker）→ 活跃进程 client_id。
struct FsContext {
    client_id: String,
    root_path: String,
    docker_container: Option<String>,
}

/// 解析 fs 请求上下文；任一环节缺失报 Err（session 未建 / 进程未 spawn / DB 无记录）。
async fn fs_context(
    db: &Database,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) -> Result<FsContext, String> {
    let session = db
        .agent_get_session(sid)
        .await
        .map_err(|e| format!("session lookup failed: {e}"))?
        .ok_or_else(|| "session not found".to_string())?;
    let ws = db
        .agent_get_workspace(&session.workspace_id)
        .await
        .map_err(|e| format!("workspace lookup failed: {e}"))?
        .ok_or_else(|| "workspace not found".to_string())?;
    let client_id = {
        let sessions = sessions.lock().await;
        sessions
            .get(sid)
            .map(|a| a.client_id.clone())
            .ok_or_else(|| "session not spawned".to_string())?
    };
    Ok(FsContext {
        client_id,
        root_path: ws.root_path,
        docker_container: ws.docker_container_id,
    })
}

/// 执行 `fs/read_text_file`：绝对路径 → 工作区相对路径 → 经隧道转发到客户端
/// 沙箱读取，返回文本内容。
async fn exec_fs_read(
    db: &Database,
    spawner: &AgentSpawner,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
    abs_path: &str,
) -> Result<String, String> {
    let ctx = fs_context(db, sessions, sid).await?;
    let rel = to_workspace_relative(&ctx.root_path, abs_path)?;
    let request_id = format!("{:032x}", rand::random::<u128>());
    let result = spawner
        .agent_exec(
            &ctx.client_id,
            &request_id,
            sid,
            &ctx.root_path,
            ctx.docker_container.as_deref(),
            rust_tunnel_common::AgentCommand::ReadFile { path: rel },
            Duration::from_secs(120),
        )
        .await
        .map_err(|e| format!("tunnel execution failed: {e}"))?;
    match result {
        rust_tunnel_common::AgentResult::FileContent { content } => Ok(content),
        rust_tunnel_common::AgentResult::Error { message } => Err(message),
        other => Err(format!("unexpected read result: {other:?}")),
    }
}

/// 执行 `fs/write_text_file`：同 read，写文件到客户端沙箱。
async fn exec_fs_write(
    db: &Database,
    spawner: &AgentSpawner,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
    abs_path: &str,
    content: &str,
) -> Result<(), String> {
    let ctx = fs_context(db, sessions, sid).await?;
    let rel = to_workspace_relative(&ctx.root_path, abs_path)?;
    let request_id = format!("{:032x}", rand::random::<u128>());
    let result = spawner
        .agent_exec(
            &ctx.client_id,
            &request_id,
            sid,
            &ctx.root_path,
            ctx.docker_container.as_deref(),
            rust_tunnel_common::AgentCommand::WriteFile {
                path: rel,
                content: content.to_string(),
            },
            Duration::from_secs(120),
        )
        .await
        .map_err(|e| format!("tunnel execution failed: {e}"))?;
    match result {
        rust_tunnel_common::AgentResult::Success => Ok(()),
        rust_tunnel_common::AgentResult::Error { message } => Err(message),
        other => Err(format!("unexpected write result: {other:?}")),
    }
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

    /// 测试固定连接 id：setup_handshake 注册的 ws_tx 属此连接，detach 用例
    /// 用同一 id 验证「自己 detach 才清空」。
    const TEST_CONN_ID: u64 = 42;

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
            agent_config_overrides: None,
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
            ws_conn_id: 0,
            busy: false,
            cancelled_turns: std::collections::HashSet::new(),
            turn_generation: 0,
            last_activity: std::time::Instant::now(),
            exited: false,
            turn_segments: Vec::new(),
            config_options: Vec::new(),
            spawn_ready: watch::channel(false).0,
            pending_prompts: VecDeque::new(),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// 装配 mock agent（duplex → pump → mock_acp_agent）并完成 ACP handshake。
    /// `ws_tx` 注册为会话条目的初始事件通道；连接任务的通知处理器此后每次事件
    /// 从条目动态解析通道（见 `current_ws_tx`）。
    async fn setup_handshake(bridge: &AcpBridge, ws_tx: mpsc::Sender<serde_json::Value>) {
        setup_handshake_with(
            bridge,
            ws_tx,
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            None,
            None,
            None,
            false,
        )
        .await;
    }

    /// `setup_handshake` 的参数化版本：`config_options` 注入 `session/new` 响应
    /// 的 `configOptions`（空数组 = 无配置项），`applied` 记录收到的
    /// `session/set_config_option` 调用（config_id, value）。
    ///
    /// `prompt_permits`（None 立即响应）：mock 收到 `session/prompt` 后先回流式
    /// 通知，再等待一个许可才回 PromptResponse——队列/取消测试需要精确控制「回合
    /// 何时结束」。`recorded`（None 不记录）：收集 mock 收到的 method/通知名
    /// （如 `session/cancel`），供断言。
    ///
    /// `persisted_id`：传给 `acp_handshake` 的持久化 ACP session id（resume 测试
    /// 用；None = 全新会话路径）。`resume_fails`：true 时 mock 的 `session/resume`
    /// 回 error（测回退 session/new）。
    #[allow(clippy::too_many_arguments)]
    async fn setup_handshake_with(
        bridge: &AcpBridge,
        ws_tx: mpsc::Sender<serde_json::Value>,
        config_options: serde_json::Value,
        applied: Arc<Mutex<Vec<(String, String)>>>,
        prompt_permits: Option<mpsc::Receiver<()>>,
        recorded: Option<Arc<Mutex<Vec<String>>>>,
        persisted_id: Option<&str>,
        resume_fails: bool,
    ) {
        let (agent_io, pump_io) = tokio::io::duplex(64 * 1024);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
        let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(32);

        let mut agent = spawned_agent();
        agent.agent_io = Some(agent_io);
        agent.stdout_tx = Some(stdout_tx.clone());
        agent.ws_tx = Some(ws_tx.clone());
        agent.ws_conn_id = TEST_CONN_ID;
        bridge.sessions.lock().await.insert("sess-1".into(), agent);

        tokio::spawn(run_stdio_pump(
            pump_io,
            stdout_rx,
            control_tx,
            "sess-1".into(),
        ));
        tokio::spawn(mock_acp_agent(
            control_rx,
            stdout_tx,
            config_options,
            applied,
            prompt_permits,
            recorded,
            resume_fails,
        ));

        bridge
            .acp_handshake("sess-1", "/mock", persisted_id.map(str::to_string))
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
                    // AgentLlmProxyStop 是单向清理消息（无响应、无等待者），跳过不协商。
                    ControlMessage::AgentLlmProxyStop { .. } => continue,
                    other => panic!("unexpected request: {other:?}"),
                };
                registry2.resolve_spawn_pending(&sid, respond(req)).await;
            }
        });
        AcpBridge::new(AgentSpawner::new(registry), db)
    }

    #[test]
    fn test_client_capabilities_declare_fs() {
        // fs 能力必须声明：agent 才走 fs/read_text_file 而非静默报「不支持」。
        let caps = client_capabilities();
        assert!(caps.fs.read_text_file);
        assert!(caps.fs.write_text_file);
    }

    #[test]
    fn test_to_workspace_relative() {
        assert_eq!(
            to_workspace_relative("/ws", "/ws/a/b.txt").unwrap(),
            "a/b.txt"
        );
        assert_eq!(to_workspace_relative("/ws", "/ws/a.txt").unwrap(), "a.txt");
        // 工作区外 / 非绝对 / 根目录自身 → Err
        assert!(to_workspace_relative("/ws", "/etc/passwd").is_err());
        assert!(to_workspace_relative("/ws", "a/b.txt").is_err());
        assert!(to_workspace_relative("/ws", "/ws").is_err());
        // 前缀歧义：/wsx 不在 /ws 下
        assert!(to_workspace_relative("/ws", "/wsx/a").is_err());
    }

    /// 装配 fs 测试环境：内存 DB（workspace `/ws` + session）、注册客户端（spawn loop
    /// 应答 AgentExecRequest 返回固定结果）、活跃会话条目（client_id=nas）。
    async fn fs_test_env(
        exec_result: rust_tunnel_common::AgentResult,
    ) -> (
        Database,
        AgentSpawner,
        Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    ) {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/ws", None, None, "gemini", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let (tx, mut rx) = mpsc::channel(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let registry2 = registry.clone();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let ControlMessage::AgentExecRequest { request_id, .. } = req else {
                    panic!("unexpected request: {req:?}");
                };
                registry2
                    .deliver_agent_response("nas", &request_id, exec_result.clone())
                    .await;
            }
        });
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let mut agent = spawned_agent();
        agent.client_id = "nas".into();
        sessions.lock().await.insert("sess-1".into(), agent);
        (db, AgentSpawner::new(registry), sessions)
    }

    #[tokio::test]
    async fn test_exec_fs_read_forwards_via_tunnel() {
        // 绝对路径 → 相对路径 → 客户端返回 FileContent → 回包给 agent。
        let (db, spawner, sessions) = fs_test_env(rust_tunnel_common::AgentResult::FileContent {
            content: "hello fs".into(),
        })
        .await;
        let content = exec_fs_read(&db, &spawner, &sessions, "sess-1", "/ws/src/main.rs")
            .await
            .expect("read should succeed");
        assert_eq!(content, "hello fs");
    }

    #[tokio::test]
    async fn test_exec_fs_read_rejects_outside_workspace() {
        let (db, spawner, sessions) = fs_test_env(rust_tunnel_common::AgentResult::Success).await;
        let err = exec_fs_read(&db, &spawner, &sessions, "sess-1", "/etc/passwd")
            .await
            .expect_err("outside workspace should be rejected");
        assert!(err.contains("outside workspace"), "err: {err}");
    }

    #[tokio::test]
    async fn test_exec_fs_read_propagates_client_error() {
        let (db, spawner, sessions) = fs_test_env(rust_tunnel_common::AgentResult::Error {
            message: "no such file".into(),
        })
        .await;
        let err = exec_fs_read(&db, &spawner, &sessions, "sess-1", "/ws/missing.txt")
            .await
            .expect_err("client error should propagate");
        assert_eq!(err, "no such file");
    }

    #[tokio::test]
    async fn test_exec_fs_write_forwards_via_tunnel() {
        let (db, spawner, sessions) = fs_test_env(rust_tunnel_common::AgentResult::Success).await;
        exec_fs_write(&db, &spawner, &sessions, "sess-1", "/ws/a.txt", "hi")
            .await
            .expect("write should succeed");
    }

    #[tokio::test]
    async fn test_exec_fs_session_not_spawned_errors() {
        // 会话条目缺失（进程未 spawn）：在构造 AgentCommand 前即报错，不触发隧道请求。
        let (db, spawner, _sessions) = fs_test_env(rust_tunnel_common::AgentResult::Success).await;
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let err = exec_fs_read(&db, &spawner, &sessions, "sess-1", "/ws/a.txt")
            .await
            .expect_err("missing session entry should error");
        assert_eq!(err, "session not spawned");
    }

    #[tokio::test]
    async fn test_ensure_session_docker_rejected() {
        let bridge = mock_bridge(|_| unreachable!("docker rejection should not spawn")).await;
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &docker_workspace(), ws_tx, TEST_CONN_ID)
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
            .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
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
        bridge.sessions.lock().await.insert("sess-1".into(), exited);
        assert!(!bridge.session_spawned("sess-1").await);
    }

    #[tokio::test]
    async fn test_ensure_session_session_model_passes_gate() {
        // session.model 已配置、workspace 未配 llm_model_id → 门禁放行，进入 spawn
        // 路径（此处 LLM 代理绑定失败 → 错误是 bind，而非「未配置」）。
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        db.agent_create_workspace(
            "w1",
            "proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "gemini",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
                    // AgentLlmProxyStop 是单向清理消息（无响应、无等待者），跳过不协商。
                    ControlMessage::AgentLlmProxyStop { .. } => continue,
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
            .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut ws = acp_workspace();
        ws.client_id = "ghost".into();
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
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
            .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
            .await
            .expect_err("unsupported agent type should fail locally");
        assert!(err.contains("unsupported agent type"), "err: {err}");
    }

    #[tokio::test]
    async fn test_wait_ready_reports_cached_spawn_failure() {
        // 预 spawn 失败会移除占位条目：后到/在等的 wait_ready 必须拿到缓存的
        // 真实原因，而非误导性的 "session not spawned"。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut ws = acp_workspace();
        ws.client_id = "ghost".into();
        let (ws_tx, _rx) = mpsc::channel(16);
        let _ = bridge.ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID).await;

        let err = bridge
            .wait_ready("sess-1")
            .await
            .expect_err("failed spawn should surface via wait_ready");
        assert!(
            err.contains("llm proxy start failed"),
            "real spawn error should propagate, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_wait_ready_unknown_session_reports_not_spawned() {
        // 从未尝试过 spawn 的会话：无失败缓存，保持 "session not spawned"。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let err = bridge
            .wait_ready("sess-x")
            .await
            .expect_err("unknown session should error");
        assert_eq!(err, "session not spawned");
    }

    #[tokio::test]
    async fn test_wait_ready_retry_clears_stale_failure() {
        // 失败缓存不得污染新一轮尝试：重试（新占位插入）清旧值后，新一轮
        // 在途期间条目缺失的极端窗口不应再报旧错误。这里验证重试失败后
        // 缓存被新错误覆盖（而非残留首次错误）。
        let bridge = mock_bridge(|req| match req {
            ControlMessage::AgentLlmProxyStart { session_id } => {
                ControlMessage::AgentLlmProxyReady {
                    session_id,
                    port: 0, // 绑定失败：每次 spawn 都以相同原因失败
                }
            }
            other => panic!("unexpected request: {other:?}"),
        })
        .await;
        let ws = acp_workspace();
        for _ in 0..2 {
            let (ws_tx, _rx) = mpsc::channel(16);
            let _ = bridge.ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID).await;
        }
        let err = bridge
            .wait_ready("sess-1")
            .await
            .expect_err("failed spawn should surface via wait_ready");
        assert!(
            err.contains("failed to bind"),
            "latest failure should be cached, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_wait_ready_visible_when_ready_set_before_subscribe() {
        // 回归：spawn 在首个 wait_ready（首个 subscribe）之前完成时，就绪置位
        // 必须对后到订阅者可见。spawn_ready 建通道只存 Sender（Receiver 当场
        // drop），watch::Sender::send 在无接收者时静默失败且**不写入新值**——
        // 预 spawn 快于首条消息时 wait_ready 曾空等整个 READY_TIMEOUT，误报
        // 「spawn 仍在进行」。ensure_session 成功路径必须用 send_modify 置位。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let agent = spawned_agent();
        let sender = agent.spawn_ready.clone();
        bridge.sessions.lock().await.insert("sess-1".into(), agent);
        // 无任何订阅者时置位（等价 ensure_session 成功路径的 send_modify）
        sender.send_modify(|ready| *ready = true);
        tokio::time::timeout(Duration::from_secs(2), bridge.wait_ready("sess-1"))
            .await
            .expect("wait_ready must not time out")
            .expect("ready set before subscribe must be visible");
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
            .ensure_session("sess-1", &acp_workspace(), ws_tx, TEST_CONN_ID)
            .await
            .expect("dedup should return Ok");
    }

    #[tokio::test]
    async fn test_ensure_session_exited_entry_respawns() {
        // 回归：exited=true 的陈旧条目不能短路 dedup。用离线客户端让 spawn
        // 路径快速失败——若 ensure_session 直接 Ok（bug 行为）则 expect_err panic。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut exited = spawned_agent();
        exited.exited = true;
        bridge.sessions.lock().await.insert("sess-1".into(), exited);
        let mut ws = acp_workspace();
        ws.client_id = "ghost".into();
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut exited = spawned_agent();
        exited.exited = true;
        bridge.sessions.lock().await.insert("sess-1".into(), exited);
        let err = bridge
            .prompt("sess-1", "hi")
            .await
            .expect_err("exited session should error");
        assert!(err.contains("exited"), "err: {err}");
    }

    #[tokio::test]
    async fn test_handle_spawn_exit_marks_exited() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
        tokio::spawn(run_stdio_pump(
            pump_io,
            stdout_rx,
            control_tx,
            "sess-1".into(),
        ));

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
        tokio::spawn(run_stdio_pump(
            pump_io,
            stdout_rx,
            control_tx,
            "sess-1".into(),
        ));

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
        let task = tokio::spawn(run_stdio_pump(
            pump_io,
            stdout_rx,
            control_tx,
            "sess-1".into(),
        ));
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut agent = spawned_agent();
        agent.busy = true;
        bridge.sessions.lock().await.insert("sess-1".into(), agent);
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge.cancel("no-such-session").await; // 不 panic
    }

    #[tokio::test]
    async fn test_kill_removes_entry() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
    /// （→ pump → ACP crate）。`config_options` 注入 `session/new` 响应的
    /// `configOptions`；`applied` 记录收到的 `session/set_config_option`
    /// 调用（config_id, value）——value 形态以 ACP 实际序列化为准（select 为
    /// 裸字符串，boolean 为 bool + 顶层 type）。
    ///
    /// `prompt_permits`（None 立即响应）：mock 收到 `session/prompt` 后先回流式
    /// 通知，再等待一个许可才回 PromptResponse——队列/取消测试用。
    /// `recorded`（None 不记录）：收集收到的 method/通知名（如 `session/cancel`）。
    /// `resume_fails`：true 时 `session/resume` 回 JSON-RPC error（测回退 session/new）。
    #[allow(clippy::too_many_arguments)]
    async fn mock_acp_agent(
        mut stdin_rx: mpsc::Receiver<ControlMessage>,
        stdout_tx: mpsc::Sender<Vec<u8>>,
        config_options: serde_json::Value,
        applied: Arc<Mutex<Vec<(String, String)>>>,
        mut prompt_permits: Option<mpsc::Receiver<()>>,
        recorded: Option<Arc<Mutex<Vec<String>>>>,
        resume_fails: bool,
    ) {
        let mut buf = String::new();
        while let Some(msg) = stdin_rx.recv().await {
            let ControlMessage::AgentSpawnData {
                data, stdin: true, ..
            } = msg
            else {
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
                if let Some(r) = &recorded {
                    r.lock().await.push(method.clone());
                }
                let id = json.get("id").cloned().unwrap_or(serde_json::Value::Null);
                // 单条请求可能产出多条输出行（prompt：通知 + 响应）。
                let mut out_lines: Vec<serde_json::Value> = Vec::new();
                match method.as_str() {
                    "initialize" => {
                        // 声明 loadSession + session/resume/delete 能力，与
                        // claude-agent-acp 对齐（resume 测试依赖它）。
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {
                                "protocolVersion": 1,
                                "agentCapabilities": {
                                    "loadSession": true,
                                    "sessionCapabilities": { "resume": {}, "delete": {} }
                                }
                            }
                        }));
                    }
                    "session/new" => {
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "sessionId": "acp-1", "configOptions": config_options }
                        }));
                    }
                    "session/resume" => {
                        // 成功回显请求的 sessionId；失败回 JSON-RPC error
                        // （acp_handshake 据此回退 session/new）。
                        if resume_fails {
                            out_lines.push(serde_json::json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": { "code": -32601, "message": "session not found" }
                            }));
                        } else {
                            let req_sid = json
                                .pointer("/params/sessionId")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string();
                            out_lines.push(serde_json::json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": { "sessionId": req_sid, "configOptions": config_options }
                            }));
                        }
                    }
                    "session/delete" => {
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {}
                        }));
                    }
                    "session/set_config_option" => {
                        let params = json.get("params").cloned().unwrap_or_default();
                        let config_id = params
                            .get("configId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // ACP 实际序列化：select 的 value 是裸字符串（"sonnet"）；
                        // boolean 是 bool（{"type":"boolean","value":true} 平铺到
                        // params 顶层）。响应必须带 configOptions 字段（schema 的
                        // SetSessionConfigOptionResponse 必填，缺则反序列化报错）。
                        let value = params.get("value").cloned().unwrap_or_default();
                        let value_str = value
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| value.to_string());
                        applied.lock().await.push((config_id, value_str));
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "configOptions": config_options }
                        }));
                    }
                    "session/prompt" => {
                        // 流式通知：先发（回合立即进入 running 状态，前端可见）。
                        let stream_lines = vec![
                            serde_json::json!({
                                "jsonrpc": "2.0", "method": "session/update",
                                "params": { "sessionId": "acp-1",
                                    "update": { "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": "hello from mock" } } }
                            }),
                            serde_json::json!({
                                "jsonrpc": "2.0", "method": "session/update",
                                "params": { "sessionId": "acp-1",
                                    "update": { "sessionUpdate": "tool_call", "toolCallId": "call_1",
                                        "title": "shell", "status": "completed",
                                        "rawInput": { "cmd": "ls" } } }
                            }),
                            // ToolCallUpdate 不带 title：验证 name 从前序 ToolCall 缓存补
                            serde_json::json!({
                                "jsonrpc": "2.0", "method": "session/update",
                                "params": { "sessionId": "acp-1",
                                    "update": { "sessionUpdate": "tool_call_update",
                                        "toolCallId": "call_1", "status": "completed",
                                        "rawOutput": "a.rs" } }
                            }),
                            // plan + thought：验证新事件类型的落库
                            serde_json::json!({
                                "jsonrpc": "2.0", "method": "session/update",
                                "params": { "sessionId": "acp-1",
                                    "update": { "sessionUpdate": "agent_thought_chunk",
                                        "content": { "type": "text", "text": "思考一下" } } }
                            }),
                            serde_json::json!({
                                "jsonrpc": "2.0", "method": "session/update",
                                "params": { "sessionId": "acp-1",
                                    "update": { "sessionUpdate": "plan",
                                        "entries": [ { "content": "步骤一", "priority": "high",
                                            "status": "in_progress" } ] } }
                            }),
                        ];
                        for line_value in &stream_lines {
                            let mut bytes = serde_json::to_vec(line_value).unwrap();
                            bytes.push(b'\n');
                            if stdout_tx.send(bytes).await.is_err() {
                                return;
                            }
                        }
                        // 响应 gate：None 立即回；Some 等待一个许可（队列/取消测试
                        // 需要精确控制「回合何时结束」）。等待期间回合保持 busy。
                        if let Some(permits) = &mut prompt_permits {
                            let _ = permits.recv().await;
                        }
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "stopReason": "end_turn" }
                        });
                        let mut bytes = serde_json::to_vec(&resp).unwrap();
                        bytes.push(b'\n');
                        if stdout_tx.send(bytes).await.is_err() {
                            return;
                        }
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
            assert_eq!(agent.acp_session_id.as_ref().unwrap().0.as_ref(), "acp-1");
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
        assert_eq!(
            events[2]["name"], "shell",
            "name should be cached from ToolCall"
        );
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
        // B 用独立 conn_id（旧连接 A 是 TEST_CONN_ID），验证事件按通道切到 B。
        let (ws_tx_b, mut ws_rx_b) = mpsc::channel::<serde_json::Value>(16);
        bridge
            .ensure_session("sess-1", &acp_workspace(), ws_tx_b, TEST_CONN_ID + 1)
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
        let stale =
            tokio::time::timeout(std::time::Duration::from_millis(200), ws_rx_a.recv()).await;
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
        let terminal =
            tokio::time::timeout(std::time::Duration::from_millis(300), ws_rx.recv()).await;
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
            "w1",
            "proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "gemini",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
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
        // assistant 文本：在 plan/tool 边界 flush 成一行（不再攒到终态），
        // 保证 DB rowid 顺序 = 对话顺序——文本行必须排在 tool_calls 行之前，
        // 刷新后历史里正文才出现在其调用的工具之前。
        let texts: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == "message" && r.name.is_none())
            .collect();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].content, "hello from mock");
        let first_text = rows
            .iter()
            .position(|r| r.kind == "message" && r.name.is_none())
            .unwrap();
        let first_call = rows.iter().position(|r| r.kind == "tool_calls").unwrap();
        assert!(
            first_text < first_call,
            "text row should precede tool_calls row (boundary flush): {rows:?}"
        );
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
    async fn test_flush_preserves_thought_before_text() {
        // 回归：思考→回复 的回合（无工具边界，done 一次性 flush）落库顺序必须
        // 保持 thought 在正文之前。旧实现 text_buf/thought_buf 独立缓冲 + flush
        // 硬编码先正文后思考，DB rowid 反了 → 刷新后思考卡与回复顺序颠倒。
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1",
            "proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "gemini",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions
            .lock()
            .await
            .insert("sess-1".into(), spawned_agent());

        // thought chunk 先到达、正文后到达（真实 ACP 回合顺序），done 时才 flush
        for (content, thought) in [("先思考", true), ("再回复", false)] {
            persist_acp_frame(
                &db,
                &sessions,
                "sess-1",
                &serde_json::json!({
                    "type": "assistant_chunk",
                    "content": content,
                    "thought": thought,
                }),
            )
            .await;
        }
        flush_acp_turn_buffers(&db, &sessions, "sess-1").await;

        let rows = db.agent_list_messages("sess-1").await.unwrap();
        let thoughts: Vec<_> = rows
            .iter()
            .filter(|r| r.name.as_deref() == Some("thought"))
            .collect();
        let texts: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == "message" && r.name.is_none())
            .collect();
        assert_eq!(thoughts.len(), 1);
        assert_eq!(thoughts[0].content, "先思考");
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].content, "再回复");
        // rowid 顺序 = 对话顺序：思考行必须先于正文行
        let thought_pos = rows
            .iter()
            .position(|r| r.name.as_deref() == Some("thought"))
            .unwrap();
        let text_pos = rows
            .iter()
            .position(|r| r.kind == "message" && r.name.is_none())
            .unwrap();
        assert!(
            thought_pos < text_pos,
            "thought must precede text: {rows:?}"
        );
    }

    #[tokio::test]
    async fn test_flush_preserves_interleaved_thought_text() {
        // 交错变体：正文先出、再思考、再正文（无工具边界）。每段独立落行，
        // 顺序严格按到达顺序保持，不能按类型归并重排。
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1",
            "proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "gemini",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-2", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions
            .lock()
            .await
            .insert("sess-2".into(), spawned_agent());

        for (content, thought) in [("正文一", false), ("思考", true), ("正文二", false)] {
            persist_acp_frame(
                &db,
                &sessions,
                "sess-2",
                &serde_json::json!({
                    "type": "assistant_chunk",
                    "content": content,
                    "thought": thought,
                }),
            )
            .await;
        }
        flush_acp_turn_buffers(&db, &sessions, "sess-2").await;

        let rows = db.agent_list_messages("sess-2").await.unwrap();
        let kinds: Vec<(bool, String)> = rows
            .iter()
            .filter(|r| r.kind == "message")
            .map(|r| (r.name.as_deref() == Some("thought"), r.content.clone()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (false, "正文一".to_string()),
                (true, "思考".to_string()),
                (false, "正文二".to_string()),
            ],
            "rows must keep arrival order: {rows:?}"
        );
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
            "w1",
            "proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "gemini",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db.clone());

        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake(&bridge, ws_tx.clone()).await;
        // 真实断线：清空条目 WS 通道（本连接自己的通道，sender 通道身份匹配）
        bridge.detach_ws_tx("sess-1", TEST_CONN_ID).await;
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

    #[tokio::test]
    async fn test_detach_ws_tx_only_clears_own_connection() {
        // 刷新/重连竞态：旧连接 close 检测晚于新连接注册（ensure_session 已把
        // ws_tx 换成新连接的 sender）。旧连接 teardown 必须只清自己的通道，否则
        // 新连接后续 tool_result/done 全部丢弃（前端 running 卡死）。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let (tx, _rx) = mpsc::channel::<serde_json::Value>(16);
        const OLD: u64 = 1;
        const NEW: u64 = 2;

        // 旧连接独占通道（ws_conn_id=OLD）：其 teardown 应清空
        let mut a = spawned_agent();
        a.ws_tx = Some(tx.clone());
        a.ws_conn_id = OLD;
        bridge.sessions.lock().await.insert("sess-1".into(), a);
        bridge.detach_ws_tx("sess-1", OLD).await;
        assert!(
            bridge.sessions.lock().await.get("sess-1").unwrap().ws_tx.is_none(),
            "own detach should clear ws_tx"
        );

        // 新连接已注册（ws_conn_id=NEW）：旧连接晚到的 teardown 不得清掉它
        let mut a = spawned_agent();
        a.ws_tx = Some(tx.clone());
        a.ws_conn_id = NEW;
        bridge.sessions.lock().await.insert("sess-1".into(), a);
        bridge.detach_ws_tx("sess-1", OLD).await;
        assert!(
            bridge.sessions.lock().await.get("sess-1").unwrap().ws_tx.is_some(),
            "old connection teardown must not clear newer connection's ws_tx"
        );
        // 新连接自己的 teardown 仍能清空
        bridge.detach_ws_tx("sess-1", NEW).await;
        assert!(bridge.sessions.lock().await.get("sess-1").unwrap().ws_tx.is_none());
    }

    // ── workspace 级 config overrides 注入 ──────────────────────

    /// 握手后按 workspace.agent_config_overrides 注入；config_state 回放其后
    /// （用户显式选择覆盖 workspace 默认）。快照中不存在的 config_id 跳过不报错。
    #[tokio::test]
    async fn test_apply_config_overrides_on_handshake() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        db.agent_create_workspace(
            "w1",
            "proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "claude-code",
            None,
            Some("model-1"),
            Some(r#"{"model":"sonnet","fast":"haiku","nonexistent":"x"}"#),
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, None)
            .await
            .unwrap();
        // session 级 config_state：用户显式把 model 改为 opus —— 必须覆盖 workspace 注入
        db.agent_update_session_config_state("sess-1", "model", Some("opus"))
            .await
            .unwrap();

        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db.clone());

        let applied = Arc::new(Mutex::new(Vec::new()));
        let options = serde_json::json!([
            {"id": "model", "name": "Model", "type": "select",
             "currentValue": "sonnet",
             "options": [{"value": "sonnet", "name": "Sonnet"}, {"value": "opus", "name": "Opus"}]},
            {"id": "fast", "name": "Fast model", "type": "select",
             "currentValue": "haiku",
             "options": [{"value": "haiku", "name": "Haiku"}]}
        ]);
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake_with(&bridge, ws_tx, options, applied.clone(), None, None, None, false)
            .await;

        // workspace 注入：fast → haiku；model 先被 workspace 设为 sonnet，随后
        // config_state 回放覆盖为 opus；nonexistent 不在快照中 → 跳过
        bridge
            .apply_config_overrides(
                "sess-1",
                &db.agent_get_workspace("w1").await.unwrap().unwrap(),
            )
            .await;
        bridge.replay_config_state("sess-1").await;

        let calls = applied.lock().await.clone();
        // 顺序：workspace 按 config_id 字典序（fast 先于 model）注入，回放其后
        assert_eq!(
            calls,
            vec![
                ("fast".to_string(), "haiku".to_string()),
                ("model".to_string(), "sonnet".to_string()),
                ("model".to_string(), "opus".to_string()),
            ]
        );
    }

    /// workspace 未配置 overrides（None / 非法 JSON / 空对象）→ 不发任何
    /// set_config_option，不报错。
    #[tokio::test]
    async fn test_apply_config_overrides_noop_when_unset() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let options = serde_json::json!([
            {"id": "model", "name": "Model", "type": "select",
             "currentValue": "sonnet",
             "options": [{"value": "sonnet", "name": "Sonnet"}]}
        ]);

        for (label, overrides) in [
            ("none", None),
            ("not-json", Some("not-json")),
            ("empty-object", Some("{}")),
        ] {
            let ws = AgentWorkspaceRecord {
                agent_config_overrides: overrides.map(str::to_string),
                ..acp_workspace()
            };
            let applied = Arc::new(Mutex::new(Vec::new()));
            let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
            setup_handshake_with(
                &bridge, ws_tx, options.clone(), applied.clone(), None, None, None, false,
            )
            .await;
            bridge.apply_config_overrides("sess-1", &ws).await;
            assert!(
                applied.lock().await.is_empty(),
                "{label}: apply_config_overrides should be a no-op"
            );
        }
    }

    // ── submit_prompt 排队 + 优雅取消/兜底杀进程 ────────────────

    /// 注册客户端并观察 AgentExecCancel（优雅取消测试断言「未杀进程」，
    /// 兜底测试断言「已杀进程」）。
    async fn register_cancel_observer(
        registry: &crate::client_registry::ClientRegistry,
    ) -> Arc<Mutex<Vec<String>>> {
        let (client_tx, mut client_rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", client_tx)
            .await
            .unwrap();
        let cancels = Arc::new(Mutex::new(Vec::<String>::new()));
        let observer = cancels.clone();
        tokio::spawn(async move {
            while let Some(msg) = client_rx.recv().await {
                if let ControlMessage::AgentExecCancel { request_id } = msg {
                    observer.lock().await.push(request_id);
                }
            }
        });
        cancels
    }

    /// 轮询 `cond`（async 闭包）直到返回 true（超时 panic）。
    async fn wait_until<F, Fut>(timeout: Duration, mut cond: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + timeout;
        while !cond().await {
            assert!(
                std::time::Instant::now() < deadline,
                "condition not met within {timeout:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// busy 时 submit_prompt 入队并推 queued 帧；当前回合终态后自动发下一条；
    /// done 只在队列排空后发（回合连续）。
    #[tokio::test]
    async fn test_submit_prompt_queues_when_busy_and_drains() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(32);
        let (permit_tx, permit_rx) = mpsc::channel::<()>(16);
        setup_handshake_with(
            &bridge,
            ws_tx.clone(),
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            Some(permit_rx),
            None,
            None,
            false,
        )
        .await;

        // 第一条：空闲 → 直接跑（busy）
        bridge
            .submit_prompt("sess-1", "first", vec![])
            .await
            .expect("first prompt accepted");
        assert!(bridge.sessions.lock().await.get("sess-1").unwrap().busy);

        // 第二条：busy → 排队 + queued 帧
        bridge
            .submit_prompt("sess-1", "second", vec![])
            .await
            .expect("second prompt queued");
        assert_eq!(
            bridge
                .sessions
                .lock()
                .await
                .get("sess-1")
                .unwrap()
                .pending_prompts
                .len(),
            1,
            "busy prompt should be queued"
        );

        // 放行第一条 → 终态回调 drain → 自动发第二条
        permit_tx.send(()).await.unwrap();

        let mut events = Vec::new();
        let mut text_chunks = 0;
        let mut sent_second_permit = false;
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), ws_rx.recv())
                .await
                .expect("timed out waiting for events")
                .expect("ws channel closed");
            // mock 每回合发 1 个正文 chunk（thought 也是 assistant_chunk 类型，
            // 用 thought 字段区分，只数正文用于判定「下一回合已开跑」）
            if ev["type"] == "assistant_chunk"
                && !ev.get("thought").and_then(serde_json::Value::as_bool).unwrap_or(false)
            {
                text_chunks += 1;
                if text_chunks >= 2 && !sent_second_permit {
                    // 第二条已开跑：放行其 PromptResponse
                    sent_second_permit = true;
                    permit_tx.send(()).await.unwrap();
                }
            }
            let done = ev["type"] == "done";
            events.push(ev);
            if done {
                break;
            }
        }
        assert!(sent_second_permit, "queued prompt should auto-send after first turn");
        assert_eq!(text_chunks, 2, "both turns should stream");
        let queued = events.iter().filter(|e| e["type"] == "queued").count();
        assert_eq!(queued, 1, "busy queue should push a queued frame");
        assert_eq!(
            events.last().unwrap()["type"], "done",
            "done only after the queue drains"
        );
        assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
    }

    /// 优雅取消：收到 session/cancel、无 AgentExecCancel、busy 保持到
    /// PromptResponse 到达才复位，取消回合不发生产者终态帧。
    #[tokio::test]
    async fn test_cancel_graceful_keeps_process_and_busy() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let cancels = register_cancel_observer(&registry).await;
        // 默认 cancel_grace（10s）：测试期间不会触发兜底杀进程
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(32);
        let (permit_tx, permit_rx) = mpsc::channel::<()>(16);
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded2 = recorded.clone();
        setup_handshake_with(
            &bridge,
            ws_tx.clone(),
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            Some(permit_rx),
            Some(recorded2),
            None,
            false,
        )
        .await;

        bridge
            .prompt("sess-1", "hello")
            .await
            .expect("prompt should send");
        // 消费 mock 的流式通知（assistant_chunk/tool_call/tool_result/thought/plan）
        for _ in 0..5 {
            tokio::time::timeout(Duration::from_secs(5), ws_rx.recv())
                .await
                .expect("timed out waiting for stream")
                .expect("closed");
        }

        bridge.cancel("sess-1").await;
        assert!(
            bridge.sessions.lock().await.get("sess-1").unwrap().busy,
            "graceful cancel keeps busy until PromptResponse arrives"
        );
        // 放行 PromptResponse → 终态回调清 busy。
        // 注意：mock 逐行处理，session/cancel 通知排在 prompt 响应之后才被读取，
        // 因此 session/cancel 的断言放在放行之后。
        permit_tx.send(()).await.unwrap();
        wait_until(Duration::from_secs(2), async || {
            !bridge.sessions.lock().await.get("sess-1").unwrap().busy
        })
        .await;
        // mock 已收到 session/cancel 通知
        wait_until(Duration::from_secs(2), async || {
            recorded.lock().await.iter().any(|m| m == "session/cancel")
        })
        .await;

        // 无 AgentExecCancel（进程保留）
        assert!(
            cancels.lock().await.is_empty(),
            "graceful cancel must not kill the process"
        );
        // 被取消的回合不发生产者终态帧（stopped 已由 WS handler 回发）
        let stale = tokio::time::timeout(Duration::from_millis(300), ws_rx.recv()).await;
        assert!(
            matches!(stale, Err(_) | Ok(None)),
            "cancelled turn must not emit a terminal frame: {stale:?}"
        );
    }

    /// 兜底杀进程：cancel_grace 调极短，mock agent 不响应 cancel →
    /// 超时后 send_agent_cancel + cancel_fallback 帧 + busy 复位。
    #[tokio::test]
    async fn test_cancel_fallback_kills_after_grace() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let cancels = register_cancel_observer(&registry).await;
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db)
            .with_cancel_grace(Duration::from_millis(50));

        let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(32);
        // `_permit_tx` 必须保持存活到测试结束：drop 会让 mock 的 recv 返回 None
        // 而放行 PromptResponse（回合结束，兜底无从触发）；且永不 send → 回合
        // 一直 busy，agent 不响应 cancel。
        let (_permit_tx, permit_rx) = mpsc::channel::<()>(16);
        setup_handshake_with(
            &bridge,
            ws_tx.clone(),
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            Some(permit_rx),
            None,
            None,
            false,
        )
        .await;

        bridge
            .prompt("sess-1", "hello")
            .await
            .expect("prompt should send");
        bridge.cancel("sess-1").await;

        // 等待兜底任务：cancel_fallback 帧
        let frame = loop {
            let ev = tokio::time::timeout(Duration::from_secs(2), ws_rx.recv())
                .await
                .expect("timed out waiting for cancel_fallback")
                .expect("closed");
            if ev["type"] == "cancel_fallback" {
                break ev;
            }
        };
        assert_eq!(frame["type"], "cancel_fallback");
        // 兜底杀进程已下发
        assert_eq!(
            cancels.lock().await.as_slice(),
            &["sess-1".to_string()],
            "fallback should send AgentExecCancel for the session"
        );
        // busy 已复位（回合不再被卡死）
        assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
    }

    /// cancel 后队列自动 drain：被取消的回合结束后，排队的消息自动续跑，
    /// 新回合的 done 不被 cancelled_turns 误吞。
    #[tokio::test]
    async fn test_cancel_queued_prompts_auto_send_after_graceful_stop() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let cancels = register_cancel_observer(&registry).await;
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(32);
        let (permit_tx, permit_rx) = mpsc::channel::<()>(16);
        setup_handshake_with(
            &bridge,
            ws_tx.clone(),
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            Some(permit_rx),
            None,
            None,
            false,
        )
        .await;

        bridge
            .submit_prompt("sess-1", "first", vec![])
            .await
            .expect("first accepted");
        bridge
            .submit_prompt("sess-1", "second", vec![])
            .await
            .expect("second queued");
        bridge
            .submit_prompt("sess-1", "third", vec![])
            .await
            .expect("third queued");
        assert_eq!(
            bridge
                .sessions
                .lock()
                .await
                .get("sess-1")
                .unwrap()
                .pending_prompts
                .len(),
            2
        );

        // 取消当前回合（优雅路径，进程保留）
        bridge.cancel("sess-1").await;

        // 放行被取消的第一回合 → 队列自动 drain（second → third → done）
        permit_tx.send(()).await.unwrap();
        let mut events = Vec::new();
        let mut text_chunks = 0;
        let mut permits_sent = 0;
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), ws_rx.recv())
                .await
                .expect("timed out waiting for drain")
                .expect("closed");
            // 只数正文 chunk（thought 也是 assistant_chunk 类型）判定回合推进
            if ev["type"] == "assistant_chunk"
                && !ev.get("thought").and_then(serde_json::Value::as_bool).unwrap_or(false)
            {
                text_chunks += 1;
                // 第 2、3 个正文 chunk 分别放行 second/third 的 PromptResponse
                if text_chunks >= 2 && permits_sent < 2 {
                    permits_sent += 1;
                    permit_tx.send(()).await.unwrap();
                }
            }
            let done = ev["type"] == "done";
            events.push(ev);
            if done {
                break;
            }
        }
        assert_eq!(
            text_chunks, 3,
            "queued prompts should auto-send after the cancelled turn"
        );
        // 新回合的 done 不被 cancelled_turns 误吞
        assert_eq!(events.last().unwrap()["type"], "done");
        let queued = events.iter().filter(|e| e["type"] == "queued").count();
        assert_eq!(queued, 2, "second and third were queued");
        assert!(
            cancels.lock().await.is_empty(),
            "graceful cancel queue drain must not kill the process"
        );
        assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
    }

    // ── ACP 会话上下文持久化：session/resume + session/delete ──

    /// 建 workspace w1 + session sess-1（`agent_set_acp_session_id` 落库需要
    /// session 行；`agent_get_session` 断言同样依赖它）。
    async fn seeded_bridge() -> (AcpBridge, Database) {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        db.agent_create_workspace(
            "w1",
            "proj",
            "nas",
            "host",
            "/ws",
            None,
            None,
            "gemini",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db.clone());
        (bridge, db)
    }

    /// 断线重拉：持有多余的持久化 ACP session id + agent 支持 session/resume →
    /// 握手走 resume（sessionId 复用、DB 落库同 id），不建全新会话。
    #[tokio::test]
    async fn test_handshake_resumes_persisted_session() {
        let (bridge, db) = seeded_bridge().await;
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake_with(
            &bridge,
            ws_tx,
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            None,
            Some(recorded.clone()),
            Some("acp-persisted-1"),
            false,
        )
        .await;

        // mock 收到 session/resume，未走 session/new
        let methods = recorded.lock().await.clone();
        assert!(
            methods.iter().any(|m| m == "session/resume"),
            "should send session/resume, got: {methods:?}"
        );
        assert!(
            !methods.iter().any(|m| m == "session/new"),
            "resume-capable path must not send session/new: {methods:?}"
        );
        // 会话条目的 acp_session_id 复用持久化 id
        assert_eq!(
            bridge
                .sessions
                .lock()
                .await
                .get("sess-1")
                .unwrap()
                .acp_session_id
                .as_ref()
                .unwrap()
                .0
                .as_ref(),
            "acp-persisted-1"
        );
        // DB 落库同 id（下次重拉继续 resume）
        assert_eq!(
            db.agent_get_session("sess-1").await.unwrap().unwrap().acp_session_id,
            Some("acp-persisted-1".into())
        );
    }

    /// 无持久化 id（首次会话）→ 行为不变：session/new，新 id 落库。
    #[tokio::test]
    async fn test_handshake_new_when_no_persisted_id() {
        let (bridge, db) = seeded_bridge().await;
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake_with(
            &bridge,
            ws_tx,
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            None,
            Some(recorded.clone()),
            None,
            false,
        )
        .await;

        let methods = recorded.lock().await.clone();
        assert!(
            methods.iter().any(|m| m == "session/new"),
            "no persisted id should use session/new: {methods:?}"
        );
        assert!(!methods.iter().any(|m| m == "session/resume"));
        // 新 id（mock 固定返回 acp-1）落库
        assert_eq!(
            db.agent_get_session("sess-1").await.unwrap().unwrap().acp_session_id,
            Some("acp-1".into())
        );
    }

    /// resume 失败（会话文件缺失/已清理）→ 回退 session/new，新 id 落库，
    /// 会话建立不阻断。
    #[tokio::test]
    async fn test_handshake_resume_failure_falls_back_to_new() {
        let (bridge, db) = seeded_bridge().await;
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake_with(
            &bridge,
            ws_tx,
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            None,
            Some(recorded.clone()),
            Some("acp-persisted-1"),
            true,
        )
        .await;

        let methods = recorded.lock().await.clone();
        let resume_pos = methods.iter().position(|m| m == "session/resume");
        let new_pos = methods.iter().position(|m| m == "session/new");
        assert!(
            resume_pos.is_some(),
            "should attempt resume first: {methods:?}"
        );
        assert!(
            new_pos.is_some(),
            "resume failure should fall back to new: {methods:?}"
        );
        assert!(
            resume_pos < new_pos,
            "resume must be attempted before new: {methods:?}"
        );
        // 回退后使用新 id（mock 固定返回 acp-1）落库
        assert_eq!(
            db.agent_get_session("sess-1").await.unwrap().unwrap().acp_session_id,
            Some("acp-1".into())
        );
    }

    /// kill_and_delete（会话删除路径）先发 ACP session/delete（让 agent 清理其
    /// 持久化会话文件），再移除会话条目。方法等待 session/delete 响应后才继续
    /// → 断言时已记录。
    #[tokio::test]
    async fn test_kill_and_delete_sends_session_delete() {
        let (bridge, _db) = seeded_bridge().await;
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake_with(
            &bridge,
            ws_tx,
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            None,
            Some(recorded.clone()),
            None,
            false,
        )
        .await;

        bridge.kill_and_delete("sess-1").await;
        assert!(
            recorded.lock().await.iter().any(|m| m == "session/delete"),
            "kill_and_delete() should send session/delete to clean up client-side session data"
        );
        assert!(
            !bridge.sessions.lock().await.contains_key("sess-1"),
            "kill_and_delete() should remove the session entry"
        );
    }

    /// kill()（归档路径）**不**发 session/delete：归档后重开会话仍可
    /// session/resume 恢复上下文，客户端持久化会话数据必须保留。
    #[tokio::test]
    async fn test_kill_preserves_client_session_data() {
        let (bridge, _db) = seeded_bridge().await;
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake_with(
            &bridge,
            ws_tx,
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            None,
            Some(recorded.clone()),
            None,
            false,
        )
        .await;

        bridge.kill("sess-1").await;
        assert!(
            !recorded.lock().await.iter().any(|m| m == "session/delete"),
            "kill() (archive path) must preserve client-side session data: {:?}",
            recorded.lock().await
        );
        assert!(!bridge.sessions.lock().await.contains_key("sess-1"));
    }
}
