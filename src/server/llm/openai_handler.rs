use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::auth::validate_api_key;
use super::router::{list_available_models, resolve_model};
use super::upstream::{call_upstream, error_response};
use super::{ChatCompletionRequest, ChatMessage, LlmState};

/// State for LLM request handlers.
#[derive(Clone)]
pub struct LlmHandlerState {
    pub llm: Arc<LlmState>,
}

/// Validate that the request's Host header matches the configured LLM domain.
/// Returns true if valid or if no domain is configured.
pub async fn validate_host(state: &LlmState, headers: &HeaderMap) -> bool {
    let cfg = state.gateway_config.read().await;
    let cfg = match cfg.as_ref() {
        Some(c) => c,
        None => return true,
    };

    let domain = &cfg.domain;
    // If no domain is configured, allow all hosts
    if domain.is_empty() || !cfg.enabled {
        return true;
    }

    // Extract Host header (strip port if present)
    let host = match headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        Some(h) => h.split(':').next().unwrap_or(h).to_string(),
        None => return false,
    };

    host == *domain
}

/// GET /v1/models — list available models.
pub async fn handle_list_models(
    State(state): State<LlmHandlerState>,
    headers: HeaderMap,
) -> Response {
    // Validate Host header matches configured LLM domain
    if !validate_host(&state.llm, &headers).await {
        return error_response(StatusCode::NOT_FOUND, "Not found".into(), "invalid_request_error");
    }

    // Validate API key
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if validate_api_key(&state.llm, auth).await.is_none() {
        return error_response(StatusCode::UNAUTHORIZED, "Invalid API key".into(), "authentication_error");
    }

    match list_available_models(&state.llm).await {
        Ok(models) => Json(serde_json::json!({
            "object": "list",
            "data": models,
        })).into_response(),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, e, "server_error"),
    }
}

/// POST /v1/chat/completions — chat completion.
pub async fn handle_chat_completions(
    State(state): State<LlmHandlerState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // Validate Host header matches configured LLM domain
    if !validate_host(&state.llm, &headers).await {
        return error_response(StatusCode::NOT_FOUND, "Not found".into(), "invalid_request_error");
    }

    // Validate API key
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if validate_api_key(&state.llm, auth).await.is_none() {
        return error_response(StatusCode::UNAUTHORIZED, "Invalid API key".into(), "authentication_error");
    }

    // Extract model name
    let model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return error_response(StatusCode::BAD_REQUEST, "model is required".into(), "invalid_request_error"),
    };

    // Resolve model → provider
    let (provider, actual_model) = match resolve_model(&state.llm, &model).await {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::NOT_FOUND, e, "invalid_request_error"),
    };

    // Build unified request
    let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let messages: Vec<ChatMessage> = match body.get("messages") {
        Some(msgs) => {
            match serde_json::from_value(msgs.clone()) {
                Ok(m) => m,
                Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("invalid messages: {}", e), "invalid_request_error"),
            }
        }
        None => return error_response(StatusCode::BAD_REQUEST, "messages is required".into(), "invalid_request_error"),
    };

    let request = ChatCompletionRequest {
        model: actual_model,
        messages,
        stream,
        max_tokens: body.get("max_tokens").and_then(|v| v.as_u64()).map(|v| v as u32),
        temperature: body.get("temperature").and_then(|v| v.as_f64()).map(|v| v as f32),
        top_p: body.get("top_p").and_then(|v| v.as_f64()).map(|v| v as f32),
    };

    // Call upstream
    match call_upstream(&provider.base_url, &provider.api_key, &request).await {
        Ok(resp) => resp,
        Err((status, msg)) => error_response(status, msg, "upstream_error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::llm::LlmGatewayConfig;
    use axum::http::HeaderValue;

    #[tokio::test]
    async fn test_validate_host_no_config() {
        let state = LlmState::new(None);
        let headers = HeaderMap::new();
        assert!(validate_host(&state, &headers).await);
    }

    #[tokio::test]
    async fn test_validate_host_empty_domain() {
        let state = LlmState::new(None);
        *state.gateway_config.write().await = Some(LlmGatewayConfig {
            enabled: true,
            domain: String::new(),
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        let headers = HeaderMap::new();
        assert!(validate_host(&state, &headers).await);
    }

    #[tokio::test]
    async fn test_validate_host_matches() {
        let state = LlmState::new(None);
        *state.gateway_config.write().await = Some(LlmGatewayConfig {
            enabled: true,
            domain: "llm.example.com".into(),
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("llm.example.com"));
        assert!(validate_host(&state, &headers).await);
    }

    #[tokio::test]
    async fn test_validate_host_mismatch() {
        let state = LlmState::new(None);
        *state.gateway_config.write().await = Some(LlmGatewayConfig {
            enabled: true,
            domain: "llm.example.com".into(),
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("evil.example.com"));
        assert!(!validate_host(&state, &headers).await);
    }

    #[tokio::test]
    async fn test_validate_host_no_host_header() {
        let state = LlmState::new(None);
        *state.gateway_config.write().await = Some(LlmGatewayConfig {
            enabled: true,
            domain: "llm.example.com".into(),
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        let headers = HeaderMap::new();
        assert!(!validate_host(&state, &headers).await);
    }

    #[tokio::test]
    async fn test_validate_host_with_port() {
        let state = LlmState::new(None);
        *state.gateway_config.write().await = Some(LlmGatewayConfig {
            enabled: true,
            domain: "llm.example.com".into(),
            listen: "0.0.0.0:443".into(),
            tls_enabled: false,
            tls_acme: false,
        });
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("llm.example.com:443"));
        assert!(validate_host(&state, &headers).await);
    }
}
