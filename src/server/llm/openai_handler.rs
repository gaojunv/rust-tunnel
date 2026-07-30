use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::router::{list_available_models, resolve_model};
use super::upstream::call_upstream;
use super::{ChatCompletionRequest, ChatMessage, LlmProtocol, LlmState};

/// State for LLM request handlers.
#[derive(Clone)]
pub struct LlmHandlerState {
    pub llm: Arc<LlmState>,
    /// Which protocol matched this request; None means "not set" (shouldn't happen).
    pub protocol: Option<LlmProtocol>,
}

impl LlmHandlerState {
    /// Return an error response in the format appropriate for the matched protocol.
    pub fn error_for_protocol(
        &self,
        status: StatusCode,
        message: String,
        error_type: &str,
    ) -> Response {
        match self.protocol {
            Some(LlmProtocol::Anthropic) => {
                super::upstream::error_response_anthropic(status, message, error_type)
            }
            _ => super::upstream::error_response(status, message, error_type),
        }
    }
}

/// GET /v1/models — list available models.
pub async fn handle_list_models(
    State(state): State<LlmHandlerState>,
    headers: HeaderMap,
) -> Response {
    // Validate API key
    if super::auth::authenticate(&state.llm, &headers)
        .await
        .is_none()
    {
        return state.error_for_protocol(
            StatusCode::UNAUTHORIZED,
            "Invalid API key".into(),
            "authentication_error",
        );
    }

    match list_available_models(&state.llm).await {
        Ok(models) => Json(serde_json::json!({
            "object": "list",
            "data": models,
        }))
        .into_response(),
        Err(e) => state.error_for_protocol(StatusCode::INTERNAL_SERVER_ERROR, e, "server_error"),
    }
}

/// POST /v1/chat/completions — chat completion.
pub async fn handle_chat_completions(
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
                    protocol: "openai".into(),
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

    // Extract model name
    let model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => {
            // 记录请求错误（缺少 model）
            if let Some(ref db) = state.llm.db {
                let ctx = super::usage::UsageContext {
                    api_key_id: Some(api_key_id.clone()),
                    api_key_name: api_key_name.clone(),
                    protocol: "openai".into(),
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
                    protocol: "openai".into(),
                    ..Default::default()
                };
                ctx.record_failure(db, 404, "model_resolution_error", std::time::Instant::now());
            }
            return super::router::resolve_error_response(&state.llm, e).await;
        }
    };

    // Build unified request
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let messages: Vec<ChatMessage> = match body.get("messages") {
        Some(msgs) => match serde_json::from_value(msgs.clone()) {
            Ok(m) => m,
            Err(e) => {
                // 记录请求解析错误
                if let Some(ref db) = state.llm.db {
                    let ctx = super::usage::UsageContext {
                        api_key_id: Some(api_key_id.clone()),
                        api_key_name: api_key_name.clone(),
                        provider_id: Some(provider.id.clone()),
                        provider_name: provider.name.clone(),
                        model_id: Some(model_id.clone()),
                        model_name: actual_model.clone(),
                        requested_model: model.clone(),
                        protocol: "openai".into(),
                        stream,
                    };
                    ctx.record_failure(db, 400, "invalid_request_error", std::time::Instant::now());
                }
                return state.error_for_protocol(
                    StatusCode::BAD_REQUEST,
                    format!("invalid messages: {}", e),
                    "invalid_request_error",
                );
            }
        },
        None => {
            // 记录请求错误（缺少 messages）
            if let Some(ref db) = state.llm.db {
                let ctx = super::usage::UsageContext {
                    api_key_id: Some(api_key_id.clone()),
                    api_key_name: api_key_name.clone(),
                    provider_id: Some(provider.id.clone()),
                    provider_name: provider.name.clone(),
                    model_id: Some(model_id.clone()),
                    model_name: actual_model.clone(),
                    requested_model: model.clone(),
                    protocol: "openai".into(),
                    stream,
                };
                ctx.record_failure(db, 400, "invalid_request_error", std::time::Instant::now());
            }
            return state.error_for_protocol(
                StatusCode::BAD_REQUEST,
                "messages is required".into(),
                "invalid_request_error",
            );
        }
    };

    let request = ChatCompletionRequest {
        model: actual_model.clone(),
        messages,
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
        // OpenAI 兼容入口：tools / tool_choice 直接透传上游。
        tools: body.get("tools").and_then(|v| v.as_array()).cloned(),
        tool_choice: body.get("tool_choice").cloned(),
    };

    // 用量采集上下文
    let ctx = super::usage::UsageContext {
        api_key_id: Some(api_key_id),
        api_key_name,
        provider_id: Some(provider.id.clone()),
        provider_name: provider.name.clone(),
        model_id: Some(model_id),
        model_name: actual_model,
        requested_model: model,
        protocol: "openai".into(),
        stream,
    };
    let started = std::time::Instant::now();
    let db = state.llm.db.clone();

    // 兼容模式：provider 开启 compat_tool_history 时，把工具调用历史改写为纯文本。
    let mut request = request;
    if super::compat::compat_tool_history_enabled(provider.extra_config.as_deref()) {
        super::compat::rewrite_tool_history(&mut request.messages);
    }

    // Call upstream
    let compat_enabled = super::compat::compat_tool_history_enabled(provider.extra_config.as_deref());
    match call_upstream(&provider.base_url, &provider.api_key, &request).await {
        Ok(resp) => {
            let resp = if compat_enabled {
                if request.stream {
                    // 流式 + compat 模式：缓冲所有 content，流结束时解析伪工具调用
                    rewrite_pseudo_tool_calls_in_stream(resp).await
                } else {
                    // 非流式 + compat 模式：解析伪工具调用，还原为结构化 tool_calls
                    rewrite_pseudo_tool_calls_in_response(resp).await
                }
            } else {
                resp
            };
            super::usage::wrap_and_record(resp, ctx, db, started).await
        }
        Err((status, msg)) => {
            // 记录失败请求到用量日志，确保请求明细中可见
            if let Some(ref db) = db {
                ctx.record_failure(db, status.as_u16() as i32, "upstream_error", started);
            }
            state.error_for_protocol(status, msg, "upstream_error")
        }
    }
}

