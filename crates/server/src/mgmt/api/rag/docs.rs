//! RAG 文档 handlers：列表 / 详情 / multipart 上传 / 删除 / reindex。
use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use crate::db::knowledge::{IndexKind, KnowledgeDocIndexRecord, KnowledgeDocRecord, KnowledgeSourceRecord};
use crate::llm::rag::extractor::FileType;
use crate::llm::rag::ingest::{spawn_ingest, IngestOpts};
use crate::llm::rag::store::VectorStore;
use crate::mgmt::api::ApiState;
use sha2::Digest;

use super::{rag_rt, RagRuntime};

/// 文档视图 JSON：保持旧 `RagDocumentRecord` 的键名与结构不变（`kb_id`/`chunk_count` 等）。
/// `KnowledgeDocRecord` 本身不含状态，状态由 `knowledge_doc_index` per-(doc,Vector) 提供。
fn doc_json(doc: &KnowledgeDocRecord, idx: Option<&KnowledgeDocIndexRecord>) -> serde_json::Value {
    serde_json::json!({
        "id": doc.id,
        "kb_id": doc.source_id,
        "filename": doc.filename,
        "file_type": doc.file_type,
        "content_hash": doc.content_hash,
        "status": idx.map(|i| i.status.as_str()).unwrap_or("pending"),
        "chunk_count": idx.map(|i| i.item_count).unwrap_or(0),
        "error": idx.and_then(|i| i.error.clone()),
        "created_at": doc.created_at,
        "updated_at": doc.updated_at,
    })
}


/// 文档原文落盘路径。薄包装 [`rust_tunnel_rag::doc_store::doc_source_path`]，
/// 只为省掉调用点的 `rt.store.data_dir()` 样板；`pub(crate)` 供 `kb.rs` 全量
/// 重建路径引用。
pub(crate) fn doc_source_path(
    store: &VectorStore,
    kb_id: &str,
    doc_id: &str,
    ext: &str,
) -> std::path::PathBuf {
    crate::llm::rag::doc_store::doc_source_path(store.data_dir(), kb_id, doc_id, ext)
}

/// `reindex_kb_doc` 的结果：全量重建路径据此统计，单文档端点据此映射 409。
pub(crate) enum ReindexOutcome {
    /// 已抢占并 spawn 摄入任务。
    Spawned,
    /// 文档在途（pending/processing），CAS 未抢到，跳过。
    Skipped,
    /// 原文缺失：文档已置 failed（提示删除重传），未 spawn。
    MissingSource,
}

