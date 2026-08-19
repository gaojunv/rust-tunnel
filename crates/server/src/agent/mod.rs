//! AI agent workbench: server-side agent loop, tool execution over tunnel.

pub mod acp_bridge;
pub mod acp_events;
pub mod approval;
pub mod compact;
pub mod executor;
pub mod git_plan;
pub mod github;
pub mod llm_bridge;
#[cfg(feature = "rag")]
pub mod mcp;
#[cfg(feature = "rag")]
pub mod memory;
#[cfg(feature = "rag")]
pub mod skill;
pub mod notify;
pub mod roles;
pub mod runner;
pub mod session;
pub mod spawner;
pub mod sse;
pub mod title;
pub mod tool_result;
pub mod tools;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use agent_client_protocol::schema::v1::ElicitationContentValue;
use serde::{Deserialize, Serialize};

use crate::client_registry::ClientRegistry;
use crate::db::Database;

use self::acp_bridge::AcpBridge;
use self::llm_bridge::LlmGatewayEndpoint;
use self::spawner::AgentSpawner;

/// elicitation 等待超时（与审批一致 5 分钟；超时按 `Cancel` 回 agent，前端表单
/// 卡悬停过久同样取消，agent 不卡死）。
const ELICITATION_TIMEOUT: Duration = Duration::from_mins(5);

/// 审批卡片上展示的选项（ACP `session/request_permission` 透传；runner 路径为空）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalOption {
    pub id: String,
    pub label: String,
    /// 选项类别：allow_once / allow_always / reject_once / reject_always / 自定义。
    /// 前端据此决定按钮样式；allow_always 点击时附带 remember 语义。
    pub kind: String,
}

/// 审批结果：runner / 无选项路径用 `Approved` / `Denied`；ACP 有选项路径用户选中
/// 具体选项时返回 `Selected(option_id)`，由调用方原样回传给 agent。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResult {
    Approved,
    Denied,
    Selected(String),
}

impl ApprovalResult {
    /// 是否放行（runner 判断用；runner 不传 options，只产生 Approved/Denied）。
    pub fn approved(&self) -> bool {
        matches!(self, Self::Approved)
    }
}

/// 挂起请求表结构（审批 / elicitation 共用）：`request_id` → (标识, 唤醒 `sender`)。
/// 别名化避免 `clippy::type_complexity` 在多层嵌套字段上触发。
type PendingMap<V> = HashMap<String, (String, oneshot::Sender<V>)>;

/// 挂起的审批请求表：`request_id` → (工具名, 唤醒 `sender`)。
type PendingApprovals = PendingMap<ApprovalResult>;

/// `request_elicitation` 的用户响应结果（与 `ApprovalResult` 并列）。`Accept` 携带
/// 与 `requested_schema` 匹配的字段值（可空）；`Decline` 用户跳过；`Cancel` 会话已
/// 回收 / 断线 / 超时 / 发送失败等非用户主动选择。镜像审批链路，前端表单卡三按钮
/// 各自映射。
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationResult {
    /// 用户接受并填表：content 为与 `requested_schema` 匹配的字段值（可空）。
    Accept(Option<BTreeMap<String, ElicitationContentValue>>),
    /// 用户跳过。
    Decline,
    /// 取消（会话已回收 / 断线 / 超时 / 发送失败）。
    Cancel,
}

/// 挂起的 elicitation 请求表：`request_id` → (session_id, 唤醒 `sender`)。
type PendingElicitations = PendingMap<ElicitationResult>;

/// 挂起请求的清理 guard（审批 / elicitation 共用）：future 被 drop（cancel/断连）
/// 时移除 pending 条目，防止泄漏。正常完成时通过 [`Self::disarm`] 避免重复移除
/// （无害但省一次锁）。
struct PendingGuard<V: Send + 'static> {
    pending: Arc<Mutex<PendingMap<V>>>,
    request_id: String,
    armed: bool,
}

