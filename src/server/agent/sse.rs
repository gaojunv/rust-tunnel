//! SSE 增量解析：OpenAI 流式 chat.completion 的 delta 聚合。
use super::runner::ParsedToolCall;

/// 一个回合的聚合结果。
pub struct AggregatedTurn {
    pub text: String,
    pub tool_calls: Vec<ParsedToolCall>,
    /// 重建的 OpenAI tool_calls JSON（rt.messages 回填与落库用）。
    pub raw_tool_calls: Vec<serde_json::Value>,
}

pub enum SseFeed {
    /// content 增量
    Content(String),
    /// 该行无产出（role delta、空行、注释、畸形行跳过）
    None,
    /// [DONE]
    Done,
}

#[derive(Default)]
pub struct SseAggregator {
    text: String,
    // (id, name, arguments) 按 index 分桶
    calls: Vec<(String, String, String)>,
}

impl SseAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一行（不含 \n）。`data: ` 前缀可选空格；非 data 行/畸形 JSON 跳过。
    pub fn feed_line(&mut self, line: &str) -> SseFeed {
        let line = line.trim_end_matches('\r');
        let Some(data) = line.strip_prefix("data:") else {
            return SseFeed::None;
        };
        let data = data.trim();
        if data == "[DONE]" {
            return SseFeed::Done;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
            return SseFeed::None; // 畸形行跳过，不中断流
        };
        let Some(delta) = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"))
        else {
            return SseFeed::None;
        };

        // tool_calls 增量：按 index 分桶，id/name/arguments 字符串拼接
        if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in calls {
                let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                if self.calls.len() <= index {
                    self.calls
                        .resize(index + 1, (String::new(), String::new(), String::new()));
                }
                let slot = &mut self.calls[index];
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    slot.0.push_str(id);
                }
                if let Some(f) = tc.get("function") {
                    if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                        slot.1.push_str(name);
                    }
                    if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                        slot.2.push_str(args);
                    }
                }
            }
        }

        match delta.get("content").and_then(|c| c.as_str()) {
            Some(s) if !s.is_empty() => {
                self.text.push_str(s);
                SseFeed::Content(s.to_string())
            }
            _ => SseFeed::None,
        }
    }

    /// 流结束：聚合 tool_calls 为 ParsedToolCall + 重建原始 JSON。
    pub fn finish(self) -> Result<AggregatedTurn, String> {
        let mut tool_calls = Vec::new();
        let mut raw_tool_calls = Vec::new();
        for (id, name, args) in self.calls {
            if id.is_empty() && name.is_empty() {
                continue;
            }
            tool_calls.push(ParsedToolCall {
                id: id.clone(),
                name: name.clone(),
                args: args.clone(),
            });
            raw_tool_calls.push(serde_json::json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args},
            }));
        }
        Ok(AggregatedTurn {
            text: self.text,
            tool_calls,
            raw_tool_calls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(agg: &mut SseAggregator, chunks: &[&str]) -> (String, bool) {
        let mut out = String::new();
        let mut done = false;
        for c in chunks {
            for line in c.split('\n') {
                match agg.feed_line(line) {
                    SseFeed::Content(s) => out.push_str(&s),
                    SseFeed::Done => done = true,
                    SseFeed::None => {}
                }
            }
        }
        (out, done)
    }

    #[test]
    fn test_content_deltas_streamed() {
        let mut agg = SseAggregator::new();
        let (text, done) = feed_all(
            &mut agg,
            &[
                r#"data: {"choices":[{"delta":{"role":"assistant"},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"content":"你好"},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"content":"，世界"},"index":0}]}"#,
                "data: [DONE]",
            ],
        );
        assert!(done);
        assert_eq!(text, "你好，世界");
        let turn = agg.finish().unwrap();
        assert_eq!(turn.text, "你好，世界");
        assert!(turn.tool_calls.is_empty());
    }

    #[test]
    fn test_tool_calls_aggregated_by_index() {
        let mut agg = SseAggregator::new();
        feed_all(
            &mut agg,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"cmd\":"}}]},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}]},"index":0}]}"#,
                "data: [DONE]",
            ],
        );
        let turn = agg.finish().unwrap();
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].name, "shell");
        assert_eq!(turn.tool_calls[0].args, r#"{"cmd":"ls"}"#);
        assert_eq!(turn.tool_calls[1].name, "read_file");
        // raw_tool_calls 可用于直接回填 OpenAI 消息
        assert_eq!(turn.raw_tool_calls[0]["id"], "call_1");
        assert_eq!(turn.raw_tool_calls[1]["function"]["name"], "read_file");
    }

    #[test]
    fn test_malformed_lines_skipped() {
        let mut agg = SseAggregator::new();
        let (text, done) = feed_all(
            &mut agg,
            &[
                "",
                ": comment",
                "data: {not json}",
                r#"data: {"choices":[{"delta":{"content":"ok"},"index":0}]}"#,
                "data: [DONE]",
            ],
        );
        assert!(done);
        assert_eq!(text, "ok");
    }

    #[test]
    fn test_finish_without_done_still_aggregates() {
        // 上游中断（无 [DONE]）：已聚合内容保留，由调用方决定是否使用
        let mut agg = SseAggregator::new();
        feed_all(
            &mut agg,
            &[r#"data: {"choices":[{"delta":{"content":"半句"},"index":0}]}"#],
        );
        let turn = agg.finish().unwrap();
        assert_eq!(turn.text, "半句");
    }
}
