//! 工具调用处理：参数预览/diff 合成、单调用执行、批量编排（含只读并发组）。

use super::{
    agent_result_to_text, client_supports_edit, client_supports_git_exec,
    client_supports_read_range, client_supports_search_patch, exec_group, executor, mpsc,
    persist_message, record_tool_result, roles, sse, subagent, tools, with_parent, AgentCommand,
    AgentResult, AgentState, Arc, ChatMessage, LlmState, ParsedToolCall, SessionRuntime,
    SubagentFuture, MIN_EDIT_CLIENT_VERSION, MIN_GIT_EXEC_CLIENT_VERSION,
    MIN_READ_RANGE_CLIENT_VERSION, MIN_SEARCH_PATCH_CLIENT_VERSION,
};

/// 把 SseFeed::ToolCallDelta 的增量发送为 WS 帧（主循环与 flush 残留行共用）。
/// `parent`：子 agent 归属的父 tool_call_id（主循环传 None）。
pub(crate) async fn send_tool_call_delta(
    ws_tx: &mpsc::Sender<serde_json::Value>,
    calls: Vec<sse::ToolCallDeltaItem>,
    content: Option<String>,
    parent: Option<&str>,
) {
    for item in calls {
        let mut frame = serde_json::json!({"type": "tool_call_chunk", "index": item.index});
        if let Some(id) = &item.id {
            frame["id"] = serde_json::Value::String(id.clone());
        }
        if let Some(name) = &item.name {
            frame["name"] = serde_json::Value::String(name.clone());
        }
        if let Some(arguments) = &item.arguments {
            frame["arguments"] = serde_json::Value::String(arguments.clone());
        }
        if let Some(p) = parent {
            frame["parent_tool_call_id"] = serde_json::Value::String(p.to_string());
        }
        let _ = ws_tx.send(frame).await;
    }
    if let Some(c) = content {
        let mut frame =
            serde_json::json!({"type": "assistant_chunk", "content": c, "final": false});
        if let Some(p) = parent {
            frame["parent_tool_call_id"] = serde_json::Value::String(p.to_string());
        }
        let _ = ws_tx.send(frame).await;
    }
}

