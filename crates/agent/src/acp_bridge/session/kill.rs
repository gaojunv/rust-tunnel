//! 会话清理：kill/kill_and_delete、WS detach、客户端断连批量回收。

use std::time::Duration;

#[allow(unused_imports)]
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, CreateElicitationRequest, CreateElicitationResponse,
    DeleteSessionRequest, ElicitationAcceptAction, ElicitationAction, ElicitationMode,
    InitializeRequest, McpServer, McpServerHttp, NewSessionRequest, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptRequest, ReadTextFileRequest,
    ReadTextFileResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, SelectedPermissionOutcome,
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigOptionValue, SessionConfigValueId, SessionId, SessionNotification,
    SetSessionConfigOptionRequest, TextContent, WriteTextFileRequest, WriteTextFileResponse,
};


use super::super::store::flush_acp_turn_buffers;
use super::super::AcpBridge;


impl AcpBridge {
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
        // 先 flush 回合缓冲：进行中回合未到终态/工具边界的流式文本落库（与
        // drop_client_sessions 一致），避免用户已看到的输出在归档/删除后丢库。
        flush_acp_turn_buffers(&self.db, &self.sessions, session_id).await;
        // AI 记忆蒸馏（归档触发）：flush 后会话内容完整再蒸馏。CAS 防重——
        // archive_session handler 的触发与这里多路并发，只有一个赢家。
        #[cfg(feature = "rag")]
        if let Some(memory) = self.memory.as_ref() {
            crate::memory::distill::trigger_distill(memory, session_id, "archive").await;
        }
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
        let mut sessions = self.sessions.lock().await;
        let Some(a) = sessions.get_mut(session_id) else {
            return;
        };
        // 从广播列表移除本连接（无论是否主通道）——流式帧不再发往已断开的连接。
        a.ws_conns.retain(|(id, _)| *id != my_conn_id);
        // 本连接是主通道（审批/elicitation 帧通道）：清空并把主通道顺延到剩余
        // 连接里最近注册的一个（ws_conns 按注册顺序 push，末尾即最近），保持
        // 审批/表单帧有主通道可发；全部断开则归 0。通知审批等待者连接已切换：
        // 值变化 → wait_for 唤醒 → 旧连接等待立即拒绝（detach 早于订阅时
        // borrow 已看到新值，同样短路拒绝）。
        if a.ws_conn_id == my_conn_id {
            a.ws_tx = None;
            a.ws_conn_id = a.ws_conns.last().map(|(id, _)| *id).unwrap_or(0);
            if let Some(tx) = a
                .ws_conns
                .iter()
                .find(|(id, _)| *id == a.ws_conn_id)
                .map(|(_, tx)| tx.clone())
            {
                a.ws_tx = Some(tx);
            }
            a.ws_conn_watch.send_replace(a.ws_conn_id);
        }
    }

    /// 客户端控制连接断开时清理该客户端的所有 ACP 会话：先 flush 回合缓冲
    /// （断线瞬间未到终态的内容也落库，刷新历史仍可追溯），再移除会话条目。
    /// 客户端进程随控制连接断开而终止，残留条目只会被 idle reaper 晚回收
    /// （30 分钟），此处在断开点即时清理，避免审批等待/缓冲长时间悬挂。
    pub async fn drop_client_sessions(&self, client_id: &str) {
        let sids: Vec<String> = self
            .sessions
            .lock()
            .await
            .iter()
            .filter(|(_, a)| a.client_id == client_id)
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in sids {
            flush_acp_turn_buffers(&self.db, &self.sessions, &sid).await;
            // AI 记忆蒸馏（断线触发）：flush 后内容完整再蒸馏。CAS 防重。
            #[cfg(feature = "rag")]
            if let Some(memory) = self.memory.as_ref() {
                crate::memory::distill::trigger_distill(memory, &sid, "disconnect").await;
            }
            self.sessions.lock().await.remove(&sid);
            tracing::info!(
                session_id = %sid,
                client_id,
                "dropped ACP session on client disconnect"
            );
        }
    }

    /// 构造全量 session_state 帧；无状态（未握手/agent 不上报）返回 None。
    /// 同时包含 available_commands（若有），确保新连接的标签页能拿到最新命令列表。
    pub(crate) async fn session_state_frame(&self, session_id: &str) -> Option<serde_json::Value> {
        let sessions = self.sessions.lock().await;
        let agent = sessions.get(session_id)?;
        if agent.acp_session_id.is_none() || agent.config_options.is_empty() {
            return None;
        }
        let mut frame = serde_json::json!({
            "type": "session_state",
            "options": agent.config_options,
        });
        if !agent.available_commands.is_empty() {
            let commands: Vec<serde_json::Value> = agent
                .available_commands
                .iter()
                .map(|cmd| {
                    serde_json::json!({
                        "name": cmd.name,
                        "description": cmd.description,
                    })
                })
                .collect();
            frame["available_commands"] = serde_json::Value::Array(commands);
        }
        Some(frame)
    }

}
