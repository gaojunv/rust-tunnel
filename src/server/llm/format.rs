//! OpenAI ↔ Anthropic 格式互转。
//!
//! 上游四个提供商均为 OpenAI 兼容格式；当客户端使用 Anthropic Messages API
//! 时，网关在响应侧把 OpenAI 格式转回 Anthropic 格式（非流式整体转换，
//! 流式逐 chunk 转换，见 [`AnthropicSseTranslator`]）。

use serde_json::{json, Value};

/// OpenAI `finish_reason` → Anthropic `stop_reason` 映射。
pub fn map_stop_reason(openai: &str) -> String {
    match openai {
        "stop" => "end_turn".to_string(),
        "length" => "max_tokens".to_string(),
        "tool_calls" => "tool_use".to_string(),
        other => other.to_string(),
    }
}

/// 把 OpenAI usage 对象转成 Anthropic usage 对象，保留缓存与输入细分。
///
/// 映射规则（与 [`crate::server::llm::usage::extract_usage`] 的解析口径互逆）：
/// - `prompt_tokens` → `input_tokens`（新增未缓存输入 = prompt - cache_hit）
/// - `prompt_cache_hit_tokens` → `cache_read_input_tokens`
/// - `completion_tokens` → `output_tokens`
///
/// `input_tokens` 按 Anthropic 口径取「未缓存新增输入」，让 `extract_usage` 能
/// 用 `input + cache_read + cache_creation` 还原出完整 prompt_tokens，进而
/// 算出 cache_hit / cache_miss。缺省字段不输出（保持 Anthropic 响应整洁）。
fn openai_usage_to_anthropic(usage: &Value) -> Value {
    if !usage.is_object() {
        return json!({ "input_tokens": 0, "output_tokens": 0 });
    }
    let get = |k: &str| usage.get(k).and_then(Value::as_i64);
    let prompt = get("prompt_tokens").unwrap_or(0);
    let completion = get("completion_tokens").unwrap_or(0);
    let cache_hit = get("prompt_cache_hit_tokens")
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    // 新增未缓存输入；上游给了 prompt_cache_miss_tokens 时优先用（与计费口径一致）。
    let input = get("prompt_cache_miss_tokens").unwrap_or(prompt - cache_hit);

    let mut out = json!({
        "input_tokens": input,
        "output_tokens": completion,
    });
    if cache_hit > 0 {
        out["cache_read_input_tokens"] = json!(cache_hit);
    }
    out
}

