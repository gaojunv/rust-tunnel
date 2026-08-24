//! AI 工作台 GitHub Actions 面板后端：`/api/agent/workspaces/:id/github/*` 端点。
//!
//! 统一前置（[`load_github_ctx`]）：workspace 无 token → 400「github_token 未配置」；
//! 无 owner/repo（手填 + 隧道探测都失败）→ 400。写操作（dispatch/rerun/cancel）
//! 额外走 409 审批模式：任何 approval_mode 下未带 `approved: true` 一律返回
//! `{needs_approval, summary}`，前端确认后带 `approved: true` 重发放行。
//!
//! 安全约束：token 只经内存中的 `GitHubClient` 注入 Authorization 头，日志与
//! 错误响应绝不包含 token 明文；workspace 记录序列化时也永不输出 token（见
//! `db::agent::AgentWorkspaceRecord` 手写 `Serialize`）。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use tokio::sync::Mutex;

use crate::agent::git_plan;
use crate::agent::github::{GitHubClient, GitHubError};
use crate::agent::AgentState;
use crate::db::agent::AgentWorkspaceRecord;
use crate::mgmt::api::ApiState;

use super::dto::{GithubApprovedBody, GithubDispatchBody, GithubRepoQuery, GithubRunsQuery};

/// 隧道探测结果的内存缓存有效期（5 分钟；`?refresh=true` 强制重探）。
const REPO_PROBE_TTL: Duration = Duration::from_secs(300);

/// 探测缓存条目。
struct RepoCacheEntry {
    at: Instant,
    owner: String,
    repo: String,
}

/// 按 workspace_id 缓存「经隧道探测出的 owner/repo」，避免每次请求都打隧道。
static REPO_CACHE: LazyLock<Mutex<HashMap<String, RepoCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 从 GitHub 错误归一化出 HTTP 响应。映射：401 → 400 token 无效；403/429 → 429
/// 限流；其余上游 4xx/5xx → 502 + 上游 message；网络错误 → 502。
fn github_error_response(e: &GitHubError) -> axum::response::Response {
    match e {
        GitHubError::Unauthorized => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "github_token 无效或已过期（401），请在 workspace 设置中更新"})),
        )
            .into_response(),
        GitHubError::RateLimited { status, message } => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": format!("GitHub 请求被限流（HTTP {status}）：{message}")})),
        )
            .into_response(),
        GitHubError::Api { message, .. } => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": message})),
        )
            .into_response(),
        GitHubError::Network(msg) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("GitHub 网络错误：{msg}")})),
        )
            .into_response(),
    }
}

/// 写操作统一审批门：未确认 → 409 `{needs_approval, summary}`；已确认 → None 放行。
/// 与 git 面板的 409 审批模式一致，但**不区分 approval_mode**——GitHub 远程写
/// 操作（触发/重跑/取消 CI）一律需要用户确认。
fn approve_or_409(approved: bool, summary: &str) -> Option<axum::response::Response> {
    if approved {
        return None;
    }
    Some(
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "needs_approval": true,
                "summary": summary,
            })),
        )
            .into_response(),
    )
}

/// 校验 owner/repo 名字符：GitHub 仓库/所有者名只允许字母数字与 `-_.`（手工填写
/// 时防御 URL 注入——`/`、`?`、空白等一律拒绝）。探测结果（parse_remote_url
/// 输出）天然满足。
fn is_valid_github_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// 解密 workspace 的 github_token：无 token → `Ok(None)`；解密失败（主密钥缺失 /
/// 轮换 / 数据损坏）→ 400。
async fn decrypt_workspace_token(
    state: &ApiState,
    ws: &AgentWorkspaceRecord,
) -> Result<Option<String>, axum::response::Response> {
    let Some(stored) = ws.github_token.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let cipher = super::agent_cipher(state).await;
    match crate::llm::crypto::decrypt_field(cipher.as_ref(), stored) {
        Ok(t) => Ok(Some(t)),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("github_token 解密失败：{e}")})),
        )
            .into_response()),
    }
}

