//! POST /v1/responses — Responses API 入口（挂在 OpenAI 域名下）。
//!
//! Codex 等使用 Responses API 的客户端直接对接网关，内部复用现有 pipeline
//! （认证/路由/RAG/compat/故障转移/用量）。请求/响应/流式双向转换由
//! [`super::responses`] 完成。

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;

use super::openai_handler::LlmHandlerState;
use super::pipeline;
use super::pipeline::PreparedRequest;
use super::pipeline::ResponsePostProcess;
use super::responses;
use super::usage::UsageContext;

/// POST /v1/responses — Responses API request.
///
/// 流程严格仿 `openai_handler::handle_chat_completions`，协议标识用 `"responses"`。
pub async fn handle_responses(
    State(state): State<LlmHandlerState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // 认证
    let (api_key_id, api_key_name) =
        match pipeline::authenticate_or_reject(&state, &headers, "responses").await {
            Ok(a) => a,
            Err(resp) => return resp,
        };

    // 提取 model
    let model = match pipeline::extract_model_or_reject(
        &state,
        &body,
        &api_key_id,
        &api_key_name,
        "responses",
    )
    .await
    {
        Ok(m) => m,
        Err(resp) => return resp,
    };

    // 解析模型路由
    let chain = match pipeline::resolve_chain_or_reject(
        &state,
        &model,
        &api_key_id,
        &api_key_name,
        "responses",
    )
    .await
    {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    let first_candidate = chain.candidates[0].clone();
    let provider = first_candidate.provider.clone();
    let actual_model = first_candidate.model_name.clone();
    let model_id = first_candidate.model_id.clone();

    let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    // RAG 知识库查询（同 openai_handler）
    let api_key_id_for_rag = api_key_id.clone();
    let mut kb_id_for_rag: Option<String> = None;
    if let Some(ref db) = state.llm.db {
        kb_id_for_rag = db
            .rag_get_kb_id_for_api_key(&api_key_id_for_rag)
            .await
            .ok()
            .flatten();
    }
    let compat_enabled =
        super::compat::compat_tool_history_enabled(provider.extra_config.as_deref());

    // 将 Responses 请求转换为内部 ChatCompletionRequest
    let mut request = match responses::responses_request_to_chat(&body) {
        Ok(r) => r,
        Err(msg) => {
            // 记录 400 usage 失败（同 openai_handler 的 messages 缺失分支）
            if let Some(ref db) = state.llm.db {
                let ctx = UsageContext {
                    api_key_id: Some(api_key_id.clone()),
                    api_key_name: api_key_name.clone(),
                    provider_id: Some(provider.id.clone()),
                    provider_name: provider.name.clone(),
                    model_id: Some(model_id.clone()),
                    model_name: actual_model.clone(),
                    requested_model: model.clone(),
                    protocol: "responses".into(),
                    stream,
                    rag_chunks_injected: None,
                    failover_from: None,
                };
                ctx.record_failure(db, 400, "invalid_request_error", std::time::Instant::now());
            }
            return state.error_for_protocol(
                axum::http::StatusCode::BAD_REQUEST,
                msg,
                "invalid_request_error",
            );
        }
    };

    // 覆盖为上游真实模型名（同 openai_handler）
    request.model = actual_model.clone();

    // raw_body 保持 None（重建模式）：Responses 请求的 input/instructions/max_output_tokens
    // 等字段对 chat completions 上游是非法字段，透传会被严格校验的上游 400；
    // 重建模式由 build_upstream_body 按 request 字段重新组装，max_tokens 等映射自然生效。

    // 用量采集上下文
    let mut ctx = UsageContext {
        api_key_id: Some(api_key_id),
        api_key_name,
        provider_id: Some(provider.id.clone()),
        provider_name: provider.name.clone(),
        model_id: Some(model_id),
        model_name: actual_model,
        requested_model: model,
        protocol: "responses".into(),
        stream,
        rag_chunks_injected: None,
        failover_from: None,
    };
    let started = std::time::Instant::now();
    let db = state.llm.db.clone();

    // RAG 注入 + compat 改写
    pipeline::inject_rag_and_compat(
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

    let prepared = PreparedRequest {
        request,
        message_count,
        has_tools,
        compat_enabled,
        // Responses 入口无原始 Anthropic body，直通分支永不触发。
        anthropic_body: None,
    };

    pipeline::run_execution(
        &state,
        "responses",
        prepared,
        &chain,
        &first_candidate.model_name,
        ctx,
        db,
        started,
        ResponsePostProcess::ToResponses,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header, HeaderValue, StatusCode};
    use axum::routing::post;
    use axum::Router;

    /// 构造带真实临时 DB 的 LlmState，并插入一个启用的 provider+model。
    /// 返回 (state, 有效 API key, _tempdir 守卫)。
    async fn state_with_db() -> (super::super::LlmState, String, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
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

        let (key, hash, prefix) = crate::llm::auth::generate_api_key();
        let kid = uuid::Uuid::new_v4().to_string();
        db.llm_save_api_key(&kid, &hash, &prefix, "test", None)
            .await
            .unwrap();

        (super::super::LlmState::new(Some(db), None), key, tmp)
    }

    fn authed_headers(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
        );
        headers
    }

    // ── 未认证 → 401 ──

    #[tokio::test]
    async fn test_responses_requires_auth() {
        let (state, _key, _tmp) = state_with_db().await;
        let resp = handle_responses(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            HeaderMap::new(),
            Json(serde_json::json!({"model": "m", "input": "hi"})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ── 缺 model → 400 ──

    #[tokio::test]
    async fn test_responses_missing_model_returns_400() {
        let (state, key, _tmp) = state_with_db().await;
        let resp = handle_responses(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({"input": "hi"})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ── 缺 input → 400 ──

    #[tokio::test]
    async fn test_responses_missing_input_returns_400() {
        let (state, key, _tmp) = state_with_db().await;
        let resp = handle_responses(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({"model": "deepseek-chat"})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ── 非流式端到端 ──

    #[tokio::test]
    async fn test_responses_nonstream_end_to_end() {
        // mock upstream LLM：捕获请求体，返回 OpenAI chat completion 格式
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(serde_json::Value::Null));
        let captured2 = captured.clone();
        let llm_app = Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = captured2.clone();
                async move {
                    *captured.lock().await = body;
                    axum::Json(serde_json::json!({
                        "id": "chatcmpl-resp-1",
                        "object": "chat.completion",
                        "model": "deepseek-chat",
                        "created": 1700000000,
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "Hello from Responses"},
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

        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
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

        let resp = handle_responses(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "deepseek-chat",
                "instructions": "You are helpful",
                "input": "Hello",
                "max_output_tokens": 512,
                "tools": [{
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {}}
                }]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // 断言上游收到的是 chat completions 格式
        let upstream_body = captured.lock().await.clone();
        let msgs = upstream_body["messages"]
            .as_array()
            .expect("upstream should receive messages");
        // 第一条是 system（来自 instructions）
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are helpful");
        // 第二条是 user（来自 input 字符串）
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "Hello");
        // tools 被包装为 function 形式
        let tools = upstream_body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        // 重建模式：Responses 专有字段不得泄漏到 chat 上游（严格校验的上游会 400）
        assert!(upstream_body.get("input").is_none(), "{upstream_body}");
        assert!(upstream_body.get("instructions").is_none(), "{upstream_body}");
        assert!(
            upstream_body.get("max_output_tokens").is_none(),
            "{upstream_body}"
        );
        // max_output_tokens 映射为 max_tokens
        assert_eq!(upstream_body["max_tokens"], 512, "{upstream_body}");

        // 断言响应是 Responses 格式
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "response");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["model"], "deepseek-chat");
        let output = v["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(
            output[0]["content"][0]["text"],
            "Hello from Responses"
        );
        // usage 字段
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["output_tokens"], 5);
    }

    // ── 流式端到端 ──

    #[tokio::test]
    async fn test_responses_stream_end_to_end() {
        let llm_app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let sse = concat!(
                    "data: {\"id\":\"chatcmpl-s1\",\"model\":\"deepseek-chat\",\"created\":1700000000,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-s1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi \"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-s1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"there\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-s1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
                    "data: [DONE]\n\n",
                );
                (
                    axum::http::StatusCode::OK,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/event-stream; charset=utf-8",
                    )],
                    sse,
                )
            }),
        );
        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(llm_listener, llm_app).await.unwrap();
        });

        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
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

        let resp = handle_responses(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "deepseek-chat",
                "input": "Hello",
                "stream": true
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let text = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(text.to_vec()).unwrap();

        // 断言 SSE 输出包含 Responses 事件
        assert!(
            text.contains("response.created"),
            "应含 response.created: {text}"
        );
        assert!(
            text.contains("response.output_text.delta"),
            "应含 response.output_text.delta: {text}"
        );
        assert!(
            text.contains("response.completed"),
            "应含 response.completed: {text}"
        );
        assert!(text.contains("[DONE]"), "应含 [DONE]: {text}");

        // 等待用量落库
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert!(!logs.is_empty(), "应有 usage log 记录");
        assert_eq!(logs[0].protocol, "responses");
    }

    // ── function_call 端到端 ──

    #[tokio::test]
    async fn test_responses_function_call_end_to_end() {
        let llm_app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let sse = concat!(
                    "data: {\"id\":\"chatcmpl-fc1\",\"model\":\"deepseek-chat\",\"created\":1700000000,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-fc1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"loc\\\":\\\"SF\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-fc1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                    "data: [DONE]\n\n",
                );
                (
                    axum::http::StatusCode::OK,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/event-stream; charset=utf-8",
                    )],
                    sse,
                )
            }),
        );
        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(llm_listener, llm_app).await.unwrap();
        });

        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();
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

        let resp = handle_responses(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "deepseek-chat",
                "input": "What's the weather?",
                "stream": true
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let text = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(text.to_vec()).unwrap();

        // 断言包含 function_call 相关事件
        assert!(
            text.contains("response.output_item.added"),
            "应含 output_item.added: {text}"
        );
        assert!(
            text.contains("response.function_call_arguments.delta"),
            "应含 function_call_arguments.delta: {text}"
        );
        assert!(
            text.contains("response.function_call_arguments.done"),
            "应含 function_call_arguments.done: {text}"
        );
        assert!(
            text.contains("response.completed"),
            "应含 response.completed: {text}"
        );
    }

    // ── 模型未找到 → 404 ──

    #[tokio::test]
    async fn test_responses_model_not_found() {
        let (state, key, _tmp) = state_with_db().await;
        let resp = handle_responses(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "nonexistent-model",
                "input": "hi"
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
