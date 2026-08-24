//! WS 事件通道：当前连接解析与帧广播。

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex};

#[allow(unused_imports)]
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, CreateElicitationRequest, CreateElicitationResponse,
    DeleteSessionRequest, ElicitationAcceptAction, ElicitationAction, ElicitationMode,
    InitializeRequest, McpServer, McpServerHttp, NewSessionRequest, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptRequest, ReadTextFileRequest,
    ReadTextFileResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigId,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigValueId, SessionId, SessionNotification, SetSessionConfigOptionRequest,
    TextContent, WriteTextFileRequest, WriteTextFileResponse,
};

use super::super::SpawnedAgent;

/// 动态解析会话当前的 WS 事件通道：重连/多标签页时 `ensure_session` 的 dedup
/// 刷新、连接关闭时 [`AcpBridge::detach_ws_tx`] 清空，都会改动条目里的
/// `ws_tx`。连接任务的通知/请求处理器**每次事件**都读最新值，避免流式帧/审批
/// 弹层推给已断开的旧连接（旧 sender 的 try_send 会静默失败，前端只看到 done）。
/// 顺带刷新 last_activity——长回合无 stdout 时不被 idle reaper 误回收。
/// 会话不存在返回 None。
pub(crate) async fn current_ws_tx(
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) -> Option<mpsc::Sender<serde_json::Value>> {
    let mut map = sessions.lock().await;
    let agent = map.get_mut(sid)?;
    agent.last_activity = std::time::Instant::now();
    agent.ws_tx.clone()
}
/// 动态解析会话当前的 WS 事件通道 + 连接标识 + 连接变化 watch（`request_permission`
/// 处理器专用）。与 [`current_ws_tx`] 同样的「每次事件读最新值」语义，额外返回
/// `ws_conn_id` 与 watch Receiver——`approve_or_disconnect` 据此在连接断开/重连时
/// 即时拒绝审批，不等满审批超时。会话不存在返回 None。
pub(crate) async fn current_ws_channel(
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) -> Option<(
    Option<mpsc::Sender<serde_json::Value>>,
    u64,
    watch::Receiver<u64>,
)> {
    let mut map = sessions.lock().await;
    let agent = map.get_mut(sid)?;
    agent.last_activity = std::time::Instant::now();
    Some((
        agent.ws_tx.clone(),
        agent.ws_conn_id,
        agent.ws_conn_watch.subscribe(),
    ))
}
/// 把帧广播到本会话**所有** WS 连接（多标签页/多窗口 fan-out）。遍历 `ws_conns`
/// 逐个发送：某连接断开/通道满只丢弃该连接，不影响其余——根治「最新连接获胜」
/// 劫持（H5）：被动打开的新标签页不再把正在运行回合的流式帧/终态帧切走，原标签页
/// 永久卡 running。审批/elicitation 请求帧不走这里（仍只发主通道 `ws_tx`，见
/// [`current_ws_channel`]——多连接下审批卡只在主连接展示，响应按 request_id
/// 全局可达，任意连接可答）。
///
/// 顺带刷新 last_activity（同 [`current_ws_tx`] 语义：长回合无 stdout 不被 idle
/// reaper 误回收）。`must_deliver` 用于终态帧（done/error）：低频、必须送达，
/// 走阻塞发送（前端在时通道很快被 push_task 排空，不会长期阻塞）；流式/状态帧
/// 用 try_send（实时流丢帧可容忍，避免慢消费端阻塞整个 ACP 连接任务）。
pub(crate) async fn broadcast_ws_frame(
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
    frame: &serde_json::Value,
    must_deliver: bool,
) {
    let conns: Vec<mpsc::Sender<serde_json::Value>> = {
        let mut map = sessions.lock().await;
        let Some(a) = map.get_mut(sid) else {
            return;
        };
        a.last_activity = std::time::Instant::now();
        a.ws_conns.iter().map(|(_, tx)| tx.clone()).collect()
    };
    for tx in conns {
        if must_deliver {
            let _ = tx.send(frame.clone()).await;
        } else if tx.try_send(frame.clone()).is_err() {
            tracing::trace!(session_id = %sid, "acp event dropped (ws channel full/closed)");
        }
    }
}
