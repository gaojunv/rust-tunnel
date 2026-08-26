//! RAG 知识库：多格式文本提取（extractor）→ Markdown 分块（chunker）→
//! 远端 embedding（embedder）→ qdrant-edge 向量 shard（store）→
//! 检索与注入文本组装（retriever）→ 后台摄入任务（ingest）。
//!
//! 本 crate 不依赖 llm 协议类型；[`retrieve_context`] 返回结构化检索结果，
//! 由调用方（llm pipeline）注入自己的请求消息类型。

// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod chunker;
pub mod doc_store;
pub mod embedder;
pub mod extractor;
pub mod ingest;
pub mod retriever;
pub mod store;

use retriever::RetrievedChunk;
use rust_tunnel_common::crypto::LlmCipher;
use rust_tunnel_persistence::Database;
use store::VectorStore;

/// RAG 检索结果：命中 chunk + 组装好的注入文本。
///
/// 调用方负责把 `system_message` 注入自己的请求消息类型——rag 模块
/// 不依赖 llm 协议类型（`ChatCompletionRequest`/`ChatMessage`），
/// 这是 rag 拆分为独立 crate 的接缝。
#[derive(Debug, Clone)]
pub struct RagContext {
    /// 命中的向量块，含原文、标题路径与得分。
    pub chunks: Vec<RetrievedChunk>,
    /// 注入 LLM 的系统提示词，由命中块组装而成。
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
    let kb = match db.ks_get(kb_id).await {
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
