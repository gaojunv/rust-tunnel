//! AI agent workbench: server-side agent loop, tool execution over tunnel.

pub mod acp_bridge;
pub mod acp_events;
pub mod approval;
pub mod compact;
pub mod executor;
pub mod llm_bridge;
pub mod runner;
pub mod session;
pub mod spawner;
pub mod sse;
pub mod title;
pub mod tools;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::client_registry::ClientRegistry;
use crate::db::Database;

use self::acp_bridge::AcpBridge;
use self::llm_bridge::LlmGatewayEndpoint;
use self::spawner::AgentSpawner;

/// 挂起的审批请求表：`request_id` → (工具名, 唤醒 `sender`)。别名化避免
/// `clippy::type_complexity` 在多层嵌套字段上触发。
type PendingApprovals = HashMap<String, (String, oneshot::Sender<bool>)>;

/// `request_approval` 的清理 guard：future 被 drop（cancel/断连）时移除 pending 条目，
/// 防止泄漏。正常完成时通过 [`Self::disarm`] 避免重复移除（无害但省一次锁）。
struct ApprovalGuard {
    approvals: Arc<Mutex<PendingApprovals>>,
    request_id: String,
    armed: bool,
}

impl ApprovalGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ApprovalGuard {
    fn drop(&mut self) {
        if self.armed {
            let approvals = self.approvals.clone();
            let id = std::mem::take(&mut self.request_id);
            // Drop 不能 await：try_lock 失败则 spawn 异步清理（锁竞争极短，几乎不会失败）。
            // try_lock 的 Result 持有借用直至被显式 drop，先绑定再用 drop 释放以允许 move。
            let lock = approvals.try_lock();
            if let Ok(mut map) = lock {
                map.remove(&id);
            } else {
                drop(lock);
                tokio::spawn(async move {
                    approvals.lock().await.remove(&id);
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
    /// "本会话允许此类工具"记忆集：`session_id` → 工具名集合。内存态，进程重启清零。
    session_allowed: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    /// 进行中的 exec：workspace_id → request_id。WS cancel/断连时据此把取消
    /// 信号下发到客户端。锁短持有（仅索引），与 workspace_locks 分离。
    exec_inflight: Arc<Mutex<HashMap<String, String>>>,
    /// ACP 远程 agent 会话桥（配置了 agent_type 的 workspace 走 ACP 路径）。
    /// 惰性 spawn + 事件映射 + LLM 代理路由；控制循环的 AgentSpawnData/Exit、
    /// AgentLlmProxyRequest 经它路由。
    pub acp_bridge: Option<AcpBridge>,
}

impl AgentState {
    pub fn new(registry: ClientRegistry, db: Database) -> Self {
        // ACP 桥构造在前、AgentState 尚不完整：先建占位（acp_bridge=None），
        // 用克隆（仅用于审批，不触碰 acp_bridge）注入审批回调后回填。
        let mut state = Self {
            registry: registry.clone(),
            db: db.clone(),
            workspace_locks: Arc::new(Mutex::new(HashMap::new())),
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            session_allowed: Arc::new(Mutex::new(HashMap::new())),
            exec_inflight: Arc::new(Mutex::new(HashMap::new())),
            acp_bridge: None,
        };
        // 审批走 AgentState::request_approval（与 runner 共用审批弹层/pending map；
        // 克隆只有 acp_bridge=None，request_approval 不依赖它）。
        let approval_agent = state.clone();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db).with_approval(Arc::new(
            move |session_id, tool, summary, args_preview, ws_tx| {
                let agent = approval_agent.clone();
                Box::pin(async move {
                    agent
                        .request_approval(&session_id, &tool, &summary, &args_preview, &ws_tx)
                        .await
                })
            },
        ));
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
    pub fn with_acp_cipher(
        mut self,
        cipher: Option<crate::llm::crypto::LlmCipher>,
    ) -> Self {
        if let Some(bridge) = self.acp_bridge.take() {
            self.acp_bridge = Some(bridge.with_cipher(cipher));
        }
        self
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

    /// 发审批请求帧并挂起等待用户响应。返回是否批准；发送失败（前端已断开）、
    /// 5 分钟超时、连接取消（sender 被 drop）一律视为 deny。发送失败立即返回，
    /// 不等超时——否则调用方（ACP 连接任务的请求处理器）会被占用 5 分钟，
    /// 阻塞 agent 下一个工具调用。
    pub async fn request_approval(
        &self,
        session_id: &str,
        tool: &str,
        summary: &str,
        args_preview: &str,
        ws_tx: &mpsc::Sender<serde_json::Value>,
    ) -> bool {
        let request_id = format!("{:032x}", rand::random::<u128>());
        let (tx, rx) = oneshot::channel();
        self.approvals
            .lock()
            .await
            .insert(request_id.clone(), (tool.to_string(), tx));
        // 清理 guard：future 被 drop（cancel/断连）时兜底移除 pending 条目，防止泄漏。
        let mut guard = ApprovalGuard {
            approvals: self.approvals.clone(),
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
            }))
            .await
            .is_ok();
        if !send_ok {
            // 审批弹层无法送达（前端已断开/通道关闭）：直接视为拒绝并立即返回，
            // 不等待 5 分钟超时。pending 条目在此显式清理（guard 同时 disarmed）。
            self.approvals.lock().await.remove(&request_id);
            guard.disarm();
            return false;
        }
        let approved = matches!(
            tokio::time::timeout(std::time::Duration::from_mins(5), rx).await,
            Ok(Ok(true))
        );
        self.approvals.lock().await.remove(&request_id);
        guard.disarm();
        let _ = session_id; // 预留：审计日志可按 session 记录
        approved
    }

    /// `WS` 收到 `approval_response` 时唤醒对应挂起。未知 `id` 静默忽略（可能已超时清除）。
    /// `remember=true` 且批准时，把工具名记入 `session_allowed`（本会话同工具免审批）。
    pub async fn resolve_approval(
        &self,
        session_id: &str,
        request_id: &str,
        approved: bool,
        remember: bool,
    ) {
        let entry = self.approvals.lock().await.remove(request_id);
        if let Some((tool, tx)) = entry {
            if approved && remember {
                self.remember_for_session(session_id, &tool).await;
            }
            let _ = tx.send(approved);
        }
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
}
