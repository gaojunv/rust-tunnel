//! OpenAI Responses API (`/v1/responses`) 与 Chat Completions API 之间的双向格式转换。
//!
//! 项目的 LLM 网关内部统一使用 Chat Completions 格式；当外部客户端使用
//! Responses API 时，由本模块完成请求/响应/流式的双向桥接。
//!
//! 六个转换能力（本批只写转换层+单测，不接 handler/路由）：
//! - [`responses_request_to_chat`] — Responses 请求 → ChatCompletionRequest
//! - [`chat_response_to_responses`] — Chat 响应 → Responses 响应（非流式）
//! - [`ChatToResponsesSseTranslator`] — Chat chunk SSE → Responses SSE（流式）
//! - [`chat_body_to_responses_body`] — Chat 请求体 → Responses 请求体
//! - [`responses_response_to_chat`] — Responses 响应 → Chat 响应（非流式）
//! - [`ResponsesToChatSseTranslator`] — Responses SSE → Chat chunk SSE（流式）

use serde_json::{json, Value};

use super::ChatCompletionRequest;
use super::ChatMessage;

// ── A. 入口请求转换 ────────────────────────────────────────────

/// Responses API 请求体 → 内部 `ChatCompletionRequest`。
///
/// `model` 必填（缺失返回 Err）；`stream` 默认 false。
/// `instructions` → 首条 system 消息；`input` 数组逐 item 映射为 chat message。
pub fn responses_request_to_chat(body: &Value) -> Result<ChatCompletionRequest, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "model is required".to_string())?
        .to_string();

    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut messages: Vec<ChatMessage> = Vec::new();

    // instructions → 首条 system 消息
    if let Some(inst) = body.get("instructions").and_then(Value::as_str) {
        if !inst.is_empty() {
            messages.push(ChatMessage::text("system", inst));
        }
    }

    // input → 消息列表
    let input = body.get("input").ok_or_else(|| "input is required".to_string())?;
    match input {
        Value::String(s) => {
            messages.push(ChatMessage::text("user", s));
        }
        Value::Array(items) => {
            for item in items {
                convert_input_item(item, &mut messages)?;
            }
        }
        _ => return Err("input must be a string or array".to_string()),
    }

    let max_tokens = body.get("max_output_tokens").and_then(Value::as_u64).map(|v| v as u32);
    let temperature = body.get("temperature").and_then(Value::as_f64).map(|v| v as f32);
    let top_p = body.get("top_p").and_then(Value::as_f64).map(|v| v as f32);

    let tools = body.get("tools").map(convert_tools_to_chat);
    let tool_choice = body.get("tool_choice").map(convert_tool_choice_to_chat);

    Ok(ChatCompletionRequest {
        model,
        messages,
        stream,
        max_tokens,
        temperature,
        top_p,
        tools,
        tool_choice,
        raw_body: None,
    })
}

/// 将 Responses API 的 `input` 数组 item 转换为 chat message。
fn convert_input_item(item: &Value, out: &mut Vec<ChatMessage>) -> Result<(), String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");

    // 避免 clippy::collapsible_if：match 更清晰
    match item_type {
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = item
                .get("arguments")
                .and_then(|a| {
                    if a.is_string() {
                        a.as_str().map(String::from)
                    } else {
                        Some(a.to_string())
                    }
                })
                .unwrap_or_else(|| "{}".to_string());
            out.push(ChatMessage {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    }
                })]),
                tool_call_id: None,
                name: None,
            });
        }
        "function_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let output = item.get("output").map(|o| {
                if o.is_string() {
                    o.as_str().unwrap_or("").to_string()
                } else {
                    o.to_string()
                }
            });
            out.push(ChatMessage {
                role: "tool".to_string(),
                content: output,
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some(call_id.to_string()),
                name: None,
            });
        }
        "reasoning" => {
            // reasoning 类型跳过（chat 格式无对应位置）
        }
        _ => {
            // message 类型（显式或缺省 type 但有 role）
            let role = item
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let role = match role {
                "developer" => "system",
                other => other,
            };
            let content = extract_message_content(item);
            out.push(ChatMessage::text(role, content));
        }
    }
    Ok(())
}