/// 执行一轮工具调用：回填 assistant tool_calls 消息、执行并落库/回填 tool 结果。
/// 连续的只读调用（ReadFile/ListDir/Search/GitStatus/GitDiff/GitExec-Read）
/// 以 bounded 并发执行；写类/审批类保持串行语义。结果落库与 WS 帧发送严格
/// 保持 tool_calls 原顺序。
/// `persist`：false 时跳过 thought/assistant 行落库（子 agent 内存态）。
/// `llm`：task 短路需要调 LLM。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_tool_calls(
    agent: &AgentState,
    llm: &Arc<LlmState>,
    rt: &mut SessionRuntime,
    ws_tx: &mpsc::Sender<serde_json::Value>,
    calls: Vec<ParsedToolCall>,
    mut raw_calls: Vec<serde_json::Value>,
    reasoning: &str,
    persist: bool,
) -> Result<(), String> {
    // reasoning 非空时先落库 thought 行（位于 tool_calls 之前）
    if !reasoning.is_empty() {
        let mut thought_frame = serde_json::json!({
            "type": "assistant_chunk",
            "content": reasoning,
            "thought": true,
            "final": false,
        });
        with_parent(&mut thought_frame, rt);
        let _ = ws_tx.send(thought_frame).await;
        if persist {
            persist_message(
                agent,
                &rt.session_id,
                "assistant",
                reasoning,
                None,
                None,
                Some("thought"),
                "message",
            )
            .await;
        }
    }

    // is_subagent 注入：task 调用在落库前标记 is_subagent=true（持久化行无 parent）
    for raw in &mut raw_calls {
        if raw.pointer("/function/name").and_then(|v| v.as_str()) == Some("task") {
            raw["is_subagent"] = serde_json::Value::Bool(true);
        }
    }

    rt.messages.push(ChatMessage {
        role: "assistant".into(),
        content: None,
        reasoning_content: None,
        tool_calls: Some(raw_calls.clone()),
        tool_call_id: None,
        name: None,
    });
    if persist {
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
    }

    // 预分类：只读（可并发）或串行（remember/use_skill/审批/写操作/解析错误/task）
    let parallel_flags: Vec<bool> = calls
        .iter()
        .map(|c| {
            // task 工具走串行路径（需要 spawn 子循环）
            if c.name == "task" {
                return false;
            }
            match tools::parse_tool_call(&c.name, &c.args) {
                Ok(cmd) => crate::approval::is_readonly_command(&cmd),
                Err(_) => false,
            }
        })
        .collect();

    let segments = crate::approval::partition_tool_calls(&parallel_flags);
    for (start, end, parallel) in segments {
        let group = &calls[start..end];
        if !parallel {
            // 串行路径：检查是否有连续的 task 调用可批量处理
            let mut i = 0;
            while i < group.len() {
                // 收集连续的 task 调用
                if group[i].name == "task" {
                    let batch_start = i;
                    while i < group.len() && group[i].name == "task" {
                        i += 1;
                    }
                    let batch = &group[batch_start..i];
                    // 按序发父卡 tool_call 帧
                    for call in batch {
                        let mut frame = serde_json::json!({
                            "type": "tool_call",
                            "id": &call.id,
                            "name": "task",
                            "args": &call.args,
                            "is_subagent": true,
                        });
                        with_parent(&mut frame, rt);
                        let _ = ws_tx.send(frame).await;
                    }
                    // 并发执行子 agent 循环：join_all 在同一 task 内并发 poll，
                    // 无需 Send；外层 turn future 被 drop 时子 future 随之中止。
                    // 先收集 owned 数据（prompt、sub_rt、call_id、role），再创建借用它们的 future。
                    let mut sub_owned: Vec<(
                        String,
                        SessionRuntime,
                        String,
                        Option<crate::db::roles::AgentRoleRecord>,
                    )> = Vec::new();
                    let mut error_indices: Vec<(usize, String)> = Vec::new();

                    // 批处理前一次性查可见角色列表（避免每个 task 调用都查库）
                    let visible_roles = agent
                        .db
                        .role_list_visible(&rt.client_id, &rt.workspace_id, Some("subagent"))
                        .await
                        .unwrap_or_default();

                    for (bi, call) in batch.iter().enumerate() {
                        match tools::parse_task_args(&call.args) {
                            Ok((agent_name, prompt)) => {
                                // 按 agent 名解析角色：None → 默认（不传角色）
                                let role = agent_name.as_deref().and_then(|name| {
                                    visible_roles.iter().find(|r| r.name == name).cloned()
                                });
                                // 未命中/禁用一律报错（明确告知模型无效角色）
                                if let Some(ref name) = agent_name {
                                    if role.is_none() {
                                        error_indices
                                            .push((bi, format!("unknown sub-agent role '{name}'")));
                                        continue;
                                    }
                                }
                                let sub_rt = subagent::clone_sub_rt(rt);
                                sub_owned.push((prompt, sub_rt, call.id.clone(), role));
                            }
                            Err(e) => {
                                error_indices.push((bi, e));
                            }
                        }
                    }
                    let mut futures: Vec<SubagentFuture<'_>> = Vec::new();
                    for (prompt, sub_rt, call_id, role) in &sub_owned {
                        let fut = subagent::run_subagent_loop(
                            agent,
                            llm,
                            sub_rt,
                            prompt,
                            call_id,
                            ws_tx,
                            role.as_ref(),
                        );
                        futures.push(Box::pin(fut));
                    }
                    let results = futures_util::future::join_all(futures).await;
                    // 按序遍历 batch：error_indices 或 join_all 结果
                    let mut fi = 0;
                    for (bi, call) in batch.iter().enumerate() {
                        let text = if let Some(pos) =
                            error_indices.iter().position(|(idx, _)| *idx == bi)
                        {
                            let (_, e) = error_indices.remove(pos);
                            format!("error: {e}")
                        } else if fi < results.len() {
                            let r = results[fi].clone();
                            fi += 1;
                            match r {
                                Ok(text) => text,
                                Err(e) => format!("[subagent error: {e}]"),
                            }
                        } else {
                            "[subagent error: missing result]".to_string()
                        };
                        let mut result_frame = serde_json::json!({
                            "type": "tool_result",
                            "id": &call.id,
                            "name": "task",
                            "result": &text,
                        });
                        with_parent(&mut result_frame, rt);
                        let _ = ws_tx.send(result_frame).await;
                        record_tool_result(agent, rt, &call.id, "task", text, persist).await;
                    }
                    continue;
                }
                // 非 task 调用照旧走 handle_single_tool_call
                handle_single_tool_call(agent, llm, rt, ws_tx, &group[i], persist).await?;
                i += 1;
            }
            continue;
        }
        // 只读并发段：先按序发 tool_call 帧，再并发执行，最后按序发 tool_result+落库
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
        let results = exec_group::exec_readonly_group(agent.clone(), rt, group).await;
        for (call, text) in group.iter().zip(results) {
            let mut result_frame = serde_json::json!({
                "type": "tool_result",
                "id": &call.id,
                "name": &call.name,
                "result": &text,
            });
            with_parent(&mut result_frame, rt);
            let _ = ws_tx.send(result_frame).await;
            record_tool_result(agent, rt, &call.id, &call.name, text, persist).await;
        }
    }
    Ok(())
}