/// 重建单文档索引的完整流程：原子 CAS 置 pending（防与在途摄入双写）→ 校验原文
/// 存在（缺失回滚为 failed）→ 清旧索引（向量 + SQLite 分块）→ `spawn_ingest` 走完整
/// 摄入。单文档端点（`reindex_doc`）与全量重建（`kb.rs update_kb` 的 emb 变更分支）
/// 共用本函数，保证每文档的原子性语义一致。
///
/// `sem` 为可选并发信号量（全量重建时限流用，见 `spawn_ingest`）。
pub(crate) async fn reindex_kb_doc(
    rt: &RagRuntime,
    kb: &KnowledgeSourceRecord,
    doc: &KnowledgeDocRecord,
    sem: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<ReindexOutcome, (StatusCode, String)> {
    // 并发防护：pending/processing 表示原始摄入或上一次 reindex 仍在途，
    // 此时再 reindex 会与在途任务同时写向量+分块 → 重复数据。
    // 用原子 CAS 抢占（check-then-act 会让两个并发请求双双通过守卫），
    // 只有一个请求能把状态从 ready/failed 置回 pending。
    match rt.db.kdoc_mark_pending_if_idle(&doc.id, IndexKind::Vector).await {
        Ok(true) => {}
        Ok(false) => return Ok(ReindexOutcome::Skipped),
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}"))),
    }

    // 定位原文（摄入时已按 file_type 落盘，二进制原文也按同类型重新解析）。
    // 老数据 file_type 可能为 ""（迁移前落库），回退 Markdown 保持旧路径兼容。
    let file_type = FileType::from_extension(&doc.file_type).unwrap_or(FileType::Markdown);
    let source_path = doc_source_path(&rt.store, &kb.id, &doc.id, file_type.as_str());
    // 存在性检查（读字节太贵，先 metadata 探测）：缺失 → 无法无损重建，提示删除重传。
    // CAS 已把状态置为 pending，此处失败需回滚，否则文档永远卡在 pending。
    if tokio::fs::metadata(&source_path).await.is_err() {
        tracing::warn!(kb_id = %kb.id, doc_id = %doc.id, path = %source_path.display(), "rag reindex: source file missing");
        if let Err(e) = rt
            .db
            .kdoc_update_index_status(
                &doc.id,
                IndexKind::Vector,
                "failed",
                0,
                Some("original document missing; delete and re-upload it"),
            )
            .await
        {
            tracing::warn!(doc_id = %doc.id, error = %e, "rag reindex: rollback status failed");
        }
        return Ok(ReindexOutcome::MissingSource);
    }

    // 清旧索引：先向量后 SQLite（向量删除失败仅 warn，DB 是源）。
    if let Err(e) = rt
        .store
        .delete_by_doc(
            &kb.id,
            usize::try_from(kb.emb_dimension).unwrap_or(usize::MAX),
            &doc.id,
        )
        .await
    {
        tracing::warn!(kb_id = %kb.id, doc_id = %doc.id, error = %e, "rag reindex: store delete_by_doc failed");
    }
    if let Err(e) = rt.db.rag_delete_chunks_by_doc(&doc.id).await {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")));
    }

    // 状态已是 pending（CAS 时置位），直接 spawn_ingest 走完整摄入
    // （与 upload 同路径：processing → ready/failed + SSE 事件）。
    spawn_ingest(IngestOpts {
        db: rt.db.clone(),
        store: rt.store.clone(),
        cipher: rt.cipher.clone(),
        source: kb.clone(),
        doc_id: doc.id.clone(),
        source_path,
        file_type,
        tx: rt.tx.clone(),
        vector_sem: sem,
        pages_sem: None,
        page_extractor: None,
        // 本端点是「KB 页面」的重建入口，用户只请求了向量索引。容器若同时开了
        // pages，从这里连带跑一遍 LLM 抽取就是用户没点过的账单。批 3 合并统一
        // 路由后本入口消失，届时应改回 None（跑容器启用的全部索引）。
        only: Some(IndexKind::Vector),
    });
    Ok(ReindexOutcome::Spawned)
}

