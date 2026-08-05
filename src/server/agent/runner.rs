//! Agent main loop: LLM call → tool execution over tunnel → feed results back.
use std::sync::Arc;
use tokio::sync::mpsc;

use super::{compact, executor, session::SessionRuntime, sse, tools, AgentState};
use crate::common::AgentResult;
use crate::server::llm::{ChatCompletionRequest, ChatMessage, LlmState};

/// 按行切分 SSE 字节流：HTTP chunk 边界可切断一行，未完结部分留缓冲。
#[derive(Default)]
struct LineBuf {
    pending: Vec<u8>,
}

impl LineBuf {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.pending[..pos]).into_owned();
            self.pending.drain(..=pos);
            if !line.trim().is_empty() {
                lines.push(line);
            }
        }
        lines
    }

    fn flush(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let line = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        if line.trim().is_empty() {
            None
        } else {
            Some(line)
        }
    }
}

fn is_sse_response(content_type: &str) -> bool {
    content_type.starts_with("text/event-stream")
}

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

/// 执行一轮工具调用：回填 assistant tool_calls 消息、逐个执行并落库/回填 tool 结果。
async fn handle_tool_calls(
    agent: &AgentState,
    rt: &mut SessionRuntime,
    ws_tx: &mpsc::Sender<serde_json::Value>,
    calls: Vec<ParsedToolCall>,
    raw_calls: Vec<serde_json::Value>,
) -> Result<(), String> {
    rt.messages.push(ChatMessage {
        role: "assistant".into(),
        content: None,
        tool_calls: Some(raw_calls.clone()),
        tool_call_id: None,
        name: None,
    });
    persist_message(
        agent,
        &rt.session_id,
        "assistant",
        "",
        Some(&serde_json::to_string(&raw_calls).unwrap_or_default()),
        None,
        None,
        "tool_calls",
    )
    .await;

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
                // docker 运行时但容器未启动（container_id 为空）→ 直接报错，
                // 避免静默回退到宿主机执行。
                let result = if rt.runtime_type == "docker" && rt.docker_container.is_none() {
                    AgentResult::Error {
                        message: "docker container not started".into(),
                    }
                } else {
                    executor::exec_on_client(
                        agent,
                        &rt.workspace_id,
                        &rt.client_id,
                        &rt.root_path,
                        rt.docker_container.as_deref(),
                        command,
                    )
                    .await
                };
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

        persist_message(
            agent,
            &rt.session_id,
            "tool",
            &result_text,
            None,
            Some(&call.id),
            Some(&call.name),
            "tool_result",
        )
        .await;
        rt.messages.push(ChatMessage {
            role: "tool".into(),
            content: Some(result_text),
            tool_calls: None,
            tool_call_id: Some(call.id.clone()),
            name: Some(call.name.clone()),
        });
    }
    Ok(())
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
        // 每轮 LLM 调用前检查上下文超限 → 压缩早期历史（失败降级截断，不阻断回合）
        compact::maybe_compact(&agent, &llm, rt, &ws_tx).await?;
        let chain = crate::server::llm::router::resolve_with_failover(&llm, &rt.model)
            .await
            .map_err(|e| format!("model resolution failed: {e}"))?;

        let request = ChatCompletionRequest {
            model: rt.model.clone(),
            messages: rt.messages.clone(),
            stream: true,
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
            true,
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

        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if is_sse_response(&content_type) {
            // ── 流式路径 ──
            use futures_util::StreamExt;
            let mut agg = sse::SseAggregator::new();
            let mut line_buf = LineBuf::default();
            let mut byte_stream = resp.into_body().into_data_stream();
            'sse: while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.map_err(|e| format!("stream read failed: {e}"))?;
                for line in line_buf.feed(&chunk) {
                    match agg.feed_line(&line) {
                        sse::SseFeed::Content(delta) => {
                            let _ = ws_tx
                                .send(serde_json::json!({"type": "assistant_chunk", "content": delta, "final": false}))
                                .await;
                        }
                        sse::SseFeed::Done => break 'sse,
                        sse::SseFeed::None => {}
                    }
                }
            }
            if let Some(last) = line_buf.flush() {
                if let sse::SseFeed::Content(delta) = agg.feed_line(&last) {
                    let _ = ws_tx
                        .send(serde_json::json!({"type": "assistant_chunk", "content": delta, "final": false}))
                        .await;
                }
            }
            let turn = agg.finish()?;
            if turn.tool_calls.is_empty() {
                // 文本回合：收尾 final chunk + 落库 + done
                let _ = ws_tx
                    .send(serde_json::json!({"type": "assistant_chunk", "content": "", "final": true}))
                    .await;
                rt.messages.push(ChatMessage::text("assistant", &turn.text));
                persist_message(
                    &agent,
                    &rt.session_id,
                    "assistant",
                    &turn.text,
                    None,
                    None,
                    None,
                    "message",
                )
                .await;
                let _ = ws_tx.send(serde_json::json!({"type": "done"})).await;
                return Ok(());
            }
            // tool 回合：转成与 parse_llm_turn 相同的处理流（见下）
            handle_tool_calls(&agent, rt, &ws_tx, turn.tool_calls, turn.raw_tool_calls).await?;
            continue;
        }

        // ── 非 SSE 回退（某些上游/代理返回普通 JSON）──
        let body_bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
            .await
            .map_err(|e| format!("failed to read LLM response: {e}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body_bytes)
            .map_err(|e| format!("invalid LLM response JSON: {e}"))?;
        match parse_llm_turn(&body)? {
            LlmTurn::Text(text) => {
                let _ = ws_tx
                    .send(serde_json::json!({"type": "assistant_chunk", "content": &text, "final": true}))
                    .await;
                rt.messages.push(ChatMessage::text("assistant", &text));
                persist_message(
                    &agent,
                    &rt.session_id,
                    "assistant",
                    &text,
                    None,
                    None,
                    None,
                    "message",
                )
                .await;
                let _ = ws_tx.send(serde_json::json!({"type": "done"})).await;
                return Ok(());
            }
            LlmTurn::ToolCalls(calls) => {
                let raw_calls = body["choices"][0]["message"]["tool_calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                handle_tool_calls(&agent, rt, &ws_tx, calls, raw_calls).await?;
            }
        }
    }

    let _ = ws_tx
        .send(serde_json::json!({"type": "error", "message": "tool round limit reached"}))
        .await;
    Err("tool round limit reached".to_string())
}

#[allow(clippy::too_many_arguments)]
async fn persist_message(
    agent: &AgentState,
    session_id: &str,
    role: &str,
    content: &str,
    tool_calls: Option<&str>,
    tool_call_id: Option<&str>,
    name: Option<&str>,
    kind: &str,
) {
    let id = format!("{:032x}", rand::random::<u128>());
    if let Err(e) = agent
        .db
        .agent_add_message_v2(
            &id,
            session_id,
            role,
            content,
            tool_calls,
            tool_call_id,
            name,
            kind,
        )
        .await
    {
        tracing::warn!("failed to persist agent message: {}", e);
    }
}

/// 落库一行 kind='summary' 的消息（压缩模块用）。
pub async fn runner_persist_summary(agent: &AgentState, session_id: &str, content: &str) {
    persist_message(
        agent, session_id, "user", content, None, None, None, "summary",
    )
    .await;
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

    #[test]
    fn test_is_sse_response() {
        assert!(is_sse_response("text/event-stream; charset=utf-8"));
        assert!(is_sse_response("text/event-stream"));
        assert!(!is_sse_response("application/json"));
    }

    #[test]
    fn test_line_splitter_handles_partial_chunks() {
        // HTTP chunk 边界可能切断 SSE 行：缓冲拼行
        let mut buf = LineBuf::default();
        assert!(!buf.feed(b"data: {\"a\":1}\r\n\r\nda").is_empty());
        // 第一行完整产出，"da" 留在缓冲
        let lines = buf.feed(b"ta: [DONE]\n");
        assert!(lines.iter().any(|l| l.contains("[DONE]")));
    }

    #[test]
    fn test_flush_feeds_final_delta_to_aggregator() {
        // 回归：flush() 丢弃返回值会丢最后一行的 content delta 推送。
        // 组合验证 flush 取行 → feed_line 返回 Content（修复路径的行为，不测 ws 发送）。
        let mut buf = LineBuf::default();
        // 无换行的完整 data 行：feed 不会产出，留在缓冲
        let line = r#"data: {"choices":[{"delta":{"content":"收尾"},"index":0}]}"#;
        assert!(buf.feed(line.as_bytes()).is_empty());
        let flushed = buf.flush().expect("flush should return buffered line");
        assert_eq!(flushed, line);

        let mut agg = sse::SseAggregator::new();
        match agg.feed_line(&flushed) {
            sse::SseFeed::Content(delta) => assert_eq!(delta, "收尾"),
            sse::SseFeed::None => panic!("expected Content delta, got None"),
            sse::SseFeed::Done => panic!("expected Content delta, got Done"),
        }
    }

    #[tokio::test]
    async fn test_persist_message_v2_writes_all_columns() {
        let db = crate::server::db::Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let agent = test_agent_state(db.clone()).await;
        persist_message(
            &agent,
            "s1",
            "tool",
            "exit_code=0",
            None,
            Some("call_1"),
            Some("shell"),
            "tool_result",
        )
        .await;

        let msgs = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, "tool_result");
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msgs[0].name.as_deref(), Some("shell"));
    }

    async fn test_agent_state(db: crate::server::db::Database) -> AgentState {
        let server_state = crate::server::control::ServerState::with_db(db);
        server_state.agent_state.expect("agent_state initialized")
    }
}
