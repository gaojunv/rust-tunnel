use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use super::openai_handler::LlmHandlerState;
use super::router::resolve_model;
use super::upstream::call_upstream;
use super::{ChatCompletionRequest, ChatMessage};

/// 拆解 Anthropic 消息 content 字段的结果。
///
/// Anthropic content 允许是纯字符串或 content block 数组，块类型包含
/// `text` / `tool_use` / `tool_result`（本次只识别这三种）。
struct ParsedContent {
    /// 所有 `text` 块拼接后的文本；空字符串表示没有文本内容。
    text: String,
    /// assistant 消息里的 `tool_use` 块 → OpenAI `tool_calls` 元素。
    tool_uses: Vec<Value>,
    /// user 消息里的 `tool_result` 块 → 每个都要展开成一条 `role="tool"` 消息。
    tool_results: Vec<ToolResult>,
}

/// 一个 Anthropic `tool_result` 块的关键字段。
struct ToolResult {
    tool_call_id: String,
    /// Anthropic 允许 content 是字符串或 content block 数组；这里统一序列化为字符串。
    content: String,
}

/// 把 Anthropic content 字段（字符串 或 block 数组）解析为文本 + 工具信息。
fn parse_anthropic_content(content: &Value) -> ParsedContent {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_uses: Vec<Value> = Vec::new();
    let mut tool_results: Vec<ToolResult> = Vec::new();

    match content {
        Value::String(s) => text_parts.push(s.clone()),
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            text_parts.push(t.to_string());
                        }
                    }
                    Some("tool_use") => {
                        // Anthropic: {id, name, input:{...}} → OpenAI: {id, type:"function",
                        // function:{name, arguments: JSON.stringify(input)}}
                        let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                        let arguments =
                            serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                        tool_uses.push(json!({
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": arguments },
                        }));
                    }
                    Some("tool_result") => {
                        let tool_call_id = block
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // content 可为字符串，或 content block 数组（只取 text 块）；
                        // 展平成一个字符串给 OpenAI 的 tool 消息用。
                        let content = match block.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(Value::Array(inner)) => inner
                                .iter()
                                .filter_map(|b| {
                                    b.get("type")
                                        .and_then(|t| t.as_str())
                                        .filter(|t| *t == "text")
                                        .and(b.get("text").and_then(|t| t.as_str()))
                                        .map(str::to_string)
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };
                        tool_results.push(ToolResult {
                            tool_call_id,
                            content,
                        });
                    }
                    // image / document / 其它块暂不支持，静默忽略（保持现状）。
                    _ => {}
                }
            }
        }
        _ => {}
    }

    ParsedContent {
        text: text_parts.join("\n"),
        tool_uses,
        tool_results,
    }
}

/// 把 Anthropic 顶层 `tools` 数组转成 OpenAI functions 声明。
fn anthropic_tools_to_openai(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = t.get("description").cloned().unwrap_or(Value::Null);
            let parameters = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                },
            })
        })
        .collect()
}

/// 把 Anthropic `tool_choice` 对象映射到 OpenAI 形式。
fn anthropic_tool_choice_to_openai(v: &Value) -> Option<Value> {
    let ty = v.get("type").and_then(|t| t.as_str())?;
    match ty {
        "auto" => Some(Value::String("auto".to_string())),
        // Anthropic `any` = 必须调用某个工具（任意）
        "any" => Some(Value::String("required".to_string())),
        "tool" => {
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
            Some(json!({
                "type": "function",
                "function": { "name": name },
            }))
        }
        // Anthropic 也允许 "none"（虽不常见），保守透传。
        "none" => Some(Value::String("none".to_string())),
        _ => None,
    }
}

