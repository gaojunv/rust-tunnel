use std::sync::Arc;

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
    match call_upstream(&provider.base_url, &provider.api_key, &request).await {
        Ok(resp) => super::usage::wrap_and_record(resp, ctx, db, started).await,
        Err((status, msg)) => {
            // 记录失败请求到用量日志，确保请求明细中可见
            if let Some(ref db) = db {
                ctx.record_failure(db, status.as_u16() as i32, "upstream_error", started);
            }
            state.error_for_protocol(status, msg, "upstream_error")
        }
    }
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
}