/// 非流式响应：从 OpenAI chat.completion body 中解析伪工具调用文本，
/// 还原为结构化 `tool_calls`，让客户端能正常执行工具。
///
/// 只在 compat 模式（`compat_tool_history`）开启时调用。
/// 如果响应中没有伪工具调用格式，原样返回。
pub async fn rewrite_pseudo_tool_calls_in_response(resp: Response) -> Response {
    let (parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return Response::from_parts(parts, Body::from("failed to read response")),
    };

    let mut json: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Response::from_parts(parts, Body::from(bytes)),
    };

    // 只处理 OpenAI chat.completion 格式
    let Some(choices) = json.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return Response::from_parts(parts, Body::from(bytes));
    };

    for choice in choices.iter_mut() {
        let Some(message) = choice.get_mut("message") else {
            continue;
        };
        let Some(content) = message.get("content").and_then(|c| c.as_str()) else {
            continue;
        };

        let (remaining, tool_calls) = super::compat::parse_pseudo_tool_calls(content);
        if let Some(calls) = tool_calls {
            // 有伪工具调用：更新 content（去除工具调用部分），注入结构化 tool_calls
            if remaining.is_empty() {
                message["content"] = serde_json::Value::Null;
            } else {
                message["content"] = serde_json::Value::String(remaining);
            }
            message["tool_calls"] = serde_json::Value::Array(calls);
            // 修改 finish_reason 为 tool_calls（OpenAI 惯例）
            choice["finish_reason"] = serde_json::Value::String("tool_calls".into());
        }
    }

    let new_bytes = serde_json::to_vec(&json).unwrap_or_else(|_| bytes.to_vec());
    Response::from_parts(parts, Body::from(new_bytes))
}

