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

/// 非流式：OpenAI chat.completion 响应 JSON → Anthropic Messages 响应 JSON。
pub fn openai_response_to_anthropic(openai: &Value) -> Value {
    let content: Vec<Value> = openai["choices"]
        .as_array()
        .map(|choices| {
            choices
                .iter()
                .map(|c| {
                    json!({
                        "type": "text",
                        "text": c["message"]["content"].as_str().unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

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
        "usage": {
            "input_tokens": openai["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            "output_tokens": openai["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        },
    })
}

/// 流式：把上游 OpenAI SSE 字节流逐 chunk 翻译成 Anthropic SSE 事件流。
///
/// 用法：对上游每个字节块调用 [`push`](Self::push)，把返回的字节直接发给客户端。
/// 内部缓冲跨 chunk 边界的不完整行。
pub struct AnthropicSseTranslator {
    /// 尚未遇到换行符的不完整数据
    line_buf: String,
    /// message_start 是否已发送
    started: bool,
    /// content_block 是否处于打开状态
    block_open: bool,
    /// message_stop 是否已发送
    closed: bool,
}

impl Default for AnthropicSseTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicSseTranslator {
    pub fn new() -> Self {
        Self {
            line_buf: String::new(),
            started: false,
            block_open: false,
            closed: false,
        }
    }

    /// 喂入上游字节块，返回可立即发给客户端的 Anthropic SSE 字节。
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.line_buf.push_str(&String::from_utf8_lossy(bytes));

        let mut out = String::new();
        // 逐行处理完整行，残留部分留在 line_buf
        while let Some(pos) = self.line_buf.find('\n') {
            let line = self.line_buf[..pos].trim_end_matches('\r').to_string();
            self.line_buf.drain(..=pos);
            self.process_line(&line, &mut out);
        }
        out.into_bytes()
    }

    fn process_line(&mut self, line: &str, out: &mut String) {
        let Some(payload) = line.strip_prefix("data:") else {
            return; // event:/comment/空行一律忽略，事件类型由转换器自行生成
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            self.close("end_turn", 0, out);
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
                    "usage": {
                        "input_tokens": chunk["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                        "output_tokens": 0,
                    },
                },
            });
            push_event(out, "message_start", &msg);

            let block_start = json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            });
            push_event(out, "content_block_start", &block_start);
            self.block_open = true;
        }

        // 文本增量
        if let Some(text) = chunk["choices"][0]["delta"]["content"].as_str() {
            if !text.is_empty() {
                let delta = json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                });
                push_event(out, "content_block_delta", &delta);
            }
        }

        // 结束
        if let Some(reason) = chunk["choices"][0]["finish_reason"].as_str() {
            let output_tokens = chunk["usage"]["completion_tokens"].as_u64().unwrap_or(0);
            self.close(&map_stop_reason(reason), output_tokens, out);
        }
    }

    /// 发送收尾事件（content_block_stop / message_delta / message_stop）。幂等。
    fn close(&mut self, stop_reason: &str, output_tokens: u64, out: &mut String) {
        if self.closed || !self.started {
            return;
        }
        self.closed = true;

        if self.block_open {
            self.block_open = false;
            let block_stop = json!({
                "type": "content_block_stop",
                "index": 0,
            });
            push_event(out, "content_block_stop", &block_stop);
        }

        let msg_delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": output_tokens},
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
}
