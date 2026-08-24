//! 文档摄入后台任务：分块 → embedding → 写向量 → 更新状态 → 发 SSE 事件。

use super::{
    chunker,
    embedder::Embedder,
    extractor::{self, FileType},
    store::{ChunkPoint, VectorStore},
};
use futures_util::FutureExt;
use rust_tunnel_common::crypto::{decrypt_field, LlmCipher};
use rust_tunnel_persistence::rag::RagKnowledgeBaseRecord;
use rust_tunnel_persistence::Database;
use std::sync::Arc;
use tokio::sync::broadcast;

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
/// 流程：doc status=processing（发事件）→ 提取 → chunk → embed 批量 → store.upsert →
/// rag_insert_chunks → doc status=ready + chunk_count（发事件）；任何失败 →
/// doc status=failed + error（发事件）。事件与 db 状态写入一一对应，
/// 事件在状态落库之后发出，前端可据此轮询/推送双通道感知进度。
///
/// `source_path` 为已落盘的原始文件路径（`<data_dir>/rag_docs/<kb_id>/<doc_id>.<ext>`，
/// 见 mgmt/api/rag.rs `doc_source_path`），`file_type` 决定解析方式。
///
/// `sem` 为可选并发信号量：全量重建时由调用方注入 `Some(Arc<Semaphore(4)>)`，任务持有
/// permit 直到结束，避免对远端 embedding 服务瞬时打满；`None` 表示不限（单文档上传/reindex）。
#[allow(clippy::too_many_arguments)]
pub fn spawn_ingest(
    db: Database,
    store: VectorStore,
    cipher: Option<LlmCipher>,
    kb: RagKnowledgeBaseRecord,
    doc_id: String,
    source_path: std::path::PathBuf,
    file_type: FileType,
    tx: broadcast::Sender<KbEvent>,
    sem: Option<Arc<tokio::sync::Semaphore>>,
) {
    tokio::spawn(async move {
        // 限并发：全量重建时持有 permit 到任务结束（释放后下一个任务才能获取）；
        // None 表示不限制（单文档路径）。acquire_owned 拿到 OwnedSemaphorePermit，
        // 任务结束随 `_guard` 析构自动释放，无需借用语义。
        let _guard = match sem {
            Some(s) => Some(s.acquire_owned().await.ok()),
            None => None,
        };

        let emit = |status: &str, count: i64, err: Option<String>| {
            let _ = tx.send(KbEvent {
                doc_id: doc_id.clone(),
                kb_id: kb.id.clone(),
                status: status.to_string(),
                chunk_count: count,
                error: err,
            });
        };

        // 任务体整体包进 catch_unwind：提取/分块/embedding/store 任一步 panic
        // （如解析器内部 expect）不能让任务静默死亡、doc 永久卡 processing。
        // AssertUnwindSafe 绕过异步状态 UnwindSafe 的静态检查——panic 后我们
        // 只用任务体自有的克隆（db/doc_id/kb/tx），无跨 await 共享借用。
        let result = std::panic::AssertUnwindSafe(async {
            // processing
            let _ = db
                .rag_update_document_status(&doc_id, "processing", 0, None)
                .await;
            emit("processing", 0, None);

            match do_ingest(
                &db,
                &store,
                cipher.as_ref(),
                &kb,
                &doc_id,
                &source_path,
                file_type,
            )
            .await
            {
                Ok(count) => {
                    let _ = db
                        .rag_update_document_status(&doc_id, "ready", count, None)
                        .await;
                    emit("ready", count, None);
                }
                Err(e) => {
                    let _ = db
                        .rag_update_document_status(&doc_id, "failed", 0, Some(&e))
                        .await;
                    emit("failed", 0, Some(e));
                }
            }
        })
        .catch_unwind()
        .await;

        // panic 兜底：与 do_ingest 返回 Err 同语义——置 failed 并记录 panic 信息，
        // 让 doc 不卡 processing、前端能感知失败。
        if let Err(payload) = result {
            let msg = panic_message(&*payload);
            tracing::error!(doc_id = %doc_id, kb_id = %kb.id, panic = %msg, "rag ingest task panicked");
            let _ = db
                .rag_update_document_status(&doc_id, "failed", 0, Some(&msg))
                .await;
            emit("failed", 0, Some(msg));
        }
    });
}

