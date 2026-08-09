//! RAG 知识库管理 API（KB CRUD + docs + test-embedding + query + SSE 事件流）。
//!
//! 路由挂在 `/api/llm/kb`，除 SSE 端点外均受 JWT 保护（SSE 的 token 走 query 参数，
//! 参照 `/api/logs/stream`）。向量本体在 `qdrant-edge` shard，元数据与原文在 `SQLite`。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use tokio::sync::broadcast;

use crate::server::auth::validate_token;
use crate::server::db::rag::RagKnowledgeBaseRecord;
use crate::server::db::Database;
use crate::server::llm::crypto::{encrypt_field, LlmCipher};
use crate::server::llm::rag::embedder::Embedder;
use crate::server::llm::rag::extractor::FileType;
use crate::server::llm::rag::ingest::{spawn_ingest, KbEvent};
use crate::server::llm::rag::retriever;
use crate::server::llm::rag::store::VectorStore;
use crate::server::llm::LlmState;
use sha2::Digest;

use super::{dto::SseQuery, ApiState};

/// multipart 请求体总长上限：取最大单文件上限（二进制 20MB）+ 开销。
/// 字面量与 FileType::max_bytes 的二进制值保持一致（const 上下文无法调用
/// 非 const fn，故写字面量并注明对应关系）。
pub(crate) const MULTIPART_BODY_LIMIT: usize = 20 * 1024 * 1024 + 64 * 1024;

/// RAG 公开路由（SSE 事件流，`?token=` 认证）。仅 `rag` feature 启用时存在。
/// 返回 `Router<ApiState>`（handler 均依赖 `ApiState`，与生产路由链类型一致）。
pub fn public_router() -> Router<ApiState> {
    axum::Router::new().route("/api/llm/kb/events", get(sse_kb_events))
}

/// RAG 受保护路由（需 JWT）。仅 `rag` feature 启用时存在。
/// 返回 `Router<ApiState>`（handler 均依赖 `ApiState`，与生产路由链类型一致）。
pub fn protected_router() -> Router<ApiState> {
    axum::Router::new()
        .route("/api/llm/kb", get(list_kbs).post(create_kb))
        .route(
            "/api/llm/kb/:id",
            get(get_kb).put(update_kb).patch(patch_kb).delete(delete_kb),
        )
        .route(
            "/api/llm/kb/:id/docs",
            get(list_docs).post(upload_doc),
        )
        .layer(DefaultBodyLimit::max(MULTIPART_BODY_LIMIT))
        .route(
            "/api/llm/kb/:id/docs/:doc_id",
            get(get_doc).delete(delete_doc),
        )
        .route(
            "/api/llm/kb/:id/docs/:doc_id/reindex",
            post(reindex_doc),
        )
        .route("/api/llm/kb/test-embedding", post(test_embedding))
        .route("/api/llm/kb/:id/query", post(query_kb))
}

/// 文档原文落盘路径：`<data_dir>/rag_docs/<kb_id>/<doc_id>.<ext>`（保留真实扩展名，
/// 二进制原文 reindex 时按 file_type 重新解析）。
fn doc_source_path(
    store: &VectorStore,
    kb_id: &str,
    doc_id: &str,
    ext: &str,
) -> std::path::PathBuf {
    store
        .data_dir()
        .join("rag_docs")
        .join(kb_id)
        .join(format!("{doc_id}.{ext}"))
}

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
    if !(1..=20).contains(&top_k) {
        return Some("top_k must be 1-20".to_string());
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
    // 类型校验前置：非布尔 enabled 直接 400，避免 `as_bool().unwrap_or(false)`
    // 把一次类型错误静默变成「禁用 KB」（对齐 api-key PATCH 的校验语义）。
    let enabled = match body.get("enabled") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => return (StatusCode::BAD_REQUEST, "enabled must be a boolean").into_response(),
        None => return (StatusCode::BAD_REQUEST, "enabled is required").into_response(),
    };
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
    // 清理该库全部原文文件（best-effort，失败仅 warn）
    let source_dir = rt.store.data_dir().join("rag_docs").join(&id);
    if source_dir.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(&source_dir).await {
            tracing::warn!(kb_id = %id, error = %e, "rag: remove kb doc source dir failed");
        }
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

