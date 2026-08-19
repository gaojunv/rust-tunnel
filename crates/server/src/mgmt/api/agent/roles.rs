//! Agent 角色管理 REST API：CRUD + toggle + scope 可见性。
//! 角色定义系统提示词、工具白名单/黑名单、模型覆盖，供 subagent 调度和
//! 主会话角色切换使用。无 rag feature 依赖，无条件编译。

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};

use crate::db::roles::AgentRoleRecord;
use crate::mgmt::api::ApiState;

use super::dto::{CreateRoleRequest, ListRolesParams, UpdateRoleRequest};
use super::new_id;

const VALID_SCOPES: [&str; 3] = ["global", "client", "workspace"];
const VALID_MODES: [&str; 3] = ["subagent", "primary", "all"];

/// 合法工具名集合（以 tools.rs 实际注册的工具名为准）。
/// 注意：`read_file_range` 不是独立工具（是 read_file 的行区间参数变体），不在此列。
const VALID_TOOL_NAMES: &[&str] = &[
    "shell",
    "read_file",
    "write_file",
    "patch_file",
    "edit_file",
    "list_dir",
    "search",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
    "git_branch",
    "git_commit",
    "git_push",
    "git_stage",
    "git_unstage",
    "git_checkout",
    "git_pull",
    "git_revert",
    "git_reset",
    "git_stash",
    "code_outline",
    "read_symbol",
    "task",
    "todo_write",
    "remember",
    "use_skill",
];

// ── 校验 ─────────────────────────────────────────────────────────

/// 校验 name 为合法 kebab-case（小写字母/数字/短横线，非空，≤64 字符）。
fn validate_name(name: &str) -> Result<(), (StatusCode, String)> {
    let name = name.trim();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name is required".to_string()));
    }
    if name.chars().count() > 64 {
        return Err((
            StatusCode::BAD_REQUEST,
            "name must be at most 64 chars".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "name must be kebab-case (lowercase letters, digits, hyphens only)".to_string(),
        ));
    }
    // 不得以短横线开头或结尾
    if name.starts_with('-') || name.ends_with('-') {
        return Err((
            StatusCode::BAD_REQUEST,
            "name must not start or end with a hyphen".to_string(),
        ));
    }
    Ok(())
}

/// 校验 mode 值。
fn validate_mode(mode: &str) -> Result<(), (StatusCode, String)> {
    if !VALID_MODES.contains(&mode) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("mode must be one of: {}", VALID_MODES.join(", ")),
        ));
    }
    Ok(())
}

/// 校验 scope_type 值。
fn validate_scope(scope: &str) -> Result<(), (StatusCode, String)> {
    if !VALID_SCOPES.contains(&scope) {
        return Err((
            StatusCode::BAD_REQUEST,
            "scope_type must be one of: global, client, workspace".to_string(),
        ));
    }
    Ok(())
}

/// 校验工具名列表：每个元素必须是合法工具名。
fn validate_tool_list(
    tools: &[String],
    field_name: &str,
) -> Result<(), (StatusCode, String)> {
    for tool in tools {
        let t = tool.trim();
        if t.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{field_name} contains empty tool name"),
            ));
        }
        if !VALID_TOOL_NAMES.contains(&t) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "{field_name} contains invalid tool name: '{t}'. Valid names: {}",
                    VALID_TOOL_NAMES.join(", ")
                ),
            ));
        }
    }
    Ok(())
}

/// scope 坐标归一化：global → (global, "", "")。
fn scope_coords(scope: &str, client_id: &str, workspace_id: &str) -> (String, String, String) {
    match scope {
        "global" => ("global".to_string(), String::new(), String::new()),
        "client" => (
            "client".to_string(),
            client_id.to_string(),
            String::new(),
        ),
        "workspace" => (
            "workspace".to_string(),
            client_id.to_string(),
            workspace_id.to_string(),
        ),
        _ => (
            scope.to_string(),
            client_id.to_string(),
            workspace_id.to_string(),
        ),
    }
}

/// 校验创建/更新请求的公共字段。
fn validate_role_fields(
    name: &str,
    description: &str,
    scope: &str,
    mode: &str,
    tools_allow: Option<&[String]>,
    tools_deny: Option<&[String]>,
) -> Result<(), (StatusCode, String)> {
    validate_name(name)?;
    validate_scope(scope)?;
    validate_mode(mode)?;
    if description.chars().count() > 500 {
        return Err((
            StatusCode::BAD_REQUEST,
            "description must be at most 500 chars".to_string(),
        ));
    }
    if let Some(tools) = tools_allow {
        validate_tool_list(tools, "tools_allow")?;
    }
    if let Some(tools) = tools_deny {
        validate_tool_list(tools, "tools_deny")?;
    }
    Ok(())
}

