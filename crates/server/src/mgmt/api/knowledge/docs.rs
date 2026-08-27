//! 文档 handlers：列表 / 详情 / multipart 上传 / 删除 / reindex（统一双索引）。

use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use crate::db::agent::normalize_db_datetime;
use crate::db::knowledge::{
    IndexKind, KnowledgeDocIndexRecord, KnowledgeDocRecord, KnowledgeSourceRecord,
};
use crate::llm::rag::extractor::FileType;
use crate::llm::rag::ingest::{spawn_ingest, IngestOpts};
use crate::llm::rag::store::VectorStore;
use crate::mgmt::api::ApiState;
use sha2::Digest;

use super::{knowledge_rt, KnowledgeRuntime};

/// 单侧索引状态视图。
///
/// 未启用该侧 → `null`；已启用但还没有索引行（典型场景：容器后来才打开这一侧，
/// 已有文档尚未重建）→ 合成 `idle`。这样前端契约无歧义：`null` 只表示未启用。
fn index_json(
    enabled: bool,
    idx: Option<&KnowledgeDocIndexRecord>,
    count_key: &str,
) -> serde_json::Value {
    if !enabled {
        return serde_json::Value::Null;
    }
    let mut obj = serde_json::Map::new();
    if let Some(i) = idx {
        obj.insert("status".into(), serde_json::json!(i.status));
        obj.insert(count_key.to_string(), serde_json::json!(i.item_count));
        obj.insert("error".into(), serde_json::json!(i.error));
    } else {
        obj.insert("status".into(), serde_json::json!("idle"));
        obj.insert(count_key.to_string(), serde_json::json!(0));
        obj.insert("error".into(), serde_json::Value::Null);
    }
    serde_json::Value::Object(obj)
}

/// 统一文档视图：`vector` / `pages` 各自独立，`null` 表示容器未启用该索引。
fn doc_json(
    src: &KnowledgeSourceRecord,
    doc: &KnowledgeDocRecord,
    vec_idx: Option<&KnowledgeDocIndexRecord>,
    pages_idx: Option<&KnowledgeDocIndexRecord>,
) -> serde_json::Value {
    let vector = index_json(src.index_vector != 0, vec_idx, "chunk_count");
    let pages = index_json(src.index_pages != 0, pages_idx, "page_count");
    serde_json::json!({
        "id": doc.id,
        "source_id": doc.source_id,
        "filename": doc.filename,
        "file_type": doc.file_type,
        "content_hash": doc.content_hash,
        "created_at": normalize_db_datetime(&doc.created_at),
        "updated_at": normalize_db_datetime(&doc.updated_at),
        "vector": vector,
        "pages": pages,
    })
}

/// 文档原文落盘路径。薄包装 `doc_store::doc_source_path`。
pub(crate) fn doc_source_path(
    store: &VectorStore,
    source_id: &str,
    doc_id: &str,
    ext: &str,
) -> std::path::PathBuf {
    crate::llm::rag::doc_store::doc_source_path(store.data_dir(), source_id, doc_id, ext)
}

pub(crate) enum ReindexOutcome {
    Spawned,
    Skipped,
    MissingSource,
}

/// 向量侧全量重建用的单文档重建：仅处理 `Vector` 索引（emb 变更触发）。
/// 与旧 `reindex_kb_doc` 同语义，供 `sources::update_source` 的重建循环复用。
pub(crate) async fn reindex_vector_doc(
    rt: &KnowledgeRuntime,
    kb: &KnowledgeSourceRecord,
    doc: &KnowledgeDocRecord,
    sem: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<ReindexOutcome, (StatusCode, String)> {
    match rt
        .db
        .kdoc_mark_pending_if_idle(&doc.id, IndexKind::Vector)
        .await
    {
        Ok(true) => {}
        Ok(false) => return Ok(ReindexOutcome::Skipped),
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}"))),
    }
    let file_type = FileType::from_extension(&doc.file_type).unwrap_or(FileType::Markdown);
    let source_path = doc_source_path(&rt.store, &kb.id, &doc.id, file_type.as_str());
    if tokio::fs::metadata(&source_path).await.is_err() {
        tracing::warn!(kb_id = %kb.id, doc_id = %doc.id, path = %source_path.display(), "knowledge reindex(vector): source missing");
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
            tracing::warn!(doc_id = %doc.id, error = %e, "knowledge reindex(vector): rollback failed");
        }
        return Ok(ReindexOutcome::MissingSource);
    }
    if let Err(e) = rt
        .store
        .delete_by_doc(
            &kb.id,
            usize::try_from(kb.emb_dimension).unwrap_or(usize::MAX),
            &doc.id,
        )
        .await
    {
        tracing::warn!(kb_id = %kb.id, doc_id = %doc.id, error = %e, "knowledge reindex(vector): store delete_by_doc failed");
    }
    if let Err(e) = rt.db.rag_delete_chunks_by_doc(&doc.id).await {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")));
    }
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
        only: Some(IndexKind::Vector),
    });
    Ok(ReindexOutcome::Spawned)
}

