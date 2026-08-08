//! 上下文压缩：估算超限 → LLM 摘要替换早期历史；失败降级滑动截断。
use std::sync::Arc;
use tokio::sync::mpsc;

use super::{session::SessionRuntime, AgentState};
use crate::server::llm::{ChatMessage, LlmState};

/// 全局默认上下文上限（输入字符估算，chars/4 ≈ tokens）。
pub const DEFAULT_CONTEXT_LIMIT_CHARS: usize = 100_000;
/// 压缩时保留的最近消息条数（原文不动）。
pub const KEEP_RECENT_MESSAGES: usize = 6;

/// 估算消息列表的字符量（content + tool_calls JSON + tool 结果）。
pub fn estimate_chars(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| {
            m.content.as_deref().unwrap_or("").len()
                + m.tool_calls
                    .as_ref()
                    .map(|tc| serde_json::to_string(tc).unwrap_or_default().len())
                    .unwrap_or(0)
        })
        .sum()
}

/// 确定压缩切割点：压缩段为 messages[1..cut]（跳过 system），保留 messages[cut..]。
/// 返回 0 表示无可压缩段。切割点对齐 tool 配对边界：右边界不得落在 tool 序列
/// 中间（右侧第一条若是 tool 消息则继续右移到该 tool 序列结束）。
pub fn find_cut_point(messages: &[ChatMessage], keep_recent: usize) -> usize {
    // messages[0] 是 system；压缩段至少留 1 条才有意义
    if messages.len() <= 1 + keep_recent + 1 {
        return 0;
    }
    let mut cut = messages.len() - keep_recent;
    // 右边界对齐：cut 处若是 tool 消息（其配对 tool_calls 在压缩段内），
    // 把整个 tool 结果序列划入压缩段
    while cut < messages.len() && messages[cut].role == "tool" {
        cut += 1;
    }
    // 对齐后若保留段被吃光则不压缩
    if cut >= messages.len() - 1 {
        return 0;
    }
    cut
}

/// per-model 阈值：extra_config.agent_context_limit；未设置/解析失败回落默认。
pub async fn context_limit_for(db: &crate::server::db::Database, model: &str) -> usize {
    let Ok(Some(record)) = db.llm_find_model_by_name_or_alias(model).await else {
        return DEFAULT_CONTEXT_LIMIT_CHARS;
    };
    record
        .extra_config
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get("agent_context_limit")?.as_u64())
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_CONTEXT_LIMIT_CHARS)
}

/// 待压缩段渲染为纯文本（喂给摘要 LLM）。
pub fn render_for_summary(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        match (m.role.as_str(), &m.content, &m.tool_calls) {
            ("tool", Some(c), _) => {
                out.push_str(&format!(
                    "tool({}): {}\n",
                    m.name.as_deref().unwrap_or("?"),
                    c
                ));
            }
            (_, _, Some(calls)) => {
                let names: Vec<&str> = calls
                    .iter()
                    .filter_map(|c| c.pointer("/function/name").and_then(|n| n.as_str()))
                    .collect();
                out.push_str(&format!("assistant called tools: {}\n", names.join(", ")));
            }
            (_, Some(c), _) => out.push_str(&format!("{}: {}\n", m.role, c)),
            _ => {}
        }
    }
    out
}

const SUMMARY_PROMPT: &str = "Summarize the following conversation segment between a user and an AI programming assistant. Preserve: operations already performed and their results, key file paths, current task state. Be concise (under 500 words). Output plain text only.";

