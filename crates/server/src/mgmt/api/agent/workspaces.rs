//! Agent workbench workspace 相关 handler：workspace CRUD + FS 面板 + Git 面板 + 文件补全。
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use rust_tunnel_common::AgentCommand;
use rust_tunnel_common::AgentResult;

use crate::agent::git_plan;
use crate::db::agent::AgentWorkspaceRecord;
use crate::mgmt::api::ApiState;

use super::dto::{
    CreateWorkspaceRequest, FsPathQuery, GitApprovedBody, GitBranchDeleteRequest,
    GitCheckoutRequest, GitCommitRequest, GitDiffQuery, GitLogQuery, GitResetRequest,
    GitRevertRequest, GitShowQuery, GitStageRequest, GitStashIndexRequest, GitStashPushRequest,
    GitUnstageRequest, PutFsFileRequest, UpdateWorkspaceRequest, WorkspaceFilesQuery,
};
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

/// 校验 claude_tier_models：空串合法（调用方归一化 None）；非空必须是
/// JSON object，key 白名单 opus|sonnet|haiku|subagent 且 value 必须为 string（模型引用）。
fn validate_tier_models(raw: &str) -> bool {
    if raw.is_empty() {
        return true;
    }
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(map)) => map.iter().all(|(k, v)| {
            matches!(k.as_str(), "opus" | "sonnet" | "haiku" | "subagent") && v.is_string()
        }),
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
    if let Some(raw) = body.claude_tier_models.as_deref() {
        if !validate_tier_models(raw) {
            return (
                StatusCode::BAD_REQUEST,
                "claude_tier_models must be a JSON object with opus|sonnet|haiku|subagent keys and string values",
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
    let claude_tier_models = body.claude_tier_models.as_deref().filter(|s| !s.is_empty());
    // GitHub 字段：token 空串归一化 None；非空则用 LlmCipher 加密后落库（与 LLM
    // provider API Key 同一机制，未配置主密钥时明文兼容降级）。owner/repo 空串
    // 归一化 None。写入走独立的 `agent_set_workspace_github`（COALESCE 语义）。
    let cipher = super::agent_cipher(&state).await;
    let github_token = body.github_token.as_deref().filter(|s| !s.is_empty());
    let github_owner = body.github_owner.as_deref().filter(|s| !s.is_empty());
    let github_repo = body.github_repo.as_deref().filter(|s| !s.is_empty());
    let github_token_stored =
        github_token.map(|t| crate::llm::crypto::encrypt_field(cipher.as_ref(), t));
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
            claude_tier_models,
        )
        .await
    {
        Ok(()) => {
            if let Err(e) = agent
                .db
                .agent_set_workspace_github(
                    &id,
                    github_token_stored.as_deref(),
                    github_owner,
                    github_repo,
                )
                .await
            {
                tracing::error!(workspace_id = %id, "persist github fields failed: {e}");
            }
            match agent.db.agent_get_workspace(&id).await {
                Ok(Some(ws)) => Json(ws).into_response(),
                _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
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
        if !matches!(m, "safe" | "auto_write" | "full_auto" | "plan") {
            return (
                StatusCode::BAD_REQUEST,
                "approval_mode must be safe|auto_write|full_auto|plan",
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
    // agent_config_overrides 三态校验：None（省略）跳过；Some(None)（null）= 清空，
    // 无需校验；Some(Some(raw)) = 写入新值，须是 JSON object 且 value 为 string。
    if let Some(Some(raw)) = &body.agent_config_overrides {
        if !validate_config_overrides(raw) {
            return (
                StatusCode::BAD_REQUEST,
                "agent_config_overrides must be a JSON object with string values",
            )
                .into_response();
        }
    }
    if let Some(Some(raw)) = &body.claude_tier_models {
        if !validate_tier_models(raw) {
            return (
                StatusCode::BAD_REQUEST,
                "claude_tier_models must be a JSON object with opus|sonnet|haiku|subagent keys and string values",
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
    // agent_path/llm_model_id 空串归一化为 None（本迭代不支持清空）；
    // agent_config_overrides 三态：None=保留、Some(None)=清空、Some(Some(s))=写入。
    let agent_path = body.agent_path.as_deref().filter(|s| !s.is_empty());
    let llm_model_id = body.llm_model_id.as_deref().filter(|s| !s.is_empty());
    // 三态解析：None → (None, false)；Some(None) → (None, true)；
    // Some(Some(s)) → (Some(s), false)（空串归一化为清空，向后兼容旧前端惯例）。
    let (agent_config_overrides, clear_overrides) = match &body.agent_config_overrides {
        None => (None, false),
        Some(None) => (None, true),
        Some(Some(s)) if s.is_empty() => (None, true), // 空串归一化为清空
        Some(Some(s)) => (Some(s.as_str()), false),
    };
    // claude_tier_models 同 overrides 三态语义：省略=保持、null/空串=清空、非空=写入。
    let (claude_tier_models, clear_tier_models) = match &body.claude_tier_models {
        None => (None, false),
        Some(None) => (None, true),
        Some(Some(s)) if s.is_empty() => (None, true),
        Some(Some(s)) => (Some(s.as_str()), false),
    };
    // GitHub 字段：token 空串/缺省 → None（DB COALESCE 保持已存密文）；非空 →
    // 加密后更新。owner/repo 同语义。写入走独立的 `agent_set_workspace_github`。
    let cipher = super::agent_cipher(&state).await;
    let github_token = body.github_token.as_deref().filter(|s| !s.is_empty());
    let github_owner = body.github_owner.as_deref().filter(|s| !s.is_empty());
    let github_repo = body.github_repo.as_deref().filter(|s| !s.is_empty());
    let github_token_stored =
        github_token.map(|t| crate::llm::crypto::encrypt_field(cipher.as_ref(), t));
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
            claude_tier_models,
            clear_overrides,
            clear_tier_models,
        )
        .await
    {
        Ok(()) => {
            if let Err(e) = agent
                .db
                .agent_set_workspace_github(
                    &id,
                    github_token_stored.as_deref(),
                    github_owner,
                    github_repo,
                )
                .await
            {
                tracing::error!(workspace_id = %id, "persist github fields failed: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            get_workspace(State(state), Path(id)).await.into_response()
        }
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

/// 加载 workspace + docker 容器存在性检查 + GitExec 客户端版本门控。
/// GitExec 是新 bincode 变体：老客户端（<0.5.0）收到会反序列化失败断开控制连接，
/// 必须在服务端短路为 409 提示升级（面板读/写端点统一走这里）。
async fn load_git_workspace(
    state: &ApiState,
    workspace_id: &str,
) -> Result<(crate::agent::AgentState, AgentWorkspaceRecord), axum::response::Response> {
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
    match agent.registry.client_handle(&ws.client_id).await {
        // 客户端未注册（离线）：与其余面板端点一致 → 503（前端区分「离线」与「升级」）。
        None => return Err(StatusCode::SERVICE_UNAVAILABLE.into_response()),
        Some(entry) => {
            if !crate::agent::runner::client_supports_git_exec(entry.client_version.as_deref()) {
                return Err((
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "needs_upgrade": true,
                        "message": "client too old: git panel requires client >= 0.5.0; please upgrade the client",
                    })),
                )
                    .into_response());
            }
        }
    }
    Ok((agent.clone(), ws))
}

/// Git 读端点统一流程：加载+版本门控 → git_plan 校验（fail-closed 400）→ 隧道执行。
/// 返回 FileContent 原文；git 命令失败保留 stderr（503 + error 体，区别于旧的
/// workspace_exec 裸 503，便于面板展示 git 错误信息）。
async fn run_git_read(
    state: &ApiState,
    workspace_id: &str,
    git_args: Vec<String>,
) -> Result<String, axum::response::Response> {
    let (agent, ws) = load_git_workspace(state, workspace_id).await?;
    let planned = git_plan::plan(&git_args).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response()
    })?;
    let result = crate::agent::executor::exec_on_client(
        &agent,
        &ws.id,
        &ws.client_id,
        &ws.root_path,
        ws.docker_container_id.as_deref(),
        AgentCommand::GitExec { args: planned.args },
    )
    .await;
    match result {
        AgentResult::FileContent { content } => Ok(content),
        AgentResult::Error { message } => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": message })),
        )
            .into_response()),
        _ => Err(StatusCode::SERVICE_UNAVAILABLE.into_response()),
    }
}

/// Git 写端点统一流程：git_plan 校验（fail-closed 400）→ 加载+版本门控 →
/// 审批判定（需审且未确认 → 409 `{needs_approval, summary}`）→ 隧道执行。
/// 前端带 `approved: true` 重发即跳过审批。
async fn run_git_write(
    state: &ApiState,
    workspace_id: &str,
    approved: bool,
    git_args: Vec<String>,
) -> axum::response::Response {
    let planned = match git_plan::plan(&git_args) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let ws = match agent.db.agent_get_workspace(workspace_id).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let command = AgentCommand::GitExec {
        args: planned.args.clone(),
    };
    // 审批判定在版本门控之前：需审时无论客户端在线/版本如何都先弹审批卡片，
    // 前端确认后带 approved=true 重发再走到版本门控。
    if !approved && crate::agent::approval::needs_approval(&ws.approval_mode, &command) {
        let summary = crate::agent::approval::approval_summary(&command);
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "needs_approval": true,
                "summary": summary,
            })),
        )
            .into_response();
    }
    // 通过审批后：docker 容器存在性 + GitExec 客户端版本门控（离线 503 / 老客户端 409）。
    if ws.runtime_type == "docker" && ws.docker_container_id.is_none() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match agent.registry.client_handle(&ws.client_id).await {
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Some(entry) => {
            if !crate::agent::runner::client_supports_git_exec(entry.client_version.as_deref()) {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "needs_upgrade": true,
                        "message": "client too old: git panel requires client >= 0.5.0; please upgrade the client",
                    })),
                )
                    .into_response();
            }
        }
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
        AgentResult::FileContent { content } => {
            Json(serde_json::json!({ "output": content })).into_response()
        }
        AgentResult::Success => Json(serde_json::json!({ "output": "ok" })).into_response(),
        AgentResult::Shell {
            stdout,
            stderr,
            exit_code,
        } => Json(serde_json::json!({
            "output": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
        }))
        .into_response(),
        AgentResult::Error { message } => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": message })),
        )
            .into_response(),
        AgentResult::WriteOutcome {
            bytes_written,
            lines_added,
            lines_removed,
            ..
        } => Json(serde_json::json!({
            "output": format!("wrote: +{lines_added}/-{lines_removed} lines, {bytes_written} bytes"),
            "bytes_written": bytes_written,
            "lines_added": lines_added,
            "lines_removed": lines_removed,
        }))
        .into_response(),
    }
}