// ── JSON 视图 ────────────────────────────────────────────────────

/// 角色 JSON 视图（含全字段）。
fn role_json(r: &AgentRoleRecord) -> serde_json::Value {
    let tools_allow: Option<Vec<String>> =
        r.tools_allow.as_deref().and_then(|s| serde_json::from_str(s).ok());
    let tools_deny: Option<Vec<String>> =
        r.tools_deny.as_deref().and_then(|s| serde_json::from_str(s).ok());
    serde_json::json!({
        "id": r.id,
        "name": r.name,
        "description": r.description,
        "system_prompt": r.system_prompt,
        "tools_allow": tools_allow,
        "tools_deny": tools_deny,
        "model_override": r.model_override,
        "mode": r.mode,
        "scope_type": r.scope_type,
        "client_id": r.client_id,
        "workspace_id": r.workspace_id,
        "is_builtin": r.is_builtin != 0,
        "enabled": r.enabled != 0,
        "created_at": crate::db::agent::normalize_db_datetime(&r.created_at),
        "updated_at": crate::db::agent::normalize_db_datetime(&r.updated_at),
    })
}

// ── COUNT 辅助 ──────────────────────────────────────────────────

/// 与 DAO `role_list` 相同过滤条件的 COUNT 查询。
async fn count_roles(
    db: &crate::db::Database,
    params: &ListRolesParams,
) -> Result<i64, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new("SELECT COUNT(*) FROM agent_roles WHERE 1=1");
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
    if let Some(m) = params.mode.as_deref().filter(|m| !m.is_empty()) {
        qb.push(" AND mode = ").push_bind(m);
    }
    qb.build_query_scalar::<i64>()
        .fetch_one(&db.pool)
        .await
}

// ── Handlers ─────────────────────────────────────────────────────