/// 经隧道 `git remote get-url origin` + `git_plan::parse_remote_url` 探测仓库。
/// 客户端离线 / 老版本（不支持 GitExec）/ 非 git 仓库 / exec 失败一律 None。
async fn probe_repo_via_tunnel(
    agent: &AgentState,
    ws: &AgentWorkspaceRecord,
) -> Option<(String, String)> {
    if ws.runtime_type == "docker" && ws.docker_container_id.is_none() {
        return None;
    }
    match agent.registry.client_handle(&ws.client_id).await {
        None => return None,
        Some(entry) => {
            if !crate::agent::runner::client_supports_git_exec(entry.client_version.as_deref()) {
                return None;
            }
        }
    }
    let planned = git_plan::plan(&[
        "remote".to_string(),
        "get-url".to_string(),
        "origin".to_string(),
    ])
    .ok()?;
    let result = crate::agent::executor::exec_on_client(
        agent,
        &ws.id,
        &ws.client_id,
        &ws.root_path,
        ws.docker_container_id.as_deref(),
        rust_tunnel_common::AgentCommand::GitExec { args: planned.args },
    )
    .await;
    match result {
        rust_tunnel_common::AgentResult::FileContent { content } => {
            git_plan::parse_remote_url(content.trim())
        }
        _ => None,
    }
}

/// 解析仓库定位：优先 workspace 手填的 owner/repo（两者齐全才生效），否则经隧道
/// 探测。探测结果带 5 分钟内存缓存；`refresh=true` 强制重探。
async fn resolve_owner_repo(
    agent: &AgentState,
    ws: &AgentWorkspaceRecord,
    refresh: bool,
) -> Option<(String, String)> {
    if let (Some(o), Some(r)) = (&ws.github_owner, &ws.github_repo) {
        if is_valid_github_name(o) && is_valid_github_name(r) {
            return Some((o.clone(), r.clone()));
        }
    }
    if !refresh {
        let cache = REPO_CACHE.lock().await;
        if let Some(entry) = cache.get(&ws.id) {
            if entry.at.elapsed() < REPO_PROBE_TTL {
                return Some((entry.owner.clone(), entry.repo.clone()));
            }
        }
    }
    let probed = probe_repo_via_tunnel(agent, ws).await;
    if let Some((owner, repo)) = &probed {
        REPO_CACHE.lock().await.insert(
            ws.id.clone(),
            RepoCacheEntry {
                at: Instant::now(),
                owner: owner.clone(),
                repo: repo.clone(),
            },
        );
    }
    probed
}

/// GitHub 调用上下文：已解密的 token（仅内存）+ 仓库定位 + 就绪的客户端。
struct GithubCtx {
    client: GitHubClient,
    owner: String,
    repo: String,
}

/// 统一前置：加载 workspace（404）、解密 token（无 token → 400）、解析 owner/repo
/// （手填 + 探测都失败 → 400）。写操作在拿到 ctx 后再做 409 审批判定。
async fn load_github_ctx(
    state: &ApiState,
    workspace_id: &str,
) -> Result<GithubCtx, axum::response::Response> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    };
    let ws = match agent.db.agent_get_workspace(workspace_id).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return Err(StatusCode::NOT_FOUND.into_response()),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    };
    let token = match decrypt_workspace_token(state, &ws).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "github_token 未配置：请在 workspace 设置中填写 GitHub token",
                })),
            )
                .into_response());
        }
        Err(resp) => return Err(resp),
    };
    let (owner, repo) = match resolve_owner_repo(agent, &ws, false).await {
        Some(x) => x,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "无法确定 GitHub 仓库：请在 workspace 设置 github_owner/github_repo，或确认客户端在线且工作区为 git 仓库",
                })),
            )
                .into_response());
        }
    };
    Ok(GithubCtx {
        client: GitHubClient::new(agent.github_base_url(), &token),
        owner,
        repo,
    })
}