/// 校验 commit / stash message 上限（与控制通道协议上限同源，见 tools::MAX_COMMIT_MSG_LEN）。
/// 返回 `Some(Response)` 表示非法（400），`None` 表示通过。避免返回大体积
/// `Err(Response)` 触发 `result_large_err`。
fn check_git_message(message: &str) -> Option<axum::response::Response> {
    if message.is_empty() {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "message must not be empty" })),
            )
                .into_response(),
        );
    }
    if message.len() > crate::agent::tools::MAX_COMMIT_MSG_LEN {
        return Some((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("message too long (>{})", crate::agent::tools::MAX_COMMIT_MSG_LEN)
            })),
        )
            .into_response());
    }
    None
}

/// 解析 `git branch --format=%(refname:short)%09%(HEAD)%09%(upstream:short)` 输出。
fn parse_branches(content: &str) -> Vec<serde_json::Value> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut it = line.splitn(3, '\t');
            let name = it.next().unwrap_or_default().to_string();
            let head = it.next().unwrap_or_default();
            let upstream = it.next().unwrap_or_default();
            serde_json::json!({
                "name": name,
                "current": head == "*",
                "upstream": (!upstream.is_empty()).then_some(upstream.to_string()),
            })
        })
        .collect()
}

/// 解析 `git log --format=%H%x00%h%x00%an%x00%aI%x00%s` 输出（NUL 分隔字段，
/// 换行分隔记录；subject 恒为单行）。
fn parse_commits(content: &str) -> Vec<serde_json::Value> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut fields = line.split('\0');
            serde_json::json!({
                "hash": fields.next().unwrap_or_default(),
                "short": fields.next().unwrap_or_default(),
                "author": fields.next().unwrap_or_default(),
                "date": fields.next().unwrap_or_default(),
                "subject": fields.next().unwrap_or_default(),
            })
        })
        .collect()
}

