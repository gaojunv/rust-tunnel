//! 会话配置项：set_config_option 与配置快照广播。


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

use crate::db::agent::AgentWorkspaceRecord;

use super::super::{
    AcpBridge, CONFIG_OPTION_TIMEOUT,
};

use super::*;

impl AcpBridge {
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
            Err(_) => {
                // 超时对账：agent 可能实际已生效但响应丢失，也可能未生效——
                // 无论哪种，都把内存中的权威快照广播给前端，让其收敛回真实
                // 状态（前端 optimistic UI 得以回滚）。
                self.broadcast_config_snapshot(session_id).await;
                Err(format!("set_config_option timed out: {config_id}"))
            }
            Ok(inner) => {
                if let Err(e) = inner {
                    // 错误路径同样对账（agent 显式拒绝时快照即旧值）。
                    self.broadcast_config_snapshot(session_id).await;
                    return Err(format!("set_config_option failed: {e}"));
                }
                Ok(())
            }
        }
    }

    /// 把会话内存中的 config_options 快照以 `config_option_update` 帧广播给
    /// 当前 WS 连接（best-effort）——用于 set_config_option 超时/失败后的对账。
    async fn broadcast_config_snapshot(&self, session_id: &str) {
        let options = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(session_id)
                .map(|a| a.config_options.clone())
        };
        let Some(options) = options else { return };
        if options.is_empty() {
            return;
        }
        let frame = serde_json::json!({
            "type": "config_option_update",
            "options": options,
        });
        if let Some(ws_tx) = current_ws_tx(&self.sessions, session_id).await {
            let _ = ws_tx.try_send(frame);
        }
    }

    /// 握手成功后注入 workspace 级 ACP 引擎选项覆盖（`agent_config_overrides`，
    /// JSON map：config_id → value）。先于 [`Self::replay_config_state`] 执行——
    /// session 级 config_state（用户显式选择）回放覆盖 workspace 默认。
    /// config_id 按字典序（`mode` 提前，与回放一致）逐项 set；agent 未暴露的
    /// config_id 或单条失败仅 warn 跳过，不阻断会话建立与其余项注入。
    pub(crate) async fn apply_config_overrides(&self, session_id: &str, workspace: &AgentWorkspaceRecord) {
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
    pub(crate) async fn replay_config_state(&self, session_id: &str) {
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

}
