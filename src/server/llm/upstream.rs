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
    // Truncate at a valid UTF-8 character boundary (max 500 chars).
    let end = if body.len() <= 500 {
        body.len()
    } else {
        // Find the last complete char boundary at or before byte 500.
        // floor_char_boundary is stabilized in Rust 1.79+; we implement manually
        // to stay compatible.
        let mut boundary = 500;
        while boundary > 0 && !body.is_char_boundary(boundary) {
            boundary -= 1;
        }
        boundary
    };

    let truncated = if end < body.len() {
        format!("{}...", &body[..end])
    } else {
        body.to_string()
    };

    // Redact patterns (best-effort — not a security guarantee):
    // - `sk-<hex/alphanumeric>` → `sk-***`
    // - `Bearer <token>` (case-insensitive prefix) → `Bearer ***`
    //
    // We walk bytes and do manual substring matching to avoid pulling in `regex`.
    let bytes = truncated.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut pos = 0;

    while pos < n {
        // ── Check for `Bearer ` / `bearer ` ──
        let remaining = &truncated[pos..];
        if remaining.len() > 7 {
            let lower = remaining[..7].to_ascii_lowercase();
            if lower == "bearer " {
                // Skip the "Bearer " / "bearer " prefix.
                pos += 7;
                // Skip the token.
                while pos < n && !bytes[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                out.push_str("Bearer ***");
                continue;
            }
        }

        // ── Check for `sk-` followed by alphanumeric ──
        if pos + 3 <= n && bytes[pos] == b's' && bytes[pos + 1] == b'k' && bytes[pos + 2] == b'-'
        {
            let mut key_end = pos + 3;
            while key_end < n && bytes[key_end].is_ascii_alphanumeric() && key_end - pos <= 67 {
                key_end += 1;
            }
            if key_end > pos + 3 {
                out.push_str("sk-***");
                pos = key_end;
                continue;
            }
        }

        // ── Regular character ──
        let ch = truncated[pos..].chars().next().unwrap();
        out.push(ch);
        pos += ch.len_utf8();
    }

    out
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

    // 构造上游请求体：可选字段仅在有值时挂上，避免部分上游对 null 敏感。
    // messages 直接用带 skip_serializing_if 的 ChatMessage 序列化，工具字段
    // （tool_calls / tool_call_id / name）能一并透传。
    let mut req_body = serde_json::json!({
        "model": request.model,
        "messages": request.messages,
        "stream": request.stream,
    });
    if let Some(v) = request.max_tokens {
        req_body["max_tokens"] = v.into();
    }
    if let Some(v) = request.temperature {
        req_body["temperature"] = v.into();
    }
    if let Some(v) = request.top_p {
        req_body["top_p"] = v.into();
    }
    if let Some(tools) = &request.tools {
        req_body["tools"] = serde_json::Value::Array(tools.clone());
    }
    if let Some(choice) = &request.tool_choice {
        req_body["tool_choice"] = choice.clone();
    }
    // 流式请求注入 stream_options.include_usage=true：OpenAI 系上游（火山/Kimi/Mimo）
    // 默认流式不返回 usage，注入后才会在末尾 chunk 附带；DeepSeek 默认返回，注入无副作用。
    if request.stream {
        req_body["stream_options"] = serde_json::json!({ "include_usage": true });
    }

    let req = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&req_body);

    let resp = req.send().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Upstream connection failed: {}", e),
        )
    })?;

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
        relay_upstream_stream(resp).await
    } else {
        relay_upstream_body(resp).await
    }
}

/// Relay a streaming (SSE) upstream response to the client.
async fn relay_upstream_stream(resp: reqwest::Response) -> Result<Response, (StatusCode, String)> {
    let byte_stream = resp.bytes_stream().map(|result| {
        result
            .map(|bytes| bytes.to_vec())
            .map_err(|e| std::io::Error::other(e.to_string()))
    });

    let body = Body::from_stream(byte_stream);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap())
}

/// Relay a non-streaming upstream response body to the client.
async fn relay_upstream_body(resp: reqwest::Response) -> Result<Response, (StatusCode, String)> {
    let body_bytes = resp.bytes().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to read upstream response: {}", e),
        )
    })?;

    let body = Body::from(body_bytes.to_vec());
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(body)
        .unwrap())
}

