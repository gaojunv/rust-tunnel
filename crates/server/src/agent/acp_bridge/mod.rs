//! ACP bridge: owns agent sessions over the tunnel, translates between
//! the frontend WebSocket protocol and ACP JSON-RPC.
//!
//! 生命周期：`ensure_session` 惰性 spawn（LLM 回环代理 → agent 进程），
//! 进程 stdio 经控制通道（AgentSpawnData）与 ACP 连接互转；空闲 30 分钟由
//! reaper 回收。
//!
//! ## 模块结构（`acp_bridge/` 子模块）
//!
//! - `session` — session 生命周期与连接处理：创建、attach/detach、resume、kill、
//!   回合 prompt/cancel、ACP handshake（fs 请求转发与审批）、客户端控制消息路由
//! - `pump` — stdio pump：进程 stdin/stdout 与 ACP 连接（duplex）的字节转发
//! - `reaper` — idle reaper 与活动追踪：空闲超时的 agent 进程回收
//! - `store` — 事件/消息落库：WS 帧与回合缓冲写入 SQLite（best-effort）
//!
//! 本文件（`mod.rs`）是门面：持有共享类型（`PendingPrompt`/`TurnSegment`/
//! `SpawnedAgent`/`AcpBridge`）与构造器；`crate::agent::acp_bridge::*` 的外部
//! 路径保持不变（`AcpBridge` 及其 `pub` 方法）。
//!
//! ## stdio pump 与 ACP 连接
//!
//! 每个 session 一对 `tokio::io::duplex`：
//! - ACP 端（`agent_io`）交给 `agent-client-protocol` 的 `Client` 作为
//!   `ByteStreams`（入 = 进程 stdout，出 = 进程 stdin）。
//! - pump 端（`pump_io`）由 `pump::run_stdio_pump` 驱动：进程 stdout
//!   （`AgentSpawnData{stdin:false}` → 通道）写入 duplex 喂给 ACP crate；
//!   ACP crate 写出的字节读回后以 `AgentSpawnData{stdin:true}` 下发客户端。
//!
//! duplex 本身就是缓冲区：handshake 期间进程早产 stdout 会缓冲在 duplex/通道
//! 里（不会丢），ACP 连接建立后随即消费。

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex};

use agent_client_protocol::schema::v1::SessionConfigOption;
use agent_client_protocol::ConnectionTo;

use crate::db::Database;
use crate::llm::crypto::LlmCipher;

use super::llm_bridge::LlmGatewayEndpoint;
use super::spawner::AgentSpawner;
use super::{ApprovalOption, ApprovalResult, ElicitationResult};

#[cfg(test)]
use crate::persistence::db::agent::AgentWorkspaceRecord;
#[cfg(test)]
use rust_tunnel_common::ControlMessage;
#[cfg(test)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod pump;
mod reaper;
mod session;
mod store;

// 测试模块（`#[cfg(test)] mod tests` 用 `use super::*`）与子模块辅助函数的
// 桥接：子模块间调用各自直接 `use super::{pump,reaper,store}::...`；这里仅
// 为 `mod tests` 提供测试构建可见的导入（非测试构建不引入未使用导入）。
#[cfg(test)]
use pump::run_stdio_pump;
#[cfg(test)]
use session::{
    approve_or_disconnect, client_capabilities, elicit_or_disconnect, exec_fs_read, exec_fs_write,
    to_workspace_relative,
};
#[cfg(test)]
use store::{flush_acp_turn_buffers, persist_acp_frame};
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
    /// 子 agent 文本归属：发起本文本的 Task 工具调用 id（`_meta.claudeCode
    /// .parentToolUseId`）。主 agent 文本为 None。同段仅合并归属相同的 chunk——
    /// 若主/子 agent 文本交错到达，在 parent 变化处开新段，保证每行归属正确。
    parent_tool_call_id: Option<String>,
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
    /// 本会话所有 WS 连接的事件通道（`(conn_id, tx)`）。多标签页/多窗口打开同一
    /// 会话时，流式回合帧（assistant_chunk/tool_call/tool_result/plan/status/
    /// done/error/queued/cancel_fallback）广播到全部连接（fan-out）——根治
    /// 「最新连接获胜」劫持：被动打开的新标签页不应把正在运行回合的后续帧切走，
    /// 原标签页永久卡 running。`ensure_session` 注册/刷新，`detach_ws_tx` 按
    /// conn_id 移除。审批/elicitation 请求帧仍走主通道 `ws_tx`（最近注册连接），
    /// 多连接下审批卡只在主连接展示（响应按 request_id 全局可达，任意连接可答）。
    ws_conns: Vec<(u64, mpsc::Sender<serde_json::Value>)>,
    /// 当前注册 WS 通道所属的连接唯一标识：`detach_ws_tx` 按它判断「这个
    /// teardown 是不是注册方本人」。刷新竞态下旧连接 teardown 晚于新连接注册，
    /// 若无条件清空会误清新连接通道（tool_result/done 全丢）。
    ws_conn_id: u64,
    /// WS 连接变化通知：`ensure_session` 每次注册/刷新写入新 conn_id（insert
    /// 时以 conn_id 为初始值），[`AcpBridge::detach_ws_tx`] 匹配清空时写入 0。
    /// 审批等待（`request_permission` → [`approve_or_disconnect`]）订阅此通道：
    /// 连接断开/重连（值变为 ≠ 捕获的 conn_id）即立即拒绝，不再等满 5 分钟
    /// 审批超时——断线后审批帧缓冲在无人消费的 channel 里、新重连也收不到。
    ws_conn_watch: watch::Sender<u64>,
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
    /// AI 记忆注入缓存：None = 尚未检索；Some("") = 无可用记忆；Some(非空) = 已
    /// 注入块。`prompt_inner` 发送前把块 prepend 到 user content 头部（不进 DB，
    /// 持久化保持干净；distill 渲染也会剥离 `<memory>` 块，无回环）。
    memory_block: Option<String>,
    /// Skill 清单注入缓存（同 memory_block 模式）：None = 尚未检索；Some("") =
    /// 无可用技能；Some(非空) = 已注入 `<skills>` 块。`prompt_inner` 与 memory_block
    /// 一并 prepend；distill 渲染也会剥离 `<skills>` 块，无回环。纯 SQL 检索，
    /// 零 embedding 依赖。
    skill_list_block: Option<String>,
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

