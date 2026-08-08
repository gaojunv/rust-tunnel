//! Map ACP session updates to the existing WebSocket event JSON shapes
//! (assistant_chunk / tool_call / tool_result / done), so the frontend
//! needs no changes for the ACP path.
//!
//! 纯函数、无 I/O：单独文件便于单测（用 `serde_json::from_value` 构造
//! ACP crate 的 fixture，避免手写嵌套结构）。

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, ToolCallStatus};

/// 把一个 ACP update 映射为现有 WS 帧；无需推送的更新返回 None。
///
/// 帧形状对齐 `src/server/agent/runner.rs` 现有 WS 协议：
/// - `assistant_chunk` → `{"type", "content"}`
/// - `tool_call`       → `{"type", "id", "name", "status", "args"?}`
/// - `tool_call_update`→ `{"type": "tool_result", "id", "status", "result"?}`
pub fn map_update(update: &SessionUpdate) -> Option<serde_json::Value> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            let ContentBlock::Text(text) = &chunk.content else {
                // 非文本块（image/audio/resource 等）无正文可推
                return None;
            };
            if text.text.is_empty() {
                return None;
            }
            Some(serde_json::json!({"type": "assistant_chunk", "content": text.text}))
        }
        SessionUpdate::ToolCall(tc) => {
            let mut frame = serde_json::json!({
                "type": "tool_call",
                "id": tc.tool_call_id.to_string(),
                "name": tc.title,
                "status": status_str(Some(tc.status)),
            });
            if let Some(args) = &tc.raw_input {
                frame["args"] = args.clone();
            }
            Some(frame)
        }
        SessionUpdate::ToolCallUpdate(upd) => {
            let mut frame = serde_json::json!({
                "type": "tool_result",
                "id": upd.tool_call_id.to_string(),
                "status": status_str(upd.fields.status),
            });
            if let Some(title) = &upd.fields.title {
                frame["name"] = serde_json::Value::String(title.clone());
            }
            if let Some(output) = &upd.fields.raw_output {
                frame["result"] = output.clone();
            }
            Some(frame)
        }
        _ => None,
    }
}

/// ACP `ToolCallStatus` → WS 帧里的字符串状态。
/// Pending/InProgress 对前端都是"运行中"。
fn status_str(status: Option<ToolCallStatus>) -> &'static str {
    match status {
        Some(ToolCallStatus::Completed) => "completed",
        Some(ToolCallStatus::Failed) => "failed",
        _ => "running",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从 JSON fixture 反序列化一个 SessionUpdate（ACP crate 类型均可
    /// serde 反序列化，字段名以 crate 的 serde 注解为准）。
    fn update(v: serde_json::Value) -> SessionUpdate {
        serde_json::from_value(v).expect("fixture should deserialize")
    }

    #[test]
    fn test_map_agent_message_chunk() {
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"}
        }));
        let frame = map_update(&u).expect("text chunk should map");
        assert_eq!(frame["type"], "assistant_chunk");
        assert_eq!(frame["content"], "hello");
    }

    #[test]
    fn test_map_empty_text_chunk_returns_none() {
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": ""}
        }));
        assert!(map_update(&u).is_none(), "empty text should be dropped");
    }

    #[test]
    fn test_map_tool_call() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_1",
            "title": "shell",
            "status": "in_progress",
            "rawInput": {"cmd": "ls"}
        }));
        let frame = map_update(&u).expect("tool_call should map");
        assert_eq!(frame["type"], "tool_call");
        assert_eq!(frame["id"], "call_1");
        assert_eq!(frame["name"], "shell");
        assert_eq!(frame["status"], "running");
        assert_eq!(frame["args"]["cmd"], "ls");
    }

    #[test]
    fn test_map_tool_call_update_completed() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "status": "completed",
            "rawOutput": "a.rs"
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(frame["type"], "tool_result");
        assert_eq!(frame["id"], "call_1");
        assert_eq!(frame["status"], "completed");
        assert_eq!(frame["result"], "a.rs");
    }

    #[test]
    fn test_map_tool_call_update_failed() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_2",
            "status": "failed"
        }));
        let frame = map_update(&u).expect("failed update should map");
        assert_eq!(frame["status"], "failed");
    }

    #[test]
    fn test_map_irrelevant_update_returns_none() {
        let u = update(serde_json::json!({
            "sessionUpdate": "current_mode_update",
            "currentModeId": "default"
        }));
        assert!(map_update(&u).is_none());
    }

    #[test]
    fn test_status_str() {
        assert_eq!(status_str(Some(ToolCallStatus::Completed)), "completed");
        assert_eq!(status_str(Some(ToolCallStatus::Failed)), "failed");
        assert_eq!(status_str(Some(ToolCallStatus::Pending)), "running");
        assert_eq!(status_str(Some(ToolCallStatus::InProgress)), "running");
        assert_eq!(status_str(None), "running");
    }
}
