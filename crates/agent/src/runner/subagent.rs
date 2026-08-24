//! Sub-agent loop: independent contexts for batched `task` tool calls.
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::session::SessionRuntime;
use crate::{roles, sse, tools, AgentState};
use crate::llm::{ChatCompletionRequest, ChatMessage, LlmState};
use super::{
    handle_single_tool_call, is_sse_line, is_sse_response, parse_llm_turn, record_tool_result,
    runner_usage_ctx, send_tool_call_delta, with_parent, LineBuf,
    LlmTurn, ParsedToolCall,
    MAX_SUBAGENT_ROUNDS, TASK_SUMMARY_MAX_CHARS,
};
use super::exec_group::exec_readonly_group;

pub fn clone_sub_rt(rt: &SessionRuntime) -> SessionRuntime {
    SessionRuntime {
        session_id: rt.session_id.clone(),
        workspace_id: rt.workspace_id.clone(),
        client_id: rt.client_id.clone(),
        runtime_type: rt.runtime_type.clone(),
        root_path: rt.root_path.clone(),
        docker_container: rt.docker_container.clone(),
        model: rt.model.clone(),
        approval_mode: rt.approval_mode.clone(),
        todos: rt.todos.clone(),
        agents_md: rt.agents_md.clone(),
        memory_block: rt.memory_block.clone(),
        skill_list_block: rt.skill_list_block.clone(),
        wiki_list_block: rt.wiki_list_block.clone(),
        roles_block: rt.roles_block.clone(),
        messages: rt.messages.clone(),
        depth: rt.depth,
        parent_tool_call_id: rt.parent_tool_call_id.clone(),
        file_hashes: rt.file_hashes.clone(),
        active_role: rt.active_role.clone(),
    }
}

/// 子 agent 循环：独立 messages 上下文、共享 workspace 锁（经同一 executor 路径）、
/// 子 agent 的工具调用处理器：简化版 handle_tool_calls，无 task 批处理、无 persist、
/// 无 todo_write 短路（schema 已裁剪）。避免与 run_subagent_loop 形成递归 async 循环。
async fn handle_subagent_tool_calls(
    agent: &AgentState,
    llm: &Arc<LlmState>,
    rt: &mut SessionRuntime,
    ws_tx: &mpsc::Sender<serde_json::Value>,
    calls: Vec<ParsedToolCall>,
    mut raw_calls: Vec<serde_json::Value>,
    reasoning: &str,
) -> Result<(), String> {
    // reasoning 非空时发 thought 帧（不落库）
    if !reasoning.is_empty() {
        let mut thought_frame = serde_json::json!({
            "type": "assistant_chunk",
            "content": reasoning,
            "thought": true,
            "final": false,
        });
        with_parent(&mut thought_frame, rt);
        let _ = ws_tx.send(thought_frame).await;
    }

    // is_subagent 注入（子 agent 的 task 已被 schema 裁剪，此处为防御性保留）
    for raw in &mut raw_calls {
        if raw.pointer("/function/name").and_then(|v| v.as_str()) == Some("task") {
            raw["is_subagent"] = serde_json::Value::Bool(true);
        }
    }

    rt.messages.push(ChatMessage {
        role: "assistant".into(),
        content: None,
        reasoning_content: None,
        tool_calls: Some(raw_calls),
        tool_call_id: None,
        name: None,
    });

    // 预分类：只读（可并发）或串行
    let parallel_flags: Vec<bool> = calls
        .iter()
        .map(|c| match tools::parse_tool_call(&c.name, &c.args) {
            Ok(cmd) => super::super::approval::is_readonly_command(&cmd),
            Err(_) => false,
        })
        .collect();

    let segments = super::super::approval::partition_tool_calls(&parallel_flags);
    for (start, end, parallel) in segments {
        let group = &calls[start..end];
        if !parallel {
            for call in group {
                handle_single_tool_call(agent, llm, rt, ws_tx, call, false).await?;
            }
            continue;
        }
        // 只读并发段
        for call in group {
            let mut frame = serde_json::json!({
                "type": "tool_call",
                "id": &call.id,
                "name": &call.name,
                "args": &call.args,
            });
            with_parent(&mut frame, rt);
            let _ = ws_tx.send(frame).await;
        }
        let results = exec_readonly_group(agent.clone(), rt, group).await;
        for (call, text) in group.iter().zip(results) {
            let mut result_frame = serde_json::json!({
                "type": "tool_result",
                "id": &call.id,
                "name": &call.name,
                "result": &text,
            });
            with_parent(&mut result_frame, rt);
            let _ = ws_tx.send(result_frame).await;
            record_tool_result(agent, rt, &call.id, &call.name, text, false).await;
        }
    }
    Ok(())
}

