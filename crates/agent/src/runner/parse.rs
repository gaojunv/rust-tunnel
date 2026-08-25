//! LLM 响应解析：SSE 行切分（[`LineBuf]`）、非流式 body → [`LlmTurn`]。

use crate::sse;

/// 按行切分 SSE 字节流：HTTP chunk 边界可切断一行，未完结部分留缓冲。
pub(crate) struct LineBuf {
    pending: Vec<u8>,
    /// pending 超过上限（无换行的超长单行）→ true；runner 应终止流。
    pub(crate) overflowed: bool,
    limit: usize,
}

impl Default for LineBuf {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            overflowed: false,
            limit: sse::MAX_STREAM_BYTES,
        }
    }
}

impl LineBuf {
    /// 用自定义上限构造（测试用小 limit，避免测试分配 10MB）。
    #[cfg(test)]
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > self.limit {
            self.overflowed = true;
        }
        let mut lines = Vec::new();
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.pending[..pos]).into_owned();
            self.pending.drain(..=pos);
            if !line.trim().is_empty() {
                lines.push(line);
            }
        }
        lines
    }

    pub(crate) fn flush(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let line = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        if line.trim().is_empty() {
            None
        } else {
            Some(line)
        }
    }

    /// 非 SSE 嗅探：缓冲的首批字节是否已能判定为非 SSE 流。
    ///
    /// 判定条件：pending 长度足够（≥5，即 `data:` 长度），trim 后既不是
    /// `data:` 前缀、也不是 SSE 允许的开头（空行/注释行 `:` 前缀）、也不是
    /// 纯空白（需继续等待更多字节）。
    pub(crate) fn has_non_sse_prefix(&self) -> bool {
        if self.pending.len() < 5 {
            return false;
        }
        let trimmed = std::str::from_utf8(&self.pending)
            .unwrap_or_default()
            .trim_start();
        !trimmed.is_empty() && !trimmed.starts_with("data:") && !trimmed.starts_with(':')
    }

    pub(crate) fn pending(&self) -> &[u8] {
        &self.pending
    }

    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

/// 判断一行是否为合法的 SSE 首行（data 行或注释行；空行已被 LineBuf 过滤）。
/// 首个非空行既不是 `data:` 也不是 `:` 注释 → 上游实际返回的是普通文本/JSON，
/// 走非 SSE 回退。
pub(crate) fn is_sse_line(line: &str) -> bool {
    let line = line.trim_end_matches('\r');
    line.starts_with("data:") || line.starts_with(':')
}

pub(crate) fn is_sse_response(content_type: &str) -> bool {
    content_type.starts_with("text/event-stream")
}

/// One LLM response, parsed.
pub enum LlmTurn {
    /// Assistant produced final text (no tool calls)
    Text(String),
    /// Assistant wants to call tools
    ToolCalls(Vec<ParsedToolCall>),
}

/// 解析后的工具调用。
pub struct ParsedToolCall {
    /// 工具调用标识。
    pub id: String,
    /// 工具名。
    pub name: String,
    /// 参数 JSON。
    pub args: String,
}

impl std::fmt::Debug for LlmTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(t) => write!(f, "Text({t:?})"),
            Self::ToolCalls(c) => write!(f, "ToolCalls({} calls)", c.len()),
        }
    }
}

