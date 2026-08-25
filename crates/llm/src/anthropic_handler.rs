#[cfg(test)]
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use super::openai_handler::LlmHandlerState;
use super::{ChatCompletionRequest, ChatMessage};

/// 拆解 Anthropic 消息 content 字段的结果。
///
/// Anthropic content 允许是纯字符串或 content block 数组，块类型包含
/// `text` / `thinking` / `tool_use` / `tool_result`（本次只识别这四种）。
struct ParsedContent {
    /// 所有 `text` 块拼接后的文本；空字符串表示没有文本内容。
    text: String,
    /// assistant 历史消息里的 `thinking` 块拼接文本 → OpenAI `reasoning_content`。
    /// DeepSeek 思考模式要求历史 assistant 消息必须携带该字段回传上游。
    thinking: String,
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
    let mut thinking_parts: Vec<String> = Vec::new();
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
                    // Claude Code 走 Anthropic 协议且开思考链时，历史 assistant 消息
                    // 会携带 thinking 块；映射为 DeepSeek 的 reasoning_content 字段，
                    // 否则上游 400「reasoning_content must be passed back」。
                    // redacted_thinking（加密签名）对 DeepSeek 不可验证，忽略。
                    Some("thinking") => {
                        if let Some(t) = block.get("thinking").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                thinking_parts.push(t.to_string());
                            }
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
        thinking: thinking_parts.join("\n"),
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

    // 中段 system 降级标记：出现非 system 消息后，再出现的 system 消息
    // （如 Claude Code 每轮追加的 system-reminder）改按 user 角色发出。
    // DeepSeek 等上游会把 system 消息汇聚到 prompt 开头处理，中段 system
    // 每轮新增等于往 prompt 头部插入内容，前缀缓存从 system 块结尾就断掉
    // （实测 cache hit 只剩 system 块大小且长期固定）。降级为 user 后
    // 整段 prompt 保持纯追加，前缀缓存才能逐轮累积。
    let mut seen_non_system = false;

    for msg in raw_arr {
        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("user")
            .to_string();
        let parsed = msg
            .get("content")
            .map_or(ParsedContent {
                text: String::new(),
                thinking: String::new(),
                tool_uses: Vec::new(),
                tool_results: Vec::new(),
            }, parse_anthropic_content);

        if role.as_str() == "assistant" {
            seen_non_system = true;
            // assistant 消息：文本与 tool_use 可共存，映射到同一条 message。
            let content = if parsed.text.is_empty() {
                None
            } else {
                Some(parsed.text)
            };
            let reasoning_content = if parsed.thinking.is_empty() {
                None
            } else {
                Some(parsed.thinking)
            };
            let tool_calls = if parsed.tool_uses.is_empty() {
                None
            } else {
                Some(parsed.tool_uses)
            };
            // 只有当至少一个字段有值时才推入（防止全空消息）。
            if content.is_some() || reasoning_content.is_some() || tool_calls.is_some() {
                all_messages.push(ChatMessage {
                    role,
                    content,
                    reasoning_content,
                    tool_calls,
                    tool_call_id: None,
                    name: None,
                });
            }
        } else {
            // user / 其它角色：tool_result 展开成独立 `role="tool"` 消息，
            // 文本正常挂到 user 消息。
            //
            // 关键顺序：tool 消息必须先于 user 文本输出。
            // OpenAI 要求 assistant(tool_calls) 后紧跟 tool 消息，
            // 如果 text 先输出会插入一条 user 消息打断这个序列，
            // 导致上游返回 400（"insufficient tool messages following
            // tool_calls message"）。
            //
            // 中段 system（seen_non_system 之后）降级为 user，见上方注释。
            let effective_role = if role == "system" && seen_non_system {
                "user"
            } else {
                role.as_str()
            };
            if role != "system" {
                seen_non_system = true;
            }
            for tr in parsed.tool_results {
                all_messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(tr.content),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: Some(tr.tool_call_id),
                    name: None,
                });
            }
            if !parsed.text.is_empty() {
                all_messages.push(ChatMessage::text(effective_role, parsed.text));
            }
        }
    }

    let stream = body
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let tools = body
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| anthropic_tools_to_openai(arr.as_slice()))
        .filter(|v| !v.is_empty());

    let tool_choice = body
        .get("tool_choice")
        .and_then(anthropic_tool_choice_to_openai);

    // Anthropic → OpenAI：仅透传已知安全的 stop 参数,其余 Anthropic 原生字段
    // (metadata/thinking 等)不上 OpenAI 端点。raw_body 是裁剪后的安全 body,
    // 由 build_upstream_body 透传；messages 在后续 RAG/compat 改写后回写。
    // 转换字段(max_tokens/temperature/top_p/tools/tool_choice)必须继续透传,
    // 否则透传模式下 build_upstream_body 不会补它们(它只覆盖 model/stream_options)。
    // "model" 值会被 build_upstream_body 用 request.model(别名解析后)覆盖,此处仅为占位。
    let mut passthrough = serde_json::json!({
        "model": anthropic_model,
        "messages": all_messages.clone(),
        "stream": stream,
    });
    // 显式 null 按旧重建路径语义忽略(as_u64/as_f64 对 null 取 None),避免 null 敏感上游。
    if let Some(v) = body.get("max_tokens").filter(|v| !v.is_null()) {
        passthrough["max_tokens"] = v.clone();
    }
    if let Some(v) = body.get("temperature").filter(|v| !v.is_null()) {
        passthrough["temperature"] = v.clone();
    }
    if let Some(v) = body.get("top_p").filter(|v| !v.is_null()) {
        passthrough["top_p"] = v.clone();
    }
    if let Some(tools) = &tools {
        passthrough["tools"] = serde_json::Value::Array(tools.clone());
    }
    if let Some(choice) = &tool_choice {
        passthrough["tool_choice"] = choice.clone();
    }
    if let Some(stops) = body.get("stop_sequences").filter(|v| !v.is_null()) {
        passthrough["stop"] = stops.clone();
    }

    Ok(ChatCompletionRequest {
        model: anthropic_model.to_string(),
        messages: all_messages,
        stream,
        max_tokens: body
            .get("max_tokens")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as u32),
        temperature: body
            .get("temperature")
            .and_then(serde_json::Value::as_f64)
            .map(|v| v as f32),
        top_p: body.get("top_p").and_then(serde_json::Value::as_f64).map(|v| v as f32),
        tools,
        tool_choice,
        raw_body: Some(passthrough),
    })
}