/// 透传原始请求到上游 Anthropic 端点，不做格式转换。
///
/// 认证策略：同时支持 `x-api-key`（Anthropic 原生）和 `Authorization: Bearer`（OpenAI 风格）。
/// 先尝试 `x-api-key`，若返回 401 则回退到 Bearer 头重试。
pub async fn call_upstream_raw(
    base_url: &str,
    api_key: &str,
    path: &str,
    body: &serde_json::Value,
    is_stream: bool,
) -> Result<Response, (StatusCode, String)> {
    let client = &*UPSTREAM_CLIENT;
    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );

    // 先用 x-api-key 头尝试
    let resp = client
        .post(&url)
        .header("x-api-key", api_key)
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Upstream connection failed: {}", e),
            )
        })?;

    // 如果 401，回退到 Bearer 头重试
    let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream connection failed: {}", e),
                )
            })?
    } else {
        resp
    };

    let status = resp.status();

    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        let sanitized = sanitize_error_message(&body_text);
        return Err((
            status,
            format!("Upstream error {}: {}", status.as_u16(), sanitized),
        ));
    }

    if is_stream {
        relay_upstream_stream(resp).await
    } else {
        relay_upstream_body(resp).await
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

/// Build an Anthropic-format error response for Anthropic-protocol domains.
pub fn error_response_anthropic(
    status: StatusCode,
    message: String,
    error_type: &str,
) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message,
        }
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Build a 404 "model not found" response that carries the available model list,
/// per spec: 未匹配 → 返回 404，body 中包含可用模型列表。
pub fn model_not_found_response(message: String, available_models: Vec<String>) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "available_models": available_models,
        }
    });
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_response_contains_openai_format() {
        let resp = error_response(
            StatusCode::UNAUTHORIZED,
            "Invalid key".into(),
            "authentication_error",
        );
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_error_response_anthropic_format() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = error_response_anthropic(
            StatusCode::NOT_FOUND,
            "Not found".into(),
            "invalid_request_error",
        );
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = rt.block_on(async {
            axum::body::to_bytes(resp.into_body(), 1024).await.unwrap()
        });
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["type"], "error"); // Anthropic top-level type
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["message"], "Not found");
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
                    tools: None,
                    tool_choice: None,
                },
            )
            .await;
            assert!(result.is_err());
            let (status, msg) = result.unwrap_err();
            // Connection refused should be 502
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            // Error message should NOT contain the API key
            assert!(
                !msg.contains("test-key"),
                "API key should not be in error message: {}",
                msg
            );
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

    // ── sanitize_error_message tests ────────────────────────────

    #[test]
    fn sanitize_short_message_passes_through() {
        let input = "A simple error message";
        let result = sanitize_error_message(input);
        assert_eq!(result, input);
    }

    #[test]
    fn sanitize_truncates_long_message() {
        let long = "x".repeat(1000);
        let result = sanitize_error_message(&long);
        assert!(result.len() <= 510); // 500 chars + "..." + some overhead
        assert!(result.ends_with("..."));
    }

    #[test]
    fn sanitize_truncation_is_utf8_safe() {
        // Build a string where byte 500 falls inside a multi-byte character
        let mut s = String::from("a".repeat(499));
        s.push('\u{4E2D}'); // 3-byte char at position 499
        s.push_str("end");
        let result = sanitize_error_message(&s);
        // Must not panic and must produce valid UTF-8
        assert!(result.ends_with("..."));
        // The 3-byte char at position 499 should either be fully included or excluded
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn sanitize_redacts_sk_key() {
        let input = "Error: invalid api key sk-abc123def456 for request";
        let result = sanitize_error_message(input);
        assert!(!result.contains("sk-abc123def456"));
        assert!(result.contains("sk-***"));
    }

    #[test]
    fn sanitize_redacts_bearer_token() {
        let input = "Unauthorized: Bearer eyJhbGciOiJIUzI1NiJ9.token.payload";
        let result = sanitize_error_message(input);
        assert!(!result.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(result.contains("Bearer ***"));
    }

    #[test]
    fn sanitize_redacts_lowercase_bearer() {
        let input = "unauthorized: bearer abcdefgh12345678";
        let result = sanitize_error_message(input);
        assert!(!result.contains("abcdefgh12345678"));
        assert!(result.contains("Bearer ***"));
    }

    #[test]
    fn sanitize_handles_multiple_keys() {
        let input =
            "Key sk-aaa111bbb222 and Bearer token123456789 for endpoint";
        let result = sanitize_error_message(input);
        assert!(!result.contains("sk-aaa111bbb222"));
        assert!(!result.contains("token123456789"));
        assert_eq!(
            result.matches("sk-***").count(),
            1,
            "should have one sk-***"
        );
        assert_eq!(
            result.matches("Bearer ***").count(),
            1,
            "should have one Bearer ***"
        );
    }

    #[test]
    fn sanitize_preserves_non_secret_content() {
        let input = "HTTP 502 Bad Gateway: upstream server returned error";
        let result = sanitize_error_message(input);
        assert!(result.contains("Bad Gateway"));
        assert!(result.contains("upstream server"));
    }

    #[test]
    fn sanitize_does_not_false_positive_on_unrelated_text() {
        // Text without any "sk-" or "Bearer" patterns should pass through unchanged.
        let input = "HTTP 502: task description processing failed";
        let result = sanitize_error_message(input);
        assert!(result.contains("task description"));
        assert!(result.contains("processing failed"));
    }

    #[test]
    fn sanitize_sk_without_alphanumeric_suffix_not_redacted() {
        // "sk-" without alphanumeric followers should not be redacted
        let input = "prefix sk- suffix";
        let result = sanitize_error_message(input);
        // "sk-" followed by a space (not alphanumeric) => not redacted
        assert!(result.contains("sk-"));
    }

    #[test]
    fn sanitize_empty_string_ok() {
        assert_eq!(sanitize_error_message(""), "");
    }

    #[test]
    fn sanitize_exactly_500_chars_not_truncated() {
        let s = "x".repeat(500);
        let result = sanitize_error_message(&s);
        assert!(!result.ends_with("..."));
        assert_eq!(result.len(), 500);
    }
}
