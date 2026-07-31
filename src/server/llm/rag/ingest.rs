//! 文档摄入后台任务：分块 → embedding → 写向量 → 更新状态 → 发 SSE 事件。

use tokio::sync::broadcast;
use crate::server::db::Database;
use crate::server::llm::crypto::{decrypt_field, LlmCipher};
use crate::server::db::rag::RagKnowledgeBaseRecord;
use super::{chunker, embedder::Embedder, store::{ChunkPoint, VectorStore}};

/// 文档状态变更事件（SSE 推送给前端）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct KbEvent {
    pub doc_id: String,
    pub kb_id: String,
    pub status: String,
    pub chunk_count: i64,
    pub error: Option<String>,
}

/// 启动文档摄入后台任务。
///
/// 流程：doc status=processing（发事件）→ chunk → embed 批量 → store.upsert →
/// rag_insert_chunks → doc status=ready + chunk_count（发事件）；任何失败 →
/// doc status=failed + error（发事件）。事件与 db 状态写入一一对应，
/// 事件在状态落库之后发出，前端可据此轮询/推送双通道感知进度。
pub fn spawn_ingest(
    db: Database,
    store: VectorStore,
    cipher: Option<LlmCipher>,
    kb: RagKnowledgeBaseRecord,
    doc_id: String,
    content: String,
    tx: broadcast::Sender<KbEvent>,
) {
    tokio::spawn(async move {
        let emit = |status: &str, count: i64, err: Option<String>| {
            let _ = tx.send(KbEvent {
                doc_id: doc_id.clone(),
                kb_id: kb.id.clone(),
                status: status.to_string(),
                chunk_count: count,
                error: err,
            });
        };

        // processing
        let _ = db.rag_update_document_status(&doc_id, "processing", 0, None).await;
        emit("processing", 0, None);

        match do_ingest(&db, &store, cipher.as_ref(), &kb, &doc_id, &content).await {
            Ok(count) => {
                let _ = db.rag_update_document_status(&doc_id, "ready", count, None).await;
                emit("ready", count, None);
            }
            Err(e) => {
                let _ = db.rag_update_document_status(&doc_id, "failed", 0, Some(&e)).await;
                emit("failed", 0, Some(e));
            }
        }
    });
}