/// 每轮 LLM 调用前检查：超限则压缩 rt.messages（并落库 summary 行）。
/// 压缩失败降级为滑动截断，永不阻断回合。
pub async fn maybe_compact(
    agent: &AgentState,
    llm: &Arc<LlmState>,
    rt: &mut SessionRuntime,
    ws_tx: &mpsc::Sender<serde_json::Value>,
) -> Result<(), String> {
    let limit = context_limit_for(&agent.db, &rt.model).await;
    if estimate_chars(&rt.messages) <= limit {
        return Ok(());
    }
    let cut = find_cut_point(&rt.messages, KEEP_RECENT_MESSAGES);
    if cut == 0 {
        return Ok(()); // 历史太短，无可压缩段
    }

    let _ = ws_tx
        .send(serde_json::json!({"type": "status", "message": "compacting context..."}))
        .await;

    // 压缩前记录保留段对应的 DB 行（落库 summary 后重插用）。rt.messages[0] 是
    // system 不落库，保留段 rt.messages[cut..] 对应 DB 中最后 kept_count 行——
    // 会话全程 DB 与 rt.messages 同步追加，load 又从最后一个 summary 起重放，
    // 故 DB 尾部即保留段。
    let kept_count = rt.messages.len() - cut;
    let kept_rows = agent
        .db
        .agent_list_messages(&rt.session_id)
        .await
        .map(|records| {
            let start = records.len().saturating_sub(kept_count);
            records[start..].to_vec()
        })
        .unwrap_or_default();

    let segment = rt.messages[1..cut].to_vec();
    let rendered = render_for_summary(&segment);

    // 摘要 LLM 调用：同会话模型、非流式、无 tools
    let summary = summarize(llm, &rt.model, &rendered).await;

    let replacement = match summary {
        Ok(s) => format!("[上下文摘要] {s}"),
        Err(e) => {
            tracing::warn!("compaction failed, fallback to truncation: {e}");
            let _ = ws_tx
                .send(serde_json::json!({"type": "status", "message": "compaction failed, truncated history"}))
                .await;
            format!(
                "[上下文已截断] 早期 {n} 条消息因长度超限被移除",
                n = segment.len()
            )
        }
    };

    // 内存替换：system + summary + 保留段
    let kept: Vec<ChatMessage> = rt.messages[cut..].to_vec();
    rt.messages.truncate(1);
    rt.messages.push(ChatMessage::text("user", &replacement));
    rt.messages.extend(kept);

    // 落库 summary 行（role=user，load 重放时从其开始取消息）
    super::runner::runner_persist_summary(agent, &rt.session_id, &replacement).await;

    // 在 summary 之后重插保留段（新 id、按序），使 DB 物理顺序为 [..., summary,
    // kept...]，与 load 的 rposition(kind=="summary") 重放语义对齐——否则 kept 段
    // 落在 summary 之前，WS 重连/页面刷新后整段从 LLM 上下文丢失。旧 kept 行保留
    // 不删：它们只落在最新 summary 之前、不再被 LLM 重放，UI 历史仍完整可见。
    for row in &kept_rows {
        let id = format!("{:032x}", rand::random::<u128>());
        if let Err(e) = agent
            .db
            .agent_add_message_v2(
                &id,
                &row.session_id,
                &row.role,
                &row.content,
                row.tool_calls.as_deref(),
                row.tool_call_id.as_deref(),
                row.name.as_deref(),
                &row.kind,
            )
            .await
        {
            tracing::warn!("failed to re-persist kept segment during compaction: {}", e);
        }
    }

    let _ = ws_tx
        .send(serde_json::json!({"type": "status", "message": "context compacted"}))
        .await;
    Ok(())
}

