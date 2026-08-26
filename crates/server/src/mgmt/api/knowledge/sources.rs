//! 容器 CRUD：合并旧 `kb` 与 `wiki` 的容器侧。

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use crate::agent::memory::scope_coords;
use crate::db::agent::normalize_db_datetime;
use crate::db::knowledge::{IndexKind, KnowledgeSourceRecord, KsCreateOpts, KsUpdateOpts};
use crate::llm::crypto::{decrypt_field, encrypt_field};
use crate::mgmt::api::ApiState;

use super::{knowledge_rt, llm_state};

// ── DTO ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSourceRequest {
    pub name: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub index_vector: Option<bool>,
    #[serde(default)]
    pub index_pages: Option<bool>,
    #[serde(default)]
    pub scope_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub emb_base_url: Option<String>,
    #[serde(default)]
    pub emb_api_key: Option<String>,
    #[serde(default)]
    pub emb_model: Option<String>,
    #[serde(default)]
    pub emb_dimension: Option<i64>,
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

#[derive(Debug, Deserialize)]
pub struct UpdateSourceRequest {
    pub name: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub index_vector: Option<bool>,
    #[serde(default)]
    pub index_pages: Option<bool>,
    #[serde(default)]
    pub emb_base_url: Option<String>,
    #[serde(default)]
    pub emb_api_key: Option<String>,
    #[serde(default)]
    pub emb_model: Option<String>,
    #[serde(default)]
    pub emb_dimension: Option<i64>,
    #[serde(default)]
    pub top_k: Option<i64>,
    #[serde(default)]
    pub chunk_size: Option<i64>,
    #[serde(default)]
    pub chunk_overlap: Option<i64>,
    #[serde(default)]
    pub score_threshold: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListSourcesParams {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub index_kind: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ── 辅助 ─────────────────────────────────────────────────────────────

const VALID_SCOPES: [&str; 3] = ["global", "client", "workspace"];

fn validate_source_name(name: &str) -> Result<(), (StatusCode, String)> {
    let t = name.trim();
    if t.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required".into()));
    }
    if t.chars().count() > 64 {
        return Err((StatusCode::BAD_REQUEST, "name too long (max 64)".into()));
    }
    Ok(())
}

fn resolve_summary(body_summary: Option<&String>, body_desc: Option<&String>) -> String {
    if let Some(s) = body_summary {
        if !s.trim().is_empty() || body_desc.is_none() {
            return s.trim().to_string();
        }
    }
    if let Some(d) = body_desc {
        return d.trim().to_string();
    }
    String::new()
}

/// 统一视图：返回全集字段，前端旧组件各自取所需字段，JSON 多字段无害。
/// `emb_api_key` 绝不输出，只回显 `has_api_key` 布尔（沿用旧 kb 行为）。
fn source_json(src: &KnowledgeSourceRecord, doc_count: i64) -> serde_json::Value {
    // has_api_key 需解密判空（与 memory settings 的 has_key 同逻辑），但此处
    // 不持有 cipher 时退化为密文非空即视为有 key（测试固定主密钥场景已加密）。
    let has_key = !src.emb_api_key.is_empty();
    serde_json::json!({
        "id": src.id,
        "name": src.name,
        "summary": src.summary,
        "description": src.summary,
        "index_vector": src.index_vector != 0,
        "index_pages": src.index_pages != 0,
        "scope_type": src.scope_type,
        "client_id": src.client_id,
        "workspace_id": src.workspace_id,
        "emb_base_url": src.emb_base_url,
        "emb_api_key": "",
        "has_api_key": has_key,
        "emb_model": src.emb_model,
        "emb_dimension": src.emb_dimension,
        "top_k": src.top_k,
        "chunk_size": src.chunk_size,
        "chunk_overlap": src.chunk_overlap,
        "score_threshold": src.score_threshold,
        "status": src.status,
        "version": src.version,
        "page_count": src.page_count,
        "enabled": src.enabled != 0,
        "doc_count": doc_count,
        "created_at": normalize_db_datetime(&src.created_at),
        "updated_at": normalize_db_datetime(&src.updated_at),
    })
}

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

/// 解析创建时的 embedding 配置（仅 index_vector=true 时调用）。
async fn resolve_kb_embedding(
    rt: &super::KnowledgeRuntime,
    body: &CreateSourceRequest,
) -> Result<(String, String, String, i64), (StatusCode, String)> {
    let explicit = body
        .emb_base_url
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
        || body
            .emb_model
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        || body.emb_dimension.is_some();
    if explicit {
        let base = body.emb_base_url.as_deref().ok_or((
            StatusCode::BAD_REQUEST,
            "emb_base_url is required when customizing embedding".to_string(),
        ))?;
        let model = body.emb_model.as_deref().ok_or((
            StatusCode::BAD_REQUEST,
            "emb_model is required when customizing embedding".to_string(),
        ))?;
        let dim = body.emb_dimension.ok_or((
            StatusCode::BAD_REQUEST,
            "emb_dimension must be >= 1 (probe it via /test-embedding)".to_string(),
        ))?;
        if base.trim().is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "emb_base_url is required".to_string(),
            ));
        }
        if model.trim().is_empty() {
            return Err((StatusCode::BAD_REQUEST, "emb_model is required".to_string()));
        }
        if dim < 1 {
            return Err((
                StatusCode::BAD_REQUEST,
                "emb_dimension must be >= 1 (probe it via /test-embedding)".to_string(),
            ));
        }
        let key = body.emb_api_key.clone().unwrap_or_default();
        return Ok((base.to_string(), key, model.to_string(), dim));
    }
    let s = rt
        .db
        .memory_get_settings()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")))?;
    if s.emb_base_url.trim().is_empty() || s.emb_model.trim().is_empty() || s.emb_dimension < 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            "embedding is not configured: set a global embedding config in the shared settings first"
                .to_string(),
        ));
    }
    let key = decrypt_field(rt.cipher.as_ref(), &s.emb_api_key).unwrap_or_default();
    Ok((s.emb_base_url, key, s.emb_model, s.emb_dimension))
}

