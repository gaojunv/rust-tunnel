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
                    block.get("type")
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
    let anthropic_model = body.get("model")
        .and_then(|v| v.as_str())
        .ok_or("model is required")?;

    let messages_raw = body.get("messages")
        .ok_or("messages is required")?;

    // Parse Anthropic messages with flexible content field (string or content blocks)
    let messages: Vec<ChatMessage> = messages_raw
        .as_array()
        .ok_or("messages must be an array")?
        .iter()
        .map(|msg| {
            let role = msg.get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user")
                .to_string();
            let content = msg.get("content")
                .map(extract_content)
                .unwrap_or_default();
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

    let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    Ok(ChatCompletionRequest {
        model: anthropic_model.to_string(),
        messages: all_messages,
        stream,
        max_tokens: body.get("max_tokens").and_then(|v| v.as_u64()).map(|v| v as u32),
        temperature: body.get("temperature").and_then(|v| v.as_f64()).map(|v| v as f32),
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
        return error_response(StatusCode::NOT_FOUND, "Not found".into(), "invalid_request_error");
    }

    // Validate API key
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if validate_api_key(&state.llm, auth).await.is_none() {
        return error_response(StatusCode::UNAUTHORIZED, "Invalid API key".into(), "authentication_error");
    }

    // Convert Anthropic → OpenAI format
    let request = match anthropic_to_openai(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e, "invalid_request_error"),
    };

    // Resolve model → provider
    let (provider, actual_model) = match resolve_model(&state.llm, &request.model).await {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::NOT_FOUND, e, "invalid_request_error"),
    };

    // Use actual upstream model name
    let mut request = request;
    request.model = actual_model;

    // Call upstream (providers use OpenAI format)
    match call_upstream(&provider.base_url, &provider.api_key, &request).await {
        Ok(resp) => {
            // For non-streaming responses, convert OpenAI format → Anthropic format
            if !request.stream {
                convert_openai_to_anthropic_response(resp).await
            } else {
                // For streaming, relay SSE as-is
                resp
            }
        }
        Err((status, msg)) => error_response(status, msg, "upstream_error"),
    }
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
    let content: Vec<serde_json::Value> = openai["choices"]
        .as_array()
        .map(|choices| {
            choices.iter().map(|c| {
                serde_json::json!({
                    "type": "text",
                    "text": c["message"]["content"].as_str().unwrap_or(""),
                })
            }).collect()
        })
        .unwrap_or_default();

    let anthropic_resp = serde_json::json!({
        "id": openai["id"].as_str().unwrap_or(""),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": openai["model"].as_str().unwrap_or(""),
        "stop_reason": openai["choices"][0]["finish_reason"].as_str().unwrap_or("stop"),
        "stop_sequence": null,
        "usage": {
            "input_tokens": openai["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            "output_tokens": openai["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        },
    });

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
}