/// 解析 `git stash list` 输出（每行 `stash@{N}: <message>`）。
fn parse_stashes(content: &str) -> Vec<serde_json::Value> {
    content
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("stash@{")?;
            let (index_str, after) = rest.split_once('}')?;
            let index = index_str.parse::<usize>().ok()?;
            let message = after.strip_prefix(": ").unwrap_or_default().to_string();
            Some(serde_json::json!({ "index": index, "message": message }))
        })
        .collect()
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

/// GET /api/agent/workspaces/:id/git/diff?path=<rel>&cached=true
/// GitPanel 文件 diff：path 为空时返回整个工作区 diff；`cached=true` 取 staged diff
/// （走 GitExec，要求客户端 ≥0.5.0；非 cached 保留旧 GitDiff 路径兼容老客户端）。
pub async fn get_git_diff(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<GitDiffQuery>,
) -> impl IntoResponse {
    if params.cached.unwrap_or(false) {
        let mut args = vec!["diff".to_string(), "--cached".to_string()];
        if let Some(path) = params.path.filter(|p| !p.is_empty()) {
            args.push("--".to_string());
            args.push(path);
        }
        return match run_git_read(&state, &id, args).await {
            Ok(content) => Json(serde_json::json!({ "diff": content })).into_response(),
            Err(resp) => resp,
        };
    }
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

/// GET /api/agent/workspaces/:id/git/branches
/// 分支列表：{branches:[{name, current, upstream?}]}。
pub async fn get_git_branches(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let content = match run_git_read(
        &state,
        &id,
        vec![
            "branch".to_string(),
            "--format=%(refname:short)%09%(HEAD)%09%(upstream:short)".to_string(),
        ],
    )
    .await
    {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    Json(serde_json::json!({ "branches": parse_branches(&content) })).into_response()
}

/// GET /api/agent/workspaces/:id/git/log?limit=50
/// 提交历史：{commits:[{hash, short, author, date, subject}]}。
pub async fn get_git_log(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<GitLogQuery>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let content = match run_git_read(
        &state,
        &id,
        vec![
            "log".to_string(),
            "-n".to_string(),
            limit.to_string(),
            "--format=%H%x00%h%x00%an%x00%aI%x00%s".to_string(),
        ],
    )
    .await
    {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    Json(serde_json::json!({ "commits": parse_commits(&content) })).into_response()
}

/// GET /api/agent/workspaces/:id/git/show?rev=<rev>
/// 提交详情：{diff: <文本>}（git show 原文，含提交元信息 + diff）。
pub async fn get_git_show(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<GitShowQuery>,
) -> impl IntoResponse {
    let mut args = vec!["show".to_string()];
    if let Some(rev) = params.rev.filter(|r| !r.is_empty()) {
        args.push(rev);
    }
    let content = match run_git_read(&state, &id, args).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    Json(serde_json::json!({ "diff": content })).into_response()
}

/// GET /api/agent/workspaces/:id/git/stash
/// stash 列表：{stashes:[{index, message}]}。
pub async fn get_git_stash(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let content = match run_git_read(&state, &id, vec!["stash".into(), "list".into()]).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    Json(serde_json::json!({ "stashes": parse_stashes(&content) })).into_response()
}

/// POST /api/agent/workspaces/:id/git/stage  {paths[]}
pub async fn post_git_stage(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitStageRequest>,
) -> impl IntoResponse {
    let mut args = vec!["add".to_string(), "--".to_string()];
    args.extend(body.paths.iter().cloned());
    run_git_write(&state, &id, body.approved.unwrap_or(false), args).await
}

/// POST /api/agent/workspaces/:id/git/unstage  {paths[]}
pub async fn post_git_unstage(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitUnstageRequest>,
) -> impl IntoResponse {
    let mut args = vec![
        "restore".to_string(),
        "--staged".to_string(),
        "--".to_string(),
    ];
    args.extend(body.paths.iter().cloned());
    run_git_write(&state, &id, body.approved.unwrap_or(false), args).await
}

/// POST /api/agent/workspaces/:id/git/commit  {message}
pub async fn post_git_commit(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitCommitRequest>,
) -> impl IntoResponse {
    if let Some(resp) = check_git_message(&body.message) {
        return resp;
    }
    run_git_write(
        &state,
        &id,
        body.approved.unwrap_or(false),
        vec!["commit".to_string(), "-m".to_string(), body.message],
    )
    .await
}

/// POST /api/agent/workspaces/:id/git/checkout  {branch, create?}
pub async fn post_git_checkout(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitCheckoutRequest>,
) -> impl IntoResponse {
    let args = if body.create.unwrap_or(false) {
        vec!["checkout".to_string(), "-b".to_string(), body.branch]
    } else {
        vec!["checkout".to_string(), body.branch]
    };
    run_git_write(&state, &id, body.approved.unwrap_or(false), args).await
}

/// POST /api/agent/workspaces/:id/git/branch/delete  {branch, force?}
pub async fn post_git_branch_delete(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitBranchDeleteRequest>,
) -> impl IntoResponse {
    let flag = if body.force.unwrap_or(false) {
        "-D"
    } else {
        "-d"
    };
    run_git_write(
        &state,
        &id,
        body.approved.unwrap_or(false),
        vec!["branch".to_string(), flag.to_string(), body.branch],
    )
    .await
}

/// POST /api/agent/workspaces/:id/git/pull  请求体可为空 / {} / {approved}
pub async fn post_git_pull(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Option<Json<GitApprovedBody>>,
) -> impl IntoResponse {
    let approved = body.and_then(|b| b.approved).unwrap_or(false);
    run_git_write(&state, &id, approved, vec!["pull".to_string()]).await
}

/// POST /api/agent/workspaces/:id/git/push  请求体可为空 / {} / {approved}
pub async fn post_git_push(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Option<Json<GitApprovedBody>>,
) -> impl IntoResponse {
    let approved = body.and_then(|b| b.approved).unwrap_or(false);
    run_git_write(&state, &id, approved, vec!["push".to_string()]).await
}

/// POST /api/agent/workspaces/:id/git/revert  {rev}
pub async fn post_git_revert(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitRevertRequest>,
) -> impl IntoResponse {
    run_git_write(
        &state,
        &id,
        body.approved.unwrap_or(false),
        vec!["revert".to_string(), body.rev],
    )
    .await
}

/// POST /api/agent/workspaces/:id/git/reset  {rev?, mode: soft|mixed|hard}
pub async fn post_git_reset(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitResetRequest>,
) -> impl IntoResponse {
    let mut args = vec!["reset".to_string()];
    match body.mode.as_str() {
        "soft" | "mixed" | "hard" => args.push(format!("--{}", body.mode)),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "mode must be soft|mixed|hard" })),
            )
                .into_response();
        }
    }
    if let Some(rev) = body.rev {
        args.push(rev);
    }
    run_git_write(&state, &id, body.approved.unwrap_or(false), args).await
}

