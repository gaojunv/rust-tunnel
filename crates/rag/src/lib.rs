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
    // `index_vector = 0` 的容器（纯 pages）没有向量 shard，被误指定为 RAG 知识库
    // 时若放行，会对空 emb_base_url 发起一次注定失败的 embedding 调用。在校验
    // 层挡住，比靠下游失败降级更快也更明确。
    let kb = match db.ks_get(kb_id).await {
        Ok(Some(k)) if k.enabled != 0 && k.index_vector != 0 => k,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_tunnel_persistence::knowledge::KsCreateOpts;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    /// 建一个 vector 容器（`enabled`/`index_vector` 可覆盖），返回记录。
    async fn create_vector_source(db: &Database, id: &str, base: &str, enabled: bool, vector: bool) {
        db.ks_create(&KsCreateOpts {
            id: id.to_owned(),
            name: format!("kb-{id}"),
            summary: String::new(),
            index_vector: vector,
            index_pages: !vector,
            scope_type: "global".to_owned(),
            client_id: String::new(),
            workspace_id: String::new(),
            emb_base_url: base.to_owned(),
            emb_api_key: "sk-plain".to_owned(),
            emb_model: "test-model".to_owned(),
            emb_dimension: 8,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled,
        })
        .await
        .unwrap();
    }

    /// 记录调用次数的 embedding mock：校验层若放行，这里会收到一次请求。
    async fn counting_embedding_server(calls: Arc<AtomicUsize>) -> String {
        use axum::{extract::Json, routing::post, Router};
        let app = Router::new().route(
            "/embeddings",
            post(move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"object": "list", "data": []}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// pages-only 容器被误指定为 RAG 知识库：校验层直接挡下，一次 embedding
    /// 调用都不该发。
    #[tokio::test]
    async fn pages_only_container_is_rejected_before_embedding() {
        let db = Database::new(":memory:").await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let base = counting_embedding_server(calls.clone()).await;
        create_vector_source(&db, "pages-only", &base, true, false).await;
        let store = VectorStore::new(std::path::Path::new("/tmp"));

        let got = retrieve_context(&db, &store, None, "pages-only", "随便问点什么").await;

        assert!(got.is_none());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "pages-only 容器不应触发 embedding 调用"
        );
    }

    /// 停用容器同样在校验层挡下（原有行为，防回归）。
    #[tokio::test]
    async fn disabled_container_is_rejected_before_embedding() {
        let db = Database::new(":memory:").await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let base = counting_embedding_server(calls.clone()).await;
        create_vector_source(&db, "off", &base, false, true).await;
        let store = VectorStore::new(std::path::Path::new("/tmp"));

        let got = retrieve_context(&db, &store, None, "off", "随便问点什么").await;

        assert!(got.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