/// `GET /api/llm/kb/:id/docs` 列出指定知识库下的全部文档。
pub async fn list_docs(
    State(state): State<ApiState>,
    Path(kb_id): Path<String>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(_kb) = (match rt.db.ks_get(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    let docs = match rt.db.kdoc_list(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    // per-kind 状态化：为每个文档取 Vector 索引，组装与旧形状一致的视图
    let mut out = Vec::with_capacity(docs.len());
    for d in &docs {
        let idx = rt.db.kdoc_get_index(&d.id, IndexKind::Vector).await.unwrap_or(None);
        out.push(doc_json(d, idx.as_ref()));
    }
    Json(serde_json::json!({ "documents": out })).into_response()
}

/// `GET /api/llm/kb/:id/docs/:doc_id` 获取单文档详情，校验归属后返回记录。
pub async fn get_doc(
    State(state): State<ApiState>,
    Path((kb_id, doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(doc) = (match rt.db.kdoc_get(&doc_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    };
    if doc.source_id != kb_id {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    }
    let idx = rt.db.kdoc_get_index(&doc.id, IndexKind::Vector).await.unwrap_or(None);
    Json(doc_json(&doc, idx.as_ref())).into_response()
}

 /// POST /api/llm/kb/:id/docs — multipart 上传文档（文本 ≤2MB、二进制 ≤20MB），
/// 建 doc(pending) 后异步摄入并立即返回 doc 记录。摄入进度经
/// `/api/llm/kb/events` SSE 推送。
#[allow(clippy::too_many_lines, reason = "顺序编排：multipart 校验→落盘→入库→异步摄入，拆分会打散共享状态")]
pub async fn upload_doc(
    State(state): State<ApiState>,
    Path(kb_id): Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(kb) = (match rt.db.ks_get(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };

    // 软关检查：delete_kb 先软关（enabled=0）再删数据。窗口期内到达的 upload
    // 在此被拒，避免「删库竞态」重建 shard 目录、留下指向已删 KB 的 doc 行。
    if kb.enabled == 0 {
        return (StatusCode::CONFLICT, "knowledge base is disabled").into_response();
    }

    // 读取文件字段：按 file_name 判定（首个带文件名且非空的字段即文件）。
    // 扩展名判定前置：不合法扩展名立即 400，不读 body（避免为大文件浪费 IO）。
    let mut filename: Option<String> = None;
    let mut file_type: Option<FileType> = None;
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => return (StatusCode::BAD_REQUEST, "invalid multipart body").into_response(),
        };
        if field.file_name().is_none() {
            continue;
        }
        if filename.is_some() {
            return (StatusCode::BAD_REQUEST, "multiple files in one request").into_response();
        }
        let name = field.file_name().unwrap_or_default().to_string();
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase);
        let Some(ft) = ext.as_deref().and_then(FileType::from_extension) else {
            let e = ext.as_deref().unwrap_or("");
            let msg = match e {
                "doc" | "xls" | "ppt" => {
                    format!("legacy Office format '.{e}' is not supported; save as .{e}x and retry")
                }
                "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" => {
                    "image files are not supported (OCR unavailable)".to_string()
                }
                _ => format!(
                    "unsupported file type '{e}'; allowed: .md, .txt, .pdf, .docx, .xlsx, .pptx"
                ),
            };
            return (StatusCode::BAD_REQUEST, msg).into_response();
        };
        // 流式读取 + 即时超限截断：累计超过该类型上限立即返回自定义 400，不做全文
        // 缓冲。与 DefaultBodyLimit 解耦——即使未来放宽/禁用请求体上限，内存也
        // 始终有界（至多 max + 单个 chunk）。axum 0.7 的 Field 无 reader()，
        // 用等价的 chunk() 流式接口。
        let max = ft.max_bytes();
        loop {
            let chunk = match field.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "failed to read file field").into_response()
                }
            };
            if bytes.len() + chunk.len() > max {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("file too large (max {max} bytes)"),
                )
                    .into_response();
            }
            bytes.extend_from_slice(&chunk);
        }
        filename = Some(name);
        file_type = Some(ft);
    }

    let Some(name) = filename else {
        return (StatusCode::BAD_REQUEST, "missing file field").into_response();
    };
    let Some(file_type) = file_type else {
        return (StatusCode::BAD_REQUEST, "missing file field").into_response();
    };

    // 探测 magic bytes（二进制类防货不对板）+ 文本类 UTF-8/空内容预检——md/txt
    // 保持"上传即拒"体验（probe 对文本类是 no-op）。零拷贝：from_utf8 校验通过后
    // into_bytes 原样取回字节，继续用于落盘/哈希。
    if let Err(e) = file_type.probe(&bytes) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    if matches!(file_type, FileType::Markdown | FileType::Text) {
        let Ok(text) = String::from_utf8(bytes) else {
            return (StatusCode::BAD_REQUEST, "file must be UTF-8 text").into_response();
        };
        if text.trim().is_empty() {
            return (StatusCode::BAD_REQUEST, "file is empty").into_response();
        }
        bytes = text.into_bytes();
    }

    let doc_id = uuid::Uuid::new_v4().to_string();
    let content_hash = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&bytes)));

    // 原文落盘（规格 §2.1）：reindex 依赖此文件。写盘失败则返回错误、不落库、
    // 不摄入，避免出现"有 doc 记录无原文"的不可 reindex 状态。落盘保留真实
    // 扩展名——二进制原文 reindex 时按 file_type 重新解析。
    let source_path = doc_source_path(&rt.store, &kb_id, &doc_id, file_type.as_str());
    if let Some(parent) = source_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to init document storage: {e}"),
            )
                .into_response();
        }
    }
    if let Err(e) = tokio::fs::write(&source_path, &bytes).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to persist original document: {e}"),
        )
            .into_response();
    }

    if let Err(e) = rt
        .db
        .kdoc_create(&doc_id, &kb_id, &name, file_type.as_str(), &content_hash)
        .await
    {
        // 清理已落盘的原文，避免孤儿文件
        let _ = tokio::fs::remove_file(&source_path).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    // 后台摄入：提取 → 分块 → embedding → 写向量 → 落库 → 发事件；
    // 调用方立即拿 doc(pending)。
    spawn_ingest(IngestOpts {
        db: rt.db.clone(),
        store: rt.store.clone(),
        cipher: rt.cipher.clone(),
        source: kb,
        doc_id: doc_id.clone(),
        source_path: source_path.clone(),
        file_type,
        tx: rt.tx.clone(),
        vector_sem: None,
        pages_sem: None,
        page_extractor: None,
        // 同 reindex：KB 上传入口只跑向量侧，理由见 `reindex_kb_doc`。
        only: Some(IndexKind::Vector),
    });

    match rt.db.kdoc_get(&doc_id).await {
        Ok(Some(doc)) => {
            let idx = rt.db.kdoc_get_index(&doc.id, IndexKind::Vector).await.unwrap_or(None);
            (
                StatusCode::CREATED,
                Json(doc_json(&doc, idx.as_ref())),
            )
                .into_response()
        }
        _ => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": doc_id,
                "kb_id": kb_id,
                "filename": name,
                "content_hash": content_hash,
                "status": "pending",
                "chunk_count": 0,
            })),
        )
            .into_response(),
    }
}