/// POST /api/llm/kb/:id/docs — multipart 上传文档（文本 ≤2MB、二进制 ≤20MB），
/// 建 doc(pending) 后异步摄入并立即返回 doc 记录。摄入进度经
/// `/api/llm/kb/events` SSE 推送。
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
        .rag_create_document(&doc_id, &kb_id, &name, &content_hash, file_type.as_str())
        .await
    {
        // 清理已落盘的原文，避免孤儿文件
        let _ = tokio::fs::remove_file(&source_path).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    // 后台摄入：提取 → 分块 → embedding → 写向量 → 落库 → 发事件；
    // 调用方立即拿 doc(pending)。
    spawn_ingest(
        rt.db.clone(),
        rt.store.clone(),
        rt.cipher.clone(),
        kb,
        doc_id.clone(),
        source_path.clone(),
        file_type,
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
/// 摄入时已把原文落盘（`<data_dir>/rag_docs/<kb_id>/<doc_id>.<ext>`，见 upload_doc），
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

    // 并发防护：pending/processing 表示原始摄入或上一次 reindex 仍在途，
    // 此时再 reindex 会与在途任务同时写向量+分块 → 重复数据。
    // 用原子 CAS 抢占（check-then-act 会让两个并发请求双双通过守卫），
    // 只有一个请求能把状态从 ready/failed 置回 pending，另一个收到 409。
    match rt.db.rag_mark_document_pending_if_idle(&doc_id).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                "document is being processed, retry later".to_string(),
            )
                .into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
        }
    }

    // 定位原文（摄入时已按 file_type 落盘，二进制原文也按同类型重新解析）。
    // 老数据 file_type 可能为 ""（迁移前落库），回退 Markdown 保持旧路径兼容。
    let file_type = FileType::from_extension(&doc.file_type).unwrap_or(FileType::Markdown);
    let source_path = doc_source_path(&rt.store, &kb_id, &doc_id, file_type.as_str());
    // 存在性检查（读字节太贵，先 metadata 探测）：缺失 → 无法无损重建，提示删除重传。
    // CAS 已把状态置为 pending，此处失败需回滚，否则文档永远卡在 pending。
    if tokio::fs::metadata(&source_path).await.is_err() {
        tracing::warn!(kb_id = %kb_id, doc_id = %doc_id, path = %source_path.display(), "rag reindex: source file missing");
        if let Err(e) = rt
            .db
            .rag_update_document_status(&doc_id, "failed", 0, Some("original document missing; delete and re-upload it"))
            .await
        {
            tracing::warn!(doc_id = %doc_id, error = %e, "rag reindex: rollback status failed");
        }
        return (
            StatusCode::CONFLICT,
            "original document missing; delete and re-upload it",
        )
            .into_response();
    }

    // 清旧索引：先向量后 SQLite（向量删除失败仅 warn，DB 是源）。
    if let Err(e) = rt
        .store
        .delete_by_doc(&kb_id, kb.emb_dimension as usize, &doc_id)
        .await
    {
        tracing::warn!(kb_id = %kb_id, doc_id = %doc_id, error = %e, "rag reindex: store delete_by_doc failed");
    }
    if let Err(e) = rt.db.rag_delete_chunks_by_doc(&doc_id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    // 状态已是 pending（CAS 时置位），直接 spawn_ingest 走完整摄入
    // （与 upload 同路径：processing → ready/failed + SSE 事件）。
    spawn_ingest(
        rt.db.clone(),
        rt.store.clone(),
        rt.cipher.clone(),
        kb,
        doc_id.clone(),
        source_path,
        file_type,
        rt.tx.clone(),
    );

    match rt.db.rag_get_document(&doc_id).await {
        Ok(Some(d)) => (
            StatusCode::OK,
            Json(serde_json::to_value(d).unwrap_or_default()),
        )
            .into_response(),
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
    use axum::extract::DefaultBodyLimit;
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
            .init_llm_state(
                server_state.db().cloned(),
                Some([42u8; 32]),
                rag_dir,
                std::sync::Arc::new(tokio::sync::RwLock::new(
                    crate::server::dynamic_config::DynamicConfig::default_for_llm(),
                )),
            )
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
            .layer(DefaultBodyLimit::max(super::MULTIPART_BODY_LIMIT))
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

    /// oneshot 请求助手：返回 (status, 原始响应文本)。错误响应多为纯文本，
    /// 需要断言具体消息（如 "file too large"）时用这个。
    async fn call_raw(app: &Router, req: Request<Body>) -> (HttpStatus, String) {
        let resp = app.clone().oneshot(req).await.expect("router responds");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .expect("read response body");
        (status, String::from_utf8_lossy(&bytes).to_string())
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

    /// 字节版 multipart 请求体（非 UTF-8 上传测试用）。
    fn multipart_body_bytes(boundary: &str, filename: &str, content: &[u8]) -> Vec<u8> {
        let mut v = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: text/plain\r\n\r\n"
        )
        .into_bytes();
        v.extend_from_slice(content);
        v.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        v
    }

    /// 组装 multipart 上传请求（字节体，二进制 fixture 用）。
    fn multipart_upload_request(
        kb_id: &str,
        boundary: &str,
        filename: &str,
        content: &[u8],
    ) -> Request<Body> {
        let body = multipart_body_bytes(boundary, filename, content);
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/llm/kb/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .expect("build multipart request")
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

    /// 轮询 GET /docs/:id 直到 status=ready，返回 chunk_count（10s 上限）。
    async fn wait_doc_ready(app: &Router, kb_id: &str, doc_id: &str) -> i64 {
        for _ in 0..50 {
            let (status, body) = call(
                app,
                json_request(
                    Method::GET,
                    format!("/api/llm/kb/{kb_id}/docs/{doc_id}"),
                    &json!(null),
                ),
            )
            .await;
            assert_eq!(status, HttpStatus::OK);
            if body["status"] == json!("ready") {
                return body["chunk_count"].as_i64().unwrap_or(0);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("doc {doc_id} did not become ready");
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

        // §2.1 原文落盘：<data_dir>/rag_docs/<kb_id>/<doc_id>.md
        let source_path = dir
            .path()
            .join("rag_docs")
            .join(&kb_id)
            .join(format!("{doc_id}.md"));
        assert!(
            source_path.exists(),
            "original doc file should be persisted on upload"
        );

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
        assert!(
            !source_path.exists(),
            "doc source file should be removed on delete"
        );

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
        assert!(
            !dir.path().join("rag_docs").join(&kb_id).exists(),
            "kb doc source dir should be removed"
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

        // >2MB → 400，且是自写的 "file too large" 消息（流式超限截断可达，
        // 不依赖 DefaultBodyLimit 的通用错误）
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
        let (status, body_text) = call_raw(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("file too large"),
            "oversize should return custom message, got: {body_text}"
        );

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

    #[tokio::test]
    async fn upload_rejects_non_utf8_file() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        // 非 UTF-8 字节序列（扩展名合法 .md）→ 400 "file must be UTF-8 text"
        let boundary = "b-utf8";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/llm/kb/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body_bytes(
                boundary,
                "bad.md",
                &[0xffu8, 0xfe, 0xfd],
            )))
            .expect("build request");
        let (status, body_text) = call_raw(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("UTF-8"),
            "non-utf8 should return UTF-8 message, got: {body_text}"
        );
    }

    #[tokio::test]
    async fn upload_rejects_legacy_image_and_mismatched_binary() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        // 旧版 Office（.doc）→ 400，提示另存为 .docx
        let (status, body_text) = call_raw(
            &app,
            multipart_upload_request(&kb_id, "b-legacy", "legacy.doc", b"x"),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("legacy Office format '.doc' is not supported"),
            "got: {body_text}"
        );

        // 图片 → 400，提示 OCR 不可用
        let (status, body_text) = call_raw(
            &app,
            multipart_upload_request(&kb_id, "b-img", "photo.png", b"x"),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("image files are not supported"),
            "got: {body_text}"
        );

        // .pdf 扩展名但内容不是 PDF → 400（probe 拦截，而非 ingest 期才失败）
        let (status, body_text) = call_raw(
            &app,
            multipart_upload_request(&kb_id, "b-pdf", "fake.pdf", b"not a pdf at all"),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(body_text.contains("not a PDF file"), "got: {body_text}");

        // 无扩展名 → 400 通用消息
        let (status, body_text) = call_raw(
            &app,
            multipart_upload_request(&kb_id, "b-noext", "README", b"x"),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("unsupported file type"),
            "got: {body_text}"
        );
    }

    #[tokio::test]
    async fn reindex_doc_rebuilds_index() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        // 上传 → 原文落盘
        let content =
            "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n\n## 配置\n\n编辑 config.toml。\n"
                .to_string();
        let boundary = "b-reindex";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/llm/kb/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "guide.md", &content)))
            .expect("build request");
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "upload: {body}");
        let doc_id = body["id"].as_str().expect("doc id").to_string();
        let source_path = dir
            .path()
            .join("rag_docs")
            .join(&kb_id)
            .join(format!("{doc_id}.md"));
        assert!(
            source_path.exists(),
            "original doc file should be persisted"
        );
        assert_eq!(
            std::fs::read_to_string(&source_path).unwrap(),
            content,
            "persisted source should match upload"
        );

        // 首次摄入 → ready
        let first = wait_doc_ready(&app, &kb_id, &doc_id).await;
        assert!(first > 0);
        assert_eq!(db.rag_count_kb_chunks(&kb_id).await.unwrap(), first);

        // reindex → 立即返回 pending/processing
        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/llm/kb/{kb_id}/docs/{doc_id}/reindex"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "reindex: {body}");
        let st = body["status"].as_str().unwrap_or("");
        assert!(
            st == "pending" || st == "processing",
            "reindex immediate status should be pending/processing, got {st}"
        );

        // 重建完成 → ready，chunk 数一致；旧索引已清（无重复分块）
        let second = wait_doc_ready(&app, &kb_id, &doc_id).await;
        assert!(second > 0);
        assert_eq!(
            second, first,
            "reindex should rebuild the same number of chunks"
        );
        assert_eq!(
            db.rag_count_kb_chunks(&kb_id).await.unwrap(),
            second,
            "old chunks must be removed before re-ingest"
        );

        // 重建后可检索
        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/llm/kb/{kb_id}/query"),
                &json!({ "text": "怎么安装?" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "query after reindex: {body}");
        assert!(
            !body["chunks"].as_array().unwrap().is_empty(),
            "query should hit rebuilt chunks"
        );
    }

    #[tokio::test]
    async fn create_and_update_enforce_top_k_cap() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);

        // 边界值 top_k=20 → 允许
        let req = json_request(
            Method::POST,
            "/api/llm/kb".to_string(),
            &json!({
                "name": "边界库",
                "emb_base_url": base,
                "emb_model": "m",
                "emb_dimension": 8,
                "top_k": 20,
            }),
        );
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "top_k=20 应允许: {body}");
        let kb_id = body["id"].as_str().expect("kb id").to_string();

        // 超上限 top_k=21 → 400，消息为 "top_k must be 1-20"
        let req = json_request(
            Method::POST,
            "/api/llm/kb".to_string(),
            &json!({
                "name": "超限库",
                "emb_base_url": base,
                "emb_model": "m",
                "emb_dimension": 8,
                "top_k": 21,
            }),
        );
        let (status, body_text) = call_raw(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST, "top_k=21 应 400");
        assert!(
            body_text.contains("top_k must be 1-20"),
            "错误消息应提示 1-20, got: {body_text}"
        );

        // 更新路径同样受限于上限
        let (status, body_text) = call_raw(
            &app,
            json_request(
                Method::PUT,
                format!("/api/llm/kb/{kb_id}"),
                &json!({ "name": "改名", "top_k": 21, "chunk_size": 512, "chunk_overlap": 64 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST, "update top_k=21 应 400");
        assert!(body_text.contains("top_k must be 1-20"));
    }

    #[tokio::test]
    async fn patch_kb_rejects_non_boolean_enabled() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        // 非布尔 → 400，且不得静默禁用 KB
        let (status, _body) = call(
            &app,
            json_request(
                Method::PATCH,
                format!("/api/llm/kb/{kb_id}"),
                &json!({ "enabled": "yes" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert_eq!(db.rag_get_kb(&kb_id).await.unwrap().unwrap().enabled, 1);
    }

    #[tokio::test]
    async fn patch_kb_rejects_missing_enabled_and_preserves_state() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;
        assert_eq!(db.rag_get_kb(&kb_id).await.unwrap().unwrap().enabled, 1);

        // 空体 {} → 400（不再静默禁用）
        let (status, _body) = call(
            &app,
            json_request(Method::PATCH, format!("/api/llm/kb/{kb_id}"), &json!({})),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert_eq!(
            db.rag_get_kb(&kb_id).await.unwrap().unwrap().enabled,
            1,
            "空 PATCH 不得把 KB 静默禁用"
        );

        // 非布尔 → 400，状态不变
        let (status, _body) = call(
            &app,
            json_request(
                Method::PATCH,
                format!("/api/llm/kb/{kb_id}"),
                &json!({ "enabled": 1 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert_eq!(db.rag_get_kb(&kb_id).await.unwrap().unwrap().enabled, 1);
    }

    #[tokio::test]
    async fn upload_to_disabled_kb_rejected() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        // 软关 KB
        db.rag_toggle_kb(&kb_id, false).await.unwrap();

        // 上传 → 409 "knowledge base is disabled"（与 delete_kb 的软关配合）
        let boundary = "b-disabled";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/llm/kb/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "a.md", "hi")))
            .expect("build request");
        let (status, body_text) = call_raw(&app, req).await;
        assert_eq!(status, HttpStatus::CONFLICT);
        assert!(
            body_text.contains("knowledge base is disabled"),
            "禁用 KB 上传应提示 disabled, got: {body_text}"
        );
        // 未留下任何 doc 记录/原文文件
        assert!(db.rag_list_documents(&kb_id).await.unwrap().is_empty());
        assert!(!dir.path().join("rag_docs").join(&kb_id).exists());
    }

    #[tokio::test]
    async fn reindex_rejected_while_doc_processing() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        // 手工构造一个 status=processing 的 doc + 落盘原文（确定性，无时序竞态）
        let doc_id = uuid::Uuid::new_v4().to_string();
        db.rag_create_document(&doc_id, &kb_id, "busy.md", "sha256:x", "md")
            .await
            .unwrap();
        db.rag_update_document_status(&doc_id, "processing", 0, None)
            .await
            .unwrap();
        let source_path = dir
            .path()
            .join("rag_docs")
            .join(&kb_id)
            .join(format!("{doc_id}.md"));
        tokio::fs::create_dir_all(source_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&source_path, "# busy").await.unwrap();

        // 在途 reindex → 409 "document is being processed, retry later"
        let (status, body_text) = call_raw(
            &app,
            json_request(
                Method::POST,
                format!("/api/llm/kb/{kb_id}/docs/{doc_id}/reindex"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CONFLICT);
        assert!(
            body_text.contains("document is being processed, retry later"),
            "processing 中 reindex 应 409, got: {body_text}"
        );
        // pending 同样拒绝
        db.rag_update_document_status(&doc_id, "pending", 0, None)
            .await
            .unwrap();
        let (status, _body) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/llm/kb/{kb_id}/docs/{doc_id}/reindex"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CONFLICT);
    }
}
