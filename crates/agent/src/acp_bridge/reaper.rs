//! idle reaper 与活动追踪：空闲超时的 ACP agent 进程回收。
//!
//! 超 `IDLE_TIMEOUT` 未活动（prompt/cancel/stdio/ACP 通知）即移除会话表条目并
//! 经 registry 下发进程退出语义（AgentExecCancel request_id = session_id，
//! 客户端 spawn manager 终止对应进程）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

use super::store::flush_acp_turn_buffers;
use super::{AcpBridge, SpawnedAgent};

/// 回合超时广播（status 帧）：看门狗触发 cancel 前给用户明确原因。
/// 独立小函数而非直接调 session 的 broadcast_ws_frame（其为 session 模块
/// 私有）：reaper 只需这一帧，不值得把广播器提升为跨模块公共项——这里经
/// 同一 ws_conns 结构手动 fan-out，语义与 broadcast_ws_frame(must_deliver=true)
/// 一致（阻塞发送 + 刷新 last_activity）。
async fn broadcast_turn_timeout(sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>, sid: &str) {
    let frame = serde_json::json!({
        "type": "status",
        "message": "回合超时（30 分钟无响应），已自动取消"
    });
    let conns: Vec<mpsc::Sender<serde_json::Value>> = {
        let mut map = sessions.lock().await;
        let Some(a) = map.get_mut(sid) else {
            return;
        };
        a.last_activity = std::time::Instant::now();
        a.ws_conns.iter().map(|(_, tx)| tx.clone()).collect()
    };
    for tx in conns {
        let _ = tx.send(frame.clone()).await;
    }
}

/// 空闲 30 分钟杀进程（重挂 ACP 连接由客户端 spawn manager 处理）。
const IDLE_TIMEOUT: Duration = Duration::from_mins(30);
/// reaper 检查间隔。
const REAP_INTERVAL: Duration = Duration::from_mins(1);
/// 回合级看门狗：单回合超此时长即触发 session/cancel（agent 在
/// `cancel_grace` 内不响应则复用 cancel 兜底杀进程）。模型/上游网络挂起
/// 时避免会话永久 busy——此前只能靠 idle reaper（30min）兜底，且活动刷新
/// 可能让挂起回合永远不被回收。30 分钟对长任务（大重构/批量文件生成）
/// 足够宽容，对真挂起（无任何事件到达）仍能及时回收（原 10 分钟过短，长
/// 会话常被误杀）。
const TURN_TIMEOUT: Duration = Duration::from_mins(30);

impl AcpBridge {
    /// 后台回收空闲 ACP agent：超 `IDLE_TIMEOUT` 未活动即移除会话表条目并
    /// 经 registry 下发进程退出语义（AgentExecCancel request_id = session_id，
    /// 客户端 spawn manager 终止对应进程）。
    ///
    /// 同一循环兼任回合级看门狗：busy 回合超 `TURN_TIMEOUT` 即触发 cancel
    /// （agent 在 cancel_grace 内不响应则由 cancel 的兜底任务真杀进程）。
    pub(super) fn start_idle_reaper(&self) {
        let sessions = self.sessions.clone();
        let spawner = self.spawner.clone();
        let db = self.db.clone();
        #[cfg(feature = "rag")]
        let memory = self.memory.clone();
        let watchdog = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAP_INTERVAL).await;
                // ── 回合级看门狗：先收超时回合并逐个触发 cancel（锁外调用，
                // cancel 内部自取锁）。最后活动时间在回合进行中由事件帧持续刷新，
                // 但 turn_started_at 只看回合起点，不受刷新影响。
                let timed_out: Vec<String> = sessions
                    .lock()
                    .await
                    .iter()
                    .filter(|(_, a)| {
                        a.busy
                            && !a.exited
                            && a.turn_started_at
                                .is_some_and(|t| t.elapsed() > TURN_TIMEOUT)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in timed_out {
                    tracing::warn!(
                        session_id = %id,
                        "ACP turn exceeded TURN_TIMEOUT ({}s); cancelling",
                        TURN_TIMEOUT.as_secs()
                    );
                    // 广播先于 cancel：cancel 的兜底路径发 cancel_fallback 帧，
                    // 先给用户一条明确的超时原因。
                    broadcast_turn_timeout(&sessions, &id).await;
                    watchdog.cancel(&id).await;
                }
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
                    let still_stale = {
                        let guard = sessions.lock().await;
                        guard
                            .get(&id)
                            .is_some_and(|a| a.last_activity.elapsed() > IDLE_TIMEOUT)
                    };
                    if !still_stale {
                        continue;
                    }
                    // 锁外 flush 回合缓冲：进行中回合未到终态的流式文本落库
                    // （与 drop_client_sessions/kill 一致），避免用户已看到的输出
                    // 在空闲回收后丢库。flush 内部会再锁 sessions，必须在上面的
                    // 复查锁释放后调用（tokio::sync::Mutex 不可重入）。
                    flush_acp_turn_buffers(&db, &sessions, &id).await;
                    // AI 记忆蒸馏（idle 触发）：flush 后内容完整再蒸馏。CAS 防重。
                    #[cfg(feature = "rag")]
                    if let Some(memory) = memory.as_ref() {
                        crate::memory::distill::trigger_distill(memory, &id, "idle").await;
                    }
                    // 移除条目并取 client_id（真杀进程锁外发送）。
                    let client_id = {
                        let mut guard = sessions.lock().await;
                        guard.remove(&id).map(|a| a.client_id)
                    };
                    tracing::info!(session_id = %id, "evicted idle ACP session");
                    if let Some(client_id) = client_id {
                        spawner.send_agent_cancel(&client_id, &id).await;
                    }
                }
            }
        });
    }
}

/// 刷新会话活动时间并返回条目是否存在。与 `session::current_ws_tx` 的锁内刷新
/// 语义一致，但不读 `ws_tx`——通知处理器用它对会话做存活守卫：条目在
/// （即使断线 `ws_tx=None`）就继续落库，条目被 kill/reaper 回收后返回
/// false 放弃本帧（落库与推送都依赖会话条目，回收后两者都无意义）。
/// 断线期间不依赖 WS 通道存活即落库，是「断线期间后台跑完的回合同样可
/// 追溯」的前提（评审修复：persist 移出 ws_tx guard 之前）。
pub(super) async fn touch_activity(
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
