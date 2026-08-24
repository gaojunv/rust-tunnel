//! Skill 库管理 REST API（Chat Memory 二期）：settings（skill_enabled/skill_list_max
//! 在 memory.rs 的 settings 端点）之外，skills CRUD / 启停切换。仅 `rag` feature 编译。
//!
//! Skill **不向量化**（纯 SQLite + SQL），创建/编辑走 `upsert_skill_with_dedup` /
//! `skill_update` DAO；列表响应**不含 content**（content 最大 16KB，详情端点才返回）。
//!
//! 路由由本模块的 `protected_router` 组装，在 `mgmt/api/mod.rs` 的 rag 合并块中挂载
//! （位于 JWT layer 之前），与 AI 记忆体路由同位置。

#![cfg(feature = "rag")]

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::agent::memory::scope_coords;
use crate::agent::skill::{
    SKILL_CONTENT_MAX_CHARS, SKILL_DESCRIPTION_MAX_CHARS, SKILL_NAME_MAX_CHARS,
};
use crate::db::agent::normalize_db_datetime;
use crate::db::skills::{AgentSkillRecord, AgentSkillSummary};
use crate::mgmt::api::ApiState;

use super::mem_runtime;
use super::memory::validate_tags;

const VALID_SCOPES: [&str; 3] = ["global", "client", "workspace"];

// ── 请求体 DTO ──────────────────────────────────────────────────

/// GET /api/agent/skills 的 query 参数。`limit` 默认 50（handler 层 clamp 到 [1, 200]）。
#[derive(Debug, Default, Deserialize)]
pub struct ListSkillsParams {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// POST /api/agent/skills 请求体。`name`/`content` 必填；`scope_type` 缺省/空串 →
/// workspace。手动创建走去重 upsert（同作用域同名 → 更新既有），`source_trigger`
/// 固定 'manual'。
#[derive(Debug, Deserialize)]
pub struct CreateSkillRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
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

/// PUT /api/agent/skills/:id 请求体（部分更新：缺省字段沿用当前值）。
#[derive(Debug, Default, Deserialize)]
pub struct UpdateSkillRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub scope_type: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

// ── 视图 JSON ────────────────────────────────────────────────────

/// 完整视图（含 content 全文，详情端点用）。
fn skill_json(s: &AgentSkillRecord) -> serde_json::Value {
    let tags: Vec<String> = serde_json::from_str(&s.tags).unwrap_or_default();
    let last_used_at = s.last_used_at.as_ref().map(|t| normalize_db_datetime(t));
    serde_json::json!({
        "id": s.id,
        "name": s.name,
        "description": s.description,
        "content": s.content,
        "scope_type": s.scope_type,
        "client_id": s.client_id,
        "workspace_id": s.workspace_id,
        "tags": tags,
        "enabled": s.enabled != 0,
        "source_session_id": s.source_session_id,
        "source_trigger": s.source_trigger,
        "use_count": s.use_count,
        "last_used_at": last_used_at,
        "created_at": normalize_db_datetime(&s.created_at),
        "updated_at": normalize_db_datetime(&s.updated_at),
    })
}

/// 列表视图（**不含 content**，避免大字段拖慢列表响应）。
fn skill_summary_json(s: &AgentSkillSummary) -> serde_json::Value {
    let tags: Vec<String> = serde_json::from_str(&s.tags).unwrap_or_default();
    let last_used_at = s.last_used_at.as_ref().map(|t| normalize_db_datetime(t));
    serde_json::json!({
        "id": s.id,
        "name": s.name,
        "description": s.description,
        "scope_type": s.scope_type,
        "client_id": s.client_id,
        "workspace_id": s.workspace_id,
        "tags": tags,
        "enabled": s.enabled != 0,
        "source_session_id": s.source_session_id,
        "source_trigger": s.source_trigger,
        "use_count": s.use_count,
        "last_used_at": last_used_at,
        "created_at": normalize_db_datetime(&s.created_at),
        "updated_at": normalize_db_datetime(&s.updated_at),
    })
}

// ── 校验 ─────────────────────────────────────────────────────────

/// 校验 name（非空 + ≤64）与 content（非空 + ≤16KB）。
fn validate_name_and_content(name: &str, content: &str) -> Result<(), (StatusCode, String)> {
    let name = name.trim();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required".to_string()));
    }
    if name.chars().count() > SKILL_NAME_MAX_CHARS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("name must be at most {SKILL_NAME_MAX_CHARS} chars"),
        ));
    }
    let content = content.trim();
    if content.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "content is required".to_string()));
    }
    if content.chars().count() > SKILL_CONTENT_MAX_CHARS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("content must be at most {SKILL_CONTENT_MAX_CHARS} chars"),
        ));
    }
    Ok(())
}