/// 流式响应：缓冲整个 SSE 流，解析伪工具调用，重新构建 SSE 流。
///
/// 只在 compat 模式（`compat_tool_history`）开启时调用。
/// 如果流中没有伪工具调用格式，原样返回。
///
/// 实现策略：读取整个 SSE 流到内存（LLM 流通常 < 几 MB），解析所有
/// chunk 的 `delta.content`，流结束时用 `parse_pseudo_tool_calls` 检测
/// 伪工具调用。如果有，重新构建 SSE 流：
///   1. 先发送剩余文本的 content chunk（如果有）
///   2. 再发送 tool_calls chunk
///   3. 最后发送 finish chunk（finish_reason="tool_calls"）
///
/// 如果没有伪工具调用，原样转发所有 chunk。
pub async fn rewrite_pseudo_tool_calls_in_stream(resp: Response) -> Response {
    use futures_util::StreamExt;

    let (parts, body) = resp.into_parts();

    // 读取整个流到内存
    let mut all_bytes = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => all_bytes.extend_from_slice(&bytes),
            Err(_) => break,
        }
    }

    let text = String::from_utf8_lossy(&all_bytes);

    // 解析所有 SSE 行，提取 content 和元数据
    let mut parser = super::compat::StreamPseudoToolCallParser::new();
    let mut first_chunk: Option<serde_json::Value> = None;
    let mut model = String::new();
    let mut id = String::new();
    // 上游末尾携带 usage 的 chunk（OpenAI include_usage 时出现在收尾 chunk）。
    // 重建流时必须保留，否则下游 Anthropic 翻译只能走 [DONE] 分支硬编码
    // output_tokens=0，导致 usage 统计全 0。
    let mut usage_chunk: Option<serde_json::Value> = None;

    for line in text.lines() {
        if line.starts_with("data:") {
            let payload = line.strip_prefix("data:").unwrap().trim();
            if payload == "[DONE]" {
                continue;
            }
            if let Ok(chunk) = serde_json::from_str::<serde_json::Value>(payload) {
                if first_chunk.is_none() {
                    first_chunk = Some(chunk.clone());
                    model = chunk["model"].as_str().unwrap_or("").to_string();
                    id = chunk["id"].as_str().unwrap_or("").to_string();
                }
                // 记录最后一个携带非空 usage 对象的 chunk（保留其 choices/finish_reason，
                // 重建时原样转发，仅依赖它把 usage 带给下游）。
                if chunk
                    .get("usage")
                    .map(|u| u.is_object())
                    .unwrap_or(false)
                {
                    usage_chunk = Some(chunk);
                }
                parser.push_chunk(line);
            }
        }
    }

    let result = parser.finish();

    if !result.has_tool_calls {
        // 没有伪工具调用，原样返回
        return Response::from_parts(parts, Body::from(all_bytes));
    }

    // 有伪工具调用：重新构建 SSE 流
    let mut output = String::new();
    let tool_calls = result.tool_calls.unwrap();

    // 1. 如果有剩余文本，先发送 content chunk
    if !result.remaining_text.is_empty() {
        let chunk = serde_json::json!({
            "id": id,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {"content": result.remaining_text},
                "finish_reason": null
            }]
        });
        output.push_str(&format!("data: {}\n\n", chunk));
    }

    // 2. 发送 tool_calls chunk（每个 tool_call 一个 chunk）
    for (i, call) in tool_calls.iter().enumerate() {
        let is_last = i == tool_calls.len() - 1;
        let finish = if is_last { "tool_calls" } else { "" };
        let chunk = serde_json::json!({
            "id": id,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": i,
                        "id": call["id"],
                        "type": "function",
                        "function": call["function"],
                    }]
                },
                "finish_reason": if is_last { serde_json::Value::String(finish.into()) } else { serde_json::Value::Null }
            }]
        });
        output.push_str(&format!("data: {}\n\n", chunk));
    }

    // 3. 保留上游 usage chunk（若有）：下游 Anthropic 翻译靠它发出带完整
    //    input/output 细分的 message_delta，usage 统计才不会全 0。
    if let Some(usage) = usage_chunk {
        output.push_str(&format!("data: {}\n\n", usage));
    }

    // 4. 发送 [DONE]
    output.push_str("data: [DONE]\n\n");

    Response::from_parts(parts, Body::from(output.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;
    use axum::http::HeaderValue;

    /// 构造带真实临时 DB 的 LlmState，并插入一个启用的 provider+model。
    /// 返回 (state, 有效 API key, _tempdir 守卫)。
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
        db.llm_save_api_key(&kid, &hash, &prefix, "test")
            .await
            .unwrap();

        (LlmState::new(Some(db), None), key, tmp)
    }

    fn authed_headers(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn test_chat_completions_requires_auth() {
        let (state, _key, _tmp) = state_with_db().await;
        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            HeaderMap::new(),
            Json(serde_json::json!({"model": "deepseek-chat", "messages": []})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_model_not_found_includes_available_models() {
        let (state, key, _tmp) = state_with_db().await;
        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "nonexistent-model",
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // spec: 未匹配 → 返回 404，body 中包含可用模型列表
        let available = v["error"]["available_models"]
            .as_array()
            .expect("error should include available_models list");
        assert!(
            available.iter().any(|m| m.as_str() == Some("fast-model")),
            "available_models should contain the alias: {v}"
        );
    }

    #[tokio::test]
    async fn test_list_models_returns_enabled_only() {
        let (state, key, _tmp) = state_with_db().await;
        let resp = handle_list_models(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "list");
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], "fast-model");
        assert_eq!(data[0]["object"], "model");
    }

    #[tokio::test]
    async fn test_x_api_key_header_authenticates() {
        // Anthropic 原生认证方式：x-api-key 头应与 Authorization: Bearer 等效。
        let (state, key, _tmp) = state_with_db().await;
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_str(&key).unwrap());
        let resp = handle_list_models(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── 失败请求记录到 usage log 的验证 ──────────────────────────

    /// 认证失败必须写入一条 failure 记录到 llm_usage_logs。
    #[tokio::test]
    async fn test_auth_failure_is_logged() {
        let (state, _key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap(); // clone before state is consumed

        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            HeaderMap::new(),
            Json(serde_json::json!({"model": "deepseek-chat", "messages": []})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // fire-and-forget 写入是异步的，稍等一下
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "认证失败应写入一条 usage log");
        assert_eq!(logs[0].success, 0);
        assert_eq!(logs[0].status_code, 401);
        assert_eq!(logs[0].error_type.as_deref(), Some("authentication_error"));
    }

    /// 模型未找到必须写入一条 failure 记录到 llm_usage_logs。
    #[tokio::test]
    async fn test_model_not_found_is_logged() {
        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();

        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
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
        assert_eq!(logs.len(), 1, "模型未找到应写入一条 usage log");
        assert_eq!(logs[0].success, 0);
        assert_eq!(logs[0].status_code, 404);
        assert_eq!(
            logs[0].error_type.as_deref(),
            Some("model_resolution_error")
        );
    }

    /// 上游连接失败（不可达地址）必须写入一条 failure 记录。
    #[tokio::test]
    async fn test_upstream_failure_is_logged() {
        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();

        // 把 provider base_url 改为不可达地址，触发上游连接失败 → 502
        let providers = db.llm_list_providers().await.unwrap();
        let pid = &providers[0].id;
        db.llm_save_provider(
            pid,
            "DS",
            "deepseek",
            "http://127.0.0.1:1", // 没人监听的端口
            "sk-upstream",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();

        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "deepseek-chat",
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "上游连接失败应写入一条 usage log");
        assert_eq!(logs[0].success, 0);
        assert_eq!(logs[0].status_code, 502);
        assert_eq!(logs[0].error_type.as_deref(), Some("upstream_error"));
        // 路由成功的记录应包含 provider/model 标识
        assert_eq!(logs[0].provider_name, "DS");
        assert_eq!(logs[0].model_name, "deepseek-chat");
    }

    // ── 伪工具调用解析集成测试 ──────────────────────────────────

    #[tokio::test]
    async fn test_rewrite_pseudo_tool_calls_in_response() {
        // 模拟上游返回的 OpenAI chat.completion 响应，content 中包含伪工具调用
        let upstream_body = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "[调用工具 Bash] {\"command\":\"ls\"}"
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // content 应为 null（只有工具调用，无文本）
        assert!(v["choices"][0]["message"]["content"].is_null());
        // 应有结构化 tool_calls
        let calls = v["choices"][0]["message"]["tool_calls"]
            .as_array()
            .expect("应有 tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        let args: serde_json::Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "ls");
        // finish_reason 应为 tool_calls
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn test_rewrite_preserves_text_and_tool_calls() {
        let upstream_body = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "我来执行命令\n[调用工具 Bash] {\"command\":\"ls\"}"
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // 文本部分保留
        assert_eq!(
            v["choices"][0]["message"]["content"].as_str().unwrap(),
            "我来执行命令"
        );
        // 同时有 tool_calls
        assert!(v["choices"][0]["message"]["tool_calls"].is_array());
    }

    #[tokio::test]
    async fn test_rewrite_no_pseudo_tool_calls_passthrough() {
        // 没有伪工具调用时，响应应原样通过
        let upstream_body = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "普通回复"},
                "finish_reason": "stop"
            }]
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            v["choices"][0]["message"]["content"].as_str().unwrap(),
            "普通回复"
        );
        assert!(v["choices"][0]["message"].get("tool_calls").is_none());
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn test_rewrite_multiple_tool_calls() {
        let upstream_body = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "[调用工具 Bash] {\"command\":\"ls\"}\n[调用工具 Read] {\"path\":\"/tmp\"}"
                },
                "finish_reason": "stop"
            }]
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        let calls = v["choices"][0]["message"]["tool_calls"]
            .as_array()
            .expect("应有 tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        assert_eq!(calls[1]["function"]["name"], "Read");
    }

    // ── 流式伪工具调用解析集成测试 ──────────────────────────────

    #[tokio::test]
    async fn test_stream_rewrite_with_tool_calls() {
        // 模拟上游 SSE 流，content 中包含伪工具调用
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"[调用工具 Bash] {\\\"command\\\":\\\"ls\\\"}\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(sse_data))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_stream(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // 应包含 tool_calls chunk
        assert!(text.contains("tool_calls"), "应包含 tool_calls: {text}");
        assert!(text.contains("Bash"), "应包含工具名: {text}");
        assert!(text.contains("call_1"), "应包含 call id: {text}");
        // finish_reason 应为 tool_calls
        assert!(text.contains("\"finish_reason\":\"tool_calls\""), "finish_reason 应为 tool_calls: {text}");
        // 不应包含原始伪工具调用文本
        assert!(!text.contains("[调用工具"), "不应包含伪工具调用文本: {text}");
    }

    #[tokio::test]
    async fn test_stream_rewrite_no_tool_calls_passthrough() {
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"普通文本\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(sse_data))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_stream(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // 应原样透传
        assert!(text.contains("普通文本"), "应包含原始文本: {text}");
        assert!(text.contains("[DONE]"), "应包含 [DONE]: {text}");
    }

    #[tokio::test]
    async fn test_stream_rewrite_mixed_content() {
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"我来执行\\n[调用工具 Bash] {\\\"command\\\":\\\"ls\\\"}\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(sse_data))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_stream(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // 应包含剩余文本和 tool_calls
        assert!(text.contains("我来执行"), "应包含剩余文本: {text}");
        assert!(text.contains("tool_calls"), "应包含 tool_calls: {text}");
    }

    /// 回归：compat 流式重写命中伪工具调用时，必须保留上游末尾的 usage chunk，
    /// 否则下游 AnthropicSseTranslator 只能走 `[DONE]` 分支硬编码 output_tokens=0，
    /// 导致 UsageSseScanner 统计的 tokens 全 0。
    #[tokio::test]
    async fn test_stream_rewrite_preserves_usage_chunk() {
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"[调用工具 Bash] {\\\"command\\\":\\\"ls\\\"}\"},\"finish_reason\":null}],\"usage\":null}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":87,\"completion_tokens\":16,\"total_tokens\":103,\"prompt_cache_hit_tokens\":60,\"prompt_cache_miss_tokens\":27}}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(sse_data))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_stream(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // 伪工具调用重写仍然生效
        assert!(text.contains("tool_calls"), "应包含 tool_calls: {text}");
        // 上游 usage 必须保留在重写后的流里
        assert!(
            text.contains("\"prompt_tokens\":87"),
            "重写后丢失上游 prompt_tokens: {text}"
        );
        assert!(
            text.contains("\"completion_tokens\":16"),
            "重写后丢失上游 completion_tokens: {text}"
        );

        // 端到端：重写输出经 Anthropic 翻译 + usage 扫描，必须解析出非零 tokens。
        let anthropic_resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(text.clone()))
            .unwrap();
        let anthropic_stream =
            crate::server::llm::anthropic_handler::convert_openai_stream_to_anthropic_for_test(
                anthropic_resp,
            );
        let anthropic_bytes = axum::body::to_bytes(anthropic_stream.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let mut scanner = crate::server::llm::usage::UsageSseScanner::new();
        scanner.push(&anthropic_bytes);
        let u = scanner.finish();
        assert_eq!(u.prompt_tokens, 87, "端到端 prompt_tokens 不应为 0");
        assert_eq!(u.completion_tokens, 16, "端到端 completion_tokens 不应为 0");
    }
}
