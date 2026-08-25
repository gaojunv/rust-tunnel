//! Wiki 管理 API（批 1 骨架）：容器/文档/页面/搜索/图谱/SSE。
//! 仅 `rag` feature 编译，对齐 `agent/memory.rs` 与 `agent/skills.rs` 风格。

#![cfg(feature = "rag")]

use std::time::Duration;

use axum::{
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use sha2::Digest;
#[allow(unused_imports, reason = "测试模块经 cfg(test) 引用 Arc，单测编译期需要，常规构建未直接使用")]
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::agent::memory::scope_coords;
use crate::agent::wiki::WikiState;
use crate::auth::validate_token;
use crate::db::agent::normalize_db_datetime;
use crate::db::knowledge::{IndexKind, KnowledgeDocIndexRecord, KnowledgeDocRecord, KnowledgeSourceRecord};
use crate::db::wiki::{normalize_wiki_ref, AgentWikiPageRecord, AgentWikiPageSummary};
use crate::llm::rag::extractor::FileType;
use crate::mgmt::api::dto::SseQuery;
use crate::mgmt::api::ApiState;

use super::mem_runtime;

/// Wiki SSE 保活与订阅超时：30s，与其他 SSE 对齐。
const WIKI_SSE_TIMEOUT: Duration = Duration::from_secs(30);
/// Wiki SSE KeepAlive 间隔：30s。
const WIKI_SSE_KEEPALIVE: Duration = Duration::from_secs(30);
/// Wiki 轮询等待：30s，与 SSE 超时同值。
const WIKI_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// 从 `ApiState` 取 Wiki 运行时；未注入（非 rag 构建 / 未初始化）→ 503。
/// 与 `mem_runtime` 同模式，对齐 `MemoryState` 的挂载形态。
pub(crate) fn wiki_runtime(state: &ApiState) -> Result<WikiState, (StatusCode, String)> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "agent workbench not initialized".into(),
        ));
    };
    let Some(wiki) = &agent.wiki else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "wiki runtime not initialized".into(),
        ));
    };
    Ok(wiki.clone())
}

// ── 辅助 ─────────────────────────────────────────────────────────

const VALID_SCOPES: [&str; 3] = ["global", "client", "workspace"];

fn wiki_json(w: &KnowledgeSourceRecord) -> serde_json::Value {
    serde_json::json!({
        "id": w.id,
        "name": w.name,
        "summary": w.summary,
        "status": w.status,
        "version": w.version,
        "page_count": w.page_count,
        "scope_type": w.scope_type,
        "client_id": w.client_id,
        "workspace_id": w.workspace_id,
        "created_at": normalize_db_datetime(&w.created_at),
        "updated_at": normalize_db_datetime(&w.updated_at),
    })
}

fn page_summary_json(p: &AgentWikiPageSummary) -> serde_json::Value {
    serde_json::json!({
        "id": p.id,
        "wiki_id": p.wiki_id,
        "ref": p.page_ref,
        "title": p.title,
        "summary": p.summary,
        "locked": p.locked != 0,
        "source_doc_id": p.source_doc_id,
        "use_count": p.use_count,
        "last_used_at": p.last_used_at.as_ref().map(|t| normalize_db_datetime(t)),
        "created_at": normalize_db_datetime(&p.created_at),
        "updated_at": normalize_db_datetime(&p.updated_at),
    })
}

fn page_json(p: &AgentWikiPageRecord) -> serde_json::Value {
    serde_json::json!({
        "id": p.id,
        "wiki_id": p.wiki_id,
        "ref": p.page_ref,
        "title": p.title,
        "summary": p.summary,
        "content": p.content,
        "locked": p.locked != 0,
        "source_doc_id": p.source_doc_id,
        "use_count": p.use_count,
        "last_used_at": p.last_used_at.as_ref().map(|t| normalize_db_datetime(t)),
        "created_at": normalize_db_datetime(&p.created_at),
        "updated_at": normalize_db_datetime(&p.updated_at),
    })
}

