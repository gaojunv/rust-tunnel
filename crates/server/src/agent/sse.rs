//! SSE 增量解析：OpenAI 流式 chat.completion 的 delta 聚合。
use super::runner::ParsedToolCall;

/// 单回合 tool_calls 上限：实践中 LLM 并行调用不超过数十；
/// 无界 index 直接 resize 会被恶意上游用作 OOM/panic 向量。
const MAX_TOOL_CALLS: usize = 64;

/// 流式聚合总字节上限：与非流式路径的 10MB body 上限对齐。
/// content 累加、tool arguments 拼接、LineBuf.pending 共用此上限，
/// 防止恶意/畸形上游推送无界字节导致 OOM。
pub const MAX_STREAM_BYTES: usize = 10 * 1024 * 1024;

/// 一个回合的聚合结果。
pub struct AggregatedTurn {
    pub text: String,
    /// reasoning_content 增量聚合（DeepSeek thinking 模式）；无思考输出为空串。
    pub reasoning: String,
    pub tool_calls: Vec<ParsedToolCall>,
    /// 重建的 OpenAI tool_calls JSON（rt.messages 回填与落库用）。
    pub raw_tool_calls: Vec<serde_json::Value>,
}

#[derive(Debug)]
pub enum SseFeed {
    /// content 增量
    Content(String),
    /// reasoning_content 增量；`content` 为同行 delta 携带的正文增量
    /// （同帧同时含 reasoning + content 时两个 WS 帧都要发，正文不丢）。
    Thought {
        reasoning: String,
        content: Option<String>,
    },
    /// 该行无产出（role delta、空行、注释、畸形行跳过）
    None,
    /// [DONE]
    Done,
    /// 聚合字节超限（MAX_STREAM_BYTES）：调用方应终止流并报错，
    /// 不落库半截消息。
    Overflow,
}

pub struct SseAggregator {
    text: String,
    /// reasoning_content 聚合桶（DeepSeek thinking 模式）。
    reasoning: String,
    // (id, name, arguments) 按 index 分桶
    calls: Vec<(String, String, String)>,
    /// 已累计的聚合字节（text + reasoning + tool_calls 各字段拼接），超 limit 即 Overflow。
    bytes: usize,
    /// 是否收到过 data: 行（空流兜底判定用）。
    saw_data: bool,
    limit: usize,
}

impl Default for SseAggregator {
    fn default() -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            calls: Vec::new(),
            bytes: 0,
            saw_data: false,
            limit: MAX_STREAM_BYTES,
        }
    }
}

