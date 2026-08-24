//! AI 记忆体管理 REST API（步骤 7）：settings / memories CRUD / clear / 手动蒸馏 /
//! SSE 事件流。仅 `rag` feature 编译（模块声明在 `mgmt/api/agent/mod.rs` 门控）。
//!
//! 路由由本模块的 `protected_router` / `public_router` 组装，在 `mgmt/api/mod.rs`
//! 的 rag 合并块中挂载（位于 JWT layer 之前），与 RAG 知识库路由同位置。

#![cfg(feature = "rag")]

use std::time::Duration;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::agent::memory::{
    scope_coords, MemoryState, MEMORY_CONTENT_MAX_CHARS, MAX_TAGS, MEMORY_KB_ID, TAG_MAX_CHARS,
};
use crate::auth::validate_token;
use crate::db::agent::normalize_db_datetime;
use crate::db::memory::{AgentMemoryRecord, AgentMemorySettingsRecord};
use crate::llm::crypto::{decrypt_field, encrypt_field, LlmCipher};
use crate::llm::rag::store::ChunkPoint;
use crate::mgmt::api::dto::SseQuery;
use crate::mgmt::api::ApiState;

use super::{mem_runtime, new_id};

// ── 请求体 DTO ──────────────────────────────────────────────────

/// PUT /api/agent/memory/settings 请求体（部分更新：缺省字段沿用当前值）。
///
/// - `emb_api_key` 空串 = 沿用已存（密文原样保留，不二次加密）；
/// - `emb_dimension` 与已存值不同 → 409（需先 `POST /clear` 清空重建）；
/// - 启用（`enabled` 置 1）前必须 emb 配置齐全。
#[derive(Debug, Deserialize)]
pub struct UpdateMemorySettingsRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub emb_base_url: Option<String>,
    #[serde(default)]
    pub emb_api_key: Option<String>,
    #[serde(default)]
    pub emb_model: Option<String>,
    #[serde(default)]
    pub emb_dimension: Option<i64>,
    #[serde(default)]
    pub distill_model: Option<String>,
    #[serde(default)]
    pub top_k: Option<i64>,
    #[serde(default)]
    pub score_threshold: Option<f64>,
    #[serde(default)]
    pub inject_budget_tokens: Option<i64>,
    #[serde(default)]
    pub pin_always_inject: Option<bool>,
    /// Skill 库总闸（opt-in 默认关；开启不要求 embedding——Skill 蒸馏仅需 LLM）。
    #[serde(default)]
    pub skill_enabled: Option<bool>,
    /// 会话开始注入的技能清单条数上限（默认 20）。
    #[serde(default)]
    pub skill_list_max: Option<i64>,
    /// Wiki 总闸（默认 1：显式上传才有 LLM 开销，无非预期成本）。
    #[serde(default)]
    pub wiki_enabled: Option<bool>,
    /// 会话开始注入的 Wiki 清单条数上限（默认 20）。
    #[serde(default)]
    pub wiki_list_max: Option<i64>,
}

