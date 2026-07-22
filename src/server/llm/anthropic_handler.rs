use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;

use super::auth::validate_api_key;
use super::openai_handler::LlmHandlerState;
use super::router::resolve_model;
use super::upstream::{call_upstream, error_response};
use super::{ChatCompletionRequest, ChatMessage};

/// Extract text content from an Anthropic message's content field,
/// which can be either a plain string or an array of content blocks.
fn extract_content(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("type")
                        .and_then(|t| t.as_str())
                        .filter(|t| *t == "text")
                        .and(block.get("text").and_then(|t| t.as_str()))
                })
                .collect();
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Convert Anthropic Messages request to unified ChatCompletionRequest.
fn anthropic_to_openai(body: &serde_json::Value) -> Result<ChatCompletionRequest, String> {
    let anthropic_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("model is required")?;

    let messages_raw = body.get("messages").ok_or("messages is required")?;

    // Parse Anthropic messages with flexible content field (string or content blocks)
    let messages: Vec<ChatMessage> = messages_raw
        .as_array()
        .ok_or("messages must be an array")?
        .iter()
        .map(|msg| {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user")
                .to_string();
            let content = msg.get("content").map(extract_content).unwrap_or_default();
            ChatMessage { role, content }
        })
        .collect();

    // Anthropic uses system as a top-level field, not a message role
    let mut all_messages = Vec::new();
    if let Some(system) = body.get("system") {
        let system_text = extract_content(system);
        if !system_text.is_empty() {
            all_messages.push(ChatMessage {
                role: "system".to_string(),
                content: system_text,
            });
        }
    }
    all_messages.extend(messages);

    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Ok(ChatCompletionRequest {
        model: anthropic_model.to_string(),
        messages: all_messages,
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
    })
}

/// POST /v1/messages — Anthropic Messages API.
pub async fn handle_messages(
    State(state): State<LlmHandlerState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // Validate Host header matches configured LLM domain
    if !super::openai_handler::validate_host(&state.llm, &headers).await {
        return error_response(
            StatusCode::NOT_FOUND,
            "Not found".into(),
            "invalid_request_error",
        );
    }

    // Validate API key
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if validate_api_key(&state.llm, auth).await.is_none() {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "Invalid API key".into(),
            "authentication_error",
        );
    }

    // Convert Anthropic → OpenAI format
    let request = match anthropic_to_openai(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e, "invalid_request_error"),
    };

    // Resolve model → provider
    let (provider, actual_model) = match resolve_model(&state.llm, &request.model).await {
        Ok(r) => r,
        Err(e) => return super::router::resolve_error_response(&state.llm, e).await,
    };

    // Use actual upstream model name
    let mut request = request;
    request.model = actual_model;

    // Call upstream (providers use OpenAI format)
    match call_upstream(&provider.base_url, &provider.api_key, &request).await {
        Ok(resp) => {
            // 响应需要转回 Anthropic 格式：非流式整体转换，流式逐 chunk 转换
            if !request.stream {
                convert_openai_to_anthropic_response(resp).await
            } else {
                convert_openai_stream_to_anthropic(resp)
            }
        }
        Err((status, msg)) => error_response(status, msg, "upstream_error"),
    }
}

/// 流式：把上游 OpenAI SSE 响应体逐 chunk 转换为 Anthropic SSE 事件流。
fn convert_openai_stream_to_anthropic(openai_resp: Response) -> Response {
    use futures_util::StreamExt;

    let byte_stream = openai_resp.into_body().into_data_stream();
    let translator = std::sync::Arc::new(std::sync::Mutex::new(
        super::format::AnthropicSseTranslator::new(),
    ));
    let out = byte_stream.filter_map(move |chunk| {
        let translator = translator.clone();
        async move {
            match chunk {
                Ok(bytes) => {
                    let converted = translator.lock().unwrap().push(&bytes);
                    if converted.is_empty() {
                        None
                    } else {
                        Some(Ok(converted))
                    }
                }
                Err(e) => Some(Err(std::io::Error::other(e.to_string()))),
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(Body::from_stream(out))
        .unwrap()
}

/// Convert OpenAI chat completion response to Anthropic Messages format.
async fn convert_openai_to_anthropic_response(openai_resp: Response) -> Response {
    let status = openai_resp.status();
    let body_bytes = axum::body::to_bytes(openai_resp.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();

    let openai: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => {
            return Response::builder()
                .status(status)
                .header("Content-Type", "application/json")
                .body(Body::from(body_bytes))
                .unwrap();
        }
    };

    // Build Anthropic-format response
    let anthropic_resp = super::format::openai_response_to_anthropic(&openai);

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&anthropic_resp).unwrap()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(result.messages[1].role, "user");
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
        assert_eq!(result.messages[0].content, "Hello\nWorld");
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
        assert_eq!(result.messages[0].content, "Just one block");
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
        assert_eq!(result.messages[0].content, "plain string");
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

        let converted = convert_openai_to_anthropic_response(resp).await;
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

        let converted = convert_openai_stream_to_anthropic(resp);
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
}
