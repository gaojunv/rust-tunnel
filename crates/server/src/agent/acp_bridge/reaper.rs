//! idle reaper 与活动追踪：空闲超时的 ACP agent 进程回收。
//!
//! 超 `IDLE_TIMEOUT` 未活动（prompt/cancel/stdio/ACP 通知）即移除会话表条目并
//! 经 registry 下发进程退出语义（AgentExecCancel request_id = session_id，
//! 客户端 spawn manager 终止对应进程）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use super::store::flush_acp_turn_buffers;
use super::{AcpBridge, SpawnedAgent};

/// 空闲 30 分钟杀进程（重挂 ACP 连接由客户端 spawn manager 处理）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// reaper 检查间隔。
const REAP_INTERVAL: Duration = Duration::from_secs(60);

impl AcpBridge {
    /// 后台回收空闲 ACP agent：超 `IDLE_TIMEOUT` 未活动即移除会话表条目并
    /// 经 registry 下发进程退出语义（AgentExecCancel request_id = session_id，
    /// 客户端 spawn manager 终止对应进程）。
    pub(super) fn start_idle_reaper(&self) {
        let sessions = self.sessions.clone();
        let spawner = self.spawner.clone();
        let db = self.db.clone();
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
pub(super) async fn touch_activity(sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>, sid: &str) -> bool {
    let mut map = sessions.lock().await;
    match map.get_mut(sid) {
        Some(a) => {
            a.last_activity = std::time::Instant::now();
            true
        }
        None => false,
    }
}