/// ACP `elicitation/create` → 用户表单回调。
///
/// `(session_id, message, schema, ws_tx) → 表单结果`。`schema` 是 agent 的
/// `requestedSchema` 原始 JSON（复杂 serde 枚举，后端不透传前端重建表单模型）。
/// 与 [`ApproveFn`] 同模式：`AcpBridge` 构造时 `AgentState` 尚在构建（循环依赖），
/// 由 `AgentState::new` 通过 [`Self::with_elicitation`] 注入真实实现（走
/// `AgentState::request_elicitation`）；未注入时默认取消。
type ElicitFn = dyn Fn(
        String,
        String,
        serde_json::Value,
        mpsc::Sender<serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = ElicitationResult> + Send>>
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
    /// ACP `elicitation/create` → 用户表单回调（`AgentState::new` 注入；未注入
    /// 默认取消——agent 的 AskUserQuestion 在没有实现时安全降级，不会卡死）。
    elicitation: Arc<ElicitFn>,
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
    /// AI 记忆体运行时（kill/断线/idle 蒸馏触发用）。仅 `rag` feature 下存在，
    /// 由 `AgentState::with_memory` 注入（与 AgentState.memory 同一实例）。
    #[cfg(feature = "rag")]
    memory: Option<super::memory::MemoryState>,
}
impl AcpBridge {
    pub fn new(spawner: AgentSpawner, db: Database) -> Self {
        let bridge = Self {
            spawner,
            db,
            cipher: None,
            approval: Arc::new(|_, _, _, _, _, _| Box::pin(async { ApprovalResult::Denied })),
            elicitation: Arc::new(|_, _, _, _| Box::pin(async { ElicitationResult::Cancel })),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            spawn_errors: Arc::new(Mutex::new(HashMap::new())),
            gateway: None,
            cancel_grace: DEFAULT_CANCEL_GRACE,
            #[cfg(feature = "rag")]
            memory: None,
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

    /// 注入 ACP `elicitation/create` 的表单回调（`AgentState::new` 在构造期调用，
    /// 与 `with_approval` 同模式解决循环依赖）。走 `AgentState::request_elicitation`
    /// 的 pending map，WS `elicitation_response` 帧分发唤醒。
    #[must_use]
    pub fn with_elicitation(mut self, elicitation: Arc<ElicitFn>) -> Self {
        self.elicitation = elicitation;
        self
    }

    /// 注入 AI 记忆体运行时（kill/断线/idle 蒸馏触发用）。由 `AgentState::with_memory`
    /// 在 `init_llm_state` 后调用，与 `AgentState.memory` 共享同一 `MemoryState` 实例。
    #[cfg(feature = "rag")]
    #[must_use]
    pub fn with_memory(mut self, memory: super::memory::MemoryState) -> Self {
        self.memory = Some(memory);
        self
    }

    /// 读会话的 AI 记忆注入缓存（None = 尚未检索）。`prompt_inner` 发送前读它
    /// prepend `<memory>` 块；WS handler 在首条消息检索后经 [`Self::set_memory_block`]
    /// 写入。
    pub async fn cached_memory_block(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|a| a.memory_block.clone())
    }

    /// 写会话的 AI 记忆注入缓存。`Some(block)`（含空串=无可用记忆）覆盖；此字段
    /// 仅在会话条目存活期间有意义，kill/reaper 移除条目后自然消失。
    pub async fn set_memory_block(&self, session_id: &str, block: Option<String>) {
        if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
            a.memory_block = block;
        }
    }

