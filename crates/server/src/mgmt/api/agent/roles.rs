//! Agent 角色管理 REST API：CRUD + toggle + scope 可见性。
//! 角色定义系统提示词、工具白名单/黑名单、模型覆盖，供 subagent 调度和
//! 主会话角色切换使用。无 rag feature 依赖，无条件编译。
//!
//! 薄壳 handler：校验/视图/坐标归一化在 `rust_tunnel_agent::roles`（service 层），
//! 分页 COUNT 在 `db::roles::role_count`（DAO 层）；本文件只做 DTO 拆解与
//! ApiError 映射。

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};

use crate::agent::roles as role_service;
use crate::mgmt::api::error::{ApiError, ApiResult};
use crate::mgmt::api::ApiState;

use super::dto::{CreateRoleRequest, ListRolesParams, UpdateRoleRequest};
use super::new_id;

// ── Handlers ─────────────────────────────────────────────────────

/// GET /api/agent/roles — 列表（scope/client/workspace/q/enabled/mode 过滤 + 分页）。
/// 返回 `{roles, total}`。
///
/// # Errors
/// 503 agent 未初始化；500 DB 错误。
pub async fn list_roles(
    State(state): State<ApiState>,
    Query(params): Query<ListRolesParams>,
) -> ApiResult<impl IntoResponse> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err(ApiError::unavailable("agent workbench not initialized"));
    };
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let rows = agent
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
        .map_err(|e| ApiError::db(&e))?;
    let total = agent
        .db
        .role_count(
            params.scope.as_deref(),
            params.client_id.as_deref(),
            params.workspace_id.as_deref(),
            params.q.as_deref(),
            params.enabled,
            params.mode.as_deref(),
        )
        .await
        .map_err(|e| ApiError::db(&e))?;
    let roles: Vec<serde_json::Value> = rows.iter().map(role_service::role_json).collect();
    Ok(Json(serde_json::json!({ "roles": roles, "total": total })))
}

/// POST /api/agent/roles — 创建角色。
///
/// # Errors
/// 400 字段校验失败；409 同 scope 重名；503 agent 未初始化；500 DB 错误。
pub async fn create_role(
    State(state): State<ApiState>,
    Json(body): Json<CreateRoleRequest>,
) -> ApiResult<impl IntoResponse> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err(ApiError::unavailable("agent workbench not initialized"));
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

    role_service::validate_role_fields(
        name,
        description,
        scope,
        mode,
        body.tools_allow.as_deref(),
        body.tools_deny.as_deref(),
    )
    .map_err(ApiError::bad_request)?;

    let (scope_type, client_id, workspace_id) =
        role_service::scope_coords(scope, &body.client_id, &body.workspace_id);

    // 唯一约束校验（提前报 409 而非 DB error）
    if let Ok(Some(_)) = agent
        .db
        .role_get_by_name_scope(name, &scope_type, &client_id, &workspace_id)
        .await
    {
        return Err(ApiError::conflict(format!(
            "role '{name}' already exists in this scope"
        )));
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

    agent
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
        .map_err(|e| ApiError::db(&e))?;

    match agent.db.role_get_by_id(&id).await {
        Ok(Some(r)) => Ok((
            StatusCode::CREATED,
            Json(role_service::role_json(&r)),
        )),
        _ => Err(ApiError::internal("failed to load created role")),
    }
}

/// GET /api/agent/roles/:id — 详情。
///
/// # Errors
/// 404 角色不存在；503 agent 未初始化；500 DB 错误。
pub async fn get_role(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err(ApiError::unavailable("agent workbench not initialized"));
    };
    match agent.db.role_get_by_id(&id).await {
        Ok(Some(r)) => Ok(Json(role_service::role_json(&r))),
        Ok(None) => Err(ApiError::not_found("role not found")),
        Err(e) => Err(ApiError::db(&e)),
    }
}

/// PUT /api/agent/roles/:id — 部分更新。内置角色不可改名。
///
/// # Errors
/// 400 字段校验失败；403 内置角色改名；404 不存在；409 重名；503 未初始化；500 DB 错误。
pub async fn update_role(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRoleRequest>,
) -> ApiResult<impl IntoResponse> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err(ApiError::unavailable("agent workbench not initialized"));
    };
    let existing = match agent.db.role_get_by_id(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(ApiError::not_found("role not found")),
        Err(e) => return Err(ApiError::db(&e)),
    };

    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .unwrap_or(&existing.name);
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
    let model_override = body
        .model_override
        .as_deref()
        .or(existing.model_override.as_deref());

    role_service::validate_role_fields(
        name,
        description,
        scope,
        mode,
        Some(tools_allow),
        Some(tools_deny),
    )
    .map_err(ApiError::bad_request)?;

    // 内置角色不可改名
    if existing.is_builtin != 0 && name != existing.name {
        return Err(ApiError::forbidden(
            "builtin role name cannot be changed",
        ));
    }

    let (scope_type, client_id, workspace_id) =
        role_service::scope_coords(scope, base_client, base_workspace);

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
            return Err(ApiError::conflict(format!(
                "role '{name}' already exists in this scope"
            )));
        }
    }

    let tools_allow_json =
        serde_json::to_string(tools_allow).unwrap_or_else(|_| "[]".to_string());
    let tools_deny_json =
        serde_json::to_string(tools_deny).unwrap_or_else(|_| "[]".to_string());

    agent
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
        .map_err(|e| ApiError::db(&e))?;

    match agent.db.role_get_by_id(&id).await {
        Ok(Some(r)) => Ok(Json(role_service::role_json(&r))),
        _ => Err(ApiError::internal("failed to load updated role")),
    }
}

/// DELETE /api/agent/roles/:id — 删除。内置角色不可删除。
///
/// # Errors
/// 403 内置角色；404 不存在；503 未初始化；500 DB 错误。
pub async fn delete_role(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err(ApiError::unavailable("agent workbench not initialized"));
    };
    let existing = match agent.db.role_get_by_id(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(ApiError::not_found("role not found")),
        Err(e) => return Err(ApiError::db(&e)),
    };
    if existing.is_builtin != 0 {
        return Err(ApiError::forbidden("builtin role cannot be deleted"));
    }
    agent
        .db
        .role_delete(&id)
        .await
        .map_err(|e| ApiError::db(&e))?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// PATCH /api/agent/roles/:id/toggle — 启停切换。
///
/// # Errors
/// 404 不存在；503 未初始化；500 DB 错误。
pub async fn toggle_role(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err(ApiError::unavailable("agent workbench not initialized"));
    };
    match agent.db.role_get_by_id(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return Err(ApiError::not_found("role not found")),
        Err(e) => return Err(ApiError::db(&e)),
    }
    agent
        .db
        .role_toggle_enabled(&id)
        .await
        .map_err(|e| ApiError::db(&e))?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
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
    use crate::control_plane::ServerState;
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
            json_request(
                Method::POST,
                &format!("/api/agent/roles/{id}/toggle"),
                &json!(null),
            ),
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
            json_request(
                Method::DELETE,
                &format!("/api/agent/roles/{id}"),
                &json!(null),
            ),
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
            json_request(
                Method::POST,
                "/api/agent/roles",
                &json!({ "name": "MyRole" }),
            ),
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
            json_request(Method::GET, "/api/agent/roles?scope=global", &json!(null)),
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
