pub mod chunker;
pub mod embedder;
pub mod extractor;
pub mod ingest;
pub mod retriever;
pub mod store;

use crate::db::Database;
use crate::llm::crypto::LlmCipher;
use retriever::RetrievedChunk;
use store::VectorStore;

/// RAG 检索结果：命中 chunk + 组装好的注入文本。
///
/// 调用方负责把 `system_message` 注入自己的请求消息类型——rag 模块
/// 不依赖 llm 协议类型（`ChatCompletionRequest`/`ChatMessage`），
/// 这是 rag 拆分为独立 crate 的接缝。
#[derive(Debug, Clone)]
pub struct RagContext {
    pub chunks: Vec<RetrievedChunk>,
    pub system_message: String,
}

/// RAG 检索入口：查 KB 配置、检索 top-K、组装注入文本。
///
/// 失败降级：embedding 失败 / 无命中 / KB 禁用 / 查询为空时返回 None。
/// 永不返回 Err。
pub async fn retrieve_context(
    db: &Database,
    store: &VectorStore,
    cipher: Option<&LlmCipher>,
    kb_id: &str,
    query_text: &str,
) -> Option<RagContext> {
    if query_text.trim().is_empty() {
        return None;
    }
    let kb = match db.rag_get_kb(kb_id).await {
        Ok(Some(k)) if k.enabled != 0 => k,
        _ => return None,
    };
    let chunks = retriever::retrieve(db, store, cipher, &kb, query_text).await;
    if chunks.is_empty() {
        return None;
    }
    let system_message = retriever::build_system_message(&chunks);
    Some(RagContext {
        chunks,
        system_message,
    })
}