/// GET /api/agent/roles — 列表（scope/client/workspace/q/enabled/mode 过滤 + 分页）。
/// 返回 `{roles, total}`。
pub async fn list_roles(
    State(state): State<ApiState>,
    Query(params): Query<ListRolesParams>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let rows = match agent
        .db
        .role_list(
            params.scope.as_deref(),
            params.client_id.as_deref(),
            params.workspace_id.as_deref(),
            params.q.as_deref(),
            params.enabled,
            params.mode.as_deref(),
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
    let total = match count_roles(&agent.db, &params).await {
        Ok(n) => n,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let roles: Vec<serde_json::Value> = rows.iter().map(role_json).collect();
    Json(serde_json::json!({ "roles": roles, "total": total })).into_response()
}

/// POST /api/agent/roles — 创建角色。
pub async fn create_role(
    State(state): State<ApiState>,
    Json(body): Json<CreateRoleRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let name = body.name.trim();
    let description = body.description.trim();
    let system_prompt = body.system_prompt.trim();
    let mode = if body.mode.trim().is_empty() {
        "all"
    } else {
        body.mode.trim()
    };
    let scope = if body.scope_type.trim().is_empty() {
        "global"
    } else {
        body.scope_type.trim()
    };

    if let Err(e) = validate_role_fields(
        name,
        description,
        scope,
        mode,
        body.tools_allow.as_deref(),
        body.tools_deny.as_deref(),
    ) {
        return e.into_response();
    }

    let (scope_type, client_id, workspace_id) = scope_coords(scope, &body.client_id, &body.workspace_id);

    // 唯一约束校验（提前报 409 而非 DB error）
    if let Ok(Some(_)) = agent
        .db
        .role_get_by_name_scope(name, &scope_type, &client_id, &workspace_id)
        .await
    {
        return (
            StatusCode::CONFLICT,
            format!("role '{name}' already exists in this scope"),
        )
            .into_response();
    }

    let id = new_id();
    let tools_allow_json = body
        .tools_allow
        .as_ref()
        .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".to_string()));
    let tools_deny_json = body
        .tools_deny
        .as_ref()
        .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".to_string()));

    match agent
        .db
        .role_insert(
            &id,
            name,
            description,
            system_prompt,
            tools_allow_json.as_deref(),
            tools_deny_json.as_deref(),
            body.model_override.as_deref(),
            mode,
            &scope_type,
            &client_id,
            &workspace_id,
        )
        .await
    {
        Ok(()) => match agent.db.role_get_by_id(&id).await {
            Ok(Some(r)) => (StatusCode::CREATED, Json(role_json(&r))).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// GET /api/agent/roles/:id — 详情。
pub async fn get_role(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.role_get_by_id(&id).await {
        Ok(Some(r)) => Json(role_json(&r)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// PUT /api/agent/roles/:id — 部分更新。内置角色不可改名。
pub async fn update_role(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRoleRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let existing = match agent.db.role_get_by_id(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };

    let name = body.name.as_deref().map(str::trim).unwrap_or(&existing.name);
    let description = body
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or(&existing.description);
    let system_prompt = body
        .system_prompt
        .as_deref()
        .map(str::trim)
        .unwrap_or(&existing.system_prompt);
    let mode = body
        .mode
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(&existing.mode);
    let scope = body
        .scope_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&existing.scope_type);
    let base_client = body.client_id.as_deref().unwrap_or(&existing.client_id);
    let base_workspace = body
        .workspace_id
        .as_deref()
        .unwrap_or(&existing.workspace_id);

    // tools_allow / tools_deny：Some 使用新值，None 沿用旧值
    let default_allow: Vec<String> = existing
        .tools_allow
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let default_deny: Vec<String> = existing
        .tools_deny
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let tools_allow = body.tools_allow.as_deref().unwrap_or(&default_allow);
    let tools_deny = body.tools_deny.as_deref().unwrap_or(&default_deny);
    let model_override = body.model_override.as_deref().or(existing.model_override.as_deref());

    if let Err(e) = validate_role_fields(
        name,
        description,
        scope,
        mode,
        Some(tools_allow),
        Some(tools_deny),
    ) {
        return e.into_response();
    }

    // 内置角色不可改名
    if existing.is_builtin != 0 && name != existing.name {
        return (
            StatusCode::FORBIDDEN,
            "builtin role name cannot be changed".to_string(),
        )
            .into_response();
    }

    let (scope_type, client_id, workspace_id) = scope_coords(scope, base_client, base_workspace);

    // scope/name 变更时唯一约束校验
    if name != existing.name
        || scope_type != existing.scope_type
        || client_id != existing.client_id
        || workspace_id != existing.workspace_id
    {
        if let Ok(Some(_)) = agent
            .db
            .role_get_by_name_scope(name, &scope_type, &client_id, &workspace_id)
            .await
        {
            return (
                StatusCode::CONFLICT,
                format!("role '{name}' already exists in this scope"),
            )
                .into_response();
        }
    }

    let tools_allow_json = serde_json::to_string(tools_allow).unwrap_or_else(|_| "[]".to_string());
    let tools_deny_json = serde_json::to_string(tools_deny).unwrap_or_else(|_| "[]".to_string());

    if let Err(e) = agent
        .db
        .role_update(
            &id,
            name,
            description,
            system_prompt,
            Some(&tools_allow_json),
            Some(&tools_deny_json),
            model_override,
            mode,
            &scope_type,
            &client_id,
            &workspace_id,
        )
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    match agent.db.role_get_by_id(&id).await {
        Ok(Some(r)) => Json(role_json(&r)).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// DELETE /api/agent/roles/:id — 删除。内置角色不可删除。
pub async fn delete_role(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let existing = match agent.db.role_get_by_id(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    if existing.is_builtin != 0 {
        return (
            StatusCode::FORBIDDEN,
            "builtin role cannot be deleted".to_string(),
        )
            .into_response();
    }
    match agent.db.role_delete(&id).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

/// PATCH /api/agent/roles/:id/toggle — 启停切换。
pub async fn toggle_role(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.role_get_by_id(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }
    match agent.db.role_toggle_enabled(&id).await {
        Ok(()) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response(),
    }
}

// ── 路由 ─────────────────────────────────────────────────────────

/// 受保护路由（JWT 覆盖）。无 rag feature 依赖，无条件编译。
pub fn protected_router() -> Router<ApiState> {
    Router::new()
        .route("/api/agent/roles", get(list_roles).post(create_role))
        .route(
            "/api/agent/roles/:id",
            get(get_role).put(update_role).delete(delete_role),
        )
        .route("/api/agent/roles/:id/toggle", post(toggle_role))
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode as HttpStatus};
    use axum::Router;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::auth::AuthConfig;
    use crate::control::ServerState;
    use crate::db::Database;

    /// 构造 ApiState（内存 DB + agent_state）。
    async fn test_api_state() -> ApiState {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let server_state = ServerState::with_db(db);
        ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        }
    }

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

    fn json_request(method: Method, uri: &str, body: &Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request")
    }

    #[tokio::test]
    async fn roles_crud_toggle_and_validation() {
        let app = test_router(test_api_state().await);

        // create
        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/roles",
                &json!({
                    "name": "code-reviewer",
                    "description": "代码审查",
                    "system_prompt": "Review code",
                    "tools_allow": ["read_file", "search"],
                    "mode": "subagent",
                    "scope_type": "workspace",
                    "client_id": "c1",
                    "workspace_id": "w1",
                }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CREATED, "create: {body}");
        assert_eq!(body["name"], json!("code-reviewer"));
        assert_eq!(body["mode"], json!("subagent"));
        assert_eq!(body["scope_type"], json!("workspace"));
        assert_eq!(body["is_builtin"], json!(false));
        assert_eq!(body["enabled"], json!(true));
        let id = body["id"].as_str().unwrap().to_string();

        // get
        let (status, body) = call(
            &app,
            json_request(Method::GET, &format!("/api/agent/roles/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["name"], json!("code-reviewer"));

        // list
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                "/api/agent/roles?scope=workspace&workspace_id=w1",
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["total"], json!(1));
        assert_eq!(body["roles"][0]["name"], json!("code-reviewer"));

        // update（仅改 description）
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                &format!("/api/agent/roles/{id}"),
                &json!({ "description": "新描述" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "update: {body}");
        assert_eq!(body["description"], json!("新描述"));

        // toggle
        let (status, _) = call(
            &app,
            json_request(Method::POST, &format!("/api/agent/roles/{id}/toggle"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (_, body) = call(
            &app,
            json_request(Method::GET, &format!("/api/agent/roles/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(body["enabled"], json!(false));

        // delete
        let (status, _) = call(
            &app,
            json_request(Method::DELETE, &format!("/api/agent/roles/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (status, _) = call(
            &app,
            json_request(Method::GET, &format!("/api/agent/roles/{id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::NOT_FOUND);
    }

    #[tokio::test]
    async fn role_create_validation() {
        let app = test_router(test_api_state().await);

        // 空 name
        let (status, _) = call(
            &app,
            json_request(Method::POST, "/api/agent/roles", &json!({ "name": "  " })),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 非 kebab-case name（大写）
        let (status, _) = call(
            &app,
            json_request(Method::POST, "/api/agent/roles", &json!({ "name": "MyRole" })),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // name 以短横线开头
        let (status, _) = call(
            &app,
            json_request(Method::POST, "/api/agent/roles", &json!({ "name": "-bad" })),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 非法 mode
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/roles",
                &json!({ "name": "ok", "mode": "bogus" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 非法 scope
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/roles",
                &json!({ "name": "ok", "scope_type": "bogus" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        // 非法工具名
        let (status, _) = call(
            &app,
            json_request(
                Method::POST,
                "/api/agent/roles",
                &json!({ "name": "ok", "tools_allow": ["read_file", "not_a_tool"] }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
    }

    #[tokio::test]
    async fn role_builtin_protected() {
        let app = test_router(test_api_state().await);

        // 内置角色（seed 后）存在
        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                "/api/agent/roles?scope=global",
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let roles = body["roles"].as_array().unwrap();
        let general = roles.iter().find(|r| r["name"] == "general").unwrap();
        let general_id = general["id"].as_str().unwrap();
        assert_eq!(general["is_builtin"], json!(true));

        // 删除内置角色 → 403
        let (status, _) = call(
            &app,
            json_request(
                Method::DELETE,
                &format!("/api/agent/roles/{general_id}"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::FORBIDDEN);

        // 改名内置角色 → 403
        let (status, _) = call(
            &app,
            json_request(
                Method::PUT,
                &format!("/api/agent/roles/{general_id}"),
                &json!({ "name": "renamed" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::FORBIDDEN);

        // 改 prompt/tools 允许
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                &format!("/api/agent/roles/{general_id}"),
                &json!({ "system_prompt": "New prompt" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "改 prompt 应允许: {body}");
        assert_eq!(body["system_prompt"], json!("New prompt"));
    }

    #[tokio::test]
    async fn role_duplicate_name_in_scope() {
        let app = test_router(test_api_state().await);

        let body = json!({ "name": "my-role", "scope_type": "global" });
        let (status, _) = call(&app, json_request(Method::POST, "/api/agent/roles", &body)).await;
        assert_eq!(status, HttpStatus::CREATED);

        // 同名同 scope → 409
        let (status, _) = call(&app, json_request(Method::POST, "/api/agent/roles", &body)).await;
        assert_eq!(status, HttpStatus::CONFLICT);

        // 不同 scope → 允许
        let body2 = json!({ "name": "my-role", "scope_type": "client", "client_id": "c1" });
        let (status, _) = call(&app, json_request(Method::POST, "/api/agent/roles", &body2)).await;
        assert_eq!(status, HttpStatus::CREATED);
    }
}