/// GET /api/agent/workspaces/:id/github/repo
/// 仓库定位检测：`{configured, owner?, repo?, token_set, repo_info?}`。owner/repo
/// 优先取手填，否则经隧道探测（5 分钟缓存，`?refresh=true` 强制重探）；客户端离线
/// /非 git 仓库时 `configured:false` 但 `token_set` 照常返回。`repo_info` 仅在
/// token 已配置且 get_repo 成功时填充，失败保持 null（不把错误升级为整端点失败）。
pub async fn get_repo_info(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<GithubRepoQuery>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let ws = match agent.db.agent_get_workspace(&id).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let token_set = ws.github_token_set();
    match resolve_owner_repo(agent, &ws, query.refresh.unwrap_or(false)).await {
        Some((owner, repo)) => {
            let mut resp = serde_json::json!({
                "configured": true,
                "owner": owner,
                "repo": repo,
                "token_set": token_set,
                "repo_info": null,
            });
            if token_set {
                if let Ok(Some(token)) = decrypt_workspace_token(&state, &ws).await {
                    let client = GitHubClient::new(agent.github_base_url(), &token);
                    if let Ok(info) = client.get_repo(&owner, &repo).await {
                        resp["repo_info"] = info;
                    }
                }
            }
            Json(resp).into_response()
        }
        None => Json(serde_json::json!({
            "configured": false,
            "owner": ws.github_owner,
            "repo": ws.github_repo,
            "token_set": token_set,
            "repo_info": null,
        }))
        .into_response(),
    }
}

/// GET /api/agent/workspaces/:id/github/workflows — 代理 GitHub 工作流列表。
pub async fn list_workflows(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let ctx = match load_github_ctx(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match ctx.client.list_workflows(&ctx.owner, &ctx.repo).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => github_error_response(&e),
    }
}

/// GET /api/agent/workspaces/:id/github/runs?workflow_id=&per_page= — 代理运行列表。
pub async fn list_workflow_runs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<GithubRunsQuery>,
) -> impl IntoResponse {
    let ctx = match load_github_ctx(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let per_page = params.per_page.unwrap_or(20).clamp(1, 100);
    match ctx
        .client
        .list_workflow_runs(
            &ctx.owner,
            &ctx.repo,
            params.workflow_id.as_deref(),
            per_page,
        )
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => github_error_response(&e),
    }
}