    /// 读会话的 Skill 清单注入缓存（None = 尚未检索）。WS handler 在首条消息检索后
    /// 经 [`Self::set_skill_list_block`] 写入；`prompt_inner` 发送前与 memory_block
    /// 一并 prepend。
    pub async fn cached_skill_list_block(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|a| a.skill_list_block.clone())
    }

    /// 写会话的 Skill 清单注入缓存。语义与 [`Self::set_memory_block`] 一致。
    pub async fn set_skill_list_block(&self, session_id: &str, block: Option<String>) {
        if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
            a.skill_list_block = block;
        }
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
            github_token: None,
            github_owner: None,
            github_repo: None,
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
            memory_block: None,
            skill_list_block: None,
            acp_session_id: None,
            connection: None,
            agent_io: None,
            stdout_tx: None,
            client_id: "nas".into(),
            ws_tx: None,
            ws_conns: Vec::new(),
            ws_conn_id: 0,
            ws_conn_watch: watch::channel(0).0,
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
            None,
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
    /// 回 error（测回退 session/new）。`fail_config_id`：mock 对该 config_id 的
    /// `session/set_config_option` 回 JSON-RPC error（测「单条失败不阻断其余注入」）。
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
        fail_config_id: Option<&str>,
    ) {
        let (agent_io, pump_io) = tokio::io::duplex(64 * 1024);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
        let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(32);

        let mut agent = spawned_agent();
        agent.agent_io = Some(agent_io);
        agent.stdout_tx = Some(stdout_tx.clone());
        agent.ws_tx = Some(ws_tx.clone());
        agent.ws_conn_id = TEST_CONN_ID;
        // 广播列表同步登记（与真实路径 ensure_session 建条目一致）：多连接 fan-out
        // 测试依赖 setup 时就含首连接。
        agent.ws_conns = vec![(TEST_CONN_ID, ws_tx.clone())];
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
            fail_config_id.map(str::to_string),
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
    fn test_client_capabilities_declare_subagent_transcript() {
        // opt-in 约定：_meta["subagent-transcript"]=true 请求 agent 转发子 agent
        // 事件的 parentToolUseId/subagent 元数据。不支持 _meta 的 agent 忽略该键。
        let caps = client_capabilities();
        let meta = caps.meta.expect("capabilities should carry _meta");
        assert_eq!(
            meta.get("subagent-transcript").and_then(|v| v.as_bool()),
            Some(true),
            "_meta.subagent-transcript must be true: {meta:?}"
        );
    }

    #[test]
    fn test_client_capabilities_declare_elicitation_form() {
        // 声明 elicitation.form：claude-code-acp 据此启用 AskUserQuestion（否则放入
        // disallowedTools 报「not enabled in this context」）。只声明 form、不声明
        // url（缺省 None → 序列化不含该键）。
        let caps = client_capabilities();
        let elicitation = caps
            .elicitation
            .as_ref()
            .expect("capabilities should carry elicitation");
        assert!(
            elicitation.form.is_some(),
            "form capability must be declared"
        );
        assert!(
            elicitation.url.is_none(),
            "url capability must not be declared"
        );
        let json = serde_json::to_value(&caps).unwrap();
        assert!(json["elicitation"]["form"].is_object());
        assert!(
            json["elicitation"].get("url").is_none(),
            "url key must be absent: {json}"
        );
    }

    #[tokio::test]
    async fn test_elicit_or_disconnect_cancels_on_detach() {
        // 断线即时取消：表单等待期间 detach_ws_tx 写入 0 → conn_watch 值变化
        // → wait_for 唤醒 → Cancel，不等满 5 分钟表单超时（镜像审批的 detach 测试）。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        let mut agent = spawned_agent();
        agent.ws_tx = Some(ws_tx);
        agent.ws_conn_id = TEST_CONN_ID;
        agent.ws_conn_watch.send_replace(TEST_CONN_ID);
        let conn_rx = agent.ws_conn_watch.subscribe();
        bridge.sessions.lock().await.insert("sess-1".into(), agent);

        // elicitation 挂起（永不返回）：等待中途断线由 watch 唤醒短路取消。
        let elicitation: Arc<ElicitFn> = Arc::new(|_, _, _, _| {
            Box::pin(async {
                std::future::pending::<()>().await;
                ElicitationResult::Cancel
            })
        });
        let handle = tokio::spawn(elicit_or_disconnect(
            elicitation,
            "sess-1".into(),
            "please choose".into(),
            serde_json::json!({}),
            mpsc::channel::<serde_json::Value>(1).0,
            TEST_CONN_ID,
            conn_rx,
        ));

        // 表单在途时连接断开 → 立即取消
        bridge.detach_ws_tx("sess-1", TEST_CONN_ID).await;
        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("must cancel promptly on disconnect")
            .unwrap();
        assert_eq!(result, ElicitationResult::Cancel);
    }

    #[tokio::test]
    async fn test_elicit_or_disconnect_accepts_when_connected() {
        // 连接保持时 conn_watch 值不变：表单正常完成（Accept + content），不被误取消。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        let mut agent = spawned_agent();
        agent.ws_tx = Some(ws_tx);
        agent.ws_conn_id = TEST_CONN_ID;
        agent.ws_conn_watch.send_replace(TEST_CONN_ID);
        let conn_rx = agent.ws_conn_watch.subscribe();
        bridge.sessions.lock().await.insert("sess-1".into(), agent);

        let content = serde_json::from_value::<
            std::collections::BTreeMap<
                String,
                agent_client_protocol::schema::v1::ElicitationContentValue,
            >,
        >(serde_json::json!({"name": "Alice"}))
        .unwrap();
        let elicitation: Arc<ElicitFn> = Arc::new(move |_, _, _, _| {
            let content = content.clone();
            Box::pin(async move { ElicitationResult::Accept(Some(content)) })
        });
        let result = elicit_or_disconnect(
            elicitation,
            "sess-1".into(),
            "please choose".into(),
            serde_json::json!({}),
            mpsc::channel::<serde_json::Value>(1).0,
            TEST_CONN_ID,
            conn_rx,
        )
        .await;
        match result {
            ElicitationResult::Accept(Some(content)) => {
                assert_eq!(
                    content.get("name"),
                    Some(
                        &agent_client_protocol::schema::v1::ElicitationContentValue::String(
                            "Alice".into()
                        )
                    )
                );
            }
            other => panic!("expected Accept, got {other:?}"),
        }
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
        let _ = bridge
            .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
            .await;

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
            let _ = bridge
                .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
                .await;
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
        fail_config_id: Option<String>,
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
                        // 指定的 config_id 硬失败：回 JSON-RPC error，验证调用方
                        // 「单条失败不阻断其余注入」。
                        if fail_config_id.as_deref() == Some(config_id.as_str()) {
                            out_lines.push(serde_json::json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": { "code": -32000, "message": "invalid config value" }
                            }));
                            continue;
                        }
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
    async fn test_multitab_broadcasts_frames_and_detach_stops_old_tab() {
        // 回归（H5）：多标签页/多窗口共用同一 ACP 进程。旧实现「最新连接获胜」——
        // ensure_session 把流式帧切到最新连接，回合进行中被动打开的第二个标签页把
        // 正在运行回合的帧/done 全劫走，原标签页永久卡 running。修复后流式/终态帧
        // fan-out 到全部连接；已 detach 的旧标签页不再收到后续帧。
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        // 标签页 A：handshake 建立常驻连接任务（setup 已把 A 登记进 ws_conns）。
        let (ws_tx_a, mut ws_rx_a) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake(&bridge, ws_tx_a.clone()).await;

        // 标签页 B：重连 → ensure_session dedup 把 B 追加进广播列表（独立 conn_id）。
        let (ws_tx_b, mut ws_rx_b) = mpsc::channel::<serde_json::Value>(16);
        bridge
            .ensure_session("sess-1", &acp_workspace(), ws_tx_b, TEST_CONN_ID + 1)
            .await
            .expect("reconnect dedup should register second connection");

        bridge
            .prompt("sess-1", "hello")
            .await
            .expect("prompt should send");

        // 回合帧（assistant_chunk / tool_call / tool_result / thought / plan / done）
        // 应**同时**到达 A 与 B——广播而非「最新连接获胜」劫持。
        for expected in [
            "assistant_chunk",
            "tool_call",
            "tool_result",
            "assistant_chunk",
            "plan",
            "done",
        ] {
            let ev_a = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx_a.recv())
                .await
                .expect("timed out waiting for ws event on tab A")
                .expect("ws channel closed");
            assert_eq!(ev_a["type"], expected, "event on tab A: {ev_a}");
            let ev_b = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx_b.recv())
                .await
                .expect("timed out waiting for ws event on tab B")
                .expect("ws channel closed");
            assert_eq!(ev_b["type"], expected, "event on tab B: {ev_b}");
        }

        // A 关闭：detach 只移除 A（且把主通道顺延到 B），B 继续收到后续回合帧。
        bridge.detach_ws_tx("sess-1", TEST_CONN_ID).await;

        bridge
            .prompt("sess-1", "again")
            .await
            .expect("prompt should send");

        for expected in [
            "assistant_chunk",
            "tool_call",
            "tool_result",
            "assistant_chunk",
            "plan",
            "done",
        ] {
            let ev_b = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx_b.recv())
                .await
                .expect("timed out waiting for ws event on tab B after A detached")
                .expect("ws channel closed");
            assert_eq!(
                ev_b["type"], expected,
                "event on tab B after A detached: {ev_b}"
            );
        }
        // A 已从广播列表移除：后续帧不再到达（只有 Ok(Some) 才是泄漏）。
        let stale =
            tokio::time::timeout(std::time::Duration::from_millis(200), ws_rx_a.recv()).await;
        assert!(
            !matches!(stale, Ok(Some(_))),
            "detached tab A should receive nothing: {stale:?}"
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
    async fn test_process_crash_sends_error_frame() {
        // 回归（H1）：进程崩溃（exited 置位、非用户取消）后 PromptResponse 到达
        // 时，终态回调必须发 error 帧上报前端——否则前端 running 永久卡死。
        // 与 test_cancel_suppresses_terminal_frame 相对：取消抑制终态帧，崩溃上报。
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
        // prompt_permits 控制 mock 何时回 PromptResponse：允许我们在回调触发前
        // 置 exited=true，精确模拟「进程在回合中途崩溃」。
        let (permits_tx, permits_rx) = mpsc::channel::<()>(1);
        setup_handshake_with(
            &bridge,
            ws_tx.clone(),
            serde_json::json!([]),
            Arc::new(Mutex::new(Vec::new())),
            Some(permits_rx),
            None,
            None,
            false,
            None,
        )
        .await;

        bridge
            .prompt("sess-1", "hello")
            .await
            .expect("prompt should send");

        // 消费流式通知，确保 mock 已进入等待许可（回合 busy、PromptResponse 未回）。
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

        // 模拟进程崩溃：直接置 exited=true（等价 handle_spawn_exit 的语义——进程
        // 退出后 ACP 连接关闭，PromptResponse 回调以 Err 触发，`alive` 变 false）。
        bridge
            .sessions
            .lock()
            .await
            .get_mut("sess-1")
            .unwrap()
            .exited = true;

        // 释放许可，mock 回 PromptResponse → 终态回调触发。
        permits_tx.send(()).await.expect("permit send");

        // 崩溃（非取消）必须发 error 帧，前端据此解除 running。
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv())
            .await
            .expect("timed out waiting for error frame")
            .expect("ws channel closed");
        assert_eq!(err["type"], "error", "crash must emit error frame: {err}");
        assert!(
            err["message"].as_str().unwrap_or("").contains("进程已退出"),
            "error message should mention process exit: {err}"
        );
        // busy 已复位（回合状态不被卡死）。
        assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
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
        // tool_result 行：M2 起 content 为结构化 JSON（text/status），status 落库
        // 供前端区分失败/成功。
        let results: Vec<_> = rows.iter().filter(|r| r.kind == "tool_result").collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_call_id.as_deref(), Some("call_1"));
        let result_json: serde_json::Value = serde_json::from_str(&results[0].content).unwrap();
        assert_eq!(result_json["text"], "a.rs");
        assert_eq!(result_json["status"], "completed");
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
    async fn test_turn_segments_split_by_parent() {
        // 主 agent 文本与子 agent 文本交错到达：同一缓冲段内混入不同 parent 的
        // chunk 必须在 parent 变化处切分 segment，保证每行消息的父归属正确。
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

        // 主 agent 开场 → 子 agent（task_1）文本 → 回到主 agent
        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({"type": "assistant_chunk", "content": "主开场"}),
        )
        .await;
        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({
                "type": "assistant_chunk",
                "content": "子文本",
                "parent_tool_call_id": "task_1",
            }),
        )
        .await;
        // 同 parent 的后续 chunk 应合并进子 agent 段
        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({
                "type": "assistant_chunk",
                "content": "续",
                "parent_tool_call_id": "task_1",
            }),
        )
        .await;
        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({"type": "assistant_chunk", "content": "主收尾"}),
        )
        .await;
        flush_acp_turn_buffers(&db, &sessions, "sess-1").await;

        let rows = db.agent_list_messages("sess-1").await.unwrap();
        let texts: Vec<(String, Option<String>)> = rows
            .iter()
            .filter(|r| r.kind == "message")
            .map(|r| (r.content.clone(), r.parent_tool_call_id.clone()))
            .collect();
        assert_eq!(
            texts,
            vec![
                ("主开场".to_string(), None),
                // 同 parent 相邻 chunk 合并成一段，归属正确
                ("子文本续".to_string(), Some("task_1".to_string())),
                ("主收尾".to_string(), None),
            ],
            "segments must split at parent change: {rows:?}"
        );
    }

    // ── M2：tool_result 结构化 content 落库 ──────────────────────

    /// 建一个含 workspace/session + 空会话表的 persist 环境。
    async fn persist_env() -> (Database, Arc<Mutex<HashMap<String, SpawnedAgent>>>) {
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
        (db, sessions)
    }

    #[tokio::test]
    async fn test_tool_result_persists_structured_json() {
        // M2 契约：中间态 running（空占位）→ 终态 completed（带 result + diffs），
        // 落库 content 为 JSON `{"text","status","diffs",...}`——status/diffs
        // 落库后刷新不丢（失败打叉、diff 展示依赖它）。
        let (db, sessions) = persist_env().await;

        // 中间态：空占位（running、无产出）
        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({
                "type": "tool_result",
                "id": "call_1",
                "name": "shell",
                "status": "running",
            }),
        )
        .await;
        // 终态：completed + result + diffs + locations
        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({
                "type": "tool_result",
                "id": "call_1",
                "name": "shell",
                "status": "completed",
                "result": "a.rs",
                "diffs": [{"old": "x", "new": "y"}],
                "locations": [{"path": "a.rs", "line": 3}],
            }),
        )
        .await;

        let rows = db.agent_list_messages("sess-1").await.unwrap();
        let results: Vec<_> = rows.iter().filter(|r| r.kind == "tool_result").collect();
        assert_eq!(results.len(), 1, "upsert 收敛为一行: {rows:?}");
        let v: serde_json::Value = serde_json::from_str(&results[0].content).unwrap();
        assert_eq!(v["text"], "a.rs");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["diffs"][0]["new"], "y");
        assert_eq!(v["locations"][0]["line"], 3);
    }

    #[tokio::test]
    async fn test_tool_result_failed_empty_text_persists() {
        // 失败工具恒显 ✓ 的根因修复：failed 终态即使 result 为空也要落库
        // （status=failed），前端据此打叉。
        let (db, sessions) = persist_env().await;

        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({
                "type": "tool_result",
                "id": "call_1",
                "name": "shell",
                "status": "running",
            }),
        )
        .await;
        // 失败终态：无 result 文本
        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({
                "type": "tool_result",
                "id": "call_1",
                "name": "shell",
                "status": "failed",
            }),
        )
        .await;

        let rows = db.agent_list_messages("sess-1").await.unwrap();
        let results: Vec<_> = rows.iter().filter(|r| r.kind == "tool_result").collect();
        assert_eq!(results.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&results[0].content).unwrap();
        assert_eq!(v["text"], "", "失败终态 text 为空");
        assert_eq!(
            v["status"], "failed",
            "status 必须落库: {}",
            results[0].content
        );
    }

    #[tokio::test]
    async fn test_tool_result_late_placeholder_does_not_overwrite() {
        // 空占位语义：已完成（JSON 落库）后迟到的中间态 running 帧不得覆盖真实结果。
        let (db, sessions) = persist_env().await;

        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({
                "type": "tool_result",
                "id": "call_1",
                "name": "shell",
                "status": "completed",
                "result": "真实结果",
            }),
        )
        .await;
        // 迟到的中间态帧（刷新/重连乱序）：空占位 → "" → upsert 不覆盖
        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({
                "type": "tool_result",
                "id": "call_1",
                "name": "shell",
                "status": "running",
            }),
        )
        .await;

        let rows = db.agent_list_messages("sess-1").await.unwrap();
        let results: Vec<_> = rows.iter().filter(|r| r.kind == "tool_result").collect();
        assert_eq!(results.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&results[0].content).unwrap();
        assert_eq!(v["text"], "真实结果");
        assert_eq!(v["status"], "completed");
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
            bridge
                .sessions
                .lock()
                .await
                .get("sess-1")
                .unwrap()
                .ws_tx
                .is_none(),
            "own detach should clear ws_tx"
        );

        // 新连接已注册（ws_conn_id=NEW）：旧连接晚到的 teardown 不得清掉它
        let mut a = spawned_agent();
        a.ws_tx = Some(tx.clone());
        a.ws_conn_id = NEW;
        bridge.sessions.lock().await.insert("sess-1".into(), a);
        bridge.detach_ws_tx("sess-1", OLD).await;
        assert!(
            bridge
                .sessions
                .lock()
                .await
                .get("sess-1")
                .unwrap()
                .ws_tx
                .is_some(),
            "old connection teardown must not clear newer connection's ws_tx"
        );
        // 新连接自己的 teardown 仍能清空
        bridge.detach_ws_tx("sess-1", NEW).await;
        assert!(bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .ws_tx
            .is_none());
    }

    // ── WS 连接变化 watch：审批断线/重连即时拒绝 ──────────────────

    #[tokio::test]
    async fn test_ws_conn_watch_denies_on_detach() {
        // 断线即时拒绝：审批等待期间 detach_ws_tx 写入 0 → conn_watch 值变化
        // → wait_for 唤醒 → Deny，不等满 5 分钟审批超时（旧实现审批帧缓冲在
        // 无人消费的 channel 里，新重连也看不到审批卡）。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        let mut agent = spawned_agent();
        agent.ws_tx = Some(ws_tx);
        agent.ws_conn_id = TEST_CONN_ID;
        agent.ws_conn_watch.send_replace(TEST_CONN_ID);
        let conn_rx = agent.ws_conn_watch.subscribe();
        bridge.sessions.lock().await.insert("sess-1".into(), agent);

        // approval 挂起（永不返回）：等待中途断线由 watch 唤醒短路拒绝。
        let approval: Arc<ApproveFn> = Arc::new(|_, _, _, _, _, _| {
            Box::pin(async {
                std::future::pending::<()>().await;
                ApprovalResult::Approved
            })
        });
        let handle = tokio::spawn(approve_or_disconnect(
            approval,
            "sess-1".into(),
            "shell".into(),
            "ls".into(),
            vec![],
            mpsc::channel::<serde_json::Value>(1).0,
            TEST_CONN_ID,
            conn_rx,
        ));

        // 审批在途时连接断开 → 立即拒绝
        bridge.detach_ws_tx("sess-1", TEST_CONN_ID).await;
        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("must deny promptly on disconnect")
            .unwrap();
        assert_eq!(result, ApprovalResult::Denied);
    }

    #[tokio::test]
    async fn test_ws_conn_watch_approves_when_connected() {
        // 连接保持时 conn_watch 值不变：审批正常完成（Approved），不被误拒。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        let mut agent = spawned_agent();
        agent.ws_tx = Some(ws_tx);
        agent.ws_conn_id = TEST_CONN_ID;
        agent.ws_conn_watch.send_replace(TEST_CONN_ID);
        let conn_rx = agent.ws_conn_watch.subscribe();
        bridge.sessions.lock().await.insert("sess-1".into(), agent);

        let approval: Arc<ApproveFn> =
            Arc::new(|_, _, _, _, _, _| Box::pin(async { ApprovalResult::Approved }));
        let result = approve_or_disconnect(
            approval,
            "sess-1".into(),
            "shell".into(),
            "ls".into(),
            vec![],
            mpsc::channel::<serde_json::Value>(1).0,
            TEST_CONN_ID,
            conn_rx,
        )
        .await;
        assert_eq!(result, ApprovalResult::Approved);
    }

    // ── drop_client_sessions：客户端控制连接断开清理 ──────────────

    #[tokio::test]
    async fn test_drop_client_sessions_cleans_sessions_for_client() {
        // 两个 nas 会话 + 一个其他客户端会话：drop 后仅 nas 的两条被移除。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        let mut nas1 = spawned_agent();
        nas1.client_id = "nas".into();
        let mut nas2 = spawned_agent();
        nas2.client_id = "nas".into();
        let mut other = spawned_agent();
        other.client_id = "other".into();
        {
            let mut sessions = bridge.sessions.lock().await;
            sessions.insert("sess-nas-1".into(), nas1);
            sessions.insert("sess-nas-2".into(), nas2);
            sessions.insert("sess-other".into(), other);
        }
        bridge.drop_client_sessions("nas").await;
        let sessions = bridge.sessions.lock().await;
        assert!(!sessions.contains_key("sess-nas-1"));
        assert!(!sessions.contains_key("sess-nas-2"));
        assert!(sessions.contains_key("sess-other"));
    }

    #[tokio::test]
    async fn test_drop_client_sessions_flushes_buffers() {
        // 会话有缓冲 turn_segments（断线瞬间未到终态）：drop 时先 flush 落库，
        // 刷新历史仍可追溯（思考先行、正文随后）。
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
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db.clone());

        let mut agent = spawned_agent();
        agent.client_id = "nas".into();
        agent.turn_segments = vec![
            TurnSegment {
                thought: true,
                content: "先思考".into(),
                parent_tool_call_id: None,
            },
            TurnSegment {
                thought: false,
                content: "再回复".into(),
                parent_tool_call_id: None,
            },
        ];
        bridge.sessions.lock().await.insert("sess-1".into(), agent);

        bridge.drop_client_sessions("nas").await;

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
    }

    #[tokio::test]
    async fn test_drop_client_sessions_unknown_client_noop() {
        // 不存在的 client：不 panic、不动其它会话条目。
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge
            .sessions
            .lock()
            .await
            .insert("sess-1".into(), spawned_agent());
        bridge.drop_client_sessions("no-such-client").await;
        assert!(bridge.sessions.lock().await.contains_key("sess-1"));
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
        setup_handshake_with(
            &bridge,
            ws_tx,
            options,
            applied.clone(),
            None,
            None,
            None,
            false,
            None,
        )
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
                &bridge,
                ws_tx,
                options.clone(),
                applied.clone(),
                None,
                None,
                None,
                false,
                None,
            )
            .await;
            bridge.apply_config_overrides("sess-1", &ws).await;
            assert!(
                applied.lock().await.is_empty(),
                "{label}: apply_config_overrides should be a no-op"
            );
        }
    }

    /// 端到端 ensure_session mock 客户端：注册模拟客户端，自动应答协商请求
    /// （AgentLlmProxyStart → AgentLlmProxyReady、AgentSpawnRequest →
    /// AgentSpawnResponse success），并扮演 ACP 进程——stdin 数据（JSON-RPC 请求）
    /// 经 pump → 控制通道到达后转发给 [`mock_acp_agent`] 逐行应答，进程 stdout
    /// 经 `SpawnedAgent.stdout_tx` 送回 pump → ACP 连接。生产接线
    /// `ensure_session` 全链路（start_llm_proxy → spawn_agent → handshake → 配置注入）
    /// 不经此桥无法完成，测试由此验证真实调用顺序。
    async fn spawn_e2e_client(
        registry: &crate::client_registry::ClientRegistry,
        sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
        config_options: serde_json::Value,
        applied: Arc<Mutex<Vec<(String, String)>>>,
        fail_config_id: Option<&str>,
    ) {
        let (tx, mut rx) = mpsc::channel::<ControlMessage>(64);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let registry2 = registry.clone();
        let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(32);
        let (proc_stdout_tx, mut proc_stdout_rx) = mpsc::channel::<Vec<u8>>(128);
        // 进程侧 ACP 模拟（复用 handshake 测试的 mock_acp_agent 逻辑）
        tokio::spawn(mock_acp_agent(
            control_rx,
            proc_stdout_tx,
            config_options,
            applied,
            None,
            None,
            false,
            fail_config_id.map(str::to_string),
        ));
        // 客户端侧：协商应答 + stdin（server→process）桥接到进程侧
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    ControlMessage::AgentLlmProxyStart { session_id } => {
                        registry2
                            .resolve_spawn_pending(
                                &session_id,
                                ControlMessage::AgentLlmProxyReady {
                                    session_id: session_id.clone(),
                                    port: 45678,
                                },
                            )
                            .await;
                    }
                    ControlMessage::AgentSpawnRequest { session_id, .. } => {
                        registry2
                            .resolve_spawn_pending(
                                &session_id,
                                ControlMessage::AgentSpawnResponse {
                                    session_id: session_id.clone(),
                                    success: true,
                                    error: None,
                                },
                            )
                            .await;
                    }
                    ControlMessage::AgentSpawnData {
                        data,
                        stdin: true,
                        session_id,
                    } => {
                        let _ = control_tx
                            .send(ControlMessage::AgentSpawnData {
                                data,
                                stdin: true,
                                session_id,
                            })
                            .await;
                    }
                    // AgentLlmProxyStop / AgentExecCancel 等单向清理：忽略。
                    _ => {}
                }
            }
        });
        // 进程 stdout（process→server）桥接到 pump（经 SpawnedAgent.stdout_tx）
        let sessions3 = sessions.clone();
        tokio::spawn(async move {
            while let Some(bytes) = proc_stdout_rx.recv().await {
                let st = sessions3
                    .lock()
                    .await
                    .get("sess-1")
                    .and_then(|a| a.stdout_tx.clone());
                if let Some(st) = st {
                    let _ = st.send(bytes).await;
                }
            }
        });
    }

    /// 生产接线端到端：`ensure_session` 内部真实调用顺序 apply_config_overrides →
    /// replay_config_state → spawn_ready（不经手工逐函数调用）。workspace overrides
    /// 注入后 session 级 config_state 回放覆盖之；全部完成后 spawn_ready 才置位。
    #[tokio::test]
    async fn test_ensure_session_production_config_injection_order() {
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
            Some("model-1"),
            Some(r#"{"model":"sonnet","fast":"haiku"}"#),
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, None)
            .await
            .unwrap();
        // 用户显式 config_state：model→opus，必须覆盖 workspace 注入的 sonnet
        db.agent_update_session_config_state("sess-1", "model", Some("opus"))
            .await
            .unwrap();

        let options = serde_json::json!([
            {"id": "model", "name": "Model", "type": "select", "currentValue": "sonnet",
             "options": [{"value": "sonnet", "name": "Sonnet"}, {"value": "opus", "name": "Opus"}]},
            {"id": "fast", "name": "Fast model", "type": "select", "currentValue": "haiku",
             "options": [{"value": "haiku", "name": "Haiku"}]}
        ]);
        let applied = Arc::new(Mutex::new(Vec::new()));
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry.clone()), db.clone());
        spawn_e2e_client(&registry, &bridge.sessions, options, applied.clone(), None).await;

        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        bridge
            .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
            .await
            .expect("ensure_session 应走完全部注入");

        // 调用顺序 = 生产接线：workspace overrides（fast 先于 model）→ config_state
        // 回放（model→opus，用户显式选择覆盖 workspace 默认）。
        let calls = applied.lock().await.clone();
        assert_eq!(
            calls,
            vec![
                ("fast".to_string(), "haiku".to_string()),
                ("model".to_string(), "sonnet".to_string()),
                ("model".to_string(), "opus".to_string()),
            ],
            "ensure_session 真实注入顺序错误: {calls:?}"
        );
        // 配置注入全部完成后 spawn_ready 才置位（wait_ready 依赖此信号，首条
        // prompt 不与在途 set_config_option 竞态）
        let ready = bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .spawn_ready
            .borrow()
            .clone();
        assert!(ready, "spawn_ready 应在配置注入完成后置位");
        // config_options 快照已从 session/new 捕获
        assert_eq!(
            bridge
                .sessions
                .lock()
                .await
                .get("sess-1")
                .unwrap()
                .config_options
                .len(),
            2
        );
    }

    /// set_config_option 硬失败继续注入（overrides 路径）：mock 对 model 回 JSON-RPC
    /// error，apply_config_overrides 必须跳过该条并继续注入 fast（nonexistent 本地
    /// 校验跳过）——现有 mock 恒成功，此路径此前无独立测试。
    #[tokio::test]
    async fn test_config_injection_continues_after_hard_failure() {
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
            Some("model-1"),
            Some(r#"{"model":"sonnet","fast":"haiku","nonexistent":"x"}"#),
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, None)
            .await
            .unwrap();

        let options = serde_json::json!([
            {"id": "model", "name": "Model", "type": "select", "currentValue": "sonnet",
             "options": [{"value": "sonnet", "name": "Sonnet"}, {"value": "opus", "name": "Opus"}]},
            {"id": "fast", "name": "Fast model", "type": "select", "currentValue": "haiku",
             "options": [{"value": "haiku", "name": "Haiku"}]}
        ]);
        let applied = Arc::new(Mutex::new(Vec::new()));
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db.clone());
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        // mock 对 model 的 set_config_option 回 JSON-RPC error
        setup_handshake_with(
            &bridge,
            ws_tx,
            options,
            applied.clone(),
            None,
            None,
            None,
            false,
            Some("model"),
        )
        .await;

        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        bridge.apply_config_overrides("sess-1", &ws).await;

        let calls = applied.lock().await.clone();
        assert_eq!(
            calls,
            vec![("fast".to_string(), "haiku".to_string())],
            "model 硬失败后 fast 应继续注入: {calls:?}"
        );
    }

    /// set_config_option 硬失败继续注入（config_state 回放路径）：回放按 mode 优先
    /// 排序，mode 硬失败后 fast 仍注入——单条失败不阻断整个回放。
    #[tokio::test]
    async fn test_replay_config_state_continues_after_hard_failure() {
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
            Some("model-1"),
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, None)
            .await
            .unwrap();
        // 回放含 mode（会硬失败，且按排序必先于 fast）与 fast（应照常注入）
        db.agent_update_session_config_state("sess-1", "mode", Some("plan"))
            .await
            .unwrap();
        db.agent_update_session_config_state("sess-1", "fast", Some("haiku"))
            .await
            .unwrap();

        let options = serde_json::json!([
            {"id": "mode", "name": "Mode", "type": "select", "currentValue": "normal",
             "options": [{"value": "normal", "name": "Normal"}, {"value": "plan", "name": "Plan"}]},
            {"id": "fast", "name": "Fast model", "type": "select", "currentValue": "haiku",
             "options": [{"value": "haiku", "name": "Haiku"}]}
        ]);
        let applied = Arc::new(Mutex::new(Vec::new()));
        let registry = crate::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db.clone());
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake_with(
            &bridge,
            ws_tx,
            options,
            applied.clone(),
            None,
            None,
            None,
            false,
            Some("mode"),
        )
        .await;

        bridge.replay_config_state("sess-1").await;

        let calls = applied.lock().await.clone();
        assert_eq!(
            calls,
            vec![("fast".to_string(), "haiku".to_string())],
            "mode 硬失败后 fast 回放应继续: {calls:?}"
        );
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
            None,
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
                && !ev
                    .get("thought")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
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
        assert!(
            sent_second_permit,
            "queued prompt should auto-send after first turn"
        );
        assert_eq!(text_chunks, 2, "both turns should stream");
        let queued = events.iter().filter(|e| e["type"] == "queued").count();
        assert_eq!(queued, 1, "busy queue should push a queued frame");
        assert_eq!(
            events.last().unwrap()["type"],
            "done",
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
            None,
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
            None,
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
            None,
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
                && !ev
                    .get("thought")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
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

    /// cancel 路径 flush：回合缓冲的 assistant 文本/thought 在取消时仍正确落库。
    /// `on_receiving_result`（PromptResponse 到达，含 cancelled）先 flush 已有缓冲
    /// ——用户能看到的那部分回合过程可追溯，且思考/正文顺序不被颠倒（M11 补测）。
    #[tokio::test]
    async fn test_cancel_flushes_buffered_turn_segments() {
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
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db.clone());

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
            None,
        )
        .await;

        bridge
            .prompt("sess-1", "hello")
            .await
            .expect("prompt should send");
        // 消费 mock 的流式通知（assistant_chunk/tool_call/tool_result/thought/plan）
        // ——tool_call/plan 边界已各自 flush 掉对应缓冲段，缓冲此时为空。
        for _ in 0..5 {
            tokio::time::timeout(Duration::from_secs(5), ws_rx.recv())
                .await
                .expect("timed out waiting for stream")
                .expect("closed");
        }

        // 回合进行中（mock 在 permit 上等待）补入未到边界的缓冲段：模拟最后一个
        // 工具之后的思考→正文尾部（无 tool_call/plan 触发边界 flush）。
        {
            let mut s = bridge.sessions.lock().await;
            let agent = s.get_mut("sess-1").unwrap();
            agent.turn_segments = vec![
                TurnSegment {
                    thought: true,
                    content: "尾部思考".into(),
                    parent_tool_call_id: None,
                },
                TurnSegment {
                    thought: false,
                    content: "尾部正文".into(),
                    parent_tool_call_id: None,
                },
            ];
        }

        // 触发 cancel（真实路径：记代数 + 发 session/cancel + 兜底任务）
        bridge.cancel("sess-1").await;
        // 放行 PromptResponse（stop_reason=cancelled）→ 终态回调 flush 缓冲
        permit_tx.send(()).await.unwrap();
        wait_until(Duration::from_secs(2), async || {
            !bridge.sessions.lock().await.get("sess-1").unwrap().busy
        })
        .await;

        let rows = db.agent_list_messages("sess-1").await.unwrap();
        // 缓冲段已按顺序落库：思考行先行、正文随后（rowid 顺序 = 对话顺序）
        let tail: Vec<(bool, String)> = rows
            .iter()
            .filter(|r| r.kind == "message")
            .map(|r| (r.name.as_deref() == Some("thought"), r.content.clone()))
            .collect();
        assert!(
            tail.contains(&(true, "尾部思考".to_string())),
            "cancel 后缓冲 thought 段应落库: {tail:?}"
        );
        assert!(
            tail.contains(&(false, "尾部正文".to_string())),
            "cancel 后缓冲正文段应落库: {tail:?}"
        );
        let thought_pos = tail
            .iter()
            .position(|(t, c)| *t && c == "尾部思考")
            .unwrap();
        let text_pos = tail
            .iter()
            .position(|(t, c)| !*t && c == "尾部正文")
            .unwrap();
        assert!(thought_pos < text_pos, "思考段必须先于正文段落库: {tail:?}");
        // 缓冲已清空（flush 后 turn_segments 归零）
        assert!(
            bridge
                .sessions
                .lock()
                .await
                .get("sess-1")
                .unwrap()
                .turn_segments
                .is_empty(),
            "flush 后缓冲应清空"
        );
        // 被取消的回合不发生产者终态帧（stopped 已由 WS handler 回发）
        let stale = tokio::time::timeout(Duration::from_millis(300), ws_rx.recv()).await;
        assert!(
            matches!(stale, Err(_) | Ok(None)),
            "cancelled turn must not emit a terminal frame: {stale:?}"
        );
    }

    // ── ACP 会话上下文持久化：session/resume + session/delete ──

    /// 建 workspace w1 + session sess-1（`agent_set_acp_session_id` 落库需要
    /// session 行；`agent_get_session` 断言同样依赖它）。
    async fn seeded_bridge() -> (AcpBridge, Database) {
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
            None,
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
            db.agent_get_session("sess-1")
                .await
                .unwrap()
                .unwrap()
                .acp_session_id,
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
            None,
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
            db.agent_get_session("sess-1")
                .await
                .unwrap()
                .unwrap()
                .acp_session_id,
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
            None,
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
            db.agent_get_session("sess-1")
                .await
                .unwrap()
                .unwrap()
                .acp_session_id,
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
            None,
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
            None,
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