fn doc_json(d: &KnowledgeDocRecord, idx: Option<&KnowledgeDocIndexRecord>) -> serde_json::Value {
    serde_json::json!({
        "id": d.id,
        "wiki_id": d.source_id,
        "filename": d.filename,
        "file_type": d.file_type,
        "content_hash": d.content_hash,
        "status": idx.map(|i| i.status.as_str()).unwrap_or("pending"),
        "error": idx.and_then(|i| i.error.clone()),
        "created_at": normalize_db_datetime(&d.created_at),
        "updated_at": normalize_db_datetime(&d.updated_at),
    })
}

fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

fn validate_wiki_name(name: &str) -> Result<(), (StatusCode, String)> {
    let t = name.trim();
    if t.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required".into()));
    }
    if t.chars().count() > 64 {
        return Err((StatusCode::BAD_REQUEST, "name too long (max 64)".into()));
    }
    Ok(())
}

// ── Query DTO ────────────────────────────────────────────────────

/// 列出 Wiki 容器的查询参数，对应 `GET /api/agent/wiki`。
#[derive(Debug, Deserialize, Default)]
pub struct ListWikisParams {
    /// 作用域过滤：`global`/`client`/`workspace`，为空时不过滤。
    #[serde(default)]
    pub scope: Option<String>,
    /// 关联客户端 id，为空时不过滤。
    #[serde(default)]
    pub client_id: Option<String>,
    /// 关联工作区 id，为空时不过滤。
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// 关键词过滤，匹配名称或简介。
    #[serde(default)]
    pub q: Option<String>,
    /// 状态过滤，为空时返回全部状态。
    #[serde(default)]
    pub status: Option<String>,
    /// 返回条数，缺省 50，handler 层 clamp 到 1..=200。
    #[serde(default)]
    pub limit: Option<i64>,
    /// 偏移量，缺省 0，负数按 0 处理。
    #[serde(default)]
    pub offset: Option<i64>,
}

/// 创建 Wiki 容器的请求体，对应 `POST /api/agent/wiki`。
#[derive(Debug, Deserialize)]
pub struct CreateWikiRequest {
    /// Wiki 名称，必填，trim 后 1..=64 字符。
    pub name: String,
    /// 简介，缺省为空字符串，handler 层 trim 后落库。
    #[serde(default)]
    pub summary: String,
    /// 作用域类型：`global`/`client`/`workspace`，空串时按 `workspace` 处理。
    #[serde(default)]
    pub scope_type: String,
    /// 关联客户端 id，`scope_type=client` 时必填，其余作用域可为空。
    #[serde(default)]
    pub client_id: String,
    /// 关联工作区 id，`scope_type=workspace` 时必填，其余作用域可为空。
    #[serde(default)]
    pub workspace_id: String,
}

/// 更新 Wiki 容器的请求体，对应 `PATCH /api/agent/wiki/:id`，字段缺省表示保持原值。
#[derive(Debug, Deserialize, Default)]
pub struct UpdateWikiRequest {
    /// 新名称，为空时保持原值，传入时会校验长度 1..=64。
    #[serde(default)]
    pub name: Option<String>,
    /// 新简介，为空时保持原值。
    #[serde(default)]
    pub summary: Option<String>,
}

/// 列出 Wiki 页面的查询参数，对应 `GET /api/agent/wiki/:id/pages`。
#[derive(Debug, Deserialize, Default)]
pub struct ListPagesParams {
    /// 关键词过滤，匹配标题/摘要/内容。
    #[serde(default)]
    pub q: Option<String>,
    /// ref 前缀过滤，如 `deploy/` 仅返回该目录下页面。
    #[serde(default)]
    pub ref_prefix: Option<String>,
    /// 按锁定状态过滤，`Some(true)` 仅返回手工锁定页。
    #[serde(default)]
    pub locked: Option<bool>,
    /// 返回条数，缺省 50，handler 层 clamp 到 1..=200。
    #[serde(default)]
    pub limit: Option<i64>,
    /// 偏移量，缺省 0，负数按 0 处理。
    #[serde(default)]
    pub offset: Option<i64>,
}

