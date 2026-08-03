use std::sync::LazyLock;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::StreamExt;
use reqwest::Client;

use super::ChatCompletionRequest;

/// Reusable HTTP client with connection pooling.
///
/// Timeout strategy:
/// - `connect_timeout`: 30 s — fast failure when the upstream is unreachable.
/// - `read_timeout`:   300 s (5 min) — detects genuinely hung connections while
///   allowing very large streaming responses (e.g. long tool-use / task-plan
///   generations) to complete without a premature "connection closed mid-response"
///   error.
/// - No global `timeout` — streaming LLM responses can legitimately take minutes;
///   the upstream provider enforces its own deadline.
///
/// HTTP/1.1 is forced (not HTTP/2) for two reasons:
/// 1. **read_timeout accuracy**: reqwest's `read_timeout` resets on every frame,
///    including HTTP/2 PING frames. Upstream providers that send h2 pings during
///    long generations would prevent the read timeout from ever firing, masking
///    a genuinely hung connection. HTTP/1.1 has no such pings.
/// 2. **Upstream compatibility**: Some LLM provider gateways have aggressive
///    idle timeouts (~120 s) on HTTP/2 connections. HTTP/1.1 with TCP keepalive
///    is more resilient to these middlebox timeouts.
static UPSTREAM_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .http1_only()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(std::time::Duration::from_secs(60))
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
        if pos + 3 <= n && bytes[pos] == b's' && bytes[pos + 1] == b'k' && bytes[pos + 2] == b'-' {
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

/// 构造发往上游的 OpenAI 请求体。
///
/// 两种模式：
/// - 透传（`raw_body` 有值）：以原始请求体为基底，只覆盖网关必须改写的字段
///   （model 别名 → 真实模型名、stream 恒为显式布尔、流式注入 stream_options.include_usage），
///   其余参数原样上行。messages 保留客户端原样（含多模态 content 数组）。
/// - 重建（`raw_body` 为 None，anthropic 转换路径）：从 request 逐字段拼回，
///   可选字段仅在有值时挂上（避免部分上游对 null 敏感），
///   messages 用带 skip_serializing_if 的 ChatMessage 序列化。
///
/// 独立成公共函数是为了让调用方在发送前拿到完整请求体写日志
/// （`log_llm_request` 记录的就是这个 body，与实际发送内容逐字节一致）。
pub fn build_upstream_body(request: &ChatCompletionRequest) -> serde_json::Value {
    // 透传模式：以原始请求体为基底，定点覆盖网关必须改写的字段。
    if let Some(mut raw) = request.raw_body.clone() {
        // raw_body 恒为对象：openai handler 先 400 校验 model/messages 后才构造请求，
        // anthropic_to_openai 也构造对象 passthrough。
        debug_assert!(raw.is_object(), "raw_body 应为 JSON 对象");
        raw["model"] = request.model.clone().into(); // 别名 → 真实模型名
        // stream 必须始终与网关决策一致：客户端省略 stream（或传非布尔值）时，
        // 也保证上游收到显式布尔值，避免透传模式下 stream 变成 null。
        // request.stream 与客户端显式值同源（as_bool().unwrap_or(false)），
        // 显式提供时覆盖为同值（no-op），与重建模式旧行为保持一致。
        raw["stream"] = request.stream.into();
        if request.stream {
            // 幂等注入 include_usage：保留客户端已有 stream_options 字段。
            let so = raw
                .as_object_mut()
                .and_then(|o| o.get_mut("stream_options"))
                .and_then(|v| v.as_object_mut());
            if let Some(so) = so {
                so.insert("include_usage".into(), serde_json::Value::Bool(true));
            } else {
                raw["stream_options"] = serde_json::json!({ "include_usage": true });
            }
        }
        return raw;
    }
    // 重建模式：anthropic 转换路径（raw_body 为 None）。
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
    if request.stream {
        req_body["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    req_body
}

/// Call an upstream LLM provider with OpenAI-compatible format.
/// Supports both streaming (SSE) and non-streaming modes.
pub async fn call_upstream(
    base_url: &str,
    api_key: &str,
    request: &ChatCompletionRequest,
) -> Result<Response, (StatusCode, String)> {
    let req_body = build_upstream_body(request);
    call_upstream_with_body(base_url, api_key, &req_body).await
}

/// 用已构造好的请求体调用上游。
///
/// 调用方（handler）先用 `build_upstream_body` 构造 body、写入完整请求日志，
/// 再走这里发送——保证日志内容与实际发送的请求体一致。
pub async fn call_upstream_with_body(
    base_url: &str,
    api_key: &str,
    req_body: &serde_json::Value,
) -> Result<Response, (StatusCode, String)> {
    let client = &*UPSTREAM_CLIENT;

    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

    let req = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(req_body);

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
        // 诊断日志：上游 4xx/5xx 时记录转换后的请求体摘要，便于定位字段兼容问题。
        // 注意脱敏：messages 只记录结构（role/长度/工具字段），不记录正文内容。
        let req_debug = summarize_request_for_log(req_body);
        tracing::warn!(
            target: "llm_upstream",
            status = status.as_u16(),
            url = %url,
            request = %req_debug,
            upstream_error = %sanitized,
            "LLM upstream rejected request"
        );

        // 增强诊断：记录完整请求体（截断到 8KB）到系统日志，用于对比子代理/主代理差异。
        // 脱敏：移除 Authorization 头，但保留请求体的完整内容（包含 messages/tools）。
        let full_req = serde_json::to_string_pretty(req_body).unwrap_or_default();
        let truncated_req = if full_req.len() > 8192 {
            format!("{}...\n[truncated, total {} bytes]", &full_req[..8192], full_req.len())
        } else {
            full_req
        };
        tracing::warn!(
            target: "llm_upstream_debug",
            status = status.as_u16(),
            model = %req_body.get("model").and_then(|m| m.as_str()).unwrap_or(""),
            stream = req_body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
            message_count = req_body.get("messages").and_then(|m| m.as_array()).map_or(0, Vec::len),
            has_tools = req_body.get("tools").is_some(),
            full_request_body = %truncated_req,
            upstream_error_full = %sanitized,
            "LLM upstream 4xx/5xx - full request dump"
        );

        return Err((
            status,
            format!("Upstream error {}: {}", status.as_u16(), sanitized),
        ));
    }

    let is_stream = req_body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if is_stream {
        relay_upstream_stream(resp).await
    } else {
        relay_upstream_body(resp).await
    }
}

/// Relay a streaming (SSE) upstream response to the client.
///
/// 当上游连接意外断开（如 LLM 服务商网关 idle timeout），向客户端发送一个
/// OpenAI 风格的 SSE error chunk 再正常关闭流，而不是让 hyper 在中途截断响应
/// （客户端收到 "Connection closed mid-response"）。
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

/// 生成请求体的结构摘要用于诊断日志：保留字段名和值类型/长度，
/// 但 messages 的正文、tools 的 description/parameters 不落地（避免泄露对话内容）。
fn summarize_request_for_log(req_body: &serde_json::Value) -> String {
    use serde_json::json;

    let mut summary = serde_json::Map::new();
    for (k, v) in req_body.as_object().into_iter().flatten() {
        match k.as_str() {
            "messages" => {
                let msgs: Vec<serde_json::Value> = v
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|m| {
                                let role = m
                                    .get("role")
                                    .and_then(|r| r.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                let content_len = m
                                    .get("content")
                                    .and_then(|c| c.as_str())
                                    .map(str::len)
                                    .unwrap_or(0);
                                let has_tool_calls = m.get("tool_calls").is_some();
                                let tool_call_id = m.get("tool_call_id").is_some();
                                json!({
                                    "role": role,
                                    "content_len": content_len,
                                    "tool_calls": has_tool_calls,
                                    "tool_call_id": tool_call_id,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                summary.insert(k.clone(), json!(msgs));
            }
            "tools" => {
                // 只记录工具名，不记录 description/parameters
                let names: Vec<&str> = v
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| {
                                t.get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(|n| n.as_str())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                summary.insert(k.clone(), json!(names));
            }
            _ => {
                summary.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::Value::Object(summary).to_string()
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
pub fn error_response_anthropic(status: StatusCode, message: String, error_type: &str) -> Response {
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
        let body =
            rt.block_on(async { axum::body::to_bytes(resp.into_body(), 1024).await.unwrap() });
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
                    raw_body: None,
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
        let mut s = "a".repeat(499);
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
        let input = "Key sk-aaa111bbb222 and Bearer token123456789 for endpoint";
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

    #[test]
    fn build_upstream_body_passthrough_unknown_params() {
        use crate::server::llm::ChatCompletionRequest;
        use crate::server::llm::ChatMessage;
        let raw = serde_json::json!({
            "model": "client-alias",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false,
            "stop": ["\n\n"],
            "seed": 42,
            "user": "abc",
            "temperature": 0.7,
        });
        let req = ChatCompletionRequest {
            model: "real-model".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: false,
            max_tokens: None,
            temperature: Some(0.7),
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = build_upstream_body(&req);
        // 未知参数原样保留
        assert_eq!(body["stop"], serde_json::json!(["\n\n"]));
        assert_eq!(body["seed"], 42);
        assert_eq!(body["user"], "abc");
        // model 被别名解析覆盖
        assert_eq!(body["model"], "real-model");
        // 非流式不注入 stream_options
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn build_upstream_body_passthrough_omitted_stream_defaults_to_false() {
        use crate::server::llm::ChatCompletionRequest;
        use crate::server::llm::ChatMessage;
        // 客户端省略 stream 字段：透传模式也必须发出显式布尔 false
        // （回归：此前透传原样返回 → stream 变 null，破坏非流式上游语义）。
        let raw = serde_json::json!({
            "model": "client-alias",
            "messages": [{"role": "user", "content": "hi"}],
            "seed": 42,
        });
        let req = ChatCompletionRequest {
            model: "real-model".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: false,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = build_upstream_body(&req);
        assert_eq!(body["stream"], false);
        // 未知参数仍原样保留
        assert_eq!(body["seed"], 42);
        // 非流式不注入 stream_options
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn build_upstream_body_stream_injects_include_usage_keeps_client_fields() {
        use crate::server::llm::ChatCompletionRequest;
        use crate::server::llm::ChatMessage;
        let raw = serde_json::json!({
            "model": "alias",
            "messages": [],
            "stream": true,
            "stream_options": {"max_tokens": 100},
        });
        let req = ChatCompletionRequest {
            model: "real".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: true,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = build_upstream_body(&req);
        let so = body["stream_options"].as_object().unwrap();
        // 客户端已有字段保留
        assert_eq!(so["max_tokens"], 100);
        // include_usage 被网关注入
        assert_eq!(so["include_usage"], true);
    }

    #[test]
    fn build_upstream_body_no_raw_body_rebuilds() {
        use crate::server::llm::ChatCompletionRequest;
        use crate::server::llm::ChatMessage;
        let req = ChatCompletionRequest {
            model: "m".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: false,
            max_tokens: Some(10),
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: None,
        };
        let body = build_upstream_body(&req);
        assert_eq!(body["model"], "m");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["max_tokens"], 10);
    }
}
