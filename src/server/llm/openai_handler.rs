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

    // Extract model name
    let model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => {
            return state.error_for_protocol(
                StatusCode::BAD_REQUEST,
                "model is required".into(),
                "invalid_request_error",
            )
        }
    };

    // Resolve model → provider
    let (provider, actual_model) = match resolve_model(&state.llm, &model).await {
        Ok(r) => r,
        Err(e) => return super::router::resolve_error_response(&state.llm, e).await,
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
                return state.error_for_protocol(
                    StatusCode::BAD_REQUEST,
                    format!("invalid messages: {}", e),
                    "invalid_request_error",
                )
            }
        },
        None => {
            return state.error_for_protocol(
                StatusCode::BAD_REQUEST,
                "messages is required".into(),
                "invalid_request_error",
            )
        }
    };

    let request = ChatCompletionRequest {
        model: actual_model,
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
        tools: body
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned(),
        tool_choice: body.get("tool_choice").cloned(),
    };

    // Call upstream
    match call_upstream(&provider.base_url, &provider.api_key, &request).await {
        Ok(resp) => resp,
        Err((status, msg)) => state.error_for_protocol(status, msg, "upstream_error"),
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
}
