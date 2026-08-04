//! Agent main loop: LLM call → tool execution over tunnel → feed results back.
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::server::llm::{ChatCompletionRequest, ChatMessage, LlmState};
use crate::common::AgentResult;
use super::{executor, session::SessionRuntime, tools, AgentState};

/// One LLM response, parsed.
pub enum LlmTurn {
    /// Assistant produced final text (no tool calls)
    Text(String),
    /// Assistant wants to call tools
    ToolCalls(Vec<ParsedToolCall>),
}

pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub args: String,
}

impl std::fmt::Debug for LlmTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(t) => write!(f, "Text({t:?})"),
            Self::ToolCalls(c) => write!(f, "ToolCalls({} calls)", c.len()),
        }
    }
}

/// Parse a non-streaming OpenAI chat.completion body into an LlmTurn.
pub fn parse_llm_turn(body: &serde_json::Value) -> Result<LlmTurn, String> {
    let msg = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .ok_or_else(|| "response missing choices[0].message".to_string())?;

    if let Some(calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        if !calls.is_empty() {
            let parsed: Result<Vec<_>, String> = calls
                .iter()
                .map(|c| {
                    Ok(ParsedToolCall {
                        id: c
                            .get("id")
                            .and_then(|v| v.as_str())
                            .ok_or("tool_call missing id")?
                            .to_string(),
                        name: c
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .ok_or("tool_call missing function.name")?
                            .to_string(),
                        args: c
                            .pointer("/function/arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_string(),
                    })
                })
                .collect();
            return Ok(LlmTurn::ToolCalls(parsed?));
        }
    }

    let content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(LlmTurn::Text(content))
}

fn agent_result_to_text(result: &AgentResult) -> String {
    match result {
        AgentResult::Shell {
            stdout,
            stderr,
            exit_code,
        } => format!("exit_code={exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"),
        AgentResult::FileContent { content } => content.clone(),
        AgentResult::Success => "ok".to_string(),
        AgentResult::Error { message } => format!("error: {message}"),
    }
}

/// Run one full agent turn: send current messages to the LLM, execute any tool
/// calls over the tunnel, feed results back, repeat until the model stops
/// calling tools. Progress is streamed to `ws_tx` as JSON messages.
pub async fn run_agent_turn(
    agent: AgentState,
    llm: Arc<LlmState>,
    rt: &mut SessionRuntime,
    ws_tx: mpsc::Sender<serde_json::Value>,
) -> Result<(), String> {
    const MAX_TOOL_ROUNDS: usize = 20;

    for _round in 0..MAX_TOOL_ROUNDS {
        let chain = crate::server::llm::router::resolve_with_failover(&llm, &rt.model)
            .await
            .map_err(|e| format!("model resolution failed: {e}"))?;

        let request = ChatCompletionRequest {
            model: rt.model.clone(),
            messages: rt.messages.clone(),
            stream: false,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: Some(tools::agent_tools_schema()),
            tool_choice: None,
            raw_body: None,
        };
        let req_body = crate::server::llm::upstream::build_upstream_body(&request);
        let outcome = crate::server::llm::upstream::execute_with_failover(
            &llm.breakers,
            &chain,
            &req_body,
            false,
        )
        .await;

        let resp = match outcome {
            crate::server::llm::upstream::FailoverOutcome::Success { resp, .. } => resp,
            crate::server::llm::upstream::FailoverOutcome::Exhausted { message, .. } => {
                let _ = ws_tx
                    .send(serde_json::json!({"type": "error", "message": format!("LLM unavailable: {message}")}))
                    .await;
                return Err(format!("LLM unavailable: {message}"));
            }
        };

        let body_bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
            .await
            .map_err(|e| format!("failed to read LLM response: {e}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body_bytes)
            .map_err(|e| format!("invalid LLM response JSON: {e}"))?;

        match parse_llm_turn(&body)? {
            LlmTurn::Text(text) => {
                let _ = ws_tx
                    .send(serde_json::json!({"type": "assistant_chunk", "content": &text}))
                    .await;
                rt.messages.push(ChatMessage::text("assistant", &text));
                persist_message(&agent, &rt.session_id, "assistant", &text, None).await;
                let _ = ws_tx.send(serde_json::json!({"type": "done"})).await;
                return Ok(());
            }
            LlmTurn::ToolCalls(calls) => {
                // 记录 assistant 的 tool_calls 消息（回填上下文需要）
                let raw_calls = body["choices"][0]["message"]["tool_calls"].clone();
                rt.messages.push(ChatMessage {
                    role: "assistant".into(),
                    content: None,
                    tool_calls: Some(raw_calls.as_array().cloned().unwrap_or_default()),
                    tool_call_id: None,
                    name: None,
                });

                let mut tool_log = Vec::new();
                for call in calls {
                    let _ = ws_tx
                        .send(serde_json::json!({
                            "type": "tool_call",
                            "id": &call.id,
                            "name": &call.name,
                            "args": &call.args,
                        }))
                        .await;

                    let result_text = match tools::parse_tool_call(&call.name, &call.args) {
                        Ok(command) => {
                            let result = executor::exec_on_client(
                                &agent,
                                &rt.workspace_id,
                                &rt.client_id,
                                &rt.root_path,
                                command,
                            )
                            .await;
                            let text = agent_result_to_text(&result);
                            let _ = ws_tx
                                .send(serde_json::json!({
                                    "type": "tool_result",
                                    "id": &call.id,
                                    "name": &call.name,
                                    "result": &text,
                                }))
                                .await;
                            text
                        }
                        Err(e) => {
                            let _ = ws_tx
                                .send(serde_json::json!({
                                    "type": "tool_result",
                                    "id": &call.id,
                                    "name": &call.name,
                                    "result": format!("error: {e}"),
                                }))
                                .await;
                            format!("error: {e}")
                        }
                    };

                    tool_log.push(serde_json::json!({
                        "name": &call.name,
                        "args": &call.args,
                        "result": &result_text,
                    }));
                    rt.messages.push(ChatMessage {
                        role: "tool".into(),
                        content: Some(result_text),
                        tool_calls: None,
                        tool_call_id: Some(call.id.clone()),
                        name: Some(call.name.clone()),
                    });
                }

                persist_message(
                    &agent,
                    &rt.session_id,
                    "tool",
                    "",
                    Some(&serde_json::to_string(&tool_log).unwrap_or_default()),
                )
                .await;
            }
        }
    }

    let _ = ws_tx
        .send(serde_json::json!({"type": "error", "message": "tool round limit reached"}))
        .await;
    Err("tool round limit reached".to_string())
}

async fn persist_message(
    agent: &AgentState,
    session_id: &str,
    role: &str,
    content: &str,
    tool_calls: Option<&str>,
) {
    let id = format!("{:032x}", rand::random::<u128>());
    if let Err(e) = agent
        .db
        .agent_add_message(&id, session_id, role, content, tool_calls)
        .await
    {
        tracing::warn!("failed to persist agent message: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_text_response() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "我来帮你看看"},
                "finish_reason": "stop"
            }]
        });
        let turn = parse_llm_turn(&body).unwrap();
        match turn {
            LlmTurn::Text(t) => assert_eq!(t, "我来帮你看看"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let turn = parse_llm_turn(&body).unwrap();
        match turn {
            LlmTurn::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "shell");
                assert_eq!(calls[0].args, r#"{"cmd":"ls"}"#);
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_malformed() {
        assert!(parse_llm_turn(&serde_json::json!({})).is_err());
        assert!(parse_llm_turn(&serde_json::json!({"choices": []})).is_err());
    }
}
