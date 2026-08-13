//! Agent workbench workspace 相关 handler：workspace CRUD + FS 面板 + Git 面板 + 文件补全。
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use crate::mgmt::api::ApiState;

use super::dto::{CreateWorkspaceRequest, FsPathQuery, PutFsFileRequest, UpdateWorkspaceRequest, WorkspaceFilesQuery};
use super::new_id;

/// 校验 agent_type：空串（内置 runner）或受支持的 ACP 引擎。
fn validate_agent_type(agent_type: &str) -> bool {
    matches!(agent_type, "" | "gemini" | "claude-code" | "opencode")
}

/// 校验 agent_config_overrides：空串合法（调用方归一化 None）；非空必须是
/// JSON object 且所有 value 为 string（set_config_option 的 value 形态）。
fn validate_config_overrides(raw: &str) -> bool {
    if raw.is_empty() {
        return true;
    }
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(map)) => map.values().all(serde_json::Value::is_string),
        _ => false,
    }
}

pub async fn list_workspaces(State(state): State<ApiState>) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_list_workspaces().await {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn create_workspace(
    State(state): State<ApiState>,
    Json(body): Json<CreateWorkspaceRequest>,
) -> impl IntoResponse {
    if body.runtime_type != "host" && body.runtime_type != "docker" {
        return (
            StatusCode::BAD_REQUEST,
            "runtime_type must be 'host' or 'docker'",
        )
            .into_response();
    }
    if body.runtime_type == "docker" && body.docker_image.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "docker_image required for docker runtime",
        )
            .into_response();
    }
    if !validate_agent_type(&body.agent_type) {
        return (
            StatusCode::BAD_REQUEST,
            "agent_type must be '' | gemini | claude-code | opencode",
        )
            .into_response();
    }
    if let Some(raw) = body.agent_config_overrides.as_deref() {
        if !validate_config_overrides(raw) {
            return (
                StatusCode::BAD_REQUEST,
                "agent_config_overrides must be a JSON object with string values",
            )
                .into_response();
        }
    }
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 可选 ACP 字段的空串归一化为 None：存储保持 NULL 而非空串。
    let agent_path = body.agent_path.as_deref().filter(|s| !s.is_empty());
    let llm_model_id = body.llm_model_id.as_deref().filter(|s| !s.is_empty());
    let agent_config_overrides = body
        .agent_config_overrides
        .as_deref()
        .filter(|s| !s.is_empty());
    let id = new_id();
    match agent
        .db
        .agent_create_workspace(
            &id,
            &body.name,
            &body.client_id,
            &body.runtime_type,
            &body.root_path,
            body.docker_image.as_deref(),
            body.docker_container_id.as_deref(),
            &body.agent_type,
            agent_path,
            llm_model_id,
            agent_config_overrides,
        )
        .await
    {
        Ok(()) => match agent.db.agent_get_workspace(&id).await {
            Ok(Some(ws)) => Json(ws).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn get_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_get_workspace(&id).await {
        Ok(Some(ws)) => Json(ws).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn update_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateWorkspaceRequest>,
) -> impl IntoResponse {
    // approval_mode 校验：非法值拒绝（而不是静默落库）
    if let Some(m) = body.approval_mode.as_deref() {
        if !matches!(m, "safe" | "auto_write" | "full_auto") {
            return (
                StatusCode::BAD_REQUEST,
                "approval_mode must be safe|auto_write|full_auto",
            )
                .into_response();
        }
    }
    // agent_type 校验：空串合法（切回内置 runner），非法引擎拒绝
    if let Some(t) = body.agent_type.as_deref() {
        if !validate_agent_type(t) {
            return (
                StatusCode::BAD_REQUEST,
                "agent_type must be '' | gemini | claude-code | opencode",
            )
                .into_response();
        }
    }
    if let Some(raw) = body.agent_config_overrides.as_deref() {
        if !validate_config_overrides(raw) {
            return (
                StatusCode::BAD_REQUEST,
                "agent_config_overrides must be a JSON object with string values",
            )
                .into_response();
        }
    }
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 空串归一化为 None（保持字段语义：未设置 ≠ 空串）
    let system_prompt = body
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // ACP 字段 COALESCE 语义：None 保持原值；agent_type 空串合法（回到内置 runner）；
    // agent_path/llm_model_id 空串归一化为 None（本迭代不支持清空，见 Task 8 brief）；
    // agent_config_overrides 空串归一化 None（保持），`"{}"` 非空原样传入 db → 清空。
    let agent_path = body.agent_path.as_deref().filter(|s| !s.is_empty());
    let llm_model_id = body.llm_model_id.as_deref().filter(|s| !s.is_empty());
    let agent_config_overrides = body
        .agent_config_overrides
        .as_deref()
        .filter(|s| !s.is_empty());
    match agent
        .db
        .agent_update_workspace(
            &id,
            &body.name,
            &body.root_path,
            system_prompt,
            body.approval_mode.as_deref(),
            body.agent_type.as_deref(),
            agent_path,
            llm_model_id,
            agent_config_overrides,
        )
        .await
    {
        Ok(()) => get_workspace(State(state), Path(id)).await.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn delete_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_delete_workspace(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// 面板执行辅助：加载 workspace、docker container 存在性检查、经隧道执行命令。
/// 错误一律映射为 HTTP 响应（404/503），与 `list_workspace_files` 的语义一致：
/// 客户端离线/隧道失败/exec 错误 → 503（前端区分「离线」与「空结果」）。
async fn workspace_exec(
    state: &ApiState,
    workspace_id: &str,
    command: rust_tunnel_common::AgentCommand,
) -> Result<rust_tunnel_common::AgentResult, axum::response::Response> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    };
    let ws = match agent.db.agent_get_workspace(workspace_id).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return Err(StatusCode::NOT_FOUND.into_response()),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    };
    if ws.runtime_type == "docker" && ws.docker_container_id.is_none() {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    let result = crate::agent::executor::exec_on_client(
        agent,
        &ws.id,
        &ws.client_id,
        &ws.root_path,
        ws.docker_container_id.as_deref(),
        command,
    )
    .await;
    match result {
        rust_tunnel_common::AgentResult::Error { .. } => {
            Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
        ok => Ok(ok),
    }
}

/// GET /api/agent/workspaces/:id/fs/tree?path=<rel>
/// FilesPanel 目录树数据源：ListDir 输出（目录以 '/' 结尾）解析为结构化 JSON。
pub async fn get_fs_tree(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<FsPathQuery>,
) -> impl IntoResponse {
    let path = params.path.unwrap_or_else(|| ".".to_string());
    let result = match workspace_exec(
        &state,
        &id,
        rust_tunnel_common::AgentCommand::ListDir { path },
    )
    .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let rust_tunnel_common::AgentResult::FileContent { content } = result else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let entries: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let is_dir = l.ends_with('/');
            let name = l.strip_suffix('/').unwrap_or(l);
            serde_json::json!({ "name": name, "is_dir": is_dir })
        })
        .collect();
    Json(serde_json::json!({ "entries": entries })).into_response()
}

/// GET /api/agent/workspaces/:id/fs/file?path=<rel>
/// FilesPanel 文件预览：返回内容与截断标记（客户端 100KB 截断惯例 `[truncated]`）。
pub async fn get_fs_file(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<FsPathQuery>,
) -> impl IntoResponse {
    let Some(path) = params.path.filter(|p| !p.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "path is required").into_response();
    };
    let result = match workspace_exec(
        &state,
        &id,
        rust_tunnel_common::AgentCommand::ReadFile { path },
    )
    .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let rust_tunnel_common::AgentResult::FileContent { content } = result else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let truncated = content.contains("[truncated]");
    Json(serde_json::json!({ "content": content, "truncated": truncated })).into_response()
}

/// PUT /api/agent/workspaces/:id/fs/file
/// FilesPanel 文件保存。按 workspace approval_mode 判定：需审批且未确认 → 409
/// `{needs_approval:true}`，前端弹确认后带 `approved:true` 重发。
pub async fn put_fs_file(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<PutFsFileRequest>,
) -> impl IntoResponse {
    if body.path.is_empty() {
        return (StatusCode::BAD_REQUEST, "path is required").into_response();
    }
    // 审批判定需要 approval_mode：先取 workspace（workspace_exec 内部还会再取一次，
    // 多一次 DB 读换取 helper 复用，面板低频操作可接受）。
    let approval_mode = match &state.server_state.agent_state {
        Some(agent) => match agent.db.agent_get_workspace(&id).await {
            Ok(Some(ws)) => ws.approval_mode,
            Ok(None) => return StatusCode::NOT_FOUND.into_response(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let command = rust_tunnel_common::AgentCommand::WriteFile {
        path: body.path,
        content: body.content,
    };
    if !body.approved.unwrap_or(false)
        && crate::agent::approval::needs_approval(&approval_mode, &command)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "needs_approval": true })),
        )
            .into_response();
    }
    match workspace_exec(&state, &id, command).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(resp) => resp,
    }
}