/// POST /api/agent/workspaces/:id/git/stash/push  {message?}
pub async fn post_git_stash_push(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitStashPushRequest>,
) -> impl IntoResponse {
    let mut args = vec!["stash".to_string(), "push".to_string()];
    if let Some(message) = body.message {
        if let Some(resp) = check_git_message(&message) {
            return resp;
        }
        args.push("-m".to_string());
        args.push(message);
    }
    run_git_write(&state, &id, body.approved.unwrap_or(false), args).await
}

/// POST /api/agent/workspaces/:id/git/stash/apply  {index}
pub async fn post_git_stash_apply(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitStashIndexRequest>,
) -> impl IntoResponse {
    run_git_write(
        &state,
        &id,
        body.approved.unwrap_or(false),
        vec![
            "stash".to_string(),
            "apply".to_string(),
            format!("stash@{{{}}}", body.index),
        ],
    )
    .await
}

/// POST /api/agent/workspaces/:id/git/stash/pop  {index}
pub async fn post_git_stash_pop(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitStashIndexRequest>,
) -> impl IntoResponse {
    run_git_write(
        &state,
        &id,
        body.approved.unwrap_or(false),
        vec![
            "stash".to_string(),
            "pop".to_string(),
            format!("stash@{{{}}}", body.index),
        ],
    )
    .await
}

