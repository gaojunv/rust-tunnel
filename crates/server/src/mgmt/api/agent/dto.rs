//! Agent workbench 请求 / 响应 / query 类型（DTO）。
use serde::Deserialize;

use crate::db::agent::AgentMessageRecord;

#[derive(Debug, Deserialize)]
/// 创建工作区的请求体。
pub struct CreateWorkspaceRequest {
    /// 名称。
    pub name: String,
    /// 关联客户端 id。
    pub client_id: String,
    /// 运行时类型（`host`/`docker`）。
    pub runtime_type: String, // "host" | "docker"
    /// 工作区根路径。
    pub root_path: String,
    /// Docker 镜像名。
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
    /// Claude Code tier 模型映射（JSON object：key ∈ {opus,sonnet,haiku,subagent}，值为
    /// 模型引用 `model:<id>`/`group:<id>`/裸别名）；空串归一化为 None。
    #[serde(default)]
    pub claude_tier_models: Option<String>,
    /// GitHub Actions 集成：token（API 层加密后落库；空串视为未配置）。
    #[serde(default)]
    pub github_token: Option<String>,
    /// GitHub 仓库定位（手工填写；缺省时经隧道 `git remote get-url origin` 探测）。
    #[serde(default)]
    pub github_owner: Option<String>,
    #[serde(default)]
    /// GitHub 仓库名。
    pub github_repo: Option<String>,
}

#[derive(Debug, Deserialize)]
/// 更新工作区的请求体。
pub struct UpdateWorkspaceRequest {
    /// 名称。
    pub name: String,
    /// 工作区根路径。
    pub root_path: String,
    /// 系统提示词。
    pub system_prompt: Option<String>,
    /// 审批模式（`safe`/`auto_write`/`full_auto`/`plan`）。
    pub approval_mode: Option<String>,
    /// ACP 字段，COALESCE 语义：缺省 None 保持原值。`agent_type` 空串表示切回内置
    /// runner；`agent_path`/`llm_model_id` 空串视为忽略（本迭代不支持清空）；
    /// `agent_config_overrides`：缺省(None)保留、显式 null 清空、非空字符串写入。
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    /// ACP agent 可执行路径。
    pub agent_path: Option<String>,
    #[serde(default)]
    /// 关联 LLM 模型 id。
    pub llm_model_id: Option<String>,
    /// ACP 引擎选项覆盖（JSON map：config_id → value）。
    /// 三态语义：`None`（字段省略）= 保留原值；`Some(None)`（显式 JSON null）=
    /// 清空（设为 NULL）；`Some(Some(s))`（非空字符串）= 写入新值。
    /// 空串 `""` 在 handler 层归一化为清空（向后兼容旧前端传空串=清空的惯例）。
    #[serde(default)]
    pub agent_config_overrides: Option<Option<String>>,
    /// Claude Code tier 模型映射（JSON object：key ∈ {opus,sonnet,haiku,subagent}，值为
    /// 模型引用）。三态语义同 `agent_config_overrides`：省略=保持、显式 null
    ///（或空串）= 清空、非空字符串=写入。
    #[serde(default)]
    pub claude_tier_models: Option<Option<String>>,
    /// GitHub Actions 集成字段，COALESCE 语义：缺省（None）/ 空串保持原值，
    /// 非空更新。token 由 API 层加密后落库。
    #[serde(default)]
    pub github_token: Option<String>,
    #[serde(default)]
    /// GitHub 仓库 owner。
    pub github_owner: Option<String>,
    #[serde(default)]
    /// GitHub 仓库名。
    pub github_repo: Option<String>,
}

