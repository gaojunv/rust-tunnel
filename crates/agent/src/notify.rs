//! 工作台全局通知：把出站 WS 帧中「任务完成 / 出错 / 需用户干预」的事件翻译成
//! 广播通知，供浏览器全局通知 WS 订阅（前端标签闪动 + 系统通知）。
//!
//! 挂在 `AgentState::notifications` 的 `broadcast::Sender` 上；发射点在
//! `mgmt/api/agent/ws.rs` 的 `push_task`（唯一出站帧汇聚点）——runner 与 ACP
//! 两条路径的 `done`/`error`、以及审批/elicitation 请求帧都经该通道流向浏览器。

/// 广播给前端全局通知通道的事件。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentNotification {
    /// 判别字段：`turn_done` / `turn_error` / `approval_needed` / `elicitation_needed`。
    pub event: &'static str,
    pub session_id: String,
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl AgentNotification {
    pub fn turn_done(session_id: &str, workspace_id: &str) -> Self {
        Self {
            event: "turn_done",
            session_id: session_id.to_string(),
            workspace_id: workspace_id.to_string(),
            message: None,
            tool: None,
            summary: None,
        }
    }

    pub fn turn_error(session_id: &str, workspace_id: &str, message: &str) -> Self {
        Self {
            event: "turn_error",
            session_id: session_id.to_string(),
            workspace_id: workspace_id.to_string(),
            message: Some(message.to_string()),
            tool: None,
            summary: None,
        }
    }

    pub fn approval_needed(
        session_id: &str,
        workspace_id: &str,
        tool: &str,
        summary: &str,
    ) -> Self {
        Self {
            event: "approval_needed",
            session_id: session_id.to_string(),
            workspace_id: workspace_id.to_string(),
            message: None,
            tool: Some(tool.to_string()),
            summary: Some(summary.to_string()),
        }
    }

    pub fn elicitation_needed(session_id: &str, workspace_id: &str, message: &str) -> Self {
        Self {
            event: "elicitation_needed",
            session_id: session_id.to_string(),
            workspace_id: workspace_id.to_string(),
            message: Some(message.to_string()),
            tool: None,
            summary: None,
        }
    }
}

/// 把一条出站 WS 帧映射成通知事件；非通知类型返回 `None`。
///
/// 通知范围与前端判定一致：
/// - `done`（回合成功结束）→ `turn_done`
/// - `error` → `turn_error`，**但** `设置失败` 开头的 error 是 config option 乐观
///   更新回滚（非回合终态），不通知
/// - `approval_request` / `elicitation_request` → 需用户干预
/// - `stopped`（用户主动停止）与 `cancel_fallback`（停止超时兜底）不通知
pub fn notification_from_frame(
    ev: &serde_json::Value,
    session_id: &str,
    workspace_id: &str,
) -> Option<AgentNotification> {
    match ev.get("type").and_then(|t| t.as_str()) {
        Some("done") => Some(AgentNotification::turn_done(session_id, workspace_id)),
        Some("error") => {
            let message = ev.get("message").and_then(|m| m.as_str()).unwrap_or("");
            if message.starts_with("设置失败") {
                return None;
            }
            Some(AgentNotification::turn_error(
                session_id,
                workspace_id,
                message,
            ))
        }
        Some("approval_request") => {
            let tool = ev.get("tool").and_then(|v| v.as_str()).unwrap_or("");
            let summary = ev.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            Some(AgentNotification::approval_needed(
                session_id,
                workspace_id,
                tool,
                summary,
            ))
        }
        Some("elicitation_request") => {
            let message = ev.get("message").and_then(|v| v.as_str()).unwrap_or("");
            Some(AgentNotification::elicitation_needed(
                session_id,
                workspace_id,
                message,
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(json: serde_json::Value) -> serde_json::Value {
        json
    }

    #[test]
    fn maps_done() {
        let ev = frame(serde_json::json!({"type": "done"}));
        let n = notification_from_frame(&ev, "s1", "w1").expect("done maps");
        assert_eq!(n.event, "turn_done");
        assert_eq!(n.session_id, "s1");
        assert_eq!(n.workspace_id, "w1");
        assert!(n.message.is_none() && n.tool.is_none() && n.summary.is_none());
    }

    #[test]
    fn maps_turn_error_with_message() {
        let ev = frame(serde_json::json!({"type": "error", "message": "boom"}));
        let n = notification_from_frame(&ev, "s1", "w1").expect("error maps");
        assert_eq!(n.event, "turn_error");
        assert_eq!(n.message.as_deref(), Some("boom"));
    }

    #[test]
    fn skips_config_failure_error() {
        // "设置失败" 前缀 = config option 回滚，非回合终态，不通知
        let ev = frame(serde_json::json!({"type": "error", "message": "设置失败: bad value"}));
        assert!(notification_from_frame(&ev, "s1", "w1").is_none());
    }

    #[test]
    fn maps_approval_request() {
        let ev = frame(serde_json::json!({
            "type": "approval_request",
            "request_id": "r1",
            "tool": "shell",
            "summary": "run rm -rf /tmp",
            "args_preview": "{}"
        }));
        let n = notification_from_frame(&ev, "s1", "w1").expect("approval maps");
        assert_eq!(n.event, "approval_needed");
        assert_eq!(n.tool.as_deref(), Some("shell"));
        assert_eq!(n.summary.as_deref(), Some("run rm -rf /tmp"));
    }

    #[test]
    fn maps_elicitation_request() {
        let ev = frame(serde_json::json!({
            "type": "elicitation_request",
            "request_id": "r2",
            "message": "选择部署目标",
            "schema": {"type": "object"}
        }));
        let n = notification_from_frame(&ev, "s1", "w1").expect("elicitation maps");
        assert_eq!(n.event, "elicitation_needed");
        assert_eq!(n.message.as_deref(), Some("选择部署目标"));
    }

    #[test]
    fn skips_non_notify_frames() {
        // 流式帧 / 用户主动停止 / 停止兜底：一律不通知
        for ev in [
            serde_json::json!({"type": "assistant_chunk", "content": "hi"}),
            serde_json::json!({"type": "stopped"}),
            serde_json::json!({"type": "cancel_fallback"}),
            serde_json::json!({"type": "tool_call", "id": "t1"}),
            serde_json::json!({"type": "session_state"}),
        ] {
            assert!(
                notification_from_frame(&ev, "s1", "w1").is_none(),
                "frame {ev:?} must not notify"
            );
        }
    }
}