/// 为审批弹窗构建结构化预览文本：patch_file/edit_file 展示编辑内容，
/// write_file 展示新内容前 20 行，其他工具回退到原始 JSON 截断。
const PREVIEW_MAX_LINES: usize = 10;
const PREVIEW_MAX_TOTAL: usize = 2000;

pub(crate) fn build_args_preview(tool_name: &str, command: &AgentCommand) -> String {
    let truncate_lines = |text: &str, max_lines: usize| -> String {
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() <= max_lines {
            text.to_string()
        } else {
            format!(
                "{}\n[... truncated, {} total lines]",
                lines[..max_lines].join("\n"),
                lines.len()
            )
        }
    };
    let mut preview = match command {
        AgentCommand::PatchFile {
            path,
            old_string,
            new_string,
        } => {
            format!(
                "{path}\n--- old\n{}\n+++ new\n{}",
                truncate_lines(old_string, PREVIEW_MAX_LINES),
                truncate_lines(new_string, PREVIEW_MAX_LINES)
            )
        }
        AgentCommand::EditFile { path, edits, .. } => {
            let mut parts = vec![path.clone()];
            for edit in edits.iter().take(5) {
                parts.push(format!(
                    "--- old\n{}",
                    truncate_lines(&edit.old_string, PREVIEW_MAX_LINES)
                ));
                parts.push(format!(
                    "+++ new\n{}",
                    truncate_lines(&edit.new_string, PREVIEW_MAX_LINES)
                ));
            }
            if edits.len() > 5 {
                parts.push(format!("... ({} more edits)", edits.len() - 5));
            }
            parts.join("\n")
        }
        AgentCommand::WriteFile { path, content }
        | AgentCommand::WriteFile2 { path, content, .. } => {
            let total = content.lines().count();
            format!(
                "{path}\n--- new content ({} lines shown / {total} total)\n{}",
                PREVIEW_MAX_LINES.min(total),
                truncate_lines(content, PREVIEW_MAX_LINES)
            )
        }
        _ => return tool_name.to_string(),
    };
    preview.truncate(PREVIEW_MAX_TOTAL);
    preview
}

/// 为文件编辑工具从命令参数合成 diffs 数组（WS 帧落库用）。
/// 成功时调用方已确认结果为 WriteOutcome 或 Success。
pub(crate) fn synthesize_tool_diffs(
    tool_name: &str,
    command: &AgentCommand,
) -> Option<serde_json::Value> {
    let diffs = match command {
        AgentCommand::PatchFile {
            path,
            old_string,
            new_string,
        } => {
            serde_json::json!([{"path": path, "old_text": old_string, "new_text": new_string}])
        }
        AgentCommand::EditFile { path, edits, .. } => {
            let old_text = edits
                .iter()
                .map(|e| e.old_string.as_str())
                .collect::<Vec<_>>()
                .join("\n...\n");
            let new_text = edits
                .iter()
                .map(|e| e.new_string.as_str())
                .collect::<Vec<_>>()
                .join("\n...\n");
            serde_json::json!([{"path": path, "old_text": old_text, "new_text": new_text}])
        }
        AgentCommand::WriteFile { path, content }
        | AgentCommand::WriteFile2 { path, content, .. } => {
            let truncated: String = content.lines().take(500).collect::<Vec<_>>().join("\n");
            serde_json::json!([{"path": path, "old_text": "", "new_text": truncated}])
        }
        _ => return None,
    };
    if diffs.as_array().is_some_and(|a| !a.is_empty()) && tool_name != "todo_write" {
        Some(diffs)
    } else {
        None
    }
}

