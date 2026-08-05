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
                out.push_str(&format!("tool({}): {}\n", m.name.as_deref().unwrap_or("?"), c));
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
            format!("[上下文已截断] 早期 {n} 条消息因长度超限被移除", n = segment.len())
        }
    };

    // 内存替换：system + summary + 保留段
    let kept: Vec<ChatMessage> = rt.messages[cut..].to_vec();
    rt.messages.truncate(1);
    rt.messages.push(ChatMessage::text("user", &replacement));
    rt.messages.extend(kept);

    // 落库 summary 行（role=user，load 重放时从其开始取消息）
    super::runner::runner_persist_summary(agent, &rt.session_id, &replacement).await;

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
    let outcome =
        crate::server::llm::upstream::execute_with_failover(&llm.breakers, &chain, &req_body, false)
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
    let body: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| format!("invalid summary JSON: {e}"))?;
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
                tool_calls: Some(vec![serde_json::json!({"id": id, "type": "function", "function": {"name": "shell", "arguments": "{}"}})]),
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
        let expect = 4 + 8 + serde_json::to_string(&vec![serde_json::json!({"id": "c1"})]).unwrap().len();
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
        db.llm_save_provider("p1", "prov", "deepseek", "https://api", "key", None, None, true)
            .await
            .unwrap();
        db.llm_save_model("m1", "p1", "big-model", "", "[]", true, Some(r#"{"agent_context_limit":200000}"#))
            .await
            .unwrap();
        db.llm_save_model("m2", "p1", "plain-model", "", "[]", true, None)
            .await
            .unwrap();

        assert_eq!(context_limit_for(&db, "big-model").await, 200_000);
        assert_eq!(context_limit_for(&db, "plain-model").await, DEFAULT_CONTEXT_LIMIT_CHARS);
        assert_eq!(context_limit_for(&db, "no-such-model").await, DEFAULT_CONTEXT_LIMIT_CHARS);
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
}