/// 从 `catch_unwind` 的 panic payload 提取人类可读消息（`&str` 或 `String`）。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// 提取 → 分块 → 向量化 → 写 shard → 落库，返回分块数。失败时返回人类可读错误。
async fn do_ingest(
    db: &Database,
    store: &VectorStore,
    cipher: Option<&LlmCipher>,
    kb: &RagKnowledgeBaseRecord,
    doc_id: &str,
    source_path: &std::path::Path,
    file_type: FileType,
) -> Result<i64, String> {
    let bytes = tokio::fs::read(source_path)
        .await
        .map_err(|e| format!("read source file: {e}"))?;
    // CPU 密集的解析放阻塞池，避免卡住 tokio worker（PDF 解析大文件可达数百 ms）。
    let content = tokio::task::spawn_blocking(move || extractor::extract(&bytes, file_type))
        .await
        .map_err(|e| format!("extract task: {e}"))?
        .map_err(|e| e.to_string())?;
    if content.trim().is_empty() {
        return Err("no text extracted from document".to_string());
    }
    let chunks =
        chunker::chunk_markdown(&content, kb.chunk_size as usize, kb.chunk_overlap as usize);
    if chunks.is_empty() {
        return Err("empty content".to_string());
    }
    let api_key = decrypt_field(cipher, &kb.emb_api_key).map_err(|e| e.to_string())?;
    let embedder = Embedder::new(&kb.emb_base_url, &api_key, &kb.emb_model);
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let vectors = embedder.embed(&texts).await.map_err(|e| e.to_string())?;
    if vectors.len() != texts.len() {
        return Err(format!(
            "embedding count mismatch: sent {}, got {}",
            texts.len(),
            vectors.len()
        ));
    }

    // 写向量 + 元数据
    let mut points = Vec::with_capacity(chunks.len());
    let mut rows = Vec::with_capacity(chunks.len());
    for (i, (c, v)) in chunks.iter().zip(vectors).enumerate() {
        let cid = uuid::Uuid::new_v4().to_string();
        points.push(ChunkPoint {
            id: cid.clone(),
            vector: v,
            doc_id: doc_id.to_string(),
            seq: i as i64,
            heading_path: c.heading_path.clone(),
        });
        rows.push((
            cid,
            doc_id.to_string(),
            kb.id.clone(),
            i as i64,
            c.heading_path.clone(),
            c.content.clone(),
            c.token_count as i64,
        ));
    }
    store
        .upsert(&kb.id, kb.emb_dimension as usize, points)
        .await
        .map_err(|e| e.to_string())?;
    if let Err(e) = db.rag_insert_chunks(&rows).await {
        // 回滚本任务刚写入的向量：rag_insert_chunks 失败意味着元数据未落库
        // （FK 失败——doc 在摄入中途被删、或库被软关后的竞态），不清理则这些
        // 向量永久残留（chunk id 不在 rag_chunks 中，检索不可见，纯磁盘泄漏）。
        // best-effort：失败仅 warn，DB 仍是源，不影响错误上报。
        if let Err(se) = store
            .delete_by_doc(&kb.id, kb.emb_dimension as usize, doc_id)
            .await
        {
            tracing::warn!(kb_id = %kb.id, doc_id, error = %se, "rag ingest: vector rollback failed");
        }
        return Err(e.to_string());
    }
    Ok(chunks.len() as i64)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::broadcast;

    use crate::extractor::pdf::make_empty_page_pdf;
    use rust_tunnel_persistence::rag::RagKnowledgeBaseRecord;
    use rust_tunnel_persistence::Database;

    use super::*;

    /// 把文本写入临时源文件，返回 (TempDir, 路径)。TempDir 需活到任务结束。
    fn write_source(content: &str, ext: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("doc.{ext}"));
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// 字节版 write_source：用于二进制 fixture（PDF/OOXML）。
    fn write_source_bytes(bytes: &[u8], ext: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("doc.{ext}"));
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    /// TempDir 放前、store 放后：qadrant-edge 的 `EdgeShard` Drop 时同步 flush
    /// 并 `expect()`（目录已删会 panic），故 store 必须先于 TempDir 析构。
    fn tmp_store() -> (tempfile::TempDir, VectorStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::new(dir.path());
        (dir, store)
    }

    /// 建一个指向给定 embedding base 的知识库并返回完整 record。
    async fn create_kb(
        db: &Database,
        id: &str,
        emb_base_url: &str,
        dim: i64,
    ) -> RagKnowledgeBaseRecord {
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{}", addr)
    }

    /// 起一个返回固定 embedding 的本地 HTTP server，但 data 比 input 少一条
    /// （模拟服务商部分结果，触发 do_ingest 的 count mismatch 校验）。
    async fn mock_embedding_server_short(dim: usize) -> String {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;
        use serde_json::{json, Value};
        let app = Router::new().route(
            "/embeddings",
            post(move |body: Json<Value>| async move {
                let n = body["input"].as_array().map(|a| a.len()).unwrap_or(1);
                let data: Vec<_> = (0..n.saturating_sub(1))
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        db.rag_create_document(&doc_id, &kb.id, "guide.md", "sha256:abc", "md")
            .await
            .unwrap();
        let content =
            "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n\n## 配置\n\n编辑 config.toml。\n"
                .to_string();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, src) = write_source(&content, "md");
        spawn_ingest(
            db.clone(),
            store.clone(),
            None,
            kb.clone(),
            doc_id.clone(),
            src,
            FileType::Markdown,
            tx,
            None,
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
        assert_eq!(
            db.rag_count_kb_chunks(&kb.id).await.unwrap(),
            doc.chunk_count
        );

        // 向量已写入：同 kb search 能命中
        let query = [1.0f32; 8];
        let hits = store
            .search(&kb.id, kb.emb_dimension as usize, &query, 5)
            .await;
        assert_eq!(hits.len() as i64, doc.chunk_count);
        assert!(!hits.is_empty());
    }

    #[tokio::test]
    async fn ingest_failure_marks_doc_failed() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        // emb_base_url 指向不可达地址（127.0.0.1:1 → connection refused）
        let kb = create_kb(&db, "kb-fail", "http://127.0.0.1:1", 8).await;
        let doc_id = "doc-fail".to_string();
        db.rag_create_document(&doc_id, &kb.id, "x.md", "sha256:x", "md")
            .await
            .unwrap();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, src) = write_source("some content", "md");
        spawn_ingest(
            db.clone(),
            store.clone(),
            None,
            kb,
            doc_id.clone(),
            src,
            FileType::Markdown,
            tx,
            None,
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

    #[tokio::test]
    async fn embed_count_mismatch_marks_doc_failed() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        // mock 返回的向量比输入文本少一条 → do_ingest 报 count mismatch，
        // 而非静默丢弃尾部 chunk 后仍报 ready。
        let base = mock_embedding_server_short(8).await;
        let kb = create_kb(&db, "kb-short", &base, 8).await;
        let doc_id = "doc-short".to_string();
        db.rag_create_document(&doc_id, &kb.id, "y.md", "sha256:y", "md")
            .await
            .unwrap();
        // 内容需产生至少 2 个 chunk，使 mock 的 n-1 返回值非空
        // （否则会先触发 embedder 的 EmptyResponse，而非本校验）。
        let content = "# A\n\n段落一。\n\n## B\n\n段落二。\n".to_string();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, src) = write_source(&content, "md");
        spawn_ingest(
            db.clone(),
            store.clone(),
            None,
            kb.clone(),
            doc_id.clone(),
            src,
            FileType::Markdown,
            tx,
            None,
        );

        // 事件序列：processing → failed，error 含 count mismatch
        let s1 = next_event(&mut rx).await;
        assert_eq!(s1.status, "processing");
        let s2 = next_event(&mut rx).await;
        assert_eq!(s2.status, "failed");
        let err = s2.error.as_deref().expect("failed event has error");
        assert!(
            err.contains("count mismatch"),
            "error should mention count mismatch: {err}"
        );

        let doc = db.rag_get_document(&doc_id).await.unwrap().unwrap();
        assert_eq!(doc.status, "failed");
        assert!(doc
            .error
            .as_deref()
            .is_some_and(|e| e.contains("count mismatch")));
        // 尾部 chunk 未入索引：count 保持 0
        assert_eq!(doc.chunk_count, 0);
        assert_eq!(db.rag_count_kb_chunks(&kb.id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn ingest_insert_failure_rolls_back_vectors() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let kb = create_kb(&db, "kb-rollback", &base, 8).await;
        let (_d, store) = tmp_store();

        // 不创建 doc 行：rag_insert_chunks 因 FK（doc_id 不存在）失败。
        // 这是「doc 在摄入中途被删 / insert 落库失败」的确定性模拟 ——
        // upsert 已写入向量，随后 insert 失败，必须回滚本次向量防孤儿残留。
        let content = "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n".to_string();
        let (_sd, src) = write_source(&content, "md");
        let res = do_ingest(
            &db,
            &store,
            None,
            &kb,
            "ghost-doc",
            &src,
            FileType::Markdown,
        )
        .await;
        assert!(res.is_err(), "FK 失败应使摄入失败: {res:?}");

        // 向量已回滚：同 kb search 应为空（不留孤儿向量），分块也未落库。
        let hits = store
            .search(&kb.id, kb.emb_dimension as usize, &[1.0f32; 8], 10)
            .await;
        assert!(hits.is_empty(), "insert 失败后应回滚本次写入的向量");
        assert_eq!(db.rag_count_kb_chunks(&kb.id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn ingest_extract_failure_marks_doc_failed() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let kb = create_kb(&db, "kb-scan", &base, 8).await;
        let doc_id = "doc-scan".to_string();
        db.rag_create_document(&doc_id, &kb.id, "scan.pdf", "sha256:x", "pdf")
            .await
            .unwrap();
        // 无文本层 PDF（复用 extractor::pdf 测试用空页 PDF 构造）。
        let (_sd, src) = write_source_bytes(&make_empty_page_pdf(), "pdf");
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        spawn_ingest(
            db.clone(),
            store.clone(),
            None,
            kb.clone(),
            doc_id.clone(),
            src,
            FileType::Pdf,
            tx,
            None,
        );

        // 事件序列：processing → failed，error 含 no text layer
        let s1 = next_event(&mut rx).await;
        assert_eq!(s1.status, "processing");
        let s2 = next_event(&mut rx).await;
        assert_eq!(s2.status, "failed");
        assert!(
            s2.error
                .as_deref()
                .is_some_and(|e| e.contains("no text layer")),
            "error should mention no text layer: {:?}",
            s2.error
        );

        let doc = db.rag_get_document(&doc_id).await.unwrap().unwrap();
        assert_eq!(doc.status, "failed");
        assert!(doc
            .error
            .as_deref()
            .is_some_and(|e| e.contains("no text layer")));
        assert_eq!(db.rag_count_kb_chunks(&kb.id).await.unwrap(), 0);
    }

    #[test]
    fn panic_message_extracts_readable_payload() {
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*str_payload), "boom");
        let string_payload: Box<dyn std::any::Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_message(&*string_payload), "boom");
        let other: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*other), "unknown panic");
    }
}