#[derive(Debug, Deserialize)]
/// 创建会话的请求体。
pub struct CreateSessionRequest {
    /// 标题。
    pub title: Option<String>,
    /// 模型 id。
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
/// 更新会话模型的请求体。
pub struct UpdateSessionModelRequest {
    /// 空串表示清除会话模型，回退到默认解析。
    pub model: String,
}

#[derive(Debug, serde::Serialize)]
/// 默认模型响应。
pub struct DefaultModelResponse {
    /// 模型 id。
    pub model: String,
}

#[derive(Debug, Deserialize)]
/// 更新会话标题的请求体。
pub struct UpdateSessionRequest {
    /// 标题。
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
    /// 消息列表。
    pub messages: Vec<AgentMessageRecord>,
    /// 是否还有更早消息。
    pub has_more: bool,
}

#[derive(Debug, Deserialize)]
/// Agent WebSocket 连接的查询参数。
pub struct AgentWsQuery {
    /// 会话 id。
    pub session_id: String,
    /// 鉴权 token（`?token=`）。
    pub token: Option<String>,
}

/// 全局通知 WS 的 query：无 session 维度，仅 JWT token（复用 `agent_ws` 的鉴权方式）。
#[derive(Debug, Deserialize)]
pub struct NotificationsWsQuery {
    /// 鉴权 token（`?token=`）。
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
/// 终端 WebSocket 的查询参数。
pub struct TerminalWsQuery {
    /// 工作区 id。
    pub workspace_id: String,
    /// 终端列数。
    pub cols: Option<u16>,
    /// 终端行数。
    pub rows: Option<u16>,
    /// 鉴权 token（`?token=`）。
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
/// 工作区文件列表的查询参数。
pub struct WorkspaceFilesQuery {
    /// 搜索关键字/前缀。
    pub q: String,
    /// 返回条数限制。
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
/// 工作区文件系统路径的查询参数。
pub struct FsPathQuery {
    /// 相对工作区根的路径；tree 默认 "."，file 必填。
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
/// 写入工作区文件的请求体。
pub struct PutFsFileRequest {
    /// 文件路径。
    pub path: String,
    /// 文件内容。
    pub content: String,
    /// 前端确认后重发携带：跳过审批检查（用户已在面板确认）。
    pub approved: Option<bool>,
}

/// `GET /api/agent/workspaces/:id/git/log` 的 query 参数。
#[derive(Debug, Default, Deserialize)]
pub struct GitLogQuery {
    #[serde(default)]
    /// 返回条数限制。
    pub limit: Option<usize>,
}

/// `GET /api/agent/workspaces/:id/git/diff` 的 query 参数（`cached=true` 取 staged diff）。
#[derive(Debug, Default, Deserialize)]
pub struct GitDiffQuery {
    /// 文件路径。
    pub path: Option<String>,
    #[serde(default)]
    /// 是否取 staged diff。
    pub cached: Option<bool>,
}

/// `GET /api/agent/workspaces/:id/git/show` 的 query 参数。
#[derive(Debug, Default, Deserialize)]
pub struct GitShowQuery {
    /// 提交/分支标识。
    pub rev: Option<String>,
}

/// 无参数写操作（pull/push）的请求体：允许空 body / `{}` / 仅 `approved`。
#[derive(Debug, Default, Deserialize)]
pub struct GitApprovedBody {
    #[serde(default)]
    /// 是否已获审批确认。
    pub approved: Option<bool>,
}

/// 写操作统一审批标记：`approved: true` 表示用户已在面板确认后重发。
/// 面板所有 Git 写端点的请求体都携带该字段。
macro_rules! git_write_body {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Debug, Deserialize)]
        #[allow(missing_docs)]
        pub struct $name {
            $(
                #[allow(missing_docs)]
                pub $field: $ty,
            )*
            #[serde(default)]
            /// 是否已获审批确认（`true` 跳过二次确认）。
            pub approved: Option<bool>,
        }
    };
}

/// GitStageRequest 请求体（Git 写操作，含 approved 审批标记）。
git_write_body!(GitStageRequest {
    paths: Vec<String>,
});
/// GitUnstageRequest 请求体（Git 写操作，含 approved 审批标记）。
git_write_body!(GitUnstageRequest {
    paths: Vec<String>,
});
/// GitCommitRequest 请求体（Git 写操作，含 approved 审批标记）。
git_write_body!(GitCommitRequest { message: String });
/// GitCheckoutRequest 请求体（Git 写操作，含 approved 审批标记）。
git_write_body!(GitCheckoutRequest {
    branch: String,
    create: Option<bool>,
});
/// GitBranchDeleteRequest 请求体（Git 写操作，含 approved 审批标记）。
git_write_body!(GitBranchDeleteRequest {
    branch: String,
    force: Option<bool>,
});
/// GitRevertRequest 请求体（Git 写操作，含 approved 审批标记）。
git_write_body!(GitRevertRequest { rev: String });
/// GitResetRequest 请求体（Git 写操作，含 approved 审批标记）。
git_write_body!(GitResetRequest {
    rev: Option<String>,
    mode: String,
});
/// GitStashPushRequest 请求体（Git 写操作，含 approved 审批标记）。
git_write_body!(GitStashPushRequest {
    message: Option<String>,
});
/// GitStashIndexRequest 请求体（Git 写操作，含 approved 审批标记）。
git_write_body!(GitStashIndexRequest { index: usize });

// ── GitHub Actions 面板 ───────────────────────────────────────

/// `GET /api/agent/workspaces/:id/github/repo` 的 query：`?refresh=true` 强制重探
/// （忽略 5 分钟内存缓存）。
#[derive(Debug, Default, Deserialize)]
pub struct GithubRepoQuery {
    #[serde(default)]
    /// 是否强制刷新（忽略缓存）。
    pub refresh: Option<bool>,
}

/// `GET /api/agent/workspaces/:id/github/runs` 的 query 参数。
#[derive(Debug, Default, Deserialize)]
pub struct GithubRunsQuery {
    #[serde(default)]
    /// 工作流 id。
    pub workflow_id: Option<String>,
    #[serde(default)]
    /// 每页条数。
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
    /// 是否已获审批确认。
    pub approved: Option<bool>,
}

// ── Agent Roles ─────────────────────────────────────────────────

/// POST /api/agent/roles 请求体。
#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    /// 名称。
    pub name: String,
    #[serde(default)]
    /// 描述。
    pub description: String,
    #[serde(default)]
    /// 系统提示词。
    pub system_prompt: String,
    #[serde(default)]
    /// 允许的工具列表。
    pub tools_allow: Option<Vec<String>>,
    #[serde(default)]
    /// 禁止的工具列表。
    pub tools_deny: Option<Vec<String>>,
    #[serde(default)]
    /// 模型覆盖。
    pub model_override: Option<String>,
    #[serde(default)]
    /// 模式。
    pub mode: String,
    #[serde(default)]
    /// 作用域类型。
    pub scope_type: String,
    #[serde(default)]
    /// 关联客户端 id。
    pub client_id: String,
    #[serde(default)]
    /// 工作区 id。
    pub workspace_id: String,
}

/// PUT /api/agent/roles/:id 请求体（部分更新）。
#[derive(Debug, Default, Deserialize)]
pub struct UpdateRoleRequest {
    #[serde(default)]
    /// 名称。
    pub name: Option<String>,
    #[serde(default)]
    /// 描述。
    pub description: Option<String>,
    #[serde(default)]
    /// 系统提示词。
    pub system_prompt: Option<String>,
    #[serde(default)]
    /// 允许的工具列表。
    pub tools_allow: Option<Vec<String>>,
    #[serde(default)]
    /// 禁止的工具列表。
    pub tools_deny: Option<Vec<String>>,
    #[serde(default)]
    /// 模型覆盖。
    pub model_override: Option<String>,
    #[serde(default)]
    /// 模式。
    pub mode: Option<String>,
    #[serde(default)]
    /// 作用域类型。
    pub scope_type: Option<String>,
    #[serde(default)]
    /// 关联客户端 id。
    pub client_id: Option<String>,
    #[serde(default)]
    /// 工作区 id。
    pub workspace_id: Option<String>,
}

/// PATCH /api/agent/sessions/:id/role 请求体。
#[derive(Debug, Deserialize)]
pub struct UpdateSessionRoleRequest {
    /// 角色 id；null/空串表示清除会话角色绑定。
    pub role_id: Option<String>,
}

/// GET /api/agent/roles 的 query 参数。
#[derive(Debug, Default, Deserialize)]
pub struct ListRolesParams {
    #[serde(default)]
    /// 作用域过滤。
    pub scope: Option<String>,
    #[serde(default)]
    /// 关联客户端 id。
    pub client_id: Option<String>,
    #[serde(default)]
    /// 工作区 id。
    pub workspace_id: Option<String>,
    #[serde(default)]
    /// 搜索关键字/前缀。
    pub q: Option<String>,
    #[serde(default)]
    /// 是否启用。
    pub enabled: Option<bool>,
    #[serde(default)]
    /// 模式。
    pub mode: Option<String>,
    #[serde(default)]
    /// 返回条数限制。
    pub limit: Option<i64>,
    #[serde(default)]
    /// 偏移量。
    pub offset: Option<i64>,
}
