//! Agent main loop: LLM call → tool execution over tunnel → feed results back.
use std::sync::Arc;
use tokio::sync::mpsc;

use super::{compact, executor, session::SessionRuntime, sse, tools, AgentState};
use crate::llm::{ChatCompletionRequest, ChatMessage, LlmState};
use rust_tunnel_common::{AgentCommand, AgentResult};

/// @引用限制：个数、单文件字节、总字节。
pub const MAX_REFS: usize = 10;
pub const MAX_REF_FILE_BYTES: usize = 50 * 1024;
pub const MAX_REFS_TOTAL_BYTES: usize = 200 * 1024;

/// 把用户消息与引用文件内容合成单条 user 消息（落库/进上下文的都是这条）。
pub fn compose_user_message(
    content: &str,
    ref_files: &[(String, Result<String, String>)],
) -> String {
    if ref_files.is_empty() {
        return content.to_string();
    }
    let mut out = content.to_string();
    for (path, result) in ref_files {
        match result {
            Ok(text) => {
                let truncated = if text.len() > MAX_REF_FILE_BYTES {
                    let mut cut = MAX_REF_FILE_BYTES;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    format!("{}\n[truncated]", &text[..cut])
                } else {
                    text.clone()
                };
                out.push_str(&format!(
                    "\n\n--- 引用文件: {path} ---\n```\n{truncated}\n```"
                ));
            }
            Err(_) => out.push_str(&format!("\n\n[无法读取: {path}]")),
        }
    }
    out
}

/// 按行切分 SSE 字节流：HTTP chunk 边界可切断一行，未完结部分留缓冲。
struct LineBuf {
    pending: Vec<u8>,
    /// pending 超过上限（无换行的超长单行）→ true；runner 应终止流。
    overflowed: bool,
    limit: usize,
}

impl Default for LineBuf {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            overflowed: false,
            limit: sse::MAX_STREAM_BYTES,
        }
    }
}

impl LineBuf {
    /// 用自定义上限构造（测试用小 limit，避免测试分配 10MB）。
    #[cfg(test)]
    fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > self.limit {
            self.overflowed = true;
        }
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

    /// 非 SSE 嗅探：缓冲的首批字节是否已能判定为非 SSE 流。
    ///
    /// 判定条件：pending 长度足够（≥5，即 `data:` 长度），trim 后既不是
    /// `data:` 前缀、也不是 SSE 允许的开头（空行/注释行 `:` 前缀）、也不是
    /// 纯空白（需继续等待更多字节）。
    fn has_non_sse_prefix(&self) -> bool {
        if self.pending.len() < 5 {
            return false;
        }
        let trimmed = std::str::from_utf8(&self.pending)
            .unwrap_or_default()
            .trim_start();
        !trimmed.is_empty() && !trimmed.starts_with("data:") && !trimmed.starts_with(':')
    }

    fn pending(&self) -> &[u8] {
        &self.pending
    }

    fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

/// 判断一行是否为合法的 SSE 首行（data 行或注释行；空行已被 LineBuf 过滤）。
/// 首个非空行既不是 `data:` 也不是 `:` 注释 → 上游实际返回的是普通文本/JSON，
/// 走非 SSE 回退。
fn is_sse_line(line: &str) -> bool {
    let line = line.trim_end_matches('\r');
    line.starts_with("data:") || line.starts_with(':')
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

/// 首个支持 Search/PatchFile 命令的客户端版本（随本特性发布 bump）。
const MIN_SEARCH_PATCH_CLIENT_VERSION: (u64, u64, u64) = (0, 2, 0);

/// 解析 "x.y.z"（允许 v 前缀）为数字三元组；非严格 semver 输入返回 None。
/// 客户端在 agent 模式下上报 `{CARGO_PKG_VERSION}+agent`，故解析前须剥离
/// semver 构建元数据（`+`）与预发布（`-`）后缀。
pub(crate) fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    // 顺序：先 strip 'v' 前缀，再切掉 +（构建元数据）/ -（预发布）后缀。
    let s = s.split(['+', '-']).next().unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// 客户端版本是否支持 search/patch；版本缺失/非法视为不支持（保守策略，
/// 避免老客户端收到未知 bincode 变体后反序列化失败断开控制连接）。
fn client_supports_search_patch(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_SEARCH_PATCH_CLIENT_VERSION)
}

/// 首个带回环 PTY 服务（交互式终端）的客户端版本。
const MIN_TERMINAL_CLIENT_VERSION: (u64, u64, u64) = (0, 3, 0);

/// 客户端版本是否支持交互式终端（PTY 服务）；缺失/非法视为不支持。
pub(crate) fn client_supports_terminal(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_TERMINAL_CLIENT_VERSION)
}