/// POST /api/agent/workspaces/:id/git/stash/drop  {index}
pub async fn post_git_stash_drop(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<GitStashIndexRequest>,
) -> impl IntoResponse {
    run_git_write(
        &state,
        &id,
        body.approved.unwrap_or(false),
        vec![
            "stash".to_string(),
            "drop".to_string(),
            format!("stash@{{{}}}", body.index),
        ],
    )
    .await
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None, None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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

    // ── GitHub 集成字段（创建/更新落库 + 脱敏）──────────────────

    #[tokio::test]
    async fn test_create_workspace_persists_github_fields() {
        let (state, db) = test_state().await;
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
                agent_config_overrides: None,
                claude_tier_models: None,
                github_token: Some("ghp_secret_123".into()),
                github_owner: Some("octo".into()),
                github_repo: Some("repo".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        // 响应体脱敏：无 github_token 字段 / 无 token 明文，只有布尔位 + owner/repo
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let raw = json.to_string();
        assert!(!raw.contains("ghp_secret_123"), "token 明文不得出现在响应");
        assert!(!raw.contains("github_token\""), "不得有 github_token 字段");
        assert_eq!(json["github_token_set"], true);
        assert_eq!(json["github_owner"], "octo");
        assert_eq!(json["github_repo"], "repo");

        // 落库：token 已加密（test_state 未注入主密钥 → 明文降级，但写语义一致）
        let id = json["id"].as_str().expect("created workspace id");
        let ws = db.agent_get_workspace(id).await.unwrap().unwrap();
        assert_eq!(ws.github_owner.as_deref(), Some("octo"));
        assert_eq!(ws.github_repo.as_deref(), Some("repo"));
        assert_eq!(ws.github_token.as_deref(), Some("ghp_secret_123"));
    }

    #[tokio::test]
    async fn test_create_workspace_empty_github_fields_are_null() {
        let (state, _db) = test_state().await;
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
                agent_config_overrides: None,
                claude_tier_models: None,
                github_token: Some("".into()), // 空串归一化 → 未配置
                github_owner: Some("".into()),
                github_repo: Some("".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["github_token_set"], false);
        assert_eq!(json["github_owner"], serde_json::Value::Null);
        assert_eq!(json["github_repo"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_update_workspace_github_keep_or_replace() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
        .await
        .unwrap();
        db.agent_set_workspace_github("w1", Some("ghp_existing"), Some("octo"), Some("repo"))
            .await
            .unwrap();

        // 1) 空串 / 缺省 → 保持已存值（COALESCE 语义）
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
                claude_tier_models: None,
                github_token: Some("".into()),
                github_owner: Some("".into()),
                github_repo: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.github_token.as_deref(),
            Some("ghp_existing"),
            "空串保持已存 token"
        );
        assert_eq!(
            ws.github_owner.as_deref(),
            Some("octo"),
            "空串保持已存 owner"
        );

        // 2) 非空 → 覆盖更新
        let resp = update_workspace(
            State(state),
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
                claude_tier_models: None,
                github_token: Some("ghp_new".into()),
                github_owner: Some("newowner".into()),
                github_repo: Some("newrepo".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.github_token.as_deref(), Some("ghp_new"));
        assert_eq!(ws.github_owner.as_deref(), Some("newowner"));
        assert_eq!(ws.github_repo.as_deref(), Some("newrepo"));
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
            "w1", "p", "nas", "host", "/p", None, None, "gemini", None, None, None, None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
            None,
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
                agent_config_overrides: Some(Some("{}".into())),
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
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
                agent_config_overrides: Some(Some("not-json".into())),
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_config_overrides.as_deref(), Some("{}"));
    }

    /// 显式 JSON null 清空语义：前端传 null 将 agent_config_overrides 设为 NULL。
    #[tokio::test]
    async fn test_update_workspace_config_overrides_null_clears() {
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
            None,
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.agent_config_overrides.as_deref(),
            Some(r#"{"mode":"plan"}"#)
        );
        // 显式 null → 清空（DB 列设为 NULL）
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
                agent_config_overrides: Some(None),
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.agent_config_overrides, None,
            "null should clear overrides"
        );

        // 省略字段（None）→ 保持已清空的 NULL
        let resp = update_workspace(
            State(state),
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
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_config_overrides, None);
    }

    #[test]
    fn test_validate_tier_models() {
        // 合法：空串（归一化清空）、空对象（合法值，等价无映射）、白名单 key + string 值
        assert!(validate_tier_models(""));
        assert!(validate_tier_models("{}"));
        assert!(validate_tier_models(r#"{"opus":"model:x"}"#));
        assert!(validate_tier_models(
            r#"{"opus":"model:x","sonnet":"group:y","haiku":"plain-alias"}"#
        ));
        assert!(validate_tier_models(r#"{"subagent":"model:z"}"#));
        // 非法：白名单外 key / 非 string 值 / 非 object / 非法 JSON
        assert!(!validate_tier_models(r#"{"opusx":"model:x"}"#));
        assert!(!validate_tier_models(r#"{"opus":"a","b":"c"}"#));
        assert!(!validate_tier_models(r#"{"opus":1}"#));
        assert!(!validate_tier_models(r#"{"opus":null}"#));
        assert!(!validate_tier_models(r#"["opus"]"#));
        assert!(!validate_tier_models("not-json"));
    }

    #[tokio::test]
    async fn test_create_workspace_invalid_tier_models_400() {
        let (state, _db) = test_state().await;
        for bad in [
            r#"{"opusx":"model:x"}"#, // 白名单外 key
            r#"{"opus":1}"#,          // 非 string 值
            r#"["opus"]"#,            // 非 object
        ] {
            let resp = create_workspace(
                State(state.clone()),
                Json(CreateWorkspaceRequest {
                    name: "p".into(),
                    client_id: "nas".into(),
                    runtime_type: "host".into(),
                    root_path: "/p".into(),
                    docker_image: None,
                    docker_container_id: None,
                    agent_type: "claude-code".into(),
                    agent_path: None,
                    llm_model_id: None,
                    agent_config_overrides: None,
                    claude_tier_models: Some(bad.into()),
                    github_token: None,
                    github_owner: None,
                    github_repo: None,
                }),
            )
            .await
            .into_response();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "invalid tier models must 400: {bad}"
            );
        }
    }

    #[tokio::test]
    async fn test_update_workspace_tier_models_clear_semantics() {
        // 与 agent_config_overrides 同三态语义：省略（None）保持、显式 null / 空串
        // 清空（DB 列设为 NULL）、非空值写入、非法值 400 且原值不变。
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1",
            "p",
            "nas",
            "host",
            "/p",
            None,
            None,
            "claude-code",
            None,
            None,
            None,
            Some(r#"{"opus":"model:x"}"#),
        )
        .await
        .unwrap();
        let mk_update = |tier: Option<Option<String>>| UpdateWorkspaceRequest {
            name: "p".into(),
            root_path: "/p".into(),
            system_prompt: None,
            approval_mode: None,
            agent_type: None,
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: tier,
            github_token: None,
            github_owner: None,
            github_repo: None,
        };

        // 1) 省略字段（None）→ 保持原值
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(mk_update(None)),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.claude_tier_models.as_deref(),
            Some(r#"{"opus":"model:x"}"#)
        );

        // 2) 空串 → 归一化为清空（DB 列设为 NULL）
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(mk_update(Some(Some(String::new())))),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.claude_tier_models, None, "空串应归一化为清空");

        // 3) 显式 null → 清空（先重新写入值再清空）
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(mk_update(Some(Some(r#"{"opus":"model:x"}"#.into())))),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(mk_update(Some(None))),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.claude_tier_models, None, "显式 null 应清空");

        // 4) 非法值 → 400 且原值（步骤 3 后的 NULL）不变
        let resp = update_workspace(
            State(state.clone()),
            Path("w1".to_string()),
            Json(mk_update(Some(Some(r#"{"haiku":2}"#.into())))),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.claude_tier_models, None);
    }

    /// create 空串归一化：agent_path/llm_model_id/agent_config_overrides 传
    /// `Some("")` → 落库为 NULL（未设置 ≠ 空串，读取方与注入路径统一按 None 处理）。
    #[tokio::test]
    async fn test_create_workspace_empty_strings_normalized_to_null() {
        let (state, db) = test_state().await;
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
                agent_path: Some(String::new()),
                llm_model_id: Some(String::new()),
                agent_config_overrides: Some(String::new()),
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_list_workspaces().await.unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].agent_path, None, "空串应归一化为 NULL: {ws:?}");
        assert_eq!(ws[0].llm_model_id, None, "空串应归一化为 NULL");
        assert_eq!(ws[0].agent_config_overrides, None, "空串应归一化为 NULL");
    }

    /// update 空串归一化：agent_path/llm_model_id 空串 → filter 归一化为 None →
    /// DB COALESCE 保持原值。agent_config_overrides 空串归一化为清空（向后兼容旧前端
    /// 惯例：空串 = 清空；新前端用 null 显式清空）。
    #[tokio::test]
    async fn test_update_workspace_empty_string_keeps_existing_value() {
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
            Some("/opt/agent"),
            Some("model-1"),
            Some(r#"{"mode":"plan"}"#),
            None,
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
                agent_type: None,
                agent_path: Some(String::new()),
                llm_model_id: Some(String::new()),
                agent_config_overrides: Some(Some(String::new())),
                claude_tier_models: None,
                github_token: None,
                github_owner: None,
                github_repo: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.agent_path.as_deref(),
            Some("/opt/agent"),
            "空串 update 应保持原值: {ws:?}"
        );
        assert_eq!(ws.llm_model_id.as_deref(), Some("model-1"));
        // agent_config_overrides 空串 = 清空（向后兼容旧前端惯例）
        assert_eq!(ws.agent_config_overrides, None);
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
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
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
            State(state.clone()),
            Path("ghost".to_string()),
            Query(GitDiffQuery {
                path: None,
                cached: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let responses = vec![
            get_git_branches(State(state.clone()), Path("ghost".to_string()))
                .await
                .into_response(),
            get_git_log(
                State(state.clone()),
                Path("ghost".to_string()),
                Query(GitLogQuery { limit: None }),
            )
            .await
            .into_response(),
            get_git_show(
                State(state.clone()),
                Path("ghost".to_string()),
                Query(GitShowQuery { rev: None }),
            )
            .await
            .into_response(),
            get_git_stash(State(state.clone()), Path("ghost".to_string()))
                .await
                .into_response(),
        ];
        for resp in responses {
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "git read endpoint on ghost workspace should 404"
            );
        }
    }

    #[tokio::test]
    async fn test_fs_endpoints_client_offline_returns_503() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
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
            State(state.clone()),
            Path("w1".to_string()),
            Query(GitDiffQuery {
                path: None,
                cached: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        // cached diff 走 GitExec：客户端离线 → 503
        let resp = get_git_diff(
            State(state.clone()),
            Path("w1".to_string()),
            Query(GitDiffQuery {
                path: None,
                cached: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        // GitExec 读端点：客户端离线 → 503
        let responses = vec![
            get_git_branches(State(state.clone()), Path("w1".to_string()))
                .await
                .into_response(),
            get_git_log(
                State(state.clone()),
                Path("w1".to_string()),
                Query(GitLogQuery { limit: None }),
            )
            .await
            .into_response(),
            get_git_show(
                State(state.clone()),
                Path("w1".to_string()),
                Query(GitShowQuery { rev: None }),
            )
            .await
            .into_response(),
            get_git_stash(State(state.clone()), Path("w1".to_string()))
                .await
                .into_response(),
        ];
        for resp in responses {
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "git read endpoint with offline client should 503"
            );
        }
    }

    #[tokio::test]
    async fn test_fs_file_requires_path() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
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
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
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
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
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
            None,
            false,
            false,
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

    // ── Git 面板（GitExec）─────────────────────────────────────────────────────

    /// 注册一个客户端（版本可指定）到 registry，模拟在线状态。
    async fn register_client(state: &ApiState, db: &Database, name: &str, version: Option<&str>) {
        db.save_server_auth("secret").await.unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(32);
        state
            .server_state
            .client_registry
            .as_ref()
            .expect("client_registry")
            .register(name, None, version.map(str::to_string), "secret", tx)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_git_read_old_client_409_upgrade() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
        .await
        .unwrap();
        register_client(&state, &db, "nas", Some("0.4.0")).await;
        // 老客户端（<0.5.0）：GitExec 新变体会断其控制连接，服务端短路 409 提示升级
        let resp = get_git_branches(State(state), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["needs_upgrade"], true);
    }

    #[tokio::test]
    async fn test_git_read_new_client_offline_after_exec_503() {
        // 客户端在线（0.5.0）但隧道执行失败（无人消费控制通道 → 超时）→ 503。
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
        .await
        .unwrap();
        register_client(&state, &db, "nas", Some("0.5.0")).await;
        let resp = get_git_branches(State(state), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_git_write_safe_mode_approval_flow() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
        .await
        .unwrap();
        // 默认 approval_mode=safe：git commit 属 SafeWrite → 未确认 409 needs_approval
        let resp = post_git_commit(
            State(state.clone()),
            Path("w1".to_string()),
            Json(GitCommitRequest {
                message: "fix".into(),
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
        assert!(json["summary"].as_str().unwrap().contains("git commit"));

        // 确认后重发 → 通过审批 → 客户端离线（未注册）503
        let resp = post_git_commit(
            State(state),
            Path("w1".to_string()),
            Json(GitCommitRequest {
                message: "fix".into(),
                approved: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_git_write_dangerous_auto_write_approval() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
        .await
        .unwrap();
        db.agent_update_workspace(
            "w1",
            "proj",
            "/p",
            None,
            Some("auto_write"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .await
        .unwrap();
        // auto_write：普通写放行（reset --soft → SafeWrite），危险写需审（reset --hard）
        let resp = post_git_reset(
            State(state.clone()),
            Path("w1".to_string()),
            Json(GitResetRequest {
                rev: Some("HEAD~1".into()),
                mode: "soft".into(),
                approved: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE); // 免审直接执行 → 离线 503

        let resp = post_git_reset(
            State(state.clone()),
            Path("w1".to_string()),
            Json(GitResetRequest {
                rev: None,
                mode: "hard".into(),
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

        // 确认后重发 → 免审放行 → 离线 503
        let resp = post_git_reset(
            State(state),
            Path("w1".to_string()),
            Json(GitResetRequest {
                rev: None,
                mode: "hard".into(),
                approved: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_git_write_plan_validation_400() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
        .await
        .unwrap();
        // 路径注入在 plan 阶段 fail-closed → 400（不触碰隧道）
        let resp = post_git_stage(
            State(state.clone()),
            Path("w1".to_string()),
            Json(GitStageRequest {
                paths: vec!["../etc/passwd".into()],
                approved: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // 空 paths → 400
        let resp = post_git_stage(
            State(state.clone()),
            Path("w1".to_string()),
            Json(GitStageRequest {
                paths: vec![],
                approved: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // 非法 reset mode → 400
        let resp = post_git_reset(
            State(state),
            Path("w1".to_string()),
            Json(GitResetRequest {
                rev: None,
                mode: "danger".into(),
                approved: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_git_write_old_client_409_upgrade() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
        .await
        .unwrap();
        register_client(&state, &db, "nas", Some("0.4.0")).await;
        // 已确认重发：审批跳过，但老客户端版本门控 → 409 升级提示
        let resp = post_git_push(
            State(state),
            Path("w1".to_string()),
            Some(Json(GitApprovedBody {
                approved: Some(true),
            })),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["needs_upgrade"], true);
    }

    #[tokio::test]
    async fn test_git_write_empty_body_pull() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
        .await
        .unwrap();
        // 空 body（无 JSON）也能解出 GitApprovedBody（approved=None）
        let resp = post_git_pull(State(state), Path("w1".to_string()), None)
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT); // safe 档 pull 需审
    }

    #[test]
    fn test_parse_branches() {
        let out = parse_branches("main\t*\torigin/main\nfeature\t\torigin/feature\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["name"], "main");
        assert_eq!(out[0]["current"], true);
        assert_eq!(out[0]["upstream"], "origin/main");
        assert_eq!(out[1]["name"], "feature");
        assert_eq!(out[1]["current"], false);
        assert_eq!(out[1]["upstream"], "origin/feature");
    }

    #[test]
    fn test_parse_branches_no_upstream() {
        let out = parse_branches("main\t*\t\nlocal\t\t\n");
        assert_eq!(out[0]["upstream"], serde_json::Value::Null);
        assert_eq!(out[1]["current"], false);
    }

    #[test]
    fn test_parse_commits() {
        let content = "\
abc\0ab\0Alice\02026-08-14T10:00:00+08:00\0first commit\n\
def\0de\0Bob\02026-08-13T09:00:00+08:00\0second commit\n";
        let out = parse_commits(content);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["hash"], "abc");
        assert_eq!(out[0]["short"], "ab");
        assert_eq!(out[0]["author"], "Alice");
        assert_eq!(out[0]["date"], "2026-08-14T10:00:00+08:00");
        assert_eq!(out[0]["subject"], "first commit");
        assert_eq!(out[1]["subject"], "second commit");
    }

    #[test]
    fn test_parse_stashes() {
        let out = parse_stashes("stash@{0}: WIP on main: 1a2b3c fix\nstash@{1}: On feature: wip\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["index"], 0);
        assert_eq!(out[0]["message"], "WIP on main: 1a2b3c fix");
        assert_eq!(out[1]["index"], 1);
        assert_eq!(out[1]["message"], "On feature: wip");
        // 无关行 / 空行忽略
        assert!(parse_stashes("\n  \nnot a stash\n").is_empty());
    }
}