/// GET /api/agent/workspaces/:id/github/runs/:run_id/jobs — 代理作业列表。
pub async fn list_run_jobs(
    State(state): State<ApiState>,
    Path((id, run_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let ctx = match load_github_ctx(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match ctx
        .client
        .list_run_jobs(&ctx.owner, &ctx.repo, &run_id)
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => github_error_response(&e),
    }
}

/// GET /api/agent/workspaces/:id/github/jobs/:job_id/logs — 作业日志（尾部截断）。
/// 响应 `{logs: string, truncated: bool}`。截取上限见 [`GitHubClient::get_job_logs`]。
pub async fn get_job_logs(
    State(state): State<ApiState>,
    Path((id, job_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let ctx = match load_github_ctx(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match ctx
        .client
        .get_job_logs(&ctx.owner, &ctx.repo, &job_id)
        .await
    {
        Ok((logs, truncated)) => {
            Json(serde_json::json!({ "logs": logs, "truncated": truncated })).into_response()
        }
        Err(e) => github_error_response(&e),
    }
}

/// POST /api/agent/workspaces/:id/github/workflows/:workflow_id/dispatch
/// 请求体 `{ref, inputs?, approved?}`。写操作走 409 审批模式。成功 → `{status:
/// "dispatched"}`。
pub async fn dispatch_workflow(
    State(state): State<ApiState>,
    Path((id, workflow_id)): Path<(String, String)>,
    Json(body): Json<GithubDispatchBody>,
) -> impl IntoResponse {
    let Some(ref_) = body.r#ref.filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "ref 必填（GitHub workflow_dispatch 需要目标分支/tag）",
            })),
        )
            .into_response();
    };
    let ctx = match load_github_ctx(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let summary = format!(
        "github workflow_dispatch: {}/{} workflows/{} (ref={})",
        ctx.owner, ctx.repo, workflow_id, ref_
    );
    if let Some(resp) = approve_or_409(body.approved.unwrap_or(false), &summary) {
        return resp;
    }
    match ctx
        .client
        .dispatch_workflow(&ctx.owner, &ctx.repo, &workflow_id, &ref_, body.inputs)
        .await
    {
        Ok(_) => Json(serde_json::json!({ "status": "dispatched" })).into_response(),
        Err(e) => github_error_response(&e),
    }
}

/// POST /api/agent/workspaces/:id/github/runs/:run_id/rerun — 写操作走 409 审批。
/// 请求体可为空 / `{}` / 仅 `approved`。成功 → `{status: "rerun_queued"}`。
pub async fn rerun_workflow(
    State(state): State<ApiState>,
    Path((id, run_id)): Path<(String, String)>,
    body: Option<Json<GithubApprovedBody>>,
) -> impl IntoResponse {
    let approved = body.and_then(|b| b.approved).unwrap_or(false);
    let ctx = match load_github_ctx(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let summary = format!("github rerun: {}/{} runs/{}", ctx.owner, ctx.repo, run_id);
    if let Some(resp) = approve_or_409(approved, &summary) {
        return resp;
    }
    match ctx
        .client
        .rerun_workflow(&ctx.owner, &ctx.repo, &run_id)
        .await
    {
        Ok(_) => Json(serde_json::json!({ "status": "rerun_queued" })).into_response(),
        Err(e) => github_error_response(&e),
    }
}

/// POST /api/agent/workspaces/:id/github/runs/:run_id/cancel — 写操作走 409 审批。
/// 请求体可为空 / `{}` / 仅 `approved`。成功 → `{status: "cancel_requested"}`。
pub async fn cancel_run(
    State(state): State<ApiState>,
    Path((id, run_id)): Path<(String, String)>,
    body: Option<Json<GithubApprovedBody>>,
) -> impl IntoResponse {
    let approved = body.and_then(|b| b.approved).unwrap_or(false);
    let ctx = match load_github_ctx(&state, &id).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let summary = format!("github cancel: {}/{} runs/{}", ctx.owner, ctx.repo, run_id);
    if let Some(resp) = approve_or_409(approved, &summary) {
        return resp;
    }
    match ctx.client.cancel_run(&ctx.owner, &ctx.repo, &run_id).await {
        Ok(_) => Json(serde_json::json!({ "status": "cancel_requested" })).into_response(),
        Err(e) => github_error_response(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthConfig;
    use crate::control_plane::ServerState;
    use crate::db::Database;
    use axum::routing::get;
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

    /// 在随机端口起 axum mock server，返回 base_url；并把 AgentState 的
    /// github_base_url 指向它。
    async fn mock_state_with_base(routes: axum::Router) -> (ApiState, Database, String) {
        let (mut state, db) = test_state().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        let base = format!("http://{}", addr);
        if let Some(agent) = state.server_state.agent_state.take() {
            state.server_state.agent_state = Some(agent.with_github_base_url(base.clone()));
        }
        (state, db, base)
    }

    /// 造一个 workspace：token（无主密钥路径 → 明文降级存储）+ 手填 owner/repo
    /// （绕过隧道探测，端点测试不需要真的跑客户端）。
    async fn seed_workspace(
        db: &Database,
        token: Option<&str>,
        owner: Option<&str>,
        repo: Option<&str>,
    ) -> String {
        let id = "w1".to_string();
        db.agent_create_workspace(
            &id, "proj", "nas", "host", "/p", None, None, "", None, None, None, None,
        )
        .await
        .unwrap();
        db.agent_set_workspace_github(&id, token, owner, repo)
            .await
            .unwrap();
        id
    }

    // ── 仓库定位解析（resolve_owner_repo）───────────────────────

    #[tokio::test]
    async fn test_resolve_owner_repo_hand_filled_wins() {
        let (state, db) = test_state().await;
        let agent = state.server_state.agent_state.clone().expect("agent_state");
        let id = seed_workspace(&db, None, Some("octo"), Some("repo")).await;
        let ws = db.agent_get_workspace(&id).await.unwrap().unwrap();
        // 手填 owner/repo 生效（无需客户端在线 / 隧道探测）
        let resolved = resolve_owner_repo(&agent, &ws, false).await;
        assert_eq!(resolved, Some(("octo".to_string(), "repo".to_string())));
    }

    #[tokio::test]
    async fn test_resolve_owner_repo_invalid_name_falls_back_to_probe() {
        let (state, db) = test_state().await;
        let agent = state.server_state.agent_state.clone().expect("agent_state");
        // owner 含 '/'（URL 注入形态）→ 视为无效，不直接使用；客户端离线 → 探测 None
        let id = seed_workspace(&db, None, Some("octo/inject"), Some("repo")).await;
        let ws = db.agent_get_workspace(&id).await.unwrap().unwrap();
        let resolved = resolve_owner_repo(&agent, &ws, false).await;
        assert_eq!(resolved, None, "非法 owner 名不得直用，离线探测失败 → None");
    }

    #[test]
    fn test_is_valid_github_name() {
        assert!(is_valid_github_name("octo"));
        assert!(is_valid_github_name("my-repo_2.0"));
        assert!(!is_valid_github_name(""));
        assert!(!is_valid_github_name("octo/repo"));
        assert!(!is_valid_github_name("a b"));
        assert!(!is_valid_github_name("a?b"));
        assert!(!is_valid_github_name(".."));
    }

    // ── 前置校验（不依赖 mock 的 HTTP）──────────────────────────

    #[tokio::test]
    async fn test_token_missing_returns_400() {
        let (state, db) = test_state().await;
        seed_workspace(&db, None, Some("octo"), Some("repo")).await;

        let resp = list_workflows(State(state), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("github_token 未配置"));
    }

    #[tokio::test]
    async fn test_owner_repo_missing_returns_400() {
        let (state, db) = test_state().await;
        // 有 token、无手填 owner/repo，且客户端离线（registry 无该客户端）→ 探测失败
        seed_workspace(&db, Some("ghp_test"), None, None).await;

        let resp = list_workflows(State(state), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("无法确定 GitHub 仓库"));
    }

    #[tokio::test]
    async fn test_workspace_not_found_returns_404() {
        let (state, _db) = test_state().await;
        let resp = list_workflows(State(state), Path("ghost".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── repo 检测端点 ──────────────────────────────────────────

    #[tokio::test]
    async fn test_repo_info_hand_filled_no_token() {
        let (state, db) = test_state().await;
        seed_workspace(&db, None, Some("octo"), Some("repo")).await;
        let resp = get_repo_info(
            State(state),
            Path("w1".to_string()),
            Query(GithubRepoQuery { refresh: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["configured"], true);
        assert_eq!(json["owner"], "octo");
        assert_eq!(json["repo"], "repo");
        assert_eq!(json["token_set"], false);
        assert_eq!(json["repo_info"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_repo_info_unconfigured_returns_configured_false() {
        let (state, db) = test_state().await;
        // 无 token、无手填、客户端离线 → configured:false 但 token_set 照常返回
        seed_workspace(&db, None, None, None).await;
        let resp = get_repo_info(
            State(state),
            Path("w1".to_string()),
            Query(GithubRepoQuery { refresh: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["configured"], false);
        assert_eq!(json["token_set"], false);
        assert_eq!(json["owner"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_repo_info_with_token_fetches_repo_info() {
        let (state, db, _base) = mock_state_with_base(axum::Router::new().route(
            "/repos/octo/repo",
            get(|| async {
                axum::Json(serde_json::json!({
                    "full_name": "octo/repo",
                    "default_branch": "main",
                    "private": false,
                }))
            }),
        ))
        .await;
        seed_workspace(&db, Some("ghp_test"), Some("octo"), Some("repo")).await;
        let resp = get_repo_info(
            State(state),
            Path("w1".to_string()),
            Query(GithubRepoQuery { refresh: None }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["configured"], true);
        assert_eq!(json["token_set"], true);
        assert_eq!(json["repo_info"]["full_name"], "octo/repo");
    }

    // ── 代理读端点 + 错误映射 ──────────────────────────────────

    #[tokio::test]
    async fn test_list_workflows_proxies_mock_response() {
        let (state, db, _base) = mock_state_with_base(axum::Router::new().route(
            "/repos/octo/repo/actions/workflows",
            get(|| async {
                axum::Json(serde_json::json!({
                    "total_count": 2,
                    "workflows": [
                        {"id": 1, "name": "CI", "path": ".github/workflows/ci.yml", "state": "active"},
                        {"id": 2, "name": "Lint", "path": ".github/workflows/lint.yml", "state": "active"},
                    ],
                }))
            }),
        ))
        .await;
        seed_workspace(&db, Some("ghp_test"), Some("octo"), Some("repo")).await;

        let resp = list_workflows(State(state), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total_count"], 2);
        assert_eq!(json["workflows"][0]["name"], "CI");
    }

    #[tokio::test]
    async fn test_proxy_upstream_404_maps_to_502_with_message() {
        let (state, db, _base) = mock_state_with_base(axum::Router::new().route(
            "/repos/octo/repo/actions/workflows",
            get(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    axum::Json(serde_json::json!({"message": "Not Found"})),
                )
            }),
        ))
        .await;
        seed_workspace(&db, Some("ghp_test"), Some("octo"), Some("repo")).await;

        let resp = list_workflows(State(state), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Not Found");
    }

    #[tokio::test]
    async fn test_proxy_401_maps_to_400() {
        let (state, db, _base) = mock_state_with_base(axum::Router::new().route(
            "/repos/octo/repo/actions/workflows",
            get(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"message": "Bad credentials"})),
                )
            }),
        ))
        .await;
        seed_workspace(&db, Some("ghp_bad"), Some("octo"), Some("repo")).await;

        let resp = list_workflows(State(state), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("无效"));
    }

    #[tokio::test]
    async fn test_runs_and_jobs_and_logs_proxy() {
        let (state, db, _base) = mock_state_with_base(
            axum::Router::new()
                .route(
                    "/repos/octo/repo/actions/runs",
                    get(|req: axum::http::Request<axum::body::Body>| async move {
                        let qs = req.uri().query().unwrap_or("").to_string();
                        assert!(qs.contains("workflow_id=ci.yml"), "qs: {qs}");
                        assert!(qs.contains("per_page=50"), "qs: {qs}");
                        axum::Json(serde_json::json!({"total_count": 1, "workflow_runs": [{"id": 9}]}))
                    }),
                )
                .route(
                    "/repos/octo/repo/actions/runs/9/jobs",
                    get(|| async {
                        axum::Json(serde_json::json!({"total_count": 1, "jobs": [{"id": 42, "name": "build"}]}))
                    }),
                )
                .route(
                    "/repos/octo/repo/actions/jobs/42/logs",
                    get(|| async { "step1 ok\nstep2 ok\n" }),
                ),
        )
        .await;
        seed_workspace(&db, Some("ghp_test"), Some("octo"), Some("repo")).await;

        let resp = list_workflow_runs(
            State(state.clone()),
            Path("w1".to_string()),
            Query(GithubRunsQuery {
                workflow_id: Some("ci.yml".into()),
                per_page: Some(50),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["workflow_runs"][0]["id"], 9);

        let resp = list_run_jobs(State(state.clone()), Path(("w1".into(), "9".into())))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = get_job_logs(State(state), Path(("w1".into(), "42".into())))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["logs"], "step1 ok\nstep2 ok\n");
        assert_eq!(json["truncated"], false);
    }

    // ── 写操作审批流 ────────────────────────────────────────────

    /// 记录 dispatch 是否真的打到 mock（approved 重发后应为 true）。
    #[tokio::test]
    async fn test_dispatch_approval_flow() {
        let dispatched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = dispatched.clone();
        let (state, db, _base) = mock_state_with_base(axum::Router::new().route(
            "/repos/octo/repo/actions/workflows/ci.yml/dispatches",
            axum::routing::post(move |body: axum::Json<serde_json::Value>| async move {
                assert_eq!(body.0["ref"], "main");
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                axum::http::StatusCode::NO_CONTENT
            }),
        ))
        .await;
        seed_workspace(&db, Some("ghp_test"), Some("octo"), Some("repo")).await;

        // 未确认 → 409 needs_approval + summary，且不打到 mock
        let resp = dispatch_workflow(
            State(state.clone()),
            Path(("w1".to_string(), "ci.yml".to_string())),
            Json(GithubDispatchBody {
                r#ref: Some("main".into()),
                inputs: None,
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
        assert!(json["summary"]
            .as_str()
            .unwrap()
            .contains("workflow_dispatch"));
        assert!(
            !dispatched.load(std::sync::atomic::Ordering::SeqCst),
            "未确认不得打到 GitHub"
        );

        // 确认后重发 → 放行，打到 mock → {status: dispatched}
        let resp = dispatch_workflow(
            State(state),
            Path(("w1".to_string(), "ci.yml".to_string())),
            Json(GithubDispatchBody {
                r#ref: Some("main".into()),
                inputs: Some(serde_json::json!({"env": "prod"})),
                approved: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "dispatched");
        assert!(dispatched.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_dispatch_missing_ref_400() {
        let (state, db) = test_state().await;
        seed_workspace(&db, Some("ghp_test"), Some("octo"), Some("repo")).await;
        let resp = dispatch_workflow(
            State(state),
            Path(("w1".to_string(), "ci.yml".to_string())),
            Json(GithubDispatchBody {
                r#ref: None,
                inputs: None,
                approved: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_rerun_cancel_approval_flow() {
        let rerun_hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rerun = rerun_hit.clone();
        let cancel = cancel_hit.clone();
        let (state, db, _base) = mock_state_with_base(
            axum::Router::new()
                .route(
                    "/repos/octo/repo/actions/runs/123/rerun",
                    axum::routing::post(move || async move {
                        rerun.store(true, std::sync::atomic::Ordering::SeqCst);
                        axum::http::StatusCode::NO_CONTENT
                    }),
                )
                .route(
                    "/repos/octo/repo/actions/runs/123/cancel",
                    axum::routing::post(move || async move {
                        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                        axum::http::StatusCode::ACCEPTED
                    }),
                ),
        )
        .await;
        seed_workspace(&db, Some("ghp_test"), Some("octo"), Some("repo")).await;

        // 未确认 → 409
        let resp = rerun_workflow(
            State(state.clone()),
            Path(("w1".to_string(), "123".to_string())),
            Some(Json(GithubApprovedBody { approved: None })),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert!(!rerun_hit.load(std::sync::atomic::Ordering::SeqCst));

        let resp = cancel_run(
            State(state.clone()),
            Path(("w1".to_string(), "123".to_string())),
            None, // 空 body 也允许
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert!(!cancel_hit.load(std::sync::atomic::Ordering::SeqCst));

        // 确认后重发 → 放行
        let resp = rerun_workflow(
            State(state.clone()),
            Path(("w1".to_string(), "123".to_string())),
            Some(Json(GithubApprovedBody {
                approved: Some(true),
            })),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(rerun_hit.load(std::sync::atomic::Ordering::SeqCst));

        let resp = cancel_run(
            State(state.clone()),
            Path(("w1".to_string(), "123".to_string())),
            Some(Json(GithubApprovedBody {
                approved: Some(true),
            })),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(cancel_hit.load(std::sync::atomic::Ordering::SeqCst));
    }
}
