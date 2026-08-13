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