/// 跑完只把摘要回填主上下文。子循环可用全量工具（除 task/todo_write，schema 裁剪）、
/// 继承主会话 approval_mode 审批、执行过程通过 WS 帧带 parent_tool_call_id 透出。
/// `role`：子 agent 角色（含 allow/deny/model_override/system_prompt），None 用默认。
pub async fn run_subagent_loop(
    agent: &AgentState,
    llm: &Arc<LlmState>,
    parent_rt: &SessionRuntime,
    task_prompt: &str,
    parent_tool_call_id: &str,
    ws_tx: &mpsc::Sender<serde_json::Value>,
    role: Option<&crate::db::roles::AgentRoleRecord>,
) -> Result<String, String> {
    let system_prompt = roles::subagent_system_prompt(role);
    let model_override = role.and_then(|r| r.model_override.as_deref());
    let mut sub_rt = SessionRuntime::subagent(parent_rt, system_prompt, task_prompt, parent_tool_call_id, model_override);

    // 工具 schema：角色过滤 → 客户端版本过滤 → 裁剪 task 与 todo_write（子循环不需要）
    let allow = role.and_then(|r| roles::parse_tools_list(r.tools_allow.as_deref()));
    let deny = role.and_then(|r| roles::parse_tools_list(r.tools_deny.as_deref()));
    let client_ver = agent.registry.client_handle(&sub_rt.client_id).await.and_then(|h| h.client_version);
    let all_tools = tools::filter_tools_for_client_version(
        tools::agent_tools_schema_filtered(&sub_rt.approval_mode, allow.as_deref(), deny.as_deref()),
        client_ver.as_deref(),
    );
    let filtered_tools: Vec<serde_json::Value> = all_tools
        .into_iter()
        .filter(|t| {
            let name = t["function"]["name"].as_str().unwrap_or("");
            name != "task" && name != "todo_write"
        })
        .collect();

    for _round in 0..MAX_SUBAGENT_ROUNDS {
        let chain = crate::llm::router::resolve_with_failover(llm, &sub_rt.model)
            .await
            .map_err(|e| format!("model resolution failed: {e}"))?;

        let request = ChatCompletionRequest {
            model: sub_rt.model.clone(),
            messages: sub_rt.messages.clone(),
            stream: true,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: Some(filtered_tools.clone()),
            tool_choice: None,
            raw_body: None,
        };
        let req_body = crate::llm::upstream::build_upstream_body(&request);
        let outcome = crate::llm::upstream::execute_with_failover(
            &llm.upstream_client,
            &llm.breakers,
            &llm.known_failures,
            &chain,
            &req_body,
            true,
            None,
        )
        .await;

        let (resp, usage_ctx, usage_started) = match outcome {
            crate::llm::upstream::FailoverOutcome::Success { resp, candidate, failed_over, .. } => {
                let ctx = runner_usage_ctx(
                    &candidate,
                    &sub_rt.model,
                    if failed_over { Some(chain.candidates[0].model_name.clone()) } else { None },
                );
                (resp, Some(ctx), Some(std::time::Instant::now()))
            }
            crate::llm::upstream::FailoverOutcome::Exhausted { status, message, .. } => {
                // 上下文溢出自愈：内存级降级（清除最老 tool 消息）
                if super::super::compact::is_context_overflow(status.as_u16(), &message)
                    && subagent_compact_messages(&mut sub_rt)
                {
                    continue;
                }
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
            // 流式路径：聚合 SSE
            use futures_util::StreamExt;
            let mut agg = sse::SseAggregator::new();
            let mut line_buf = LineBuf::default();
            let mut byte_stream = resp.into_body().into_data_stream();
            let mut sse_confirmed = false;
            let mut non_sse_buf: Option<Vec<u8>> = None;
            let mut fatal = false;
            let mut fatal_msg = String::new();

            'sse: while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        fatal = true;
                        fatal_msg = format!("stream read failed: {e}");
                        break 'sse;
                    }
                };
                if let Some(buf) = &mut non_sse_buf {
                    buf.extend_from_slice(&chunk);
                    if buf.len() > sse::MAX_STREAM_BYTES {
                        fatal = true;
                        fatal_msg = "stream size limit exceeded".to_string();
                        break 'sse;
                    }
                    continue;
                }
                if !sse_confirmed && line_buf.has_non_sse_prefix() {
                    sse_confirmed = true;
                    let mut buf = line_buf.take_pending();
                    buf.extend_from_slice(&chunk);
                    non_sse_buf = Some(buf);
                    continue;
                }
                let lines = line_buf.feed(&chunk);
                if line_buf.overflowed {
                    fatal = true;
                    fatal_msg = "stream line exceeded size limit".to_string();
                    break 'sse;
                }
                for line in lines {
                    if !sse_confirmed {
                        if is_sse_line(&line) {
                            sse_confirmed = true;
                        } else {
                            sse_confirmed = true;
                            let mut buf = line.as_bytes().to_vec();
                            buf.extend_from_slice(line_buf.pending());
                            non_sse_buf = Some(buf);
                            break;
                        }
                    }
                    match agg.feed_line(&line) {
                        sse::SseFeed::Content(delta) => {
                            let mut frame = serde_json::json!({"type": "assistant_chunk", "content": delta, "final": false});
                            with_parent(&mut frame, &sub_rt);
                            let _ = ws_tx.send(frame).await;
                        }
                        sse::SseFeed::Thought { reasoning, content } => {
                            let mut frame = serde_json::json!({"type": "assistant_chunk", "content": reasoning, "thought": true, "final": false});
                            with_parent(&mut frame, &sub_rt);
                            let _ = ws_tx.send(frame).await;
                            if let Some(c) = content {
                                let mut frame = serde_json::json!({"type": "assistant_chunk", "content": c, "final": false});
                                with_parent(&mut frame, &sub_rt);
                                let _ = ws_tx.send(frame).await;
                            }
                        }
                        sse::SseFeed::ToolCallDelta { calls, content } => {
                            send_tool_call_delta(ws_tx, calls, content, sub_rt.parent_tool_call_id.as_deref()).await;
                        }
                        sse::SseFeed::Done => break 'sse,
                        sse::SseFeed::Overflow => {
                            fatal = true;
                            fatal_msg = "stream size limit exceeded".to_string();
                            break 'sse;
                        }
                        sse::SseFeed::None => {}
                    }
                }
            }

            if non_sse_buf.is_none() && !sse_confirmed && line_buf.has_non_sse_prefix() {
                non_sse_buf = Some(line_buf.take_pending());
            }

            if let Some(buf) = non_sse_buf {
                let body: serde_json::Value = serde_json::from_slice(&buf)
                    .map_err(|e| format!("invalid LLM response JSON: {e}"))?;
                // 记录用量（非 SSE 嗅探路径：从 body 提取 usage）
                if let (Some(ctx), Some(db), Some(started)) = (usage_ctx, llm.db.as_ref(), usage_started) {
                    ctx.record_success(db, crate::llm::usage::extract_usage_from_body(&body), started);
                }
                match parse_llm_turn(&body)? {
                    LlmTurn::Text(text) => {
                        sub_rt.messages.push(ChatMessage::text("assistant", &text));
                        let mut frame = serde_json::json!({"type": "assistant_chunk", "content": "", "final": true});
                        with_parent(&mut frame, &sub_rt);
                        let _ = ws_tx.send(frame).await;
                        return Ok(truncate_summary(text));
                    }
                    LlmTurn::ToolCalls(calls) => {
                        let raw_calls = body["choices"][0]["message"]["tool_calls"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        let reasoning = body["choices"][0]["message"]["reasoning_content"]
                            .as_str()
                            .unwrap_or("");
                        handle_subagent_tool_calls(agent, llm, &mut sub_rt, ws_tx, calls, raw_calls, reasoning).await?;
                        continue;
                    }
                }
            }

            if fatal {
                return Err(fatal_msg);
            }

            if let Some(last) = line_buf.flush() {
                match agg.feed_line(&last) {
                    sse::SseFeed::Content(delta) => {
                        let mut frame = serde_json::json!({"type": "assistant_chunk", "content": delta, "final": false});
                        with_parent(&mut frame, &sub_rt);
                        let _ = ws_tx.send(frame).await;
                    }
                    sse::SseFeed::Thought { reasoning, content } => {
                        let mut frame = serde_json::json!({"type": "assistant_chunk", "content": reasoning, "thought": true, "final": false});
                        with_parent(&mut frame, &sub_rt);
                        let _ = ws_tx.send(frame).await;
                        if let Some(c) = content {
                            let mut frame = serde_json::json!({"type": "assistant_chunk", "content": c, "final": false});
                            with_parent(&mut frame, &sub_rt);
                            let _ = ws_tx.send(frame).await;
                        }
                    }
                    sse::SseFeed::ToolCallDelta { calls, content } => {
                        send_tool_call_delta(ws_tx, calls, content, sub_rt.parent_tool_call_id.as_deref()).await;
                    }
                    sse::SseFeed::Overflow => {
                        return Err("stream size limit exceeded".to_string());
                    }
                    sse::SseFeed::Done | sse::SseFeed::None => {}
                }
            }

            let turn = agg.finish()?;
            // 记录用量（SSE 路径：usage 从聚合器提取；fatal 已在上游 return）
            if let (Some(ctx), Some(db), Some(started)) = (usage_ctx, llm.db.as_ref(), usage_started) {
                ctx.record_success(db, turn.usage, started);
            }
            if turn.tool_calls.is_empty() {
                sub_rt.messages.push(ChatMessage::text("assistant", &turn.text));
                let mut frame = serde_json::json!({"type": "assistant_chunk", "content": "", "final": true});
                with_parent(&mut frame, &sub_rt);
                let _ = ws_tx.send(frame).await;
                return Ok(truncate_summary(turn.text));
            }
            handle_subagent_tool_calls(agent, llm, &mut sub_rt, ws_tx, turn.tool_calls, turn.raw_tool_calls, &turn.reasoning).await?;
            continue;
        }

        // 非 SSE 回退
        let body_bytes = axum::body::to_bytes(resp.into_body(), sse::MAX_STREAM_BYTES)
            .await
            .map_err(|e| format!("failed to read LLM response: {e}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body_bytes)
            .map_err(|e| format!("invalid LLM response JSON: {e}"))?;
        // 记录用量（非 SSE 回退路径：从 body 提取 usage）
        if let (Some(ctx), Some(db), Some(started)) = (usage_ctx, llm.db.as_ref(), usage_started) {
            ctx.record_success(db, crate::llm::usage::extract_usage_from_body(&body), started);
        }
        match parse_llm_turn(&body)? {
            LlmTurn::Text(text) => {
                sub_rt.messages.push(ChatMessage::text("assistant", &text));
                let mut frame = serde_json::json!({"type": "assistant_chunk", "content": "", "final": true});
                with_parent(&mut frame, &sub_rt);
                let _ = ws_tx.send(frame).await;
                return Ok(truncate_summary(text));
            }
            LlmTurn::ToolCalls(calls) => {
                let raw_calls = body["choices"][0]["message"]["tool_calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let reasoning = body["choices"][0]["message"]["reasoning_content"]
                    .as_str()
                    .unwrap_or("");
                handle_subagent_tool_calls(agent, llm, &mut sub_rt, ws_tx, calls, raw_calls, reasoning).await?;
            }
        }
    }

    // 轮数耗尽：无 tools 的 LLM 调用取最终摘要
    sub_rt.messages.push(ChatMessage::text("user", "You have used all available rounds. Produce your final summary now."));
    let chain = crate::llm::router::resolve_with_failover(llm, &sub_rt.model)
        .await
        .map_err(|e| format!("model resolution failed: {e}"))?;
    let request = ChatCompletionRequest {
        model: sub_rt.model.clone(),
        messages: sub_rt.messages.clone(),
        stream: false,
        max_tokens: None,
        temperature: None,
        top_p: None,
        tools: None,
        tool_choice: None,
        raw_body: None,
    };
    let req_body = crate::llm::upstream::build_upstream_body(&request);
    let outcome = crate::llm::upstream::execute_with_failover(
        &llm.upstream_client,
        &llm.breakers,
        &llm.known_failures,
        &chain,
        &req_body,
        false,
        None,
    )
    .await;
    match outcome {
        crate::llm::upstream::FailoverOutcome::Success { resp, candidate, failed_over, .. } => {
            let started = std::time::Instant::now();
            let body_bytes = axum::body::to_bytes(resp.into_body(), sse::MAX_STREAM_BYTES)
                .await
                .map_err(|e| format!("failed to read LLM response: {e}"))?;
            let body: serde_json::Value = serde_json::from_slice(&body_bytes)
                .map_err(|e| format!("invalid LLM response JSON: {e}"))?;
            // 记录用量（轮数耗尽的最终摘要调用）
            if let Some(db) = llm.db.as_ref() {
                let ctx = runner_usage_ctx(
                    &candidate,
                    &sub_rt.model,
                    if failed_over { Some(chain.candidates[0].model_name.clone()) } else { None },
                );
                ctx.record_success(db, crate::llm::usage::extract_usage_from_body(&body), started);
            }
            let text = parse_llm_turn(&body).and_then(|turn| match turn {
                LlmTurn::Text(t) => Ok(t),
                _ => Err("expected text response".to_string()),
            })?;
            Ok(truncate_summary(text))
        }
        crate::llm::upstream::FailoverOutcome::Exhausted { message, .. } => {
            Err(format!("LLM unavailable: {message}"))
        }
    }
}

/// 子 agent 内存级上下文降级：把最老的 role=="tool" 消息内容替换为
/// "[old tool output cleared]"（最多清到剩余工具消息 ≤ KEEP 范围）。
/// 返回 true 表示有空间可重试，false 表示无可压缩段。
pub(crate) fn subagent_compact_messages(rt: &mut SessionRuntime) -> bool {
    const KEEP_RECENT_TOOL: usize = 4;
    // 收集所有 tool 消息的索引（从旧到新）
    let tool_indices: Vec<usize> = rt
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "tool")
        .map(|(i, _)| i)
        .collect();
    if tool_indices.len() <= KEEP_RECENT_TOOL {
        return false; // 无可压缩空间
    }
    // 保留最新的 KEEP_RECENT_TOOL 条，其余清空内容
    let clear_count = tool_indices.len() - KEEP_RECENT_TOOL;
    for &idx in &tool_indices[..clear_count] {
        if let Some(content) = &mut rt.messages[idx].content {
            *content = "[old tool output cleared]".to_string();
        }
    }
    true
}

/// 截断子 agent 摘要到 TASK_SUMMARY_MAX_CHARS。
pub(crate) fn truncate_summary(text: String) -> String {
    if text.len() <= TASK_SUMMARY_MAX_CHARS {
        return text;
    }
    let mut cut = TASK_SUMMARY_MAX_CHARS;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[... truncated]", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subagent_schema_excludes_task_and_todo_write() {
        // 子循环的工具 schema 应裁剪 task 与 todo_write
        let all_tools = tools::agent_tools_schema("safe");
        let filtered: Vec<&str> = all_tools
            .iter()
            .filter(|t| {
                let name = t["function"]["name"].as_str().unwrap_or("");
                name != "task" && name != "todo_write"
            })
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(!filtered.contains(&"task"), "task should be filtered out");
        assert!(!filtered.contains(&"todo_write"), "todo_write should be filtered out");
        // 其他工具应保留
        assert!(filtered.contains(&"shell"));
        assert!(filtered.contains(&"read_file"));
    }

    #[test]
    fn test_truncate_summary_short_unchanged() {
        let text = "short summary".to_string();
        assert_eq!(truncate_summary(text.clone()), text);
    }

    #[test]
    fn test_truncate_summary_long_truncated() {
        let text = "x".repeat(TASK_SUMMARY_MAX_CHARS + 100);
        let result = truncate_summary(text);
        assert!(result.len() < TASK_SUMMARY_MAX_CHARS + 100);
        assert!(result.contains("[... truncated]"));
        // 截断点在 UTF-8 边界
        assert!(!result.ends_with('x') || result.ends_with("x\n[... truncated]"));
    }

    #[test]
    fn test_truncate_summary_multibyte_safe() {
        let text = "汉".repeat(TASK_SUMMARY_MAX_CHARS / 3 + 100);
        let result = truncate_summary(text);
        assert!(result.contains("[... truncated]"));
    }

    #[test]
    fn test_subagent_compact_messages_clears_old_tool_msgs() {
        let mut rt = crate::session::SessionRuntime {
            session_id: "s1".into(),
            workspace_id: "w1".into(),
            client_id: "c1".into(),
            runtime_type: "host".into(),
            root_path: "/p".into(),
            docker_container: None,
            model: "m".into(),
            approval_mode: "safe".into(),
            todos: vec![],
            agents_md: None,
            memory_block: None,
            skill_list_block: None,
            wiki_list_block: None,
            roles_block: None,
            messages: vec![],
            depth: 1,
            parent_tool_call_id: Some("p1".into()),
            file_hashes: std::collections::HashMap::new(),
            active_role: None,
        };
        // 添加 8 条 tool 消息
        for i in 0..8 {
            rt.messages.push(ChatMessage {
                role: "tool".into(),
                content: Some(format!("result_{i}")),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some(format!("c{i}")),
                name: Some("shell".into()),
            });
        }
        let did_compact = subagent_compact_messages(&mut rt);
        assert!(did_compact);
        // 前 4 条应被清空，后 4 条保留
        assert_eq!(
            rt.messages[0].content.as_deref(),
            Some("[old tool output cleared]")
        );
        assert_eq!(
            rt.messages[3].content.as_deref(),
            Some("[old tool output cleared]")
        );
        assert_eq!(rt.messages[4].content.as_deref(), Some("result_4"));
        assert_eq!(rt.messages[7].content.as_deref(), Some("result_7"));
    }

    #[test]
    fn test_subagent_compact_messages_noop_when_few() {
        let mut rt = crate::session::SessionRuntime {
            session_id: "s1".into(),
            workspace_id: "w1".into(),
            client_id: "c1".into(),
            runtime_type: "host".into(),
            root_path: "/p".into(),
            docker_container: None,
            model: "m".into(),
            approval_mode: "safe".into(),
            todos: vec![],
            agents_md: None,
            memory_block: None,
            skill_list_block: None,
            wiki_list_block: None,
            roles_block: None,
            messages: vec![],
            depth: 1,
            parent_tool_call_id: Some("p1".into()),
            file_hashes: std::collections::HashMap::new(),
            active_role: None,
        };
        for i in 0..3 {
            rt.messages.push(ChatMessage {
                role: "tool".into(),
                content: Some(format!("result_{i}")),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some(format!("c{i}")),
                name: Some("shell".into()),
            });
        }
        let did_compact = subagent_compact_messages(&mut rt);
        assert!(!did_compact, "should not compact when <= KEEP_RECENT_TOOL");
        // 所有内容应保留
        for i in 0..3 {
            assert_eq!(
                rt.messages[i].content.as_deref(),
                Some(format!("result_{i}").as_str())
            );
        }
    }

    #[test]
    fn test_clone_sub_rt_copies_key_fields() {
        let rt = crate::session::SessionRuntime {
            session_id: "s1".into(),
            workspace_id: "w1".into(),
            client_id: "c1".into(),
            runtime_type: "docker".into(),
            root_path: "/container".into(),
            docker_container: Some("ctr1".into()),
            model: "gpt-4o".into(),
            approval_mode: "full_auto".into(),
            todos: vec![crate::tools::TodoItem {
                content: "task1".into(),
                status: "in_progress".into(),
                active_form: None,
            }],
            agents_md: Some("agents".into()),
            memory_block: None,
            skill_list_block: None,
            wiki_list_block: None,
            roles_block: None,
            messages: vec![ChatMessage::text("user", "hello")],
            depth: 1,
            parent_tool_call_id: Some("p1".into()),
            file_hashes: std::collections::HashMap::new(),
            active_role: None,
        };
        let cloned = clone_sub_rt(&rt);
        assert_eq!(cloned.session_id, "s1");
        assert_eq!(cloned.workspace_id, "w1");
        assert_eq!(cloned.client_id, "c1");
        assert_eq!(cloned.runtime_type, "docker");
        assert_eq!(cloned.docker_container.as_deref(), Some("ctr1"));
        assert_eq!(cloned.model, "gpt-4o");
        assert_eq!(cloned.approval_mode, "full_auto");
        assert_eq!(cloned.depth, 1);
        assert_eq!(cloned.parent_tool_call_id.as_deref(), Some("p1"));
        assert_eq!(cloned.todos.len(), 1);
        assert_eq!(cloned.messages.len(), 1);
    }

}