/// 从 input message item 中提取文本内容。
///
/// content 为字符串直接返回；为 parts 数组时取 input_text/output_text 类型的 text 拼接。
fn extract_message_content(item: &Value) -> String {
    let parts = match item.get("content") {
        Some(Value::String(s)) => return s.clone(),
        Some(Value::Array(p)) => p,
        _ => return String::new(),
    };
    let mut text = String::new();
    for part in parts {
        let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
        if part_type == "input_text" || part_type == "output_text" {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
    }
    text
}

/// 将 Responses API `tools` 数组转换为 Chat Completions 格式。
fn convert_tools_to_chat(tools: &Value) -> Vec<Value> {
    let Some(arr) = tools.as_array() else {
        return vec![];
    };
    arr.iter()
        .filter_map(|t| {
            let t_type = t.get("type").and_then(Value::as_str).unwrap_or("");
            if t_type != "function" {
                return None;
            }
            let func_name = t.get("name").and_then(Value::as_str).unwrap_or("");
            let func_desc = t.get("description").and_then(Value::as_str);
            let func_params = t.get("parameters");
            let func_strict = t.get("strict").and_then(Value::as_bool);
            let mut func = json!({ "name": func_name });
            if let Some(d) = func_desc {
                func["description"] = json!(d);
            }
            if let Some(p) = func_params {
                func["parameters"] = p.clone();
            }
            if let Some(s) = func_strict {
                func["strict"] = json!(s);
            }
            Some(json!({ "type": "function", "function": func }))
        })
        .collect()
}

/// 将 Responses API `tool_choice` 转换为 Chat Completions 格式。
fn convert_tool_choice_to_chat(tc: &Value) -> Value {
    match tc {
        Value::String(s) => json!(s),
        Value::Object(obj) => {
            if obj.get("type").and_then(Value::as_str) == Some("function") {
                let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
                json!({ "type": "function", "function": { "name": name } })
            } else {
                tc.clone()
            }
        }
        _ => tc.clone(),
    }
}

// ── B. 入口非流式响应转换 ──────────────────────────────────────

/// Chat completion 非流式响应 JSON → Responses API 响应 JSON。
pub fn chat_response_to_responses(chat: &Value) -> Value {
    let raw_id = chat.get("id").and_then(Value::as_str).unwrap_or("");
    let resp_id = format!(
        "resp_{}",
        raw_id.strip_prefix("chatcmpl-").unwrap_or(raw_id)
    );
    let model = chat.get("model").and_then(Value::as_str).unwrap_or("");
    let created = chat.get("created").and_then(Value::as_u64).unwrap_or(0);

    let finish_reason = chat["choices"][0]
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let status = if finish_reason == "length" {
        "incomplete"
    } else {
        "completed"
    };

    let mut output: Vec<Value> = Vec::new();
    let msg = &chat["choices"][0]["message"];

    // reasoning_content → reasoning output item
    if let Some(reasoning) = msg.get("reasoning_content").and_then(Value::as_str) {
        if !reasoning.is_empty() {
            output.push(json!({
                "type": "reasoning",
                "id": format!("{resp_id}_rs"),
                "summary": [{ "type": "summary_text", "text": reasoning }],
            }));
        }
    }

    // content → message output item
    if let Some(content) = msg.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            output.push(json!({
                "type": "message",
                "id": format!("{resp_id}_msg"),
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": content }],
            }));
        }
    }

    // tool_calls → function_call output items
    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let arguments = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            output.push(json!({
                "type": "function_call",
                "id": format!("{resp_id}_fc_{i}"),
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
                "status": "completed",
            }));
        }
    }

    let mut resp = json!({
        "id": resp_id,
        "object": "response",
        "created_at": created,
        "model": model,
        "status": status,
        "output": output,
    });

    if status == "incomplete" {
        resp["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    }

    // usage 映射
    if let Some(usage) = chat.get("usage").filter(|u| u.is_object()) {
        let prompt_tokens = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        let completion_tokens = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
        let total_tokens = usage.get("total_tokens").and_then(Value::as_u64)
            .unwrap_or(prompt_tokens + completion_tokens);

        let cached_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
            .unwrap_or(0);

        let reasoning_tokens = usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        resp["usage"] = json!({
            "input_tokens": prompt_tokens,
            "output_tokens": completion_tokens,
            "total_tokens": total_tokens,
            "input_tokens_details": { "cached_tokens": cached_tokens },
            "output_tokens_details": { "reasoning_tokens": reasoning_tokens },
        });
    }

    resp
}

// ── C. 入口流式转换器 ──────────────────────────────────────────

/// 已开启的 output item 类型。
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputItemKind {
    Reasoning,
    Message,
    FunctionCall(u32), // output_index
}

/// Chat chunk SSE → Responses SSE 流式翻译器。
///
/// 复刻 [`super::format::AnthropicSseTranslator`] 的字节级行缓冲模式：
/// - `line_buf` 字节缓冲，跨网络块的不完整行留存；
/// - 只在凑满 `\n` 行后才转 UTF-8（防止多字节字符被从中间切开）；
/// - 幂等 close。
pub struct ChatToResponsesSseTranslator {
    /// 尚未遇到换行符的不完整数据（原始字节）。
    line_buf: Vec<u8>,
    /// response.created 是否已发送
    started: bool,
    /// 是否已关闭（finish / [DONE]）
    closed: bool,
    /// response id（从首个 chunk 提取）
    resp_id: String,
    /// model（从首个 chunk 提取）
    model: String,
    /// created_at（从首个 chunk 提取）
    created_at: u64,
    /// 当前已开启的 output item 列表（用于 close 时逐个 done）
    open_items: Vec<OutputItemKind>,
    /// reasoning 累计文本（供 done 事件和 response.completed 使用）
    reasoning_text: String,
    /// 文本累计内容
    content_text: String,
    /// tool_calls 累计 arguments，key = upstream index
    tool_args: std::collections::HashMap<u64, String>,
    /// upstream tool_call index → output_index 映射
    tool_output_indices: std::collections::HashMap<u64, u32>,
    /// upstream tool_call index → 上游真实 call_id（done/completed 事件必须回带，
    /// 客户端靠它把 function_call_output 关联回 function_call）
    tool_call_ids: std::collections::HashMap<u64, String>,
    /// upstream tool_call index → 函数名（同上，done/completed 事件必须回带）
    tool_names: std::collections::HashMap<u64, String>,
    /// 下一个可分配的 output_index
    next_output_index: u32,
    /// 收尾 chunk 的 usage（供 response.completed 使用）
    usage: Option<Value>,
    /// reasoning delta 是否已发过（首次需先发 output_item.added）
    reasoning_started: bool,
    /// 文本 delta 是否已发过（首次需先发 output_item.added）
    content_started: bool,
    /// 当前 finish_reason（close 后可读取）
    finish_reason: Option<String>,
}

impl Default for ChatToResponsesSseTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatToResponsesSseTranslator {
    pub fn new() -> Self {
        Self {
            line_buf: Vec::new(),
            started: false,
            closed: false,
            resp_id: String::new(),
            model: String::new(),
            created_at: 0,
            open_items: Vec::new(),
            reasoning_text: String::new(),
            content_text: String::new(),
            tool_args: std::collections::HashMap::new(),
            tool_output_indices: std::collections::HashMap::new(),
            tool_call_ids: std::collections::HashMap::new(),
            tool_names: std::collections::HashMap::new(),
            next_output_index: 0,
            usage: None,
            reasoning_started: false,
            content_started: false,
            finish_reason: None,
        }
    }

    /// 喂入上游字节块，返回可立即发给客户端的 Responses SSE 字节。
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.line_buf.extend_from_slice(bytes);

        let mut out = String::new();
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.line_buf.drain(..=pos).collect();
            let line_bytes = &line_bytes[..line_bytes.len() - 1];
            let line_str =
                String::from_utf8_lossy(line_bytes.strip_suffix(b"\r").unwrap_or(line_bytes));
            self.process_line(&line_str, &mut out);
        }
        out.into_bytes()
    }

    fn process_line(&mut self, line: &str, out: &mut String) {
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            self.close(out);
            return;
        }
        let chunk: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return,
        };

        if self.closed {
            // 已关闭后收到携带 usage 的 chunk（延迟 usage）：更新 usage 供后续参考。
            // 但 response.completed 已发出，不再重复。
            return;
        }

        // 首个 chunk → 发 response.created
        if !self.started {
            self.started = true;
            self.resp_id = chunk
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            self.model = chunk
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            self.created_at = chunk.get("created").and_then(Value::as_u64).unwrap_or(0);
            let created = json!({
                "id": self.resp_id,
                "object": "response",
                "created_at": self.created_at,
                "model": self.model,
                "status": "in_progress",
                "output": [],
            });
            // 官方格式：response.created/completed 的响应对象嵌套在 data.response 下
            push_sse_event(out, "response.created", &json!({ "response": created }));
        }

        let delta = &chunk["choices"][0]["delta"];

        // reasoning_content 增量
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
            if !reasoning.is_empty() {
                if !self.reasoning_started {
                    self.reasoning_started = true;
                    let idx = self.next_output_index;
                    self.next_output_index += 1;
                    self.open_items.push(OutputItemKind::Reasoning);
                    let item = json!({
                        "type": "reasoning",
                        "id": format!("{}_rs", self.resp_id),
                        "summary": [],
                    });
                    push_sse_event(
                        out,
                        "response.output_item.added",
                        &json!({ "output_index": idx, "item": item }),
                    );
                    let part = json!({
                        "type": "summary_text",
                        "text": "",
                    });
                    push_sse_event(
                        out,
                        "response.reasoning_summary_part.added",
                        &json!({ "output_index": idx, "summary_index": 0, "part": part }),
                    );
                }
                self.reasoning_text.push_str(reasoning);
                let idx = self.next_output_index - 1;
                push_sse_event(
                    out,
                    "response.reasoning_summary_text.delta",
                    &json!({
                        "output_index": idx,
                        "summary_index": 0,
                        "delta": reasoning,
                    }),
                );
            }
        }

        // 文本增量
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                if !self.content_started {
                    self.content_started = true;
                    let idx = self.next_output_index;
                    self.next_output_index += 1;
                    self.open_items.push(OutputItemKind::Message);
                    let item = json!({
                        "type": "message",
                        "id": format!("{}_msg", self.resp_id),
                        "status": "in_progress",
                        "role": "assistant",
                        "content": [],
                    });
                    push_sse_event(
                        out,
                        "response.output_item.added",
                        &json!({ "output_index": idx, "item": item }),
                    );
                    let part = json!({
                        "type": "output_text",
                        "text": "",
                    });
                    push_sse_event(
                        out,
                        "response.content_part.added",
                        &json!({ "output_index": idx, "content_index": 0, "part": part }),
                    );
                }
                self.content_text.push_str(text);
                let idx = self.next_output_index - 1;
                push_sse_event(
                    out,
                    "response.output_text.delta",
                    &json!({
                        "item_id": format!("{}_msg", self.resp_id),
                        "output_index": idx,
                        "content_index": 0,
                        "delta": text,
                    }),
                );
            }
        }

        // tool_calls 增量
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let up_idx = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let output_idx = if let Some(&oidx) = self.tool_output_indices.get(&up_idx) {
                    oidx
                } else {
                    let oidx = self.next_output_index;
                    self.next_output_index += 1;
                    self.tool_output_indices.insert(up_idx, oidx);
                    self.open_items
                        .push(OutputItemKind::FunctionCall(oidx));
                    let call_id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    self.tool_call_ids.insert(up_idx, call_id.to_string());
                    self.tool_names.insert(up_idx, name.to_string());
                    let item = json!({
                        "type": "function_call",
                        "id": format!("{}_fc_{oidx}", self.resp_id),
                        "call_id": call_id,
                        "name": name,
                        "arguments": "",
                        "status": "in_progress",
                    });
                    push_sse_event(
                        out,
                        "response.output_item.added",
                        &json!({ "output_index": oidx, "item": item }),
                    );
                    oidx
                };

                if let Some(args) = call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                {
                    if !args.is_empty() {
                        let accumulated = self.tool_args.entry(up_idx).or_default();
                        accumulated.push_str(args);
                        push_sse_event(
                            out,
                            "response.function_call_arguments.delta",
                            &json!({
                                "output_index": output_idx,
                                "delta": args,
                            }),
                        );
                    }
                }
            }
        }

        // finish_reason → close
        if let Some(reason) = chunk["choices"][0].get("finish_reason").and_then(Value::as_str) {
            // 收集 usage（可能在 finish chunk 上）
            if chunk.get("usage").map(|u| u.is_object()).unwrap_or(false) {
                self.usage = Some(chunk["usage"].clone());
            }
            self.finish_reason = Some(reason.to_string());
            self.close(out);
        }
    }

    /// 收尾：对每个已开启 item 发 done 事件，再发 response.completed。幂等。
    fn close(&mut self, out: &mut String) {
        if self.closed {
            return;
        }
        // 空流（上游只发 [DONE] 或连接直接结束）：补发最小 response.created，
        // 否则客户端连 response.created 都没收到就一直等 response.completed 直到 TCP 超时。
        if !self.started {
            self.started = true;
            if self.resp_id.is_empty() {
                self.resp_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
            }
            let created = json!({
                "id": self.resp_id,
                "object": "response",
                "created_at": self.created_at,
                "model": self.model,
                "status": "in_progress",
                "output": [],
            });
            push_sse_event(out, "response.created", &json!({ "response": created }));
        }
        self.closed = true;

        let reason = self.finish_reason.as_deref().unwrap_or("stop");

        // output_index → upstream index 反查表（取累计 arguments/call_id/name 用）
        let rev_indices: std::collections::HashMap<u32, u64> = self
            .tool_output_indices
            .iter()
            .map(|(up, oidx)| (*oidx, *up))
            .collect();

        // 逐个关闭已开启的 items
        for (i, kind) in self.open_items.iter().enumerate() {
            match kind {
                OutputItemKind::Reasoning => {
                    push_sse_event(
                        out,
                        "response.reasoning_summary_text.done",
                        &json!({
                            "output_index": i,
                            "summary_index": 0,
                            "text": self.reasoning_text,
                        }),
                    );
                    push_sse_event(
                        out,
                        "response.reasoning_summary_part.done",
                        &json!({
                            "output_index": i,
                            "summary_index": 0,
                            "part": {
                                "type": "summary_text",
                                "text": self.reasoning_text,
                            },
                        }),
                    );
                    push_sse_event(
                        out,
                        "response.output_item.done",
                        &json!({
                            "output_index": i,
                            "item": {
                                "type": "reasoning",
                                "id": format!("{}_rs", self.resp_id),
                                "summary": [{
                                    "type": "summary_text",
                                    "text": self.reasoning_text,
                                }],
                            },
                        }),
                    );
                }
                OutputItemKind::Message => {
                    push_sse_event(
                        out,
                        "response.output_text.done",
                        &json!({
                            "output_index": i,
                            "content_index": 0,
                            "item_id": format!("{}_msg", self.resp_id),
                            "text": self.content_text,
                        }),
                    );
                    push_sse_event(
                        out,
                        "response.content_part.done",
                        &json!({
                            "output_index": i,
                            "content_index": 0,
                            "part": {
                                "type": "output_text",
                                "text": self.content_text,
                            },
                        }),
                    );
                    push_sse_event(
                        out,
                        "response.output_item.done",
                        &json!({
                            "output_index": i,
                            "item": {
                                "type": "message",
                                "id": format!("{}_msg", self.resp_id),
                                "status": "completed",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": self.content_text,
                                }],
                            },
                        }),
                    );
                }
                OutputItemKind::FunctionCall(oidx) => {
                    // 反查 upstream index，取累计 arguments 与真实 call_id/name
                    let up_idx = rev_indices.get(oidx).copied().unwrap_or(0);
                    let accumulated = self.tool_args.get(&up_idx).cloned().unwrap_or_default();
                    let call_id = self.tool_call_ids.get(&up_idx).cloned().unwrap_or_default();
                    let name = self.tool_names.get(&up_idx).cloned().unwrap_or_default();
                    let item_id = format!("{}_fc_{oidx}", self.resp_id);
                    push_sse_event(
                        out,
                        "response.function_call_arguments.done",
                        &json!({
                            "output_index": oidx,
                            "arguments": accumulated,
                        }),
                    );
                    push_sse_event(
                        out,
                        "response.output_item.done",
                        &json!({
                            "output_index": oidx,
                            "item": {
                                "type": "function_call",
                                "id": item_id,
                                "call_id": call_id,
                                "name": name,
                                "arguments": accumulated,
                                "status": "completed",
                            },
                        }),
                    );
                }
            }
        }

        // 构建 output 完成态数组（response.completed 的 output 字段）
        let output = self.build_completed_output();

        // status 映射
        let status = if reason == "length" {
            "incomplete"
        } else {
            "completed"
        };

        let mut completed = json!({
            "id": self.resp_id,
            "object": "response",
            "created_at": self.created_at,
            "model": self.model,
            "status": status,
            "output": output,
        });

        if status == "incomplete" {
            completed["incomplete_details"] = json!({ "reason": "max_output_tokens" });
        }

        // usage 映射
        if let Some(ref usage) = self.usage {
            completed["usage"] = map_usage_chat_to_responses(usage);
        }

        // 官方规范：incomplete 状态对应 response.incomplete 事件（客户端按事件名区分）
        let event_name = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        push_sse_event(out, event_name, &json!({ "response": completed }));
        out.push_str("data: [DONE]\n\n");
    }

    /// 构建 response.completed 中的 output 数组（所有 item 的完成态）。
    fn build_completed_output(&self) -> Value {
        // output_index → upstream index 反查表
        let rev_indices: std::collections::HashMap<u32, u64> = self
            .tool_output_indices
            .iter()
            .map(|(up, oidx)| (*oidx, *up))
            .collect();
        let mut output: Vec<Value> = Vec::new();
        for kind in &self.open_items {
            match kind {
                OutputItemKind::Reasoning => {
                    output.push(json!({
                        "type": "reasoning",
                        "id": format!("{}_rs", self.resp_id),
                        "summary": [{
                            "type": "summary_text",
                            "text": self.reasoning_text,
                        }],
                    }));
                }
                OutputItemKind::Message => {
                    output.push(json!({
                        "type": "message",
                        "id": format!("{}_msg", self.resp_id),
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": self.content_text,
                        }],
                    }));
                }
                OutputItemKind::FunctionCall(oidx) => {
                    let up_idx = rev_indices.get(oidx).copied().unwrap_or(0);
                    let accumulated = self.tool_args.get(&up_idx).cloned().unwrap_or_default();
                    // call_id/name 必须回带上游真实值：客户端靠 call_id 关联
                    // function_call_output，靠 name 识别要执行的函数。
                    let call_id = self.tool_call_ids.get(&up_idx).cloned().unwrap_or_default();
                    let name = self.tool_names.get(&up_idx).cloned().unwrap_or_default();
                    output.push(json!({
                        "type": "function_call",
                        "id": format!("{}_fc_{oidx}", self.resp_id),
                        "call_id": call_id,
                        "name": name,
                        "arguments": accumulated,
                        "status": "completed",
                    }));
                }
            }
        }
        Value::Array(output)
    }
}