/// 串行执行单个工具调用并发送 WS 帧+落库（remember/use_skill 短路、审批、写操作）。
/// `persist`：false 时跳过 DB 落库（子 agent 内存态）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_single_tool_call(
    agent: &AgentState,
    _llm: &Arc<LlmState>,
    rt: &mut SessionRuntime,
    ws_tx: &mpsc::Sender<serde_json::Value>,
    call: &ParsedToolCall,
    persist: bool,
) -> Result<(), String> {
    // task 防御：depth>=1 时不应出现 task 调用（schema 已裁剪，双保险）
    if call.name == "task" && rt.depth >= 1 {
        let text = "error: task tool is not available inside a subagent".to_string();
        let mut result_frame = serde_json::json!({
            "type": "tool_result",
            "id": &call.id,
            "name": "task",
            "result": &text,
        });
        with_parent(&mut result_frame, rt);
        let _ = ws_tx.send(result_frame).await;
        record_tool_result(agent, rt, &call.id, "task", text, persist).await;
        return Ok(());
    }
    // todo_write 防御：depth>=1 时不可用
    if call.name == "todo_write" && rt.depth >= 1 {
        let text = "error: todo_write is main-agent only".to_string();
        let mut result_frame = serde_json::json!({
            "type": "tool_result",
            "id": &call.id,
            "name": "todo_write",
            "result": &text,
        });
        with_parent(&mut result_frame, rt);
        let _ = ws_tx.send(result_frame).await;
        record_tool_result(agent, rt, &call.id, "todo_write", text, persist).await;
        return Ok(());
    }

    // 角色工具白名单纵深防御：schema 层已裁剪，此处为运行时兜底。
    // rt.active_role 有 allow/deny → 工具不在允许集（allow 白名单或 deny 排除）→ 拒绝执行。
    if let Some(ref role) = rt.active_role {
        let tool_name = &call.name;
        let allowed = if let Some(allow) = roles::parse_tools_list(role.tools_allow.as_deref()) {
            allow.iter().any(|a| a == tool_name)
        } else {
            true // allow None/空 = 不限制
        };
        let denied = if let Some(deny) = roles::parse_tools_list(role.tools_deny.as_deref()) {
            deny.iter().any(|d| d == tool_name)
        } else {
            false
        };
        if !allowed || denied {
            let text = format!(
                "error: tool '{}' is not allowed by the current role '{}'",
                tool_name, role.name
            );
            let mut result_frame = serde_json::json!({
                "type": "tool_result",
                "id": &call.id,
                "name": tool_name,
                "result": &text,
            });
            with_parent(&mut result_frame, rt);
            let _ = ws_tx.send(result_frame).await;
            record_tool_result(agent, rt, &call.id, tool_name, text, persist).await;
            return Ok(());
        }
    }

    let mut call_frame = serde_json::json!({
        "type": "tool_call",
        "id": &call.id,
        "name": &call.name,
        "args": &call.args,
    });
    with_parent(&mut call_frame, rt);
    let _ = ws_tx.send(call_frame).await;

    // remember 工具短路：服务端本地保存记忆，**不进 AgentCommand 协议**
    #[cfg(feature = "rag")]
    if call.name == "remember" {
        let text = match crate::memory::remember::remember_from_agent(agent, rt, &call.args).await {
            Ok(msg) => msg,
            Err(e) => format!("error: {e}"),
        };
        let mut result_frame = serde_json::json!({
            "type": "tool_result",
            "id": &call.id,
            "name": &call.name,
            "result": &text,
        });
        with_parent(&mut result_frame, rt);
        let _ = ws_tx.send(result_frame).await;
        record_tool_result(agent, rt, &call.id, &call.name, text, persist).await;
        return Ok(());
    }

    // use_skill 工具短路
    #[cfg(feature = "rag")]
    if call.name == "use_skill" {
        let text = match crate::skill::use_skill_from_agent(agent, rt, &call.args).await {
            Ok(msg) => msg,
            Err(e) => format!("error: {e}"),
        };
        let mut result_frame = serde_json::json!({
            "type": "tool_result",
            "id": &call.id,
            "name": &call.name,
            "result": &text,
        });
        with_parent(&mut result_frame, rt);
        let _ = ws_tx.send(result_frame).await;
        record_tool_result(agent, rt, &call.id, &call.name, text, persist).await;
        return Ok(());
    }

    // wiki_search 工具短路：服务端本地检索（同 use_skill，不进 AgentCommand、
    // 不落审批）。WikiState 未注入（非 rag / 启动未初始化）→ 错误文本喂回模型。
    #[cfg(feature = "rag")]
    if call.name == "wiki_search" {
        let text = match agent.wiki.as_ref() {
            Some(wiki) => {
                match crate::wiki::wiki_search_from_agent(
                    wiki,
                    &rt.client_id,
                    &rt.workspace_id,
                    &call.args,
                )
                .await
                {
                    Ok(msg) => msg,
                    Err(e) => format!("error: {e}"),
                }
            }
            None => "error: wiki is not available".to_string(),
        };
        let mut result_frame = serde_json::json!({
            "type": "tool_result",
            "id": &call.id,
            "name": &call.name,
            "result": &text,
        });
        with_parent(&mut result_frame, rt);
        let _ = ws_tx.send(result_frame).await;
        record_tool_result(agent, rt, &call.id, &call.name, text, persist).await;
        return Ok(());
    }

    // wiki_read 工具短路：批量取页面全文，命中 bump_use。
    #[cfg(feature = "rag")]
    if call.name == "wiki_read" {
        let text = match agent.wiki.as_ref() {
            Some(wiki) => {
                match crate::wiki::wiki_read_from_agent(
                    wiki,
                    &rt.client_id,
                    &rt.workspace_id,
                    &call.args,
                )
                .await
                {
                    Ok(msg) => msg,
                    Err(e) => format!("error: {e}"),
                }
            }
            None => "error: wiki is not available".to_string(),
        };
        let mut result_frame = serde_json::json!({
            "type": "tool_result",
            "id": &call.id,
            "name": &call.name,
            "result": &text,
        });
        with_parent(&mut result_frame, rt);
        let _ = ws_tx.send(result_frame).await;
        record_tool_result(agent, rt, &call.id, &call.name, text, persist).await;
        return Ok(());
    }

    // todo_write 工具短路：全量替换任务清单，发送 todo_update 帧，不进 AgentCommand 协议
    if call.name == "todo_write" {
        let text = match tools::parse_todo_write(&call.args) {
            Ok(todos) => {
                rt.todos = todos.clone();
                // 发送 todo_update 帧
                let _ = ws_tx
                    .send(serde_json::json!({
                        "type": "todo_update",
                        "todos": &todos,
                    }))
                    .await;
                format!("todo list updated: {} items", todos.len())
            }
            Err(e) => format!("error: {e}"),
        };
        let mut result_frame = serde_json::json!({
            "type": "tool_result",
            "id": &call.id,
            "name": &call.name,
            "result": &text,
        });
        with_parent(&mut result_frame, rt);
        let _ = ws_tx.send(result_frame).await;
        record_tool_result(agent, rt, &call.id, &call.name, text, persist).await;
        return Ok(());
    }

    let result_text = match tools::parse_tool_call(&call.name, &call.args) {
        Ok(mut command) => {
            // Plan 模式防御：模型理论上看不到写工具 schema，若幻觉出写工具名，
            // parse 层拒绝执行（与 schema 裁剪双保险）。
            if rt.approval_mode == "plan" {
                if let Err(e) = tools::plan_mode_guard(&call.name) {
                    let mut result_frame = serde_json::json!({
                        "type": "tool_result",
                        "id": &call.id,
                        "name": &call.name,
                        "result": &e,
                    });
                    with_parent(&mut result_frame, rt);
                    let _ = ws_tx.send(result_frame).await;
                    record_tool_result(agent, rt, &call.id, &call.name, e, persist).await;
                    return Ok(());
                }
            }
            // write_file 升级映射：客户端 >=0.8.0 时映射为 WriteFile2（带 expected_hash）
            let client_version = agent
                .registry
                .client_handle(&rt.client_id)
                .await
                .and_then(|h| h.client_version);
            if matches!(&command, AgentCommand::WriteFile { .. })
                && client_supports_edit(client_version.as_deref())
            {
                if let AgentCommand::WriteFile { path, content } = command {
                    let expected_hash = rt.file_hashes.get(&path).cloned();
                    command = AgentCommand::WriteFile2 {
                        path,
                        content,
                        expected_hash,
                    };
                }
            }
            // stale hash 注入：EditFile/WriteFile2 发送前填入 expected_hash
            match &mut command {
                AgentCommand::EditFile {
                    path,
                    expected_hash,
                    ..
                }
                | AgentCommand::WriteFile2 {
                    path,
                    expected_hash,
                    ..
                } if expected_hash.is_none() => {
                    *expected_hash = rt.file_hashes.get(path).cloned();
                }
                _ => {}
            }
            // 审批：session 记忆集命中且命令非破坏性 → 放行
            let remembered = agent
                .is_allowed_for_session(&rt.session_id, &call.name)
                .await;
            let needs_confirm = crate::approval::needs_approval(&rt.approval_mode, &command);
            if (crate::approval::command_is_destructive(&command) || !remembered) && needs_confirm {
                let summary = crate::approval::approval_summary(&command);
                let args_preview = build_args_preview(&call.name, &command);
                let approval = agent
                    .request_approval(
                        &rt.session_id,
                        &call.name,
                        &summary,
                        &args_preview,
                        &[],
                        ws_tx,
                    )
                    .await;
                if !approval.approved() {
                    let text = "[denied by user]".to_string();
                    let mut result_frame = serde_json::json!({
                        "type": "tool_result",
                        "id": &call.id,
                        "name": &call.name,
                        "result": &text,
                    });
                    with_parent(&mut result_frame, rt);
                    let _ = ws_tx.send(result_frame).await;
                    record_tool_result(agent, rt, &call.id, &call.name, text, persist).await;
                    return Ok(());
                }
            }
            // 版本门控：老客户端不认识 Search/PatchFile/EditFile/WriteFile2/GitExec
            let gated = match &command {
                AgentCommand::Search { .. } | AgentCommand::PatchFile { .. } => Some((
                    MIN_SEARCH_PATCH_CLIENT_VERSION,
                    client_supports_search_patch as fn(Option<&str>) -> bool,
                )),
                AgentCommand::EditFile { .. } | AgentCommand::WriteFile2 { .. } => Some((
                    MIN_EDIT_CLIENT_VERSION,
                    client_supports_edit as fn(Option<&str>) -> bool,
                )),
                AgentCommand::GitExec { .. } => Some((
                    MIN_GIT_EXEC_CLIENT_VERSION,
                    client_supports_git_exec as fn(Option<&str>) -> bool,
                )),
                AgentCommand::ReadFileRange { .. } => Some((
                    MIN_READ_RANGE_CLIENT_VERSION,
                    client_supports_read_range as fn(Option<&str>) -> bool,
                )),
                AgentCommand::CodeOutline { .. } | AgentCommand::ReadSymbol { .. } => Some((
                    MIN_READ_RANGE_CLIENT_VERSION,
                    client_supports_read_range as fn(Option<&str>) -> bool,
                )),
                _ => None,
            };
            if let Some((min_version, supports)) = gated {
                let version = agent
                    .registry
                    .client_handle(&rt.client_id)
                    .await
                    .and_then(|h| h.client_version);
                if !supports(version.as_deref()) {
                    let text = format!(
                        "error: tool '{}' requires client >= {}.{}.{}; please upgrade the client",
                        call.name, min_version.0, min_version.1, min_version.2,
                    );
                    let mut result_frame = serde_json::json!({
                        "type": "tool_result",
                        "id": &call.id,
                        "name": &call.name,
                        "result": &text,
                    });
                    with_parent(&mut result_frame, rt);
                    let _ = ws_tx.send(result_frame).await;
                    record_tool_result(agent, rt, &call.id, &call.name, text, persist).await;
                    return Ok(());
                }
            }
            // docker 运行时但容器未启动 → 报错
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
                    command.clone(),
                )
                .await
            };
            // stale hash 错误处理：客户端回 stale 错误时清除该 path 的记录
            if let AgentResult::Error { ref message } = result {
                if message.contains("stale") || message.contains("hash mismatch") {
                    if let AgentCommand::EditFile { ref path, .. }
                    | AgentCommand::WriteFile2 { ref path, .. } = command
                    {
                        rt.file_hashes.remove(path);
                    }
                }
            }
            // hash 追踪：WriteOutcome.file_hash → file_hashes；read_file 完整读取记录 hash
            if let AgentResult::WriteOutcome { ref file_hash, .. } = result {
                if let AgentCommand::EditFile { ref path, .. }
                | AgentCommand::WriteFile2 { ref path, .. } = command
                {
                    if !file_hash.is_empty() {
                        rt.file_hashes.insert(path.clone(), file_hash.clone());
                    }
                }
            }
            // read_file / read_file_range 完整读取记录 hash（无截断标记时）
            if let AgentResult::FileContent { ref content } = result {
                if let AgentCommand::ReadFile { ref path } = command {
                    if !content.contains("[truncated") {
                        use sha2::{Digest, Sha256};
                        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
                        rt.file_hashes.insert(path.clone(), hash);
                    }
                }
                // read_file_range 不记录 hash（部分读取，非全文件）
            }
            let text = agent_result_to_text(&result);
            // diffs 合成：文件编辑工具成功时附带 diffs 到 WS 帧
            let diffs_value = if matches!(
                &result,
                AgentResult::WriteOutcome { .. } | AgentResult::Success
            ) {
                synthesize_tool_diffs(&call.name, &command)
            } else {
                None
            };
            let mut result_frame = serde_json::json!({
                "type": "tool_result",
                "id": &call.id,
                "name": &call.name,
                "result": &text,
            });
            if let Some(ref diffs) = diffs_value {
                result_frame["diffs"] = diffs.clone();
            }
            with_parent(&mut result_frame, rt);
            let _ = ws_tx.send(result_frame).await;
            text
        }
        Err(e) => {
            let mut result_frame = serde_json::json!({
                "type": "tool_result",
                "id": &call.id,
                "name": &call.name,
                "result": format!("error: {e}"),
            });
            with_parent(&mut result_frame, rt);
            let _ = ws_tx.send(result_frame).await;
            format!("error: {e}")
        }
    };
    record_tool_result(agent, rt, &call.id, &call.name, result_text, persist).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_with_parent_injects_parent_tool_call_id() {
        let mut frame = serde_json::json!({"type": "tool_call", "id": "c1"});
        let rt = crate::session::SessionRuntime {
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
            depth: 0,
            parent_tool_call_id: Some("parent_call_123".into()),
            file_hashes: std::collections::HashMap::new(),
            active_role: None,
        };
        with_parent(&mut frame, &rt);
        assert_eq!(
            frame["parent_tool_call_id"].as_str(),
            Some("parent_call_123")
        );
    }

    #[test]
    fn test_with_parent_noop_when_none() {
        let mut frame = serde_json::json!({"type": "tool_call", "id": "c1"});
        let rt = crate::session::SessionRuntime {
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
            depth: 0,
            parent_tool_call_id: None,
            file_hashes: std::collections::HashMap::new(),
            active_role: None,
        };
        with_parent(&mut frame, &rt);
        assert!(frame.get("parent_tool_call_id").is_none());
    }

    #[test]
    fn test_stale_hash_lifecycle() {
        use std::collections::HashMap;
        let mut hashes: HashMap<String, String> = HashMap::new();
        // read_file 完整读取记录 hash
        hashes.insert("a.rs".to_string(), "abc123".to_string());
        assert_eq!(hashes.get("a.rs").map(String::as_str), Some("abc123"));
        // WriteOutcome 刷新
        hashes.insert("a.rs".to_string(), "def456".to_string());
        assert_eq!(hashes.get("a.rs").map(String::as_str), Some("def456"));
        // stale 错误清除
        hashes.remove("a.rs");
        assert!(hashes.get("a.rs").is_none());
    }

    // ── diffs 合成 ─────────────────────────────────────────

    #[test]
    fn test_synthesize_diffs_patch_file() {
        let cmd = AgentCommand::PatchFile {
            path: "a.rs".into(),
            old_string: "old".into(),
            new_string: "new".into(),
        };
        let diffs = synthesize_tool_diffs("patch_file", &cmd).unwrap();
        let arr = diffs.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["path"], "a.rs");
        assert_eq!(arr[0]["old_text"], "old");
        assert_eq!(arr[0]["new_text"], "new");
    }

    #[test]
    fn test_synthesize_diffs_edit_file() {
        let cmd = AgentCommand::EditFile {
            path: "b.rs".into(),
            edits: vec![
                rust_tunnel_common::FileEdit {
                    old_string: "aaa".into(),
                    new_string: "bbb".into(),
                    replace_all: false,
                },
                rust_tunnel_common::FileEdit {
                    old_string: "ccc".into(),
                    new_string: "ddd".into(),
                    replace_all: false,
                },
            ],
            expected_hash: None,
        };
        let diffs = synthesize_tool_diffs("edit_file", &cmd).unwrap();
        let arr = diffs.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0]["old_text"].as_str().unwrap().contains("aaa"));
        assert!(arr[0]["new_text"].as_str().unwrap().contains("ddd"));
    }

    #[test]
    fn test_synthesize_diffs_write_file() {
        let cmd = AgentCommand::WriteFile {
            path: "c.rs".into(),
            content: "line1\nline2\n".into(),
        };
        let diffs = synthesize_tool_diffs("write_file", &cmd).unwrap();
        let arr = diffs.as_array().unwrap();
        assert_eq!(arr[0]["old_text"], "");
        assert!(arr[0]["new_text"].as_str().unwrap().contains("line1"));
    }

    #[test]
    fn test_synthesize_diffs_non_file_tool_returns_none() {
        let cmd = AgentCommand::Shell {
            cmd: "ls".into(),
            cwd: None,
        };
        assert!(synthesize_tool_diffs("shell", &cmd).is_none());
    }

    // ── 审批预览格式 ──────────────────────────────────────

    #[test]
    fn test_build_args_preview_patch_file() {
        let cmd = AgentCommand::PatchFile {
            path: "a.rs".into(),
            old_string: "fn old() {}".into(),
            new_string: "fn new() {}".into(),
        };
        let preview = build_args_preview("patch_file", &cmd);
        assert!(preview.contains("a.rs"));
        assert!(preview.contains("--- old"));
        assert!(preview.contains("+++ new"));
        assert!(preview.contains("fn old() {}"));
    }

    #[test]
    fn test_build_args_preview_edit_file() {
        let cmd = AgentCommand::EditFile {
            path: "b.rs".into(),
            edits: vec![rust_tunnel_common::FileEdit {
                old_string: "aaa".into(),
                new_string: "bbb".into(),
                replace_all: false,
            }],
            expected_hash: None,
        };
        let preview = build_args_preview("edit_file", &cmd);
        assert!(preview.contains("b.rs"));
        assert!(preview.contains("--- old"));
        assert!(preview.contains("+++ new"));
    }

    #[test]
    fn test_build_args_preview_write_file() {
        let cmd = AgentCommand::WriteFile {
            path: "c.txt".into(),
            content: "hello\nworld".into(),
        };
        let preview = build_args_preview("write_file", &cmd);
        assert!(preview.contains("c.txt"));
        assert!(preview.contains("new content"));
    }

    #[test]
    fn test_build_args_preview_non_file_tool() {
        let cmd = AgentCommand::Shell {
            cmd: "ls".into(),
            cwd: None,
        };
        let preview = build_args_preview("shell", &cmd);
        assert_eq!(preview, "shell");
    }
}