/// Parse a non-streaming OpenAI chat.completion body into an LlmTurn.
pub fn parse_llm_turn(body: &serde_json::Value) -> Result<LlmTurn, String> {
    let msg = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .ok_or_else(|| "response missing choices[0].message".to_string())?;

    if let Some(calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        if !calls.is_empty() {
            let parsed: Result<Vec<_>, String> = calls
                .iter()
                .map(|c| {
                    Ok(ParsedToolCall {
                        id: c
                            .get("id")
                            .and_then(|v| v.as_str())
                            .ok_or("tool_call missing id")?
                            .to_string(),
                        name: c
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .ok_or("tool_call missing function.name")?
                            .to_string(),
                        args: c
                            .pointer("/function/arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_string(),
                    })
                })
                .collect();
            return Ok(LlmTurn::ToolCalls(parsed?));
        }
    }

    let content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(LlmTurn::Text(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_text_response() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "我来帮你看看"},
                "finish_reason": "stop"
            }]
        });
        let turn = parse_llm_turn(&body).unwrap();
        match turn {
            LlmTurn::Text(t) => assert_eq!(t, "我来帮你看看"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let turn = parse_llm_turn(&body).unwrap();
        match turn {
            LlmTurn::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "shell");
                assert_eq!(calls[0].args, r#"{"cmd":"ls"}"#);
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_malformed() {
        assert!(parse_llm_turn(&serde_json::json!({})).is_err());
        assert!(parse_llm_turn(&serde_json::json!({"choices": []})).is_err());
    }

    #[test]
    fn test_is_sse_response() {
        assert!(is_sse_response("text/event-stream; charset=utf-8"));
        assert!(is_sse_response("text/event-stream"));
        assert!(!is_sse_response("application/json"));
    }

    #[test]
    fn test_line_buf_detects_non_sse_first_bytes() {
        // 首批字节即不是 data:（JSON 开头，尚无换行）→ 判定非 SSE
        let mut buf = LineBuf::default();
        assert!(!buf.has_non_sse_prefix()); // 空缓冲不足判定
        buf.feed(b"{\"c");
        assert!(!buf.has_non_sse_prefix()); // 3 字节 < 5 → 继续等待
        buf.feed(b"hoi");
        assert!(buf.has_non_sse_prefix()); // 累计 ≥5，trim 后以 `{` 开头 → 非 SSE
        assert_eq!(buf.pending(), b"{\"choi");
    }

    #[test]
    fn test_line_buf_non_sse_prefix_allows_sse_leading() {
        // SSE 流开头允许空行/注释行/`data:` 前缀（可跨 chunk 拼合）：不应误判
        let mut buf = LineBuf::default();
        buf.feed(b"\n\n: comment\r\n");
        assert!(!buf.has_non_sse_prefix()); // trim 后以 `:` 开头 → SSE 注释

        let mut buf = LineBuf::default();
        buf.feed(b"\n\n  \n");
        assert!(!buf.has_non_sse_prefix()); // 纯空白 → 继续等待

        let mut buf = LineBuf::default();
        buf.feed(b"da");
        assert!(!buf.has_non_sse_prefix());
        buf.feed(b"ta: ");
        assert!(!buf.has_non_sse_prefix()); // data: 前缀跨 chunk 拼合 → 仍是 SSE
    }

    #[test]
    fn test_first_line_sniff_is_sse_line() {
        assert!(is_sse_line(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}"
        ));
        assert!(is_sse_line(": comment"));
        assert!(is_sse_line("data: [DONE]"));
        assert!(!is_sse_line("{\"choices\":["));
        assert!(!is_sse_line("plain text"));
    }

    #[test]
    fn test_line_buf_overflow_on_single_line() {
        // 无换行的超长单行：pending 超过上限 → overflowed 标记（runner 据此终止）
        let mut buf = LineBuf::with_limit(100);
        assert!(!buf.overflowed);
        buf.feed(&[b'x'; 150]);
        assert!(buf.overflowed);
        // 溢出后接口不 panic、不丢已解析行
        assert!(buf.feed(b"y").is_empty());
    }

    #[test]
    fn test_line_splitter_handles_partial_chunks() {
        // HTTP chunk 边界可能切断 SSE 行：缓冲拼行
        let mut buf = LineBuf::default();
        assert!(!buf.feed(b"data: {\"a\":1}\r\n\r\nda").is_empty());
        // 第一行完整产出，"da" 留在缓冲
        let lines = buf.feed(b"ta: [DONE]\n");
        assert!(lines.iter().any(|l| l.contains("[DONE]")));
    }

    #[test]
    fn test_flush_feeds_final_delta_to_aggregator() {
        // 回归：flush() 丢弃返回值会丢最后一行的 content delta 推送。
        // 组合验证 flush 取行 → feed_line 返回 Content（修复路径的行为，不测 ws 发送）。
        let mut buf = LineBuf::default();
        // 无换行的完整 data 行：feed 不会产出，留在缓冲
        let line = r#"data: {"choices":[{"delta":{"content":"收尾"},"index":0}]}"#;
        assert!(buf.feed(line.as_bytes()).is_empty());
        let flushed = buf.flush().expect("flush should return buffered line");
        assert_eq!(flushed, line);

        let mut agg = sse::SseAggregator::new();
        match agg.feed_line(&flushed) {
            sse::SseFeed::Content(delta) => assert_eq!(delta, "收尾"),
            sse::SseFeed::None => panic!("expected Content delta, got None"),
            sse::SseFeed::Done => panic!("expected Content delta, got Done"),
            sse::SseFeed::Overflow => panic!("expected Content delta, got Overflow"),
            sse::SseFeed::Thought { .. } => panic!("expected Content delta, got Thought"),
            sse::SseFeed::ToolCallDelta { .. } => {
                panic!("expected Content delta, got ToolCallDelta")
            }
        }
    }

    // SSE 流传输层失败重试的回归覆盖见 `tests/agent_basic.rs`：
    // `agent_stream_retry_succeeds_with_full_text`（重试成功：stream_reset + status
    // 帧 + 最终文本完整）与 `agent_stream_retry_exhausted_sends_error`（耗尽走
    // 错误路径）。本文件的旧 `test_retry_on_stream_read_failure` 只测本地闭包，
    // 零回归价值，已删除。
}