/// 供 `sources::update_source` 重建循环复用的别名（向量侧）。
pub(crate) async fn reindex_source_doc(
    rt: &KnowledgeRuntime,
    kb: &KnowledgeSourceRecord,
    doc: &KnowledgeDocRecord,
    sem: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<ReindexOutcome, (StatusCode, String)> {
    reindex_vector_doc(rt, kb, doc, sem).await
}

/// `GET /api/knowledge/:id/docs`：列出指定容器下的全部文档（只读，不写库不触发摄入）。
/// 按容器 `index_vector`/`index_pages` 开关分别查询索引状态并合成统一视图。
pub async fn list_docs(
    State(state): State<ApiState>,
    Path(source_id): Path<String>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(src) = (match rt.db.ks_get(&source_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    let docs = match rt.db.kdoc_list(&source_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let mut out = Vec::with_capacity(docs.len());
    for d in &docs {
        let v_idx = if src.index_vector != 0 {
            rt.db
                .kdoc_get_index(&d.id, IndexKind::Vector)
                .await
                .unwrap_or(None)
        } else {
            None
        };
        let p_idx = if src.index_pages != 0 {
            rt.db
                .kdoc_get_index(&d.id, IndexKind::Pages)
                .await
                .unwrap_or(None)
        } else {
            None
        };
        out.push(doc_json(&src, d, v_idx.as_ref(), p_idx.as_ref()));
    }
    Json(serde_json::json!({ "documents": out })).into_response()
}

/// `GET /api/knowledge/:id/docs/:doc_id`：查询单篇文档详情（只读，不写库不触发摄入）。
/// 校验文档归属容器，未启用的索引侧返回 `null`。
pub async fn get_doc(
    State(state): State<ApiState>,
    Path((source_id, doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(src) = (match rt.db.ks_get(&source_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    let Some(doc) = (match rt.db.kdoc_get(&doc_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    };
    if doc.source_id != source_id {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    }
    let v_idx = if src.index_vector != 0 {
        rt.db
            .kdoc_get_index(&doc.id, IndexKind::Vector)
            .await
            .unwrap_or(None)
    } else {
        None
    };
    let p_idx = if src.index_pages != 0 {
        rt.db
            .kdoc_get_index(&doc.id, IndexKind::Pages)
            .await
            .unwrap_or(None)
    } else {
        None
    };
    Json(doc_json(&src, &doc, v_idx.as_ref(), p_idx.as_ref())).into_response()
}

/// `POST /api/knowledge/:id/docs`：以 multipart 上传单篇文档，校验类型/大小与内容后落盘并写入 `knowledge_docs`。
/// 成功后为容器已启用的全部索引侧触发后台摄入任务；若写库失败会删除已落盘的原文文件。
#[expect(
    clippy::too_many_lines,
    reason = "multipart 解析、类型/大小校验、落盘、写库与按侧触发摄入是一条顺序流水线，各阶段共享 source_id/doc_id/落盘路径与失败清理分支，拆分会打散回滚责任"
)]
pub async fn upload_doc(
    State(state): State<ApiState>,
    Path(source_id): Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(source) = (match rt.db.ks_get(&source_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    if source.enabled == 0 {
        return (StatusCode::CONFLICT, "knowledge base is disabled").into_response();
    }

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
    let source_path = doc_source_path(&rt.store, &source_id, &doc_id, file_type.as_str());
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
        .kdoc_create(
            &doc_id,
            &source_id,
            &name,
            file_type.as_str(),
            &content_hash,
        )
        .await
    {
        let _ = tokio::fs::remove_file(&source_path).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    // 统一入口：上传一篇文档喂容器启用的全部索引，批 2 的 only=单侧只是旧入口并存期的保护。
    // 注入 pages 抽取器与限流池（复用同一池，语义是 LLM 总并发 2）。
    let extractor: Arc<dyn crate::llm::rag::ingest::PageExtractor> = Arc::new(
        crate::agent::wiki::ingest::LlmPageExtractor::new(rt.db.clone(), rt.llm.clone()),
    );
    spawn_ingest(IngestOpts {
        db: rt.db.clone(),
        store: rt.store.clone(),
        cipher: rt.cipher.clone(),
        source: source.clone(),
        doc_id: doc_id.clone(),
        source_path: source_path.clone(),
        file_type,
        tx: rt.tx.clone(),
        vector_sem: None,
        pages_sem: Some(rt.pages_sem.clone()),
        page_extractor: Some(extractor),
        only: None,
    });

    // 返回统一视图
    match rt.db.kdoc_get(&doc_id).await {
        Ok(Some(doc)) => {
            let v_idx = if source.index_vector != 0 {
                rt.db
                    .kdoc_get_index(&doc.id, IndexKind::Vector)
                    .await
                    .unwrap_or(None)
            } else {
                None
            };
            let p_idx = if source.index_pages != 0 {
                rt.db
                    .kdoc_get_index(&doc.id, IndexKind::Pages)
                    .await
                    .unwrap_or(None)
            } else {
                None
            };
            (
                StatusCode::CREATED,
                Json(doc_json(&source, &doc, v_idx.as_ref(), p_idx.as_ref())),
            )
                .into_response()
        }
        _ => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": doc_id,
                "source_id": source_id,
                "filename": name,
                "content_hash": content_hash,
                "vector": serde_json::Value::Null,
                "pages": serde_json::Value::Null,
            })),
        )
            .into_response(),
    }
}

/// `DELETE /api/knowledge/:id/docs/:doc_id`：删除单篇文档，先清向量侧向量与分片（`pages` 侧由外键级联清理），再删库记录并移除落盘原文。
/// 不触发后台摄入；若向量存储或文件删除失败仅告警，不影响本次删除成功。
pub async fn delete_doc(
    State(state): State<ApiState>,
    Path((source_id, doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(source) = (match rt.db.ks_get(&source_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    let Some(doc) = (match rt.db.kdoc_get(&doc_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    };
    if doc.source_id != source_id {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    }
    // 先删向量（pages 侧由 FK 级联或 ingest 内部清理，此处不单独清 pages）
    if source.index_vector != 0 {
        if let Err(e) = rt
            .store
            .delete_by_doc(
                &source_id,
                usize::try_from(source.emb_dimension).unwrap_or(usize::MAX),
                &doc_id,
            )
            .await
        {
            tracing::warn!(source_id = %source_id, doc_id = %doc_id, error = %e, "knowledge: store delete_by_doc failed");
        }
    }
    if let Err(e) = rt.db.kdoc_delete(&doc_id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    let ext = if doc.file_type.is_empty() {
        "md"
    } else {
        &doc.file_type
    };
    let source_path = doc_source_path(&rt.store, &source_id, &doc_id, ext);
    if source_path.exists() {
        if let Err(e) = tokio::fs::remove_file(&source_path).await {
            tracing::warn!(source_id = %source_id, doc_id = %doc_id, error = %e, "knowledge: remove doc source file failed");
        }
    }
    StatusCode::OK.into_response()
}

/// 可选：仅重建指定索引侧，缺省则重建容器启用的所有索引侧。
#[derive(Debug, Default, Deserialize)]
pub struct ReindexParams {
    #[serde(default)]
    pub kind: Option<String>,
}

/// `POST /api/knowledge/:id/docs/:doc_id/reindex`：重建单篇文档的索引，`?kind=vector|pages` 可限定单侧，缺省重建容器已启用的全部索引。
/// 对目标侧 CAS 抢占为 `pending`，向量侧先清旧向量与分片再触发后台摄入（`pages` 侧由摄入完成后自清）；正处理中则返回 409 冲突。
#[expect(
    clippy::too_many_lines,
    reason = "按 kind 分派的重建流程：CAS 抢占、向量侧清旧、失败回滚各侧索引状态共享同一组 succeeded_kinds 与错误分支，拆分会让状态回滚散落多处"
)]
pub async fn reindex_doc(
    State(state): State<ApiState>,
    Path((source_id, doc_id)): Path<(String, String)>,
    Query(params): Query<ReindexParams>,
) -> impl IntoResponse {
    let only = match params.kind.as_deref() {
        None | Some("") => None,
        Some("vector") => Some(IndexKind::Vector),
        Some("pages") => Some(IndexKind::Pages),
        Some(_) => {
            return (StatusCode::BAD_REQUEST, "kind must be vector or pages").into_response();
        }
    };
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(source) = (match rt.db.ks_get(&source_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    let Some(doc) = (match rt.db.kdoc_get(&doc_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    };
    if doc.source_id != source_id {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    }

    // 对容器启用的每个 kind 分别 CAS 抢占（`only` 指定时只抢那一侧）
    let mut succeeded_kinds = Vec::new();
    // 「既被 only 选中、又被容器启用」的侧数；为 0 说明这次请求无事可做
    let mut requested_count = 0;
    if source.index_vector != 0 && only != Some(IndexKind::Pages) {
        requested_count += 1;
        match rt
            .db
            .kdoc_mark_pending_if_idle(&doc_id, IndexKind::Vector)
            .await
        {
            Ok(true) => succeeded_kinds.push(IndexKind::Vector),
            Ok(false) => {}
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}"))
                    .into_response()
            }
        }
    }
    if source.index_pages != 0 && only != Some(IndexKind::Vector) {
        requested_count += 1;
        match rt
            .db
            .kdoc_mark_pending_if_idle(&doc_id, IndexKind::Pages)
            .await
        {
            Ok(true) => succeeded_kinds.push(IndexKind::Pages),
            Ok(false) => {}
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}"))
                    .into_response()
            }
        }
    }
    if requested_count == 0 {
        // only 指定了一侧但容器未启用该侧，与「两侧都没启用」同为参数错误
        return (
            StatusCode::BAD_REQUEST,
            "knowledge source has no index enabled",
        )
            .into_response();
    }
    if succeeded_kinds.is_empty() {
        return (
            StatusCode::CONFLICT,
            "document is being processed, retry later".to_string(),
        )
            .into_response();
    }

    // 校验文件类型（旧 Wiki 侧对不支持类型直接置 failed）
    let Some(file_type) = FileType::from_extension(&doc.file_type) else {
        for k in &succeeded_kinds {
            let _ = rt
                .db
                .kdoc_update_index_status(&doc_id, *k, "failed", 0, Some("unsupported file type"))
                .await;
        }
        return (
            StatusCode::CONFLICT,
            "unsupported file type; delete and re-upload",
        )
            .into_response();
    };
    let source_path = doc_source_path(&rt.store, &source_id, &doc_id, file_type.as_str());
    if tokio::fs::metadata(&source_path).await.is_err() {
        tracing::warn!(source_id = %source_id, doc_id = %doc_id, path = %source_path.display(), "knowledge reindex: source missing");
        for k in &succeeded_kinds {
            let _ = rt
                .db
                .kdoc_update_index_status(
                    &doc_id,
                    *k,
                    "failed",
                    0,
                    Some("original document missing; delete and re-upload it"),
                )
                .await;
        }
        return (
            StatusCode::CONFLICT,
            "original document missing; delete and re-upload it",
        )
            .into_response();
    }

    // 清旧索引：仅 vector 侧；pages 侧不动（ingest_pages 成功后自清）
    if succeeded_kinds.contains(&IndexKind::Vector) {
        if let Err(e) = rt
            .store
            .delete_by_doc(
                &source_id,
                usize::try_from(source.emb_dimension).unwrap_or(usize::MAX),
                &doc_id,
            )
            .await
        {
            tracing::warn!(source_id = %source_id, doc_id = %doc_id, error = %e, "knowledge reindex: store delete_by_doc failed");
        }
        if let Err(e) = rt.db.rag_delete_chunks_by_doc(&doc_id).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
        }
    }

    let extractor: Arc<dyn crate::llm::rag::ingest::PageExtractor> = Arc::new(
        crate::agent::wiki::ingest::LlmPageExtractor::new(rt.db.clone(), rt.llm.clone()),
    );
    spawn_ingest(IngestOpts {
        db: rt.db.clone(),
        store: rt.store.clone(),
        cipher: rt.cipher.clone(),
        source: source.clone(),
        doc_id: doc_id.clone(),
        source_path,
        file_type,
        tx: rt.tx.clone(),
        vector_sem: None,
        pages_sem: Some(rt.pages_sem.clone()),
        page_extractor: Some(extractor),
        only,
    });

    match rt.db.kdoc_get(&doc_id).await {
        Ok(Some(d)) => {
            let v_idx = if source.index_vector != 0 {
                rt.db
                    .kdoc_get_index(&d.id, IndexKind::Vector)
                    .await
                    .unwrap_or(None)
            } else {
                None
            };
            let p_idx = if source.index_pages != 0 {
                rt.db
                    .kdoc_get_index(&d.id, IndexKind::Pages)
                    .await
                    .unwrap_or(None)
            } else {
                None
            };
            (
                StatusCode::OK,
                Json(doc_json(&source, &d, v_idx.as_ref(), p_idx.as_ref())),
            )
                .into_response()
        }
        _ => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": doc_id,
                "source_id": source_id,
                "vector": serde_json::Value::Null,
                "pages": serde_json::Value::Null,
            })),
        )
            .into_response(),
    }
}