/// POST /v1/messages — Anthropic Messages API。
///
/// 候选链内每候选独立判定发送策略：候选 provider 配置了 `anthropic_base_url` 时，
/// 直接透传原始 Anthropic 请求到上游（/v1/messages，不做格式转换）；否则回退到
/// OpenAI 格式转换路径。直通失败计入 breaker/known-failures 并继续 failover 下一候选。
pub async fn handle_messages(
    State(state): State<LlmHandlerState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // Validate API key
    let (api_key_id, api_key_name) =
        match super::pipeline::authenticate_or_reject(&state, &headers, "anthropic").await {
            Ok(a) => a,
            Err(resp) => return resp,
        };

    // 提取 model 名用于路由解析
    let model = match super::pipeline::extract_model_or_reject(
        &state,
        &body,
        &api_key_id,
        &api_key_name,
        "anthropic",
    )
    .await
    {
        Ok(m) => m,
        Err(resp) => return resp,
    };

    // Resolve model → 候选链（模型组故障转移）
    let chain = match super::pipeline::resolve_chain_or_reject(
        &state,
        &model,
        &api_key_id,
        &api_key_name,
        "anthropic",
    )
    .await
    {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    // 首选候选：provider 级配置（compat 开关）以首选为准——RAG/compat 改写在循环外只做
    // 一次（作用于转换路径的 OpenAI 请求体）；`execute_with_failover` 循环内每候选独立
    // 判定：配了 anthropic_base_url 的候选直发原始 Anthropic body，否则走转换分支。
    let first_candidate = chain.candidates[0].clone();
    let provider = first_candidate.provider.clone();
    let actual_model = first_candidate.model_name.clone();
    let model_id = first_candidate.model_id.clone();

    let is_stream = body
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // 用量采集上下文
    // 仅 rag feature 启用时使用；feature 关闭时保留绑定并允许未使用。
    #[cfg_attr(not(feature = "rag"), allow(unused_variables))]
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
        failover_from: None,
    };
    let started = std::time::Instant::now();
    let db = state.llm.db.clone();

    // ── 执行：候选链故障转移。直通（anthropic_base_url）判定下沉为循环内 per-candidate
    // 发送策略：原始 body 存入 PreparedRequest.anthropic_body，`execute_with_failover`
    // 内每候选独立选择直通 /v1/messages 或转换路径，失败互相 failover。 ──

    // ── 回退路径：转成 OpenAI 格式发到 base_url ──
    let request = match anthropic_to_openai(&body) {
        Ok(r) => r,
        Err(e) => {
            return state.error_for_protocol(StatusCode::BAD_REQUEST, e, "invalid_request_error")
        }
    };

    let mut request = request;
    request.model = actual_model;

    let compat_enabled =
        super::compat::compat_tool_history_enabled(provider.extra_config.as_deref());

    // RAG：API key 绑定知识库时，检索背景资料注入 messages[0]（compat 之前）。
    // 注入作用于转换路径的 OpenAI 请求体；直通候选发送的原始 Anthropic body
    // （PreparedRequest.anthropic_body）不含注入内容 —— 规格边界，保持现状。
    let kb_id_for_rag: Option<String> = {
        #[cfg(feature = "rag")]
        {
            if let Some(ref db) = db {
                db.rag_get_kb_id_for_api_key(&api_key_id_for_rag)
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            }
        }
        #[cfg(not(feature = "rag"))]
        {
            None
        }
    };
    super::pipeline::inject_rag_and_compat(
        &state.llm,
        db.as_ref(),
        kb_id_for_rag,
        compat_enabled,
        &mut request,
        &mut ctx,
    )
    .await;

    let message_count = request.messages.len();
    let has_tools = request.tools.is_some();
    let prepared = super::pipeline::PreparedRequest {
        request,
        message_count,
        has_tools,
        compat_enabled,
        // 原始 Anthropic 请求体：配了 anthropic_base_url 的候选用它直发 /v1/messages
        // （循环内替换 model 为候选真实名），其余候选仍走下方转换分支。
        anthropic_body: Some(body),
    };
    super::pipeline::run_execution(
        &state,
        "anthropic",
        prepared,
        &chain,
        &first_candidate.model_name,
        ctx,
        db,
        started,
        super::pipeline::ResponsePostProcess::ToAnthropic,
    )
    .await
}

