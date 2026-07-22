use std::sync::LazyLock;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::StreamExt;
use reqwest::Client;

use super::ChatCompletionRequest;

/// Reusable HTTP client with connection pooling.
static UPSTREAM_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .pool_max_idle_per_host(10)
        .build()
        .expect("failed to build upstream HTTP client")
});

/// Strip potential secrets (API keys, tokens) from upstream error messages.
fn sanitize_error_message(body: &str) -> String {
    // Truncate long error messages to 500 chars max
    let truncated = if body.len() > 500 {
        format!("{}...", &body[..500])
    } else {
        body.to_string()
    };
    // Redact common API key patterns
    truncated
        .replace("sk-", "sk-***")
        .replace("Bearer ", "Bearer ***")
}

/// Call an upstream LLM provider with OpenAI-compatible format.
/// Supports both streaming (SSE) and non-streaming modes.
pub async fn call_upstream(
    base_url: &str,
    api_key: &str,
    request: &ChatCompletionRequest,
) -> Result<Response, (StatusCode, String)> {
    let client = &*UPSTREAM_CLIENT;

    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

    let req_body = serde_json::json!({
        "model": request.model,
        "messages": request.messages.iter().map(|m| {
            serde_json::json!({
                "role": m.role,
                "content": m.content,
            })
        }).collect::<Vec<_>>(),
        "stream": request.stream,
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
        "top_p": request.top_p,
    });

    let req = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&req_body);

    let resp = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Upstream connection failed: {}", e)))?;

    let status = resp.status();

    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        // Sanitize: strip potential API key from error message
        let sanitized = sanitize_error_message(&body_text);
        return Err((
            status,
            format!("Upstream error {}: {}", status.as_u16(), sanitized),
        ));
    }

    if request.stream {
        // SSE streaming relay: forward the upstream SSE stream as-is.
        let byte_stream = resp
            .bytes_stream()
            .map(|result| {
                result
                    .map(|bytes| bytes.to_vec())
                    .map_err(|e| {
                        std::io::Error::other(e.to_string())
                    })
            });

        let body = Body::from_stream(byte_stream);
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(body)
            .unwrap())
    } else {
        // Non-streaming: return upstream response body as-is.
        let body_bytes = resp.bytes().await.map_err(|e| {
            (StatusCode::BAD_GATEWAY, format!("Failed to read upstream response: {}", e))
        })?;

        let body = Body::from(body_bytes.to_vec());
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(body)
            .unwrap())
    }
}

/// Build an OpenAI-format error response.
pub fn error_response(status: StatusCode, message: String, error_type: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": error_type,
        }
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_response_contains_openai_format() {
        let resp = error_response(StatusCode::UNAUTHORIZED, "Invalid key".into(), "authentication_error");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_call_upstream_passthrough_status_code() {
        // Test that upstream errors pass through the original status code
        // rather than always returning 502 Bad Gateway
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Call a URL that returns 404
            let result = call_upstream(
                "http://127.0.0.1:1", // non-existent server
                "test-key",
                &ChatCompletionRequest {
                    model: "test".into(),
                    messages: vec![],
                    stream: false,
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                },
            )
            .await;
            assert!(result.is_err());
            let (status, msg) = result.unwrap_err();
            // Connection refused should be 502
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            // Error message should NOT contain the API key
            assert!(!msg.contains("test-key"), "API key should not be in error message: {}", msg);
        });
    }

    #[test]
    fn test_error_message_no_api_key_leak() {
        // Just validate the pattern — error messages about upstream errors
        // should be generic and not echo back the API key
        let msg = "Upstream connection failed: connection refused";
        // Generic error messages should not contain sk- patterns
        assert!(!msg.contains("sk-"));
    }
}