impl<V: Send + 'static> PendingGuard<V> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<V: Send + 'static> Drop for PendingGuard<V> {
    fn drop(&mut self) {
        if self.armed {
            let pending = self.pending.clone();
            let id = std::mem::take(&mut self.request_id);
            // Drop 不能 await：try_lock 失败则 spawn 异步清理（锁竞争极短，几乎不会失败）。
            // try_lock 的 Result 持有借用直至被显式 drop，先绑定再用 drop 释放以允许 move。
            let lock = pending.try_lock();
            if let Ok(mut map) = lock {
                map.remove(&id);
            } else {
                drop(lock);
                tokio::spawn(async move {
                    pending.lock().await.remove(&id);
                });
            }
        }
    }
}

/// Shared agent state, hung on `ServerState`.
#[derive(Clone)]
pub struct AgentState {
    pub registry: ClientRegistry,
    pub db: Database,
    /// Per-workspace serialization of command execution (git state safety)
    workspace_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Per-session serialization of agent turns：同一 session 的多个 WS 连接
    /// （多标签页/重连叠旧连接未断）各自跑 turn 会并发写库、消息交错、
    /// tool_call_id 失配，回合级互斥防止上下文被写坏。
    session_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// 等待用户审批的工具调用：`request_id` → (工具名, 唤醒 `sender`)。
    /// 跨 `runner`/`WS` 读循环共享（挂 `AgentState` 而非单连接）。
    approvals: Arc<Mutex<PendingApprovals>>,
    /// 等待用户响应的 elicitation（AskUserQuestion 表单）：`request_id` →
    /// (session_id, 唤醒 `sender`)。与 approvals 同模式挂 `AgentState`；仅 ACP
    /// 路径产生（runner 无表单概念），WS 外层读循环按 `acp_active` 门控分发。
    elicitations: Arc<Mutex<PendingElicitations>>,
    /// "本会话允许此类工具"记忆集：`session_id` → 工具名集合。内存态，进程重启清零。
    session_allowed: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    /// 进行中的 exec：workspace_id → request_id。WS cancel/断连时据此把取消
    /// 信号下发到客户端。锁短持有（仅索引），与 workspace_locks 分离。
    exec_inflight: Arc<Mutex<HashMap<String, String>>>,
    /// ACP 远程 agent 会话桥（配置了 agent_type 的 workspace 走 ACP 路径）。
    /// 惰性 spawn + 事件映射 + LLM 代理路由；控制循环的 AgentSpawnData/Exit、
    /// AgentLlmProxyRequest 经它路由。
    pub acp_bridge: Option<AcpBridge>,
    /// AI 记忆体运行时（蒸馏/注入/remember 共用）。仅 `rag` feature 下存在；
    /// `server.rs` 在 `init_llm_state` 之后经 [`Self::with_memory`] 注入。
    #[cfg(feature = "rag")]
    pub memory: Option<memory::MemoryState>,
    /// 工作台全局通知广播：任务完成 / 出错 / 需用户干预时，`push_task`
    /// （`mgmt/api/agent/ws.rs`）把出站帧翻译成 [`notify::AgentNotification`]
    /// 发布于此，浏览器全局通知 WS（`/api/agent/notifications/ws`）订阅消费。
    notifications: broadcast::Sender<notify::AgentNotification>,
    /// GitHub REST API base URL（可注入覆盖，测试指向本地 axum mock；生产保持
    /// 默认 [`crate::agent::github::GITHUB_API_BASE`]）。
    github_base_url: String,
}