/// GET /api/agent/memories 的 query 参数。`limit` 默认 50（handler 层 clamp 到 [1, 200]）。
#[derive(Debug, Default, Deserialize)]
pub struct ListMemoriesParams {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// POST /api/agent/memories 请求体。`scope_type` 缺省/空串 → workspace。
#[derive(Debug, Deserialize)]
pub struct CreateMemoryRequest {
    pub content: String,
    #[serde(default)]
    pub scope_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// PUT /api/agent/memories/:id 请求体。
#[derive(Debug, Deserialize)]
pub struct UpdateMemoryRequest {
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub scope_type: String,
    #[serde(default)]
    pub confidence: Option<f64>,
}

// ── 共享辅助 ─────────────────────────────────────────────────────

const VALID_SCOPES: [&str; 3] = ["global", "client", "workspace"];

/// settings 视图 JSON：`emb_api_key` 永不回传（密文/明文都不出库），只回 `has_key`
/// 布尔表示是否存有可用 key（对齐 provider api-key 列表的脱敏策略）。
fn settings_json(s: &AgentMemorySettingsRecord, cipher: Option<&LlmCipher>) -> serde_json::Value {
    let has_key = !decrypt_field(cipher, &s.emb_api_key)
        .unwrap_or_default()
        .is_empty();
    serde_json::json!({
        "id": s.id,
        "enabled": s.enabled != 0,
        "emb_base_url": s.emb_base_url,
        "emb_api_key": "",
        "has_key": has_key,
        "emb_model": s.emb_model,
        "emb_dimension": s.emb_dimension,
        "distill_model": s.distill_model,
        "top_k": s.top_k,
        "score_threshold": s.score_threshold,
        "inject_budget_tokens": s.inject_budget_tokens,
        "pin_always_inject": s.pin_always_inject != 0,
        "skill_enabled": s.skill_enabled != 0,
        "skill_list_max": s.skill_list_max,
        "wiki_enabled": s.wiki_enabled != 0,
        "wiki_list_max": s.wiki_list_max,
        "created_at": normalize_db_datetime(&s.created_at),
        "updated_at": normalize_db_datetime(&s.updated_at),
    })
}

/// 记忆视图 JSON：`tags` 解析为数组（DB 存 JSON 字符串）、`pinned` 归一为布尔，
/// 时间字段经 M12 的 ISO-8601 归一化。
fn memory_json(m: &AgentMemoryRecord) -> serde_json::Value {
    let tags: Vec<String> = serde_json::from_str(&m.tags).unwrap_or_default();
    let last_hit_at = m.last_hit_at.as_ref().map(|t| normalize_db_datetime(t));
    serde_json::json!({
        "id": m.id,
        "content": m.content,
        "scope_type": m.scope_type,
        "client_id": m.client_id,
        "workspace_id": m.workspace_id,
        "tags": tags,
        "confidence": m.confidence,
        "source_session_id": m.source_session_id,
        "source_trigger": m.source_trigger,
        "pinned": m.pinned != 0,
        "hit_count": m.hit_count,
        "last_hit_at": last_hit_at,
        "created_at": normalize_db_datetime(&m.created_at),
        "updated_at": normalize_db_datetime(&m.updated_at),
    })
}

/// 校验 content（非空 + ≤2048 字符）与 scope_type（global|client|workspace）。
fn validate_content_and_scope(
    content: &str,
    scope_type: &str,
) -> Result<(), (StatusCode, String)> {
    if content.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "content is required".to_string()));
    }
    if content.chars().count() > MEMORY_CONTENT_MAX_CHARS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("content must be at most {MEMORY_CONTENT_MAX_CHARS} chars"),
        ));
    }
    if !VALID_SCOPES.contains(&scope_type) {
        return Err((
            StatusCode::BAD_REQUEST,
            "scope_type must be one of: global, client, workspace".to_string(),
        ));
    }
    Ok(())
}

/// 校验 tags（数量 ≤ MAX_TAGS、每项非空且 ≤ TAG_MAX_CHARS）。`pub(crate)` 供
/// skills.rs（Skill 手动创建/编辑复用同一 tags 约束）。
pub(crate) fn validate_tags(tags: &[String]) -> Result<(), (StatusCode, String)> {
    if tags.len() > MAX_TAGS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("tags must be at most {MAX_TAGS}"),
        ));
    }
    for t in tags {
        if t.trim().is_empty() {
            return Err((StatusCode::BAD_REQUEST, "tags must be non-empty".to_string()));
        }
        if t.chars().count() > TAG_MAX_CHARS {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("tag too long (max {TAG_MAX_CHARS} chars)"),
            ));
        }
    }
    Ok(())
}

/// 记忆向量化 + 落库（全局单一 shard，`ChunkPoint.id`/`doc_id` 均取记忆 id）。
///
/// 语义：总闸开启 + emb 配置齐全 + dim>0 时才执行；embed/upsert 失败仅告警并
/// **跳过向量**，DB 记忆仍保留——检索侧向量缺失只影响相似度召回，面板始终按
/// DB 原文展示，故不因向量失败而回滚记忆落库（与任务约定一致）。
async fn embed_memory(mem: &MemoryState, id: &str, content: &str) {
    let Some(embedder) = mem.embedder().await else {
        return;
    };
    // embedder() 已保证 enabled + url/model/key 可用；upsert 还需 dim>0（设置校验
    // 强制 enable 时 dim>0，此处兜底防手改库导致的 0 维 shard）。
    let dim = mem.settings().await.emb_dimension;
    if dim <= 0 {
        return;
    }
    match embedder.embed_one(content).await {
        Ok(vec) => {
            let point = ChunkPoint {
                id: id.to_string(),
                vector: vec,
                doc_id: id.to_string(),
                seq: 0,
                heading_path: String::new(),
            };
            if let Err(e) = mem
                .store
                .upsert(MEMORY_KB_ID, dim as usize, vec![point])
                .await
            {
                tracing::warn!(memory_id = id, error = %e, "memory vector upsert failed");
            }
        }
        Err(e) => tracing::warn!(
            memory_id = id,
            error = %e,
            "memory embed failed, skipping vector"
        ),
    }
}

// ── Settings handlers ────────────────────────────────────────────

/// GET /api/agent/memory/settings — 读全局设置（emb_api_key 脱敏为 `has_key`）。
pub async fn get_settings(State(state): State<ApiState>) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let s = mem.settings().await;
    Json(settings_json(&s, mem.cipher.as_ref())).into_response()
}