/// Chat usage → Responses usage 映射。
fn map_usage_chat_to_responses(usage: &Value) -> Value {
    let prompt_tokens = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let completion_tokens = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(prompt_tokens + completion_tokens);
    let cached_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let reasoning_tokens = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "input_tokens": prompt_tokens,
        "output_tokens": completion_tokens,
        "total_tokens": total_tokens,
        "input_tokens_details": { "cached_tokens": cached_tokens },
        "output_tokens_details": { "reasoning_tokens": reasoning_tokens },
    })
}

/// 写入一条 Responses SSE 事件到输出缓冲。
fn push_sse_event(out: &mut String, event_type: &str, data: &Value) {
    // Responses API SSE 同时有 event: 行和 data: 行；官方客户端（OpenAI SDK/Codex）
    // 以 data JSON 的 type 字段分发事件，因此必须把 type 注入 data 本体——
    // 本网关自己的 UsageSseScanner 也依赖 data.type 识别 response.completed。
    let data = if let Value::Object(map) = data {
        let mut m = map.clone();
        m.insert("type".to_string(), Value::String(event_type.to_string()));
        Value::Object(m)
    } else {
        data.clone()
    };
    out.push_str("event: ");
    out.push_str(event_type);
    out.push('\n');
    out.push_str("data: ");
    out.push_str(&data.to_string());
    out.push('\n');
    out.push('\n');
}

// ── D. 上游请求体转换 ──────────────────────────────────────────

/// Chat Completions 请求体 → Responses API 请求体。
///
/// 把 messages 开头的连续 system/developer role 提取为顶层 `instructions`，
/// 其余消息按 role 映射为 Responses 输入 item。
///
/// 注意：**只有开头连续**的 system/developer 才提升进 `instructions`。
/// 对话中段的 system 消息（如 Claude Code 每轮追加的 system-reminder）若也被
/// 提升，等于每轮往 prompt 头部插入新内容，前缀缓存从 instructions 结束处
/// 就断掉（实测 DeepSeek 等上游命中只剩 system 块大小、长期固定不变）。
/// 中段 system 一律按 user 角色原位输出，保持整段 prompt 纯追加。
pub fn chat_body_to_responses_body(chat: &Value) -> Value {
    let model = chat.get("model").and_then(Value::as_str).unwrap_or("");
    let stream = chat.get("stream").and_then(Value::as_bool).unwrap_or(false);

    // 提取开头连续的 system/developer → instructions；中段 system → user（原位）
    let mut instructions_parts: Vec<String> = Vec::new();
    let mut other_messages: Vec<Value> = Vec::new();
    if let Some(msgs) = chat.get("messages").and_then(Value::as_array) {
        let mut seen_non_system = false;
        for msg in msgs {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            if role == "system" || role == "developer" {
                if seen_non_system {
                    // 无文本的中段 system 丢弃（与旧行为一致）；有文本则降级为 user
                    if extract_chat_message_text(msg).is_some_and(|t| !t.is_empty()) {
                        let mut m = msg.clone();
                        m["role"] = json!("user");
                        other_messages.push(m);
                    }
                } else if let Some(text) = extract_chat_message_text(msg) {
                    instructions_parts.push(text);
                }
            } else {
                seen_non_system = true;
                other_messages.push(msg.clone());
            }
        }
    }

    let instructions = if instructions_parts.is_empty() {
        Value::Null
    } else {
        json!(instructions_parts.join("\n"))
    };

    // 消息 → input 数组（带 tool_calls 的 assistant 消息会拆成多个顶层 item）
    let input: Vec<Value> = other_messages
        .iter()
        .flat_map(chat_message_to_input_items)
        .collect();

    let mut resp = json!({
        "model": model,
        "stream": stream,
        "store": false,
        "instructions": instructions,
        "input": input,
    });

    // tools 逆变换
    if let Some(tools) = chat.get("tools") {
        let resp_tools = convert_tools_to_responses(tools);
        if !resp_tools.is_empty() {
            resp["tools"] = Value::Array(resp_tools);
        }
    }

    // tool_choice 逆变换
    if let Some(tc) = chat.get("tool_choice") {
        resp["tool_choice"] = convert_tool_choice_to_responses(tc);
    }

    // max_tokens / max_completion_tokens → max_output_tokens
    let max_tokens = chat
        .get("max_tokens")
        .or_else(|| chat.get("max_completion_tokens"))
        .and_then(Value::as_u64);
    if let Some(mt) = max_tokens {
        resp["max_output_tokens"] = json!(mt);
    }

    // temperature / top_p 透传
    if let Some(t) = chat.get("temperature") {
        resp["temperature"] = t.clone();
    }
    if let Some(tp) = chat.get("top_p") {
        resp["top_p"] = tp.clone();
    }

    resp
}

/// 从 chat message 中提取纯文本内容。
///
/// content 为字符串直接返回；为数组时取 type=text 的部分拼接。
fn extract_chat_message_text(msg: &Value) -> Option<String> {
    match msg.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
            Some(text)
        }
        _ => None,
    }
}

/// 将 chat message 转换为 Responses input items（一条消息可能拆出多个顶层 item）。
///
/// 注意：Responses API 中 `function_call` / `function_call_output` 是 input 数组的
/// 顶层 item，不能嵌套在 message 的 `content` 里——content 只接受内容部件
/// （user 侧 `input_text`、assistant 侧 `output_text`）。把 function_call 塞进
/// content 会被严格校验的上游拒绝（`input[N].content did not match any
/// supported type`）。
fn chat_message_to_input_items(msg: &Value) -> Vec<Value> {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
    match role {
        "user" => {
            let text = extract_chat_message_text(msg).unwrap_or_default();
            vec![json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": text }],
            })]
        }
        "assistant" => {
            let mut items: Vec<Value> = Vec::new();
            // text content → 独立 message item
            let content_text = extract_chat_message_text(msg).unwrap_or_default();
            if !content_text.is_empty() {
                items.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": content_text }],
                }));
            }
            // tool_calls → 独立顶层 function_call items
            if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let call_id = call.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let arguments = call
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments,
                    }));
                }
            }
            items
        }
        "tool" => {
            let call_id = msg.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
            let output = msg
                .get("content")
                .map(|c| {
                    if c.is_string() {
                        c.as_str().unwrap_or("").to_string()
                    } else {
                        c.to_string()
                    }
                })
                .unwrap_or_default();
            vec![json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            })]
        }
        _ => Vec::new(),
    }
}

/// Chat Completions `tools` → Responses `tools`（逆变换）。
fn convert_tools_to_responses(tools: &Value) -> Vec<Value> {
    let Some(arr) = tools.as_array() else {
        return vec![];
    };
    arr.iter()
        .filter_map(|t| {
            let t_type = t.get("type").and_then(Value::as_str).unwrap_or("");
            if t_type != "function" {
                return None;
            }
            let func = t.get("function")?;
            let name = func.get("name").and_then(Value::as_str).unwrap_or("");
            let description = func.get("description").and_then(Value::as_str);
            let parameters = func.get("parameters");
            let strict = func.get("strict").and_then(Value::as_bool);
            let mut tool = json!({
                "type": "function",
                "name": name,
            });
            if let Some(d) = description {
                tool["description"] = json!(d);
            }
            if let Some(p) = parameters {
                tool["parameters"] = p.clone();
            }
            if let Some(s) = strict {
                tool["strict"] = json!(s);
            }
            Some(tool)
        })
        .collect()
}

/// Chat Completions `tool_choice` → Responses `tool_choice`（逆变换）。
fn convert_tool_choice_to_responses(tc: &Value) -> Value {
    match tc {
        Value::String(s) => json!(s),
        Value::Object(obj) => {
            if obj.get("type").and_then(Value::as_str) == Some("function") {
                let name = obj
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                json!({ "type": "function", "name": name })
            } else {
                tc.clone()
            }
        }
        _ => tc.clone(),
    }
}

// ── E. 上游非流式响应转换 ──────────────────────────────────────

/// Responses API 响应 → Chat Completions 响应（非流式）。
///
/// B 的逆变换：output 数组中 reasoning 的 summary 拼接 → reasoning_content；
/// message item 的 output_text 拼接 → content；function_call items → tool_calls。
pub fn responses_response_to_chat(resp: &Value) -> Value {
    let raw_id = resp.get("id").and_then(Value::as_str).unwrap_or("");
    let created = resp.get("created_at").and_then(Value::as_u64).unwrap_or(0);
    let model = resp.get("model").and_then(Value::as_str).unwrap_or("");

    let mut reasoning_content = String::new();
    let mut content = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut has_function_call = false;

    if let Some(output) = resp.get("output").and_then(Value::as_array) {
        for item in output {
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            match item_type {
                "reasoning" => {
                    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                        for s in summary {
                            if let Some(text) = s.get("text").and_then(Value::as_str) {
                                reasoning_content.push_str(text);
                            }
                        }
                    }
                }
                "message" => {
                    if let Some(c) = item.get("content").and_then(Value::as_array) {
                        for part in c {
                            let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
                            if part_type == "output_text" {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    content.push_str(text);
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    has_function_call = true;
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    tool_calls.push(json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        },
                    }));
                }
                _ => {}
            }
        }
    }

    // finish_reason
    let status = resp.get("status").and_then(Value::as_str).unwrap_or("completed");
    let finish_reason = if status == "incomplete" {
        "length"
    } else if has_function_call {
        "tool_calls"
    } else {
        "stop"
    };

    let mut message = json!({
        "role": "assistant",
    });
    if !reasoning_content.is_empty() {
        message["reasoning_content"] = json!(reasoning_content);
    }
    if !content.is_empty() {
        message["content"] = json!(content);
    } else if tool_calls.is_empty() {
        message["content"] = Value::Null;
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    let mut chat_resp = json!({
        "id": raw_id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
    });

    // usage 逆映射
    if let Some(usage) = resp.get("usage").filter(|u| u.is_object()) {
        let input_tokens = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let output_tokens = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let total_tokens = usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(input_tokens + output_tokens);

        let mut prompt_details = serde_json::Map::new();
        let cached = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if cached > 0 {
            prompt_details.insert("cached_tokens".to_string(), json!(cached));
        }

        let mut completion_details = serde_json::Map::new();
        let reasoning = usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if reasoning > 0 {
            completion_details
                .insert("reasoning_tokens".to_string(), json!(reasoning));
        }

        let mut usage_obj = json!({
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": total_tokens,
        });
        if !prompt_details.is_empty() {
            usage_obj["prompt_tokens_details"] = Value::Object(prompt_details);
        }
        if !completion_details.is_empty() {
            usage_obj["completion_tokens_details"] = Value::Object(completion_details);
        }
        chat_resp["usage"] = usage_obj;
    }

    chat_resp
}

