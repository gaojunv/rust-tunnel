use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::StreamExt;
use reqwest::Client;

use super::ChatCompletionRequest;

/// Call an upstream LLM provider with OpenAI-compatible format.
/// Supports both streaming (SSE) and non-streaming modes.
pub async fn call_upstream(
    base_url: &str,
    api_key: &str,
    request: &ChatCompletionRequest,
) -> Result<Response, (StatusCode, String)> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

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
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Upstream error {}: {}", status.as_u16(), body_text),
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
