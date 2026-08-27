//! 页面 / 图谱 / 搜索 handlers（从旧 `agent/wiki.rs` 搬运）。

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use crate::db::agent::normalize_db_datetime;
use crate::db::wiki::{normalize_wiki_ref, AgentWikiPageRecord, AgentWikiPageSummary};
use crate::mgmt::api::ApiState;

use super::knowledge_rt;

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

#[derive(Debug, Deserialize, Default)]
pub struct ListPagesParams {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub ref_prefix: Option<String>,
    #[serde(default)]
    pub locked: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct PutPageRequest {
    #[serde(rename = "ref")]
    pub page_ref: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: String,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/knowledge/:id/pages`：分页列出指定容器下的页面，支持 `q`/`ref_prefix`/`locked` 过滤；只读，不写库。
pub async fn list_pages(
    State(state): State<ApiState>,
    Path(source_id): Path<String>,
    Query(params): Query<ListPagesParams>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if rt.db.ks_get(&source_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    match rt
        .db
        .wiki_list_pages(
            &source_id,
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

/// `GET /api/knowledge/:id/pages/*ref`：按归一化 `ref` 查询单页详情；只读，不写库也不改图谱。
pub async fn get_page(
    State(state): State<ApiState>,
    Path((source_id, page_ref)): Path<(String, String)>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(norm) = normalize_wiki_ref(&page_ref) else {
        return (StatusCode::BAD_REQUEST, "invalid ref").into_response();
    };
    match rt.db.wiki_get_page(&source_id, &norm).await {
        Ok(Some(p)) => Json(page_json(&p)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// `PUT /api/knowledge/:id/pages/*ref`：创建或更新单页（幂等 upsert），写库并同事务同步 FTS 与页面链接图谱（解析 `[[ref]]` 重建出边、回填悬空入边）。
/// 已 `locked` 的页面不会被非锁定写入覆盖；`ref` 取 `body.ref` 优先、回退路径参数，均需归一化校验。
pub async fn put_page(
    State(state): State<ApiState>,
    Path((source_id, page_ref)): Path<(String, String)>,
    Json(body): Json<PutPageRequest>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if rt.db.ks_get(&source_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
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
    match rt
        .db
        .wiki_upsert_page(
            &source_id,
            &norm,
            body.title.trim(),
            body.summary.trim(),
            &body.content,
            true,
            None,
        )
        .await
    {
        Ok(id) => match rt.db.wiki_get_page_by_id(&id).await {
            Ok(Some(p)) => Json(page_json(&p)).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// `DELETE /api/knowledge/:id/pages/*ref`：删除指定页面，写库并同事务清理 FTS、出边及入边悬空化。
/// 同步更新容器的 `page_count`/`version`；目标不存在返回 404。
pub async fn delete_page(
    State(state): State<ApiState>,
    Path((source_id, page_ref)): Path<(String, String)>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(norm) = normalize_wiki_ref(&page_ref) else {
        return (StatusCode::BAD_REQUEST, "invalid ref").into_response();
    };
    match rt.db.wiki_delete_page(&source_id, &norm).await {
        Ok(true) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// `GET /api/knowledge/:id/graph`：返回指定容器的页面链接图谱（`nodes` 为页面摘要、`edges` 为有向边，含悬空标记）；只读，不写库。
pub async fn get_graph(
    State(state): State<ApiState>,
    Path(source_id): Path<String>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if rt.db.ks_get(&source_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match rt.db.wiki_graph(&source_id).await {
        Ok(g) => {
            let nodes: Vec<_> = g.nodes.iter().map(page_summary_json).collect();
            Json(serde_json::json!({ "nodes": nodes, "edges": g.edges })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// `GET /api/knowledge/:id/search`：在单个容器内检索页面（FTS5 优先、短词/零命中回退 LIKE）；只读，不写库。
/// 限定 `source_id` 作用域，`limit` 缺省 20、 clamped 1..20。
pub async fn search_knowledge(
    State(state): State<ApiState>,
    Path(source_id): Path<String>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if rt.db.ks_get(&source_id).await.unwrap_or(None).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 20);
    match rt.db.wiki_search(&[source_id], &params.q, limit).await {
        Ok(hits) => Json(serde_json::json!({ "hits": hits })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// `GET /api/knowledge/search`：跨全部容器全局检索页面（FTS5 优先、短词/零命中回退 LIKE）；只读，不写库。
/// 空作用域（`visible_wiki_ids=[]`）即全局，与单容器检索 `search_knowledge` 互补；`limit` 缺省 20、clamped 1..20。
pub async fn search_all(
    State(state): State<ApiState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let limit = params.limit.unwrap_or(20).clamp(1, 20);
    match rt.db.wiki_search(&[], &params.q, limit).await {
        Ok(hits) => Json(serde_json::json!({ "hits": hits })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}