/// Convert Anthropic Messages request to unified ChatCompletionRequest.
fn anthropic_to_openai(body: &Value) -> Result<ChatCompletionRequest, String> {
    let anthropic_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("model is required")?;

    let messages_raw = body.get("messages").ok_or("messages is required")?;
    let raw_arr = messages_raw.as_array().ok_or("messages must be an array")?;

    let mut all_messages: Vec<ChatMessage> = Vec::new();

    // Anthropic 的 system 是顶层字段，不是 message 角色。
    if let Some(system) = body.get("system") {
        let parsed = parse_anthropic_content(system);
        if !parsed.text.is_empty() {
            all_messages.push(ChatMessage::text("system", parsed.text));
        }
    }

    for msg in raw_arr {
        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("user")
            .to_string();
        let parsed = msg
            .get("content")
            .map(parse_anthropic_content)
            .unwrap_or(ParsedContent {
                text: String::new(),
                tool_uses: Vec::new(),
                tool_results: Vec::new(),
            });

        match role.as_str() {
            "assistant" => {
                // assistant 消息：文本与 tool_use 可共存，映射到同一条 message。
                let content = if parsed.text.is_empty() {
                    None
                } else {
                    Some(parsed.text)
                };
                let tool_calls = if parsed.tool_uses.is_empty() {
                    None
                } else {
                    Some(parsed.tool_uses)
                };
                // 只有当至少一个字段有值时才推入（防止全空消息）。
                if content.is_some() || tool_calls.is_some() {
                    all_messages.push(ChatMessage {
                        role,
                        content,
                        tool_calls,
                        tool_call_id: None,
                        name: None,
                    });
                }
            }
            _ => {
                // user / 其它角色：文本正常挂到 user 消息；tool_result 展开成独立
                // `role="tool"` 消息（OpenAI 每个 tool_call_id 一条）。
                if !parsed.text.is_empty() {
                    all_messages.push(ChatMessage::text(&role, parsed.text));
                }
                for tr in parsed.tool_results {
                    all_messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: Some(tr.content),
                        tool_calls: None,
                        tool_call_id: Some(tr.tool_call_id),
                        name: None,
                    });
                }
            }
        }
    }

    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let tools = body
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| anthropic_tools_to_openai(arr.as_slice()))
        .filter(|v| !v.is_empty());

    let tool_choice = body
        .get("tool_choice")
        .and_then(anthropic_tool_choice_to_openai);

    Ok(ChatCompletionRequest {
        model: anthropic_model.to_string(),
        messages: all_messages,
        stream,
        max_tokens: body
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        temperature: body
            .get("temperature")
            .and_then(|v| v.as_f64())
            .map(|v| v as f32),
        top_p: body.get("top_p").and_then(|v| v.as_f64()).map(|v| v as f32),
        tools,
        tool_choice,
    })
}