/// 校验 description（≤200）与 scope_type（global|client|workspace）。
fn validate_description_and_scope(
    description: &str,
    scope_type: &str,
) -> Result<(), (StatusCode, String)> {
    if description.chars().count() > SKILL_DESCRIPTION_MAX_CHARS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("description must be at most {SKILL_DESCRIPTION_MAX_CHARS} chars"),
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

// ── Handlers ─────────────────────────────────────────────────────

/// 与 DAO `skill_list` 相同过滤条件的 COUNT 查询（列表响应壳需要 total）。
async fn count_skills(
    db: &crate::db::Database,
    params: &ListSkillsParams,
) -> Result<i64, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new("SELECT COUNT(*) FROM agent_skills WHERE 1=1");
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
        qb.push(" AND (name LIKE ")
            .push_bind(format!("%{q}%"))
            .push(" OR description LIKE ")
            .push_bind(format!("%{q}%"))
            .push(")");
    }
    if let Some(true) = params.enabled {
        qb.push(" AND enabled = 1");
    } else if let Some(false) = params.enabled {
        qb.push(" AND enabled = 0");
    }
    qb.build_query_scalar::<i64>().fetch_one(&db.pool).await
}

/// GET /api/agent/skills — 列表（作用域/client/workspace/q/enabled 过滤 + 排序 +
/// 分页）。返回 `{skills, total}`；**list 不含 content**。
pub async fn list_skills(
    State(state): State<ApiState>,
    Query(params): Query<ListSkillsParams>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let rows = match mem
        .db
        .skill_list(
            params.scope.as_deref(),
            params.client_id.as_deref(),
            params.workspace_id.as_deref(),
            params.q.as_deref(),
            params.enabled,
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
    let total = match count_skills(&mem.db, &params).await {
        Ok(n) => n,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let skills: Vec<serde_json::Value> = rows.iter().map(skill_summary_json).collect();
    Json(serde_json::json!({ "skills": skills, "total": total })).into_response()
}

/// POST /api/agent/skills — 手动新建/去重更新。固定 `source_trigger='manual'`；
/// 与蒸馏共用 `upsert_skill_with_dedup`（同作用域同名 → 更新 content/description、
/// tags 并集、enabled/use_count 保持）。返回 201 完整含 content。
pub async fn create_skill(
    State(state): State<ApiState>,
    Json(body): Json<CreateSkillRequest>,
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
    let name = body.name.trim();
    let content = body.content.trim();
    let description = body.description.trim();
    if let Err(e) = validate_name_and_content(name, content) {
        return e.into_response();
    }
    if let Err(e) = validate_description_and_scope(description, scope) {
        return e.into_response();
    }
    if let Err(e) = validate_tags(&body.tags) {
        return e.into_response();
    }
    let (scope_type, client_id, workspace_id) =
        scope_coords(scope, &body.client_id, &body.workspace_id);
    match crate::agent::skill::upsert_skill_with_dedup(
        &mem,
        name,
        description,
        content,
        &scope_type,
        &client_id,
        &workspace_id,
        &body.tags,
        "",
        "manual",
    )
    .await
    {
        Ok(id) => match mem.db.skill_get_by_id(&id).await {
            Ok(Some(s)) => (StatusCode::CREATED, Json(skill_json(&s))).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// GET /api/agent/skills/:id — 详情（含 content 全文）。
pub async fn get_skill(State(state): State<ApiState>, Path(id): Path<String>) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    match mem.db.skill_get_by_id(&id).await {
        Ok(Some(s)) => Json(skill_json(&s)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// PUT /api/agent/skills/:id — 部分更新（name/description/content/tags/scope）。
/// scope 变更走 `scope_coords` 归一化坐标；name 归一化落库。
pub async fn update_skill(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSkillRequest>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    let existing = match mem.db.skill_get_by_id(&id).await {
        Ok(Some(s)) => s,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .unwrap_or(&existing.name);
    let content = body
        .content
        .as_deref()
        .map(str::trim)
        .unwrap_or(&existing.content);
    let description = body
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or(&existing.description);
    let scope = body
        .scope_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&existing.scope_type);
    if let Err(e) = validate_name_and_content(name, content) {
        return e.into_response();
    }
    if let Err(e) = validate_description_and_scope(description, scope) {
        return e.into_response();
    }
    // 缺省 tags 沿用既有（DB 存 JSON 数组字符串，解析回 Vec）
    let existing_tags: Vec<String> = serde_json::from_str(&existing.tags).unwrap_or_default();
    let tags = match &body.tags {
        Some(t) => {
            if let Err(e) = validate_tags(t) {
                return e.into_response();
            }
            t
        }
        None => &existing_tags,
    };
    let base_client = body.client_id.as_deref().unwrap_or(&existing.client_id);
    let base_workspace = body
        .workspace_id
        .as_deref()
        .unwrap_or(&existing.workspace_id);
    let (scope_type, client_id, workspace_id) = scope_coords(scope, base_client, base_workspace);
    let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string());
    let normalized_name = crate::agent::skill::normalize_skill_name(name);
    if let Err(e) = mem
        .db
        .skill_update(
            &id,
            &normalized_name,
            description,
            content,
            &tags_json,
            &scope_type,
            &client_id,
            &workspace_id,
        )
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    match mem.db.skill_get_by_id(&id).await {
        Ok(Some(s)) => Json(skill_json(&s)).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// POST /api/agent/skills/:id/toggle — 启停切换（1 ↔ 0）。
pub async fn toggle_skill(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    match mem.db.skill_get_by_id(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }
    match mem.db.skill_toggle_enabled(&id).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// DELETE /api/agent/skills/:id — 删除。
pub async fn delete_skill(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mem = match mem_runtime(&state) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    match mem.db.skill_get_by_id(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }
    match mem.db.skill_delete(&id).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

// ── 路由 ─────────────────────────────────────────────────────────

/// 受保护路由（JWT 覆盖）。挂在 `mgmt/api/mod.rs` 的 rag 合并块。
pub fn protected_router() -> Router<ApiState> {
    Router::new()
        .route("/api/agent/skills", get(list_skills).post(create_skill))
        .route(
            "/api/agent/skills/:id",
            get(get_skill).put(update_skill).delete(delete_skill),
        )
        .route("/api/agent/skills/:id/toggle", post(toggle_skill))
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

    /// 构造 ApiState（照抄 memory.rs 测试：内存 DB + RAG 目录 + 固定测试主密钥 +
    /// 注入记忆运行时）。
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
                .clone();
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

    /// 覆盖本模块全部路由的测试 Router（免 JWT）。
    fn test_router(state: ApiState) -> Router {
        super::protected_router().with_state(state)
    }

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

    #[tokio::test]
    async fn skills_crud_toggle_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        // create（workspace 作用域）
        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({
                    "name": "发布应用",
                    "description": "发布前检查与回滚",
                    "content": "## 发布步骤\n1. build\n2. deploy",
                    "scope_type": "workspace",
                    "client_id": "c1",
                    "workspace_id": "w1",
                    "tags": ["deploy", "linux"],
                }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CREATED, "create: {body}");
        assert_eq!(body["name"], json!("发布应用"));
        assert_eq!(body["content"], json!("## 发布步骤\n1. build\n2. deploy"));
        assert_eq!(body["scope_type"], json!("workspace"));
        assert_eq!(body["source_trigger"], json!("manual"));
        assert_eq!(body["enabled"], json!(true));
        assert_eq!(body["use_count"], json!(0));
        assert_eq!(body["tags"], json!(["deploy", "linux"]));
        let id = body["id"].as_str().unwrap().to_string();

        // 同作用域同名再 POST → 去重更新（upsert 语义），仍 201
        let (status, body2) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({
                    "name": "发布应用",
                    "description": "新描述",
                    "content": "新内容",
                    "scope_type": "workspace",
                    "client_id": "c1",
                    "workspace_id": "w1",
                }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CREATED);
        assert_eq!(body2["id"], json!(id), "同作用域同名应更新既有");
        assert_eq!(body2["content"], json!("新内容"));
        assert_eq!(body2["tags"], json!(["deploy", "linux"]), "tags 并集保留");

        // list：不含 content，{skills, total}
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                "/api/agent/skills?scope=workspace&workspace_id=w1".to_string(),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["skills"].as_array().unwrap().len(), 1);
        assert_eq!(body["total"], json!(1));
        assert!(
            body["skills"][0].get("content").is_none(),
            "list 不应包含 content"
        );

        // get by id：含 content
        let (status, body) = call(
            &app,
            json_request(Method::GET, format!("/api/agent/skills/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["content"], json!("新内容"));

        // update（部分：仅 scope 变更到 global；name 归一化）
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                format!("/api/agent/skills/{id}"),
                &json!({ "scope_type": "global" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "update: {body}");
        assert_eq!(body["scope_type"], json!("global"));
        assert_eq!(body["client_id"], json!(""), "global 坐标清空");
        assert_eq!(body["content"], json!("新内容"), "缺省字段沿用当前值");

        // toggle
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/agent/skills/{id}/toggle"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (_, body) = call(
            &app,
            json_request(Method::GET, format!("/api/agent/skills/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(body["enabled"], json!(false));
        // enabled=false 过滤
        let (_, body) = call(
            &app,
            json_request(
                Method::GET,
                "/api/agent/skills?enabled=false".to_string(),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(body["total"], json!(1));

        // delete → 404
        let (status, _) = call(
            &app,
            json_request(
                Method::DELETE,
                format!("/api/agent/skills/{id}"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (status, _) = call(
            &app,
            json_request(Method::GET, format!("/api/agent/skills/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::NOT_FOUND);
    }

    #[tokio::test]
    async fn skill_create_validation() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_router(test_api_state(dir.path()).await);

        // 空 name
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({ "name": "  ", "content": "x" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // name 超 64
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({ "name": "x".repeat(65), "content": "x" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 空 content
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({ "name": "deploy", "content": "  " }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // content 超 16KB
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({ "name": "deploy", "content": "x".repeat(SKILL_CONTENT_MAX_CHARS + 1) }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // description 超 200
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({ "name": "deploy", "content": "x", "description": "d".repeat(201) }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 非法 scope
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({ "name": "deploy", "content": "x", "scope_type": "bogus" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // tags 超量 / 超长
        let many: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({ "name": "deploy", "content": "x", "tags": many }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 缺 name/content 字段 → 422（serde 反序列化失败）
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/skills".to_string(),
                &json!({ "description": "x" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn skill_unconfigured_returns_503() {
        // 不注入记忆运行时：agent.memory = None → 503
        let db = Database::new(":memory:").await.unwrap();
        let state = ApiState {
            server_state: ServerState::with_db(db),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let app = Router::new()
            .route("/api/agent/skills", get(super::list_skills))
            .with_state(state);
        let (status, _) = call(
            &app,
            json_request(Method::GET, "/api/agent/skills".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::SERVICE_UNAVAILABLE);
    }
}
