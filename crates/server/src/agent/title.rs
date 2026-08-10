//! 会话标题自动生成：首个 assistant 回合结束后，用会话模型对首条 user 消息
//! 发一次非流式小请求生成简短标题。失败静默（保留空标题，前端显示默认名）。
use std::sync::Arc;

use super::AgentState;
use crate::llm::{ChatCompletionRequest, ChatMessage, LlmState};

const TITLE_MAX_CHARS: usize = 30;
const USER_MSG_MAX_CHARS: usize = 500;

/// 清洗 LLM 输出为合法标题：trim、去首尾引号、按字符截断；空结果返回 None。
fn clean_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(TITLE_MAX_CHARS).collect())
}

/// 生成并写回会话标题。调用方负责只在 title 为空时触发；本函数内部重读
/// title 做竞态守卫（生成期间用户可能已手动改名）。
///
/// `title_tx` 为触发连接的事件通道：标题成功写库后经它广播一条
/// `session_title` 帧，让 SessionBar 实时回显。此通道属于触发连接的 WebSocket，
/// 不会广播到同 session 的其他标签页——若需广播需在 AgentState 维护连接表，
/// 权衡后不做（YAGNI）：其他标签页的 SessionBar 会在下次 refetch 时自愈。
pub async fn maybe_generate_title(
    agent: AgentState,
    llm: Arc<LlmState>,
    session_id: String,
    model: String,
    title_tx: Option<tokio::sync::mpsc::Sender<serde_json::Value>>,
) {
    match generate_title_inner(&agent, &llm, &session_id, &model).await {
        Ok(Some(title)) => {
            if let Some(tx) = &title_tx {
                notify_title(tx, &session_id, &title).await;
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(session_id, "auto title generation failed: {e}"),
    }
}

/// 构造并发送 `session_title` 帧到触发连接的事件通道。发送失败静默忽略
/// （连接已断，push_task 不再消费该通道）。
pub async fn notify_title(
    tx: &tokio::sync::mpsc::Sender<serde_json::Value>,
    session_id: &str,
    title: &str,
) {
    let _ = tx
        .send(serde_json::json!({
            "type": "session_title",
            "title": title,
            "session_id": session_id,
        }))
        .await;
}

async fn generate_title_inner(
    agent: &AgentState,
    llm: &Arc<LlmState>,
    session_id: &str,
    model: &str,
) -> Result<Option<String>, String> {
    // 竞态守卫：title 已非空则不覆盖
    let session = agent
        .db
        .agent_get_session(session_id)
        .await
        .map_err(|e| format!("db error: {e}"))?
        .ok_or_else(|| "session not found".to_string())?;
    if session
        .title
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        return Ok(None);
    }

    // 首条 user 消息
    let messages = agent
        .db
        .agent_list_messages(session_id)
        .await
        .map_err(|e| format!("db error: {e}"))?;
    let first_user = messages
        .iter()
        .find(|m| m.role == "user" && m.kind == "message")
        .ok_or_else(|| "no user message yet".to_string())?;
    let excerpt: String = first_user
        .content
        .chars()
        .take(USER_MSG_MAX_CHARS)
        .collect();

    let request = ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage::text(
                "system",
                "Generate a short title (max 20 characters) for a conversation that starts with the following user message. Output only the title text — no quotes, no trailing punctuation, no explanation.",
            ),
            ChatMessage::text("user", &excerpt),
        ],
        stream: false,
        max_tokens: Some(30),
        temperature: None,
        top_p: None,
        tools: None,
        tool_choice: None,
        raw_body: None,
    };
    let chain = crate::llm::router::resolve_with_failover(llm, model)
        .await
        .map_err(|e| format!("model resolution failed: {e}"))?;
    let body = crate::llm::upstream::build_upstream_body(&request);
    let outcome =
        crate::llm::upstream::execute_with_failover(&llm.breakers, &chain, &body, false).await;
    let crate::llm::upstream::FailoverOutcome::Success { resp, .. } = outcome else {
        return Err("LLM unavailable for title generation".to_string());
    };
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .map_err(|e| format!("read response failed: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid JSON: {e}"))?;
    let raw = json
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let Some(title) = clean_title(raw) else {
        return Err("empty title from LLM".to_string());
    };

    // 写回前再次守卫（生成耗时期间用户可能改名）
    let current = agent
        .db
        .agent_get_session(session_id)
        .await
        .map_err(|e| format!("db error: {e}"))?;
    if current
        .and_then(|s| s.title)
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        return Ok(None);
    }
    agent
        .db
        .agent_update_session_title(session_id, &title)
        .await
        .map_err(|e| format!("db error: {e}"))?;
    tracing::info!(session_id, title, "auto-generated session title");
    Ok(Some(title))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_clean_title() {
        assert_eq!(
            clean_title("  修复登录 bug  "),
            Some("修复登录 bug".to_string())
        );
        assert_eq!(
            clean_title("\"quoted title\""),
            Some("quoted title".to_string())
        );
        assert_eq!(clean_title("   "), None);
        assert_eq!(clean_title(""), None);
        let long = "标".repeat(50);
        assert_eq!(clean_title(&long).unwrap().chars().count(), TITLE_MAX_CHARS);
    }

    #[tokio::test]
    async fn test_notify_title_sends_session_title_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<serde_json::Value>(4);
        notify_title(&tx, "sess-123", "修复登录 bug").await;
        let frame = rx.recv().await.expect("frame should be sent");
        assert_eq!(
            frame,
            json!({
                "type": "session_title",
                "title": "修复登录 bug",
                "session_id": "sess-123",
            })
        );
    }

    #[tokio::test]
    async fn test_notify_title_dropped_receiver_is_noop() {
        // 连接已断：接收端（push_task）已停。tokio mpsc 的 send 在 receiver 被 drop
        // 后立即返回 Err，`let _ =` 静默忽略——不 panic、不阻塞。
        let (tx, rx) = tokio::sync::mpsc::channel::<serde_json::Value>(4);
        drop(rx);
        notify_title(&tx, "sess-123", "修复登录 bug").await;
        assert!(tx.is_closed());
    }
}