/// 测试专用：暴露流式 OpenAI→Anthropic 转换，供跨模块端到端测试使用。
#[cfg(test)]
pub(crate) fn convert_openai_stream_to_anthropic_for_test(openai_resp: Response) -> Response {
    super::format::convert_openai_stream_to_anthropic(openai_resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LlmProtocol, LlmState};

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
    fn test_anthropic_to_openai_mid_conversation_system_demoted_to_user() {
        // Claude Code（claude-vscode 入口）每轮会往 messages 里追加 role=system
        // 的 reminder。中段 system 若原样发给上游，DeepSeek 等会把 system 汇聚到
        // prompt 开头处理——每轮新增等于往头部插内容，前缀缓存全断。
        // 必须降级为 user（开头连续的 system 保持 system 不变）。
        let input = serde_json::json!({
            "model": "claude-3-opus",
            "messages": [
                {"role": "system", "content": "leading-sys"},
                {"role": "user", "content": "q1"},
                {"role": "assistant", "content": "a1"},
                {"role": "system", "content": "<total_tokens>100 left</total_tokens>"},
                {"role": "user", "content": "q2"}
            ],
        });

        let result = anthropic_to_openai(&input).unwrap();
        let roles: Vec<&str> = result.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "user", "user"]);
        assert_eq!(
            result.messages[3].content.as_deref(),
            Some("<total_tokens>100 left</total_tokens>")
        );
    }

    #[test]
    fn test_anthropic_to_openai_leading_systems_keep_role() {
        // 顶层 system 字段 + messages 开头连续的 system 都保持 system 角色
        let input = serde_json::json!({
            "model": "m",
            "system": "top-sys",
            "messages": [
                {"role": "system", "content": "second-sys"},
                {"role": "user", "content": "hi"}
            ],
        });
        let result = anthropic_to_openai(&input).unwrap();
        let roles: Vec<&str> = result.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "system", "user"]);
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
    fn anthropic_stop_sequences_maps_to_openai_stop() {
        let body = serde_json::json!({
            "model": "claude-3-haiku",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "stop_sequences": ["\n\n", "</s>"],
        });
        let req = anthropic_to_openai(&body).unwrap();
        // ChatCompletionRequest 无 stop 字段,此处断言重构后的 build_upstream_body 输出。
        let out = crate::upstream::build_upstream_body(&req);
        assert_eq!(out["stop"], serde_json::json!(["\n\n", "</s>"]));
    }

    #[test]
    fn anthropic_fallback_preserves_converted_fields() {
        let body = serde_json::json!({
            "model": "claude-3-haiku",
            "max_tokens": 100,
            "temperature": 0.3,
            "top_p": 0.9,
            "messages": [{"role": "user", "content": "hi"}],
            "stop_sequences": ["\n\n"],
            "tools": [{"type": "custom", "name": "t"}],
        });
        let req = anthropic_to_openai(&body).unwrap();
        let out = crate::upstream::build_upstream_body(&req);
        assert_eq!(out["max_tokens"], 100);
        assert_eq!(out["temperature"], 0.3);
        assert_eq!(out["top_p"], 0.9);
        assert_eq!(out["stop"], serde_json::json!(["\n\n"]));
        assert!(out["tools"].is_array(), "tools must not be dropped: {out}");
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
        // 期望顺序：user, assistant, tool(a), tool(b), user("thanks")
        // tool 消息必须紧跟 assistant(tool_calls)，user 文本排在最后
        assert_eq!(r.messages.len(), 5);
        // assistant 有两个 tool_calls
        assert_eq!(r.messages[1].tool_calls.as_ref().unwrap().len(), 2);
        // 第 3/4 条：tool 消息紧跟 assistant
        assert_eq!(r.messages[2].role, "tool");
        assert_eq!(r.messages[2].tool_call_id.as_deref(), Some("toolu_a"));
        assert_eq!(r.messages[2].content.as_deref(), Some("72F"));
        assert_eq!(r.messages[3].role, "tool");
        assert_eq!(r.messages[3].tool_call_id.as_deref(), Some("toolu_b"));
        assert_eq!(r.messages[3].content.as_deref(), Some("cloudy"));
        // 第 5 条：user 剩余文本排在 tool 消息之后
        assert_eq!(r.messages[4].role, "user");
        assert_eq!(r.messages[4].content.as_deref(), Some("thanks"));
    }

    #[test]
    fn assistant_thinking_block_maps_to_reasoning_content() {
        // 回归：Claude Code 开思考链时，历史 assistant 消息携带 thinking 块；
        // 必须映射为 DeepSeek 的 reasoning_content 字段，否则上游 400
        // 「The reasoning_content in the thinking mode must be passed back」。
        let input = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "让我想想……", "signature": "sig"},
                        {"type": "text", "text": "答案"},
                        {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "/a"}}
                    ]
                }
            ],
        });
        let r = anthropic_to_openai(&input).unwrap();
        assert_eq!(r.messages.len(), 2);
        let asst = &r.messages[1];
        assert_eq!(
            asst.reasoning_content.as_deref(),
            Some("让我想想……"),
            "thinking 块必须映射为 reasoning_content"
        );
        assert_eq!(asst.content.as_deref(), Some("答案"));
        assert_eq!(asst.tool_calls.as_ref().unwrap().len(), 1);

        // 序列化进上游请求体时必须带 reasoning_content 字段
        let out = crate::upstream::build_upstream_body(&r);
        assert_eq!(
            out["messages"][1]["reasoning_content"].as_str(),
            Some("让我想想……"),
            "上行 body 必须携带 reasoning_content: {out}"
        );
    }

    #[test]
    fn assistant_without_thinking_has_no_reasoning_content() {
        // 普通 assistant 消息不得输出 reasoning_content 字段（避免 null 敏感上游）。
        let input = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "plain answer"}
            ],
        });
        let r = anthropic_to_openai(&input).unwrap();
        let out = crate::upstream::build_upstream_body(&r);
        assert!(
            out["messages"][1].get("reasoning_content").is_none(),
            "无 thinking 时不得输出 reasoning_content: {out}"
        );
    }

    #[test]
    fn redacted_thinking_block_is_ignored() {
        // redacted_thinking 只有加密签名、无可回传文本，忽略后消息其余部分照常转换。
        let input = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "redacted_thinking", "data": "opaque-blob"},
                        {"type": "text", "text": "答案"}
                    ]
                }
            ],
        });
        let r = anthropic_to_openai(&input).unwrap();
        assert_eq!(r.messages.len(), 2);
        assert_eq!(r.messages[1].reasoning_content, None);
        assert_eq!(r.messages[1].content.as_deref(), Some("答案"));
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

        let converted = crate::format::convert_openai_to_anthropic_response(resp).await;
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
    async fn test_openai_to_anthropic_response_oversized_returns_502() {
        // 上游响应超过转换上限（16MB）时，不能静默返回 200 空 body。
        // 应返回 502 并带错误说明，避免客户端拿到"成功但无内容"的假象。
        let oversized = "x".repeat(20 * 1024 * 1024); // 20MB，超过 16MB 上限
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(oversized))
            .unwrap();

        let converted = crate::format::convert_openai_to_anthropic_response(resp).await;
        assert_eq!(converted.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            text.contains("failed to read upstream"),
            "expected error message about upstream read failure, got: {text}"
        );
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

        let converted = crate::format::convert_openai_stream_to_anthropic(resp);
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
        let db = rust_tunnel_persistence::Database::new(tmp.path().join("t.db").to_str().unwrap())
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
        db.llm_save_model(&mid, &pid, "deepseek-chat", "fast-model", "[]", true, None)
            .await
            .unwrap();

        let (key, hash, prefix) = crate::auth::generate_api_key();
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

    /// 端到端（进程内）：组 [坏候选 500 → 好候选 200]，非流式请求应转移成功，
    /// 且最终响应来自好候选；usage log 记录 failover_from = 首选坏候选、
    /// model_name = 实际出账好候选。转换路径上游是 OpenAI 协议（chat.completion）。
    #[tokio::test]
    async fn test_convert_path_failover() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 坏候选：一律 500
        let bad_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bad_addr = bad_listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = bad_listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let _ = s.read(&mut buf).await;
                    let body = "{\"error\":\"boom\"}";
                    let resp = format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        // 好候选：返回 OpenAI chat.completion 非流式 JSON（转换路径上游是 OpenAI 协议）
        let good_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good_addr = good_listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = good_listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let _ = s.read(&mut buf).await;
                    let body = serde_json::json!({
                        "id": "chatcmpl-1", "object": "chat.completion",
                        "choices": [{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                    }).to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });

        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
        // 两个 provider 各一个模型（无 anthropic_base_url → 走转换路径）
        db.llm_save_provider(
            "p-bad",
            "Bad",
            "deepseek",
            &format!("http://{bad_addr}"),
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        db.llm_save_provider(
            "p-good",
            "Good",
            "deepseek",
            &format!("http://{good_addr}"),
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m-bad", "p-bad", "bad-model", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_save_model("m-good", "p-good", "good-model", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m-bad".into(), 1), ("m-good".into(), 2)])
            .await
            .unwrap();

        let resp = handle_messages(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: Some(LlmProtocol::Anthropic),
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "router", "max_tokens": 8,
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        // 用量日志：failover_from = 首选坏候选，model_name = 实际出账好候选
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        let last = logs.first().expect("usage log");
        assert_eq!(last.failover_from.as_deref(), Some("bad-model"));
        assert_eq!(last.model_name, "good-model");
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
        let tool_result_msg = msgs
            .iter()
            .find(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("<tool_result"))
            })
            .unwrap();
        assert!(
            tool_result_msg["content"]
                .as_str()
                .unwrap()
                .contains(r#"<tool_result name="Read">"#),
            "tool_result 应有 name 属性: {tool_result_msg}"
        );
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "system");
        assert!(
            last["content"].as_str().unwrap().contains("<tool_call>"),
            "末尾应有引导: {last}"
        );

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

    /// 构造带 RAG 的 LlmState（与 openai_handler 测试等价的 helper）：真实临时
    /// DB + VectorStore（tempdir）+ KB + chunk + api key 绑 KB。`emb_base_url`
    /// 由调用点启动的 mock embedding server 提供（返回固定 8 维向量）。
    #[cfg(feature = "rag")]
    async fn state_with_rag(emb_base_url: &str) -> (LlmState, String, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = rust_tunnel_persistence::Database::new(tmp.path().join("t.db").to_str().unwrap())
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
        db.llm_save_model(&mid, &pid, "deepseek-chat", "fast-model", "[]", true, None)
            .await
            .unwrap();

        // 知识库 + 一个分块（向量随后 upsert 进 store）
        let kb_id = uuid::Uuid::new_v4().to_string();
        db.rag_create_kb(
            &kb_id,
            "rag-kb",
            "",
            emb_base_url,
            "sk-emb",
            "emb-model",
            8,
            5,
            1000,
            0,
            0.0,
            true,
        )
        .await
        .unwrap();
        let chunk_id = uuid::Uuid::new_v4().to_string();
        let doc_id = uuid::Uuid::new_v4().to_string();
        db.rag_create_document(&doc_id, &kb_id, "install.md", "hash", "md")
            .await
            .unwrap();
        db.rag_insert_chunks(&[(
            chunk_id.clone(),
            doc_id.clone(),
            kb_id.clone(),
            0,
            "指南/安装".into(),
            "RAG 知识库测试内容".into(),
            8,
        )])
        .await
        .unwrap();

        // API key 绑定该知识库
        let (key, hash, prefix) = crate::auth::generate_api_key();
        let kid = uuid::Uuid::new_v4().to_string();
        db.llm_save_api_key(&kid, &hash, &prefix, "rag-test", Some(&kb_id))
            .await
            .unwrap();

        let state = LlmState::new_with_rag(Some(db), None, tmp.path());
        state
            .rag_store
            .upsert(
                &kb_id,
                8,
                vec![crate::rag::store::ChunkPoint {
                    id: chunk_id,
                    vector: vec![0.1f32; 8],
                    doc_id,
                    seq: 0,
                    heading_path: "指南/安装".into(),
                }],
            )
            .await
            .unwrap();

        (state, key, tmp)
    }

    /// 回退路径注入端到端：api key 绑 KB → Anthropic 请求 → 回退路径转 OpenAI 格式
    /// → RAG 注入 messages[0]（含 `<knowledge_base>`）→ 上游（OpenAI 格式）收到；
    /// usage log 记录 rag_chunks_injected=1。复刻 openai_handler 的 rag 注入测试。
    #[cfg(feature = "rag")]
    #[tokio::test]
    async fn anthropic_fallback_injects_knowledge_base() {
        use axum::routing::post;
        use axum::Router;

        // mock embedding server：任意输入返回固定 8 维向量
        let emb_app = Router::new().route(
            "/embeddings",
            post(|body: axum::Json<serde_json::Value>| async move {
                let n = body["input"].as_array().map_or(1, std::vec::Vec::len);
                let data: Vec<_> = (0..n)
                    .map(|i| {
                        serde_json::json!({
                            "index": i,
                            "embedding": vec![0.1f32; 8],
                            "object": "embedding"
                        })
                    })
                    .collect();
                axum::Json(serde_json::json!({"object": "list", "data": data}))
            }),
        );
        let emb_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let emb_addr = emb_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(emb_listener, emb_app).await.unwrap();
        });

        // mock upstream（OpenAI 格式）：回显请求体。回退路径的 call_upstream 指向它。
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(serde_json::Value::Null));
        let captured2 = captured.clone();
        let llm_app = Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = captured2.clone();
                async move {
                    *captured.lock().await = body;
                    axum::Json(serde_json::json!({
                        "id": "chatcmpl-rag-a",
                        "object": "chat.completion",
                        "model": "deepseek-chat",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "基于资料的回答"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
                    }))
                }
            }),
        );
        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(llm_listener, llm_app).await.unwrap();
        });

        let (state, key, _tmp) = state_with_rag(&format!("http://{emb_addr}")).await;
        let db = state.db.clone().unwrap();

        // provider 无 anthropic_base_url → 走回退路径；base_url 指向 mock upstream
        let providers = db.llm_list_providers().await.unwrap();
        db.llm_save_provider(
            &providers[0].id,
            "DS",
            "deepseek",
            &format!("http://{llm_addr}"),
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
                "messages": [{"role": "user", "content": "关于知识库的问题"}],
                "max_tokens": 100
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // 上游（OpenAI 格式）收到的 messages[0] 是注入的 system 消息
        let body = captured.lock().await.clone();
        let msgs = body["messages"]
            .as_array()
            .expect("upstream should receive messages");
        assert_eq!(
            msgs[0]["role"], "system",
            "messages[0] 应为注入的 system: {body}"
        );
        let sys = msgs[0]["content"].as_str().expect("system content");
        assert!(
            sys.contains("<knowledge_base>"),
            "system 应含 <knowledge_base>: {sys}"
        );
        assert!(
            sys.contains("RAG 知识库测试内容"),
            "system 应含 chunk 内容: {sys}"
        );

        // usage log 记录 rag_chunks_injected = 1（fire-and-forget 写入，稍等）
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "应有一条成功的 usage log");
        assert_eq!(logs[0].rag_chunks_injected, Some(1));
    }

    // ── Anthropic 直通（anthropic_base_url）集成测试 ────────────────────────

    /// 起一个裸 TCP mock，每次连接返回固定响应；捕获完整请求文本（请求行/头/体）与调用次数。
    async fn start_tcp_mock(
        status: &'static str,
        content_type: &'static str,
        resp_body: serde_json::Value,
    ) -> (
        String,
        std::sync::Arc<tokio::sync::Mutex<String>>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let c1 = captured.clone();
        let h1 = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let captured = c1.clone();
                let hits = h1.clone();
                let resp_body = resp_body.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 16384];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n > 0 {
                        *captured.lock().await = String::from_utf8_lossy(&buf[..n]).to_string();
                    }
                    let body = resp_body.to_string();
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), captured, hits)
    }

    /// 从捕获的原始 HTTP 请求文本提取 JSON body。
    fn mock_body_from(req: &str) -> serde_json::Value {
        req.split("\r\n\r\n")
            .nth(1)
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    /// 单候选直通：provider 配 anthropic_base_url → 请求直发 /v1/messages（原始 Anthropic body，
    /// 带 anthropic-version/x-api-key 头），返回 Anthropic 格式响应，且不被二次转换为 OpenAI 格式。
    #[tokio::test]
    async fn test_anthropic_passthrough_single_candidate() {
        use std::sync::atomic::Ordering;

        let (mock_url, captured, hits) = start_tcp_mock(
            "200 OK",
            "application/json",
            serde_json::json!({
                "id": "msg_direct_1",
                "type": "message",
                "role": "assistant",
                "model": "deepseek-chat",
                "content": [{"type":"text","text":"直通回答"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 2, "output_tokens": 3}
            }),
        )
        .await;

        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
        // provider 配 anthropic_base_url 指向 mock；base_url 不可达（不应被转换路径使用）
        let providers = db.llm_list_providers().await.unwrap();
        db.llm_save_provider(
            &providers[0].id,
            "DS",
            "deepseek",
            "http://127.0.0.1:1",
            "sk-upstream",
            None::<&str>,
            Some(&mock_url),
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
                "max_tokens": 8,
                "metadata": {"user_id": "t1"},
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "应恰好一次直通调用");
        // 请求行命中 /v1/messages；带 Anthropic 原生头
        let req = captured.lock().await.clone();
        let first_line = req.lines().next().unwrap_or("");
        assert!(
            first_line.contains("/v1/messages"),
            "应直发 /v1/messages: {first_line}"
        );
        assert!(
            req.contains("anthropic-version: 2023-06-01"),
            "缺 anthropic-version: {req}"
        );
        assert!(
            req.to_lowercase().contains("x-api-key"),
            "缺 x-api-key: {req}"
        );
        // 发送体 = 原始 Anthropic body：metadata 保留、model 替换为真实模型名（未被转成 OpenAI 格式）
        let body = mock_body_from(&req);
        assert_eq!(
            body["model"], "deepseek-chat",
            "直通 body model 应为真实模型名: {body}"
        );
        assert_eq!(
            body["metadata"]["user_id"], "t1",
            "metadata 应原样保留（未走转换）: {body}"
        );
        assert_eq!(body["max_tokens"], 8);
        // 响应为 Anthropic 格式，未被二次转换
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "message", "响应应为 Anthropic message 格式: {v}");
        assert_eq!(v["content"][0]["text"], "直通回答");
        assert_eq!(v["stop_reason"], "end_turn");
        assert!(
            v.get("choices").is_none(),
            "不应有 choices（未二次转换）: {v}"
        );
    }

    /// 多候选直通：组 [A(直通), B(直通)]，首选 A 被直通调用（/v1/messages + 原始 Anthropic body +
    /// Anthropic 格式响应），响应不二次转换；首选成功时备选不被调用。
    #[tokio::test]
    async fn test_anthropic_passthrough_group_both_direct_first_success() {
        use std::sync::atomic::Ordering;

        let (a_url, a_captured, a_hits) = start_tcp_mock(
            "200 OK",
            "application/json",
            serde_json::json!({
                "id": "msg_a",
                "type": "message",
                "role": "assistant",
                "content": [{"type":"text","text":"直通 A"}],
                "stop_reason": "end_turn"
            }),
        )
        .await;
        let (b_url, _b_captured, b_hits) = start_tcp_mock(
            "200 OK",
            "application/json",
            serde_json::json!({"type": "message", "content": []}),
        )
        .await;

        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
        // 两个 provider 各一个模型，都配 anthropic_base_url（base_url 不可达）
        db.llm_save_provider(
            "p-a",
            "A",
            "deepseek",
            "http://127.0.0.1:1",
            "k",
            None::<&str>,
            Some(&a_url),
            true,
        )
        .await
        .unwrap();
        db.llm_save_provider(
            "p-b",
            "B",
            "deepseek",
            "http://127.0.0.1:1",
            "k",
            None::<&str>,
            Some(&b_url),
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m-a", "p-a", "model-a", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_save_model("m-b", "p-b", "model-b", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m-a".into(), 1), ("m-b".into(), 2)])
            .await
            .unwrap();

        let resp = handle_messages(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: Some(LlmProtocol::Anthropic),
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "router",
                "max_tokens": 8,
                "metadata": {"user_id": "t1"},
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(a_hits.load(Ordering::SeqCst), 1, "首选应被直通调用一次");
        assert_eq!(b_hits.load(Ordering::SeqCst), 0, "首选成功时备选不应被调用");
        // 首选收到原始 Anthropic body，model 替换为候选真实名 model-a
        let req = a_captured.lock().await.clone();
        let first_line = req.lines().next().unwrap_or("");
        assert!(
            first_line.contains("/v1/messages"),
            "首选应直发 /v1/messages: {first_line}"
        );
        let body = mock_body_from(&req);
        assert_eq!(
            body["model"], "model-a",
            "直通 body model 应为候选名: {body}"
        );
        assert_eq!(body["metadata"]["user_id"], "t1");
        assert_eq!(body["max_tokens"], 8);
        // 响应为 Anthropic 格式（未被二次转换）
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "直通 A");
        assert!(
            v.get("choices").is_none(),
            "不应二次转换为 OpenAI 格式: {v}"
        );
    }

    /// 多候选直通 failover：首选直通 404（确定性失败 → 记 known-failures）自动尝试第二候选
    /// （同为直通）并成功返回 Anthropic 格式。
    #[tokio::test]
    async fn test_anthropic_passthrough_group_first_404_failover() {
        use std::sync::atomic::Ordering;

        let (a_url, _a_captured, a_hits) = start_tcp_mock(
            "404 Not Found",
            "application/json",
            serde_json::json!({
                "type": "error",
                "error": {"type": "not_found_error", "message": "model not found"}
            }),
        )
        .await;
        let (b_url, b_captured, b_hits) = start_tcp_mock(
            "200 OK",
            "application/json",
            serde_json::json!({
                "id": "msg_b",
                "type": "message",
                "role": "assistant",
                "content": [{"type":"text","text":"备选直通"}],
                "stop_reason": "end_turn"
            }),
        )
        .await;

        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
        db.llm_save_provider(
            "p-a",
            "A",
            "deepseek",
            "http://127.0.0.1:1",
            "k",
            None::<&str>,
            Some(&a_url),
            true,
        )
        .await
        .unwrap();
        db.llm_save_provider(
            "p-b",
            "B",
            "deepseek",
            "http://127.0.0.1:1",
            "k",
            None::<&str>,
            Some(&b_url),
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m-a", "p-a", "model-a", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_save_model("m-b", "p-b", "model-b", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m-a".into(), 1), ("m-b".into(), 2)])
            .await
            .unwrap();

        let resp = handle_messages(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: Some(LlmProtocol::Anthropic),
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "router",
                "max_tokens": 8,
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(a_hits.load(Ordering::SeqCst), 1, "首选直通被调用一次后 404");
        assert_eq!(b_hits.load(Ordering::SeqCst), 1, "404 后自动转移到备选直通");
        // 备选也走 /v1/messages 直通，body model 替换为候选名 model-b
        let req = b_captured.lock().await.clone();
        let first_line = req.lines().next().unwrap_or("");
        assert!(
            first_line.contains("/v1/messages"),
            "备选也应直发 /v1/messages: {first_line}"
        );
        let body = mock_body_from(&req);
        assert_eq!(
            body["model"], "model-b",
            "备选直通 body model 应为候选名: {body}"
        );
        // 最终响应 Anthropic 格式
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "备选直通");
        assert!(
            v.get("choices").is_none(),
            "不应二次转换为 OpenAI 格式: {v}"
        );
    }

    /// 混合链：首选直通（anthropic_base_url）404，备选无 anthropic_base_url 走转换路径成功。
    /// 最终响应为 Anthropic 格式（转换路径 post_process ToAnthropic），usage 记 failover_from。
    #[tokio::test]
    async fn test_anthropic_mixed_chain_passthrough_404_then_convert() {
        use std::sync::atomic::Ordering;

        let (a_url, _a_captured, a_hits) = start_tcp_mock(
            "404 Not Found",
            "application/json",
            serde_json::json!({
                "type": "error",
                "error": {"type": "not_found_error", "message": "nf"}
            }),
        )
        .await;
        let (b_url, b_captured, b_hits) = start_tcp_mock(
            "200 OK",
            "application/json",
            serde_json::json!({
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "转换回答"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }),
        )
        .await;

        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
        // p-a：anthropic_base_url → 直通 mock（404）；p-b：无 anthropic_base_url，base_url → 转换 mock
        db.llm_save_provider(
            "p-a",
            "A",
            "deepseek",
            "http://127.0.0.1:1",
            "k",
            None::<&str>,
            Some(&a_url),
            true,
        )
        .await
        .unwrap();
        db.llm_save_provider(
            "p-b",
            "B",
            "deepseek",
            &b_url,
            "k",
            None::<&str>,
            None,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m-a", "p-a", "model-a", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_save_model("m-b", "p-b", "model-b", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m-a".into(), 1), ("m-b".into(), 2)])
            .await
            .unwrap();

        let resp = handle_messages(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: Some(LlmProtocol::Anthropic),
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "router",
                "max_tokens": 8,
                "metadata": {"user_id": "t1"},
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(a_hits.load(Ordering::SeqCst), 1, "首选直通被调用并 404");
        assert_eq!(b_hits.load(Ordering::SeqCst), 1, "404 后转移到转换路径备选");
        // 备选收到 OpenAI 格式请求（/v1/chat/completions）——回归 body/端点错配：
        // 链上存在直通候选时，openai_body 不得被整包换成原始 Anthropic body。
        // 原始 Anthropic body 含 metadata（请求带 metadata 字段），转换路径会剥掉它；
        // OpenAI body 恒含 stream（build_upstream_body 注入），原始 Anthropic body 无（请求未带）。
        let req = b_captured.lock().await.clone();
        let first_line = req.lines().next().unwrap_or("");
        assert!(
            first_line.contains("/v1/chat/completions"),
            "备选应走转换路径: {first_line}"
        );
        let body = mock_body_from(&req);
        assert!(
            body.get("metadata").is_none(),
            "转换候选应收到 OpenAI body（metadata 被剥离）: {body}"
        );
        assert!(
            body.get("stream").is_some(),
            "OpenAI body 应含 stream 字段: {body}"
        );
        // 最终响应为 Anthropic 格式（转换路径 post_process ToAnthropic）
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "转换回答");
        assert_eq!(v["stop_reason"], "end_turn");
        // usage 归因：failover_from = 首选直通模型，model_name = 实际出账转换模型
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        let last = logs.first().expect("usage log");
        assert_eq!(last.failover_from.as_deref(), Some("model-a"));
        assert_eq!(last.model_name, "model-b");
    }

    /// 混合链（反向顺序）：首选无 anthropic_base_url 走转换路径（500 可转移），
    /// 备选有 anthropic_base_url 直通成功。回归 body/端点错配根因：链上有直通候选时，
    /// openai_body 曾被整包换成原始 Anthropic body，导致无 anthropic_base_url 的转换候选
    /// 拿 Anthropic 格式 body 打 /v1/chat/completions（上游 400）——用户 opencode zen 实证场景。
    #[tokio::test]
    async fn test_anthropic_mixed_chain_convert_then_passthrough() {
        use std::sync::atomic::Ordering;

        // p-a（首选，转换）：返回 500（server error → retryable，触发转移到备选）
        let (a_url, a_captured, a_hits) = start_tcp_mock(
            "500 Internal Server Error",
            "application/json",
            serde_json::json!({
                "error": {"message": "boom", "type": "server_error"}
            }),
        )
        .await;
        // p-b（备选，直通）：返回 Anthropic 格式 200
        let (b_url, b_captured, b_hits) = start_tcp_mock(
            "200 OK",
            "application/json",
            serde_json::json!({
                "id": "msg_b",
                "type": "message",
                "role": "assistant",
                "model": "model-b",
                "content": [{"type":"text","text":"直通回答"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
        )
        .await;

        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
        // p-a：无 anthropic_base_url，base_url → 转换 mock；p-b：anthropic_base_url → 直通 mock
        db.llm_save_provider(
            "p-a",
            "A",
            "deepseek",
            &a_url,
            "k",
            None::<&str>,
            None,
            true,
        )
        .await
        .unwrap();
        db.llm_save_provider(
            "p-b",
            "B",
            "deepseek",
            "http://127.0.0.1:1",
            "k",
            None::<&str>,
            Some(&b_url),
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m-a", "p-a", "model-a", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_save_model("m-b", "p-b", "model-b", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_create_model_group("g2", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g2", &[("m-a".into(), 1), ("m-b".into(), 2)])
            .await
            .unwrap();

        let resp = handle_messages(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: Some(LlmProtocol::Anthropic),
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "router",
                "max_tokens": 8,
                "metadata": {"user_id": "t1"},
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(a_hits.load(Ordering::SeqCst), 1, "首选转换候选被调用并 500");
        assert_eq!(b_hits.load(Ordering::SeqCst), 1, "500 后转移到直通备选");
        // 首选（无 anthropic_base_url）必须收到 OpenAI 格式 body 打 /v1/chat/completions
        let a_req = a_captured.lock().await.clone();
        let a_first = a_req.lines().next().unwrap_or("");
        assert!(
            a_first.contains("/v1/chat/completions"),
            "无 anthropic_base_url 候选应走转换路径: {a_first}"
        );
        let a_body = mock_body_from(&a_req);
        assert!(
            a_body.get("metadata").is_none(),
            "首选转换候选应收到 OpenAI body（metadata 被剥离，而非原始 Anthropic body）: {a_body}"
        );
        assert!(
            a_body.get("stream").is_some(),
            "首选 OpenAI body 应含 stream 字段: {a_body}"
        );
        // 备选（有 anthropic_base_url）必须收到原始 Anthropic body 打 /v1/messages
        let b_req = b_captured.lock().await.clone();
        let b_first = b_req.lines().next().unwrap_or("");
        assert!(
            b_first.contains("/v1/messages"),
            "配 anthropic_base_url 候选应直通 /v1/messages: {b_first}"
        );
        let b_body = mock_body_from(&b_req);
        assert_eq!(
            b_body["model"], "model-b",
            "直通 body 的 model 应替换为候选名: {b_body}"
        );
        assert_eq!(
            b_body["max_tokens"], 8,
            "直通 body 保留原始 Anthropic max_tokens: {b_body}"
        );
        assert_eq!(
            b_body["metadata"]["user_id"], "t1",
            "直通 body 保留原始 Anthropic metadata: {b_body}"
        );
        // 最终响应为 Anthropic 格式（直通成功 → 不再二次转换）
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "直通回答");
        // usage 归因：failover_from = 首选转换模型，model_name = 实际出账直通模型
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        let last = logs.first().expect("usage log");
        assert_eq!(last.failover_from.as_deref(), Some("model-a"));
        assert_eq!(last.model_name, "model-b");
    }
}
