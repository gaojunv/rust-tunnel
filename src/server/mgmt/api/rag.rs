//! RAG 知识库管理 API（KB CRUD + docs + test-embedding + query + SSE 事件流）。
//!
//! 路由挂在 `/api/llm/kb`，除 SSE 端点外均受 JWT 保护（SSE 的 token 走 query 参数，
//! 参照 `/api/logs/stream`）。向量本体在 `qdrant-edge` shard，元数据与原文在 `SQLite`。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use tokio::sync::broadcast;

use crate::server::auth::validate_token;
use crate::server::db::rag::RagKnowledgeBaseRecord;
use crate::server::db::Database;
use crate::server::llm::crypto::{encrypt_field, LlmCipher};
use crate::server::llm::rag::embedder::Embedder;
use crate::server::llm::rag::ingest::{spawn_ingest, KbEvent};
use crate::server::llm::rag::retriever;
use crate::server::llm::rag::store::VectorStore;
use crate::server::llm::LlmState;
use sha2::Digest;

use super::{dto::SseQuery, ApiState};

/// 上传文档的最大字节数（2MB）。超大文件直接 `400`，避免内存膨胀与过长的分块处理。
const MAX_DOC_BYTES: usize = 2 * 1024 * 1024;

/// 单次上传允许的字段名：handler 不关心 name，按 `file_name` 判定文件字段。
const ACCEPTED_EXTENSIONS: [&str; 2] = ["md", "txt"];

/// 取当前 LLM 运行时状态（未初始化时为 `None` → 请求失败）。
async fn llm_state(state: &ApiState) -> Option<Arc<LlmState>> {
    state
        .server_state
        .proxy_state
        .llm_state
        .read()
        .await
        .as_ref()
        .cloned()
}

/// RAG handler 需要的运行时组件（从 `LlmState` 克隆，避免长持锁）。
struct RagRuntime {
    db: Database,
    store: VectorStore,
    cipher: Option<LlmCipher>,
    tx: broadcast::Sender<KbEvent>,
}

/// 组装 RAG 运行时；LLM 网关未初始化 / 数据库缺失时返回可直出的错误。
async fn rag_rt(state: &ApiState) -> Result<RagRuntime, (StatusCode, String)> {
    let llm = llm_state(state).await.ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM gateway not initialized".to_string(),
        )
    })?;
    let db = llm
        .db
        .clone()
        .ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, "no database".to_string()))?;
    Ok(RagRuntime {
        db,
        store: llm.rag_store.clone(),
        cipher: llm.cipher.clone(),
        tx: llm.rag_tx.clone(),
    })
}

/// KB 视图 JSON：`emb_api_key` 不回显（同 provider `api_key` 策略），额外带文档数。
fn kb_json(kb: &RagKnowledgeBaseRecord, doc_count: i64) -> serde_json::Value {
    serde_json::json!({
        "id": kb.id,
        "name": kb.name,
        "description": kb.description,
        "emb_base_url": kb.emb_base_url,
        "emb_api_key": "",
        "emb_model": kb.emb_model,
        "emb_dimension": kb.emb_dimension,
        "top_k": kb.top_k,
        "chunk_size": kb.chunk_size,
        "chunk_overlap": kb.chunk_overlap,
        "score_threshold": kb.score_threshold,
        "enabled": kb.enabled != 0,
        "doc_count": doc_count,
        "created_at": kb.created_at,
        "updated_at": kb.updated_at,
    })
}

// ── KB CRUD ──────────────────────────────────────────────────────

/// POST /api/llm/kb 请求体。`emb_dimension` 必填（前端经 `test-embedding` 探测）。
#[derive(Debug, serde::Deserialize)]
pub struct CreateKbRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub emb_base_url: String,
    #[serde(default)]
    pub emb_api_key: String,
    pub emb_model: String,
    pub emb_dimension: i64,
    #[serde(default)]
    pub top_k: Option<i64>,
    #[serde(default)]
    pub chunk_size: Option<i64>,
    #[serde(default)]
    pub chunk_overlap: Option<i64>,
    #[serde(default)]
    pub score_threshold: Option<f64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// 校验创建/更新 KB 的参数（分块参数一致性），返回 None 表示合法。
