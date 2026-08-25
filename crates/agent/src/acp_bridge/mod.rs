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
//! `SpawnedAgent`/`AcpBridge`）与构造器；`crate::acp_bridge::*` 的外部
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
use crate::db::agent::AgentWorkspaceRecord;
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
const CONFIG_OPTION_TIMEOUT: Duration = Duration::from_secs(30);
/// 排队消息上限：进行中回合期间 submit_prompt 入队；超出上限报错（不静默丢弃，
/// 用户能立刻知道消息未被接受）。
const MAX_PENDING_PROMPTS: usize = 20;
/// 取消宽限期默认值：`cancel()` 发出 session/cancel 后，agent 未在此时限内响应
/// PromptResponse（终态回调未清 busy）则兜底杀客户端进程。
const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(10);
/// 排队等待发送的用户 prompt（进行中回合时经 [`AcpBridge::submit_prompt`] 暂存）。
/// `content` 是注入 @引用后的完整消息（mgmt/api/agent.rs 分派前已 `inject_refs`，
/// refs 内容已内联）；`refs` 原样留存备查。FIFO：终态回调逐个取出续跑，队列排空才
/// 发 done。入队即落 `agent_pending_prompts` 表（`persist_id` 为行 id，None = 落库
/// 失败的纯内存降级），取出执行时删行——重启/reaper 回收后 ensure_session 从 DB
/// 恢复队列，排队消息不再丢失。
#[derive(Clone)]
struct PendingPrompt {
    content: String,
    /// @引用路径列表，随消息原样留存备查（调用方在分派前已 inject_refs，
    /// refs 内容已内联进 `content`，本字段仅作记录不参与运行）。
    #[allow(dead_code)]
    refs: Vec<String>,
    /// `agent_pending_prompts` 行 id（持久化副本）；None = 落库失败的纯内存项。
    persist_id: Option<String>,
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
    /// 当前回合开始时间（`prompt_inner` 置 busy 时记录）：终态 done 帧携带
    /// `duration_ms` 供前端展示回合耗时；终态回调取出即清除。
    turn_started_at: Option<std::time::Instant>,
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
    /// ACP 可用命令列表快照（available_commands_update 全量替换）。
    /// 空 Vec = agent 未上报命令；新 WS 连接建立时补发一次。
    available_commands: Vec<agent_client_protocol::schema::v1::AvailableCommand>,
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
    /// Wiki 清单注入缓存（同 skill_list_block 模式）：None = 尚未检索；Some("") =
    /// 无可见 wiki；Some(非空) = 已注入 `<wikis>` 块。`prompt_inner` 与前两块一并
    /// prepend；distill 渲染也会剥离 `<wikis>` 块，无回环。
    wiki_list_block: Option<String>,
    /// 本会话 MCP 端点访问令牌（`/mcp/<token>` 路径）。ensure_session 铸造；
    /// 条目移除（kill/重拉/reaper）即吊销。仅 rag + memory 注入时 Some。
    /// 读取只在 rag 门控的 `/mcp/` 隧道（`handle_mcp_tunnel`）里；字段保持不
    /// cfg（非 rag 构建占位插入统一构造），故非 rag 下视为不可读（同 PendingPrompt
    /// refs 的记录保留语义）。
    #[allow(dead_code)]
    mcp_token: Option<String>,
    /// 文件内容 SHA-256 哈希缓存（绝对路径 → hex），用于 WriteFile2 stale 检测。
    /// `exec_fs_read` 成功返回时记录；`exec_fs_write` 发 WriteFile2 时读取作
    /// `expected_hash`；写入成功（WriteOutcome.file_hash）刷新；stale 错误清除。
    file_hashes: HashMap<String, String>,
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
    /// 创建 ACP 会话桥。
    #[must_use]
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

    /// 当前会话的可用命令快照（WS 连接建立后主动推送用）；未就绪返回 None。
    pub async fn session_available_commands(
        &self,
        session_id: &str,
    ) -> Option<Vec<agent_client_protocol::schema::v1::AvailableCommand>> {
        let sessions = self.sessions.lock().await;
        let agent = sessions.get(session_id)?;
        if agent.acp_session_id.is_none() || agent.available_commands.is_empty() {
            return None;
        }
        Some(agent.available_commands.clone())
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

    /// 读会话的 Wiki 清单注入缓存（同 skill）。WS handler 首条消息检索后写入；
    /// `prompt_inner` 与前两块一并 prepend。
    pub async fn cached_wiki_list_block(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|a| a.wiki_list_block.clone())
    }

    /// 写会话的 Wiki 清单注入缓存。语义与 [`Self::set_skill_list_block`] 一致。
    pub async fn set_wiki_list_block(&self, session_id: &str, block: Option<String>) {
        if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
            a.wiki_list_block = block;
        }
    }
}

#[cfg(test)]
mod tests;
