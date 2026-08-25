//! 回合主循环：LLM 调用 → 工具执行 → 结果回灌，含消息落库与用量上下文。

use super::{AgentState, Arc, LlmState, SessionRuntime, mpsc, parse_llm_turn, LlmTurn, ChatMessage, handle_tool_calls, executor, AgentCommand, AgentResult, remove_tagged_block, ROLE_BLOCK_TAG, insert_block_before, roles, compact, ChatCompletionRequest, tools, is_sse_response, sse, LineBuf, is_sse_line, send_tool_call_delta};

/// 构造 runner 路径的用量记录上下文：从候选链出账方提取 provider/model 信息，
/// 供四处复用（主流式、流中断重试、compact 摘要、title 生成）。
#[must_use] 
pub fn runner_usage_ctx(
    candidate: &crate::llm::router::Candidate,
    requested_model: &str,
    failover_from: Option<String>,
) -> crate::llm::usage::UsageContext {
    crate::llm::usage::UsageContext {
        // runner 不经网关 API Key 认证，无 key 可记；填入标识名让调用统计里
        // agent 工作台的调用可辨识（否则按 API Key 分组显示"未知"、明细为 "—"）。
        api_key_id: None,
        api_key_name: "Agent".to_string(),
        provider_id: Some(candidate.provider.id.clone()),
        provider_name: candidate.provider.name.clone(),
        model_id: Some(candidate.model_id.clone()),
        model_name: candidate.model_name.clone(),
        requested_model: requested_model.to_string(),
        protocol: "openai".to_string(),
        stream: true,
        rag_chunks_injected: None,
        failover_from,
    }
}