fn validate_kb_params(
    top_k: i64,
    chunk_size: i64,
    chunk_overlap: i64,
    score_threshold: f64,
) -> Option<String> {
    if top_k < 1 {
        return Some("top_k must be >= 1".to_string());
    }
    if chunk_size < 1 {
        return Some("chunk_size must be >= 1".to_string());
    }
    if chunk_overlap >= chunk_size {
        return Some("chunk_overlap must be < chunk_size".to_string());
    }
    if !(0.0..=1.0).contains(&score_threshold) {
        return Some("score_threshold must be in [0, 1]".to_string());
    }
    None
}

pub async fn list_kbs(State(state): State<ApiState>) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let records = match rt.db.rag_list_kbs().await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let mut kbs = Vec::with_capacity(records.len());
    for kb in records {
        let doc_count = rt.db.rag_count_kb_docs(&kb.id).await.unwrap_or(0);
        kbs.push(kb_json(&kb, doc_count));
    }
    Json(serde_json::json!({ "knowledge_bases": kbs })).into_response()
}

pub async fn create_kb(
    State(state): State<ApiState>,
    Json(body): Json<CreateKbRequest>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    if body.emb_base_url.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "emb_base_url is required").into_response();
    }
    if body.emb_model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "emb_model is required").into_response();
    }
    if body.emb_dimension < 1 {
        return (
            StatusCode::BAD_REQUEST,
            "emb_dimension must be >= 1 (probe it via /test-embedding)".to_string(),
        )
            .into_response();
    }
    let top_k = body.top_k.unwrap_or(5);
    let chunk_size = body.chunk_size.unwrap_or(512);
    let chunk_overlap = body.chunk_overlap.unwrap_or(64);
    let score_threshold = body.score_threshold.unwrap_or(0.3);
    if let Some(err) = validate_kb_params(top_k, chunk_size, chunk_overlap, score_threshold) {
        return (StatusCode::BAD_REQUEST, err).into_response();
    }
    let enabled = body.enabled.unwrap_or(true);

    // 敏感字段落库前加密（AES-256-GCM；未配置主密钥时明文兼容，同 llm.rs 模式）
    let cipher = llm_state(&state).await.and_then(|l| l.cipher.clone());
    let emb_api_key = encrypt_field(cipher.as_ref(), &body.emb_api_key);

    let id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = rt
        .db
        .rag_create_kb(
            &id,
            &body.name,
            &body.description,
            &body.emb_base_url,
            &emb_api_key,
            &body.emb_model,
            body.emb_dimension,
            top_k,
            chunk_size,
            chunk_overlap,
            score_threshold,
            enabled,
        )
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "status": "ok", "id": id })),
    )
        .into_response()
}

pub async fn get_kb(State(state): State<ApiState>, Path(id): Path<String>) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(kb) = (match rt.db.rag_get_kb(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    let doc_count = rt.db.rag_count_kb_docs(&id).await.unwrap_or(0);
    Json(kb_json(&kb, doc_count)).into_response()
}

/// PUT /api/llm/kb/:id 请求体。emb 配置建库后锁定不可改，此处只更新名称/描述/检索参数。
#[derive(Debug, serde::Deserialize)]
pub struct UpdateKbRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub top_k: Option<i64>,
    #[serde(default)]
    pub chunk_size: Option<i64>,
    #[serde(default)]
    pub chunk_overlap: Option<i64>,
    #[serde(default)]
    pub score_threshold: Option<f64>,
}