/// 非流式：OpenAI chat.completion 响应 JSON → Anthropic Messages 响应 JSON。
///
/// 除了 `message.content` 转成 `text` 块外，`message.tool_calls` 数组会被展开为
/// Anthropic 的 `tool_use` content 块（`arguments` 字符串 parse 回对象）。
pub fn openai_response_to_anthropic(openai: &Value) -> Value {
    let mut content: Vec<Value> = Vec::new();

    // text 块（如果有）
    if let Some(text) = openai["choices"][0]["message"]["content"].as_str() {
        if !text.is_empty() {
            content.push(json!({ "type": "text", "text": text }));
        }
    }

    // tool_use 块（如果有）
    if let Some(calls) = openai["choices"][0]["message"]["tool_calls"].as_array() {
        for call in calls {
            let id = call["id"].as_str().unwrap_or("");
            let name = call["function"]["name"].as_str().unwrap_or("");
            let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
            // OpenAI 的 arguments 是 JSON 字符串；Anthropic 的 input 是对象。
            let input: Value = serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }

    let finish_reason = openai["choices"][0]["finish_reason"]
        .as_str()
        .unwrap_or("stop");

    json!({
        "id": openai["id"].as_str().unwrap_or(""),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": openai["model"].as_str().unwrap_or(""),
        "stop_reason": map_stop_reason(finish_reason),
        "stop_sequence": null,
        "usage": openai_usage_to_anthropic(&openai["usage"]),
    })
}

/// 流式：把上游 OpenAI SSE 字节流逐 chunk 翻译成 Anthropic SSE 事件流。
///
/// 用法：对上游每个字节块调用 [`push`](Self::push)，把返回的字节直接发给客户端。
/// 内部缓冲跨 chunk 边界的不完整行。
///
/// **多 content block 处理**：
/// - 文本内容（`delta.content`）始终占 Anthropic block index 0（若出现）；
/// - 每个 tool_call 按上游 `delta.tool_calls[i].index` 首次出现顺序分配 index 1、2、...；
/// - 所有已开启的 block 会在 message_stop 前逐个发送 `content_block_stop`。
pub struct AnthropicSseTranslator {
    /// 尚未遇到换行符的不完整数据（原始字节）。
    ///
    /// 必须用字节而非 String：上游按 token 吐流，一个 UTF-8 多字节字符
    /// （中文 3 字节、emoji 4 字节）可能被切到两个网络块。若按块做
    /// `from_utf8_lossy`，残缺字节会物化成 U+FFFD 替换符、永久污染内容。
    /// 按字节缓冲、只在凑满一行（`\n` 边界）后才转 UTF-8，可从根源避免。
    line_buf: Vec<u8>,
    /// message_start 是否已发送
    started: bool,
    /// message_stop 是否已发送
    closed: bool,
    /// 文本 block 是否已发送 content_block_start（占据 anthropic index 0）
    text_block_open: bool,
    /// 上游 tool_call index → 分配的 anthropic block index
    tool_blocks: std::collections::HashMap<u64, u32>,
    /// 下一个可分配的 anthropic block index（text 用 0；tool 从 1 起，除非无 text）
    next_block_index: u32,
}

impl Default for AnthropicSseTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicSseTranslator {
    pub fn new() -> Self {
        Self {
            line_buf: Vec::new(),
            started: false,
            closed: false,
            text_block_open: false,
            tool_blocks: std::collections::HashMap::new(),
            // text block（若出现）用 0；tool block 用 1、2、... —— 若最终无 text，
            // Anthropic 允许 tool_use block 从任何 index 开始，因此从 0 起也可以。
            // 这里选择：text 出现即占 0，tool 从 next_block_index 起（初始 1）。
            next_block_index: 1,
        }
    }

    /// 喂入上游字节块，返回可立即发给客户端的 Anthropic SSE 字节。
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.line_buf.extend_from_slice(bytes);

        let mut out = String::new();
        // 逐行处理完整行，残留部分（含未凑齐的 UTF-8 序列）留在 line_buf
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.line_buf.drain(..=pos).collect();
            // 去掉末尾 \n 与可选 \r；此时行内不再含被切断的多字节字符
            let line_bytes = &line_bytes[..line_bytes.len() - 1];
            let line_str = String::from_utf8_lossy(line_bytes.strip_suffix(b"\r").unwrap_or(line_bytes));
            self.process_line(&line_str, &mut out);
        }
        out.into_bytes()
    }

    fn process_line(&mut self, line: &str, out: &mut String) {
        let Some(payload) = line.strip_prefix("data:") else {
            return; // event:/comment/空行一律忽略，事件类型由转换器自行生成
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            self.close("end_turn", json!({ "output_tokens": 0 }), out);
            return;
        }
        let chunk: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return, // 无法解析的行直接跳过
        };

        if !self.started {
            self.started = true;
            let msg = json!({
                "type": "message_start",
                "message": {
                    "id": chunk["id"].as_str().unwrap_or(""),
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": chunk["model"].as_str().unwrap_or(""),
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": openai_usage_to_anthropic(&chunk["usage"]),
                },
            });
            push_event(out, "message_start", &msg);
        }

        // 文本增量
        if let Some(text) = chunk["choices"][0]["delta"]["content"].as_str() {
            if !text.is_empty() {
                if !self.text_block_open {
                    self.text_block_open = true;
                    let block_start = json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {"type": "text", "text": ""},
                    });
                    push_event(out, "content_block_start", &block_start);
                }
                let delta = json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                });
                push_event(out, "content_block_delta", &delta);
            }
        }

        // 工具调用增量
        if let Some(calls) = chunk["choices"][0]["delta"]["tool_calls"].as_array() {
            for call in calls {
                // 上游 index：OpenAI 流式规定用 index 标识哪个 tool_call；缺省当 0。
                let up_idx = call.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                // 首次出现：分配 anthropic block index 并发 content_block_start
                let anthropic_idx = if let Some(idx) = self.tool_blocks.get(&up_idx) {
                    *idx
                } else {
                    let idx = self.next_block_index;
                    self.next_block_index += 1;
                    self.tool_blocks.insert(up_idx, idx);
                    let id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let block_start = json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": {},
                        },
                    });
                    push_event(out, "content_block_start", &block_start);
                    idx
                };

                // arguments 增量 → Anthropic input_json_delta
                if let Some(args) = call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                {
                    if !args.is_empty() {
                        let delta = json!({
                            "type": "content_block_delta",
                            "index": anthropic_idx,
                            "delta": {"type": "input_json_delta", "partial_json": args},
                        });
                        push_event(out, "content_block_delta", &delta);
                    }
                }
            }
        }

        // 结束
        if let Some(reason) = chunk["choices"][0]["finish_reason"].as_str() {
            let usage = openai_usage_to_anthropic(&chunk["usage"]);
            self.close(&map_stop_reason(reason), usage, out);
        }
    }

    /// 发送收尾事件（所有已开启 block 的 content_block_stop → message_delta → message_stop）。幂等。
    ///
    /// `usage` 必须携带完整输入细分（input_tokens / cache_read_input_tokens），
    /// 而非仅 output_tokens —— 下游 usage 扫描器靠它命中「完整 usage」分支，
    /// 避免与 message_start 的空 input 合并后丢失 prompt/cache 统计。
    fn close(&mut self, stop_reason: &str, usage: Value, out: &mut String) {
        if self.closed || !self.started {
            return;
        }
        self.closed = true;

        // 关闭 text block（若开启过）
        if self.text_block_open {
            self.text_block_open = false;
            let block_stop = json!({
                "type": "content_block_stop",
                "index": 0,
            });
            push_event(out, "content_block_stop", &block_stop);
        }

        // 关闭所有 tool blocks，按 anthropic index 顺序发送
        let mut indices: Vec<u32> = self.tool_blocks.values().copied().collect();
        indices.sort_unstable();
        for idx in indices {
            let block_stop = json!({
                "type": "content_block_stop",
                "index": idx,
            });
            push_event(out, "content_block_stop", &block_stop);
        }
        self.tool_blocks.clear();

        let msg_delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": usage,
        });
        push_event(out, "message_delta", &msg_delta);

        let msg_stop = json!({"type": "message_stop"});
        push_event(out, "message_stop", &msg_stop);
    }
}