impl AgentState {
    pub fn new(registry: ClientRegistry, db: Database) -> Self {
        // 通知广播先建（在 AcpBridge 构造之前，无循环依赖问题）；订阅者即浏览器的
        // 全局通知 WS。容量 256 足够覆盖短时突发（通知低频，只发不阻塞）。
        let (notify_tx, _notify_rx) = broadcast::channel::<notify::AgentNotification>(256);
        // ACP 桥构造在前、AgentState 尚不完整：先建占位（acp_bridge=None），
        // 用克隆（仅用于审批，不触碰 acp_bridge）注入审批回调后回填。
        let mut state = Self {
            registry: registry.clone(),
            db: db.clone(),
            workspace_locks: Arc::new(Mutex::new(HashMap::new())),
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            elicitations: Arc::new(Mutex::new(HashMap::new())),
            session_allowed: Arc::new(Mutex::new(HashMap::new())),
            exec_inflight: Arc::new(Mutex::new(HashMap::new())),
            acp_bridge: None,
            #[cfg(feature = "rag")]
            memory: None,
            notifications: notify_tx,
            github_base_url: crate::agent::github::GITHUB_API_BASE.to_string(),
        };
        // 审批走 AgentState::request_approval（与 runner 共用审批弹层/pending map；
        // 克隆只有 acp_bridge=None，request_approval 不依赖它）。
        let approval_agent = state.clone();
        let elicitation_agent = state.clone();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db)
            .with_approval(Arc::new(
                move |session_id, tool, summary, args_preview, options, ws_tx| {
                    let agent = approval_agent.clone();
                    Box::pin(async move {
                        agent
                            .request_approval(
                                &session_id,
                                &tool,
                                &summary,
                                &args_preview,
                                &options,
                                &ws_tx,
                            )
                            .await
                    })
                },
            ))
            // elicitation 走 AgentState::request_elicitation（与审批同理由：克隆
            // 只有 acp_bridge=None，request_elicitation 不依赖它）。AskUserQuestion
            // 表单经 `elicitation_request` 帧推前端，用户响应经 `elicitation_response`
            // 回传（WS 外层循环分发）。
            .with_elicitation(Arc::new(move |session_id, message, schema, ws_tx| {
                let agent = elicitation_agent.clone();
                Box::pin(async move {
                    agent
                        .request_elicitation(&session_id, &message, &schema, &ws_tx)
                        .await
                })
            }));
        state.acp_bridge = Some(bridge);
        state
    }

    /// 把 LLM 网关的主密钥解密器注入 ACP 桥。
    ///
    /// `AgentState::new`（`ServerState::with_db`）时 LLM 网关尚未初始化、拿不到
    /// `LlmState.cipher`；生产启动在 `init_llm_state` 之后调用本方法补注。主密钥
    /// 配置后（默认生产路径）provider API Key 落库加密，agent LLM 代理请求必须
    /// 在服务端解密后才能调上游——缺注会全部 502。
    #[must_use]
    pub fn with_acp_cipher(mut self, cipher: Option<crate::llm::crypto::LlmCipher>) -> Self {
        if let Some(bridge) = self.acp_bridge.take() {
            self.acp_bridge = Some(bridge.with_cipher(cipher));
        }
        self
    }

    /// 覆盖 GitHub REST API base URL（测试注入本地 mock；生产保持默认）。
    #[must_use]
    pub fn with_github_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.github_base_url = base_url.into();
        self
    }

    /// GitHub REST API base URL（默认 `https://api.github.com`）。
    pub fn github_base_url(&self) -> &str {
        &self.github_base_url
    }

    /// 把 LLM 网关入口（内部回环地址 + API key + 双协议域名）注入 ACP 桥。
    ///
    /// 注入后 agent LLM 代理请求经内部 HTTP 回环走网关全管线（模型组故障转移、
    /// 格式转换、用量统计、RAG 注入等），而非直接透传到上游。生产启动在
    /// `init_llm_state` 之后调用本方法补注；缺注全部 502。
    #[must_use]
    pub fn with_llm_gateway(mut self, gateway: LlmGatewayEndpoint) -> Self {
        if let Some(bridge) = self.acp_bridge.take() {
            self.acp_bridge = Some(bridge.with_llm_gateway(gateway));
        }
        self
    }

    /// 把 AI 记忆体运行时挂到 AgentState（蒸馏/注入/remember 经它访问 DB、向量
    /// 与设置）。`MemoryState` 里的 `store` 必须与 `LlmState.rag_store` 同一实例，
    /// 由 `server.rs` 在 `init_llm_state` 之后构造注入。同步注入 ACP 桥
    /// （kill/断线/idle 蒸馏触发用），共享同一实例。
    #[cfg(feature = "rag")]
    #[must_use]
    pub fn with_memory(mut self, memory: memory::MemoryState) -> Self {
        if let Some(bridge) = self.acp_bridge.take() {
            self.acp_bridge = Some(bridge.with_memory(memory.clone()));
        }
        self.memory = Some(memory);
        self
    }

    /// Get (or create) the execution mutex for a workspace.
    pub async fn workspace_lock(&self, workspace_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.workspace_locks.lock().await;
        locks
            .entry(workspace_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Get (or create) the turn mutex for a session.
    pub async fn session_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.lock().await;
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 发审批请求帧并挂起等待用户响应。返回审批结果；发送失败（前端已断开）、
    /// 5 分钟超时、连接取消（sender 被 drop）一律视为 `Denied`。发送失败立即返回，
    /// 不等超时——否则调用方（ACP 连接任务的请求处理器）会被占用 5 分钟，
    /// 阻塞 agent 下一个工具调用。
    ///
    /// `options`：ACP `request_permission` 的选项透传（runner 路径传空切片）。
    /// 有选项时前端渲染这些选项、用户点击回传 option_id → `Selected(id)`；
    /// 无选项时保持原有 approve/deny 二元按钮 → `Approved` / `Denied`。
    pub async fn request_approval(
        &self,
        session_id: &str,
        tool: &str,
        summary: &str,
        args_preview: &str,
        options: &[ApprovalOption],
        ws_tx: &mpsc::Sender<serde_json::Value>,
    ) -> ApprovalResult {
        let request_id = format!("{:032x}", rand::random::<u128>());
        let (tx, rx) = oneshot::channel();
        self.approvals
            .lock()
            .await
            .insert(request_id.clone(), (tool.to_string(), tx));
        // 清理 guard：future 被 drop（cancel/断连）时兜底移除 pending 条目，防止泄漏。
        let mut guard = PendingGuard::<ApprovalResult> {
            pending: self.approvals.clone(),
            request_id: request_id.clone(),
            armed: true,
        };
        let send_ok = ws_tx
            .send(serde_json::json!({
                "type": "approval_request",
                "request_id": &request_id,
                "tool": tool,
                "summary": summary,
                "args_preview": args_preview,
                "options": options,
            }))
            .await
            .is_ok();
        if !send_ok {
            // 审批弹层无法送达（前端已断开/通道关闭）：直接视为拒绝并立即返回，
            // 不等待 5 分钟超时。pending 条目在此显式清理（guard 同时 disarmed）。
            self.approvals.lock().await.remove(&request_id);
            guard.disarm();
            return ApprovalResult::Denied;
        }
        let result = match tokio::time::timeout(std::time::Duration::from_mins(5), rx).await {
            Ok(Ok(result)) => result,
            _ => ApprovalResult::Denied, // 超时 / sender 被 drop（取消）
        };
        self.approvals.lock().await.remove(&request_id);
        guard.disarm();
        let _ = session_id; // 预留：审计日志可按 session 记录
        result
    }

    /// `WS` 收到 `approval_response` 时唤醒对应挂起。未知 `id` 静默忽略（可能已超时清除）。
    /// 优先按 `option_id` 解析（ACP options 透传路径：用户选中具体选项 → `Selected`）；
    /// 无 `option_id` 时按 `approved` 解析（runner / 无选项路径）。`remember=true` 且
    /// 批准时，把工具名记入 `session_allowed`（本会话同工具免审批）。
    pub async fn resolve_approval(
        &self,
        session_id: &str,
        request_id: &str,
        approved: bool,
        option_id: Option<String>,
        remember: bool,
    ) {
        let entry = self.approvals.lock().await.remove(request_id);
        if let Some((tool, tx)) = entry {
            let result = match option_id {
                Some(id) => ApprovalResult::Selected(id),
                None if approved => ApprovalResult::Approved,
                None => ApprovalResult::Denied,
            };
            // remember 语义只对"放行"有意义。前端仅在用户点击 allow_always 选项时
            // 附带 remember='session'（此时结果为 Selected），无选项 runner 路径则
            // 依赖 approved。两者命中其一即记住本会话同类工具免审批。
            if remember && (approved || matches!(result, ApprovalResult::Selected(_))) {
                self.remember_for_session(session_id, &tool).await;
            }
            let _ = tx.send(result);
        }
    }

    /// 发 elicitation 请求帧（AskUserQuestion 表单）并挂起等待用户响应。镜像
    /// [`Self::request_approval`]：发送失败（前端已断开）、5 分钟超时、连接取消
    /// （sender 被 drop）一律视为 `Cancel`。发送失败立即返回，不等超时——否则
    /// 调用方（ACP 连接任务的请求处理器）会被占用 5 分钟，阻塞 agent 下一个
    /// 工具调用。
    ///
    /// `schema`：agent 的 `elicitation/create` 请求里 `requestedSchema` 的原始
    /// JSON（复杂 serde 枚举，后端不做表单模型重建，原样透传前端渲染）。
    pub async fn request_elicitation(
        &self,
        session_id: &str,
        message: &str,
        schema: &serde_json::Value,
        ws_tx: &mpsc::Sender<serde_json::Value>,
    ) -> ElicitationResult {
        self.request_elicitation_inner(session_id, message, schema, ws_tx, ELICITATION_TIMEOUT)
            .await
    }

    /// `request_elicitation` 的 timeout 可参数化内部变体（测试传短超时；生产用
    /// 默认 5 分钟）。
    async fn request_elicitation_inner(
        &self,
        session_id: &str,
        message: &str,
        schema: &serde_json::Value,
        ws_tx: &mpsc::Sender<serde_json::Value>,
        timeout: Duration,
    ) -> ElicitationResult {
        let request_id = format!("{:032x}", rand::random::<u128>());
        let (tx, rx) = oneshot::channel();
        self.elicitations
            .lock()
            .await
            .insert(request_id.clone(), (session_id.to_string(), tx));
        // 清理 guard：future 被 drop（cancel/断连）时兜底移除 pending 条目。
        let mut guard = PendingGuard::<ElicitationResult> {
            pending: self.elicitations.clone(),
            request_id: request_id.clone(),
            armed: true,
        };
        let send_ok = ws_tx
            .send(serde_json::json!({
                "type": "elicitation_request",
                "request_id": &request_id,
                "message": message,
                "schema": schema, // 原始 schema JSON 透传前端
            }))
            .await
            .is_ok();
        if !send_ok {
            // 表单帧无法送达（前端已断开/通道关闭）：直接视为取消并立即返回，
            // 不等待超时。pending 条目在此显式清理（guard 同时 disarmed）。
            self.elicitations.lock().await.remove(&request_id);
            guard.disarm();
            return ElicitationResult::Cancel;
        }
        let result = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            _ => ElicitationResult::Cancel, // 超时 / sender 被 drop（取消）
        };
        self.elicitations.lock().await.remove(&request_id);
        guard.disarm();
        result
    }

    /// `WS` 收到 `elicitation_response` 时唤醒对应挂起。未知 `id` 静默忽略
    /// （可能已超时清除 / 不属本进程）。`action` ∈ accept/decline/cancel；
    /// accept 时把 `content` 解析为字段值对象（解析失败 warn + `Accept(None)`，
    /// 不 panic、不把错误传给 agent）。
    pub async fn resolve_elicitation(
        &self,
        _session_id: &str,
        request_id: &str,
        action: &str,
        content: Option<serde_json::Value>,
    ) {
        let entry = self.elicitations.lock().await.remove(request_id);
        let Some((_, tx)) = entry else { return };
        let result = match action {
            "accept" => {
                let parsed = content.and_then(|c| {
                    serde_json::from_value::<BTreeMap<String, ElicitationContentValue>>(c)
                        .map_err(|e| tracing::warn!(%request_id, "parse elicitation content failed: {e}"))
                        .ok()
                });
                ElicitationResult::Accept(parsed)
            }
            "decline" => ElicitationResult::Decline,
            _ => ElicitationResult::Cancel,
        };
        let _ = tx.send(result);
    }

    /// 本会话是否已记住允许此类工具（免审批记忆集命中）。
    pub async fn is_allowed_for_session(&self, session_id: &str, tool: &str) -> bool {
        self.session_allowed
            .lock()
            .await
            .get(session_id)
            .is_some_and(|set| set.contains(tool))
    }

    /// 把工具名记入本会话允许集（进程内存态，重启清零）。
    pub async fn remember_for_session(&self, session_id: &str, tool: &str) {
        self.session_allowed
            .lock()
            .await
            .entry(session_id.to_string())
            .or_default()
            .insert(tool.to_string());
    }

    /// 当前挂起的审批请求数（仅测试用：泄漏检测）。
    #[cfg(test)]
    pub(crate) async fn pending_approvals_count(&self) -> usize {
        self.approvals.lock().await.len()
    }

    /// 当前挂起的 elicitation 请求数（仅测试用：泄漏检测）。
    #[cfg(test)]
    pub(crate) async fn pending_elicitations_count(&self) -> usize {
        self.elicitations.lock().await.len()
    }

    /// 生成新 exec 的 request_id 并记入 inflight，返回 id。
    /// WS cancel 用 `inflight_take` 取走；exec 结束后 `inflight_end` 清除。
    pub async fn inflight_begin(&self, workspace_id: &str) -> String {
        let id = format!("{:032x}", rand::random::<u128>());
        self.exec_inflight
            .lock()
            .await
            .insert(workspace_id.to_string(), id.clone());
        id
    }

    /// exec 正常结束后清除（幂等）。
    pub async fn inflight_end(&self, workspace_id: &str) {
        self.exec_inflight.lock().await.remove(workspace_id);
    }

    /// 取出进行中的 exec request_id 并清除（cancel/断连时用，先取后清防重复取消）。
    pub async fn inflight_take(&self, workspace_id: &str) -> Option<String> {
        self.exec_inflight.lock().await.remove(workspace_id)
    }

    /// 广播一条工作台通知（无订阅者时静默忽略，不阻塞调用方）。
    pub fn notify(&self, n: notify::AgentNotification) {
        let _ = self.notifications.send(n);
    }

    /// 订阅工作台通知广播（浏览器全局通知 WS 用）。
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<notify::AgentNotification> {
        self.notifications.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_registry::ClientRegistry;
    use crate::db::Database;

    /// 构造一个带空 pending 表的 AgentState（elicitation 测试用）。
    async fn test_agent() -> AgentState {
        let db = Database::new(":memory:").await.unwrap();
        AgentState::new(ClientRegistry::new(db.clone()), db)
    }

    /// 发一个挂起的 elicitation（长超时，测试内 resolve/abort），并等待 pending
    /// 条目已插入。返回 (spawn 的 JoinHandle, ws_tx 通道里收到的请求帧)。
    async fn spawn_pending_elicitation(
        agent: &AgentState,
    ) -> (
        tokio::task::JoinHandle<ElicitationResult>,
        serde_json::Value,
    ) {
        let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(1);
        let agent2 = agent.clone();
        let handle = tokio::spawn(async move {
            agent2
                .request_elicitation_inner(
                    "s1",
                    "请选择",
                    &serde_json::json!({"type": "object", "properties": {}}),
                    &ws_tx,
                    Duration::from_secs(3600),
                )
                .await
        });
        // 等待 pending 条目已插入（future 已执行到 insert + 发帧，随后挂起在 rx）。
        for _ in 0..100 {
            if agent.pending_elicitations_count().await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(agent.pending_elicitations_count().await, 1);
        let frame = tokio::time::timeout(Duration::from_secs(2), ws_rx.recv())
            .await
            .expect("elicitation_request frame should be sent")
            .expect("ws channel closed");
        assert_eq!(frame["type"], "elicitation_request");
        assert_eq!(frame["message"], "请选择");
        assert!(frame["schema"].is_object());
        (handle, frame)
    }

    #[tokio::test]
    async fn test_request_elicitation_send_failure_cancels() {
        // 前端已断开（接收端 drop）→ 发帧失败 → 立即 Cancel，pending 清空，不等超时。
        let agent = test_agent().await;
        let (ws_tx, ws_rx) = mpsc::channel::<serde_json::Value>(1);
        drop(ws_rx);
        let result = agent
            .request_elicitation("s1", "请选择", &serde_json::json!({}), &ws_tx)
            .await;
        assert_eq!(result, ElicitationResult::Cancel);
        assert_eq!(agent.pending_elicitations_count().await, 0);
    }

    #[tokio::test]
    async fn test_request_elicitation_timeout_cancels() {
        // 生产默认 5 分钟；内部变体传 50ms 验证超时 Cancel（接收端不响应）。
        let agent = test_agent().await;
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(1);
        let result = agent
            .request_elicitation_inner(
                "s1",
                "请选择",
                &serde_json::json!({}),
                &ws_tx,
                Duration::from_millis(50),
            )
            .await;
        assert_eq!(result, ElicitationResult::Cancel);
        assert_eq!(agent.pending_elicitations_count().await, 0);
    }

    #[tokio::test]
    async fn test_resolve_elicitation_accept_parses_content() {
        // accept：content 按 untagged 顺序解析为 String/Integer/StringArray。
        let agent = test_agent().await;
        let (handle, frame) = spawn_pending_elicitation(&agent).await;
        let request_id = frame["request_id"].as_str().unwrap().to_string();
        agent
            .resolve_elicitation(
                "s1",
                &request_id,
                "accept",
                Some(serde_json::json!({
                    "name": "Alice",
                    "age": 3,
                    "tags": ["a", "b"],
                })),
            )
            .await;
        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("elicitation should resolve")
            .expect("spawned task panicked");
        match result {
            ElicitationResult::Accept(Some(content)) => {
                assert_eq!(
                    content.get("name"),
                    Some(&ElicitationContentValue::String("Alice".into()))
                );
                assert_eq!(
                    content.get("age"),
                    Some(&ElicitationContentValue::Integer(3))
                );
                assert_eq!(
                    content.get("tags"),
                    Some(&ElicitationContentValue::StringArray(vec![
                        "a".into(),
                        "b".into()
                    ]))
                );
            }
            other => panic!("expected Accept, got {other:?}"),
        }
        assert_eq!(agent.pending_elicitations_count().await, 0);
    }

    #[tokio::test]
    async fn test_resolve_elicitation_decline_cancel() {
        // decline / cancel 分别唤醒为 Decline / Cancel。
        for (action, expected) in [
            ("decline", ElicitationResult::Decline),
            ("cancel", ElicitationResult::Cancel),
        ] {
            let agent = test_agent().await;
            let (handle, frame) = spawn_pending_elicitation(&agent).await;
            let request_id = frame["request_id"].as_str().unwrap().to_string();
            agent
                .resolve_elicitation("s1", &request_id, action, None)
                .await;
            let result = tokio::time::timeout(Duration::from_secs(2), handle)
                .await
                .expect("elicitation should resolve")
                .expect("spawned task panicked");
            assert_eq!(result, expected);
            assert_eq!(agent.pending_elicitations_count().await, 0);
        }
    }

    #[tokio::test]
    async fn test_resolve_elicitation_unknown_id_noop() {
        // 未知 id（已超时清除 / 不属本进程）：静默忽略，不 panic、不留残留。
        let agent = test_agent().await;
        let (handle, frame) = spawn_pending_elicitation(&agent).await;
        agent
            .resolve_elicitation("s1", "no-such-id", "accept", None)
            .await;
        assert_eq!(
            agent.pending_elicitations_count().await,
            1,
            "pending must survive"
        );
        let request_id = frame["request_id"].as_str().unwrap().to_string();
        agent
            .resolve_elicitation("s1", &request_id, "cancel", None)
            .await;
        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("elicitation should resolve")
            .expect("spawned task panicked");
        assert_eq!(result, ElicitationResult::Cancel);
        assert_eq!(agent.pending_elicitations_count().await, 0);
    }

    #[tokio::test]
    async fn test_pending_guard_drop_cleans() {
        // 泄漏检测：future 被 drop（abort/cancel/断连）时，PendingGuard 兜底
        // 移除 pending 条目，不残留（等价 request_elicitation 挂起中被取消）。
        let agent = test_agent().await;
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(1);
        let agent2 = agent.clone();
        let handle = tokio::spawn(async move {
            agent2
                .request_elicitation_inner(
                    "s1",
                    "请选择",
                    &serde_json::json!({}),
                    &ws_tx,
                    Duration::from_secs(3600),
                )
                .await
        });
        for _ in 0..100 {
            if agent.pending_elicitations_count().await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(agent.pending_elicitations_count().await, 1);
        // abort 是取消信号：任务 drop 在调度后发生，轮询等待 guard 兜底清理完成。
        handle.abort();
        for _ in 0..100 {
            if agent.pending_elicitations_count().await == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            agent.pending_elicitations_count().await,
            0,
            "dropped future must clean up its pending entry"
        );
    }
}