async fn summarize(llm: &Arc<LlmState>, model: &str, rendered: &str) -> Result<String, String> {
    let chain = crate::server::llm::router::resolve_with_failover(llm, model)
        .await
        .map_err(|e| format!("model resolution failed: {e}"))?;
    let request = crate::server::llm::ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage::text("system", SUMMARY_PROMPT),
            ChatMessage::text("user", rendered),
        ],
        stream: false,
        max_tokens: None,
        temperature: None,
        top_p: None,
        tools: None,
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
            return Err(format!("summary LLM unavailable: {message}"));
        }
    };
    let body_bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .map_err(|e| format!("failed to read summary response: {e}"))?;
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).map_err(|e| format!("invalid summary JSON: {e}"))?;
    match super::runner::parse_llm_turn(&body)? {
        super::runner::LlmTurn::Text(t) => Ok(t),
        super::runner::LlmTurn::ToolCalls(_) => Err("summary model returned tool calls".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::llm::ChatMessage;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage::text(role, content)
    }

    fn tool_pair(id: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![
                    serde_json::json!({"id": id, "type": "function", "function": {"name": "shell", "arguments": "{}"}}),
                ]),
                tool_call_id: None,
                name: None,
            },
            ChatMessage {
                role: "tool".into(),
                content: Some("ok".into()),
                tool_calls: None,
                tool_call_id: Some(id.into()),
                name: Some("shell".into()),
            },
        ]
    }

    #[test]
    fn test_estimate_chars_counts_content_and_tool_fields() {
        let msgs = vec![
            msg("system", "1234"),
            msg("user", "12345678"),
            ChatMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![serde_json::json!({"id": "c1"})]),
                tool_call_id: None,
                name: None,
            },
        ];
        // 4 + 8 + tool_calls JSON 长度
        let expect = 4
            + 8
            + serde_json::to_string(&vec![serde_json::json!({"id": "c1"})])
                .unwrap()
                .len();
        assert_eq!(estimate_chars(&msgs), expect);
    }

    #[test]
    fn test_cut_point_never_splits_tool_pair() {
        let mut msgs = vec![msg("system", "s")];
        for i in 0..5 {
            msgs.push(msg("user", &format!("q{i}")));
            msgs.extend(tool_pair(&format!("c{i}")));
            msgs.push(msg("assistant", &format!("a{i}")));
        }
        let total = msgs.len();
        let cut = find_cut_point(&msgs, KEEP_RECENT_MESSAGES);
        // 切割点右侧第一条不能是孤儿 tool 消息（其配对 tool_calls 在左侧）
        if cut < total {
            let right = &msgs[cut];
            assert!(
                right.role != "tool",
                "cut at {cut} splits tool pair: right side starts with tool message"
            );
        }
    }

    #[test]
    fn test_cut_point_returns_zero_when_nothing_to_compress() {
        let msgs = vec![msg("system", "s"), msg("user", "q"), msg("assistant", "a")];
        assert_eq!(find_cut_point(&msgs, KEEP_RECENT_MESSAGES), 0);
    }

    #[tokio::test]
    async fn test_context_limit_from_model_extra_config() {
        let db = crate::server::db::Database::new(":memory:").await.unwrap();
        db.llm_save_provider(
            "p1",
            "prov",
            "deepseek",
            "https://api",
            "key",
            None,
            None,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model(
            "m1",
            "p1",
            "big-model",
            "",
            "[]",
            true,
            Some(r#"{"agent_context_limit":200000}"#),
        )
        .await
        .unwrap();
        db.llm_save_model("m2", "p1", "plain-model", "", "[]", true, None)
            .await
            .unwrap();

        assert_eq!(context_limit_for(&db, "big-model").await, 200_000);
        assert_eq!(
            context_limit_for(&db, "plain-model").await,
            DEFAULT_CONTEXT_LIMIT_CHARS
        );
        assert_eq!(
            context_limit_for(&db, "no-such-model").await,
            DEFAULT_CONTEXT_LIMIT_CHARS
        );
    }

    #[test]
    fn test_render_for_summary_includes_roles_and_tools() {
        let mut msgs = vec![msg("user", "帮我修 bug")];
        msgs.extend(tool_pair("c1"));
        let text = render_for_summary(&msgs);
        assert!(text.contains("user"));
        assert!(text.contains("帮我修 bug"));
        assert!(text.contains("shell"));
    }

    #[tokio::test]
    async fn test_list_order_kept_before_summary_and_reinsert() {
        // 排序语义（压缩修复依赖）：agent_list_messages 按 created_at(秒), rowid
        // 排序。旧 kept 行落在前一秒 → 必在 summary（当前秒插入）之前；summary 与
        // 重插 kept 行同秒靠 rowid（自增）保证先后。故 DB 顺序恒为 [旧 kept, summary,
        // 重插 kept]，load 从最后一个 summary 起重放即可命中保留段。
        let db = crate::server::db::Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        // 旧 kept 行：10 秒前落库（模拟上一回合保留段）
        db.agent_add_message("old1", "s1", "user", "旧保留", None)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE agent_messages SET created_at = datetime('now', '-10 seconds') WHERE id = 'old1'",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // summary + 重插 kept：当前秒插入，先后由 rowid 保证
        db.agent_add_message_v2(
            "sum1",
            "s1",
            "user",
            "[上下文摘要] 概要",
            None,
            None,
            None,
            "summary",
        )
        .await
        .unwrap();
        db.agent_add_message("kept1", "s1", "assistant", "保留1", None)
            .await
            .unwrap();
        db.agent_add_message("kept2", "s1", "user", "保留2", None)
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["old1", "sum1", "kept1", "kept2"]);
    }

    #[tokio::test]
    async fn test_maybe_compact_reinserts_kept_segment_after_summary() {
        // 端到端：per-model 极小阈值触发压缩；summarize 无 provider 可用 → 失败走
        // 降级截断路径（无需 mock 上游）。修复前 kept 段落在 summary 之前，load
        // 重放丢失整段（红）；修复后 DB 物理顺序 [..., summary, kept...]，重连/
        // 刷新后 kept 段完整重放。
        let db = crate::server::db::Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, Some("big-model"))
            .await
            .unwrap();
        // per-model 极小阈值：10 chars 即触发压缩（模型可解析、无可用 provider）
        db.llm_save_provider(
            "p1",
            "prov",
            "deepseek",
            "https://api",
            "key",
            None,
            None,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model(
            "m1",
            "p1",
            "big-model",
            "",
            "[]",
            true,
            Some(r#"{"agent_context_limit":10}"#),
        )
        .await
        .unwrap();
        // 10 条历史（system 由 load 注入，DB 只存 10 行）
        for i in 0..5 {
            db.agent_add_message(&format!("q{i}"), "s1", "user", &format!("问题{i}"), None)
                .await
                .unwrap();
            db.agent_add_message(
                &format!("a{i}"),
                "s1",
                "assistant",
                &format!("回答{i}"),
                None,
            )
            .await
            .unwrap();
        }

        let agent = crate::server::agent::AgentState::new(
            crate::server::control::client_registry::ClientRegistry::new(db.clone()),
            db.clone(),
        );
        // LlmState::new(None, None)：无 DB → summarize 的 resolve_with_failover 失败
        // → 走降级截断路径。
        let llm = std::sync::Arc::new(crate::server::llm::LlmState::new(None, None));
        let (ws_tx, _rx) = tokio::sync::mpsc::channel(8);

        let mut rt = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        assert_eq!(rt.messages.len(), 11); // system + 10 历史
                                           // messages.len()=11, keep=6 → cut=5 → kept = messages[5..] 共 6 条
        maybe_compact(&agent, &llm, &mut rt, &ws_tx).await.unwrap();

        // 内存替换：system + 摘要 + kept 段
        assert_eq!(rt.messages.len(), 8);
        assert!(rt.messages[1]
            .content
            .as_deref()
            .unwrap()
            .contains("上下文"));
        let kept_in_mem: Vec<&str> = rt.messages[2..]
            .iter()
            .filter_map(|m| m.content.as_deref())
            .collect();
        assert_eq!(
            kept_in_mem,
            ["问题2", "回答2", "问题3", "回答3", "问题4", "回答4"]
        );

        // DB 物理顺序：summary 行之后紧跟 kept 重插行
        let rows = db.agent_list_messages("s1").await.unwrap();
        let summary_pos = rows
            .iter()
            .rposition(|r| r.kind == "summary")
            .expect("summary row must exist");
        assert_eq!(rows.len(), 17); // 10 旧 + 1 summary + 6 重插
        assert_eq!(
            rows.len() - summary_pos - 1,
            6,
            "summary 之后应恰为 kept 重插的 6 行"
        );
        let after_summary: Vec<&str> = rows[summary_pos + 1..]
            .iter()
            .map(|r| r.content.as_str())
            .collect();
        assert_eq!(
            after_summary,
            ["问题2", "回答2", "问题3", "回答3", "问题4", "回答4"]
        );

        // 重连/刷新后重放：load 从最后一个 summary 起重放，kept 段不再丢失
        let rt2 = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        assert_eq!(rt2.messages.len(), 8);
        assert!(rt2.messages[1]
            .content
            .as_deref()
            .unwrap()
            .contains("上下文"));
        let kept_reloaded: Vec<&str> = rt2.messages[2..]
            .iter()
            .filter_map(|m| m.content.as_deref())
            .collect();
        assert_eq!(
            kept_reloaded,
            ["问题2", "回答2", "问题3", "回答3", "问题4", "回答4"]
        );
    }
}
