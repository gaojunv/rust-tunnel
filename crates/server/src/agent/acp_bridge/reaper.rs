//! idle reaper 与活动追踪：空闲超时的 ACP agent 进程回收。
//!
//! 超 `IDLE_TIMEOUT` 未活动（prompt/cancel/stdio/ACP 通知）即移除会话表条目并
//! 经 registry 下发进程退出语义（AgentExecCancel request_id = session_id，
//! 客户端 spawn manager 终止对应进程）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

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