/// 创建或更新 Wiki 页面的请求体，对应 `PUT /api/agent/wiki/:id/pages/*ref`。
#[derive(Debug, Deserialize)]
pub struct PutPageRequest {
    /// 页面引用，缺省时取路径参数中的 `*ref`。
    #[serde(rename = "ref")]
    pub page_ref: Option<String>,
    /// 标题，最大 64 字符，handler 层校验。
    #[serde(default)]
    pub title: String,
    /// 摘要，最大 200 字符，handler 层校验。
    #[serde(default)]
    pub summary: String,
    /// 正文内容，必填，trim 后不能为空。
    pub content: String,
}

/// Wiki 搜索的查询参数，对应 `GET /api/agent/wiki/:id/search` 与 `/api/agent/wiki/search`。
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    /// 搜索关键词，必填，传给 `wiki_search` 做 FTS/LIKE 查询。
    pub q: String,
    /// 返回条数，缺省 20，handler 层 clamp 到 1..=20。
    #[serde(default)]
    pub limit: Option<i64>,
}

// ── Handlers: 容器 ───────────────────────────────────────────────

/// 列出 Wiki 容器，支持按作用域与关键词过滤并分页。
pub async fn list_wikis(
    State(state): State<ApiState>,
    Query(params): Query<ListWikisParams>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let filter = crate::db::knowledge::KsListFilter {
        scope_type: params.scope.clone(),
        client_id: params.client_id.clone(),
        workspace_id: params.workspace_id.clone(),
        q: params.q.clone(),
        status: params.status.clone(),
        index_kind: Some(IndexKind::Pages),
        enabled: None,
    };
    let rows = match mem.db.ks_list(&filter, limit, offset).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let total = match mem.db.ks_count(&filter).await {
        Ok(n) => n,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let wikis: Vec<_> = rows.iter().map(wiki_json).collect();
    Json(serde_json::json!({ "wikis": wikis, "total": total })).into_response()
}

/// 创建 Wiki 容器，校验名称与 `scope_type`，同作用域重名时返回 409。
pub async fn create_wiki(
    State(state): State<ApiState>,
    Json(body): Json<CreateWikiRequest>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = validate_wiki_name(&body.name) {
        return e.into_response();
    }
    let scope = if body.scope_type.trim().is_empty() {
        "workspace"
    } else {
        body.scope_type.trim()
    };
    if !VALID_SCOPES.contains(&scope) {
        return (
            StatusCode::BAD_REQUEST,
            "scope_type must be global|client|workspace",
        )
            .into_response();
    }
    let (scope_type, client_id, workspace_id) =
        scope_coords(scope, &body.client_id, &body.workspace_id);
    let id = new_id();
    match mem
        .db
        .ks_create(&crate::db::knowledge::KsCreateOpts {
            id: id.clone(),
            name: body.name.trim().to_string(),
            summary: body.summary.trim().to_string(),
            index_vector: false,
            index_pages: true,
            scope_type: scope_type.clone(),
            client_id: client_id.clone(),
            workspace_id: workspace_id.clone(),
            emb_base_url: String::new(),
            emb_api_key: String::new(),
            emb_model: String::new(),
            emb_dimension: 0,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
    {
        Ok(()) => match mem.db.ks_get(&id).await {
            Ok(Some(w)) => (StatusCode::CREATED, Json(wiki_json(&w))).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(e) if e.to_string().contains("UNIQUE") => (
            StatusCode::CONFLICT,
            "wiki name already exists in this scope",
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 按 id 查询单个 Wiki 容器，不存在时返回 404。
pub async fn get_wiki(State(state): State<ApiState>, Path(id): Path<String>) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    match mem.db.ks_get(&id).await {
        Ok(Some(w)) => Json(wiki_json(&w)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 更新 Wiki 容器的名称与简介，名称重名时返回 409。
pub async fn update_wiki(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateWikiRequest>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let existing = match mem.db.ks_get(&id).await {
        Ok(Some(w)) => w,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let name = body.name.as_deref().unwrap_or(&existing.name);
    let summary = body.summary.as_deref().unwrap_or(&existing.summary);
    if let Err(e) = validate_wiki_name(name) {
        return e.into_response();
    }
    match mem
        .db
        .ks_update(
            &id,
            &crate::db::knowledge::KsUpdateOpts {
                name: Some(name.trim().to_string()),
                summary: Some(summary.trim().to_string()),
                ..Default::default()
            },
        )
        .await
    {
        Ok(()) => match mem.db.ks_get(&id).await {
            Ok(Some(w)) => Json(wiki_json(&w)).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(e) if e.to_string().contains("UNIQUE") => (
            StatusCode::CONFLICT,
            "wiki name already exists in this scope",
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 删除 Wiki 容器及对应的落盘文档目录（目录清理为 best-effort）。
pub async fn delete_wiki(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let wiki_rt = match wiki_runtime(&state) {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    match mem.db.ks_get(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }
    // 先落库删，再清落盘目录（best-effort：DB 是源，残留文件无害）。
    if let Err(e) = mem.db.ks_delete(&id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    let data_dir = wiki_rt.llm.rag_store.data_dir().to_path_buf();
    let dir = data_dir.join("wiki_docs").join(&id);
    if dir.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
            tracing::warn!(wiki_id = %id, error = %e, "wiki delete: remove wiki_docs dir failed");
        }
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

// ── Handlers: 文档 ───────────────────────────────────────────────

/// 列出指定 Wiki 下的全部文档。
pub async fn list_docs(
    State(state): State<ApiState>,
    Path(wiki_id): Path<String>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if mem.db.ks_get(&wiki_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match mem.db.kdoc_list(&wiki_id).await {
        Ok(docs) => {
            let mut v = Vec::with_capacity(docs.len());
            for d in &docs {
                let idx = mem.db.kdoc_get_index(&d.id, IndexKind::Pages).await.unwrap_or(None);
                v.push(doc_json(d, idx.as_ref()));
            }
            Json(serde_json::json!({ "documents": v })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 删除指定 Wiki 下的单篇文档，跨容器 id 不匹配时按 404 处理。
pub async fn delete_doc(
    State(state): State<ApiState>,
    Path((wiki_id, doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let Some(doc) = mem.db.kdoc_get(&doc_id).await.unwrap_or(None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if doc.source_id != wiki_id {
        return StatusCode::NOT_FOUND.into_response();
    }
    match mem.db.kdoc_delete(&doc_id).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 重新触发单篇文档的摄入流程，处理中或文件缺失时返回 409。
pub async fn reindex_doc(
    State(state): State<ApiState>,
    Path((wiki_id, doc_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let wiki_rt = match wiki_runtime(&state) {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let Some(doc) = mem.db.kdoc_get(&doc_id).await.unwrap_or(None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if doc.source_id != wiki_id {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !mem
        .db
        .kdoc_mark_pending_if_idle(&doc_id, IndexKind::Pages)
        .await
        .unwrap_or(false)
    {
        return (StatusCode::CONFLICT, "document is being processed").into_response();
    }
    let Some(ft) = FileType::from_extension(&doc.file_type) else {
        let _ = mem
            .db
            .kdoc_update_index_status(&doc_id, IndexKind::Pages, "failed", 0, Some("unsupported file type"))
            .await;
        return (
            StatusCode::CONFLICT,
            "unsupported file type; delete and re-upload",
        )
            .into_response();
    };
    let data_dir = wiki_rt.llm.rag_store.data_dir().to_path_buf();
    let source_path = wiki_doc_source_path(&data_dir, &wiki_id, &doc_id, ft.as_str());
    if tokio::fs::metadata(&source_path).await.is_err() {
        let _ = mem
            .db
            .kdoc_update_index_status(
                &doc_id,
                IndexKind::Pages,
                "failed",
                0,
                Some("original document missing; delete and re-upload"),
            )
            .await;
        return (
            StatusCode::CONFLICT,
            "original document missing; delete and re-upload",
        )
            .into_response();
    }
    // 清该 doc 旧非 locked 页（FTS/边同事务，见 wiki_clear_pages_by_doc）。
    let _ = mem.db.wiki_clear_pages_by_doc(&wiki_id, &doc_id).await;
    crate::agent::wiki::ingest::spawn_wiki_ingest(
        wiki_rt.db.clone(),
        wiki_rt.llm.clone(),
        wiki_id.clone(),
        doc_id.clone(),
        source_path,
        ft,
        wiki_rt.events.clone(),
        Some(wiki_rt.ingest_sem.clone()),
    );
    Json(serde_json::json!({ "status": "pending", "id": doc_id })).into_response()
}

fn wiki_doc_source_path(
    data_dir: &std::path::Path,
    wiki_id: &str,
    doc_id: &str,
    ext: &str,
) -> std::path::PathBuf {
    data_dir
        .join("wiki_docs")
        .join(wiki_id)
        .join(format!("{doc_id}.{ext}"))
}

/// POST /api/agent/wiki/:id/docs — multipart 上传，落盘 pending（批 2 接摄入）。
#[allow(clippy::too_many_lines, reason = "multipart 解析、文件类型/大小/内容校验与落盘依次编排，拆分会割裂错误处理流程")]
pub async fn upload_doc(
    State(state): State<ApiState>,
    Path(wiki_id): Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if mem.db.ks_get(&wiki_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
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
            return (
                StatusCode::BAD_REQUEST,
                format!("unsupported file type '{e}'"),
            )
                .into_response();
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
    let Some(ft) = file_type else {
        return (StatusCode::BAD_REQUEST, "missing file field").into_response();
    };
    if let Err(e) = ft.probe(&bytes) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    if matches!(ft, FileType::Markdown | FileType::Text) {
        let Ok(text) = String::from_utf8(bytes) else {
            return (StatusCode::BAD_REQUEST, "file must be UTF-8 text").into_response();
        };
        if text.trim().is_empty() {
            return (StatusCode::BAD_REQUEST, "file is empty").into_response();
        }
        bytes = text.into_bytes();
    }

    let doc_id = new_id();
    let content_hash = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&bytes)));

    // 落盘：取 LlmState 的 rag_data_dir（VectorStore.data_dir() 为根，拼 wiki_docs）
    let data_dir = mem.llm.rag_store.data_dir().to_path_buf();
    let source_path = wiki_doc_source_path(&data_dir, &wiki_id, &doc_id, ft.as_str());
    if let Some(parent) = source_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to init storage: {e}"),
            )
                .into_response();
        }
    }
    if let Err(e) = tokio::fs::write(&source_path, &bytes).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to persist document: {e}"),
        )
            .into_response();
    }

    if let Err(e) = mem
        .db
        .kdoc_create(&doc_id, &wiki_id, &name, ft.as_str(), &content_hash)
        .await
    {
        let _ = tokio::fs::remove_file(&source_path).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    // 取 WikiState 运行时（与 memory 共用 data_dir/DB/LLM，事件走 wiki 通道）。
    let wiki_rt = match wiki_runtime(&state) {
        Ok(rt) => rt,
        Err((status, msg)) => {
            return (status, msg).into_response();
        }
    };
    crate::agent::wiki::ingest::spawn_wiki_ingest(
        wiki_rt.db.clone(),
        wiki_rt.llm.clone(),
        wiki_id.clone(),
        doc_id.clone(),
        source_path,
        ft,
        wiki_rt.events.clone(),
        Some(wiki_rt.ingest_sem.clone()),
    );

    match mem.db.kdoc_get(&doc_id).await {
        Ok(Some(doc)) => {
            let idx = mem.db.kdoc_get_index(&doc.id, IndexKind::Pages).await.unwrap_or(None);
            (StatusCode::CREATED, Json(doc_json(&doc, idx.as_ref()))).into_response()
        }
        _ => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "id": doc_id, "wiki_id": wiki_id, "status": "pending" })),
        )
            .into_response(),
    }
}

// ── Handlers: 页面 ───────────────────────────────────────────────

/// 列出指定 Wiki 下的页面摘要，支持关键词与前缀过滤。
pub async fn list_pages(
    State(state): State<ApiState>,
    Path(wiki_id): Path<String>,
    Query(params): Query<ListPagesParams>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if mem.db.ks_get(&wiki_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    match mem
        .db
        .wiki_list_pages(
            &wiki_id,
            params.q.as_deref(),
            params.ref_prefix.as_deref(),
            params.locked,
            limit,
            offset,
        )
        .await
    {
        Ok(rows) => {
            let pages: Vec<_> = rows.iter().map(page_summary_json).collect();
            Json(serde_json::json!({ "pages": pages, "total": pages.len() })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 按标准化后的 `ref` 查询单页内容，非法 ref 返回 400。
pub async fn get_page(
    State(state): State<ApiState>,
    Path((wiki_id, page_ref)): Path<(String, String)>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    // axum wildcard: page_ref 可能含 `/`，Path 已解码
    let Some(norm) = normalize_wiki_ref(&page_ref) else {
        return (StatusCode::BAD_REQUEST, "invalid ref").into_response();
    };
    match mem.db.wiki_get_page(&wiki_id, &norm).await {
        Ok(Some(p)) => Json(page_json(&p)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 创建或更新单页，写入时校验标题/摘要长度并标记为 `locked=1`。
pub async fn put_page(
    State(state): State<ApiState>,
    Path((wiki_id, page_ref)): Path<(String, String)>,
    Json(body): Json<PutPageRequest>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if mem.db.ks_get(&wiki_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // ref 来源：路径优先，其次 body.ref
    let raw_ref = body.page_ref.as_deref().unwrap_or(&page_ref);
    let Some(norm) = normalize_wiki_ref(raw_ref) else {
        return (StatusCode::BAD_REQUEST, "invalid ref").into_response();
    };
    if body.content.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "content is required").into_response();
    }
    if body.title.chars().count() > 64 {
        return (StatusCode::BAD_REQUEST, "title too long (max 64)").into_response();
    }
    if body.summary.chars().count() > 200 {
        return (StatusCode::BAD_REQUEST, "summary too long (max 200)").into_response();
    }
    // 手动页 locked=1（ingest 不覆盖）
    match mem
        .db
        .wiki_upsert_page(
            &wiki_id,
            &norm,
            body.title.trim(),
            body.summary.trim(),
            &body.content,
            true,
            None,
        )
        .await
    {
        Ok(id) => match mem.db.wiki_get_page_by_id(&id).await {
            Ok(Some(p)) => Json(page_json(&p)).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 按 `ref` 删除单页，不存在时返回 404。
pub async fn delete_page(
    State(state): State<ApiState>,
    Path((wiki_id, page_ref)): Path<(String, String)>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let Some(norm) = normalize_wiki_ref(&page_ref) else {
        return (StatusCode::BAD_REQUEST, "invalid ref").into_response();
    };
    match mem.db.wiki_delete_page(&wiki_id, &norm).await {
        Ok(true) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

// ── Handlers: graph / search ────────────────────────────────────

/// 查询指定 Wiki 的页面图谱（节点与边）。
pub async fn get_graph(
    State(state): State<ApiState>,
    Path(wiki_id): Path<String>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if mem.db.ks_get(&wiki_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match mem.db.wiki_graph(&wiki_id).await {
        Ok(g) => {
            let nodes: Vec<_> = g.nodes.iter().map(page_summary_json).collect();
            Json(serde_json::json!({ "nodes": nodes, "edges": g.edges })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 在单个 Wiki 内做全文/模糊搜索，`limit` 限制为 1..=20。
pub async fn search_wiki(
    State(state): State<ApiState>,
    Path(wiki_id): Path<String>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if mem.db.ks_get(&wiki_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 20);
    match mem.db.wiki_search(&[wiki_id], &params.q, limit).await {
        Ok(hits) => Json(serde_json::json!({ "hits": hits })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// 跨全部 Wiki 容器搜索（批 1 暂不过滤可见性）。
pub async fn search_all_wikis(
    State(state): State<ApiState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let limit = params.limit.unwrap_or(20).clamp(1, 20);
    // 批 1：跨容器搜全量（scope 过滤在批 3 接入可见性）
    match mem.db.wiki_search(&[], &params.q, limit).await {
        Ok(hits) => Json(serde_json::json!({ "hits": hits })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

// ── SSE ──────────────────────────────────────────────────────────

/// Wiki 事件的 SSE 订阅端点，未注入运行时时仅发送 `ping` 保活。
pub async fn sse_wiki_events(
    State(state): State<ApiState>,
    Query(params): Query<SseQuery>,
) -> impl IntoResponse {
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");
        let is_valid =
            !token.is_empty() && validate_token(token, &state.auth_config.jwt_secret).is_ok();
        if !is_valid {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }
    // wiki 运行时未注入（非 rag 构建 / 未初始化）时仍可订阅 SSE，仅发 ping 保活。
    let mut rx = wiki_runtime(&state).ok().map(|w| w.subscribe());
    let stream = async_stream::stream! {
        loop {
            let Some(rx) = rx.as_mut() else {
                // 无运行时：周期性 ping，事件永不到达。
                tokio::time::sleep(WIKI_POLL_INTERVAL).await;
                yield Ok::<_, std::convert::Infallible>(Event::default().event("ping").data(""));
                continue;
            };
            match tokio::time::timeout(WIKI_SSE_TIMEOUT, rx.recv()).await {
                Ok(Ok(ev)) => {
                    let json = serde_json::to_string(&ev).unwrap_or_default();
                    yield Ok::<_, std::convert::Infallible>(Event::default().event("wiki").data(json));
                }
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("sync").data(format!(r#"{{"lagged":{n}}}"#)),
                    );
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => {
                    yield Ok::<_, std::convert::Infallible>(Event::default().event("ping").data(""));
                }
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(WIKI_SSE_KEEPALIVE))
        .into_response()
}

// ── 路由 ─────────────────────────────────────────────────────────

/// 构建 Wiki 受保护路由（需 JWT），挂载容器/文档/页面/图谱/搜索。
pub fn protected_router() -> Router<ApiState> {
    Router::new()
        .route("/api/agent/wiki", get(list_wikis).post(create_wiki))
        .route(
            "/api/agent/wiki/:id",
            get(get_wiki).patch(update_wiki).delete(delete_wiki),
        )
        .route("/api/agent/wiki/:id/docs", get(list_docs).post(upload_doc))
        .route("/api/agent/wiki/:id/docs/:docId", delete(delete_doc))
        .route("/api/agent/wiki/:id/docs/:docId/reindex", post(reindex_doc))
        .route("/api/agent/wiki/:id/pages", get(list_pages))
        .route(
            "/api/agent/wiki/:id/pages/*ref",
            get(get_page).put(put_page).delete(delete_page),
        )
        .route("/api/agent/wiki/:id/graph", get(get_graph))
        .route("/api/agent/wiki/:id/search", get(search_wiki))
        .route("/api/agent/wiki/search", get(search_all_wikis))
}

/// 构建 Wiki 公开路由，仅暴露 SSE 事件订阅。
pub fn public_router() -> Router<ApiState> {
    Router::new().route("/api/agent/wiki/events", get(sse_wiki_events))
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(all(test, feature = "rag"))]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode as HttpStatus};
    use axum::Router;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::auth::AuthConfig;
    use crate::control_plane::ServerState;
    use crate::db::Database;

    async fn test_api_state(rag_dir: &std::path::Path) -> ApiState {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let mut server_state = ServerState::with_db(db);
        server_state
            .init_llm_state(
                server_state.db().cloned(),
                Some([42u8; 32]),
                rag_dir,
                Arc::new(tokio::sync::RwLock::new(
                    crate::dynamic_config::DynamicConfig::default_for_llm(),
                )),
            )
            .await;
        let mem = {
            let llm = server_state
                .llm_state
                .read()
                .await
                .as_ref()
                .expect("llm state initialized")
                .clone(); // Arc<LlmState>
            crate::agent::memory::MemoryState::new(
                server_state.db().cloned().expect("db present"),
                llm.rag_store.clone(),
                llm.cipher.clone(),
                (*llm).clone(),
            )
        };
        let agent = server_state
            .agent_state
            .take()
            .expect("agent state")
            .with_memory(mem);
        server_state.agent_state = Some(agent);
        ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        }
    }

    fn test_router(state: ApiState) -> Router {
        super::protected_router()
            .merge(super::public_router())
            .merge(crate::mgmt::api::agent::memory::protected_router())
            .merge(crate::mgmt::api::agent::memory::public_router())
            .with_state(state)
    }

    async fn call(app: &Router, req: Request<Body>) -> (HttpStatus, Value) {
        let resp = app.clone().oneshot(req).await.expect("router responds");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .expect("read body");
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

    #[allow(clippy::too_many_lines, reason = "端到端覆盖 Wiki 容器/页面/搜索/配置回绕，步骤顺序编排较长，拆分会割裂断言上下文")]
    #[tokio::test]
    async fn wiki_container_crud_and_manual_page_and_search_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        // create wiki
        let (status, body) = call(
            &app,
            json_request(Method::POST, "/api/agent/wiki".to_string(), &json!({"name": "my-wiki","summary":"desc","scope_type":"workspace","client_id":"c1","workspace_id":"w1"})),
        )
        .await;
        assert_eq!(status, HttpStatus::CREATED, "create wiki: {body}");
        let wiki_id = body["id"].as_str().unwrap().to_string();

        // list
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                "/api/agent/wiki?scope=workspace&workspace_id=w1".to_string(),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["total"], json!(1));

        // get
        let (status, _) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/agent/wiki/{wiki_id}"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);

        // manual page PUT（locked=1）
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                format!("/api/agent/wiki/{wiki_id}/pages/deploy/prod"),
                &json!({"title":"部署","summary":"摘要","content":"内容 [[other/page]]"}),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "put page: {body}");
        assert_eq!(body["ref"], json!("deploy/prod"));

        // GET page
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/agent/wiki/{wiki_id}/pages/deploy/prod"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["content"], json!("内容 [[other/page]]"));

        // pages list（不含 content 视图亦可用，但本批返回 summary 视图）
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/agent/wiki/{wiki_id}/pages"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["pages"].as_array().unwrap().len(), 1);

        // graph
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/agent/wiki/{wiki_id}/graph"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["nodes"].as_array().unwrap().len(), 1);

        // search 端到端（2字 LIKE 回退）
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/agent/wiki/{wiki_id}/search?q=部署"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::OK, "search: {body}");
        assert!(body["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["ref"] == json!("deploy/prod")));

        // delete page
        let (status, _) = call(
            &app,
            json_request(
                Method::DELETE,
                format!("/api/agent/wiki/{wiki_id}/pages/deploy/prod"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);

        // settings roundtrip：wiki_enabled / wiki_list_max 不丢（INSERT OR REPLACE 坑回归）
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                "/api/agent/memory/settings".to_string(),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["wiki_enabled"], json!(true));
        assert_eq!(body["wiki_list_max"], json!(20));

        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({"wiki_enabled": false, "wiki_list_max": 8}),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "put settings: {body}");
        assert_eq!(body["wiki_enabled"], json!(false));
        assert_eq!(body["wiki_list_max"], json!(8));

        // 再改 top_k：wiki 字段不应被重置
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({"top_k": 16}),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(
            body["wiki_enabled"],
            json!(false),
            "wiki_enabled 不应被重置"
        );
        assert_eq!(body["wiki_list_max"], json!(8), "wiki_list_max 不应被重置");
    }

    #[tokio::test]
    async fn wiki_sse_events_stream_headers() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/agent/wiki/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
    }
}