/// GET /api/agent/workspaces/:id/git/status
/// GitPanel 数据源：`git status --porcelain=v1 -b` 原文（解析放前端）。
pub async fn get_git_status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let result = match workspace_exec(
        &state,
        &id,
        rust_tunnel_common::AgentCommand::Shell {
            cmd: "git status --porcelain=v1 -b".to_string(),
            cwd: None,
        },
    )
    .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let rust_tunnel_common::AgentResult::Shell { stdout, stderr, .. } = result else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    Json(serde_json::json!({ "status": stdout, "stderr": stderr })).into_response()
}

/// GET /api/agent/workspaces/:id/git/diff?path=<rel>
/// GitPanel 文件 diff：path 为空时返回整个工作区 diff。
pub async fn get_git_diff(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<FsPathQuery>,
) -> impl IntoResponse {
    let result = match workspace_exec(
        &state,
        &id,
        rust_tunnel_common::AgentCommand::GitDiff {
            path: params.path.filter(|p| !p.is_empty()),
        },
    )
    .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let rust_tunnel_common::AgentResult::FileContent { content } = result else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    Json(serde_json::json!({ "diff": content })).into_response()
}

/// 单引号 shell 转义：' → '\''（标准做法），包裹后任意输入安全。
fn shell_escape_q(q: &str) -> String {
    format!("'{}'", q.replace('\'', r"'\''"))
}

