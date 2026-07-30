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

/// 从模型输出的纯文本中解析伪工具调用，转换为 OpenAI `tool_calls` 数组。
///
/// 这是 `rewrite_tool_history` 的逆操作：当 compat 模式开启时，上游模型
/// （如 deepseek-v4-flash）可能模仿 `[调用工具 name] {args}` 格式输出纯文本，
/// 本函数把这种文本还原为结构化 tool_calls，让客户端能正常执行工具。
///
/// 支持的格式：
/// - `[调用工具 name] {json_args}` — 单行
/// - `[调用工具 name]\n{json_args}` — 参数换行
/// - 多个工具调用连续出现
/// - 普通文本与工具调用混合（普通文本保留为 content）
///
/// 返回 `(remaining_text, tool_calls)`：remaining_text 是去除工具调用后的纯文本，
/// tool_calls 是解析出的 OpenAI 格式 tool_calls 数组。
pub fn parse_pseudo_tool_calls(text: &str) -> (String, Option<Vec<serde_json::Value>>) {
    let mut remaining_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
    let mut call_index = 0u32;

    let mut chars = text.char_indices().peekable();
    let mut current_text = String::new();

    while let Some((_, ch)) = chars.next() {
        if ch == '[' {
            // 尝试匹配 "[调用工具 "
            let rest: String = chars.clone().take(5).map(|(_, c)| c).collect();
            if rest == "调用工具 " {
                // 消耗掉 "调用工具 "
                for _ in 0..5 {
                    chars.next();
                }

                // 解析工具名：直到 ']' 或换行
                let mut name = String::new();
                let mut found_bracket = false;
                for (_, c) in chars.by_ref() {
                    if c == ']' {
                        found_bracket = true;
                        break;
                    }
                    if c == '\n' {
                        break;
                    }
                    name.push(c);
                }

                if found_bracket && !name.is_empty() {
                    // 跳过 ] 后的空白
                    let mut args_str = String::new();
                    let mut brace_depth = 0i32;
                    let mut in_string = false;
                    let mut escape_next = false;
                    let mut args_started = false;

                    // 先跳过 ] 后的空格和换行
                    while let Some(&(_, c)) = chars.peek() {
                        if c == ' ' || c == '\n' || c == '\r' || c == '\t' {
                            chars.next();
                        } else {
                            break;
                        }
                    }

                    // 解析 JSON 参数：找到 { ... } 的完整匹配
                    for (_, c) in chars.by_ref() {
                        if escape_next {
                            args_str.push(c);
                            escape_next = false;
                            continue;
                        }
                        if c == '\\' && in_string {
                            escape_next = true;
                            args_str.push(c);
                            continue;
                        }
                        if c == '"' && !escape_next {
                            in_string = !in_string;
                        }
                        if !in_string {
                            if c == '{' {
                                brace_depth += 1;
                                args_started = true;
                            } else if c == '}' {
                                brace_depth -= 1;
                            }
                        }
                        args_str.push(c);
                        if args_started && brace_depth == 0 {
                            break;
                        }
                    }

                    // 尝试解析 JSON
                    if args_started && brace_depth == 0 {
                        // 验证是合法 JSON
                        if serde_json::from_str::<serde_json::Value>(&args_str).is_ok() {
                            call_index += 1;
                            tool_calls.push(serde_json::json!({
                                "id": format!("call_{}", call_index),
                                "type": "function",
                                "function": {
                                    "name": name.trim(),
                                    "arguments": args_str,
                                },
                            }));
                            continue;
                        }
                    }

                    // 解析失败，回退为普通文本
                    current_text.push_str(&format!("[调用工具 {}] {}", name, args_str));
                    continue;
                }

                // 不是有效的工具调用格式，回退
                current_text.push('[');
                current_text.push_str(&rest);
                if !name.is_empty() {
                    current_text.push_str(&name);
                }
                if found_bracket {
                    current_text.push(']');
                }
                continue;
            }

            // 不是 "[调用工具 "，只是普通 '['
            current_text.push(ch);
            // 把已读的 rest 也放回去（通过 push 到 current_text）
            current_text.push_str(&rest);
            // 消耗掉 rest 对应的字符
            for _ in 0..rest.len() {
                chars.next();
            }
            continue;
        }

        current_text.push(ch);
    }

    // 清理：如果 current_text 非空则加入 remaining
    let trimmed = current_text.trim();
    if !trimmed.is_empty() {
        remaining_parts.push(trimmed.to_string());
    }

    let remaining = remaining_parts.join("\n");
    let calls = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    (remaining, calls)
}

