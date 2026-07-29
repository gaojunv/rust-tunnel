//! 兼容模式：把工具调用历史改写成纯文本，适配不支持多轮 tool calling 的上游。
//!
//! 背景：部分 OpenAI 兼容网关（如 opencode Console Go）只支持单轮工具调用，
//! 一旦请求历史里出现 `assistant.tool_calls` 或 `role="tool"` 消息就返回 400。
//! 本模块在发送前把这两类结构降级为文本：
//! - `assistant(tool_calls)` → `assistant` 文本：`调用工具 name(args)`
//! - `role="tool"` → `user` 文本：`工具结果(name): <content>`
//!
//! `tools` 声明保持不变（上游单轮 tools 正常），模型仍会以结构化 `tool_calls`
//! 返回下一轮调用，形成完整 agent 循环。

use serde_json::Value;

use super::ChatMessage;

/// 判断 provider 的 extra_config 是否开启工具历史兼容模式。
/// extra_config 是 JSON 字符串，含 `"compat_tool_history": true` 即开启。
pub fn compat_tool_history_enabled(extra_config: Option<&str>) -> bool {
    extra_config
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("compat_tool_history").cloned())
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 把消息列表里的工具调用结构改写为纯文本（原位替换）。
/// 返回是否有改写发生（用于诊断日志）。
pub fn rewrite_tool_history(messages: &mut [ChatMessage]) -> bool {
    let mut rewritten = false;

    // 第一遍：收集 tool_call_id → 函数名映射，用于给 tool 结果消息标注来源工具。
    let mut id_to_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in messages.iter() {
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                let id = c.get("id").and_then(Value::as_str).unwrap_or("");
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !id.is_empty() {
                    id_to_name.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    for m in messages.iter_mut() {
        // ── assistant.tool_calls → 文本 ──
        if let Some(calls) = m.tool_calls.take() {
            rewritten = true;
            let mut parts: Vec<String> = Vec::new();
            if let Some(text) = &m.content {
                if !text.is_empty() {
                    parts.push(text.clone());
                }
            }
            for c in &calls {
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let args = c
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                parts.push(format!("[调用工具 {name}] {args}"));
            }
            m.content = Some(parts.join("\n"));
        }

        // ── role="tool" → user 文本 ──
        if m.role == "tool" {
            rewritten = true;
            let tool_name = m
                .tool_call_id
                .as_deref()
                .and_then(|id| id_to_name.get(id))
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            let content = m.content.take().unwrap_or_default();
            m.role = "user".to_string();
            m.tool_call_id = None;
            m.content = Some(format!("[工具结果 {tool_name}]\n{content}"));
        }
    }

    rewritten
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disabled_when_no_extra_config() {
        assert!(!compat_tool_history_enabled(None));
        assert!(!compat_tool_history_enabled(Some("")));
        assert!(!compat_tool_history_enabled(Some("not json")));
        assert!(!compat_tool_history_enabled(Some("{}")));
        assert!(!compat_tool_history_enabled(
            Some(r#"{"compat_tool_history": false}"#)
        ));
    }

    #[test]
    fn enabled_when_flag_true() {
        assert!(compat_tool_history_enabled(Some(
            r#"{"compat_tool_history": true}"#
        )));
    }

    #[test]
    fn assistant_tool_calls_become_text() {
        let mut msgs = vec![ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}
            })]),
            tool_call_id: None,
            name: None,
        }];
        assert!(rewrite_tool_history(&mut msgs));
        assert_eq!(msgs[0].role, "assistant");
        assert!(msgs[0].tool_calls.is_none());
        let c = msgs[0].content.as_deref().unwrap();
        assert!(c.contains("Bash"), "应含工具名: {c}");
        assert!(c.contains("ls"), "应含参数: {c}");
    }

    #[test]
    fn tool_role_becomes_user_with_tool_name() {
        let mut msgs = vec![
            ChatMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{}"}
                })]),
                tool_call_id: None,
                name: None,
            },
            ChatMessage {
                role: "tool".into(),
                content: Some("晴 25度".into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                name: None,
            },
        ];
        assert!(rewrite_tool_history(&mut msgs));
        assert_eq!(msgs[1].role, "user");
        assert!(msgs[1].tool_call_id.is_none());
        let c = msgs[1].content.as_deref().unwrap();
        assert!(c.contains("get_weather"), "应标注来源工具: {c}");
        assert!(c.contains("晴 25度"), "应保留结果内容: {c}");
    }

    #[test]
    fn plain_messages_untouched() {
        let mut msgs = vec![
            ChatMessage::text("system", "sys"),
            ChatMessage::text("user", "hi"),
            ChatMessage::text("assistant", "hello"),
        ];
        assert!(!rewrite_tool_history(&mut msgs));
        assert_eq!(msgs[0].content.as_deref(), Some("sys"));
        assert_eq!(msgs[2].content.as_deref(), Some("hello"));
    }

    #[test]
    fn assistant_text_and_tool_calls_merge() {
        // assistant 同时有正文和 tool_calls 时，两者都保留
        let mut msgs = vec![ChatMessage {
            role: "assistant".into(),
            content: Some("我先查一下".into()),
            tool_calls: Some(vec![json!({
                "id": "c1",
                "type": "function",
                "function": {"name": "Read", "arguments": "{\"path\":\"/a\"}"}
            })]),
            tool_call_id: None,
            name: None,
        }];
        rewrite_tool_history(&mut msgs);
        let c = msgs[0].content.as_deref().unwrap();
        assert!(c.contains("我先查一下"));
        assert!(c.contains("Read"));
    }

    #[test]
    fn rewritten_output_has_no_tool_structure() {
        // 序列化后不得再出现 tool_calls / tool_call_id / role:"tool"
        let mut msgs = vec![
            ChatMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![json!({
                    "id": "c1", "type": "function",
                    "function": {"name": "Bash", "arguments": "{}"}
                })]),
                tool_call_id: None,
                name: None,
            },
            ChatMessage {
                role: "tool".into(),
                content: Some("out".into()),
                tool_calls: None,
                tool_call_id: Some("c1".into()),
                name: None,
            },
        ];
        rewrite_tool_history(&mut msgs);
        let s = serde_json::to_string(&msgs).unwrap();
        assert!(!s.contains("tool_calls"), "{s}");
        assert!(!s.contains("tool_call_id"), "{s}");
        assert!(!s.contains("\"tool\""), "{s}");
    }
}