/// GET /api/agent/workspaces/:id/files?q=<前缀>&limit=<n>
/// @补全数据源：经隧道在沙箱内 find+grep 过滤文件路径。Windows 客户端无 find/grep
/// 时 grep 报错 → 返回空列表（前端降级手输路径），不视为错误。
pub async fn list_workspace_files(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<WorkspaceFilesQuery>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let ws = match agent.db.agent_get_workspace(&id).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if ws.runtime_type == "docker" && ws.docker_container_id.is_none() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 50);
    let q = params.q.trim();
    let cmd = if q.is_empty() {
        format!(
            "find . -path ./.git -prune -o -type f -print | head -{}",
            limit
        )
    } else {
        format!(
            "find . -path ./.git -prune -o -type f -print | grep -i -F -- {} | head -{}",
            shell_escape_q(q),
            limit
        )
    };
    let result = crate::agent::executor::exec_on_client(
        agent,
        &ws.id,
        &ws.client_id,
        &ws.root_path,
        ws.docker_container_id.as_deref(),
        rust_tunnel_common::AgentCommand::Shell { cmd, cwd: None },
    )
    .await;
    let files: Vec<String> = match result {
        // grep 无命中 / Windows 无 grep 报错（走 stderr，stdout 为空）→ 空列表 200，降级语义保留
        rust_tunnel_common::AgentResult::Shell { stdout, .. } => stdout
            .lines()
            .map(|l| l.strip_prefix("./").unwrap_or(l).to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        // 客户端离线/隧道失败/exec 错误 → 503，前端据此区分「离线」与「无匹配」。
        // Windows 无 sh 时 spawn 失败也归入此分支（规格内取舍：503 对前端同样是降级）。
        _ => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    Json(serde_json::json!({ "files": files })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthConfig;
    use crate::control::ServerState;
    use crate::db::Database;
    use std::sync::Arc;

    async fn test_state() -> (ApiState, Database) {
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db.clone());
        (
            ApiState {
                server_state,
                auth_config: Arc::new(AuthConfig::new(None, None)),
                log_store: None,
            },
            db,
        )
    }

    #[tokio::test]
    async fn test_create_and_list_workspaces() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state.clone()),
            Json(CreateWorkspaceRequest {
                name: "proj".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/home/u/proj".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: String::new(),
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = list_workspaces(State(state)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_create_workspace_rejects_bad_runtime_type() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "x".into(),
                client_id: "nas".into(),
                runtime_type: "kubernetes".into(),
                root_path: "/p".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: String::new(),
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_docker_workspace_persists_container_id() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "dproj".into(),
                client_id: "nas".into(),
                runtime_type: "docker".into(),
                root_path: "/container/work".into(),
                docker_image: Some("node:20".into()),
                docker_container_id: Some("dev-ctr".into()),
                agent_type: String::new(),
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        // 响应体包含持久化的 container_id
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["runtime_type"], "docker");
        assert_eq!(json["docker_container_id"], "dev-ctr");
    }

    #[test]
    fn test_create_workspace_request_missing_acp_fields_defaults() {
        // 向后兼容：旧前端不发送 ACP 字段 → serde default 填充 agent_type=''、
        // agent_path/llm_model_id=None（自研 runner 路径）。
        let body: CreateWorkspaceRequest = serde_json::from_str(
            r#"{"name":"p","client_id":"nas","runtime_type":"host","root_path":"/p"}"#,
        )
        .unwrap();
        assert_eq!(body.agent_type, "");
        assert_eq!(body.agent_path, None);
        assert_eq!(body.llm_model_id, None);
    }

    #[tokio::test]
    async fn test_create_workspace_persists_acp_fields() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "acp-proj".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/p".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: "gemini".into(),
                agent_path: Some("/opt/gemini".into()),
                llm_model_id: Some("model-1".into()),
                agent_config_overrides: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        // 响应体回读 ACP 字段
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["agent_type"], "gemini");
        assert_eq!(json["agent_path"], "/opt/gemini");
        assert_eq!(json["llm_model_id"], "model-1");
    }

    #[tokio::test]
    async fn test_create_workspace_rejects_invalid_agent_type() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "x".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/p".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: "cursor".into(),
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_workspace_acp_fields() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(UpdateWorkspaceRequest {
                name: "p".into(),
                root_path: "/p".into(),
                system_prompt: None,
                approval_mode: None,
                agent_type: Some("gemini".into()),
                agent_path: Some("/opt/gemini".into()),
                llm_model_id: Some("model-1".into()),
                agent_config_overrides: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "gemini");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/gemini"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("model-1"));
    }

    #[tokio::test]
    async fn test_update_workspace_absent_acp_fields_keep_existing() {
        // COALESCE 语义：缺省 ACP 字段（None）保持原值；agent_path/llm_model_id
        // 空串归一化为 None → 同样保持原值（本迭代不支持清空，见 brief）。
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1",
            "p",
            "nas",
            "host",
            "/p",
            None,
            None,
            "gemini",
            Some("/opt/gemini"),
            Some("model-1"),
            None,
        )
        .await
        .unwrap();
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(UpdateWorkspaceRequest {
                name: "p".into(),
                root_path: "/p".into(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: Some("".into()),
                llm_model_id: None,
                agent_config_overrides: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "gemini");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/gemini"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("model-1"));
    }

    #[tokio::test]
    async fn test_update_workspace_clears_agent_type_to_builtin() {
        // agent_type 空串合法：从 ACP 引擎切回内置 runner（与 path/model 不同，可清空）。
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "gemini", None, None, None,
        )
        .await
        .unwrap();
        let resp = update_workspace(
            State(state),
            Path("w1".to_string()),
            Json(UpdateWorkspaceRequest {
                name: "p".into(),
                root_path: "/p".into(),
                system_prompt: None,
                approval_mode: None,
                agent_type: Some("".into()),
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "");
    }

    #[tokio::test]
    async fn test_create_workspace_rejects_bad_config_overrides() {
        let (state, _db) = test_state().await;
        // 非法 JSON → 400
        let resp = create_workspace(
            State(state.clone()),
            Json(CreateWorkspaceRequest {
                name: "p".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/p".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: String::new(),
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: Some("not-json".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // JSON 数组（非 object）→ 400
        let resp = create_workspace(
            State(state.clone()),
            Json(CreateWorkspaceRequest {
                name: "p".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/p".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: String::new(),
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: Some(r#"["model"]"#.into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // object 但 value 非 string → 400
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "p".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/p".into(),
                docker_image: None,
                docker_container_id: None,
                agent_type: String::new(),
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: Some(r#"{"model": 1}"#.into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_update_workspace_config_overrides_clear_semantics() {
        // create 带 overrides 的 workspace → update 不传字段保持原值 → 传 "{}" 清空
        // → 传非法 JSON 400（断言经读库验证）。
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1",
            "p",
            "nas",
            "host",
            "/p",
            None,
            None,
            "gemini",
            None,
            None,
            Some(r#"{"mode":"plan"}"#),
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.agent_config_overrides.as_deref(),
            Some(r#"{"mode":"plan"}"#)
        );

        // 1) 不传字段（None）→ COALESCE 保持原值
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(UpdateWorkspaceRequest {
                name: "p".into(),
                root_path: "/p".into(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.agent_config_overrides.as_deref(),
            Some(r#"{"mode":"plan"}"#)
        );

        // 2) 传 "{}" → 覆盖为空对象（清空语义）
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(UpdateWorkspaceRequest {
                name: "p".into(),
                root_path: "/p".into(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: Some("{}".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_config_overrides.as_deref(), Some("{}"));

        // 3) 传非法 JSON → 400，且原值保持不变
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(UpdateWorkspaceRequest {
                name: "p".into(),
                root_path: "/p".into(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: Some("not-json".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_config_overrides.as_deref(), Some("{}"));
    }

    #[tokio::test]
    async fn test_list_workspace_files_workspace_not_found() {
        let (state, _db) = test_state().await;
        let resp = list_workspace_files(
            State(state),
            Path("ghost".to_string()),
            Query(WorkspaceFilesQuery {
                q: "main".into(),
                limit: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_workspace_files_client_offline_returns_503() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        // 客户端不在线（未注册任何客户端到 registry）：exec_on_client 隧道层
        // 立即返回 AgentResult::Error，handler 应回 503 供前端区分「离线」与「无匹配」。
        let resp = list_workspace_files(
            State(state),
            Path("w1".to_string()),
            Query(WorkspaceFilesQuery {
                q: "main".into(),
                limit: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_fs_endpoints_workspace_not_found() {
        let (state, _db) = test_state().await;
        let resp = get_fs_tree(
            State(state.clone()),
            Path("ghost".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = get_fs_file(
            State(state.clone()),
            Path("ghost".to_string()),
            Query(FsPathQuery {
                path: Some("a.rs".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = get_git_status(State(state.clone()), Path("ghost".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = get_git_diff(
            State(state),
            Path("ghost".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_fs_endpoints_client_offline_returns_503() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        // 客户端离线：所有面板端点统一 503（前端据此显示「客户端离线」而非空态）。
        let resp = get_fs_tree(
            State(state.clone()),
            Path("w1".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resp = get_fs_file(
            State(state.clone()),
            Path("w1".to_string()),
            Query(FsPathQuery {
                path: Some("a.rs".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resp = get_git_status(State(state.clone()), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resp = get_git_diff(
            State(state),
            Path("w1".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_fs_file_requires_path() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        let resp = get_fs_file(
            State(state),
            Path("w1".to_string()),
            Query(FsPathQuery { path: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_put_fs_file_safe_mode_needs_approval_409() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        // 默认 approval_mode = safe：WriteFile 需确认。未确认 → 409 needs_approval，
        // 且不会触碰隧道（客户端离线也不会 503）。
        let resp = put_fs_file(
            State(state.clone()),
            Path("w1".to_string()),
            Json(PutFsFileRequest {
                path: "a.rs".into(),
                content: "fn main() {}".into(),
                approved: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["needs_approval"], true);

        // 确认后重发 → 通过审批层，走到隧道（客户端离线 → 503）。
        let resp = put_fs_file(
            State(state),
            Path("w1".to_string()),
            Json(PutFsFileRequest {
                path: "a.rs".into(),
                content: "fn main() {}".into(),
                approved: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_put_fs_file_full_auto_skips_approval() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_update_workspace(
            "w1",
            "proj",
            "/p",
            None,
            Some("full_auto"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        // full_auto：未确认也直接放行 → 客户端离线 503（而非 409）。
        let resp = put_fs_file(
            State(state),
            Path("w1".to_string()),
            Json(PutFsFileRequest {
                path: "a.rs".into(),
                content: "x".into(),
                approved: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_put_fs_file_workspace_not_found() {
        let (state, _db) = test_state().await;
        let resp = put_fs_file(
            State(state),
            Path("ghost".to_string()),
            Json(PutFsFileRequest {
                path: "a.rs".into(),
                content: "x".into(),
                approved: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_shell_escape_q() {
        assert_eq!(shell_escape_q("main"), "'main'");
        assert_eq!(shell_escape_q("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_escape_q("a';b|rm"), r#"'a'\'';b|rm'"#); // 单引号转义后特殊字符在引号内安全
    }
}