/// 分块 → 向量化 → 写 shard → 落库，返回分块数。失败时返回人类可读错误。
async fn do_ingest(
    db: &Database,
    store: &VectorStore,
    cipher: Option<&LlmCipher>,
    kb: &RagKnowledgeBaseRecord,
    doc_id: &str,
    content: &str,
) -> Result<i64, String> {
    let chunks = chunker::chunk_markdown(content, kb.chunk_size as usize, kb.chunk_overlap as usize);
    if chunks.is_empty() {
        return Err("empty content".to_string());
    }
    let api_key = decrypt_field(cipher, &kb.emb_api_key).map_err(|e| e.to_string())?;
    let embedder = Embedder::new(&kb.emb_base_url, &api_key, &kb.emb_model);
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let vectors = embedder.embed(&texts).await.map_err(|e| e.to_string())?;

    // 写向量 + 元数据
    let mut points = Vec::with_capacity(chunks.len());
    let mut rows = Vec::with_capacity(chunks.len());
    for (i, (c, v)) in chunks.iter().zip(vectors).enumerate() {
        let cid = uuid::Uuid::new_v4().to_string();
        points.push(ChunkPoint {
            id: cid.clone(), vector: v, doc_id: doc_id.to_string(),
            seq: i as i64, heading_path: c.heading_path.clone(),
        });
        rows.push((cid, doc_id.to_string(), kb.id.clone(), i as i64,
                   c.heading_path.clone(), c.content.clone(), c.token_count as i64));
    }
    store.upsert(&kb.id, kb.emb_dimension as usize, points).await
        .map_err(|e| e.to_string())?;
    db.rag_insert_chunks(&rows).await.map_err(|e| e.to_string())?;
    Ok(chunks.len() as i64)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::broadcast;

    use crate::server::db::Database;
    use crate::server::db::rag::RagKnowledgeBaseRecord;

    use super::*;

    /// TempDir 放前、store 放后：qadrant-edge 的 `EdgeShard` Drop 时同步 flush
    /// 并 `expect()`（目录已删会 panic），故 store 必须先于 TempDir 析构。
    fn tmp_store() -> (tempfile::TempDir, VectorStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::new(dir.path());
        (dir, store)
    }

    /// 建一个指向给定 embedding base 的知识库并返回完整 record。
    async fn create_kb(db: &Database, id: &str, emb_base_url: &str, dim: i64) -> RagKnowledgeBaseRecord {
        db.rag_create_kb(
            id,
            "测试库",
            "描述",
            emb_base_url,
            "sk-plain",
            "test-model",
            dim,
            5,   // top_k
            512, // chunk_size
            64,  // chunk_overlap
            0.3, // score_threshold
            true,
        )
        .await
        .unwrap();
        db.rag_get_kb(id).await.unwrap().unwrap()
    }

    /// 起一个返回固定 embedding 的本地 HTTP server，返回 base_url。
    async fn mock_embedding_server(dim: usize) -> String {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;
        use serde_json::{json, Value};
        let app = Router::new().route(
            "/embeddings",
            post(move |body: Json<Value>| async move {
                let n = body["input"].as_array().map(|a| a.len()).unwrap_or(1);
                let data: Vec<_> = (0..n)
                    .map(|i| {
                        json!({
                            "index": i,
                            "embedding": vec![0.1f32; dim],
                            "object": "embedding"
                        })
                    })
                    .collect();
                Json(json!({"object": "list", "data": data}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{}", addr)
    }

    /// 等待下一条摄入事件（10s 超时，避免测试无限挂起）。
    async fn next_event(rx: &mut broadcast::Receiver<KbEvent>) -> KbEvent {
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timeout waiting for ingest event")
            .expect("broadcast channel closed");
        ev
    }

    #[tokio::test]
    async fn ingest_produces_ready_doc_with_chunks() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let kb = create_kb(&db, "kb-1", &base, 8).await;
        let doc_id = "doc-1".to_string();
        db.rag_create_document(&doc_id, &kb.id, "guide.md", "sha256:abc")
            .await
            .unwrap();
        let content = "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n\n## 配置\n\n编辑 config.toml。\n".to_string();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        spawn_ingest(
            db.clone(),
            store.clone(),
            None,
            kb.clone(),
            doc_id.clone(),
            content,
            tx,
        );

        // 事件序列：processing → ready
        let s1 = next_event(&mut rx).await;
        assert_eq!(s1.status, "processing");
        assert_eq!(s1.doc_id, doc_id);
        let s2 = next_event(&mut rx).await;
        assert_eq!(s2.status, "ready");
        assert!(s2.error.is_none());

        // db 状态与分块落库
        let doc = db.rag_get_document(&doc_id).await.unwrap().unwrap();
        assert_eq!(doc.status, "ready");
        assert!(doc.chunk_count > 0);
        assert!(doc.error.is_none());
        assert_eq!(s2.chunk_count, doc.chunk_count);
        assert_eq!(db.rag_count_kb_chunks(&kb.id).await.unwrap(), doc.chunk_count);

        // 向量已写入：同 kb search 能命中
        let query = [1.0f32; 8];
        let hits = store.search(&kb.id, kb.emb_dimension as usize, &query, 5).await;
        assert_eq!(hits.len() as i64, doc.chunk_count);
        assert!(!hits.is_empty());
    }

    #[tokio::test]
    async fn ingest_failure_marks_doc_failed() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        // emb_base_url 指向不可达地址（127.0.0.1:1 → connection refused）
        let kb = create_kb(&db, "kb-fail", "http://127.0.0.1:1", 8).await;
        let doc_id = "doc-fail".to_string();
        db.rag_create_document(&doc_id, &kb.id, "x.md", "sha256:x")
            .await
            .unwrap();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        spawn_ingest(
            db.clone(),
            store.clone(),
            None,
            kb,
            doc_id.clone(),
            "some content".to_string(),
            tx,
        );

        // 事件序列：processing → failed
        let s1 = next_event(&mut rx).await;
        assert_eq!(s1.status, "processing");
        let s2 = next_event(&mut rx).await;
        assert_eq!(s2.status, "failed");
        assert!(s2.error.as_deref().is_some_and(|e| !e.is_empty()));

        let doc = db.rag_get_document(&doc_id).await.unwrap().unwrap();
        assert_eq!(doc.status, "failed");
        assert!(doc.error.as_deref().is_some_and(|e| !e.is_empty()));
    }
}