pub async fn update_kb(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateKbRequest>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(_existing) = (match rt.db.rag_get_kb(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    if body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    let top_k = body.top_k.unwrap_or(5);
    let chunk_size = body.chunk_size.unwrap_or(512);
    let chunk_overlap = body.chunk_overlap.unwrap_or(64);
    let score_threshold = body.score_threshold.unwrap_or(0.3);
    if let Some(err) = validate_kb_params(top_k, chunk_size, chunk_overlap, score_threshold) {
        return (StatusCode::BAD_REQUEST, err).into_response();
    }
    if let Err(e) = rt
        .db
        .rag_update_kb_params(
            &id,
            &body.name,
            &body.description,
            top_k,
            chunk_size,
            chunk_overlap,
            score_threshold,
        )
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

pub async fn patch_kb(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(_existing) = (match rt.db.rag_get_kb(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    if let Err(e) = rt.db.rag_toggle_kb(&id, enabled).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

pub async fn delete_kb(State(state): State<ApiState>, Path(id): Path<String>) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(_kb) = (match rt.db.rag_get_kb(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };

    // 先软关（enabled=0）：避免删除窗口内检索路径继续命中该库。
    let _ = rt.db.rag_toggle_kb(&id, false).await;

    // 并发约束：store.delete_kb 与在途 ingest/upsert 并发可能触发 EdgeShard Drop
    // flush panic（目录已删），见 store::VectorStore::delete_kb 文档。管理面操作低频，
    // 此处不额外加锁（YAGNI），依赖「软关 + 后台 ingest 短暂窗口」的务实取舍：
    // 窗口期内若在途 ingest 已发出分块写，DB 记录随级联删除一并清除，向量残留可忽略。
    if let Err(e) = rt.store.delete_kb(&id).await {
        tracing::warn!(kb_id = %id, error = %e, "rag: store delete_kb failed");
    }
    if let Err(e) = rt.db.rag_delete_kb(&id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    StatusCode::OK.into_response()
}

// ── Documents ────────────────────────────────────────────────────

pub async fn list_docs(
    State(state): State<ApiState>,
    Path(kb_id): Path<String>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(_kb) = (match rt.db.rag_get_kb(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    let docs = match rt.db.rag_list_documents(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    Json(serde_json::json!({ "documents": docs })).into_response()
}

pub async fn get_doc(
    State(state): State<ApiState>,
    Path((kb_id, doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(doc) = (match rt.db.rag_get_document(&doc_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    };
    if doc.kb_id != kb_id {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    }
    Json(serde_json::to_value(doc).unwrap_or_default()).into_response()
}

/// POST /api/llm/kb/:id/docs — multipart 上传 .md/.txt（≤2MB），建 doc(pending) 后
/// 异步摄入并立即返回 doc 记录。摄入进度经 `/api/llm/kb/events` SSE 推送。
pub async fn upload_doc(
    State(state): State<ApiState>,
    Path(kb_id): Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(kb) = (match rt.db.rag_get_kb(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };

    // 读取文件字段：按 file_name 判定（首个带文件名且非空的字段即文件）。
    let mut filename: Option<String> = None;
    let mut content: Option<Vec<u8>> = None;
    loop {
        let field = match multipart.next_field().await {
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
        let Ok(bytes) = field.bytes().await else {
            return (StatusCode::BAD_REQUEST, "failed to read file field").into_response();
        };
        if bytes.len() > MAX_DOC_BYTES {
            return (
                StatusCode::BAD_REQUEST,
                format!("file too large (max {MAX_DOC_BYTES} bytes)"),
            )
                .into_response();
        }
        filename = Some(name);
        content = Some(bytes.to_vec());
    }

    let Some(name) = filename else {
        return (StatusCode::BAD_REQUEST, "missing file field").into_response();
    };
    let bytes = content.unwrap_or_default();
    let ext = std::path::Path::new(&name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);
    if !matches!(ext.as_deref(), Some(e) if ACCEPTED_EXTENSIONS.contains(&e)) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported file type '{}'; allowed: .md, .txt",
                ext.as_deref().unwrap_or("")
            ),
        )
            .into_response();
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return (StatusCode::BAD_REQUEST, "file must be UTF-8 text").into_response();
    };
    if text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "file is empty").into_response();
    }

    let doc_id = uuid::Uuid::new_v4().to_string();
    let content_hash = format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(text.as_bytes()))
    );
    if let Err(e) = rt
        .db
        .rag_create_document(&doc_id, &kb_id, &name, &content_hash)
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    // 后台摄入：分块 → embedding → 写向量 → 落库 → 发事件；调用方立即拿 doc(pending)。
    spawn_ingest(
        rt.db.clone(),
        rt.store.clone(),
        rt.cipher.clone(),
        kb,
        doc_id.clone(),
        text,
        rt.tx.clone(),
    );

    match rt.db.rag_get_document(&doc_id).await {
        Ok(Some(doc)) => (
            StatusCode::CREATED,
            Json(serde_json::to_value(doc).unwrap_or_default()),
        )
            .into_response(),
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

pub async fn delete_doc(
    State(state): State<ApiState>,
    Path((kb_id, doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(kb) = (match rt.db.rag_get_kb(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    let Some(doc) = (match rt.db.rag_get_document(&doc_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    };
    if doc.kb_id != kb_id {
        return (StatusCode::NOT_FOUND, "document not found").into_response();
    }

    // 先删向量，再删 DB 记录（FK 级联清分块）。向量删除失败不阻断：DB 是源，
    // 残留向量可后续 GC，shard 维度不匹配时 delete_by_doc 也会安全降级。
    if let Err(e) = rt
        .store
        .delete_by_doc(&kb_id, kb.emb_dimension as usize, &doc_id)
        .await
    {
        tracing::warn!(kb_id = %kb_id, doc_id = %doc_id, error = %e, "rag: store delete_by_doc failed");
    }
    if let Err(e) = rt.db.rag_delete_document(&doc_id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    StatusCode::OK.into_response()
}

/// POST `/api/llm/kb/:id/docs/:doc_id/reindex` — 重新摄入文档。
///
/// 原始文本在摄入后不保留（只存分块），无法无损重建，故不支持就地重索引：
/// 返回 `400` 提示重新上传。该路由保留以对齐前端契约；如需换分块参数重索引，
/// 正确路径是删除后重新上传。
pub async fn reindex_doc(
    State(state): State<ApiState>,
    Path((_kb_id, _doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    // 此实现为占位：摄入流程不保留原始文本（见 upload_doc / spawn_ingest），
    // 从分块重建内容会因 overlap 失真。注册路由以对齐接口，行为是明确报错。
    let _ = rag_rt(&state).await;
    (
        StatusCode::BAD_REQUEST,
        "reindex not supported: original document text is not retained after ingestion; delete and re-upload instead",
    )
        .into_response()
}

// ── test-embedding / query / SSE ─────────────────────────────────

/// POST /api/llm/kb/test-embedding 请求体。
#[derive(Debug, serde::Deserialize)]
pub struct TestEmbeddingRequest {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub model: String,
}

/// 探测 embedding 服务：向 `POST {base_url}/embeddings` 发一条探针文本，返回维度与耗时。
/// 前端据此填写 KB 的 `emb_dimension`。
pub async fn test_embedding(
    State(_state): State<ApiState>,
    Json(body): Json<TestEmbeddingRequest>,
) -> impl IntoResponse {
    if body.base_url.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "base_url is required").into_response();
    }
    if body.model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "model is required").into_response();
    }
    let started = Instant::now();
    let embedder = Embedder::new(&body.base_url, &body.api_key, &body.model);
    match embedder.embed_one("dimension probe").await {
        Ok(v) => {
            let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
            Json(serde_json::json!({ "dimension": v.len(), "latency_ms": latency_ms }))
                .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("embedding failed: {e}")).into_response(),
    }
}

/// POST /api/llm/kb/:id/query 请求体。
#[derive(Debug, serde::Deserialize)]
pub struct QueryKbRequest {
    pub text: String,
}

/// 检索知识库：embedding 查询向量 → top-K → 阈值过滤，返回命中 chunk 及分数。
pub async fn query_kb(
    State(state): State<ApiState>,
    Path(kb_id): Path<String>,
    Json(body): Json<QueryKbRequest>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if body.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "text is required").into_response();
    }
    let Some(kb) = (match rt.db.rag_get_kb(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    let chunks = retriever::retrieve(&rt.db, &rt.store, rt.cipher.as_ref(), &kb, &body.text).await;
    let chunks: Vec<serde_json::Value> = chunks
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "heading_path": c.heading_path,
                "content": c.content,
                "score": c.score,
            })
        })
        .collect();
    Json(serde_json::json!({ "chunks": chunks })).into_response()
}

/// GET /api/llm/kb/events — SSE 事件流（文档摄入状态）。token 走 query 参数认证
/// （public 路由，参照 `/api/logs/stream`），keep-alive 30s。
///
/// # Panics
///
/// 仅当构造错误响应（Response builder + body）时 `unwrap()`，属不可达路径。
pub async fn sse_kb_events(
    State(state): State<ApiState>,
    Query(params): Query<SseQuery>,
) -> impl IntoResponse {
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");
        let is_valid =
            !token.is_empty() && validate_token(token, &state.auth_config.jwt_secret).is_ok();
        if !is_valid {
            return axum::response::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from("Unauthorized"))
                .unwrap();
        }
    }

    let Some(llm) = llm_state(&state).await else {
        return axum::response::Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("LLM gateway not initialized"))
            .unwrap();
    };
    let mut rx = llm.rag_tx.subscribe();
    let stream = async_stream::stream! {
        loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let json = serde_json::to_string(&ev).unwrap_or_default();
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("kb").data(json),
                    );
                }
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("sync").data(format!(r#"{{"lagged":{n}}}"#)),
                    );
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => {
                    // 超时 → ping 保活
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("ping").data(""),
                    );
                }
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode as HttpStatus};
    use axum::routing::{get, post};
    use axum::Router;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::server::auth::AuthConfig;
    use crate::server::control::ServerState;
    use crate::server::db::Database;

    /// 构造 ApiState：内存 DB + 指定 RAG 数据目录 + 固定测试主密钥（字段加密可用）。
    /// `rag_dir` 的存活期必须覆盖返回的 state（`VectorStore` 的 `EdgeShard` Drop 会 flush）。
    async fn test_api_state(rag_dir: &std::path::Path) -> ApiState {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let server_state = ServerState::with_db(db);
        server_state
            .proxy_state
            .init_llm_state(server_state.db().cloned(), Some([42u8; 32]), rag_dir)
            .await;
        ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        }
    }

    /// 覆盖本模块全部路由的测试 `Router`（免 JWT，`auth_config` 关闭）。
    /// 与生产 `run_api_server` 一致：SSE events 挂 public router，其余挂 protected，
    /// 再 merge —— 静态段 `events` 与参数段 `:id` 的共存由此得到验证。
    fn test_router(state: ApiState) -> Router {
        let public = Router::new().route("/api/llm/kb/events", get(super::sse_kb_events));
        let protected = Router::new()
            .route("/api/llm/kb", get(super::list_kbs).post(super::create_kb))
            .route(
                "/api/llm/kb/:id",
                get(super::get_kb)
                    .put(super::update_kb)
                    .patch(super::patch_kb)
                    .delete(super::delete_kb),
            )
            .route(
                "/api/llm/kb/:id/docs",
                get(super::list_docs).post(super::upload_doc),
            )
            .route(
                "/api/llm/kb/:id/docs/:doc_id",
                get(super::get_doc).delete(super::delete_doc),
            )
            .route(
                "/api/llm/kb/:id/docs/:doc_id/reindex",
                post(super::reindex_doc),
            )
            .route("/api/llm/kb/test-embedding", post(super::test_embedding))
            .route("/api/llm/kb/:id/query", post(super::query_kb));
        public.merge(protected).with_state(state)
    }

    /// oneshot 请求助手：返回 (status, json body)。
    async fn call(app: &Router, req: Request<Body>) -> (HttpStatus, Value) {
        let resp = app.clone().oneshot(req).await.expect("router responds");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .expect("read response body");
        let body = serde_json::from_slice(&bytes).unwrap_or(json!(null));
        (status, body)
    }

    fn json_request(method: Method, uri: String, body: &Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request")
    }

    /// 组装 multipart 请求体（单文件字段）。
    fn multipart_body(boundary: &str, filename: &str, content: &str) -> String {
        format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: text/plain\r\n\r\n\
             {content}\r\n\
             --{boundary}--\r\n"
        )
    }

    /// 起一个返回固定维度向量的本地 embedding server，返回 `base_url`。
    async fn mock_embedding_server(dim: usize) -> String {
        use axum::extract::Json as J;
        use serde_json::Value as V;
        let app = Router::new().route(
            "/embeddings",
            post(move |body: J<V>| async move {
                let n = body["input"].as_array().map_or(1, Vec::len);
                let data: Vec<_> = (0..n)
                    .map(|i| {
                        json!({
                            "index": i,
                            "embedding": vec![0.1f32; dim],
                            "object": "embedding"
                        })
                    })
                    .collect();
                J(json!({ "object": "list", "data": data }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });
        format!("http://{addr}")
    }

    /// 创建知识库，返回 `kb_id`。
    async fn create_kb(app: &Router, base: &str) -> String {
        let req = json_request(
            Method::POST,
            "/api/llm/kb".to_string(),
            &json!({
                "name": "测试知识库",
                "description": "集成测试用",
                "emb_base_url": base,
                "emb_api_key": "sk-test",
                "emb_model": "test-model",
                "emb_dimension": 8,
                "top_k": 5,
            }),
        );
        let (status, body) = call(app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "create kb: {body}");
        body["id"].as_str().expect("kb id").to_string()
    }

    #[tokio::test]
    async fn test_embedding_returns_dimension() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/kb/test-embedding".to_string(),
            &json!({ "base_url": base, "api_key": "sk-test", "model": "test-model" }),
        );
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::OK, "test-embedding: {body}");
        assert_eq!(body["dimension"].as_i64(), Some(8));
        // latency_ms 为 u64 毫秒，仅需断言字段存在且为数字
        assert!(body["latency_ms"].is_u64());
    }

    #[tokio::test]
    async fn test_embedding_fails_on_unreachable_service() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/kb/test-embedding".to_string(),
            &json!({ "base_url": "http://127.0.0.1:1", "api_key": "k", "model": "m" }),
        );
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_GATEWAY);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn kb_crud_flow_with_docs_and_query() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;

        // 订阅 SSE 端点同源事件通道（上传后应收到 processing → ready）
        let db = state.server_state.db().unwrap().clone();
        let tx = {
            let guard = state.server_state.proxy_state.llm_state.read().await;
            guard.as_ref().unwrap().rag_tx.clone()
        };
        let mut rx = tx.subscribe();
        let app = test_router(state);

        // POST /api/llm/kb → 201
        let kb_id = create_kb(&app, &base).await;
        assert!(!kb_id.is_empty());

        // emb_api_key 落库已加密（固定测试主密钥 → 密文前缀）
        let stored = db.rag_get_kb(&kb_id).await.unwrap().unwrap();
        assert!(
            stored.emb_api_key.starts_with("enc:v1:"),
            "emb_api_key should be encrypted, got: {}",
            stored.emb_api_key
        );

        // GET /api/llm/kb → 列表含该 KB
        let (status, body) = call(
            &app,
            json_request(Method::GET, "/api/llm/kb".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let kbs = body["knowledge_bases"].as_array().unwrap();
        assert!(kbs.iter().any(|k| k["id"] == json!(kb_id)));

        // GET /api/llm/kb/:id → 200，密钥不回显
        let (status, body) = call(
            &app,
            json_request(Method::GET, format!("/api/llm/kb/{kb_id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["name"], json!("测试知识库"));
        assert_eq!(body["emb_api_key"], json!(""), "api key must not be echoed");

        // PUT 更新参数（emb 配置不可改）
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                format!("/api/llm/kb/{kb_id}"),
                &json!({ "name": "改名", "description": "d2", "top_k": 8, "chunk_size": 256, "chunk_overlap": 32, "score_threshold": 0.5 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "update kb: {body}");
        let updated = db.rag_get_kb(&kb_id).await.unwrap().unwrap();
        assert_eq!(updated.top_k, 8);
        assert_eq!(
            updated.emb_base_url, stored.emb_base_url,
            "emb config locked"
        );

        // PATCH 启停
        let (status, _body) = call(
            &app,
            json_request(
                Method::PATCH,
                format!("/api/llm/kb/{kb_id}"),
                &json!({ "enabled": false }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(db.rag_get_kb(&kb_id).await.unwrap().unwrap().enabled, 0);
        let (status, _body) = call(
            &app,
            json_request(
                Method::PATCH,
                format!("/api/llm/kb/{kb_id}"),
                &json!({ "enabled": true }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);

        // POST /:id/docs（multipart .md）→ 201 doc pending
        let content =
            "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n\n## 配置\n\n编辑 config.toml。\n"
                .to_string();
        let boundary = "test-boundary-42";
        let upload_body = multipart_body(boundary, "guide.md", &content);
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/llm/kb/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(upload_body))
            .expect("build multipart request");
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "upload doc: {body}");
        // 立即返回的 doc 记录可能是 pending（摄入未开始）或 processing（后台任务很快）——
        // 摄入 flip 到 processing 的竞态是合法行为，两种状态都接受。
        let doc_status = body["status"].as_str().expect("doc status");
        assert!(
            doc_status == "pending" || doc_status == "processing",
            "doc status should be pending or processing, got {doc_status}"
        );
        assert_eq!(body["filename"], json!("guide.md"));
        assert!(body["content_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        let doc_id = body["id"].as_str().expect("doc id").to_string();

        // 等摄入事件与文档 ready
        let ev1 = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("first event timeout")
            .expect("channel closed");
        assert_eq!(ev1.status, "processing");
        assert_eq!(ev1.doc_id, doc_id);

        let mut doc_ready = false;
        for _ in 0..50 {
            let (status, body) = call(
                &app,
                json_request(
                    Method::GET,
                    format!("/api/llm/kb/{kb_id}/docs/{doc_id}"),
                    &json!(null),
                ),
            )
            .await;
            assert_eq!(status, HttpStatus::OK);
            if body["status"] == json!("ready") {
                doc_ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(doc_ready, "doc should become ready");
        let ev2 = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("ready event timeout")
            .expect("channel closed");
        assert_eq!(ev2.status, "ready");
        assert!(ev2.error.is_none());

        // POST /:id/query {text} → 命中 chunk
        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/llm/kb/{kb_id}/query"),
                &json!({ "text": "怎么安装?" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "query: {body}");
        let chunks = body["chunks"].as_array().unwrap();
        assert!(!chunks.is_empty(), "query should hit ingested chunks");
        assert!(chunks[0]["score"].as_f64().unwrap() >= 0.3);

        // GET /:id/docs → 列表含该文档
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/llm/kb/{kb_id}/docs"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["documents"].as_array().unwrap().len(), 1);

        // DELETE /:id/docs/:doc_id → 200，随后 404
        let (status, _body) = call(
            &app,
            json_request(
                Method::DELETE,
                format!("/api/llm/kb/{kb_id}/docs/{doc_id}"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (status, _body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/llm/kb/{kb_id}/docs/{doc_id}"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::NOT_FOUND);

        // DELETE /api/llm/kb/:id → 200，随后 404，store shard 目录删除
        let (status, _body) = call(
            &app,
            json_request(Method::DELETE, format!("/api/llm/kb/{kb_id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (status, _body) = call(
            &app,
            json_request(Method::GET, format!("/api/llm/kb/{kb_id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::NOT_FOUND);
        assert!(
            !dir.path().join("rag").join(&kb_id).exists(),
            "kb shard dir should be removed"
        );
        assert!(db.rag_get_kb(&kb_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upload_rejects_bad_extension_and_oversize() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        // .exe → 400
        let boundary = "b-bad";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/llm/kb/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "evil.exe", "x")))
            .expect("build request");
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // >2MB → 400
        let big = "x".repeat(2 * 1024 * 1024 + 1);
        let boundary = "b-big";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/llm/kb/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "big.md", &big)))
            .expect("build request");
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 无文件字段 → 400
        let boundary = "b-nofile";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/llm/kb/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhi\r\n--{boundary}--\r\n"
            )))
            .expect("build request");
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_kb_validates_input() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);

        // name 为空 → 400
        let (status, _body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/llm/kb".to_string(),
                &json!({ "name": "", "emb_base_url": "http://x", "emb_model": "m", "emb_dimension": 8 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // emb_dimension 缺失 → 422（serde 必填）
        let (status, _body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/llm/kb".to_string(),
                &json!({ "name": "n", "emb_base_url": "http://x", "emb_model": "m" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::UNPROCESSABLE_ENTITY);

        // chunk_overlap >= chunk_size → 400
        let (status, _body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/llm/kb".to_string(),
                &json!({ "name": "n", "emb_base_url": "http://x", "emb_model": "m", "emb_dimension": 8, "chunk_size": 128, "chunk_overlap": 200 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 缺 KB → 404（docs 上传）
        let boundary = "b-404";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/llm/kb/no-such-kb/docs")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "a.md", "hi")))
            .expect("build request");
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::NOT_FOUND);
    }
}