impl SseAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用自定义上限构造聚合器（测试用小 limit，避免测试分配 10MB）。
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    /// 预占 `additional` 字节：超限返回 false（调用方应返回 Overflow）。
    fn reserve(&mut self, additional: usize) -> bool {
        if self.bytes.saturating_add(additional) > self.limit {
            return false;
        }
        self.bytes += additional;
        true
    }

    /// 是否收到过 `data:` 前缀行（含畸形行与 [DONE]）。
    pub fn saw_data(&self) -> bool {
        self.saw_data
    }

    /// 喂入一行（不含 \n）。`data: ` 前缀可选空格；非 data 行/畸形 JSON 跳过。
    pub fn feed_line(&mut self, line: &str) -> SseFeed {
        let line = line.trim_end_matches('\r');
        let Some(data) = line.strip_prefix("data:") else {
            return SseFeed::None;
        };
        self.saw_data = true;
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
                if index >= MAX_TOOL_CALLS {
                    continue; // 恶意/畸形 index：跳过该增量，不中断流
                }
                if self.calls.len() <= index {
                    self.calls
                        .resize(index + 1, (String::new(), String::new(), String::new()));
                }
                let slot = &mut self.calls[index];
                // 字段级借用（slot 持有 self.calls，bytes/limit 是独立字段），
                // 故内联检查而非调用 &mut self 方法，避免重复借用。
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    if self.bytes.saturating_add(id.len()) > self.limit {
                        return SseFeed::Overflow;
                    }
                    self.bytes += id.len();
                    slot.0.push_str(id);
                }
                if let Some(f) = tc.get("function") {
                    if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                        if self.bytes.saturating_add(name.len()) > self.limit {
                            return SseFeed::Overflow;
                        }
                        self.bytes += name.len();
                        slot.1.push_str(name);
                    }
                    if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                        if self.bytes.saturating_add(args.len()) > self.limit {
                            return SseFeed::Overflow;
                        }
                        self.bytes += args.len();
                        slot.2.push_str(args);
                    }
                }
            }
        }

        // content 增量提取（非空时暂存，供 Thought 同帧携带）
        let content_delta = delta.get("content").and_then(|c| c.as_str()).filter(|s| !s.is_empty());
        if let Some(s) = content_delta {
            if !self.reserve(s.len()) {
                return SseFeed::Overflow;
            }
            self.text.push_str(s);
        }

        // reasoning_content 增量提取（DeepSeek thinking 模式）
        match delta.get("reasoning_content").and_then(|r| r.as_str()) {
            Some(r) if !r.is_empty() => {
                if !self.reserve(r.len()) {
                    return SseFeed::Overflow;
                }
                self.reasoning.push_str(r);
                SseFeed::Thought {
                    reasoning: r.to_string(),
                    content: content_delta.map(|s| s.to_string()),
                }
            }
            _ => match content_delta {
                Some(s) => SseFeed::Content(s.to_string()),
                _ => SseFeed::None,
            },
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
            reasoning: self.reasoning,
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
                    SseFeed::Thought { reasoning, content } => {
                        out.push_str(&reasoning);
                        if let Some(c) = content {
                            out.push_str(&c);
                        }
                    }
                    SseFeed::Done => done = true,
                    SseFeed::None => {}
                    SseFeed::Overflow => return (out, done),
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

    #[test]
    fn test_huge_index_skipped_not_panic() {
        let mut agg = SseAggregator::new();
        // 恶意 index：不 panic、不分配巨桶，正常调用不受影响
        feed_all(
            &mut agg,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1000000000,"id":"evil","function":{"name":"x","arguments":"{}"}}]},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":"{}"}}]},"index":0}]}"#,
                "data: [DONE]",
            ],
        );
        let turn = agg.finish().unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_1");
    }

    #[test]
    fn test_index_at_limit_boundary() {
        let mut agg = SseAggregator::new();
        // index 63 接受、64 跳过
        feed_all(
            &mut agg,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":63,"id":"ok63","function":{"name":"a","arguments":"{}"}}]},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":64,"id":"skip64","function":{"name":"b","arguments":"{}"}}]},"index":0}]}"#,
                "data: [DONE]",
            ],
        );
        let turn = agg.finish().unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "ok63");
    }

    #[test]
    fn test_aggregator_overflow_on_content() {
        // 单行 content 增量超小 limit → Overflow（不 panic、不落半截）
        let mut agg = SseAggregator::with_limit(100);
        let big = "x".repeat(120);
        let line = format!(r#"data: {{"choices":[{{"delta":{{"content":"{big}"}},"index":0}}]}}"#);
        match agg.feed_line(&line) {
            SseFeed::Overflow => {}
            other => panic!("expected Overflow, got {other:?}"),
        }
        // 已超限：聚合文本为空（未写入超限内容）
        let turn = agg.finish().unwrap();
        assert!(turn.text.is_empty());
        assert!(turn.tool_calls.is_empty());
    }

    #[test]
    fn test_aggregator_overflow_on_tool_arguments() {
        // tool_calls 的 arguments 拼接超小 limit → Overflow
        let mut agg = SseAggregator::with_limit(50);
        let big = "y".repeat(80);
        let line = format!(
            r#"data: {{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"c1","function":{{"name":"shell","arguments":"{big}"}}}}]}},"index":0}}]}}"#
        );
        match agg.feed_line(&line) {
            SseFeed::Overflow => {}
            other => panic!("expected Overflow, got {other:?}"),
        }
    }

    #[test]
    fn test_aggregator_below_limit_still_ok() {
        // 未超限的普通流不受影响
        let mut agg = SseAggregator::with_limit(1000);
        let (text, done) = feed_all(
            &mut agg,
            &[
                r#"data: {"choices":[{"delta":{"content":"你好"},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{\"cmd\":\"ls\"}"}}]},"index":0}]}"#,
                "data: [DONE]",
            ],
        );
        assert!(done);
        assert_eq!(text, "你好");
        let turn = agg.finish().unwrap();
        assert_eq!(turn.text, "你好");
        assert_eq!(turn.tool_calls.len(), 1);
    }

    #[test]
    fn test_aggregator_saw_data_flag() {
        // 空流兜底判定：注释行/空白不计 data 行；data: 行计入
        let mut agg = SseAggregator::new();
        assert!(!agg.saw_data());
        assert!(matches!(agg.feed_line(": comment"), SseFeed::None));
        assert!(!agg.saw_data());
        assert!(matches!(
            agg.feed_line(r#"data: {"choices":[{"delta":{"content":"x"},"index":0}]}"#),
            SseFeed::Content(_)
        ));
        assert!(agg.saw_data());
    }

    #[test]
    fn test_reasoning_deltas_aggregated() {
        let mut agg = SseAggregator::new();
        feed_all(
            &mut agg,
            &[
                r#"data: {"choices":[{"delta":{"role":"assistant"},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"reasoning_content":"让我想想"},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"reasoning_content":"...\n"},"index":0}]}"#,
                r#"data: {"choices":[{"delta":{"content":"答案"},"index":0}]}"#,
                "data: [DONE]",
            ],
        );
        let turn = agg.finish().unwrap();
        assert_eq!(turn.reasoning, "让我想想...\n");
        assert_eq!(turn.text, "答案");
    }

    #[test]
    fn test_reasoning_and_content_same_delta() {
        // DeepSeek 同一 delta 可同时携带 reasoning_content 与 content
        let mut agg = SseAggregator::new();
        let feed = agg.feed_line(
            r#"data: {"choices":[{"delta":{"reasoning_content":"thinking...","content":"answer"},"index":0}]}"#,
        );
        match feed {
            SseFeed::Thought { reasoning, content } => {
                assert_eq!(reasoning, "thinking...");
                assert_eq!(content.as_deref(), Some("answer"));
            }
            other => panic!("expected Thought, got {other:?}"),
        }
        let turn = agg.finish().unwrap();
        assert_eq!(turn.reasoning, "thinking...");
        assert_eq!(turn.text, "answer");
    }

    #[test]
    fn test_reasoning_overflow() {
        let mut agg = SseAggregator::with_limit(100);
        let big = "x".repeat(120);
        let line = format!(
            r#"data: {{"choices":[{{"delta":{{"reasoning_content":"{big}"}},"index":0}}]}}"#
        );
        match agg.feed_line(&line) {
            SseFeed::Overflow => {}
            other => panic!("expected Overflow, got {other:?}"),
        }
        let turn = agg.finish().unwrap();
        assert!(turn.reasoning.is_empty());
    }
}