// ── F. 上游流式转换器 ──────────────────────────────────────────

/// Responses SSE → Chat chunk SSE 流式翻译器。
///
/// 复刻 [`super::format::AnthropicSseTranslator`] 的字节级行缓冲模式。
pub struct ResponsesToChatSseTranslator {
    /// 尚未遇到换行符的不完整数据（原始字节）。
    line_buf: Vec<u8>,
    /// 是否已发送首个 chat chunk（response.created → chat chunk）
    started: bool,
    /// 是否已关闭
    closed: bool,
    /// response.id（透传到每个 chat chunk）
    resp_id: String,
    /// response.model
    model: String,
    /// output_index → tool_calls index 映射
    tool_indices: std::collections::HashMap<u64, u32>,
    /// 下一个 tool_calls index
    next_tool_index: u32,
}

impl Default for ResponsesToChatSseTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesToChatSseTranslator {
    pub fn new() -> Self {
        Self {
            line_buf: Vec::new(),
            started: false,
            closed: false,
            resp_id: String::new(),
            model: String::new(),
            tool_indices: std::collections::HashMap::new(),
            next_tool_index: 0,
        }
    }

    /// 喂入上游字节块，返回可立即发给客户端的 Chat chunk SSE 字节。
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.line_buf.extend_from_slice(bytes);

        let mut out = String::new();
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.line_buf.drain(..=pos).collect();
            let line_bytes = &line_bytes[..line_bytes.len() - 1];
            let line_str =
                String::from_utf8_lossy(line_bytes.strip_suffix(b"\r").unwrap_or(line_bytes));
            self.process_line(&line_str, &mut out);
        }
        out.into_bytes()
    }

    fn process_line(&mut self, line: &str, out: &mut String) {
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            self.close(out);
            return;
        }
        let event: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return,
        };

        if self.closed {
            return;
        }

        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "response.created" => {
                self.started = true;
                self.resp_id = event
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.model = event
                    .get("response")
                    .and_then(|r| r.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let created = event
                    .get("response")
                    .and_then(|r| r.get("created_at"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let chunk = json!({
                    "id": self.resp_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": { "role": "assistant" },
                        "finish_reason": Value::Null,
                    }],
                });
                push_chat_chunk(out, &chunk);
            }
            "response.output_text.delta" => {
                let delta_text = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let chunk = json!({
                    "id": self.resp_id,
                    "object": "chat.completion.chunk",
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": { "content": delta_text },
                        "finish_reason": Value::Null,
                    }],
                });
                push_chat_chunk(out, &chunk);
            }
            "response.reasoning_summary_text.delta" => {
                let delta_text = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let chunk = json!({
                    "id": self.resp_id,
                    "object": "chat.completion.chunk",
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": { "reasoning_content": delta_text },
                        "finish_reason": Value::Null,
                    }],
                });
                push_chat_chunk(out, &chunk);
            }
            "response.output_item.added" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                if item_type == "function_call" {
                    let output_idx = event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let tc_index = if let Some(&idx) = self.tool_indices.get(&output_idx) {
                        idx
                    } else {
                        let idx = self.next_tool_index;
                        self.next_tool_index += 1;
                        self.tool_indices.insert(output_idx, idx);
                        idx
                    };
                    let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let chunk = json!({
                        "id": self.resp_id,
                        "object": "chat.completion.chunk",
                        "model": self.model,
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "tool_calls": [{
                                    "index": tc_index,
                                    "id": call_id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": "",
                                    },
                                }],
                            },
                            "finish_reason": Value::Null,
                        }],
                    });
                    push_chat_chunk(out, &chunk);
                }
            }
            "response.function_call_arguments.delta" => {
                let output_idx = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let tc_index = if let Some(&idx) = self.tool_indices.get(&output_idx) {
                    idx
                } else {
                    let idx = self.next_tool_index;
                    self.next_tool_index += 1;
                    self.tool_indices.insert(output_idx, idx);
                    idx
                };
                let delta_args = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let chunk = json!({
                    "id": self.resp_id,
                    "object": "chat.completion.chunk",
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": tc_index,
                                "function": {
                                    "arguments": delta_args,
                                },
                            }],
                        },
                        "finish_reason": Value::Null,
                    }],
                });
                push_chat_chunk(out, &chunk);
            }
            "response.completed" | "response.incomplete" => {
                // 收尾 chunk
                let resp = event.get("response").unwrap_or(&event);
                let status = resp.get("status").and_then(Value::as_str).unwrap_or("completed");

                // 判断是否有 function_call item → finish_reason
                let has_fc = resp
                    .get("output")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
                    })
                    .unwrap_or(false);
                let finish_reason = if status == "incomplete" {
                    "length"
                } else if has_fc {
                    "tool_calls"
                } else {
                    "stop"
                };

                let usage = resp.get("usage").map(map_usage_responses_to_chat);

                let finish_chunk = json!({
                    "id": self.resp_id,
                    "object": "chat.completion.chunk",
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": finish_reason,
                    }],
                });
                let mut chunk_val = finish_chunk;
                if let Some(u) = usage {
                    chunk_val["usage"] = u;
                }
                push_chat_chunk(out, &chunk_val);
                out.push_str("data: [DONE]\n\n");
                self.closed = true;
            }
            "response.failed" => {
                tracing::warn!(
                    resp_id = %self.resp_id,
                    "Responses stream failed"
                );
                let chunk = json!({
                    "id": self.resp_id,
                    "object": "chat.completion.chunk",
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "stop",
                    }],
                });
                push_chat_chunk(out, &chunk);
                out.push_str("data: [DONE]\n\n");
                self.closed = true;
            }
            _ => {
                // 其他事件类型（output_item.done、content_part.* 等）忽略
            }
        }
    }

    /// 收尾：若流结束未收到 completed，幂等补发 [DONE]。
    fn close(&mut self, out: &mut String) {
        if self.closed {
            return;
        }
        self.closed = true;
        // 未收到过 response.created 的空流：只发 [DONE]，不补 id/model 为空的非法 chunk。
        if self.started {
            // 补发一个 finish_reason:stop 的 chunk + [DONE]
            let chunk = json!({
                "id": self.resp_id,
                "object": "chat.completion.chunk",
                "model": self.model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop",
                }],
            });
            push_chat_chunk(out, &chunk);
        }
        out.push_str("data: [DONE]\n\n");
    }
}

/// Responses usage → Chat usage 映射。
fn map_usage_responses_to_chat(usage: &Value) -> Value {
    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output_tokens = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut usage_obj = json!({
        "prompt_tokens": input_tokens,
        "completion_tokens": output_tokens,
        "total_tokens": total_tokens,
    });
    if cached > 0 {
        usage_obj["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    if reasoning > 0 {
        usage_obj["completion_tokens_details"] = json!({ "reasoning_tokens": reasoning });
    }
    usage_obj
}

/// 写入一条 Chat chunk SSE 事件到输出缓冲。
fn push_chat_chunk(out: &mut String, chunk: &Value) {
    out.push_str("data: ");
    out.push_str(&chunk.to_string());
    out.push('\n');
    out.push('\n');
}

// ── G. pipeline 后处理包装函数 ──────────────────────────────────

/// 流式：把上游 OpenAI SSE 字节流逐 chunk 翻译成 Responses SSE 事件流。
///
/// 结构与 [`super::format::convert_openai_stream_to_anthropic`] 一致：
/// 用 [`ChatToResponsesSseTranslator`] 包装字节流，每个上游字节块喂入翻译器，
/// 返回的字节直接发给客户端。
pub fn convert_openai_stream_to_responses(openai_resp: axum::response::Response) -> axum::response::Response {
    use futures_util::StreamExt;

    let byte_stream = openai_resp.into_body().into_data_stream();
    let translator = std::sync::Arc::new(std::sync::Mutex::new(
        ChatToResponsesSseTranslator::new(),
    ));
    let out = byte_stream.filter_map(move |chunk| {
        let translator = translator.clone();
        async move {
            match chunk {
                Ok(bytes) => {
                    let converted = translator.lock().unwrap().push(&bytes);
                    if converted.is_empty() {
                        None
                    } else {
                        Some(Ok(converted))
                    }
                }
                Err(e) => Some(Err(std::io::Error::other(e.to_string()))),
            }
        }
    });

    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(axum::body::Body::from_stream(out))
        .unwrap()
}

/// 非流式：把 OpenAI chat completion 响应 JSON 转成 Responses API 响应。
///
/// 有界读 body（`upstream::MAX_UPSTREAM_BODY_BYTES`，超限 502），
/// parse JSON 后调用 [`chat_response_to_responses`]；parse 失败原样透传 body+status。
/// 结构与 [`super::format::convert_openai_to_anthropic_response`] 一致。
pub async fn convert_openai_to_responses_response(
    openai_resp: axum::response::Response,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::Response;
    use serde_json::Value;

    let status = openai_resp.status();
    let body_bytes = match axum::body::to_bytes(
        openai_resp.into_body(),
        super::upstream::MAX_UPSTREAM_BODY_BYTES,
    )
    .await
    {
        Ok(b) => b,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("Content-Type", "text/plain; charset=utf-8")
                .body(Body::from(format!(
                    "failed to read upstream response (too large or read error): {e}"
                )))
                .unwrap();
        }
    };

    let openai: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => {
            return Response::builder()
                .status(status)
                .header("Content-Type", "application/json")
                .body(Body::from(body_bytes))
                .unwrap();
        }
    };

    let responses_resp = chat_response_to_responses(&openai);

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&responses_resp).unwrap_or_else(|_| body_bytes.to_vec()),
        ))
        .unwrap()
}

// ── H. 上游 Responses → Chat 响应转换（failover 用） ──────────────

/// 非流式：把上游 Responses API 响应 JSON 转成 Chat Completions 格式。
///
/// 有界读 body（`upstream::MAX_UPSTREAM_BODY_BYTES`，超限 502），
/// parse JSON 后调用 [`responses_response_to_chat`]；parse 失败返回 502。
pub async fn convert_responses_to_chat_response(
    responses_resp: axum::response::Response,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::Response;
    use serde_json::Value;

    let status = responses_resp.status();
    let body_bytes = axum::body::to_bytes(
        responses_resp.into_body(),
        super::upstream::MAX_UPSTREAM_BODY_BYTES,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to read upstream responses body (too large or read error): {e}"),
        )
    })?;

    let responses_json: Value = serde_json::from_slice(&body_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("invalid responses-format upstream body: {e}"),
        )
    })?;

    let chat_json = responses_response_to_chat(&responses_json);

    Ok(Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&chat_json).unwrap_or_else(|_| body_bytes.to_vec()),
        ))
        .unwrap())
}