/// `DELETE /api/llm/kb/:id/docs/:doc_id` 删除文档，先清向量再删库并清理原文文件。
pub async fn delete_doc(
    State(state): State<ApiState>,
    Path((kb_id, doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(kb) = (match rt.db.ks_get(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    let Some(doc) = (match rt.db.kdoc_get(&doc_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    };
    if doc.source_id != kb_id {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    }

    // 先删向量，再删 DB 记录（FK 级联清分块）。向量删除失败不阻断：DB 是源，
    // 残留向量可后续 GC，shard 维度不匹配时 delete_by_doc 也会安全降级。
    if let Err(e) = rt
        .store
        .delete_by_doc(
            &kb_id,
            usize::try_from(kb.emb_dimension).unwrap_or(usize::MAX),
            &doc_id,
        )
        .await
    {
        tracing::warn!(kb_id = %kb_id, doc_id = %doc_id, error = %e, "rag: store delete_by_doc failed");
    }
    if let Err(e) = rt.db.kdoc_delete(&doc_id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    // 清理原文文件（reindex 的真相源）。失败仅 warn：DB 是源，残留文件无害。
    // 老数据 file_type 可能为 ""（迁移前落库），回退 "md" 保持旧路径兼容。
    let ext = if doc.file_type.is_empty() {
        "md"
    } else {
        &doc.file_type
    };
    let source_path = doc_source_path(&rt.store, &kb_id, &doc_id, ext);
    if source_path.exists() {
        if let Err(e) = tokio::fs::remove_file(&source_path).await {
            tracing::warn!(kb_id = %kb_id, doc_id = %doc_id, error = %e, "rag: remove doc source file failed");
        }
    }
    StatusCode::OK.into_response()
}

/// POST `/api/llm/kb/:id/docs/:doc_id/reindex` — 重建单文档索引（规格 §5.2/§7）。
///
/// 摄入时已把原文落盘（`<data_dir>/knowledge_docs/<kb_id>/<doc_id>.<ext>`，见 upload_doc），
/// 故可无损重建：清旧索引（向量 + SQLite 分块）→ 按 doc.file_type 重新提取走完整
/// 摄入。换分块参数后也通过本端点重索引。原文文件缺失（老数据/手动删除）→ 409，
/// 提示删除重传。
pub async fn reindex_doc(
    State(state): State<ApiState>,
    Path((kb_id, doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(kb) = (match rt.db.ks_get(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    let Some(doc) = (match rt.db.kdoc_get(&doc_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    };
    if doc.source_id != kb_id {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    }

    // 并发防护/原文校验/清旧索引/spawn 摄入的完整流程抽到 `reindex_kb_doc`
    // （全量重建路径复用，见 kb.rs `update_kb` 的 emb 变更分支）。
    match reindex_kb_doc(&rt, &kb, &doc, None).await {
        Ok(ReindexOutcome::Spawned) => {}
        Ok(ReindexOutcome::Skipped) => {
            return (
                StatusCode::CONFLICT,
                "document is being processed, retry later".to_string(),
            )
                .into_response();
        }
        Ok(ReindexOutcome::MissingSource) => {
            return (
                StatusCode::CONFLICT,
                "original document missing; delete and re-upload it",
            )
                .into_response();
        }
        Err(e) => return e.into_response(),
    }

    match rt.db.kdoc_get(&doc_id).await {
        Ok(Some(d)) => {
            let idx = rt.db.kdoc_get_index(&d.id, IndexKind::Vector).await.unwrap_or(None);
            (
                StatusCode::OK,
                Json(doc_json(&d, idx.as_ref())),
            )
                .into_response()
        }
        _ => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": doc_id,
                "kb_id": kb_id,
                "status": "pending",
                "chunk_count": 0,
            })),
        )
            .into_response(),
    }
}