// ── Handlers ─────────────────────────────────────────────────────────

pub async fn list_sources(
    State(state): State<ApiState>,
    Query(params): Query<ListSourcesParams>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let index_kind = match params.index_kind.as_deref() {
        Some("vector") => Some(IndexKind::Vector),
        Some("pages") => Some(IndexKind::Pages),
        Some(_) => {
            return (StatusCode::BAD_REQUEST, "index_kind must be vector|pages").into_response()
        }
        None => None,
    };
    let filter = crate::db::knowledge::KsListFilter {
        scope_type: params.scope.clone(),
        client_id: params.client_id.clone(),
        workspace_id: params.workspace_id.clone(),
        q: params.q.clone(),
        status: params.status.clone(),
        index_kind,
        enabled: params.enabled,
    };
    let records = match rt.db.ks_list(&filter, limit, offset).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let total = match rt.db.ks_count(&filter).await {
        Ok(n) => n,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let mut out = Vec::with_capacity(records.len());
    for src in records {
        let doc_count = rt.db.kdoc_count_by_source(&src.id).await.unwrap_or(0);
        out.push(source_json(&src, doc_count));
    }
    Json(serde_json::json!({ "sources": out, "total": total })).into_response()
}

pub async fn create_source(
    State(state): State<ApiState>,
    Json(body): Json<CreateSourceRequest>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = validate_source_name(&body.name) {
        return e.into_response();
    }
    let index_vector = body.index_vector.unwrap_or(false);
    let index_pages = body.index_pages.unwrap_or(false);
    if !index_vector && !index_pages {
        return (StatusCode::BAD_REQUEST, "至少启用一个索引").into_response();
    }
    // scope 归一（缺省 workspace，经 scope_coords）
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

    let top_k = body.top_k.unwrap_or(5);
    let chunk_size = body.chunk_size.unwrap_or(512);
    let chunk_overlap = body.chunk_overlap.unwrap_or(64);
    let score_threshold = body.score_threshold.unwrap_or(0.3);
    if let Some(err) = validate_kb_params(top_k, chunk_size, chunk_overlap, score_threshold) {
        return (StatusCode::BAD_REQUEST, err).into_response();
    }
    let enabled = body.enabled.unwrap_or(true);
    let summary = resolve_summary(body.summary.as_ref(), body.description.as_ref());

    // embedding：仅 index_vector=true 时才需要
    let (emb_base_url, emb_api_key_raw, emb_model, emb_dimension) = if index_vector {
        match resolve_kb_embedding(&rt, &body).await {
            Ok(v) => v,
            Err(e) => return e.into_response(),
        }
    } else {
        (String::new(), String::new(), String::new(), 0)
    };
    let cipher = llm_state(&state).await.and_then(|l| l.cipher.clone());
    let emb_api_key = if emb_api_key_raw.is_empty() {
        String::new()
    } else {
        encrypt_field(cipher.as_ref(), &emb_api_key_raw)
    };

    let id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = rt
        .db
        .ks_create(&KsCreateOpts {
            id: id.clone(),
            name: body.name.trim().to_string(),
            summary: summary.clone(),
            index_vector,
            index_pages,
            scope_type: scope_type.clone(),
            client_id: client_id.clone(),
            workspace_id: workspace_id.clone(),
            emb_base_url: emb_base_url.clone(),
            emb_api_key: emb_api_key.clone(),
            emb_model: emb_model.clone(),
            emb_dimension,
            top_k,
            chunk_size,
            chunk_overlap,
            score_threshold,
            enabled,
        })
        .await
    {
        if e.to_string().contains("UNIQUE") {
            return (
                StatusCode::CONFLICT,
                "name already exists in this scope".to_string(),
            )
                .into_response();
        }
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    match rt.db.ks_get(&id).await {
        Ok(Some(src)) => {
            let doc_count = rt.db.kdoc_count_by_source(&id).await.unwrap_or(0);
            (StatusCode::CREATED, Json(source_json(&src, doc_count))).into_response()
        }
        _ => (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response(),
    }
}

pub async fn get_source(State(state): State<ApiState>, Path(id): Path<String>) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(src) = (match rt.db.ks_get(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    let doc_count = rt.db.kdoc_count_by_source(&id).await.unwrap_or(0);
    Json(source_json(&src, doc_count)).into_response()
}

pub async fn update_source(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSourceRequest>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(existing) = (match rt.db.ks_get(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    if let Err(e) = validate_source_name(&body.name) {
        return e.into_response();
    }
    let top_k = body.top_k.unwrap_or(existing.top_k);
    let chunk_size = body.chunk_size.unwrap_or(existing.chunk_size);
    let chunk_overlap = body.chunk_overlap.unwrap_or(existing.chunk_overlap);
    let score_threshold = body.score_threshold.unwrap_or(existing.score_threshold);
    if let Some(err) = validate_kb_params(top_k, chunk_size, chunk_overlap, score_threshold) {
        return (StatusCode::BAD_REQUEST, err).into_response();
    }
    // 开关：缺省保持原值
    let new_index_vector = body.index_vector.unwrap_or(existing.index_vector != 0);
    let new_index_pages = body.index_pages.unwrap_or(existing.index_pages != 0);
    if !new_index_vector && !new_index_pages {
        return (StatusCode::BAD_REQUEST, "至少启用一个索引").into_response();
    }
    let summary = resolve_summary(body.summary.as_ref(), body.description.as_ref());
    let summary_val = if summary.is_empty() {
        existing.summary.clone()
    } else {
        summary
    };

    // emb 变更检测：仅当 index_vector 启用时才有意义
    let new_base = match body.emb_base_url.as_deref() {
        None => existing.emb_base_url.clone(),
        Some(s) if s.trim().is_empty() => {
            return (StatusCode::BAD_REQUEST, "emb_base_url must not be empty").into_response();
        }
        Some(s) => s.trim().to_string(),
    };
    let new_model = match body.emb_model.as_deref() {
        None => existing.emb_model.clone(),
        Some(s) if s.trim().is_empty() => {
            return (StatusCode::BAD_REQUEST, "emb_model must not be empty").into_response();
        }
        Some(s) => s.trim().to_string(),
    };
    let new_dim = match body.emb_dimension {
        None => existing.emb_dimension,
        Some(d) if d < 1 => {
            return (
                StatusCode::BAD_REQUEST,
                "emb_dimension must be >= 1 (probe it via /test-embedding)",
            )
                .into_response();
        }
        Some(d) => d,
    };
    let new_api_key_raw = body
        .emb_api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let emb_touched = body.emb_base_url.is_some()
        || body.emb_model.is_some()
        || body.emb_dimension.is_some()
        || new_api_key_raw.is_some();
    // 开关变更本身不触发摄入：index_pages 0→1 后已有文档不会有页面，由用户逐文档
    // reindex 或等下次上传；此处加注释说明，UI 会在批 5 提示。
    let switch_changed = new_index_vector != (existing.index_vector != 0)
        || new_index_pages != (existing.index_pages != 0);

    // 未触及 emb 且开关未变：仅更新 name/summary/检索参数
    if !emb_touched && !switch_changed {
        if let Err(e) = rt
            .db
            .ks_update(
                &id,
                &KsUpdateOpts {
                    name: Some(body.name.trim().to_string()),
                    summary: Some(summary_val.clone()),
                    top_k: Some(top_k),
                    chunk_size: Some(chunk_size),
                    chunk_overlap: Some(chunk_overlap),
                    score_threshold: Some(score_threshold),
                    ..Default::default()
                },
            )
            .await
        {
            if e.to_string().contains("UNIQUE") {
                return (
                    StatusCode::CONFLICT,
                    "name already exists in this scope".to_string(),
                )
                    .into_response();
            }
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
        }
        return Json(serde_json::json!({ "status": "ok" })).into_response();
    }

    // emb 变更是否需要全量重建：仅当 vector 启用且三元变化
    let rebuild_needed = new_index_vector
        && (new_base != existing.emb_base_url
            || new_model != existing.emb_model
            || new_dim != existing.emb_dimension);

    let cipher = llm_state(&state).await.and_then(|l| l.cipher.clone());
    let new_api_key_enc = new_api_key_raw.map_or_else(
        || existing.emb_api_key.clone(),
        |k| encrypt_field(cipher.as_ref(), k),
    );

    // 先更新元数据（含开关与 emb）
    if let Err(e) = rt
        .db
        .ks_update(
            &id,
            &KsUpdateOpts {
                name: Some(body.name.trim().to_string()),
                summary: Some(summary_val.clone()),
                index_vector: Some(new_index_vector),
                index_pages: Some(new_index_pages),
                top_k: Some(top_k),
                chunk_size: Some(chunk_size),
                chunk_overlap: Some(chunk_overlap),
                score_threshold: Some(score_threshold),
                emb_base_url: Some(new_base.clone()),
                emb_api_key: Some(new_api_key_enc.clone()),
                emb_model: Some(new_model.clone()),
                emb_dimension: Some(new_dim),
                ..Default::default()
            },
        )
        .await
    {
        if e.to_string().contains("UNIQUE") {
            return (
                StatusCode::CONFLICT,
                "name already exists in this scope".to_string(),
            )
                .into_response();
        }
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    if !rebuild_needed {
        return Json(serde_json::json!({ "status": "ok" })).into_response();
    }

    // ── 全量重建：emb 三元已变，旧向量全部失效 ──
    let was_enabled = existing.enabled != 0;
    let _ = rt.db.ks_set_enabled(&id, false).await;
    if let Err(e) = rt.store.delete_kb(&id).await {
        tracing::warn!(kb_id = %id, error = %e, "knowledge: store delete_kb (rebuild) failed");
    }
    if let Err(e) = rt.db.rag_delete_chunks_by_kb(&id).await {
        let _ = rt.db.ks_set_enabled(&id, was_enabled).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    let kb_new = match rt.db.ks_get(&id).await {
        Ok(Some(k)) => k,
        Ok(None) => return (StatusCode::NOT_FOUND, "knowledge source not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let docs = match rt.db.kdoc_list(&id).await {
        Ok(d) => d,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
    let mut reindexed = 0i64;
    let mut skipped = 0i64;
    let mut missing_source = 0i64;
    for doc in &docs {
        match super::docs::reindex_source_doc(&rt, &kb_new, doc, Some(sem.clone())).await {
            Ok(super::docs::ReindexOutcome::Spawned) => reindexed += 1,
            Ok(super::docs::ReindexOutcome::Skipped) => skipped += 1,
            Ok(super::docs::ReindexOutcome::MissingSource) => missing_source += 1,
            Err(e) => {
                let _ = rt.db.ks_set_enabled(&id, was_enabled).await;
                return e.into_response();
            }
        }
    }
    let _ = rt.db.ks_set_enabled(&id, was_enabled).await;
    Json(serde_json::json!({
        "status": "ok",
        "reindexed": reindexed,
        "skipped": skipped,
        "missing_source": missing_source,
    }))
    .into_response()
}

pub async fn patch_source(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(_existing) = (match rt.db.ks_get(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    let enabled = match body.get("enabled") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => return (StatusCode::BAD_REQUEST, "enabled must be a boolean").into_response(),
        None => return (StatusCode::BAD_REQUEST, "enabled is required").into_response(),
    };
    if let Err(e) = rt.db.ks_set_enabled(&id, enabled).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

pub async fn delete_source(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(_src) = (match rt.db.ks_get(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    let _ = rt.db.ks_set_enabled(&id, false).await;
    // 删 shard：对 pages-only 容器是安全 no-op（shard 不存在），也保留以防双索引残留。
    if let Err(e) = rt.store.delete_kb(&id).await {
        tracing::warn!(kb_id = %id, error = %e, "knowledge: store delete_kb failed");
    }
    let source_dir = crate::llm::rag::doc_store::source_docs_dir(rt.store.data_dir(), &id);
    if source_dir.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(&source_dir).await {
            tracing::warn!(kb_id = %id, error = %e, "knowledge: remove source docs dir failed");
        }
    }
    if let Err(e) = rt.db.ks_delete(&id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    StatusCode::OK.into_response()
}