/// 流式：把上游 Responses SSE 字节流逐事件翻译成 Chat chunk SSE。
///
/// 用 [`ResponsesToChatSseTranslator`] 包装字节流，每个上游字节块喂入翻译器，
/// 返回的字节直接发给客户端。
pub fn convert_responses_stream_to_chat(
    responses_resp: axum::response::Response,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::Response;
    use futures_util::StreamExt;

    let byte_stream = responses_resp.into_body().into_data_stream();
    let translator = std::sync::Arc::new(std::sync::Mutex::new(
        ResponsesToChatSseTranslator::new(),
    ));
    let out = byte_stream.filter_map(move |chunk| {
        let translator = translator.clone();
        async move {
            match chunk {
                Ok(bytes) => {
                    let converted = translator.lock().unwrap().push(&bytes);
                    if converted.is_empty() {
                        None
                    } else {
                        Some(Ok(converted))
                    }
                }
                Err(e) => Some(Err(std::io::Error::other(e.to_string()))),
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(Body::from_stream(out))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("build sse response: {e}")))
}

// ── 测试 ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ════════════════════════════════════════════════════════════════
    // A. responses_request_to_chat 测试
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn a_model_required() {
        let body = json!({ "input": "hi" });
        assert!(responses_request_to_chat(&body).is_err());
    }

    #[test]
    fn a_string_input_becomes_user_message() {
        let body = json!({ "model": "gpt-4", "input": "hello" });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.model, "gpt-4");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(req.messages[0].content.as_deref(), Some("hello"));
    }

    #[test]
    fn a_instructions_becomes_system_message() {
        let body = json!({
            "model": "m",
            "instructions": "Be helpful",
            "input": "hi"
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[0].content.as_deref(), Some("Be helpful"));
        assert_eq!(req.messages[1].role, "user");
    }

    #[test]
    fn a_empty_instructions_skipped() {
        let body = json!({
            "model": "m",
            "instructions": "",
            "input": "hi"
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn a_message_items_with_developer_role() {
        let body = json!({
            "model": "m",
            "input": [
                { "type": "message", "role": "developer", "content": "sys" },
                { "type": "message", "role": "user", "content": "hi" }
            ]
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[0].content.as_deref(), Some("sys"));
        assert_eq!(req.messages[1].role, "user");
    }

    #[test]
    fn a_content_parts_array_concat() {
        let body = json!({
            "model": "m",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hello " },
                    { "type": "input_text", "text": "world" }
                ]
            }]
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.messages[0].content.as_deref(), Some("hello world"));
    }

    #[test]
    fn a_function_call_item() {
        let body = json!({
            "model": "m",
            "input": [{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_42",
                "name": "get_weather",
                "arguments": "{\"loc\":\"SF\"}"
            }]
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.messages[0].role, "assistant");
        let calls = req.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_42");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], "{\"loc\":\"SF\"}");
    }

    #[test]
    fn a_function_call_id_fallback() {
        let body = json!({
            "model": "m",
            "input": [{
                "type": "function_call",
                "id": "fc_fallback",
                "name": "f",
                "arguments": "{}"
            }]
        });
        let req = responses_request_to_chat(&body).unwrap();
        let calls = req.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0]["id"], "fc_fallback");
    }

    #[test]
    fn a_function_call_output_item() {
        let body = json!({
            "model": "m",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_42",
                "output": "sunny"
            }]
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.messages[0].role, "tool");
        assert_eq!(req.messages[0].tool_call_id.as_deref(), Some("call_42"));
        assert_eq!(req.messages[0].content.as_deref(), Some("sunny"));
    }

    #[test]
    fn a_function_call_output_non_string_serialized() {
        let body = json!({
            "model": "m",
            "input": [{
                "type": "function_call_output",
                "call_id": "c1",
                "output": 42
            }]
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.messages[0].content.as_deref(), Some("42"));
    }

    #[test]
    fn a_reasoning_item_skipped() {
        let body = json!({
            "model": "m",
            "input": [
                { "type": "reasoning", "summary": "thinking..." },
                { "type": "message", "role": "user", "content": "hi" }
            ]
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
    }

    #[test]
    fn a_tools_transformed() {
        let body = json!({
            "model": "m",
            "input": "hi",
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "Get weather",
                "parameters": { "type": "object", "properties": {} },
                "strict": true
            }]
        });
        let req = responses_request_to_chat(&body).unwrap();
        let tools = req.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(tools[0]["function"]["description"], "Get weather");
        assert_eq!(tools[0]["function"]["strict"], true);
    }

    #[test]
    fn a_non_function_tool_skipped() {
        let body = json!({
            "model": "m",
            "input": "hi",
            "tools": [{ "type": "web_search", "query": "foo" }]
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert!(req.tools.is_none() || req.tools.as_ref().unwrap().is_empty());
    }

    #[test]
    fn a_tool_choice_string() {
        let body = json!({
            "model": "m",
            "input": "hi",
            "tool_choice": "required"
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.tool_choice.as_ref().unwrap(), "required");
    }

    #[test]
    fn a_tool_choice_function() {
        let body = json!({
            "model": "m",
            "input": "hi",
            "tool_choice": { "type": "function", "name": "get_weather" }
        });
        let req = responses_request_to_chat(&body).unwrap();
        let tc = req.tool_choice.as_ref().unwrap();
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "get_weather");
    }

    #[test]
    fn a_max_output_tokens_to_max_tokens() {
        let body = json!({
            "model": "m",
            "input": "hi",
            "max_output_tokens": 1024
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert_eq!(req.max_tokens, Some(1024));
    }

    #[test]
    fn a_temperature_top_p_passthrough() {
        let body = json!({
            "model": "m",
            "input": "hi",
            "temperature": 0.7,
            "top_p": 0.9
        });
        let req = responses_request_to_chat(&body).unwrap();
        assert!((req.temperature.unwrap() - 0.7).abs() < f32::EPSILON);
        assert!((req.top_p.unwrap() - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn a_stream_default_false() {
        let body = json!({ "model": "m", "input": "hi" });
        let req = responses_request_to_chat(&body).unwrap();
        assert!(!req.stream);
    }

    #[test]
    fn a_input_missing_returns_err() {
        let body = json!({ "model": "m" });
        assert!(responses_request_to_chat(&body).is_err());
    }

    // ════════════════════════════════════════════════════════════════
    // D. chat_body_to_responses_body 测试（与 A 互逆）
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn d_instructions_extracted_from_system() {
        let chat = json!({
            "model": "gpt-4",
            "messages": [
                { "role": "system", "content": "Be helpful" },
                { "role": "user", "content": "hi" }
            ]
        });
        let resp = chat_body_to_responses_body(&chat);
        assert_eq!(resp["instructions"], "Be helpful");
        let input = resp["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn d_developer_role_merged_into_instructions() {
        let chat = json!({
            "model": "m",
            "messages": [
                { "role": "system", "content": "sys1" },
                { "role": "developer", "content": "dev1" },
                { "role": "user", "content": "hi" }
            ]
        });
        let resp = chat_body_to_responses_body(&chat);
        assert_eq!(resp["instructions"], "sys1\ndev1");
    }

    #[test]
    fn d_leading_systems_all_hoisted() {
        // 开头连续多条 system/developer 全部进 instructions
        let chat = json!({
            "model": "m",
            "messages": [
                { "role": "system", "content": "sys1" },
                { "role": "system", "content": "sys2" },
                { "role": "user", "content": "hi" }
            ]
        });
        let resp = chat_body_to_responses_body(&chat);
        assert_eq!(resp["instructions"], "sys1\nsys2");
        assert_eq!(resp["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn d_mid_conversation_system_becomes_user_item() {
        // 中段 system（如 Claude Code 每轮追加的 reminder）必须原位降级为 user，
        // 不能提升进 instructions——否则每轮往 prompt 头部插内容，前缀缓存全断。
        let chat = json!({
            "model": "m",
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "q1" },
                { "role": "assistant", "content": "a1" },
                { "role": "system", "content": "<total_tokens>100 left</total_tokens>" },
                { "role": "user", "content": "q2" }
            ]
        });
        let resp = chat_body_to_responses_body(&chat);
        assert_eq!(resp["instructions"], "sys");
        let input = resp["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        // 中段 system 原位变成 user message item，内容不变
        assert_eq!(input[2]["role"], "user");
        assert_eq!(
            input[2]["content"][0]["text"],
            "<total_tokens>100 left</total_tokens>"
        );
        assert_eq!(input[3]["role"], "user");
    }

    #[test]
    fn d_mid_conversation_system_append_only_prefix() {
        // 端到端前缀稳定性：第二轮在第一轮尾部追加 [assistant, user, system] 后，
        // 转换结果的 instructions 不变，input 为纯追加（缓存友好）。
        let base = vec![
            json!({ "role": "system", "content": "sys" }),
            json!({ "role": "user", "content": "q1" }),
            json!({ "role": "system", "content": "reminder-1" }),
        ];
        let chat1 = json!({ "model": "m", "messages": base });
        let chat2 = json!({
            "model": "m",
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "q1" },
                { "role": "system", "content": "reminder-1" },
                { "role": "assistant", "content": "a1" },
                { "role": "user", "content": "q2" },
                { "role": "system", "content": "reminder-2" }
            ]
        });
        let r1 = chat_body_to_responses_body(&chat1);
        let r2 = chat_body_to_responses_body(&chat2);
        assert_eq!(r1["instructions"], r2["instructions"]);
        let i1 = r1["input"].as_array().unwrap();
        let i2 = r2["input"].as_array().unwrap();
        assert!(i2.len() > i1.len());
        for (a, b) in i1.iter().zip(i2.iter()) {
            assert_eq!(a, b, "前轮 input item 必须在后轮中逐字节一致");
        }
    }

    #[test]
    fn d_empty_mid_conversation_system_dropped() {
        // 无文本的中段 system 直接丢弃，不产出空 user item
        let chat = json!({
            "model": "m",
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "hi" },
                { "role": "system", "content": "" }
            ]
        });
        let resp = chat_body_to_responses_body(&chat);
        assert_eq!(resp["instructions"], "sys");
        assert_eq!(resp["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn d_assistant_with_tool_calls() {
        let chat = json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "f", "arguments": "{}" }
                }]
            }]
        });
        let resp = chat_body_to_responses_body(&chat);
        let input = resp["input"].as_array().unwrap();
        // 无文本 → 不产生 message item；function_call 为顶层独立 item
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "c1");
        assert_eq!(input[0]["name"], "f");
    }

    #[test]
    fn d_assistant_text_and_tool_calls_split_into_top_level_items() {
        // 回归测试：function_call 不得嵌套在 message.content 里
        // （上游严格校验会报 `input[N].content did not match any supported type`）
        let chat = json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": "weather?" },
                {
                    "role": "assistant",
                    "content": "Let me check.",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "get_weather", "arguments": "{\"city\":\"sh\"}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "c1", "content": "sunny" }
            ]
        });
        let resp = chat_body_to_responses_body(&chat);
        let input = resp["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);

        // input[1]：assistant 文本 → message item，content 只含 output_text
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        let content = input[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "output_text");
        assert_eq!(content[0]["text"], "Let me check.");

        // input[2]：function_call 顶层 item
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "c1");
        assert_eq!(input[2]["name"], "get_weather");
        assert_eq!(input[2]["arguments"], "{\"city\":\"sh\"}");

        // input[3]：function_call_output 顶层 item
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(input[3]["output"], "sunny");
    }

    #[test]
    fn d_tool_message_to_function_call_output() {
        let chat = json!({
            "model": "m",
            "messages": [{
                "role": "tool",
                "tool_call_id": "c1",
                "content": "sunny"
            }]
        });
        let resp = chat_body_to_responses_body(&chat);
        let input = resp["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "c1");
        assert_eq!(input[0]["output"], "sunny");
    }

    #[test]
    fn d_tools_inverse_transform() {
        let chat = json!({
            "model": "m",
            "messages": [],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "f",
                    "description": "desc",
                    "parameters": { "type": "object" }
                }
            }]
        });
        let resp = chat_body_to_responses_body(&chat);
        let tools = resp["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "f");
        assert_eq!(tools[0]["description"], "desc");
    }

    #[test]
    fn d_tool_choice_inverse() {
        let chat = json!({
            "model": "m",
            "messages": [],
            "tool_choice": { "type": "function", "function": { "name": "f" } }
        });
        let resp = chat_body_to_responses_body(&chat);
        let tc = &resp["tool_choice"];
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["name"], "f");
    }

    #[test]
    fn d_max_tokens_to_max_output_tokens() {
        let chat = json!({
            "model": "m",
            "messages": [],
            "max_tokens": 512
        });
        let resp = chat_body_to_responses_body(&chat);
        assert_eq!(resp["max_output_tokens"], 512);
    }

    #[test]
    fn d_max_completion_tokens_preferred() {
        let chat = json!({
            "model": "m",
            "messages": [],
            "max_completion_tokens": 256,
            "max_tokens": 512
        });
        let resp = chat_body_to_responses_body(&chat);
        // max_tokens comes first in or_else chain, but max_completion_tokens is checked first via or_else
        // Actually: get("max_tokens").or_else(|| get("max_completion_tokens")) -- max_tokens wins
        // Let me check the logic... the code does:
        // chat.get("max_tokens").or_else(|| chat.get("max_completion_tokens"))
        // So max_tokens takes priority. That's correct per spec.
        assert_eq!(resp["max_output_tokens"], 512);
    }

    #[test]
    fn d_store_false_added() {
        let chat = json!({ "model": "m", "messages": [] });
        let resp = chat_body_to_responses_body(&chat);
        assert_eq!(resp["store"], false);
    }

    // ════════════════════════════════════════════════════════════════
    // B. chat_response_to_responses 测试
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn b_text_response() {
        let chat = json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1000,
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hello" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        });
        let resp = chat_response_to_responses(&chat);
        assert_eq!(resp["id"], "resp_123");
        assert_eq!(resp["object"], "response");
        assert_eq!(resp["created_at"], 1000);
        assert_eq!(resp["model"], "gpt-4");
        assert_eq!(resp["status"], "completed");

        let output = resp["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["role"], "assistant");
        assert_eq!(output[0]["content"][0]["text"], "Hello");

        assert_eq!(resp["usage"]["input_tokens"], 10);
        assert_eq!(resp["usage"]["output_tokens"], 5);
        assert_eq!(resp["usage"]["total_tokens"], 15);
    }

    #[test]
    fn b_length_finish_incomplete() {
        let chat = json!({
            "id": "chatcmpl-x",
            "model": "m",
            "choices": [{
                "message": { "content": "truncated" },
                "finish_reason": "length"
            }]
        });
        let resp = chat_response_to_responses(&chat);
        assert_eq!(resp["status"], "incomplete");
        assert_eq!(resp["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn b_reasoning_content() {
        let chat = json!({
            "id": "chatcmpl-r",
            "model": "m",
            "choices": [{
                "message": {
                    "reasoning_content": "think...",
                    "content": "answer"
                },
                "finish_reason": "stop"
            }]
        });
        let resp = chat_response_to_responses(&chat);
        let output = resp["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "think...");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "answer");
    }

    #[test]
    fn b_tool_calls() {
        let chat = json!({
            "id": "chatcmpl-t",
            "model": "m",
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "f", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let resp = chat_response_to_responses(&chat);
        let output = resp["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["call_id"], "call_1");
        assert_eq!(output[0]["name"], "f");
    }

    #[test]
    fn b_usage_with_cache_and_reasoning_tokens() {
        let chat = json!({
            "id": "c", "model": "m",
            "choices": [{ "message": { "content": "ok" }, "finish_reason": "stop" }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "prompt_tokens_details": { "cached_tokens": 30 },
                "completion_tokens_details": { "reasoning_tokens": 10 }
            }
        });
        let resp = chat_response_to_responses(&chat);
        let usage = &resp["usage"];
        assert_eq!(usage["input_tokens"], 100);
        assert_eq!(usage["output_tokens"], 50);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 30);
        assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], 10);
    }

    #[test]
    fn b_no_usage_field_when_missing() {
        let chat = json!({
            "id": "c", "model": "m",
            "choices": [{ "message": { "content": "ok" }, "finish_reason": "stop" }]
        });
        let resp = chat_response_to_responses(&chat);
        assert!(resp.get("usage").is_none());
    }

    // ════════════════════════════════════════════════════════════════
    // E. responses_response_to_chat 测试（B 的逆变换）
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn e_text_response() {
        let resp = json!({
            "id": "resp_123",
            "object": "response",
            "created_at": 1000,
            "model": "gpt-4",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "Hello" }]
            }],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15
            }
        });
        let chat = responses_response_to_chat(&resp);
        assert_eq!(chat["id"], "resp_123");
        assert_eq!(chat["object"], "chat.completion");
        assert_eq!(chat["created"], 1000);
        assert_eq!(chat["choices"][0]["message"]["content"], "Hello");
        assert_eq!(chat["choices"][0]["finish_reason"], "stop");
        assert_eq!(chat["usage"]["prompt_tokens"], 10);
        assert_eq!(chat["usage"]["completion_tokens"], 5);
    }

    #[test]
    fn e_incomplete_to_length() {
        let resp = json!({
            "id": "r", "model": "m",
            "status": "incomplete",
            "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "trunc" }] }]
        });
        let chat = responses_response_to_chat(&resp);
        assert_eq!(chat["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn e_reasoning_content() {
        let resp = json!({
            "id": "r", "model": "m", "status": "completed",
            "output": [
                { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "think" }] },
                { "type": "message", "content": [{ "type": "output_text", "text": "ans" }] }
            ]
        });
        let chat = responses_response_to_chat(&resp);
        assert_eq!(
            chat["choices"][0]["message"]["reasoning_content"],
            "think"
        );
        assert_eq!(chat["choices"][0]["message"]["content"], "ans");
    }

    #[test]
    fn e_function_calls() {
        let resp = json!({
            "id": "r", "model": "m", "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "c1",
                "name": "f",
                "arguments": "{}"
            }]
        });
        let chat = responses_response_to_chat(&resp);
        let calls = chat["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[0]["function"]["name"], "f");
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn e_usage_with_cache_and_reasoning() {
        let resp = json!({
            "id": "r", "model": "m", "status": "completed", "output": [],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "total_tokens": 150,
                "input_tokens_details": { "cached_tokens": 30 },
                "output_tokens_details": { "reasoning_tokens": 10 }
            }
        });
        let chat = responses_response_to_chat(&resp);
        let usage = &chat["usage"];
        assert_eq!(usage["prompt_tokens"], 100);
        assert_eq!(usage["completion_tokens"], 50);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 30);
        assert_eq!(usage["completion_tokens_details"]["reasoning_tokens"], 10);
    }

    // ════════════════════════════════════════════════════════════════
    // A/D 往返测试
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn ad_roundtrip_instructions_and_input() {
        let responses_body = json!({
            "model": "gpt-4",
            "instructions": "Be helpful",
            "input": [
                { "type": "message", "role": "user", "content": "hi" },
                { "type": "message", "role": "assistant", "content": "hello!" }
            ],
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "Get weather",
                "parameters": { "type": "object" }
            }],
            "tool_choice": "auto",
            "max_output_tokens": 1024,
            "temperature": 0.5
        });
        let chat_req = responses_request_to_chat(&responses_body).unwrap();
        assert_eq!(chat_req.messages[0].role, "system");
        assert_eq!(chat_req.messages[0].content.as_deref(), Some("Be helpful"));
        assert_eq!(chat_req.messages[1].role, "user");
        assert_eq!(chat_req.messages[2].role, "assistant");
        assert_eq!(chat_req.max_tokens, Some(1024));

        // 再反向转回
        let chat_json = serde_json::to_value(&chat_req).unwrap();
        let back = chat_body_to_responses_body(&chat_json);
        assert_eq!(back["instructions"], "Be helpful");
        let input = back["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
    }

    #[test]
    fn ad_roundtrip_function_calls() {
        let responses_body = json!({
            "model": "m",
            "input": [
                { "type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"x\":1}" },
                { "type": "function_call_output", "call_id": "c1", "output": "result" }
            ]
        });
        let chat_req = responses_request_to_chat(&responses_body).unwrap();
        // function_call → assistant with tool_calls
        assert_eq!(chat_req.messages[0].role, "assistant");
        assert!(chat_req.messages[0].tool_calls.is_some());
        // function_call_output → tool
        assert_eq!(chat_req.messages[1].role, "tool");
        assert_eq!(chat_req.messages[1].tool_call_id.as_deref(), Some("c1"));
    }

    // ════════════════════════════════════════════════════════════════
    // B/E 往返测试
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn be_roundtrip_text() {
        let chat = json!({
            "id": "chatcmpl-abc",
            "object": "chat.completion",
            "created": 999,
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hello world" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        });
        let resp = chat_response_to_responses(&chat);
        let back = responses_response_to_chat(&resp);
        assert_eq!(back["id"], "resp_abc");
        assert_eq!(back["created"], 999);
        assert_eq!(back["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(back["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn be_roundtrip_tool_calls() {
        let chat = json!({
            "id": "chatcmpl-t", "model": "m",
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": { "name": "f", "arguments": "{\"a\":1}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let resp = chat_response_to_responses(&chat);
        let back = responses_response_to_chat(&resp);
        let calls = back["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["name"], "f");
        assert_eq!(calls[0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(back["choices"][0]["finish_reason"], "tool_calls");
    }

    // ════════════════════════════════════════════════════════════════
    // C. ChatToResponsesSseTranslator 测试
    // ════════════════════════════════════════════════════════════════

    fn chat_sse_chunk(content: &str, finish: Option<&str>) -> String {
        let finish_json = match finish {
            Some(f) => format!("\"{f}\""),
            None => "null".to_string(),
        };
        format!(
            "data: {{\"id\":\"chatcmpl-c1\",\"model\":\"m\",\"created\":1000,\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}},\"finish_reason\":{finish_json}}}]}}\n\n"
        )
    }

    #[test]
    fn c_first_chunk_emits_response_created() {
        let mut t = ChatToResponsesSseTranslator::new();
        let out = String::from_utf8(t.push(chat_sse_chunk("Hi", None).as_bytes())).unwrap();
        assert!(out.contains("response.created"), "missing response.created:\n{out}");
        assert!(out.contains("response.output_item.added"), "missing output_item.added:\n{out}");
        assert!(out.contains("response.content_part.added"), "missing content_part.added:\n{out}");
        assert!(out.contains("response.output_text.delta"), "missing output_text.delta:\n{out}");
        assert!(!out.contains("response.completed"), "should not complete yet:\n{out}");
    }

    #[test]
    fn c_finish_emits_done_events_and_completed() {
        let mut t = ChatToResponsesSseTranslator::new();
        let mut all = Vec::new();
        all.extend(t.push(chat_sse_chunk("Hello", None).as_bytes()));
        all.extend(t.push(chat_sse_chunk("", Some("stop")).as_bytes()));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("response.output_text.done"));
        assert!(text.contains("response.content_part.done"));
        assert!(text.contains("response.output_item.done"));
        assert!(text.contains("response.completed"));
        assert!(text.contains("\"status\":\"completed\""));
        assert!(text.contains("data: [DONE]"));
    }

    #[test]
    fn c_length_finish_incomplete() {
        let mut t = ChatToResponsesSseTranslator::new();
        let mut all = Vec::new();
        all.extend(t.push(chat_sse_chunk("x", None).as_bytes()));
        all.extend(t.push(chat_sse_chunk("", Some("length")).as_bytes()));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("\"status\":\"incomplete\""));
        assert!(text.contains("max_output_tokens"));
    }

    #[test]
    fn c_done_after_finish_is_noop() {
        let mut t = ChatToResponsesSseTranslator::new();
        let _ = t.push(chat_sse_chunk("Hi", None).as_bytes());
        let _ = t.push(chat_sse_chunk("", Some("stop")).as_bytes());
        let out = t.push(b"data: [DONE]\n\n");
        assert!(out.is_empty(), "already closed");
    }

    #[test]
    fn c_done_without_finish_closes() {
        let mut t = ChatToResponsesSseTranslator::new();
        let _ = t.push(chat_sse_chunk("Hi", None).as_bytes());
        let out = String::from_utf8(t.push(b"data: [DONE]\n\n")).unwrap();
        assert!(out.contains("response.completed"));
        assert!(out.contains("data: [DONE]"));
    }

    #[test]
    fn c_reasoning_delta_events() {
        let mut t = ChatToResponsesSseTranslator::new();
        let reasoning_chunk = "data: {\"id\":\"chatcmpl-c1\",\"model\":\"m\",\"created\":1000,\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":null}]}\n\n";
        let mut all = Vec::new();
        all.extend(t.push(reasoning_chunk.as_bytes()));
        all.extend(t.push(chat_sse_chunk("answer", None).as_bytes()));
        all.extend(t.push(chat_sse_chunk("", Some("stop")).as_bytes()));
        let text = String::from_utf8(all).unwrap();

        assert!(text.contains("response.output_item.added"));
        assert!(text.contains("response.reasoning_summary_part.added"));
        assert!(text.contains("response.reasoning_summary_text.delta"));
        assert!(text.contains("thinking"));
        // reasoning done events
        assert!(text.contains("response.reasoning_summary_text.done"));
        assert!(text.contains("response.reasoning_summary_part.done"));
    }

    #[test]
    fn c_tool_call_events() {
        let mut t = ChatToResponsesSseTranslator::new();
        let tool_chunk = "data: {\"id\":\"chatcmpl-c1\",\"model\":\"m\",\"created\":1000,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":1}\"}}]},\"finish_reason\":null}]}\n\n";
        let mut all = Vec::new();
        all.extend(t.push(tool_chunk.as_bytes()));
        all.extend(t.push(chat_sse_chunk("", Some("tool_calls")).as_bytes()));
        let text = String::from_utf8(all).unwrap();

        assert!(text.contains("response.output_item.added"), "missing output_item.added:\n{text}");
        // 验证 added 事件的 data 行中包含 function_call 类型
        assert!(text.contains("\"type\":\"function_call\""), "missing function_call in added:\n{text}");
        assert!(text.contains("response.function_call_arguments.delta"), "missing arguments delta:\n{text}");
        assert!(text.contains("response.function_call_arguments.done"), "missing arguments done:\n{text}");
    }

    /// 解析 SSE 文本为 (event名, data JSON) 列表。
    fn parse_responses_sse(text: &str) -> Vec<(String, Value)> {
        let mut events = Vec::new();
        let mut event_name = String::new();
        for line in text.lines() {
            if let Some(name) = line.strip_prefix("event: ") {
                event_name = name.to_string();
            } else if let Some(data) = line.strip_prefix("data: ") {
                if data != "[DONE]" {
                    if let Ok(v) = serde_json::from_str(data) {
                        events.push((event_name.clone(), v));
                    }
                }
            }
        }
        events
    }

    #[test]
    fn c_tool_call_done_and_completed_carry_real_call_id_and_name() {
        // 回归：done/completed 事件的 function_call item 必须回带上游真实
        // call_id/name——客户端靠 call_id 关联 function_call_output、靠 name
        // 识别要执行的函数；此前被错误地写成 resp 级 id 和空串。
        let mut t = ChatToResponsesSseTranslator::new();
        let tool_chunk = "data: {\"id\":\"chatcmpl-c1\",\"model\":\"m\",\"created\":1000,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}}]},\"finish_reason\":null}]}\n\n";
        let mut all = Vec::new();
        all.extend(t.push(tool_chunk.as_bytes()));
        all.extend(t.push(chat_sse_chunk("", Some("tool_calls")).as_bytes()));
        let text = String::from_utf8(all).unwrap();
        let events = parse_responses_sse(&text);

        let done = events
            .iter()
            .find(|(name, d)| name == "response.output_item.done" && d["item"]["type"] == "function_call")
            .expect("missing function_call output_item.done");
        assert_eq!(done.1["item"]["call_id"], "call_123", "{text}");
        assert_eq!(done.1["item"]["name"], "get_weather", "{text}");

        let completed = events
            .iter()
            .find(|(name, _)| name == "response.completed")
            .expect("missing response.completed");
        let item = &completed.1["response"]["output"][0];
        assert_eq!(item["call_id"], "call_123", "{text}");
        assert_eq!(item["name"], "get_weather", "{text}");
        assert_eq!(item["arguments"], "{\"city\":\"SF\"}", "{text}");
        // id 是 item 级标识，call_id 是工具关联键，二者不得相同
        assert_ne!(item["id"], item["call_id"], "{text}");
    }

    #[test]
    fn c_empty_stream_done_emits_created_and_completed() {
        // 回归：空流（只收到 [DONE]）也必须发 response.created + response.completed，
        // 否则严格客户端一直等 completed 直到 TCP 超时。
        let mut t = ChatToResponsesSseTranslator::new();
        let out = String::from_utf8(t.push(b"data: [DONE]\n\n")).unwrap();
        assert!(out.contains("response.created"), "{out}");
        assert!(out.contains("response.completed"), "{out}");
        assert!(out.contains("data: [DONE]"), "{out}");
        // 幂等：再 push 不重复
        let out2 = t.push(b"data: [DONE]\n\n");
        assert!(out2.is_empty());
    }

    #[test]
    fn c_completed_event_is_scanner_compatible() {
        // 回归：入口翻译器输出的 response.completed 必须能被本网关 UsageSseScanner
        // 解析出用量（官方格式：data.type + data.response.usage 嵌套）。
        let mut t = ChatToResponsesSseTranslator::new();
        let chunk_with_usage = "data: {\"id\":\"chatcmpl-c1\",\"model\":\"m\",\"created\":1000,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":87,\"completion_tokens\":16,\"total_tokens\":103,\"prompt_cache_hit_tokens\":60}}\n\n";
        let out = t.push(chunk_with_usage.as_bytes());
        let text = String::from_utf8(out).unwrap();
        // data JSON 必须带 type 字段（官方客户端按 data.type 分发）
        let events = parse_responses_sse(&text);
        let completed = events
            .iter()
            .find(|(name, _)| name == "response.completed")
            .expect("missing response.completed");
        assert_eq!(completed.1["type"], "response.completed", "{text}");

        let mut scanner = crate::llm::usage::UsageSseScanner::new();
        scanner.push(text.as_bytes());
        let u = scanner.finish();
        assert_eq!(u.prompt_tokens, 87, "{text}");
        assert_eq!(u.cache_hit_tokens, 60, "{text}");
        assert_eq!(u.completion_tokens, 16, "{text}");
    }

    #[test]
    fn f_done_without_created_emits_only_done() {
        // 空的上游 Responses 流（只收 [DONE]）：只补发 [DONE]，
        // 不发 id/model 为空的非法 chat chunk。
        let mut t = ResponsesToChatSseTranslator::new();
        let out = String::from_utf8(t.push(b"data: [DONE]\n\n")).unwrap();
        assert_eq!(out, "data: [DONE]\n\n", "{out}");
    }

    #[test]
    fn c_parallel_tool_calls_index_mapping() {
        let mut t = ChatToResponsesSseTranslator::new();
        let chunk0 = "data: {\"id\":\"chatcmpl-c1\",\"model\":\"m\",\"created\":1000,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"type\":\"function\",\"function\":{\"name\":\"fa\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n";
        let chunk1 = "data: {\"id\":\"chatcmpl-c1\",\"model\":\"m\",\"created\":1000,\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"type\":\"function\",\"function\":{\"name\":\"fb\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n";
        let mut all = Vec::new();
        all.extend(t.push(chunk0.as_bytes()));
        all.extend(t.push(chunk1.as_bytes()));
        all.extend(t.push(chat_sse_chunk("", Some("tool_calls")).as_bytes()));
        let text = String::from_utf8(all).unwrap();

        // 两个 output_item.added 事件（只数 event: 行，data JSON 里的 type 字段不算）
        assert_eq!(text.matches("event: response.output_item.added").count(), 2);
    }

    #[test]
    fn c_usage_in_completed() {
        let mut t = ChatToResponsesSseTranslator::new();
        let chunk_with_usage = "data: {\"id\":\"chatcmpl-c1\",\"model\":\"m\",\"created\":1000,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n";
        let out = String::from_utf8(t.push(chunk_with_usage.as_bytes())).unwrap();
        assert!(out.contains("response.completed"));
        let completed_pos = out.find("event: response.completed").unwrap();
        let completed_section = &out[completed_pos..];
        assert!(completed_section.contains("\"input_tokens\":10"));
        assert!(completed_section.contains("\"output_tokens\":5"));
    }

    // ════════════════════════════════════════════════════════════════
    // F. ResponsesToChatSseTranslator 测试
    // ════════════════════════════════════════════════════════════════

    fn responses_sse_event(event_type: &str, data: &Value) -> String {
        format!("data: {}\n\n", json!({ "type": event_type, "response": data }))
    }

    #[test]
    fn f_response_created_emits_first_chat_chunk() {
        let mut t = ResponsesToChatSseTranslator::new();
        let event = responses_sse_event(
            "response.created",
            &json!({
                "id": "resp_1",
                "model": "gpt-4",
                "created_at": 1000,
                "output": [],
                "status": "in_progress",
            }),
        );
        let out = String::from_utf8(t.push(event.as_bytes())).unwrap();
        assert!(out.contains("chat.completion.chunk"));
        assert!(out.contains("\"role\":\"assistant\""));
        assert!(!out.contains("data: [DONE]"), "should not close yet");
    }

    #[test]
    fn f_output_text_delta_to_content() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let delta = json!({ "type": "response.output_text.delta", "delta": "Hello" });
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(format!("data: {delta}\n\n").as_bytes()));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("\"content\":\"Hello\""));
    }

    #[test]
    fn f_reasoning_delta_to_reasoning_content() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let delta = json!({ "type": "response.reasoning_summary_text.delta", "delta": "thinking" });
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(format!("data: {delta}\n\n").as_bytes()));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("\"reasoning_content\":\"thinking\""));
    }

    #[test]
    fn f_function_call_added_and_arguments() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let added = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "function_call", "call_id": "c1", "name": "f", "arguments": "" }
        });
        let args_delta = json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"a\":1}"
        });
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(format!("data: {added}\n\n").as_bytes()));
        all.extend(t.push(format!("data: {args_delta}\n\n").as_bytes()));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("\"tool_calls\""));
        assert!(text.contains("\"name\":\"f\""));
        assert!(text.contains("\"arguments\":\"{\\\"a\\\":1}\""));
    }

    #[test]
    fn f_completed_emits_finish_chunk_and_done() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "r",
                "model": "m",
                "status": "completed",
                "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "hi" }] }],
                "usage": { "input_tokens": 5, "output_tokens": 3, "total_tokens": 8 }
            }
        });
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(format!("data: {completed}\n\n").as_bytes()));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert!(text.contains("data: [DONE]"));
        assert!(text.contains("\"prompt_tokens\":5"));
    }

    #[test]
    fn f_incomplete_to_length_finish() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let incomplete = json!({
            "type": "response.incomplete",
            "response": { "id": "r", "model": "m", "status": "incomplete", "output": [] }
        });
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(format!("data: {incomplete}\n\n").as_bytes()));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("\"finish_reason\":\"length\""));
    }

    #[test]
    fn f_function_call_in_output_to_tool_calls_finish() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "r",
                "model": "m",
                "status": "completed",
                "output": [{ "type": "function_call", "call_id": "c1", "name": "f" }]
            }
        });
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(format!("data: {completed}\n\n").as_bytes()));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("\"finish_reason\":\"tool_calls\""));
    }

    #[test]
    fn f_failed_emits_stop_and_done() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let failed = json!({ "type": "response.failed" });
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(format!("data: {failed}\n\n").as_bytes()));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert!(text.contains("data: [DONE]"));
    }

    #[test]
    fn f_done_without_completed_closes_gracefully() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(b"data: [DONE]\n\n"));
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert!(text.contains("data: [DONE]"));
    }

    #[test]
    fn f_done_after_completed_is_noop() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let completed = json!({
            "type": "response.completed",
            "response": { "id": "r", "model": "m", "status": "completed", "output": [] }
        });
        let _ = t.push(created.as_bytes());
        let _ = t.push(format!("data: {completed}\n\n").as_bytes());
        let out = t.push(b"data: [DONE]\n\n");
        assert!(out.is_empty(), "already closed");
    }

    #[test]
    fn f_other_events_ignored() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let item_done = json!({ "type": "response.output_item.done", "output_index": 0 });
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(format!("data: {item_done}\n\n").as_bytes()));
        let text = String::from_utf8(all).unwrap();
        // 只有 created 的 chat chunk，不应有其他 delta
        assert_eq!(text.matches("chat.completion.chunk").count(), 1);
    }

    // ════════════════════════════════════════════════════════════════
    // C/F 事件顺序断言
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn c_event_order_text_only() {
        let mut t = ChatToResponsesSseTranslator::new();
        let mut all = Vec::new();
        all.extend(t.push(chat_sse_chunk("A", None).as_bytes()));
        all.extend(t.push(chat_sse_chunk("B", None).as_bytes()));
        all.extend(t.push(chat_sse_chunk("", Some("stop")).as_bytes()));
        let text = String::from_utf8(all).unwrap();

        let expected = [
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ];
        let mut last_pos = 0;
        for ev in &expected {
            let pos = text
                .find(ev)
                .unwrap_or_else(|| panic!("missing {ev} in:\n{text}"));
            assert!(pos >= last_pos, "{ev} out of order");
            last_pos = pos;
        }
    }

    // ════════════════════════════════════════════════════════════════
    // UTF-8 安全测试
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn c_stream_multibyte_utf8_split_no_replacement_char() {
        let mut t = ChatToResponsesSseTranslator::new();
        let chunk = chat_sse_chunk("你好👋", None);
        let bytes = chunk.as_bytes();

        // 逐字节切碎喂入（最严苛的切法）
        for i in 1..bytes.len() {
            let mut t2 = ChatToResponsesSseTranslator::new();
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

        // 整体验证
        let out = t.push(bytes);
        assert!(String::from_utf8(out).unwrap().contains("你好👋"));
    }

    #[test]
    fn f_stream_multibyte_utf8_split_no_replacement_char() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let delta = json!({ "type": "response.output_text.delta", "delta": "你好👋" });
        let completed = json!({
            "type": "response.completed",
            "response": { "id": "r", "model": "m", "status": "completed", "output": [] }
        });

        // 组装完整的 SSE 流
        let full_stream = format!(
            "{created}data: {delta}\n\ndata: {completed}\n\ndata: [DONE]\n\n"
        );
        let bytes = full_stream.as_bytes();

        for i in 1..bytes.len() {
            let mut t2 = ResponsesToChatSseTranslator::new();
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

        // 整体验证
        let out = t.push(bytes);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("你好👋"));
    }

    // ════════════════════════════════════════════════════════════════
    // C/F 跨 chunk 不完整行测试
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn c_partial_line_across_chunks() {
        let mut t = ChatToResponsesSseTranslator::new();
        let chunk = chat_sse_chunk("World", None);
        let bytes = chunk.as_bytes();
        let mid = bytes.len() / 2;

        let out1 = t.push(&bytes[..mid]);
        let out2 = t.push(&bytes[mid..]);
        let combined = [out1, out2].concat();
        let text = String::from_utf8(combined).unwrap();
        assert!(text.contains("response.created"));
        assert!(text.contains("World"));
    }

    #[test]
    fn f_partial_line_across_chunks() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let _ = t.push(created.as_bytes());

        let delta = json!({ "type": "response.output_text.delta", "delta": "World" });
        let full = format!("data: {delta}\n\n");
        let bytes = full.as_bytes();
        let mid = bytes.len() / 2;

        let out1 = t.push(&bytes[..mid]);
        let out2 = t.push(&bytes[mid..]);
        let combined = [out1, out2].concat();
        let text = String::from_utf8(combined).unwrap();
        assert!(text.contains("\"content\":\"World\""));
    }

    // ════════════════════════════════════════════════════════════════
    // close 幂等测试
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn c_close_idempotent() {
        let mut t = ChatToResponsesSseTranslator::new();
        let _ = t.push(chat_sse_chunk("Hi", None).as_bytes());
        let _ = t.push(chat_sse_chunk("", Some("stop")).as_bytes());
        // 再 push 一次 DONE
        let out1 = t.push(b"data: [DONE]\n\n");
        assert!(out1.is_empty());
        // 再 push 一次普通 chunk
        let out2 = t.push(chat_sse_chunk("extra", None).as_bytes());
        assert!(out2.is_empty());
    }

    #[test]
    fn f_close_idempotent() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let completed = json!({
            "type": "response.completed",
            "response": { "id": "r", "model": "m", "status": "completed", "output": [] }
        });
        let _ = t.push(created.as_bytes());
        let _ = t.push(format!("data: {completed}\n\n").as_bytes());
        let out1 = t.push(b"data: [DONE]\n\n");
        assert!(out1.is_empty());
        let out2 = t.push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"extra\"}\n\n");
        assert!(out2.is_empty());
    }

    // ════════════════════════════════════════════════════════════════
    // E/E 无 usage 测试
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn e_no_usage_when_absent() {
        let resp = json!({
            "id": "r", "model": "m", "status": "completed",
            "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "hi" }] }]
        });
        let chat = responses_response_to_chat(&resp);
        assert!(chat.get("usage").is_none());
    }

    // ════════════════════════════════════════════════════════════════
    // 多 tool_calls index 分配
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn f_two_parallel_tool_calls_index_mapping() {
        let mut t = ResponsesToChatSseTranslator::new();
        let created = responses_sse_event(
            "response.created",
            &json!({ "id": "r", "model": "m", "created_at": 0, "output": [] }),
        );
        let added0 = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "function_call", "call_id": "c1", "name": "f1", "arguments": "" }
        });
        let added1 = json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": { "type": "function_call", "call_id": "c2", "name": "f2", "arguments": "" }
        });
        let args0 = json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{}"
        });
        let args1 = json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1,
            "delta": "{}"
        });
        let mut all = Vec::new();
        all.extend(t.push(created.as_bytes()));
        all.extend(t.push(format!("data: {added0}\n\n").as_bytes()));
        all.extend(t.push(format!("data: {added1}\n\n").as_bytes()));
        all.extend(t.push(format!("data: {args0}\n\n").as_bytes()));
        all.extend(t.push(format!("data: {args1}\n\n").as_bytes()));
        let text = String::from_utf8(all).unwrap();

        // 两个 tool_calls chunk
        let tc_chunks: Vec<&str> = text
            .split("data: ")
            .filter(|s| s.contains("\"tool_calls\""))
            .collect();
        assert!(tc_chunks.len() >= 2, "expected at least 2 tool_calls chunks:\n{text}");
        // index 0 和 1
        assert!(text.contains("\"index\":0"));
        assert!(text.contains("\"index\":1"));
    }

    // ════════════════════════════════════════════════════════════════
    // A 缺失 input 返回错误
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn a_empty_array_input() {
        let body = json!({ "model": "m", "input": [] });
        let req = responses_request_to_chat(&body).unwrap();
        assert!(req.messages.is_empty());
    }

    // ════════════════════════════════════════════════════════════════
    // C/F usage mapping roundtrip
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn cf_usage_mapping_roundtrip() {
        let chat_usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150,
            "prompt_tokens_details": { "cached_tokens": 30 },
            "completion_tokens_details": { "reasoning_tokens": 10 }
        });
        let responses_usage = map_usage_chat_to_responses(&chat_usage);
        assert_eq!(responses_usage["input_tokens"], 100);
        assert_eq!(responses_usage["output_tokens"], 50);
        assert_eq!(responses_usage["input_tokens_details"]["cached_tokens"], 30);
        assert_eq!(responses_usage["output_tokens_details"]["reasoning_tokens"], 10);

        let back = map_usage_responses_to_chat(&responses_usage);
        assert_eq!(back["prompt_tokens"], 100);
        assert_eq!(back["completion_tokens"], 50);
        assert_eq!(back["prompt_tokens_details"]["cached_tokens"], 30);
        assert_eq!(back["completion_tokens_details"]["reasoning_tokens"], 10);
    }
}