/// POST /v1/messages — Anthropic Messages API.
///
/// 当 provider 配置了 `anthropic_base_url` 时，直接透传原始 Anthropic 请求到上游，
/// 不做任何格式转换；否则回退到 OpenAI 格式转换路径。
pub async fn handle_messages(
    State(state): State<LlmHandlerState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // Validate API key
    let auth = match super::auth::authenticate(&state.llm, &headers).await {
        Some(a) => a,
        None => {
            // 记录认证失败
            if let Some(ref db) = state.llm.db {
                let ctx = super::usage::UsageContext {
                    protocol: "anthropic".into(),
                    ..Default::default()
                };
                ctx.record_failure(db, 401, "authentication_error", std::time::Instant::now());
            }
            return state.error_for_protocol(
                StatusCode::UNAUTHORIZED,
                "Invalid API key".into(),
                "authentication_error",
            );
        }
    };
    let (api_key_id, api_key_name) = auth;

    // 提取 model 名用于路由解析
    let model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => {
            // 记录请求错误（缺少 model）
            if let Some(ref db) = state.llm.db {
                let ctx = super::usage::UsageContext {
                    api_key_id: Some(api_key_id.clone()),
                    api_key_name: api_key_name.clone(),
                    protocol: "anthropic".into(),
                    ..Default::default()
                };
                ctx.record_failure(db, 400, "invalid_request_error", std::time::Instant::now());
            }
            return state.error_for_protocol(
                StatusCode::BAD_REQUEST,
                "model is required".into(),
                "invalid_request_error",
            );
        }
    };

    // Resolve model → provider
    let (provider, actual_model, model_id) = match resolve_model(&state.llm, &model).await {
        Ok(r) => r,
        Err(e) => {
            // 记录路由失败到用量日志
            if let Some(ref db) = state.llm.db {
                let ctx = super::usage::UsageContext {
                    api_key_id: Some(api_key_id.clone()),
                    api_key_name: api_key_name.clone(),
                    requested_model: model.clone(),
                    protocol: "anthropic".into(),
                    ..Default::default()
                };
                ctx.record_failure(db, 404, "model_resolution_error", std::time::Instant::now());
            }
            return super::router::resolve_error_response(&state.llm, e).await;
        }
    };

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // 用量采集上下文
    let api_key_id_for_rag = api_key_id.clone();
    let mut ctx = super::usage::UsageContext {
        api_key_id: Some(api_key_id),
        api_key_name,
        provider_id: Some(provider.id.clone()),
        provider_name: provider.name.clone(),
        model_id: Some(model_id),
        model_name: actual_model.clone(),
        requested_model: model,
        protocol: "anthropic".into(),
        stream: is_stream,
        rag_chunks_injected: None,
    };
    let started = std::time::Instant::now();
    let db = state.llm.db.clone();

    // ── 直通路径：provider 配置了 anthropic_base_url ──
    if let Some(ref anthropic_url) = provider.anthropic_base_url {
        // 替换 model 为实际上游名称
        let mut body = body;
        body["model"] = serde_json::Value::String(actual_model);

        return match super::upstream::call_upstream_raw(
            anthropic_url,
            &provider.api_key,
            "/v1/messages",
            &body,
            is_stream,
        )
        .await
        {
            Ok(resp) => super::usage::wrap_and_record(resp, ctx, db, started).await,
            Err((status, msg)) => {
                if let Some(ref db) = db {
                    ctx.record_failure(db, status.as_u16() as i32, "upstream_error", started);
                }
                state.error_for_protocol(status, msg, "upstream_error")
            }
        };
    }

    // ── 回退路径：转成 OpenAI 格式发到 base_url ──
    let request = match anthropic_to_openai(&body) {
        Ok(r) => r,
        Err(e) => {
            return state.error_for_protocol(StatusCode::BAD_REQUEST, e, "invalid_request_error")
        }
    };

    let mut request = request;
    request.model = actual_model;

    // RAG：API key 绑定知识库时，检索背景资料注入 messages[0]（compat 之前）。
    // 直通路径（anthropic_base_url 分支）不注入 —— 规格边界。
    let mut rag_injected: i64 = 0;
    if let Some(ref db) = db {
        if let Ok(Some(kb_id)) = db.rag_get_kb_id_for_api_key(&api_key_id_for_rag).await {
            let outcome = super::rag::enhance(
                db,
                &state.llm.rag_store,
                state.llm.cipher.as_ref(),
                &kb_id,
                &mut request,
            )
            .await;
            rag_injected = outcome.injected as i64;
        }
    }
    if rag_injected > 0 {
        ctx.rag_chunks_injected = Some(rag_injected);
    }

    let compat_enabled = super::compat::compat_tool_history_enabled(provider.extra_config.as_deref());
    if compat_enabled {
        super::compat::rewrite_tool_history(&mut request.messages);
        super::compat::inject_tool_call_guidance(&mut request.messages);
    }

    match call_upstream(&provider.base_url, &provider.api_key, &request).await {
        Ok(resp) => {
            // 回退路径：上游是 OpenAI 格式，先采集 usage 再转成 Anthropic 格式。
            // 非流式整体转换会消费 body，因此这里在转换后再包一层。
            if !request.stream {
                // compat 模式：先解析伪工具调用还原为结构化 tool_calls，
                // 再转成 Anthropic 格式（Anthropic 的 tool_use 块）。
                let resp = if compat_enabled {
                    super::openai_handler::rewrite_pseudo_tool_calls_in_response(resp).await
                } else {
                    resp
                };
                let converted = convert_openai_to_anthropic_response(resp).await;
                super::usage::wrap_and_record(converted, ctx, db, started).await
            } else {
                // compat 模式：流式路径同样先解析伪工具调用，
                // 再转成 Anthropic SSE 事件流。
                let resp = if compat_enabled {
                    super::openai_handler::rewrite_pseudo_tool_calls_in_stream(resp).await
                } else {
                    resp
                };
                let converted = convert_openai_stream_to_anthropic(resp);
                super::usage::wrap_and_record(converted, ctx, db, started).await
            }
        }
        Err((status, msg)) => {
            if let Some(ref db) = db {
                ctx.record_failure(db, status.as_u16() as i32, "upstream_error", started);
            }
            state.error_for_protocol(status, msg, "upstream_error")
        }
    }
}