fn push_event(out: &mut String, event: &str, data: &Value) {
    out.push_str("event: ");
    out.push_str(event);
    out.push_str("\ndata: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── map_stop_reason ─────────────────────────────────────────

    #[test]
    fn stop_maps_to_end_turn() {
        assert_eq!(map_stop_reason("stop"), "end_turn");
    }

    #[test]
    fn length_maps_to_max_tokens() {
        assert_eq!(map_stop_reason("length"), "max_tokens");
    }

    #[test]
    fn tool_calls_maps_to_tool_use() {
        assert_eq!(map_stop_reason("tool_calls"), "tool_use");
    }

    #[test]
    fn unknown_reason_passthrough() {
        assert_eq!(map_stop_reason("content_filter"), "content_filter");
    }

    // ── 非流式响应转换 ──────────────────────────────────────────

    #[test]
    fn non_stream_response_converted() {
        let openai = json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "你好"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });

        let a = openai_response_to_anthropic(&openai);
        assert_eq!(a["id"], "chatcmpl-123");
        assert_eq!(a["type"], "message");
        assert_eq!(a["role"], "assistant");
        assert_eq!(a["content"][0]["type"], "text");
        assert_eq!(a["content"][0]["text"], "你好");
        assert_eq!(a["stop_reason"], "end_turn");
        assert_eq!(a["usage"]["input_tokens"], 10);
        assert_eq!(a["usage"]["output_tokens"], 5);
    }

    #[test]
    fn non_stream_length_finish_maps_to_max_tokens() {
        let openai = json!({
            "id": "x", "model": "m",
            "choices": [{"message": {"content": "..."}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 100}
        });
        let a = openai_response_to_anthropic(&openai);
        assert_eq!(a["stop_reason"], "max_tokens");
    }

    // ── 流式 SSE 翻译 ───────────────────────────────────────────

    fn openai_chunk(content: &str, finish: Option<&str>) -> String {
        let finish_json = match finish {
            Some(f) => format!("\"{f}\""),
            None => "null".to_string(),
        };
        format!(
            "data: {{\"id\":\"c1\",\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}},\"finish_reason\":{finish_json}}}]}}\n\n"
        )
    }

    #[test]
    fn stream_first_chunk_emits_message_start_and_block_start() {
        let mut t = AnthropicSseTranslator::new();
        let out = String::from_utf8(t.push(openai_chunk("Hello", None).as_bytes())).unwrap();
        assert!(out.contains("event: message_start"));
        assert!(out.contains("event: content_block_start"));
        assert!(out.contains("event: content_block_delta"));
        assert!(out.contains("\"text\":\"Hello\""));
        assert!(!out.contains("message_stop"));
    }

    #[test]
    fn stream_finish_reason_closes_message() {
        let mut t = AnthropicSseTranslator::new();
        let _ = t.push(openai_chunk("Hi", None).as_bytes());
        let out = String::from_utf8(t.push(openai_chunk("", Some("stop")).as_bytes())).unwrap();
        assert!(out.contains("event: content_block_stop"));
        assert!(out.contains("event: message_delta"));
        assert!(out.contains("\"stop_reason\":\"end_turn\""));
        assert!(out.contains("event: message_stop"));
    }

    #[test]
    fn stream_done_after_finish_is_noop() {
        let mut t = AnthropicSseTranslator::new();
        let _ = t.push(openai_chunk("Hi", None).as_bytes());
        let _ = t.push(openai_chunk("", Some("stop")).as_bytes());
        let out = t.push(b"data: [DONE]\n\n");
        assert!(out.is_empty(), "already closed — DONE must not emit twice");
    }

    #[test]
    fn stream_done_without_finish_closes_gracefully() {
        let mut t = AnthropicSseTranslator::new();
        let _ = t.push(openai_chunk("Hi", None).as_bytes());
        let out = String::from_utf8(t.push(b"data: [DONE]\n\n")).unwrap();
        assert!(out.contains("event: message_stop"));
    }

    #[test]
    fn stream_partial_line_across_chunks() {
        let mut t = AnthropicSseTranslator::new();
        let chunk = openai_chunk("World", None);
        let bytes = chunk.as_bytes();
        let mid = bytes.len() / 2;

        let out1 = t.push(&bytes[..mid]);
        let out2 = t.push(&bytes[mid..]);
        let combined = [out1, out2].concat();
        let text = String::from_utf8(combined).unwrap();
        assert!(text.contains("event: message_start"));
        assert!(text.contains("\"text\":\"World\""));
    }

    #[test]
    fn stream_multibyte_utf8_split_across_chunks_no_replacement_char() {
        // 回归：中文（3 字节）/ emoji（4 字节）被从字符中间切到两个网络块时，
        // 旧实现按块 from_utf8_lossy 会把残缺字节物化成 U+FFFD，客户端看到乱码。
        let mut t = AnthropicSseTranslator::new();
        let chunk = openai_chunk("你好👋", None);
        let bytes = chunk.as_bytes();

        // 找到每个多字节字符的字节边界，逐字节切碎喂入（最严苛的切法）
        for i in 1..bytes.len() {
            let mut t2 = AnthropicSseTranslator::new();
            let out1 = t2.push(&bytes[..i]);
            let out2 = t2.push(&bytes[i..]);
            let combined = [out1, out2].concat();
            let text = String::from_utf8(combined)
                .unwrap_or_else(|e| panic!("split at byte {i} produced invalid utf8: {e}"));
            assert!(
                !text.contains('\u{FFFD}'),
                "split at byte {i} produced replacement char: {text}"
            );
            assert!(
                text.contains("你好👋"),
                "split at byte {i} lost content: {text}"
            );
        }

        // 整体也验证一次
        let out = t.push(bytes);
        assert!(String::from_utf8(out).unwrap().contains("你好👋"));
    }

    #[test]
    fn stream_multiple_events_in_one_chunk() {
        let mut t = AnthropicSseTranslator::new();
        let two = format!(
            "{}{}",
            openai_chunk("A", None),
            openai_chunk("B", Some("stop"))
        );
        let out = String::from_utf8(t.push(two.as_bytes())).unwrap();
        assert!(out.contains("\"text\":\"A\""));
        assert!(out.contains("\"text\":\"B\""));
        // message_start 只应出现一次
        assert_eq!(out.matches("event: message_start").count(), 1);
        assert!(out.contains("event: message_stop"));
    }

    #[test]
    fn stream_output_is_valid_anthropic_event_sequence() {
        let mut t = AnthropicSseTranslator::new();
        let mut all = Vec::new();
        all.extend(t.push(openai_chunk("你", None).as_bytes()));
        all.extend(t.push(openai_chunk("好", None).as_bytes()));
        all.extend(t.push(openai_chunk("", Some("length")).as_bytes()));
        all.extend(t.push(b"data: [DONE]\n\n"));
        let text = String::from_utf8(all).unwrap();

        // 事件顺序：message_start → block_start → delta×2 → block_stop → message_delta → message_stop
        let order = [
            "event: message_start",
            "event: content_block_start",
            "event: content_block_delta",
            "event: content_block_stop",
            "event: message_delta",
            "event: message_stop",
        ];
        let mut last_pos = 0;
        for ev in order {
            let pos = text
                .find(ev)
                .unwrap_or_else(|| panic!("missing {ev} in:\n{text}"));
            assert!(pos >= last_pos, "event {ev} out of order");
            last_pos = pos;
        }
        assert!(text.contains("\"stop_reason\":\"max_tokens\""));
    }

    #[test]
    fn stream_usage_tokens_propagated() {
        let mut t = AnthropicSseTranslator::new();
        let chunk = "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n";
        let out = String::from_utf8(t.push(chunk.as_bytes())).unwrap();
        assert!(out.contains("\"input_tokens\":7"));
        assert!(out.contains("\"output_tokens\":3"));
    }

    #[test]
    fn stream_opencode_style_usage_preserves_cache_and_input() {
        // 回归：opencode 流的 usage 只出现在 finish_reason 收尾 chunk，
        // 且携带 DeepSeek 缓存字段。翻译后的 message_delta 必须带完整 input 细分，
        // 让下游 UsageSseScanner 命中「完整 usage」分支（prompt_tokens>0），
        // 否则会与 message_start 的空 input 合并、丢失 prompt/cache 统计。
        let mut t = AnthropicSseTranslator::new();
        let mut all = Vec::new();
        all.extend(t.push(b"data: {\"id\":\"c1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}],\"usage\":null}\n\n"));
        all.extend(t.push(b"data: {\"id\":\"c1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":87,\"completion_tokens\":16,\"total_tokens\":103,\"prompt_cache_hit_tokens\":60,\"prompt_cache_miss_tokens\":27}}\n\n"));
        let text = String::from_utf8(all).unwrap();

        // message_delta 的 usage 含 input/cache 细分
        let delta_pos = text.find("message_delta").expect("missing message_delta");
        let tail = &text[delta_pos..];
        assert!(tail.contains("\"cache_read_input_tokens\":60"), "{tail}");
        assert!(tail.contains("\"input_tokens\":27"), "{tail}");
        assert!(tail.contains("\"output_tokens\":16"), "{tail}");

        // 端到端：翻译输出喂回 scanner，必须解析出完整 prompt/cache
        let mut scanner = crate::server::llm::usage::UsageSseScanner::new();
        scanner.push(text.as_bytes());
        let u = scanner.finish();
        assert_eq!(u.prompt_tokens, 87);
        assert_eq!(u.cache_hit_tokens, 60);
        assert_eq!(u.cache_miss_tokens, 27);
        assert_eq!(u.completion_tokens, 16);
    }

    // ── 工具调用：非流式 ──────────────────────────────────────

    #[test]
    fn non_stream_response_tool_calls_expanded() {
        // OpenAI 的 message.tool_calls 应展开为 Anthropic tool_use 块，
        // arguments 字符串 parse 回 input 对象；stop_reason = tool_use。
        let openai = json!({
            "id": "chatcmpl-t1",
            "model": "m",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"loc\":\"SF\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 8}
        });
        let a = openai_response_to_anthropic(&openai);
        assert_eq!(a["stop_reason"], "tool_use");
        let content = a["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["id"], "call_1");
        assert_eq!(content[0]["name"], "get_weather");
        assert_eq!(content[0]["input"]["loc"], "SF");
    }

    #[test]
    fn non_stream_response_text_and_tool_call_coexist() {
        let openai = json!({
            "id": "x", "model": "m",
            "choices": [{
                "message": {
                    "content": "sure",
                    "tool_calls": [{
                        "id": "c1", "type": "function",
                        "function": {"name": "t", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let a = openai_response_to_anthropic(&openai);
        let content = a["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "sure");
        assert_eq!(content[1]["type"], "tool_use");
    }

    // ── 工具调用：流式 ────────────────────────────────────────

    fn openai_tool_chunk(
        index: u64,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
        finish: Option<&str>,
    ) -> String {
        let mut func = serde_json::Map::new();
        if let Some(n) = name {
            func.insert("name".to_string(), json!(n));
        }
        if let Some(a) = arguments {
            func.insert("arguments".to_string(), json!(a));
        }
        let mut call = serde_json::Map::new();
        call.insert("index".to_string(), json!(index));
        if let Some(i) = id {
            call.insert("id".to_string(), json!(i));
        }
        if !func.is_empty() {
            call.insert("function".to_string(), Value::Object(func));
        }
        let chunk = json!({
            "id": "c1",
            "model": "m",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [Value::Object(call)]},
                "finish_reason": finish
            }]
        });
        format!("data: {}\n\n", chunk)
    }

    #[test]
    fn stream_single_tool_call_argument_deltas() {
        // 单个 tool_call，分多次 arguments 增量 → input_json_delta 序列。
        let mut t = AnthropicSseTranslator::new();
        let mut all = Vec::new();
        // 首块：id + name + arguments 部分
        all.extend(t.push(
            openai_tool_chunk(0, Some("call_a"), Some("get_x"), Some("{\"a\":"), None).as_bytes(),
        ));
        // 后续 arguments 增量
        all.extend(t.push(openai_tool_chunk(0, None, None, Some("1}"), None).as_bytes()));
        // 结束
        all.extend(t.push(openai_tool_chunk(0, None, None, None, Some("tool_calls")).as_bytes()));
        let text = String::from_utf8(all).unwrap();

        // 首个 tool_use content_block_start，index=1（因为无 text，从 next_block_index=1 开始）
        assert!(text.contains("event: content_block_start"));
        assert!(text.contains("\"type\":\"tool_use\""));
        assert!(text.contains("\"id\":\"call_a\""));
        assert!(text.contains("\"name\":\"get_x\""));
        // input_json_delta 两段 partial_json
        assert!(text.contains("\"type\":\"input_json_delta\""));
        assert!(text.contains("\"partial_json\":\"{\\\"a\\\":\""));
        assert!(text.contains("\"partial_json\":\"1}\""));
        // 收尾
        assert!(text.contains("event: content_block_stop"));
        assert!(text.contains("\"stop_reason\":\"tool_use\""));
        assert!(text.contains("event: message_stop"));
        // 不应意外开启 text block
        assert!(!text.contains("\"type\":\"text\""));
    }

    #[test]
    fn stream_text_plus_tool_call_indices_correct() {
        // 文本 + tool_call：text 占 index 0，tool 从 index 1。
        let mut t = AnthropicSseTranslator::new();
        let mut all = Vec::new();
        all.extend(t.push(openai_chunk("hello", None).as_bytes()));
        all.extend(
            t.push(openai_tool_chunk(0, Some("c1"), Some("f"), Some("{}"), None).as_bytes()),
        );
        all.extend(t.push(openai_tool_chunk(0, None, None, None, Some("tool_calls")).as_bytes()));
        let text = String::from_utf8(all).unwrap();

        let events = parse_sse_events(&text);
        // 找到两个 content_block_start 事件
        let starts: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 2, "expected 2 block_start, got:\n{text}");
        assert_eq!(starts[0]["index"], 0);
        assert_eq!(starts[0]["content_block"]["type"], "text");
        assert_eq!(starts[1]["index"], 1);
        assert_eq!(starts[1]["content_block"]["type"], "tool_use");

        // 两个 block 都要 content_block_stop
        let stops: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "content_block_stop")
            .collect();
        let stop_indices: Vec<u64> = stops.iter().map(|s| s["index"].as_u64().unwrap()).collect();
        assert!(stop_indices.contains(&0), "missing stop for text block");
        assert!(stop_indices.contains(&1), "missing stop for tool block");
    }

    #[test]
    fn stream_two_parallel_tool_calls() {
        // 两个并行 tool_call（上游 index 0 / 1），分别对应 anthropic block 1 / 2。
        let mut t = AnthropicSseTranslator::new();
        let mut all = Vec::new();
        all.extend(
            t.push(openai_tool_chunk(0, Some("a"), Some("fa"), Some("{}"), None).as_bytes()),
        );
        all.extend(
            t.push(openai_tool_chunk(1, Some("b"), Some("fb"), Some("{}"), None).as_bytes()),
        );
        all.extend(t.push(openai_tool_chunk(0, None, None, None, Some("tool_calls")).as_bytes()));
        let text = String::from_utf8(all).unwrap();

        let events = parse_sse_events(&text);
        let starts: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 2, "expected 2 tool block_start:\n{text}");
        assert_eq!(starts[0]["index"], 1);
        assert_eq!(starts[0]["content_block"]["id"], "a");
        assert_eq!(starts[1]["index"], 2);
        assert_eq!(starts[1]["content_block"]["id"], "b");

        // block_stop 顺序按 index 升序
        let stops: Vec<u64> = events
            .iter()
            .filter(|e| e["type"] == "content_block_stop")
            .map(|s| s["index"].as_u64().unwrap())
            .collect();
        assert_eq!(stops, vec![1, 2], "block_stop must be ascending: {stops:?}");
    }

    /// 解析 SSE 文本为事件 JSON 列表（丢弃 event: 前缀行）。
    fn parse_sse_events(text: &str) -> Vec<Value> {
        text.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|s| *s != "[DONE]")
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect()
    }
}