/// 首个支持 `AgentExecCancel`（真取消）的客户端版本。
const MIN_CANCEL_CLIENT_VERSION: (u64, u64, u64) = (0, 4, 0);

/// 客户端版本是否支持真取消（AgentExecCancel）；缺失/非法视为不支持（保守，
/// 避免老客户端收到未知 bincode 变体断开控制连接）。
pub(crate) fn client_supports_cancel(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_CANCEL_CLIENT_VERSION)
}

/// 工具结果落库/回填上限：300 行或 30KB（先到者），保护 DB 体积与 LLM 上下文。
const TOOL_RESULT_MAX_LINES: usize = 300;
const TOOL_RESULT_MAX_BYTES: usize = 30 * 1024;

fn truncate_tool_result(text: String) -> String {
    let total_lines = text.lines().count();
    if total_lines <= TOOL_RESULT_MAX_LINES && text.len() <= TOOL_RESULT_MAX_BYTES {
        return text;
    }
    // 先按字节截断到安全边界，再按行数截断（两者取更严）
    let mut cut = TOOL_RESULT_MAX_BYTES.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let byte_cut = &text[..cut];
    let line_cut: String = byte_cut
        .lines()
        .take(TOOL_RESULT_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    format!("{line_cut}\n[truncated, total {total_lines} lines]")
}

fn agent_result_to_text(result: &AgentResult) -> String {
    let text = match result {
        AgentResult::Shell {
            stdout,
            stderr,
            exit_code,
        } => format!("exit_code={exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"),
        AgentResult::FileContent { content } => content.clone(),
        AgentResult::Success => "ok".to_string(),
        AgentResult::Error { message } => format!("error: {message}"),
    };
    truncate_tool_result(text)
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
        reasoning_content: None,
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
                // 审批：session 记忆集命中 → 放行；否则按 workspace approval_mode 判定，
                // 需确认时发 approval_request 挂起等待。拒绝落库 [denied by user]，
                // 回合不中断（模型看到拒绝可自行换方案）。
                if !agent
                    .is_allowed_for_session(&rt.session_id, &call.name)
                    .await
                    && super::approval::needs_approval(&rt.approval_mode, &command)
                {
                    let summary = super::approval::approval_summary(&command);
                    let args_preview: String = call.args.chars().take(500).collect();
                    // runner 路径无 ACP 选项：透传空切片，审批卡片保持 approve/deny 二元。
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
                        let _ = ws_tx
                            .send(serde_json::json!({
                                "type": "tool_result",
                                "id": &call.id,
                                "name": &call.name,
                                "result": &text,
                            }))
                            .await;
                        record_tool_result(agent, rt, &call.id, &call.name, text).await;
                        continue;
                    }
                }
                // 老客户端不认识 Search/PatchFile 变体：bincode 按索引反序列化，
                // 未知索引直接导致控制连接断开。版本不足时在服务端短路为错误喂回模型。
                let needs_new_client = matches!(
                    command,
                    AgentCommand::Search { .. } | AgentCommand::PatchFile { .. }
                );
                if needs_new_client {
                    let version = agent
                        .registry
                        .get(&rt.client_id)
                        .await
                        .and_then(|e| e.client_version.clone());
                    if !client_supports_search_patch(version.as_deref()) {
                        let text = format!(
                            "error: tool '{}' requires client >= {}.{}.{}; please upgrade the client",
                            call.name,
                            MIN_SEARCH_PATCH_CLIENT_VERSION.0,
                            MIN_SEARCH_PATCH_CLIENT_VERSION.1,
                            MIN_SEARCH_PATCH_CLIENT_VERSION.2,
                        );
                        let _ = ws_tx
                            .send(serde_json::json!({
                                "type": "tool_result",
                                "id": &call.id,
                                "name": &call.name,
                                "result": &text,
                            }))
                            .await;
                        record_tool_result(agent, rt, &call.id, &call.name, text).await;
                        continue;
                    }
                }
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

        record_tool_result(agent, rt, &call.id, &call.name, result_text).await;
    }
    Ok(())
}