/// 手工逐字段 clone SessionRuntime（用于 tokio::spawn，需 'static owned）。
/// 不 clone DB/registry（Arc 共享）；todos/messages 为内存态深拷贝。
async fn handle_llm_turn_json(
    agent: &AgentState,
    llm: &Arc<LlmState>,
    rt: &mut SessionRuntime,
    ws_tx: &mpsc::Sender<serde_json::Value>,
    body: &serde_json::Value,
) -> Result<bool, String> {
    match parse_llm_turn(body)? {
        LlmTurn::Text(text) => {
            // 非流式路径提取 reasoning_content 并落库/发 WS thought 帧
            let reasoning = body["choices"][0]["message"]["reasoning_content"]
                .as_str()
                .unwrap_or("");
            if !reasoning.is_empty() {
                let _ = ws_tx
                    .send(serde_json::json!({
                        "type": "assistant_chunk",
                        "content": reasoning,
                        "thought": true,
                        "final": false,
                    }))
                    .await;
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
            let _ = ws_tx
                .send(
                    serde_json::json!({"type": "assistant_chunk", "content": &text, "final": true}),
                )
                .await;
            rt.messages.push(ChatMessage::text("assistant", &text));
            persist_message(
                agent,
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
            Ok(true)
        }
        LlmTurn::ToolCalls(calls) => {
            let raw_calls = body["choices"][0]["message"]["tool_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            // 非流式路径提取 reasoning_content 并落库/发 WS thought 帧
            let reasoning = body["choices"][0]["message"]["reasoning_content"]
                .as_str()
                .unwrap_or("");
            if !reasoning.is_empty() {
                let _ = ws_tx
                    .send(serde_json::json!({
                        "type": "assistant_chunk",
                        "content": reasoning,
                        "thought": true,
                        "final": false,
                    }))
                    .await;
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
            handle_tool_calls(agent, llm, rt, ws_tx, calls, raw_calls, reasoning, true).await?;
            Ok(false)
        }
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

    // 首个回合前读 AGENTS.md（rt.agents_md 为 None 表示尚未尝试）。读不到/为空
    // 静默跳过；读到则重建 system 消息并缓存（同会话后续回合不重读）。
    if rt.agents_md.is_none() {
        let content = if rt.runtime_type == "docker" && rt.docker_container.is_none() {
            String::new()
        } else {
            match executor::exec_on_client(
                &agent,
                &rt.workspace_id,
                &rt.client_id,
                &rt.root_path,
                rt.docker_container.as_deref(),
                AgentCommand::ReadFile {
                    path: "AGENTS.md".to_string(),
                },
            )
            .await
            {
                AgentResult::FileContent { content } => content,
                _ => String::new(),
            }
        };
        let content = content.trim().to_string();
        if !content.is_empty() {
            let base = rt.messages[0].content.as_deref().unwrap_or_default();
            // base 是「内置 + workspace」两层（load 构建、无 AGENTS.md 段），直接追加第三段。
            rt.messages[0] = ChatMessage::text(
                "system",
                format!(
                    "{base}\n\n---\n\n# Project instructions (AGENTS.md):\n{}",
                    crate::session::truncate_agents_md(&content)
                ),
            );
        }
        rt.agents_md = Some(content);
    }

    // AI 记忆注入：AGENTS.md 之后、首回合前，每会话检索一次并缓存
    // （rt.memory_block）。查询文本 = 最近一条 user 消息（本轮刚 push）。
    // 块以 `\n\n---\n\n` 分隔追加进 system 单条（messages[0]），同 AGENTS.md
    // 段格式；system 在 load 时重建、不落库，不会重复注入。
    #[cfg(feature = "rag")]
    if rt.memory_block.is_none() {
        let query = rt
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let block = if let Some(memory) = agent.memory.as_ref() {
            crate::memory::inject::retrieve_for_session(
                memory,
                &rt.client_id,
                &rt.workspace_id,
                &query,
            )
            .await
        } else {
            None
        };
        rt.memory_block = match block {
            Some(b) if !b.is_empty() => {
                let base = rt.messages[0].content.as_deref().unwrap_or_default();
                rt.messages[0] = ChatMessage::text("system", format!("{base}\n\n---\n\n{b}"));
                Some(b)
            }
            _ => Some(String::new()), // 缓存空，避免每回合重试检索
        };
    }

    // Skill 清单注入：记忆块之后、首回合前，每会话检索一次并缓存
    // （rt.skill_list_block）。纯 SQL 无 embedding 依赖——skill_enabled 关闭或无
    // 可见技能返回 None（缓存空串，避免每回合重试）。块以 `\n\n---\n\n` 分隔
    // 追加进 system 单条（同 memory_block 段格式）。
    #[cfg(feature = "rag")]
    if rt.skill_list_block.is_none() {
        let block = if let Some(memory) = agent.memory.as_ref() {
            crate::skill::retrieve_skill_list_for_session(memory, &rt.client_id, &rt.workspace_id)
                .await
        } else {
            None
        };
        rt.skill_list_block = match block {
            Some(b) if !b.is_empty() => {
                let base = rt.messages[0].content.as_deref().unwrap_or_default();
                rt.messages[0] = ChatMessage::text("system", format!("{base}\n\n---\n\n{b}"));
                Some(b)
            }
            _ => Some(String::new()),
        };
    }

    // Wiki 清单注入：skill 之后、首回合前，每会话检索一次并缓存（rt.wiki_list_block）。
    // 纯 SQL（FTS5）零 embedding 依赖，wiki_enabled 关闭或无可见容器返回 None。
    #[cfg(feature = "rag")]
    if rt.wiki_list_block.is_none() {
        let block = if let Some(wiki) = agent.wiki.as_ref() {
            crate::wiki::retrieve_wiki_list_for_session(wiki, &rt.client_id, &rt.workspace_id).await
        } else {
            None
        };
        rt.wiki_list_block = match block {
            Some(b) if !b.is_empty() => {
                let base = rt.messages[0].content.as_deref().unwrap_or_default();
                rt.messages[0] = ChatMessage::text("system", format!("{base}\n\n---\n\n{b}"));
                Some(b)
            }
            _ => Some(String::new()),
        };
    }

    // Plan 模式系统提示注入/移除：模式切换时动态更新 system 消息（内存态，不落库）。
    // 追加 `\n\n---\n\n` 分隔的 plan 模式说明块；退出 plan 模式时移除该块。
    const PLAN_MODE_BLOCK_TAG: &str = "\n\n---\n\n# Plan Mode\n";
    const PLAN_MODE_BLOCK: &str = "\n\n---\n\n# Plan Mode\nYou are in **plan mode** (只读调研模式). In this mode:\n- You can ONLY use read-only tools: read_file, list_dir, search, git_status, git_diff, git_log, git_show, git_branch, todo_write.\n- You CANNOT write files, run shell commands, or modify the repository.\n- Your goal is to investigate the codebase and produce a detailed execution plan.\n- Use todo_write to track your investigation progress and plan items.\n- When your plan is ready, present it clearly to the user.\n- The user will confirm the plan and switch to execution mode for implementation.\n";

    // 主会话角色系统提示块：tag `# Role: `。每回合无条件重建（先按 tag 移除旧块，
    // 再按 rt.active_role 插入新块到 plan 块之前），角色 A→B 切换、清除、提示词
    // 在 DB 被编辑（refresh_session_state 每消息重读角色记录）都即时生效。内存态，不落库。
    if let Some(content) = rt.messages[0].content.take() {
        rt.messages[0] = ChatMessage::text("system", remove_tagged_block(&content, ROLE_BLOCK_TAG));
    }
    if let Some(ref role) = rt.active_role {
        let mut segs = vec![format!("# Role: {}", role.name)];
        if !role.description.is_empty() {
            segs.push(role.description.clone());
        }
        let prompt = role.system_prompt.trim();
        if !prompt.is_empty() {
            segs.push(prompt.to_string());
        }
        // 仅名称一行时没有信息量，不注入
        if segs.len() > 1 {
            let role_block = format!("\n\n---\n\n{}", segs.join("\n"));
            let content = rt.messages[0].content.take().unwrap_or_default();
            rt.messages[0] = ChatMessage::text(
                "system",
                insert_block_before(&content, PLAN_MODE_BLOCK_TAG, &role_block),
            );
        }
    }

    // task 工具角色清单注入：主会话首回合（roles_block 为 None）查可见角色，
    // 把 task_schema_roles_block() 追加到 task 工具 description（system 块独立 tag）。
    const ROLES_BLOCK_TAG: &str = "\n\n---\n\n### Available Sub-Agent Roles";
    if rt.roles_block.is_none() && rt.depth == 0 {
        let visible = agent
            .db
            .role_list_visible(&rt.client_id, &rt.workspace_id, Some("subagent"))
            .await
            .unwrap_or_default();
        let block = roles::task_schema_roles_block(&visible);
        rt.roles_block = Some(block.clone());
        // 注入到 system 块（独立 tag，便于更新）
        if !block.is_empty() {
            let base = rt.messages[0].content.as_deref().unwrap_or_default();
            // 去掉旧角色清单块（如有）再追加
            let base_clean = if let Some(pos) = base.find(ROLES_BLOCK_TAG) {
                base[..pos].trim_end().to_string()
            } else {
                base.to_string()
            };
            rt.messages[0] = ChatMessage::text("system", format!("{base_clean}\n\n---\n\n{block}"));
        }
    }
    let current_has_plan = rt.messages[0]
        .content
        .as_deref()
        .is_some_and(|s| s.contains(PLAN_MODE_BLOCK_TAG));
    if rt.approval_mode == "plan" && !current_has_plan {
        let base = rt.messages[0].content.as_deref().unwrap_or_default();
        rt.messages[0] = ChatMessage::text("system", format!("{base}{PLAN_MODE_BLOCK}"));
    } else if rt.approval_mode != "plan" && current_has_plan {
        if let Some(content) = rt.messages[0].content.take() {
            if let Some(pos) = content.find(PLAN_MODE_BLOCK_TAG) {
                let trimmed = content[..pos].trim_end().to_string();
                rt.messages[0] = ChatMessage::text("system", trimmed);
            } else {
                rt.messages[0].content = Some(content);
            }
        }
    }

    // 用量记录上下文（从出账候选构建）与请求开始时间，用于 usage 落库。
    let mut usage_ctx: Option<crate::llm::usage::UsageContext> = None;
    let mut usage_started: Option<std::time::Instant> = None;

    'round: for _round in 0..MAX_TOOL_ROUNDS {
        // 每轮 LLM 调用前检查上下文超限 → 压缩早期历史（失败降级截断，不阻断回合）
        compact::maybe_compact(&agent, &llm, rt, &ws_tx).await?;
        let chain = crate::llm::router::resolve_with_failover(&llm, &rt.model)
            .await
            .map_err(|e| format!("model resolution failed: {e}"))?;

        let request = ChatCompletionRequest {
            model: rt.model.clone(),
            messages: rt.messages.clone(),
            stream: true,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: {
                let client_ver = agent
                    .registry
                    .client_handle(&rt.client_id)
                    .await
                    .and_then(|h| h.client_version);
                Some(tools::filter_tools_for_client_version(
                    tools::agent_tools_schema_filtered(
                        &rt.approval_mode,
                        rt.active_role
                            .as_ref()
                            .and_then(|r| roles::parse_tools_list(r.tools_allow.as_deref()))
                            .as_deref(),
                        rt.active_role
                            .as_ref()
                            .and_then(|r| roles::parse_tools_list(r.tools_deny.as_deref()))
                            .as_deref(),
                    ),
                    client_ver.as_deref(),
                ))
            },
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

        let mut resp = match outcome {
            crate::llm::upstream::FailoverOutcome::Success {
                resp,
                candidate,
                failed_over,
                ..
            } => {
                // 构造用量记录上下文
                usage_ctx = Some(runner_usage_ctx(
                    &candidate,
                    &rt.model,
                    if failed_over {
                        Some(chain.candidates[0].model_name.clone())
                    } else {
                        None
                    },
                ));
                usage_started = Some(std::time::Instant::now());
                resp
            }
            crate::llm::upstream::FailoverOutcome::Exhausted {
                status, message, ..
            } => {
                // 上下文溢出自愈：token 估算（chars/4）可能低估，上游返回
                // context-length-exceeded 时强制压缩后重试一次本回合。
                // 压缩成功（find_cut_point 有空间）→ 重试；无可压缩段 → 报错。
                // 无限循环被 find_cut_point 自然遏制：压缩后保留段 <= keep_recent+2，
                // 切点必为 0，后续 force_compact 返回 false。
                if compact::is_context_overflow(status.as_u16(), &message) {
                    if let Ok(did_compact) = compact::force_compact(&agent, &llm, rt, &ws_tx).await
                    {
                        if did_compact {
                            let _ = ws_tx
                                .send(serde_json::json!({
                                    "type": "status",
                                    "message": "上下文超限，已压缩历史并重试"
                                }))
                                .await;
                            continue 'round;
                        }
                    }
                }
                // 记录 LLM 不可用失败
                if let (Some(ctx), Some(db), Some(started)) =
                    (usage_ctx.take(), llm.db.as_ref(), usage_started.take())
                {
                    ctx.record_failure(db, i32::from(status.as_u16()), "exhausted", started);
                }
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
            // ── 流式路径（含非 SSE 嗅探回退）──
            // relay 层无条件改写 Content-Type 为 text/event-stream，故上游忽略
            // stream 标志返回的普通 JSON 也只能靠内容嗅探识别（见下）。
            use futures_util::StreamExt;
            let mut agg = sse::SseAggregator::new();
            let mut line_buf = LineBuf::default();
            let mut byte_stream = resp.into_body().into_data_stream();
            // 非 SSE 嗅探：首个非空行或首批字节前缀不是 `data:`（且非注释/空行）
            // → 判定为非 SSE，剩余流全量收集进 non_sse_buf，流结束后按 JSON 回退。
            let mut sse_confirmed = false;
            let mut non_sse_buf: Option<Vec<u8>> = None;
            // 致命错误（读流失败 / 聚合超限 / 单行超长）：终止并走错误路径，
            // 不落库半截消息。
            let mut fatal = false;
            let mut fatal_msg = String::new();
            // 传输层失败（byte_stream 读返回 Err）自动重试次数，最多 2 次；
            // 溢出/解析失败/工具回合不重试，仅流 read 失败可重试。
            let mut retries = 0usize;
            const MAX_STREAM_RETRIES: usize = 2;

            'sse: while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        // 传输层失败：可重试则丢弃半截、重新请求
                        if retries < MAX_STREAM_RETRIES {
                            retries += 1;
                            let _ = ws_tx
                                .send(serde_json::json!({"type": "stream_reset"}))
                                .await;
                            let _ = ws_tx
                                .send(serde_json::json!({
                                    "type": "status",
                                    "message": format!("上游连接中断，正在重试 ({retries}/{MAX_STREAM_RETRIES})")
                                }))
                                .await;
                            // 记录流中断失败
                            if let (Some(ctx), Some(db), Some(started)) =
                                (usage_ctx.take(), llm.db.as_ref(), usage_started.take())
                            {
                                ctx.record_failure(db, 502, "stream_interrupted", started);
                            }
                            let retry = crate::llm::upstream::execute_with_failover(
                                &llm.upstream_client,
                                &llm.breakers,
                                &llm.known_failures,
                                &chain,
                                &req_body,
                                true,
                                None,
                            )
                            .await;
                            match retry {
                                crate::llm::upstream::FailoverOutcome::Success {
                                    resp: r2, ..
                                } => {
                                    resp = r2;
                                    let content_type2 = resp
                                        .headers()
                                        .get(axum::http::header::CONTENT_TYPE)
                                        .and_then(|v| v.to_str().ok())
                                        .unwrap_or("")
                                        .to_string();
                                    if !is_sse_response(&content_type2) {
                                        // 重试返回非 SSE（上游降级普通 JSON）→ 转非 SSE 回退
                                        let body_bytes = axum::body::to_bytes(
                                            resp.into_body(),
                                            sse::MAX_STREAM_BYTES,
                                        )
                                        .await
                                        .map_err(|e| format!("failed to read LLM response: {e}"))?;
                                        let body: serde_json::Value =
                                            serde_json::from_slice(&body_bytes).map_err(|e| {
                                                format!("invalid LLM response JSON: {e}")
                                            })?;
                                        if handle_llm_turn_json(&agent, &llm, rt, &ws_tx, &body)
                                            .await?
                                        {
                                            return Ok(());
                                        }
                                        continue 'round; // 外层 for _round
                                    }
                                    // 重新初始化聚合器与行缓冲，丢弃半截
                                    agg = sse::SseAggregator::new();
                                    line_buf = LineBuf::default();
                                    sse_confirmed = false;
                                    non_sse_buf = None;
                                    // 用重试响应的 body 重建读流（否则 continue 后仍读死流）
                                    byte_stream = resp.into_body().into_data_stream();
                                    continue 'sse;
                                }
                                crate::llm::upstream::FailoverOutcome::Exhausted {
                                    status,
                                    message,
                                    ..
                                } => {
                                    // 上下文溢出自愈（同主路径逻辑）
                                    if compact::is_context_overflow(status.as_u16(), &message) {
                                        if let Ok(did_compact) =
                                            compact::force_compact(&agent, &llm, rt, &ws_tx).await
                                        {
                                            if did_compact {
                                                let _ = ws_tx
                                                    .send(serde_json::json!({
                                                        "type": "status",
                                                        "message": "上下文超限，已压缩历史并重试"
                                                    }))
                                                    .await;
                                                continue 'round;
                                            }
                                        }
                                    }
                                    // 记录重试耗尽失败
                                    if let (Some(ctx), Some(db), Some(started)) =
                                        (usage_ctx.take(), llm.db.as_ref(), usage_started.take())
                                    {
                                        ctx.record_failure(
                                            db,
                                            i32::from(status.as_u16()),
                                            "retry_exhausted",
                                            started,
                                        );
                                    }
                                    let _ = ws_tx.send(serde_json::json!({"type": "error", "message": format!("LLM unavailable: {message}")})).await;
                                    return Err(format!("LLM unavailable: {message}"));
                                }
                            }
                        }
                        fatal = true;
                        fatal_msg = format!("stream read failed: {e}");
                        break 'sse;
                    }
                };
                // 已判定非 SSE：剩余流全量收集
                if let Some(buf) = &mut non_sse_buf {
                    buf.extend_from_slice(&chunk);
                    if buf.len() > sse::MAX_STREAM_BYTES {
                        fatal = true;
                        fatal_msg = "stream size limit exceeded".to_string();
                        break 'sse;
                    }
                    continue;
                }
                // 首批字节（尚无换行）即不是 data: → 非 SSE
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
                            // 首个非空行非 data: → 非 SSE：该行 + 缓冲剩余字节进收集桶
                            sse_confirmed = true;
                            let mut buf = line.as_bytes().to_vec();
                            buf.extend_from_slice(line_buf.pending());
                            non_sse_buf = Some(buf);
                            break;
                        }
                    }
                    match agg.feed_line(&line) {
                        sse::SseFeed::Content(delta) => {
                            let _ = ws_tx
                                .send(serde_json::json!({"type": "assistant_chunk", "content": delta, "final": false}))
                                .await;
                        }
                        sse::SseFeed::Thought { reasoning, content } => {
                            let _ = ws_tx
                                .send(serde_json::json!({"type": "assistant_chunk", "content": reasoning, "thought": true, "final": false}))
                                .await;
                            if let Some(c) = content {
                                let _ = ws_tx
                                    .send(serde_json::json!({"type": "assistant_chunk", "content": c, "final": false}))
                                    .await;
                            }
                        }
                        sse::SseFeed::ToolCallDelta { calls, content } => {
                            send_tool_call_delta(
                                &ws_tx,
                                calls,
                                content,
                                rt.parent_tool_call_id.as_deref(),
                            )
                            .await;
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

            if fatal {
                // 记录流致命错误
                if let (Some(ctx), Some(db), Some(started)) =
                    (usage_ctx.take(), llm.db.as_ref(), usage_started.take())
                {
                    ctx.record_failure(db, 502, "stream_fatal_error", started);
                }
                let _ = ws_tx
                    .send(serde_json::json!({"type": "error", "message": fatal_msg}))
                    .await;
                return Err(fatal_msg);
            }

            // 流结束时 pending 残留首批非 SSE 字节（无换行的单 chunk JSON）→ 整包收集
            if non_sse_buf.is_none() && !sse_confirmed && line_buf.has_non_sse_prefix() {
                non_sse_buf = Some(line_buf.take_pending());
            }

            if let Some(buf) = non_sse_buf {
                // 非 SSE 回退：收集到的整包 body 按 JSON 解析（与普通非 SSE 分支共用）
                let body: serde_json::Value = serde_json::from_slice(&buf)
                    .map_err(|e| format!("invalid LLM response JSON: {e}"))?;
                if handle_llm_turn_json(&agent, &llm, rt, &ws_tx, &body).await? {
                    return Ok(());
                }
                continue;
            }

            if let Some(last) = line_buf.flush() {
                match agg.feed_line(&last) {
                    sse::SseFeed::Content(delta) => {
                        let _ = ws_tx
                            .send(serde_json::json!({"type": "assistant_chunk", "content": delta, "final": false}))
                            .await;
                    }
                    sse::SseFeed::Thought { reasoning, content } => {
                        let _ = ws_tx
                            .send(serde_json::json!({"type": "assistant_chunk", "content": reasoning, "thought": true, "final": false}))
                            .await;
                        if let Some(c) = content {
                            let _ = ws_tx
                                .send(serde_json::json!({"type": "assistant_chunk", "content": c, "final": false}))
                                .await;
                        }
                    }
                    sse::SseFeed::ToolCallDelta { calls, content } => {
                        send_tool_call_delta(
                            &ws_tx,
                            calls,
                            content,
                            rt.parent_tool_call_id.as_deref(),
                        )
                        .await;
                    }
                    sse::SseFeed::Overflow => {
                        let _ = ws_tx
                            .send(serde_json::json!({"type": "error", "message": "stream size limit exceeded"}))
                            .await;
                        return Err("stream size limit exceeded".to_string());
                    }
                    sse::SseFeed::Done | sse::SseFeed::None => {}
                }
            }

            let saw_data = agg.saw_data();
            let turn = agg.finish()?;
            // 兜底：从未收到任何 data 行且无聚合产出 → 空流/伪装 SSE，
            // 报错而非静默落库空消息（修复前真实内容/错误被吞）。
            if turn.text.is_empty() && turn.tool_calls.is_empty() && !saw_data {
                let _ = ws_tx
                    .send(serde_json::json!({"type": "error", "message": "empty response from upstream (not an SSE stream?)"}))
                    .await;
                return Err("empty response from upstream (not an SSE stream?)".to_string());
            }
            if turn.tool_calls.is_empty() {
                // 文本回合：收尾 final chunk + 落库 + done
                // reasoning 落库 thought 行（位于正文之前）
                if !turn.reasoning.is_empty() {
                    let _ = ws_tx
                        .send(serde_json::json!({"type": "assistant_chunk", "content": &turn.reasoning, "thought": true, "final": false}))
                        .await;
                    persist_message(
                        &agent,
                        &rt.session_id,
                        "assistant",
                        &turn.reasoning,
                        None,
                        None,
                        Some("thought"),
                        "message",
                    )
                    .await;
                }
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
                // 记录用量（streaming 路径：usage 从聚合器提取）
                if let (Some(ctx), Some(db), Some(started)) =
                    (usage_ctx.take(), llm.db.as_ref(), usage_started.take())
                {
                    ctx.record_success(db, turn.usage, started);
                }
                return Ok(());
            }
            // tool 回合：转成与 parse_llm_turn 相同的处理流（见下）
            handle_tool_calls(
                &agent,
                &llm,
                rt,
                &ws_tx,
                turn.tool_calls,
                turn.raw_tool_calls,
                &turn.reasoning,
                true,
            )
            .await?;
            // 记录用量（streaming 路径：usage 从聚合器提取）
            if let (Some(ctx), Some(db), Some(started)) =
                (usage_ctx.take(), llm.db.as_ref(), usage_started.take())
            {
                ctx.record_success(db, turn.usage, started);
            }
            continue;
        }

        // ── 非 SSE 回退（某些上游/代理返回普通 JSON）──
        let body_bytes = axum::body::to_bytes(resp.into_body(), sse::MAX_STREAM_BYTES)
            .await
            .map_err(|e| format!("failed to read LLM response: {e}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body_bytes)
            .map_err(|e| format!("invalid LLM response JSON: {e}"))?;
        if handle_llm_turn_json(&agent, &llm, rt, &ws_tx, &body).await? {
            return Ok(());
        }
    }

    let _ = ws_tx
        .send(serde_json::json!({"type": "error", "message": "tool round limit reached"}))
        .await;
    Err("tool round limit reached".to_string())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_message(
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
            None, // runner 路径无 ACP 子 agent 归属
        )
        .await
    {
        tracing::warn!("failed to persist agent message: {}", e);
    }
}

/// 把一条 tool 结果消息同时写入 DB（kind='tool_result'）与内存上下文
/// （role='tool'，带 tool_call_id/name）。handle_tool_calls 的正常路径与
/// 版本门控拒绝路径共用，保证两者落库行为一致。
/// `persist`：false 时跳过 DB 落库（子 agent 内存态，不污染主会话持久化）。
pub(crate) async fn record_tool_result(
    agent: &AgentState,
    rt: &mut SessionRuntime,
    call_id: &str,
    call_name: &str,
    content: String,
    persist: bool,
) {
    if persist {
        persist_message(
            agent,
            &rt.session_id,
            "tool",
            &content,
            None,
            Some(call_id),
            Some(call_name),
            "tool_result",
        )
        .await;
    }
    rt.messages.push(ChatMessage {
        role: "tool".into(),
        content: Some(content),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: Some(call_id.to_string()),
        name: Some(call_name.to_string()),
    });
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

    async fn test_agent_state(db: crate::db::Database) -> AgentState {
        AgentState::new(
            std::sync::Arc::new(crate::test_helpers::TestRegistry::new(&db)),
            db,
        )
    }

    #[tokio::test]
    async fn test_persist_message_v2_writes_all_columns() {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
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

    #[test]
    fn test_runner_usage_ctx_identifiable() {
        // runner 路径无网关 API Key，但必须填标识名，否则调用统计里无法辨识来源
        let candidate = crate::llm::router::Candidate {
            provider: crate::llm::ProviderConfig {
                id: "p1".into(),
                name: "P1".into(),
                provider_type: "deepseek".into(),
                base_url: "https://example.com".into(),
                api_key: "k".into(),
                extra_config: None,
                anthropic_base_url: None,
                enabled: true,
                created_at: String::new(),
                updated_at: String::new(),
            },
            model_name: "deepseek-chat".into(),
            model_id: "m1".into(),
            priority: 0,
            upstream_protocol: crate::llm::router::UpstreamProtocol::default(),
        };
        let ctx = runner_usage_ctx(&candidate, "my-alias", None);
        assert_eq!(ctx.api_key_id, None);
        assert_eq!(ctx.api_key_name, "Agent");
        assert_eq!(ctx.provider_id.as_deref(), Some("p1"));
        assert_eq!(ctx.model_id.as_deref(), Some("m1"));
        assert_eq!(ctx.requested_model, "my-alias");
        assert!(ctx.failover_from.is_none());
    }

    // ── client_supports_edit 版本门控 ──────────────────────
}
