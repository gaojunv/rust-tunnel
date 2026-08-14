//! Agent workbench 请求 / 响应 / query 类型（DTO）。
use serde::Deserialize;

use crate::persistence::db::agent::AgentMessageRecord;

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub client_id: String,
    pub runtime_type: String, // "host" | "docker"
    pub root_path: String,
    pub docker_image: Option<String>,
    /// Pre-started container to `docker exec` into. MVP: container lifecycle is
    /// out of scope — the user must start the container and supply its id here.
    pub docker_container_id: Option<String>,
    /// ACP 远程 agent 引擎：空串（缺省，向后兼容）为内置 runner；非空取
    /// `gemini` / `claude-code` / `opencode`（见 `spawner::agent_command`）。
    #[serde(default)]
    pub agent_type: String,
    /// ACP agent 可执行文件路径；缺省 None 时依赖 PATH 查找。
    #[serde(default)]
    pub agent_path: Option<String>,
    /// workspace 默认 LLM 模型 id（`llm_models.id`，ACP 会话启动时必需）。
    #[serde(default)]
    pub llm_model_id: Option<String>,
    /// ACP 引擎选项覆盖（JSON map：config_id → value）；空串归一化为 None。
    #[serde(default)]
    pub agent_config_overrides: Option<String>,
    /// GitHub Actions 集成：token（API 层加密后落库；空串视为未配置）。
    #[serde(default)]
    pub github_token: Option<String>,
    /// GitHub 仓库定位（手工填写；缺省时经隧道 `git remote get-url origin` 探测）。
    #[serde(default)]
    pub github_owner: Option<String>,
    #[serde(default)]
    pub github_repo: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateWorkspaceRequest {
    pub name: String,
    pub root_path: String,
    pub system_prompt: Option<String>,
    pub approval_mode: Option<String>,
    /// ACP 字段，COALESCE 语义：缺省 None 保持原值。`agent_type` 空串表示切回内置
    /// runner；`agent_path`/`llm_model_id` 空串视为忽略（本迭代不支持清空）；
    /// `agent_config_overrides` 显式 `"{}"` 清空、空串视为忽略。
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub agent_path: Option<String>,
    #[serde(default)]
    pub llm_model_id: Option<String>,
    /// ACP 引擎选项覆盖（JSON map：config_id → value）；None 保持原值，`"{}"` 清空。
    #[serde(default)]
    pub agent_config_overrides: Option<String>,
    /// GitHub Actions 集成字段，COALESCE 语义：缺省（None）/ 空串保持原值，
    /// 非空更新。token 由 API 层加密后落库。
    #[serde(default)]
    pub github_token: Option<String>,
    #[serde(default)]
    pub github_owner: Option<String>,
    #[serde(default)]
    pub github_repo: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub title: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSessionModelRequest {
    /// 空串表示清除会话模型，回退到默认解析。
    pub model: String,
}

#[derive(Debug, serde::Serialize)]
pub struct DefaultModelResponse {
    pub model: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSessionRequest {
    pub title: String,
}

/// `GET /api/agent/sessions/:id/messages` 的 query 参数。
#[derive(Debug, Default, Deserialize)]
pub struct ListMessagesParams {
    /// 返回条数；缺省 200，handler 层 clamp 到 [1, 500]。
    #[serde(default)]
    pub limit: Option<i64>,
    /// 游标：返回该消息**更早**（rowid 更小）的最近 `limit` 条，游标本身不含。
    #[serde(default)]
    pub before: Option<String>,
}

/// `GET /api/agent/sessions/:id/messages` 的响应体：分页消息 + 是否还有更早。
#[derive(Debug, serde::Serialize)]
pub struct ListMessagesResponse {
    pub messages: Vec<AgentMessageRecord>,
    pub has_more: bool,
}

#[derive(Debug, Deserialize)]
pub struct AgentWsQuery {
    pub session_id: String,
    pub token: Option<String>,
}

/// 全局通知 WS 的 query：无 session 维度，仅 JWT token（复用 `agent_ws` 的鉴权方式）。
#[derive(Debug, Deserialize)]
pub struct NotificationsWsQuery {
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TerminalWsQuery {
    pub workspace_id: String,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceFilesQuery {
    pub q: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct FsPathQuery {
    /// 相对工作区根的路径；tree 默认 "."，file 必填。
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PutFsFileRequest {
    pub path: String,
    pub content: String,
    /// 前端确认后重发携带：跳过审批检查（用户已在面板确认）。
    pub approved: Option<bool>,
}

/// `GET /api/agent/workspaces/:id/git/log` 的 query 参数。
#[derive(Debug, Default, Deserialize)]
pub struct GitLogQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/agent/workspaces/:id/git/diff` 的 query 参数（`cached=true` 取 staged diff）。
#[derive(Debug, Default, Deserialize)]
pub struct GitDiffQuery {
    pub path: Option<String>,
    #[serde(default)]
    pub cached: Option<bool>,
}

/// `GET /api/agent/workspaces/:id/git/show` 的 query 参数。
#[derive(Debug, Default, Deserialize)]
pub struct GitShowQuery {
    pub rev: Option<String>,
}

/// 无参数写操作（pull/push）的请求体：允许空 body / `{}` / 仅 `approved`。
#[derive(Debug, Default, Deserialize)]
pub struct GitApprovedBody {
    #[serde(default)]
    pub approved: Option<bool>,
}

/// 写操作统一审批标记：`approved: true` 表示用户已在面板确认后重发。
/// 面板所有 Git 写端点的请求体都携带该字段。
macro_rules! git_write_body {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Debug, Deserialize)]
        pub struct $name {
            $(pub $field: $ty,)*
            #[serde(default)]
            pub approved: Option<bool>,
        }
    };
}

git_write_body!(GitStageRequest {
    paths: Vec<String>,
});
git_write_body!(GitUnstageRequest {
    paths: Vec<String>,
});
git_write_body!(GitCommitRequest {
    message: String,
});
git_write_body!(GitCheckoutRequest {
    branch: String,
    create: Option<bool>,
});
git_write_body!(GitBranchDeleteRequest {
    branch: String,
    force: Option<bool>,
});
git_write_body!(GitRevertRequest {
    rev: String,
});
git_write_body!(GitResetRequest {
    rev: Option<String>,
    mode: String,
});
git_write_body!(GitStashPushRequest {
    message: Option<String>,
});
git_write_body!(GitStashIndexRequest {
    index: usize,
});

// ── GitHub Actions 面板 ───────────────────────────────────────

/// `GET /api/agent/workspaces/:id/github/repo` 的 query：`?refresh=true` 强制重探
/// （忽略 5 分钟内存缓存）。
#[derive(Debug, Default, Deserialize)]
pub struct GithubRepoQuery {
    #[serde(default)]
    pub refresh: Option<bool>,
}

/// `GET /api/agent/workspaces/:id/github/runs` 的 query 参数。
#[derive(Debug, Default, Deserialize)]
pub struct GithubRunsQuery {
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub per_page: Option<usize>,
}

/// `POST /api/agent/workspaces/:id/github/workflows/:workflow_id/dispatch` 请求体。
#[derive(Debug, Default, Deserialize)]
pub struct GithubDispatchBody {
    /// 触发目标分支 / tag（GitHub workflow_dispatch 的 `ref`，**必填**）。
    #[serde(default)]
    pub r#ref: Option<String>,
    /// 工作流输入（JSON object）；缺省为空对象。
    #[serde(default)]
    pub inputs: Option<serde_json::Value>,
    /// 写操作统一审批标记：前端确认后重发携带 `approved: true`。
    #[serde(default)]
    pub approved: Option<bool>,
}

/// 无参写操作（rerun/cancel）的请求体：允许空 body / `{}` / 仅 `approved`。
#[derive(Debug, Default, Deserialize)]
pub struct GithubApprovedBody {
    #[serde(default)]
    pub approved: Option<bool>,
}
