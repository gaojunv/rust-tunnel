pub mod chunker;
pub mod embedder;
pub mod extractor;
pub mod ingest;
pub mod retriever;
pub mod store;

use crate::server::db::Database;
use crate::server::llm::crypto::LlmCipher;
use crate::server::llm::{ChatCompletionRequest, ChatMessage};
use store::VectorStore;

/// RAG 增强结果：注入的 chunk 数（0 = 未注入）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RagOutcome {
    pub injected: usize,
}

/// 取最后一条 user 消息的文本。
fn last_user_text(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
        .filter(|t| !t.trim().is_empty())
}

/// 把一条 system 消息插到 messages 最前。
fn inject_system(messages: &mut Vec<ChatMessage>, text: String) {
    messages.insert(0, ChatMessage::text("system", text));
}

/// RAG 增强入口：检索 KB 并把背景资料注入 messages[0]。
///
/// 失败降级：embedding 失败 / 无命中 / KB 禁用 / 无 user 消息时，
/// 原样不动 request，返回 RagOutcome{ injected: 0 }。永不返回 Err。
pub async fn enhance(
    db: &Database,
    store: &VectorStore,
    cipher: Option<&LlmCipher>,
    kb_id: &str,
    request: &mut ChatCompletionRequest,
) -> RagOutcome {
    let Some(query_text) = last_user_text(&request.messages) else {
        return RagOutcome::default();
    };
    let kb = match db.rag_get_kb(kb_id).await {
        Ok(Some(k)) if k.enabled != 0 => k,
        _ => return RagOutcome::default(),
    };
    let chunks = retriever::retrieve(db, store, cipher, &kb, &query_text).await;
    if chunks.is_empty() {
        return RagOutcome::default();
    }
    let injected = chunks.len();
    let msg = retriever::build_system_message(&chunks);
    inject_system(&mut request.messages, msg);
    RagOutcome { injected }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_user_message_extracted() {
        let msgs = vec![
            crate::server::llm::ChatMessage::text("system", "s"),
            crate::server::llm::ChatMessage::text("user", "第一句"),
            crate::server::llm::ChatMessage::text("assistant", "答"),
            crate::server::llm::ChatMessage::text("user", "第二句"),
        ];
        assert_eq!(last_user_text(&msgs), Some("第二句".to_string()));
    }

    #[test]
    fn inject_prepends_system_message() {
        let mut msgs = vec![crate::server::llm::ChatMessage::text("user", "问")];
        inject_system(&mut msgs, "KB内容".into());
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content.as_deref(), Some("KB内容"));
        assert_eq!(msgs[1].role, "user");
    }
}