/// 流式 SSE 伪工具调用解析器。
///
/// 逐 chunk 累积 `delta.content`，在流结束时解析伪工具调用文本，
/// 返回解析结果供调用方注入结构化 `tool_calls`。
///
/// 用法：对每个 SSE chunk 调用 [`push_chunk`]，流结束时调用 [`finish`]。
pub struct StreamPseudoToolCallParser {
    /// 累积的 content 文本（从所有 chunk 的 delta.content 拼接）
    accumulated: String,
}

/// 流式解析结果。
pub struct StreamParseResult {
    /// 是否检测到伪工具调用
    pub has_tool_calls: bool,
    /// 解析出的 OpenAI tool_calls 数组（如果有）
    pub tool_calls: Option<Vec<serde_json::Value>>,
    /// 去除工具调用后的剩余文本
    pub remaining_text: String,
}

impl StreamPseudoToolCallParser {
    pub fn new() -> Self {
        Self {
            accumulated: String::new(),
        }
    }

    /// 喂入一个 SSE chunk 行（`data: {...}` 格式）。
    ///
    /// 只提取 `choices[0].delta.content` 字段累积，其他字段忽略。
    /// 非 data 行、[DONE]、无效 JSON 都安全跳过。
    pub fn push_chunk(&mut self, line: &str) {
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };
        if let Some(content) = chunk["choices"][0]["delta"]["content"].as_str() {
            self.accumulated.push_str(content);
        }
    }

    /// 结束解析，返回结果。
    pub fn finish(self) -> StreamParseResult {
        let (remaining, tool_calls) = parse_pseudo_tool_calls(&self.accumulated);
        StreamParseResult {
            has_tool_calls: tool_calls.is_some(),
            tool_calls,
            remaining_text: remaining,
        }
    }
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

    // ── parse_pseudo_tool_calls 测试 ─────────────────────────────

    #[test]
    fn parse_single_tool_call() {
        let text = "[调用工具 Bash] {\"command\":\"ls\"}";
        let (remaining, calls) = parse_pseudo_tool_calls(text);
        assert!(remaining.is_empty(), "剩余文本应为空: {remaining}");
        let calls = calls.expect("应解析出 tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        let args: serde_json::Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn parse_tool_call_with_text_before() {
        let text = "我来执行命令\n[调用工具 Bash] {\"command\":\"ls\"}";
        let (remaining, calls) = parse_pseudo_tool_calls(text);
        assert_eq!(remaining, "我来执行命令");
        let calls = calls.expect("应解析出 tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
    }

    #[test]
    fn parse_tool_call_with_text_after() {
        let text = "[调用工具 Bash] {\"command\":\"ls\"}\n执行完毕";
        let (remaining, calls) = parse_pseudo_tool_calls(text);
        assert_eq!(remaining, "执行完毕");
        assert!(calls.is_some());
    }

    #[test]
    fn parse_multiple_tool_calls() {
        let text = "[调用工具 Bash] {\"command\":\"ls\"}\n[调用工具 Read] {\"path\":\"/tmp/a\"}";
        let (remaining, calls) = parse_pseudo_tool_calls(text);
        assert!(remaining.is_empty());
        let calls = calls.expect("应解析出 tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        assert_eq!(calls[1]["function"]["name"], "Read");
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[1]["id"], "call_2");
    }

    #[test]
    fn parse_tool_call_multiline_args() {
        let text = "[调用工具 Bash]\n{\"command\":\"ls -la /tmp\"}";
        let (remaining, calls) = parse_pseudo_tool_calls(text);
        assert!(remaining.is_empty());
        let calls = calls.expect("应解析出 tool_calls");
        assert_eq!(calls[0]["function"]["name"], "Bash");
    }

    #[test]
    fn parse_no_tool_calls_plain_text() {
        let text = "这只是普通文本，没有工具调用";
        let (remaining, calls) = parse_pseudo_tool_calls(text);
        assert_eq!(remaining, text);
        assert!(calls.is_none());
    }

    #[test]
    fn parse_tool_call_with_nested_json() {
        let text = "[调用工具 Bash] {\"command\":\"echo '{\\\"nested\\\":true}'\"}";
        let (_remaining, calls) = parse_pseudo_tool_calls(text);
        let calls = calls.expect("应解析出 tool_calls");
        let args: serde_json::Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "echo '{\"nested\":true}'");
    }

    #[test]
    fn parse_tool_call_with_empty_args() {
        let text = "[调用工具 Bash] {}";
        let (_remaining, calls) = parse_pseudo_tool_calls(text);
        let calls = calls.expect("应解析出 tool_calls");
        let args: serde_json::Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({}));
    }

    #[test]
    fn parse_invalid_json_falls_back_to_text() {
        let text = "[调用工具 Bash] {invalid json}";
        let (remaining, calls) = parse_pseudo_tool_calls(text);
        assert!(remaining.contains("[调用工具 Bash]"), "无效 JSON 应回退为文本");
        assert!(calls.is_none());
    }

    #[test]
    fn parse_mixed_content() {
        let text = "先查看文件\n[调用工具 Read] {\"path\":\"/a.txt\"}\n然后执行\n[调用工具 Bash] {\"command\":\"cat /a.txt\"}\n完成";
        let (remaining, calls) = parse_pseudo_tool_calls(text);
        assert!(remaining.contains("先查看文件"));
        assert!(remaining.contains("然后执行"));
        assert!(remaining.contains("完成"));
        let calls = calls.expect("应解析出 tool_calls");
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn parse_tool_call_generates_unique_ids() {
        let text = "[调用工具 A] {}\n[调用工具 B] {}\n[调用工具 C] {}";
        let (_, calls) = parse_pseudo_tool_calls(text);
        let calls = calls.unwrap();
        let ids: Vec<&str> = calls.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["call_1", "call_2", "call_3"]);
    }

    #[test]
    fn parse_roundtrip_with_rewrite() {
        // rewrite_tool_history → parse_pseudo_tool_calls 应还原原始 tool_calls
        let mut msgs = vec![ChatMessage {
            role: "assistant".into(),
            content: Some("执行命令".into()),
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}
            })]),
            tool_call_id: None,
            name: None,
        }];
        rewrite_tool_history(&mut msgs);
        let text = msgs[0].content.as_deref().unwrap();
        let (remaining, calls) = parse_pseudo_tool_calls(text);
        assert!(remaining.contains("执行命令"));
        let calls = calls.expect("应还原 tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
    }

    // ── 流式伪工具调用解析测试 ──────────────────────────────────

    #[test]
    fn stream_parse_single_tool_call() {
        let mut parser = StreamPseudoToolCallParser::new();
        // 模拟 SSE chunk 流：content 分多个 chunk 到达
        let chunks = vec![
            r#"data: {"choices":[{"delta":{"content":"[调用工"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"具 Bash] {\"command\":\"ls\"}"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ];
        for chunk in &chunks {
            parser.push_chunk(chunk);
        }
        let result = parser.finish();
        assert!(result.has_tool_calls, "应检测到伪工具调用");
        let calls = result.tool_calls.expect("应有 tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
    }

    #[test]
    fn stream_parse_no_tool_calls() {
        let mut parser = StreamPseudoToolCallParser::new();
        let chunks = vec![
            r#"data: {"choices":[{"delta":{"content":"普通文本"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ];
        for chunk in &chunks {
            parser.push_chunk(chunk);
        }
        let result = parser.finish();
        assert!(!result.has_tool_calls);
        assert!(result.tool_calls.is_none());
    }

    #[test]
    fn stream_parse_mixed_content() {
        let mut parser = StreamPseudoToolCallParser::new();
        let chunks = vec![
            r#"data: {"choices":[{"delta":{"content":"我来执行\n[调用工具 Bash] {\"command\":\"ls\"}"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ];
        for chunk in &chunks {
            parser.push_chunk(chunk);
        }
        let result = parser.finish();
        assert!(result.has_tool_calls);
        assert!(result.remaining_text.contains("我来执行"));
    }

    #[test]
    fn stream_parse_multiple_tool_calls() {
        let mut parser = StreamPseudoToolCallParser::new();
        let chunks = vec![
            r#"data: {"choices":[{"delta":{"content":"[调用工具 A] {}"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"\n[调用工具 B] {}"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ];
        for chunk in &chunks {
            parser.push_chunk(chunk);
        }
        let result = parser.finish();
        let calls = result.tool_calls.expect("应有 tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["name"], "A");
        assert_eq!(calls[1]["function"]["name"], "B");
    }
}