/// 流式：把上游 OpenAI SSE 响应体逐 chunk 转换为 Anthropic SSE 事件流。
fn convert_openai_stream_to_anthropic(openai_resp: Response) -> Response {
    use futures_util::StreamExt;

    let byte_stream = openai_resp.into_body().into_data_stream();
    let translator = std::sync::Arc::new(std::sync::Mutex::new(
        super::format::AnthropicSseTranslator::new(),
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
        .unwrap()
}

/// 测试专用：暴露流式 OpenAI→Anthropic 转换，供跨模块端到端测试使用。
#[cfg(test)]
pub(crate) fn convert_openai_stream_to_anthropic_for_test(openai_resp: Response) -> Response {
    convert_openai_stream_to_anthropic(openai_resp)
}

/// Convert OpenAI chat completion response to Anthropic Messages format.
async fn convert_openai_to_anthropic_response(openai_resp: Response) -> Response {    let status = openai_resp.status();
    let body_bytes = axum::body::to_bytes(openai_resp.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();

    let openai: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => {
            return Response::builder()
                .status(status)
                .header("Content-Type", "application/json")
                .body(Body::from(body_bytes))
                .unwrap();
        }
    };

    // Build Anthropic-format response
    let anthropic_resp = super::format::openai_response_to_anthropic(&openai);

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&anthropic_resp).unwrap()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::llm::{LlmProtocol, LlmState};

    #[test]
    fn test_anthropic_to_openai_conversion() {
        let input = serde_json::json!({
            "model": "claude-3-opus",
            "messages": [
                {"role": "user", "content": "Hello"}
            ],
            "system": "You are helpful.",
            "stream": false,
            "max_tokens": 1024,
        });

        let result = anthropic_to_openai(&input).unwrap();
        assert_eq!(result.model, "claude-3-opus");
        assert_eq!(result.messages.len(), 2); // system + user
        assert_eq!(result.messages[0].role, "system");
        assert_eq!(
            result.messages[0].content.as_deref(),
            Some("You are helpful.")
        );
        assert_eq!(result.messages[1].role, "user");
        assert_eq!(result.messages[1].content.as_deref(), Some("Hello"));
        assert!(!result.stream);
        assert_eq!(result.max_tokens, Some(1024));
    }

    #[test]
    fn test_anthropic_to_openai_without_system() {
        let input = serde_json::json!({
            "model": "claude-sonnet",
            "messages": [
                {"role": "user", "content": "Hi"}
            ],
            "stream": true,
        });

        let result = anthropic_to_openai(&input).unwrap();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].role, "user");
        assert!(result.stream);
        assert_eq!(result.max_tokens, None);
    }

    #[test]
    fn test_anthropic_to_openai_missing_model() {
        let input = serde_json::json!({
            "messages": [{"role": "user", "content": "Hi"}]
        });
        assert!(anthropic_to_openai(&input).is_err());
    }

    #[test]
    fn test_anthropic_to_openai_missing_messages() {
        let input = serde_json::json!({
            "model": "test",
        });
        assert!(anthropic_to_openai(&input).is_err());
    }

    #[test]
    fn test_anthropic_to_openai_with_temperature() {
        let input = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": 0.7,
            "top_p": 0.9,
        });

        let result = anthropic_to_openai(&input).unwrap();
        assert_eq!(result.temperature, Some(0.7));
        assert_eq!(result.top_p, Some(0.9));
    }

    #[test]
    fn test_anthropic_content_blocks() {
        // Anthropic SDK sends content as an array of content blocks
        let input = serde_json::json!({
            "model": "claude-3-opus",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Hello"},
                        {"type": "text", "text": "World"}
                    ]
                }
            ],
            "stream": false,
        });

        let result = anthropic_to_openai(&input).unwrap();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].role, "user");
        assert_eq!(result.messages[0].content.as_deref(), Some("Hello\nWorld"));
    }

    #[test]
    fn test_anthropic_mixed_content_single_text_block() {
        // Single text block should work too
        let input = serde_json::json!({
            "model": "test",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Just one block"}
                    ]
                }
            ],
        });

        let result = anthropic_to_openai(&input).unwrap();
        assert_eq!(
            result.messages[0].content.as_deref(),
            Some("Just one block")
        );
    }

    #[test]
    fn test_anthropic_content_string_still_works() {
        // Plain string content should still work
        let input = serde_json::json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": "plain string"}
            ],
        });

        let result = anthropic_to_openai(&input).unwrap();
        assert_eq!(result.messages[0].content.as_deref(), Some("plain string"));
    }

    // ── 工具调用相关测试 ──────────────────────────────────────

    #[test]
    fn tools_top_level_maps_to_openai_functions() {
        let input = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {
                    "name": "get_weather",
                    "description": "Fetch weather",
                    "input_schema": {"type":"object","properties":{"loc":{"type":"string"}}}
                }
            ],
        });
        let r = anthropic_to_openai(&input).unwrap();
        let tools = r.tools.expect("tools should be present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(tools[0]["function"]["description"], "Fetch weather");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn tool_choice_all_three_forms() {
        for (input_choice, expected) in [
            (
                serde_json::json!({"type": "auto"}),
                serde_json::json!("auto"),
            ),
            (
                serde_json::json!({"type": "any"}),
                serde_json::json!("required"),
            ),
            (
                serde_json::json!({"type": "tool", "name": "get_x"}),
                serde_json::json!({"type":"function","function":{"name":"get_x"}}),
            ),
        ] {
            let input = serde_json::json!({
                "model": "m",
                "messages": [{"role":"user","content":"hi"}],
                "tool_choice": input_choice,
            });
            let r = anthropic_to_openai(&input).unwrap();
            assert_eq!(r.tool_choice, Some(expected));
        }
    }

    #[test]
    fn assistant_tool_use_block_maps_to_tool_calls() {
        let input = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "what's the weather"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Let me check."},
                        {
                            "type": "tool_use",
                            "id": "toolu_01",
                            "name": "get_weather",
                            "input": {"loc": "SF"}
                        }
                    ]
                }
            ],
        });
        let r = anthropic_to_openai(&input).unwrap();
        // 消息序列：user, assistant(含 text + tool_calls)
        assert_eq!(r.messages.len(), 2);
        let asst = &r.messages[1];
        assert_eq!(asst.role, "assistant");
        assert_eq!(asst.content.as_deref(), Some("Let me check."));
        let calls = asst.tool_calls.as_ref().expect("tool_calls set");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "toolu_01");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        // arguments 必须是 JSON 字符串
        let args_str = calls[0]["function"]["arguments"].as_str().unwrap();
        let args: serde_json::Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(args["loc"], "SF");
    }

    #[test]
    fn user_tool_result_block_expands_to_tool_role_messages() {
        let input = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "what's the weather"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "toolu_a", "name": "get_x", "input": {}},
                        {"type": "tool_use", "id": "toolu_b", "name": "get_y", "input": {}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_a", "content": "72F"},
                        {"type": "tool_result", "tool_use_id": "toolu_b",
                         "content": [{"type":"text","text":"cloudy"}]},
                        {"type": "text", "text": "thanks"}
                    ]
                }
            ],
        });
        let r = anthropic_to_openai(&input).unwrap();
        // user, assistant, user("thanks"), tool(a), tool(b)
        assert_eq!(r.messages.len(), 5);
        // assistant 有两个 tool_calls
        assert_eq!(r.messages[1].tool_calls.as_ref().unwrap().len(), 2);
        // 第 3 条：user 剩余文本
        assert_eq!(r.messages[2].role, "user");
        assert_eq!(r.messages[2].content.as_deref(), Some("thanks"));
        // 第 4/5 条：tool 消息
        assert_eq!(r.messages[3].role, "tool");
        assert_eq!(r.messages[3].tool_call_id.as_deref(), Some("toolu_a"));
        assert_eq!(r.messages[3].content.as_deref(), Some("72F"));
        assert_eq!(r.messages[4].role, "tool");
        assert_eq!(r.messages[4].tool_call_id.as_deref(), Some("toolu_b"));
        assert_eq!(r.messages[4].content.as_deref(), Some("cloudy"));
    }

    #[test]
    fn no_tools_serialization_matches_previous_shape() {
        // 回归保证：不带工具的请求，转换出的 ChatCompletionRequest 序列化后
        // 不多余出现 tools/tool_choice/tool_calls/tool_call_id/name 字段。
        let input = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let r = anthropic_to_openai(&input).unwrap();
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("\"tools\""), "should not emit tools: {s}");
        assert!(
            !s.contains("\"tool_choice\""),
            "should not emit tool_choice: {s}"
        );
        assert!(
            !s.contains("\"tool_calls\""),
            "should not emit tool_calls: {s}"
        );
        assert!(
            !s.contains("\"tool_call_id\""),
            "should not emit tool_call_id: {s}"
        );
    }

    #[tokio::test]
    async fn test_openai_to_anthropic_maps_stop_reason() {
        // OpenAI finish_reason "stop" 必须映射为 Anthropic 的 "end_turn"
        let openai_body = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_vec(&openai_body).unwrap()))
            .unwrap();

        let converted = convert_openai_to_anthropic_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["content"][0]["text"], "hi");
        assert_eq!(v["usage"]["input_tokens"], 3);
        assert_eq!(v["usage"]["output_tokens"], 2);
    }

    #[tokio::test]
    async fn test_openai_stream_converted_to_anthropic_events() {
        // 流式 Anthropic 请求必须收到 Anthropic SSE 事件，而不是 OpenAI chunk
        let upstream_sse = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(upstream_sse))
            .unwrap();

        let converted = convert_openai_stream_to_anthropic(resp);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(
            text.contains("event: message_start"),
            "missing message_start:\n{text}"
        );
        assert!(
            text.contains("event: content_block_delta"),
            "missing delta:\n{text}"
        );
        assert!(text.contains("\"text\":\"Hello\""), "missing text:\n{text}");
        assert!(
            text.contains("event: message_stop"),
            "missing message_stop:\n{text}"
        );
        // 不应残留 OpenAI 格式
        assert!(!text.contains("chat.completion.chunk"));
        assert!(!text.contains("[DONE]"));
    }

    // ── handle_messages 集成测试（带真实临时 DB）───────────────

    /// 构造带真实临时 DB 的 LlmState，并插入一个启用的 provider+model+api_key。
    async fn state_with_db() -> (LlmState, String, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();

        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "sk-upstream",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "fast-model", "[]", true)
            .await
            .unwrap();

        let (key, hash, prefix) = crate::server::llm::auth::generate_api_key();
        let kid = uuid::Uuid::new_v4().to_string();
        db.llm_save_api_key(&kid, &hash, &prefix, "test", None)
            .await
            .unwrap();

        (LlmState::new(Some(db), None), key, tmp)
    }

    fn authed_headers(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
        );
        headers
    }

    /// 认证失败必须写入一条 failure 记录到 llm_usage_logs。
    #[tokio::test]
    async fn test_auth_failure_is_logged() {
        let (state, _key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();

        let resp = handle_messages(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: Some(LlmProtocol::Anthropic),
            }),
            HeaderMap::new(),
            Json(serde_json::json!({"model": "deepseek-chat", "messages": [{"role":"user","content":"hi"}]})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "Anthropic 认证失败应写入一条 usage log");
        assert_eq!(logs[0].success, 0);
        assert_eq!(logs[0].status_code, 401);
        assert_eq!(logs[0].error_type.as_deref(), Some("authentication_error"));
        assert_eq!(logs[0].protocol, "anthropic");
    }

    /// 模型未找到必须写入一条 failure 记录。
    #[tokio::test]
    async fn test_model_not_found_is_logged() {
        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();

        let resp = handle_messages(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: Some(LlmProtocol::Anthropic),
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "nonexistent-model",
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "Anthropic 模型未找到应写入一条 usage log");
        assert_eq!(logs[0].success, 0);
        assert_eq!(logs[0].status_code, 404);
        assert_eq!(
            logs[0].error_type.as_deref(),
            Some("model_resolution_error")
        );
        assert_eq!(logs[0].protocol, "anthropic");
    }

    /// 上游连接失败（不可达地址）必须写入一条 failure 记录。
    #[tokio::test]
    async fn test_upstream_failure_is_logged() {
        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();

        // 把 provider base_url 改为不可达地址 → 502
        let providers = db.llm_list_providers().await.unwrap();
        let pid = &providers[0].id;
        db.llm_save_provider(
            pid,
            "DS",
            "deepseek",
            "http://127.0.0.1:1",
            "sk-upstream",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();

        let resp = handle_messages(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: Some(LlmProtocol::Anthropic),
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "deepseek-chat",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 100,
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "Anthropic 上游失败应写入一条 usage log");
        assert_eq!(logs[0].success, 0);
        assert_eq!(logs[0].status_code, 502);
        assert_eq!(logs[0].error_type.as_deref(), Some("upstream_error"));
        assert_eq!(logs[0].protocol, "anthropic");
    }

    /// v2 端到端：Anthropic 请求 → compat 改写 → 上游（mock）→
    /// 增量解析 → Anthropic SSE。验证：
    /// 1. 发往上游的 messages 末尾有 system 引导；
    /// 2. 上游返回 <tool_call> 文本时被还原为 Anthropic tool_use 事件；
    /// 3. 坏标签不泄漏。
    #[tokio::test]
    async fn test_anthropic_compat_end_to_end_with_mock_upstream() {
        use axum::routing::post;
        use axum::Router;

        // mock 上游：记录请求体，返回含 <tool_call> 的 OpenAI SSE
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(serde_json::Value::Null));
        let captured2 = captured.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = captured2.clone();
                async move {
                    *captured.lock().await = body;
                    let sse = concat!(
                        "data: {\"id\":\"c1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"我来读取\\n<tool_call>\\n{\\\"name\\\":\\\"Read\\\",\\\"arguments\\\":{\\\"path\\\":\\\"/a.txt\\\"}}\\n</tool_call>\"},\"finish_reason\":null}]}\n\n",
                        "data: {\"id\":\"c1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":10,\"total_tokens\":60}}\n\n",
                        "data: [DONE]\n\n",
                    );
                    axum::response::Response::builder()
                        .header("Content-Type", "text/event-stream")
                        .body(axum::body::Body::from(sse))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // state：provider base_url 指向 mock，开启 compat
        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
        let providers = db.llm_list_providers().await.unwrap();
        db.llm_save_provider(
            &providers[0].id,
            "DS",
            "deepseek",
            &format!("http://{addr}"),
            "sk-upstream",
            Some(r#"{"compat_tool_history": true}"#),
            None::<&str>,
            true,
        )
        .await
        .unwrap();

        let resp = handle_messages(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: Some(LlmProtocol::Anthropic),
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "deepseek-chat",
                "messages": [
                    {"role": "user", "content": "读文件"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "/old"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "t1", "content": "old content"}
                    ]}
                ],
                "max_tokens": 100,
                "stream": true
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // 1. 上游收到的请求：历史是标签文本 + 末尾引导 + 无 tool 结构
        let body = captured.lock().await.clone();
        let msgs = body["messages"].as_array().unwrap();
        let s = serde_json::to_string(&msgs).unwrap();
        assert!(!s.contains("tool_call_id"), "上游不得收到 tool 结构: {s}");
        assert!(s.contains("<tool_call>"), "历史应为标签格式: {s}");
        let tool_result_msg = msgs.iter().find(|m| {
            m["content"].as_str().map_or(false, |c| c.contains("<tool_result"))
        }).unwrap();
        assert!(tool_result_msg["content"].as_str().unwrap().contains(r#"<tool_result name="Read">"#),
            "tool_result 应有 name 属性: {tool_result_msg}");
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "system");
        assert!(last["content"].as_str().unwrap().contains("<tool_call>"),
            "末尾应有引导: {last}");

        // 2. 客户端收到 Anthropic SSE：text + tool_use，标签不泄漏
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("我来读取"), "{text}");
        assert!(text.contains("\"type\":\"tool_use\""), "{text}");
        assert!(text.contains("\"name\":\"Read\""), "{text}");
        assert!(!text.contains("tool_call>"), "标签不得泄漏: {text}");
        assert!(text.contains("event: message_stop"), "{text}");
    }
}