/// PUT /api/agent/memory/settings — 更新全局设置。
///
/// 规则：
/// - `enabled` 置 1 前必须 emb_base_url / emb_model 齐全、emb_dimension > 0，
///   且 emb_api_key 非空（或已存有可用 key），否则 400；
/// - `emb_dimension` 与已存值不同 → 409 拒绝直切（换 emb 模型需先清空重建）；
/// - `emb_api_key` 空串 = 沿用已存密文。
pub async fn put_settings(
    State(state): State<ApiState>,
    Json(body): Json<UpdateMemorySettingsRequest>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let current = mem.settings().await;

    // 部分更新：请求缺省字段沿用当前值。最终生效值 = 请求覆盖 + 当前基线。
    let enabled = body.enabled.unwrap_or(current.enabled != 0);
    let emb_base_url = body
        .emb_base_url
        .as_deref()
        .unwrap_or(&current.emb_base_url)
        .trim()
        .to_string();
    let emb_model = body
        .emb_model
        .as_deref()
        .unwrap_or(&current.emb_model)
        .trim()
        .to_string();
    let emb_dimension = body.emb_dimension.unwrap_or(current.emb_dimension);

    // 启用前配置校验（基于最终值）
    if enabled {
        if emb_base_url.is_empty() || emb_model.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                "enabling memory requires emb_base_url and emb_model",
            )
                .into_response();
        }
        if emb_dimension <= 0 {
            return (
                StatusCode::BAD_REQUEST,
                "enabling memory requires emb_dimension > 0",
            )
                .into_response();
        }
        let has_stored_key = !decrypt_field(mem.cipher.as_ref(), &current.emb_api_key)
            .unwrap_or_default()
            .is_empty();
        let key_provided = body
            .emb_api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty());
        if !key_provided && !has_stored_key {
            return (
                StatusCode::BAD_REQUEST,
                "enabling memory requires emb_api_key",
            )
                .into_response();
        }
    }

    // dim 冲突：请求显式携带新 dim 且与已存（都 >0）不同 → 409
    if let Some(dim) = body.emb_dimension {
        if dim > 0 && current.emb_dimension > 0 && dim != current.emb_dimension {
            return (
                StatusCode::CONFLICT,
                "emb_dimension change requires clearing existing memories first \
                 (POST /api/agent/memory/clear)",
            )
                .into_response();
        }
    }

    // 空串/未提供 key 保持已存（密文原样），否则加密新值
    let api_key = match body.emb_api_key.as_deref() {
        Some(k) if !k.trim().is_empty() => encrypt_field(mem.cipher.as_ref(), k.trim()),
        _ => current.emb_api_key.clone(),
    };

    let s = AgentMemorySettingsRecord {
        id: 1,
        enabled: i32::from(enabled),
        emb_base_url,
        emb_api_key: api_key,
        emb_model,
        emb_dimension: if emb_dimension > 0 {
            emb_dimension
        } else {
            current.emb_dimension
        },
        distill_model: body
            .distill_model
            .as_deref()
            .unwrap_or(&current.distill_model)
            .trim()
            .to_string(),
        top_k: body.top_k.unwrap_or(current.top_k),
        score_threshold: body.score_threshold.unwrap_or(current.score_threshold),
        inject_budget_tokens: body
            .inject_budget_tokens
            .unwrap_or(current.inject_budget_tokens),
        pin_always_inject: i32::from(
            body.pin_always_inject
                .unwrap_or(current.pin_always_inject != 0),
        ),
        skill_enabled: i32::from(body.skill_enabled.unwrap_or(current.skill_enabled != 0)),
        skill_list_max: body.skill_list_max.unwrap_or(current.skill_list_max),
        wiki_enabled: i32::from(body.wiki_enabled.unwrap_or(current.wiki_enabled != 0)),
        wiki_list_max: body.wiki_list_max.unwrap_or(current.wiki_list_max),
        created_at: current.created_at,
        updated_at: current.updated_at,
    };
    match mem.db.memory_upsert_settings(&s).await {
        Ok(()) => Json(settings_json(&s, mem.cipher.as_ref())).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// POST /api/agent/memory/clear — 清空全部记忆：删向量 shard + 清空 agent_memories 表。
///
/// 供切换 emb 模型（dim 变更）后重建。管理面操作低频，不额外加并发锁（与 RAG
/// delete_kb 同理）；`store.delete_kb` 需保证无在途 upsert/search/delete_by_doc。
pub async fn clear_memory(State(state): State<ApiState>) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    // 先删向量 shard（清缓存句柄 + 删目录），再清 DB——向量删失败仅告警，DB 始终一致。
    if let Err(e) = mem.store.delete_kb(MEMORY_KB_ID).await {
        tracing::warn!(error = %e, "memory clear: store delete_kb failed");
    }
    // 清空记忆表（无专用 DAO，直连 pool 执行；与 mgmt/stats.rs 中 db.pool 用法一致）
    if let Err(e) = sqlx::query("DELETE FROM agent_memories")
        .execute(&mem.db.pool)
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

// ── Memories CRUD ────────────────────────────────────────────────

/// 与 DAO `memory_list` 相同过滤条件的 COUNT 查询（列表响应壳需要 total；DAO 无
/// count 方法，且本文件不新增 DAO，故在此镜像 WHERE 子句）。返回总条数。
async fn count_memories(
    db: &crate::db::Database,
    params: &ListMemoriesParams,
) -> Result<i64, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new("SELECT COUNT(*) FROM agent_memories WHERE 1=1");
    if let Some(s) = params.scope.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND scope_type = ").push_bind(s);
    }
    if let Some(c) = params.client_id.as_deref().filter(|c| !c.is_empty()) {
        qb.push(" AND client_id = ").push_bind(c);
    }
    if let Some(w) = params.workspace_id.as_deref().filter(|w| !w.is_empty()) {
        qb.push(" AND workspace_id = ").push_bind(w);
    }
    if let Some(q) = params.q.as_deref().filter(|q| !q.is_empty()) {
        qb.push(" AND content LIKE ").push_bind(format!("%{q}%"));
    }
    if let Some(true) = params.pinned {
        qb.push(" AND pinned = 1");
    } else if let Some(false) = params.pinned {
        qb.push(" AND pinned = 0");
    }
    qb.build_query_scalar::<i64>().fetch_one(&db.pool).await
}