/// 处理一个已解析的完整 LLM 响应（非 SSE 回退与 SSE 嗅探回退共用）。
/// 返回 Ok(true) = 文本回合已完成（已落库 + done，调用方结束回合）；
/// Ok(false) = tool 回合已执行（调用方继续下一轮）。
async fn handle_llm_turn_json(
    agent: &AgentState,
    rt: &mut SessionRuntime,
    ws_tx: &mpsc::Sender<serde_json::Value>,
    body: &serde_json::Value,
) -> Result<bool, String> {
    match parse_llm_turn(body)? {
        LlmTurn::Text(text) => {
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
            handle_tool_calls(agent, rt, ws_tx, calls, raw_calls).await?;
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
                    crate::agent::session::truncate_agents_md(&content)
                ),
            );
        }
        rt.agents_md = Some(content);
    }

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
            tools: Some(tools::agent_tools_schema()),
            tool_choice: None,
            raw_body: None,
        };
        let req_body = crate::llm::upstream::build_upstream_body(&request);
        let outcome =
            crate::llm::upstream::execute_with_failover(&llm.breakers, &chain, &req_body, true)
                .await;

        let mut resp = match outcome {
            crate::llm::upstream::FailoverOutcome::Success { resp, .. } => resp,
            crate::llm::upstream::FailoverOutcome::Exhausted { message, .. } => {
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
                            let retry = crate::llm::upstream::execute_with_failover(
                                &llm.breakers,
                                &chain,
                                &req_body,
                                true,
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
                                        if handle_llm_turn_json(&agent, rt, &ws_tx, &body).await? {
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
                                    message,
                                    ..
                                } => {
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
                if handle_llm_turn_json(&agent, rt, &ws_tx, &body).await? {
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
        let body_bytes = axum::body::to_bytes(resp.into_body(), sse::MAX_STREAM_BYTES)
            .await
            .map_err(|e| format!("failed to read LLM response: {e}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body_bytes)
            .map_err(|e| format!("invalid LLM response JSON: {e}"))?;
        if handle_llm_turn_json(&agent, rt, &ws_tx, &body).await? {
            return Ok(());
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
async fn record_tool_result(
    agent: &AgentState,
    rt: &mut SessionRuntime,
    call_id: &str,
    call_name: &str,
    content: String,
) {
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
    fn test_parse_version() {
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.10.3"), Some((1, 10, 3)));
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0))); // 允许 v 前缀
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("abc"), None);
        // agent 模式客户端上报 `{CARGO_PKG_VERSION}+agent`：构建元数据须剥离
        assert_eq!(parse_version("0.2.0+agent"), Some((0, 2, 0)));
        // 预发布后缀同样剥离（robustness）
        assert_eq!(parse_version("0.2.0-rc.1"), Some((0, 2, 0)));
        assert_eq!(parse_version("v0.2.0+agent"), Some((0, 2, 0)));
    }

    #[test]
    fn test_client_supports_search_patch() {
        assert!(!client_supports_search_patch(Some("0.1.0")));
        assert!(client_supports_search_patch(Some("0.2.0")));
        assert!(client_supports_search_patch(Some("1.0.0")));
        assert!(!client_supports_search_patch(None)); // 缺失视为过旧
        assert!(!client_supports_search_patch(Some("garbage")));
        // 回归：agent 模式版本后缀 +agent 不得破坏版本门控
        assert!(client_supports_search_patch(Some("0.2.0+agent")));
        assert!(!client_supports_search_patch(Some("0.1.0+agent")));
    }

    #[test]
    fn test_client_supports_terminal() {
        assert!(!client_supports_terminal(Some("0.2.0")));
        assert!(client_supports_terminal(Some("0.3.0")));
        assert!(client_supports_terminal(Some("1.0.0")));
        assert!(!client_supports_terminal(None)); // 缺失/离线视为不支持
        assert!(!client_supports_terminal(Some("garbage")));
        // 回归：agent 模式版本后缀 +agent 不得破坏版本门控
        assert!(client_supports_terminal(Some("0.3.0+agent")));
        assert!(!client_supports_terminal(Some("0.2.0+agent")));
    }

    #[test]
    fn test_client_supports_cancel() {
        assert!(client_supports_cancel(Some("0.4.0")));
        assert!(client_supports_cancel(Some("v0.4.1")));
        assert!(!client_supports_cancel(Some("0.3.9")));
        assert!(!client_supports_cancel(Some("0.3.0+agent")));
        assert!(!client_supports_cancel(None));
        assert!(!client_supports_cancel(Some("garbage")));
    }

    #[test]
    fn test_line_buf_detects_non_sse_first_bytes() {
        // 首批字节即不是 data:（JSON 开头，尚无换行）→ 判定非 SSE
        let mut buf = LineBuf::default();
        assert!(!buf.has_non_sse_prefix()); // 空缓冲不足判定
        buf.feed(b"{\"c");
        assert!(!buf.has_non_sse_prefix()); // 3 字节 < 5 → 继续等待
        buf.feed(b"hoi");
        assert!(buf.has_non_sse_prefix()); // 累计 ≥5，trim 后以 `{` 开头 → 非 SSE
        assert_eq!(buf.pending(), b"{\"choi");
    }

    #[test]
    fn test_line_buf_non_sse_prefix_allows_sse_leading() {
        // SSE 流开头允许空行/注释行/`data:` 前缀（可跨 chunk 拼合）：不应误判
        let mut buf = LineBuf::default();
        buf.feed(b"\n\n: comment\r\n");
        assert!(!buf.has_non_sse_prefix()); // trim 后以 `:` 开头 → SSE 注释

        let mut buf = LineBuf::default();
        buf.feed(b"\n\n  \n");
        assert!(!buf.has_non_sse_prefix()); // 纯空白 → 继续等待

        let mut buf = LineBuf::default();
        buf.feed(b"da");
        assert!(!buf.has_non_sse_prefix());
        buf.feed(b"ta: ");
        assert!(!buf.has_non_sse_prefix()); // data: 前缀跨 chunk 拼合 → 仍是 SSE
    }

    #[test]
    fn test_first_line_sniff_is_sse_line() {
        assert!(is_sse_line(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}"
        ));
        assert!(is_sse_line(": comment"));
        assert!(is_sse_line("data: [DONE]"));
        assert!(!is_sse_line("{\"choices\":["));
        assert!(!is_sse_line("plain text"));
    }

    #[test]
    fn test_line_buf_overflow_on_single_line() {
        // 无换行的超长单行：pending 超过上限 → overflowed 标记（runner 据此终止）
        let mut buf = LineBuf::with_limit(100);
        assert!(!buf.overflowed);
        buf.feed(&[b'x'; 150]);
        assert!(buf.overflowed);
        // 溢出后接口不 panic、不丢已解析行
        assert!(buf.feed(b"y").is_empty());
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
            sse::SseFeed::Overflow => panic!("expected Content delta, got Overflow"),
        }
    }

    // SSE 流传输层失败重试的回归覆盖见 `tests/agent_basic.rs`：
    // `agent_stream_retry_succeeds_with_full_text`（重试成功：stream_reset + status
    // 帧 + 最终文本完整）与 `agent_stream_retry_exhausted_sends_error`（耗尽走
    // 错误路径）。本文件的旧 `test_retry_on_stream_read_failure` 只测本地闭包，
    // 零回归价值，已删除。

    #[tokio::test]
    async fn test_persist_message_v2_writes_all_columns() {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
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

    async fn test_agent_state(db: crate::db::Database) -> AgentState {
        let server_state = crate::control::ServerState::with_db(db);
        server_state.agent_state.expect("agent_state initialized")
    }

    #[test]
    fn test_truncate_tool_result_by_lines() {
        let text = (0..400)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_tool_result(text);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() <= TOOL_RESULT_MAX_LINES + 1); // +1 为 truncated 标记行
        assert!(out.contains("[truncated"));
        assert!(out.contains("400")); // 总行数写入标记
    }

    #[test]
    fn test_truncate_tool_result_by_bytes() {
        let text = "x".repeat(40 * 1024);
        let out = truncate_tool_result(text);
        assert!(out.len() < 35 * 1024);
        assert!(out.contains("[truncated"));
    }

    #[test]
    fn test_truncate_tool_result_short_unchanged() {
        let text = "short output".to_string();
        assert_eq!(truncate_tool_result(text.clone()), text);
    }

    #[test]
    fn test_truncate_tool_result_multibyte_safe() {
        // 截断点落在 UTF-8 多字节序列中间不得 panic
        let text = "汉".repeat(15 * 1024); // ~45KB
        let out = truncate_tool_result(text);
        assert!(out.contains("[truncated"));
    }

    #[test]
    fn test_compose_user_message_with_refs() {
        let msg = compose_user_message(
            "帮我重构",
            &[("src/main.rs".to_string(), Ok("fn main() {}".to_string()))],
        );
        assert!(msg.starts_with("帮我重构"));
        assert!(msg.contains("--- 引用文件: src/main.rs ---"));
        assert!(msg.contains("fn main() {}"));
    }

    #[test]
    fn test_compose_user_message_ref_failure_annotated() {
        let msg = compose_user_message(
            "看下这个",
            &[("missing.rs".to_string(), Err("not found".to_string()))],
        );
        assert!(msg.contains("[无法读取: missing.rs]"));
    }

    #[test]
    fn test_compose_user_message_no_refs_passthrough() {
        assert_eq!(compose_user_message("纯文本", &[]), "纯文本");
    }

    #[test]
    fn test_compose_user_message_file_truncated() {
        let big = "x".repeat(60 * 1024);
        let msg = compose_user_message("看", &[("big.rs".to_string(), Ok(big))]);
        assert!(msg.contains("[truncated]"));
    }
}