/// GET /api/agent/memories — 列表（作用域/client/workspace/q/pinned 过滤 + 排序 + 分页）。
/// 返回 `{memories, total}` 响应壳。`limit` 默认 50，clamp 到 [1, 200]；`sort` 白名单
/// （recent/created/confidence/hits）由 DAO 处理，未知值回退 recent。
pub async fn list_memories(
    State(state): State<ApiState>,
    Query(params): Query<ListMemoriesParams>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let rows = match mem
        .db
        .memory_list(
            params.scope.as_deref(),
            params.client_id.as_deref(),
            params.workspace_id.as_deref(),
            params.q.as_deref(),
            params.pinned,
            params.sort.as_deref(),
            limit,
            offset,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let total = match count_memories(&mem.db, &params).await {
        Ok(n) => n,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let memories: Vec<serde_json::Value> = rows.iter().map(memory_json).collect();
    Json(serde_json::json!({ "memories": memories, "total": total })).into_response()
}

/// POST /api/agent/memories — 手动新建记忆。
///
/// 固定 `source_trigger='manual'`、`confidence=1.0`（手动新建即高置信）；scope 校验
/// 后按 `scope_coords` 归一化三元组（global → 全部空，client → 无 workspace）。
/// 向量化在总闸开启 + emb 可用时执行，embed 失败仅告警并跳过向量（DB 记忆保留）。
pub async fn create_memory(
    State(state): State<ApiState>,
    Json(body): Json<CreateMemoryRequest>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let scope = if body.scope_type.trim().is_empty() {
        "workspace"
    } else {
        body.scope_type.trim()
    };
    if let Err(e) = validate_content_and_scope(&body.content, scope) {
        return e.into_response();
    }
    if let Err(e) = validate_tags(&body.tags) {
        return e.into_response();
    }
    let (scope_type, client_id, workspace_id) = scope_coords(scope, &body.client_id, &body.workspace_id);
    let id = new_id();
    let tags = serde_json::to_string(&body.tags).unwrap_or_else(|_| "[]".to_string());
    let content = body.content.trim();
    if let Err(e) = mem
        .db
        .memory_insert(
            &id,
            content,
            &scope_type,
            &client_id,
            &workspace_id,
            &tags,
            1.0,
            "",
            "manual",
            false,
        )
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    embed_memory(&mem, &id, content).await;
    match mem.db.memory_get_by_id(&id).await {
        Ok(Some(m)) => (StatusCode::CREATED, Json(memory_json(&m))).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// GET /api/agent/memories/:id — 详情。
pub async fn get_memory(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    match mem.db.memory_get_by_id(&id).await {
        Ok(Some(m)) => Json(memory_json(&m)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// PUT /api/agent/memories/:id — 编辑 content/tags/scope/confidence。
///
/// content 变化时重新 embed 并 upsert 向量**覆盖**同一记忆 id 的点（id/doc_id 对齐，
/// upsert 天然覆盖）。embed 失败仅告警并跳过向量——检索侧向量仍指向同一记忆 id，
/// 面板按 DB 原文展示，故不影响正确性（与 create 的降级语义一致）。
pub async fn update_memory(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateMemoryRequest>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let existing = match mem.db.memory_get_by_id(&id).await {
        Ok(Some(m)) => m,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let scope = if body.scope_type.trim().is_empty() {
        "workspace"
    } else {
        body.scope_type.trim()
    };
    if let Err(e) = validate_content_and_scope(&body.content, scope) {
        return e.into_response();
    }
    if let Err(e) = validate_tags(&body.tags) {
        return e.into_response();
    }
    let confidence = body.confidence.unwrap_or(0.8);
    if !(0.0..=1.0).contains(&confidence) {
        return (
            StatusCode::BAD_REQUEST,
            "confidence must be in [0, 1]".to_string(),
        )
            .into_response();
    }
    let (scope_type, client_id, workspace_id) =
        scope_coords(scope, &existing.client_id, &existing.workspace_id);
    let tags = serde_json::to_string(&body.tags).unwrap_or_else(|_| "[]".to_string());
    let content = body.content.trim();
    // `memory_update` DAO 不更新 client_id/workspace_id——scope 变更（global↔client↔
    // workspace）需按 `scope_coords` 归一化坐标（见 memory/mod.rs），否则 global 记忆
    // 会残留旧坐标，破坏 scope_ok 精确匹配。此处用一条 SQL 同步更新全部可变字段。
    if let Err(e) = sqlx::query(
        "UPDATE agent_memories SET content = ?, tags = ?, scope_type = ?, \
         client_id = ?, workspace_id = ?, confidence = ?, updated_at = datetime('now') \
         WHERE id = ?",
    )
    .bind(content)
    .bind(&tags)
    .bind(&scope_type)
    .bind(&client_id)
    .bind(&workspace_id)
    .bind(confidence)
    .bind(&id)
    .execute(&mem.db.pool)
    .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    if existing.content != content {
        embed_memory(&mem, &id, content).await;
    }
    match mem.db.memory_get_by_id(&id).await {
        Ok(Some(m)) => Json(memory_json(&m)).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// POST /api/agent/memories/:id/pin — 置顶切换（1 ↔ 0）。
pub async fn toggle_pin(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    match mem.db.memory_get_by_id(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }
    match mem.db.memory_toggle_pin(&id).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// DELETE /api/agent/memories/:id — 删 DB 行 + `delete_by_doc` 删向量点。
pub async fn delete_memory(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    match mem.db.memory_get_by_id(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }
    // 删向量点（doc_id = 记忆 id）。dim>0 才执行：0 维 shard 无意义且 open_shard 会
    // 空建目录；shard 不存在 / 维度失配由 store 内部降级或报错 → best-effort。
    let dim = mem.settings().await.emb_dimension;
    if dim > 0 {
        if let Err(e) = mem.store.delete_by_doc(MEMORY_KB_ID, dim as usize, &id).await {
            tracing::warn!(memory_id = %id, error = %e, "memory vector delete failed");
        }
    }
    match mem.db.memory_delete(&id).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

// ── 手动蒸馏 ─────────────────────────────────────────────────────

/// POST /api/agent/sessions/:id/distill — 手动重新蒸馏。
///
/// 先把 `agent_sessions.distilled` 复位为 0（使蒸馏器内部 CAS 本次必赢），再调用
/// `distill::trigger_distill`（enabled 检查 → CAS → 快照 → spawn 由 distill 模块
/// 内部完成，失败仅广播 failed 事件不报错）。记忆总闸关闭时触发为 no-op，端点仍
/// 返回 200——蒸馏在后台执行，不阻塞。
pub async fn distill_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    match mem.db.agent_get_session(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }
    // 复位蒸馏标记（无专用 DAO，直连 pool 执行）
    if let Err(e) = sqlx::query(
        "UPDATE agent_sessions SET distilled = 0, updated_at = datetime('now') WHERE id = ?",
    )
    .bind(&id)
    .execute(&mem.db.pool)
    .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    crate::agent::memory::distill::trigger_distill(&mem, &id, "manual").await;
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

// ── SSE 事件流 ───────────────────────────────────────────────────

/// GET /api/agent/memory/events — SSE 事件流（蒸馏/注入状态）。token 走 query 参数
/// 认证（public 路由，照抄 sse_kb_events），事件名 "memory"，keep-alive 30s。
///
/// # Panics
///
/// 仅当构造错误响应（Response builder + body）时 `unwrap()`，属不可达路径。
pub async fn sse_memory_events(
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
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => {
            return axum::response::Response::builder()
                .status(e.status())
                .body(Body::from(e.message().to_string()))
                .unwrap();
        }
    };
    let mut rx = mem.subscribe();
    let stream = async_stream::stream! {
        loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let json = serde_json::to_string(&ev).unwrap_or_default();
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("memory").data(json),
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

// ── 路由 ─────────────────────────────────────────────────────────

/// 受保护路由（JWT 覆盖）。test-embedding 直接复用 RAG 的 handler，不抄代码。
pub fn protected_router() -> Router<ApiState> {
    Router::new()
        .route(
            "/api/agent/memory/settings",
            get(get_settings).put(put_settings),
        )
        .route(
            "/api/agent/memory/settings/test-embedding",
            post(crate::mgmt::api::rag::test_embedding),
        )
        .route("/api/agent/memory/clear", post(clear_memory))
        .route("/api/agent/memories", get(list_memories).post(create_memory))
        .route(
            "/api/agent/memories/:id",
            get(get_memory).put(update_memory).delete(delete_memory),
        )
        .route("/api/agent/memories/:id/pin", post(toggle_pin))
        .route("/api/agent/sessions/:id/distill", post(distill_session))
}

/// 公开路由（SSE 事件流，`?token=` 认证）。
pub fn public_router() -> Router<ApiState> {
    Router::new().route("/api/agent/memory/events", get(sse_memory_events))
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(all(test, feature = "rag"))]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode as HttpStatus};
    use axum::Router;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::auth::AuthConfig;
    use crate::control_plane::ServerState;
    use crate::db::Database;

    /// 构造 ApiState：内存 DB + RAG 数据目录 + 固定测试主密钥（字段加密可用），并
    /// 注入记忆运行时（与 server.rs 生产路径一致：克隆 LlmState.rag_store 同一实例）。
    /// `rag_dir` 的存活期必须覆盖返回的 state（VectorStore 的 EdgeShard Drop 会 flush）。
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
            MemoryState::new(
                server_state.db().cloned().expect("db present"),
                llm.rag_store.clone(),
                llm.cipher.clone(),
                (*llm).clone(), // 与 server.rs 生产路径一致：Arc 解引用后克隆 LlmState
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

    /// 覆盖本模块全部路由的测试 Router（免 JWT）：SSE 挂 public，其余挂 protected，
    /// 与生产路由链一致。
    fn test_router(state: ApiState) -> Router {
        let public = Router::new().route(
            "/api/agent/memory/events",
            get(super::sse_memory_events),
        );
        let protected = Router::new()
            .route(
                "/api/agent/memory/settings",
                get(super::get_settings).put(super::put_settings),
            )
            .route(
                "/api/agent/memory/settings/test-embedding",
                post(crate::mgmt::api::rag::test_embedding),
            )
            .route("/api/agent/memory/clear", post(super::clear_memory))
            .route(
                "/api/agent/memories",
                get(super::list_memories).post(super::create_memory),
            )
            .route(
                "/api/agent/memories/:id",
                get(super::get_memory)
                    .put(super::update_memory)
                    .delete(super::delete_memory),
            )
            .route("/api/agent/memories/:id/pin", post(super::toggle_pin))
            .route(
                "/api/agent/sessions/:id/distill",
                post(super::distill_session),
            );
        public.merge(protected).with_state(state)
    }

    /// oneshot 请求助手：返回 (status, json body)。错误响应多为纯文本 → body 解析
    /// 失败返回 json!(null)，状态断言用。
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

    // ── settings ─────────────────────────────────────────────────

    #[tokio::test]
    async fn settings_roundtrip_and_masking() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        // 默认：disabled，无 key，密文不回传
        let (status, body) = call(
            &app,
            json_request(Method::GET, "/api/agent/memory/settings".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["enabled"], json!(false));
        assert_eq!(body["has_key"], json!(false));
        assert_eq!(body["emb_api_key"], json!(""));
        assert_eq!(body["emb_dimension"], json!(0));

        // 启用
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({
                    "enabled": true,
                    "emb_base_url": "http://localhost:11434/v1",
                    "emb_api_key": "sk-test",
                    "emb_model": "nomic-embed-text",
                    "emb_dimension": 8,
                }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "enable: {body}");
        assert_eq!(body["enabled"], json!(true));
        assert_eq!(body["has_key"], json!(true), "key 已存应回 has_key=true");
        assert_eq!(body["emb_api_key"], json!(""), "密文不回传");
        assert_eq!(body["emb_dimension"], json!(8));

        // 空串 key = 保持不变（沿用已存密文）
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({
                    "enabled": true,
                    "emb_base_url": "http://localhost:11434/v1",
                    "emb_api_key": "",
                    "emb_model": "nomic-embed-text",
                    "emb_dimension": 8,
                }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "empty key keep: {body}");
        assert_eq!(body["has_key"], json!(true), "沿用已存 key");
    }

    #[tokio::test]
    async fn settings_skill_fields_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        // 默认：skill_enabled=false、skill_list_max=20
        let (status, body) = call(
            &app,
            json_request(Method::GET, "/api/agent/memory/settings".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["skill_enabled"], json!(false));
        assert_eq!(body["skill_list_max"], json!(20));

        // 只开 skill_enabled（不要求 emb 配置——Skill 蒸馏仅需 LLM）
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({ "skill_enabled": true, "skill_list_max": 8 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "skill_enabled 单独开启: {body}");
        assert_eq!(body["skill_enabled"], json!(true));
        assert_eq!(body["skill_list_max"], json!(8));
        assert_eq!(body["enabled"], json!(false), "记忆总闸不受影响");

        // 部分更新只改 skill_list_max：skill_enabled 保持
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({ "skill_list_max": 30 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["skill_enabled"], json!(true), "缺省字段沿用当前值");
        assert_eq!(body["skill_list_max"], json!(30));
    }

    #[tokio::test]
    async fn settings_enable_requires_complete_config() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        // 缺 base_url
        let (status, _) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({ "enabled": true, "emb_api_key": "k", "emb_model": "m", "emb_dimension": 8 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // dim 为 0
        let (status, _) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({ "enabled": true, "emb_base_url": "http://x", "emb_api_key": "k", "emb_model": "m", "emb_dimension": 0 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 无 key（body 空且未存）
        let (status, _) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({ "enabled": true, "emb_base_url": "http://x", "emb_api_key": "", "emb_model": "m", "emb_dimension": 8 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
    }

    #[tokio::test]
    async fn settings_dim_change_conflict_409() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        let (status, _) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({ "enabled": true, "emb_base_url": "http://x", "emb_api_key": "k", "emb_model": "m", "emb_dimension": 8 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);

        // 换 dim → 409（需先 clear 清空重建）
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({ "enabled": true, "emb_base_url": "http://x", "emb_api_key": "k", "emb_model": "m", "emb_dimension": 16 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CONFLICT, "409 body: {body}");
    }

    #[tokio::test]
    async fn settings_disable_keeps_dim() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        let (status, _) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({ "enabled": true, "emb_base_url": "http://x", "emb_api_key": "k", "emb_model": "m", "emb_dimension": 8 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);

        // 关闭总闸：不要求 emb 配置，dim 沿用已存
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({ "enabled": false }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "disable: {body}");
        assert_eq!(body["enabled"], json!(false));
        assert_eq!(body["emb_dimension"], json!(8), "dim 应沿用已存值");
    }

    // ── memories CRUD ────────────────────────────────────────────

    #[tokio::test]
    async fn memories_crud_pin_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        // create
        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/memories".to_string(),
                &json!({
                    "content": "用户喜欢简洁的代码",
                    "scope_type": "workspace",
                    "client_id": "c1",
                    "workspace_id": "w1",
                    "tags": ["rust", "clean"],
                }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CREATED, "create: {body}");
        assert_eq!(body["content"], json!("用户喜欢简洁的代码"));
        assert_eq!(body["scope_type"], json!("workspace"));
        assert_eq!(body["source_trigger"], json!("manual"));
        assert_eq!(body["confidence"], json!(1.0));
        assert_eq!(body["pinned"], json!(false));
        assert_eq!(body["tags"], json!(["rust", "clean"]));
        let id = body["id"].as_str().unwrap().to_string();

        // list（作用域 + workspace 过滤）→ {memories, total}
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                "/api/agent/memories?scope=workspace&workspace_id=w1".to_string(),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["memories"].as_array().unwrap().len(), 1);
        assert_eq!(body["total"], json!(1), "total 应反映过滤后总数");

        // get by id
        let (status, body) = call(
            &app,
            json_request(Method::GET, format!("/api/agent/memories/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["id"], json!(id));

        // update content/tags/scope/confidence
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                format!("/api/agent/memories/{id}"),
                &json!({
                    "content": "用户偏好简洁实现",
                    "tags": ["rust", "clean", "tunnel"],
                    "scope_type": "global",
                    "confidence": 0.85,
                }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "update: {body}");
        assert_eq!(body["content"], json!("用户偏好简洁实现"));
        assert_eq!(body["scope_type"], json!("global"));
        assert_eq!(body["confidence"], json!(0.85));
        assert_eq!(body["tags"], json!(["rust", "clean", "tunnel"]));

        // pin toggle
        let (status, _) = call(
            &app,
            json_request(Method::POST, format!("/api/agent/memories/{id}/pin"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (_, body) = call(
            &app,
            json_request(Method::GET, format!("/api/agent/memories/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(body["pinned"], json!(true));
        let (_, _) = call(
            &app,
            json_request(Method::POST, format!("/api/agent/memories/{id}/pin"), &json!(null)),
        )
        .await;
        let (_, body) = call(
            &app,
            json_request(Method::GET, format!("/api/agent/memories/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(body["pinned"], json!(false));

        // delete → 404
        let (status, _) = call(
            &app,
            json_request(Method::DELETE, format!("/api/agent/memories/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (status, _) = call(
            &app,
            json_request(Method::GET, format!("/api/agent/memories/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::NOT_FOUND);

        // clear
        for i in 0..3 {
            let (status, _) = call(
                &app,
                json_request(
                    Method::POST,
                    "/api/agent/memories".to_string(),
                    &json!({ "content": format!("fact {i}"), "scope_type": "global" }),
                ),
            )
            .await;
            assert_eq!(status, HttpStatus::CREATED);
        }
        let (status, _) = call(
            &app,
            json_request(Method::POST, "/api/agent/memory/clear".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (status, body) = call(
            &app,
            json_request(Method::GET, "/api/agent/memories".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["memories"].as_array().unwrap().len(), 0);
        assert_eq!(body["total"], json!(0), "clear 后 total 应为 0");
    }

    #[tokio::test]
    async fn memory_create_validation() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        // 空 content
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/memories".to_string(),
                &json!({ "content": "", "scope_type": "global" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // content 超限
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/memories".to_string(),
                &json!({ "content": "x".repeat(2049), "scope_type": "global" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 非法 scope
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/memories".to_string(),
                &json!({ "content": "fact", "scope_type": "bogus" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 缺 content 字段 → 422（serde 反序列化失败）
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/memories".to_string(),
                &json!({ "scope_type": "global" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn memories_embed_when_enabled() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        // 在建 router 前克隆 store（search 验证向量落库用）
        let store = state
            .server_state
            .agent_state
            .as_ref()
            .unwrap()
            .memory
            .as_ref()
            .unwrap()
            .store
            .clone();
        let app = test_router(state);

        // 启用 + emb 配置
        let (status, _) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({
                    "enabled": true,
                    "emb_base_url": base,
                    "emb_api_key": "sk-test",
                    "emb_model": "test-model",
                    "emb_dimension": 8,
                }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);

        // 创建 → 应写入向量（ChunkPoint id/doc_id = 记忆 id）
        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/memories".to_string(),
                &json!({ "content": "向量化测试事实", "scope_type": "global" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CREATED, "create with embed: {body}");
        let id = body["id"].as_str().unwrap().to_string();
        let hits = store.search(MEMORY_KB_ID, 8, &[0.1f32; 8], 10).await;
        assert!(
            hits.iter().any(|p| p.id == id),
            "memory {id} 应已写入向量（hits: {hits:?}）"
        );

        // content 变化 → 向量覆盖，检索仍命中同一 id
        let (status, _) = call(
            &app,
            json_request(
                Method::PUT,
                format!("/api/agent/memories/{id}"),
                &json!({ "content": "更新后的向量化事实", "scope_type": "global", "confidence": 0.9 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let hits = store.search(MEMORY_KB_ID, 8, &[0.1f32; 8], 10).await;
        assert!(hits.iter().any(|p| p.id == id), "更新后向量应仍指向 {id}");
    }

    #[tokio::test]
    async fn memory_unconfigured_returns_503() {
        // 不注入记忆运行时：agent.memory = None → 503
        let db = Database::new(":memory:").await.unwrap();
        let state = ApiState {
            server_state: ServerState::with_db(db),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let app = Router::new()
            .route("/api/agent/memory/settings", get(super::get_settings))
            .with_state(state);
        let (status, _) = call(
            &app,
            json_request(Method::GET, "/api/agent/memory/settings".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_embedding_route_reuses_rag_handler() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);
        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/memory/settings/test-embedding".to_string(),
                &json!({ "base_url": base, "api_key": "k", "model": "m" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "test-embedding: {body}");
        assert_eq!(body["dimension"].as_i64(), Some(8));
        assert!(body["latency_ms"].is_u64());
    }

    #[tokio::test]
    async fn sse_events_endpoint_returns_stream() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);
        // 只断言响应头：SSE body 为长流，读 body 会挂起。
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/agent/memory/events")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router responds");
        assert_eq!(resp.status(), HttpStatus::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
    }

    #[tokio::test]
    async fn distill_session_manual_resets_and_retriggers() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        // 先建 workspace + session（agent_sessions 有到 agent_workspaces 的 FK）
        db.agent_create_workspace("w1", "w", "c1", "host", "/tmp", None, None, "", None, None, None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None).await.unwrap();
        // 启用记忆（trigger_distill 的 enabled 检查通过）
        let mut s = AgentMemorySettingsRecord::default_disabled();
        s.enabled = 1;
        s.emb_base_url = "http://localhost:11434/v1".to_string();
        s.emb_api_key = "encrypted-key".to_string();
        s.emb_model = "m".to_string();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();
        // 标记已蒸馏 → 手动重跑应复位 distilled=0 后再次 CAS 标记回 1
        assert!(db.memory_mark_distilled_if_not("s1").await.unwrap());

        let app = test_router(state);
        let (status, body) = call(
            &app,
            json_request(Method::POST, "/api/agent/sessions/s1/distill".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "manual distill: {body}");

        let v: i64 = sqlx::query_scalar("SELECT distilled FROM agent_sessions WHERE id = 's1'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(v, 1, "复位后 trigger 内部 CAS 应再标记 distilled=1");

        // 不存在的会话 → 404
        let (status, _) = call(
            &app,
            json_request(Method::POST, "/api/agent/sessions/nope/distill".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::NOT_FOUND);
    }
}
