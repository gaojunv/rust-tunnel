//! Agent workbench persistence: workspaces / sessions / messages.
use super::Database;

/// 把 SQLite `datetime('now')` 的空格分隔时间（UTC、无时区标记）归一化为
/// ISO-8601 `YYYY-MM-DDTHH:MM:SSZ`（M12）。Safari 的 `new Date` 不认空格格式
/// （会话相对时间整行不渲染）；Chrome 虽宽容但按本地时间误解析，UTC 存值偏差
/// 数小时。已含 T/Z 或其它格式的字符串原样返回（其它写入方可能直接存 ISO）。
/// 原子字符串：长度 ≥19 且第 11 个字符为空格即命中；带毫秒/偏移由 JS Date 宽容
/// 解析，不在此处理。
#[must_use]
pub fn normalize_db_datetime(raw: &str) -> String {
    if raw.len() >= 19 && raw.as_bytes().get(10) == Some(&b' ') {
        let mut s = raw.to_string();
        s.replace_range(10..11, "T");
        s.push('Z');
        s
    } else {
        raw.to_string()
    }
}

/// [`normalize_db_datetime`] 的 serde `serialize_with` 适配（String 字段）。
/// `&String` 是 serde `serialize_with` 对 String 字段要求的固定签名（非可改写的
/// `&str`），故 allow `ptr_arg`。
#[allow(clippy::ptr_arg)]
pub fn ser_de_normalized_dt<S: serde::Serializer>(s: &String, ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(&normalize_db_datetime(s))
}

/// Agent 工作区记录（agent_workspaces 表的一行）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AgentWorkspaceRecord {
    /// 工作区 id。
    pub id: String,
    /// 工作区名称。
    pub name: String,
    /// 归属客户端 id。
    pub client_id: String,
    /// 运行时类型（host/docker）。
    pub runtime_type: String,
    /// 工作区根路径。
    pub root_path: String,
    /// Docker 镜像，未使用容器为 None。
    pub docker_image: Option<String>,
    /// Docker 容器 id，未使用容器为 None。
    pub docker_container_id: Option<String>,
    /// 审批模式。
    pub approval_mode: String,
    /// 系统提示词，未配置为 None。
    pub system_prompt: Option<String>,
    /// ACP 远程 agent 类型。列由 `migrate_agent_workspaces_v3` 落地，未迁移前默认为空串。
    #[sqlx(default)]
    pub agent_type: String,
    /// ACP agent 可执行文件路径，未配置为 None。
    #[sqlx(default)]
    pub agent_path: Option<String>,
    /// 关联 LLM 模型 id，未配置为 None。
    #[sqlx(default)]
    pub llm_model_id: Option<String>,
    /// ACP 引擎选项覆盖（JSON map：config_id → value），会话建立时经
    /// `set_config_option` 注入。列由 `migrate_agent_workspaces_v4` 落地。
    #[sqlx(default)]
    pub agent_config_overrides: Option<String>,
    // GitHub Actions 集成字段。列由 `migrate_agent_workspaces_v5`（schema.rs）落地；
    // `#[sqlx(default)]` 保证旧库未跑 v5 迁移前 `SELECT *` 仍可解码。
    /// GitHub token **密文**（LlmCipher 加密后落库，`enc:v1:` 前缀）。序列化时
    /// 永不输出——DTO 只暴露 [`Self::github_token_set`]（见手写 `Serialize`）。
    #[sqlx(default)]
    pub github_token: Option<String>,
    /// GitHub 仓库 owner，未配置为 None。
    #[sqlx(default)]
    pub github_owner: Option<String>,
    /// GitHub 仓库名，未配置为 None。
    #[sqlx(default)]
    pub github_repo: Option<String>,
    /// Claude Code tier 模型映射（JSON object：key ∈ {opus,sonnet,haiku}，
    /// 值为模型引用 `model:<id>`/`group:<id>`/裸别名）。列由
    /// `migrate_agent_workspaces_v6`（schema.rs）落地；`#[sqlx(default)]`
    /// 保证旧库未跑 v6 迁移前 `SELECT *` 仍可解码。
    #[sqlx(default)]
    pub claude_tier_models: Option<String>,
    /// 创建时间（SQLite datetime 字符串）。
    pub created_at: String,
    /// 更新时间（SQLite datetime 字符串）。
    pub updated_at: String,
}

impl AgentWorkspaceRecord {
    /// 是否已配置 GitHub token（密文非空即视为已配置；密文可能是"明文降级"
    /// 路径下的原样 token，非 `enc:v1:` 前缀，同样按已配置处理）。
    #[must_use]
    pub fn github_token_set(&self) -> bool {
        self.github_token.as_deref().is_some_and(|t| !t.is_empty())
    }
}

/// 手写 `Serialize`：`github_token` 永不进入 JSON（防泄露）；`github_token_set`
/// 以布尔位替代下发。其余字段与 derive 输出一致（含 created_at/updated_at 的
/// ISO-8601 归一化）。
impl serde::Serialize for AgentWorkspaceRecord {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("AgentWorkspaceRecord", 20)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("client_id", &self.client_id)?;
        s.serialize_field("runtime_type", &self.runtime_type)?;
        s.serialize_field("root_path", &self.root_path)?;
        s.serialize_field("docker_image", &self.docker_image)?;
        s.serialize_field("docker_container_id", &self.docker_container_id)?;
        s.serialize_field("approval_mode", &self.approval_mode)?;
        s.serialize_field("system_prompt", &self.system_prompt)?;
        s.serialize_field("agent_type", &self.agent_type)?;
        s.serialize_field("agent_path", &self.agent_path)?;
        s.serialize_field("llm_model_id", &self.llm_model_id)?;
        s.serialize_field("agent_config_overrides", &self.agent_config_overrides)?;
        s.serialize_field("github_owner", &self.github_owner)?;
        s.serialize_field("github_repo", &self.github_repo)?;
        s.serialize_field("github_token_set", &self.github_token_set())?;
        s.serialize_field("claude_tier_models", &self.claude_tier_models)?;
        s.serialize_field("created_at", &normalize_db_datetime(&self.created_at))?;
        s.serialize_field("updated_at", &normalize_db_datetime(&self.updated_at))?;
        s.end()
    }
}

/// Agent 会话记录（agent_sessions 表的一行）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentSessionRecord {
    /// 会话 id。
    pub id: String,
    /// 所属工作区 id。
    pub workspace_id: String,
    /// 会话标题，未命名为 None。
    pub title: Option<String>,
    /// 会话状态（active/archived 等）。
    pub status: String,
    /// 关联模型，未指定为 None。
    pub model: Option<String>,
    /// ACP 会话配置状态（JSON map：config_id → value；仅用户显式切换过的项）
    pub config_state: Option<String>,
    /// agent 侧 ACP 会话 id（`session/new` 返回，断线重拉时 `session/resume`
    /// 凭它恢复上下文）。列由 `migrate_agent_sessions_v3`（schema.rs）落地；
    /// `#[sqlx(default)]` 保证旧库未跑迁移前 `SELECT *` 仍可解码。
    #[sqlx(default)]
    #[serde(default)]
    pub acp_session_id: Option<String>,
    /// 会话蒸馏防重标记（1 = 已蒸馏）。列由 `migrate_agent_sessions_add_distilled`
    /// （schema.rs）落地；`#[sqlx(default)]` 保证旧库未跑迁移前 `SELECT *` 仍可解码。
    #[sqlx(default)]
    #[serde(default)]
    pub distilled: i32,
    /// 关联角色 id（引用 agent_roles.id，应用层校验无 FK）。列由
    /// `migrate_agent_sessions_add_role`（schema.rs）落地；`#[sqlx(default)]` 保证
    /// 旧库未跑迁移前 `SELECT *` 仍可解码。
    #[sqlx(default)]
    #[serde(default)]
    pub role_id: Option<String>,
    /// ACP UsageUpdate 最近一次上下文用量快照（tokens）。列由
    /// `migrate_agent_sessions_add_context_usage`（schema.rs）落地。
    #[sqlx(default)]
    #[serde(default)]
    pub context_used: Option<i64>,
    /// 上下文窗口大小（tokens），与 `context_used` 成对出现。
    #[sqlx(default)]
    #[serde(default)]
    pub context_size: Option<i64>,
    /// 最近一次 ACP spawn 失败的归因描述（带 stage 前缀；spawn 成功时清空）。
    /// 列由 `migrate_agent_sessions_add_spawn_error`（schema.rs）落地。
    #[sqlx(default)]
    #[serde(default)]
    pub last_spawn_error: Option<String>,
    /// 创建时间（SQLite datetime 字符串，序列化时归一化为 ISO-8601）。
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub created_at: String,
    /// 更新时间（SQLite datetime 字符串，序列化时归一化为 ISO-8601）。
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub updated_at: String,
}

/// Agent 消息记录（agent_messages 表的一行）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentMessageRecord {
    /// 消息 id。
    pub id: String,
    /// 所属会话 id。
    pub session_id: String,
    /// 消息角色（user/assistant/tool 等）。
    pub role: String,
    /// 消息正文。
    pub content: String,
    /// 工具调用 JSON，未涉及工具为 None。
    pub tool_calls: Option<String>,
    /// 关联工具调用 id，未关联为 None。
    pub tool_call_id: Option<String>,
    /// 工具/调用名称，未涉及为 None。
    pub name: Option<String>,
    /// 消息类型（message/tool_calls/tool_result/summary 等）。
    pub kind: String,
    /// 子 agent 归属：发起本消息的 Task 工具调用 id（claude-code-acp
    /// `_meta.claudeCode.parentToolUseId`）。主 agent 消息为 None。列由
    /// `migrate_agent_messages_v4`（schema.rs）落地；`#[sqlx(default)]` 保证旧库
    /// 未跑迁移前 `SELECT *` 仍可解码。
    #[sqlx(default)]
    #[serde(default)]
    pub parent_tool_call_id: Option<String>,
    /// 创建时间（SQLite datetime 字符串，序列化时归一化为 ISO-8601）。
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub created_at: String,
}

/// `agent_create_workspace` 参数包：创建 agent workspace 的全部字段（12 项）。
#[derive(Debug, Clone, Default)]
pub struct AgentWorkspaceCreateOpts {
    /// 工作区 id。
    pub id: String,
    /// 工作区名称。
    pub name: String,
    /// 归属客户端 id。
    pub client_id: String,
    /// 运行时类型（host/docker）。
    pub runtime_type: String,
    /// 工作区根路径。
    pub root_path: String,
    /// Docker 镜像，未使用容器为 None。
    pub docker_image: Option<String>,
    /// Docker 容器 id，未使用容器为 None。
    pub docker_container_id: Option<String>,
    /// ACP agent 类型。
    pub agent_type: String,
    /// ACP agent 可执行文件路径，未配置为 None。
    pub agent_path: Option<String>,
    /// 关联 LLM 模型 id，未配置为 None。
    pub llm_model_id: Option<String>,
    /// ACP 引擎选项覆盖（JSON map），未配置为 None。
    pub agent_config_overrides: Option<String>,
    /// Claude Code tier 模型映射（JSON object），未配置为 None。
    pub claude_tier_models: Option<String>,
}

/// `agent_update_workspace` 参数包：更新 agent workspace 的全部字段（11 项含 clear_overrides/clear_tier_models 布尔）。
#[derive(Debug, Clone, Default)]
pub struct AgentWorkspaceUpdateOpts {
    /// 工作区名称。
    pub name: String,
    /// 工作区根路径。
    pub root_path: String,
    /// 系统提示词，None 保持原值。
    pub system_prompt: Option<String>,
    /// 审批模式，None 保持原值。
    pub approval_mode: Option<String>,
    /// ACP agent 类型，None 保持原值。
    pub agent_type: Option<String>,
    /// ACP agent 可执行文件路径，None 保持原值。
    pub agent_path: Option<String>,
    /// 关联 LLM 模型 id，None 保持原值。
    pub llm_model_id: Option<String>,
    /// ACP 引擎选项覆盖（JSON map），None 保持原值。
    pub agent_config_overrides: Option<String>,
    /// Claude Code tier 模型映射（JSON object），None 保持原值。
    pub claude_tier_models: Option<String>,
    /// 是否清空引擎选项覆盖。
    pub clear_overrides: bool,
    /// 是否清空 tier 模型映射。
    pub clear_tier_models: bool,
}

/// `agent_add_message_v2` 参数包：agent 消息落库一行的全部列（9 项，`parent_tool_call_id` 为 ACP 子 agent 归属）。
#[derive(Debug, Clone, Default)]
pub struct AgentMessageOpts {
    /// 消息 id。
    pub id: String,
    /// 所属会话 id。
    pub session_id: String,
    /// 消息角色。
    pub role: String,
    /// 消息正文。
    pub content: String,
    /// 工具调用 JSON，未涉及工具为 None。
    pub tool_calls: Option<String>,
    /// 关联工具调用 id，未关联为 None。
    pub tool_call_id: Option<String>,
    /// 工具/调用名称，未涉及为 None。
    pub name: Option<String>,
    /// 消息类型（message/tool_calls/tool_result 等）。
    pub kind: String,
    /// 子 agent 归属（发起该消息的 Task 工具调用 id），主 agent 消息为 None。
    pub parent_tool_call_id: Option<String>,
}

impl Database {
    // ── Workspace CRUD ──────────────────────────────────────────

    /// 创建 agent workspace。`agent_type` 为 ACP 远程 agent 类型（非空列，默认空串），
    /// `agent_path`/`llm_model_id`/`agent_config_overrides` 可空（后者为 ACP 引擎
    /// 选项覆盖，JSON map：config_id → value，None 表示未配置）。调用方暂未接入请求
    /// DTO 时传 `""` / `None` 占位。
    pub async fn agent_create_workspace(
        &self,
        opts: &AgentWorkspaceCreateOpts,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            INSERT INTO agent_workspaces
                (id, name, client_id, runtime_type, root_path, docker_image, docker_container_id,
                 agent_type, agent_path, llm_model_id, agent_config_overrides, claude_tier_models)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(&opts.id)
        .bind(&opts.name)
        .bind(&opts.client_id)
        .bind(&opts.runtime_type)
        .bind(&opts.root_path)
        .bind(&opts.docker_image)
        .bind(&opts.docker_container_id)
        .bind(&opts.agent_type)
        .bind(&opts.agent_path)
        .bind(&opts.llm_model_id)
        .bind(&opts.agent_config_overrides)
        .bind(&opts.claude_tier_models)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 按 id 查询单个工作区，不存在返回 None。
    pub async fn agent_get_workspace(
        &self,
        id: &str,
    ) -> Result<Option<AgentWorkspaceRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkspaceRecord>("SELECT * FROM agent_workspaces WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// 列出全部工作区，按创建时间升序。
    pub async fn agent_list_workspaces(&self) -> Result<Vec<AgentWorkspaceRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkspaceRecord>(
            "SELECT * FROM agent_workspaces ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// 更新 agent workspace 的可变字段。ACP 字段（agent_type/agent_path/llm_model_id/
    /// agent_config_overrides）采用 COALESCE 语义：`None` 保持原值，`Some` 写入新值，
    /// 与 `approval_mode` 一致。`agent_config_overrides` 为 ACP 引擎选项覆盖（JSON
    /// map：config_id → value）；`clear_overrides=true` 时强制清空（设为 NULL），
    /// 否则按 COALESCE 语义处理。`claude_tier_models` 为 Claude Code tier 模型映射
    ///（JSON object），`clear_tier_models=true` 时强制清空（设为 NULL）。
    pub async fn agent_update_workspace(
        &self,
        id: &str,
        opts: &AgentWorkspaceUpdateOpts,
    ) -> Result<(), sqlx::Error> {
        // tier 三态经 CASE WHEN 内联表达（clear=true → NULL；否则 COALESCE 保持/写入），
        // 避免与 clear_overrides 的组合再翻倍 SQL 分支。
        if opts.clear_overrides {
            sqlx::query(
                "UPDATE agent_workspaces SET name = ?, root_path = ?, system_prompt = ?, \
                 approval_mode = COALESCE(?, approval_mode), \
                 agent_type = COALESCE(?, agent_type), \
                 agent_path = COALESCE(?, agent_path), \
                 llm_model_id = COALESCE(?, llm_model_id), \
                 agent_config_overrides = NULL, \
                 claude_tier_models = CASE WHEN ? THEN NULL \
                 ELSE COALESCE(?, claude_tier_models) END, \
                 updated_at = datetime('now') WHERE id = ?",
            )
            .bind(&opts.name)
            .bind(&opts.root_path)
            .bind(&opts.system_prompt)
            .bind(&opts.approval_mode)
            .bind(&opts.agent_type)
            .bind(&opts.agent_path)
            .bind(&opts.llm_model_id)
            .bind(opts.clear_tier_models)
            .bind(&opts.claude_tier_models)
            .bind(id)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE agent_workspaces SET name = ?, root_path = ?, system_prompt = ?, \
                 approval_mode = COALESCE(?, approval_mode), \
                 agent_type = COALESCE(?, agent_type), \
                 agent_path = COALESCE(?, agent_path), \
                 llm_model_id = COALESCE(?, llm_model_id), \
                 agent_config_overrides = COALESCE(?, agent_config_overrides), \
                 claude_tier_models = CASE WHEN ? THEN NULL \
                 ELSE COALESCE(?, claude_tier_models) END, \
                 updated_at = datetime('now') WHERE id = ?",
            )
            .bind(&opts.name)
            .bind(&opts.root_path)
            .bind(&opts.system_prompt)
            .bind(&opts.approval_mode)
            .bind(&opts.agent_type)
            .bind(&opts.agent_path)
            .bind(&opts.llm_model_id)
            .bind(&opts.agent_config_overrides)
            .bind(opts.clear_tier_models)
            .bind(&opts.claude_tier_models)
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// 单字段更新 workspace 的 `llm_model_id`。原为测试专用 helper，拆 crate 后
    /// server 侧测试跨 crate 调用，提升为正式方法（语义：直接覆盖）。
    pub async fn agent_set_workspace_llm_model_id(
        &self,
        id: &str,
        model_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_workspaces SET llm_model_id = ?, updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(model_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 写 workspace 的 GitHub Actions 集成三列（token 为 LlmCipher 加密后的密文，
    /// owner/repo 明文）。写语义「空串 / 缺省（None）= 保持已存值，非空 = 更新」，
    /// 经 `COALESCE(NULLIF(?, ''), col)` 表达：空串归一化为 NULL 后取原列值，
    /// 非空直接覆盖。调用方（API 层）负责在传参前完成 token 加密。
    pub async fn agent_set_workspace_github(
        &self,
        id: &str,
        github_token: Option<&str>,
        github_owner: Option<&str>,
        github_repo: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_workspaces \
             SET github_token = COALESCE(NULLIF(?, ''), github_token), \
                 github_owner = COALESCE(NULLIF(?, ''), github_owner), \
                 github_repo  = COALESCE(NULLIF(?, ''), github_repo), \
                 updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(github_token)
        .bind(github_owner)
        .bind(github_repo)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 删除指定工作区（级联由外键/应用层处理）。
    pub async fn agent_delete_workspace(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_workspaces WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Session CRUD ────────────────────────────────────────────

    /// 创建 agent 会话。
    pub async fn agent_create_session(
        &self,
        id: &str,
        workspace_id: &str,
        title: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_sessions (id, workspace_id, title, model) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(workspace_id)
        .bind(title)
        .bind(model)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 按 id 查询单个会话，不存在返回 None。
    pub async fn agent_get_session(
        &self,
        id: &str,
    ) -> Result<Option<AgentSessionRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentSessionRecord>("SELECT * FROM agent_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// 列出指定工作区下的全部会话，按创建时间倒序。
    pub async fn agent_list_sessions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentSessionRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentSessionRecord>(
            "SELECT * FROM agent_sessions WHERE workspace_id = ? ORDER BY created_at DESC",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
    }

    /// 更新会话标题。
    pub async fn agent_update_session_title(
        &self,
        id: &str,
        title: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET title = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(title)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 更新会话关联模型，None 表示清除。
    pub async fn agent_update_session_model(
        &self,
        id: &str,
        model: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET model = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(model)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 写入/清除 session 的角色绑定。role_id=None 清除（主会话回退默认行为）。
    pub async fn agent_update_session_role(
        &self,
        id: &str,
        role_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET role_id = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(role_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 写入/清除 session 的 ACP 会话 id（handshake 完成后落库，供断线重拉时
    /// `session/resume` 恢复上下文）。None 清空（如重拉后 session/delete 删除）。
    pub async fn agent_set_acp_session_id(
        &self,
        id: &str,
        acp_session_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET acp_session_id = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(acp_session_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 写入 session 的上下文用量快照（ACP UsageUpdate 每次推送覆盖；用于
    /// 刷新/重连后恢复前端用量条，不做历史累计）。
    pub async fn agent_update_session_context_usage(
        &self,
        id: &str,
        used: Option<i64>,
        size: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agent_sessions SET context_used = ?, context_size = ? WHERE id = ?")
            .bind(used)
            .bind(size)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 写入/清空 session 的最近 spawn 失败归因（Some=失败描述带 stage 前缀，
    /// None=spawn 成功清空）。会话行不存在时静默无影响（预 spawn 可能先于
    /// session 行创建？否——session 行由 API 先建；防御性写法不校验行数）。
    pub async fn agent_update_session_spawn_error(
        &self,
        id: &str,
        error: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agent_sessions SET last_spawn_error = ? WHERE id = ?")
            .bind(error)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── ACP 排队 prompt 持久化（agent_pending_prompts） ─────────────

    /// 入队一条等待执行的 prompt（busy 时）。返回行 id（供取出执行后删除）。
    pub async fn agent_pending_enqueue(
        &self,
        id: &str,
        session_id: &str,
        content: &str,
        refs_json: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_pending_prompts (id, session_id, content, refs) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(session_id)
        .bind(content)
        .bind(refs_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 按 FIFO（rowid 升序 = 入队顺序）列出 session 的排队 prompt。
    pub async fn agent_pending_list(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, content, refs FROM agent_pending_prompts WHERE session_id = ? ORDER BY rowid",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
    }

    /// 取出执行后删除对应行（best-effort 调用地不阻塞主流程）。
    pub async fn agent_pending_delete(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_pending_prompts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 清空 session 的全部排队 prompt（会话删除时级联清理）。
    pub async fn agent_pending_clear_session(&self, session_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_pending_prompts WHERE session_id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// upsert/删除 session 的 ACP 配置项：value=Some 写入该 key，None 删除；
    /// map 为空时列置 NULL。config_state 非 JSON（历史脏数据）时视为空 map 重建。
    pub async fn agent_update_session_config_state(
        &self,
        id: &str,
        config_id: &str,
        value: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let row = self.agent_get_session(id).await?;
        let mut map: serde_json::Map<String, serde_json::Value> = row
            .and_then(|r| r.config_state)
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        match value {
            Some(v) => {
                map.insert(
                    config_id.to_string(),
                    serde_json::Value::String(v.to_string()),
                );
            }
            None => {
                map.remove(config_id);
            }
        }
        let serialized = if map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(map).to_string())
        };
        sqlx::query(
            "UPDATE agent_sessions SET config_state = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(serialized)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 归档指定会话（status 置为 archived）。
    pub async fn agent_archive_session(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET status = 'archived', updated_at = datetime('now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 删除指定会话并级联清理排队 prompt。
    pub async fn agent_delete_session(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        // 级联清理排队 prompt（表无 FK，应用层级联；会话已删，残留行无意义）
        self.agent_pending_clear_session(id).await?;
        Ok(())
    }

    // ── Messages ────────────────────────────────────────────────

    /// 追加一条消息（旧接口，kind 由 role 推导；新代码优先用 `agent_add_message_v2`）。
    pub async fn agent_add_message(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
        tool_calls: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        // 旧接口兼容：role=tool 的合并行推导 kind="tool"（重放时按旧格式跳过），
        // 其余为普通 message。
        let kind = if role == "tool" { "tool" } else { "message" };
        self.agent_add_message_v2(&AgentMessageOpts {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: tool_calls.map(|s| s.to_string()),
            tool_call_id: None,
            name: None,
            kind: kind.to_string(),
            parent_tool_call_id: None,
        })
        .await
    }

    /// 新格式消息写入（全列）。`kind` 取值：message / tool_calls / tool_result / summary。
    /// `parent_tool_call_id`：子 agent 归属（发起本消息的 Task 工具调用 id），
    /// 主 agent 消息传 None。
    pub async fn agent_add_message_v2(&self, opts: &AgentMessageOpts) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_messages \
             (id, session_id, role, content, tool_calls, tool_call_id, name, kind, parent_tool_call_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&opts.id)
        .bind(&opts.session_id)
        .bind(&opts.role)
        .bind(&opts.content)
        .bind(&opts.tool_calls)
        .bind(&opts.tool_call_id)
        .bind(&opts.name)
        .bind(&opts.kind)
        .bind(&opts.parent_tool_call_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 回填 tool_calls 行里指定调用的 arguments（claude-code-acp 的 ToolCall 首帧
    /// rawInput 常是 {}，真正的参数经后续 ToolCallUpdate.rawInput 才到达；若不回填，
    /// 重载后历史卡片无操作内容）。只重写 `tool_calls` JSON 数组中 id 匹配项的
    /// `arguments` 字段，其余字段（name/tool_kind/diffs/locations）保持原样。
    ///
    /// `session_id` 必须传入：部分 agent 用顺序 id（如 `call_1`），不同会话会出现
    /// 相同 tool_call_id，缺 session 约束会把会话 A 的 args 回填到会话 B 的
    /// tool_calls 行（跨会话历史卡片参数错乱），查询也退化为全表扫描。
    pub async fn agent_update_tool_call_args(
        &self,
        session_id: &str,
        tool_call_id: &str,
        args: &str,
    ) -> Result<(), sqlx::Error> {
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT rowid, tool_calls FROM agent_messages \
             WHERE kind = 'tool_calls' AND session_id = ? AND tool_call_id = ?",
        )
        .bind(session_id)
        .bind(tool_call_id)
        .fetch_all(&self.pool)
        .await?;
        for (rowid, json) in rows {
            let Ok(mut calls) = serde_json::from_str::<Vec<serde_json::Value>>(&json) else {
                continue; // 畸形 JSON 跳过（best-effort，不影响实时路径）
            };
            let mut touched = false;
            for c in &mut calls {
                if c.get("id").and_then(|v| v.as_str()) == Some(tool_call_id) {
                    c["arguments"] = serde_json::Value::String(args.to_string());
                    touched = true;
                }
            }
            if touched {
                sqlx::query("UPDATE agent_messages SET tool_calls = ? WHERE rowid = ?")
                    .bind(serde_json::to_string(&calls).unwrap_or_default())
                    .bind(rowid)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// ACP 工具行 upsert：按 (session_id, tool_call_id, kind) 定位 agent_messages
    /// 行。历史版本对每个 ACP 事件纯 INSERT，同一 tool_call_id 会产生多行
    /// （tool_call 每次事件、tool_result 每个中间态各一行），前端刷新后重复卡片。
    /// 此函数把同组多行收敛为一行（保留 rowid 最大者，删除其余）后做 UPDATE/INSERT。
    ///
    /// 不能加唯一索引收敛——compact.rs 压缩时会带相同 (session_id, tool_call_id,
    /// kind) 重插 kept 段，唯一索引会直接冲突报错。
    ///
    /// tool_calls 覆盖规则：新 JSON 长度 >= 旧值时覆盖（新帧通常带更完整的
    /// rawInput/diffs），否则保持旧值（如回放带来的短占位不覆盖已回填的真实参数）。
    /// `parent_tool_call_id`：子 agent 归属（发起该调用的 Task 工具调用 id），
    /// 主 agent 调用传 None；更新路径以 COALESCE 补全（同一 tool_call_id 的归属
    /// 固定，先到者生效）。
    pub async fn agent_upsert_tool_call(
        &self,
        id: &str,
        session_id: &str,
        tool_call_id: &str,
        name: Option<&str>,
        tool_calls_json: &str,
        parent_tool_call_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let rows: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
            "SELECT rowid, tool_calls FROM agent_messages \
             WHERE session_id = ? AND tool_call_id = ? AND kind = 'tool_calls' \
             ORDER BY rowid",
        )
        .bind(session_id)
        .bind(tool_call_id)
        .fetch_all(&self.pool)
        .await?;
        match rows.split_last() {
            Some((max, rest)) => {
                // 收敛：删除非 rowid 最大的其余行
                for (rid, _) in rest {
                    sqlx::query("DELETE FROM agent_messages WHERE rowid = ?")
                        .bind(rid)
                        .execute(&self.pool)
                        .await?;
                }
                let (rowid, old_json) = max;
                if tool_calls_json.len() >= old_json.len() {
                    sqlx::query(
                        "UPDATE agent_messages SET tool_calls = ?, name = COALESCE(?, name), \
                         parent_tool_call_id = COALESCE(?, parent_tool_call_id) \
                         WHERE rowid = ?",
                    )
                    .bind(tool_calls_json)
                    .bind(name)
                    .bind(parent_tool_call_id)
                    .bind(rowid)
                    .execute(&self.pool)
                    .await?;
                } else if name.is_some() {
                    // 旧 JSON 更完整：仅补名（不覆盖已回填的完整 tool_calls）
                    sqlx::query("UPDATE agent_messages SET name = ? WHERE rowid = ?")
                        .bind(name)
                        .bind(rowid)
                        .execute(&self.pool)
                        .await?;
                }
                Ok(())
            }
            None => {
                self.agent_add_message_v2(&AgentMessageOpts {
                    id: id.to_string(),
                    session_id: session_id.to_string(),
                    role: "assistant".to_string(),
                    content: String::new(),
                    tool_calls: Some(tool_calls_json.to_string()),
                    tool_call_id: Some(tool_call_id.to_string()),
                    name: name.map(|s| s.to_string()),
                    kind: "tool_calls".to_string(),
                    parent_tool_call_id: parent_tool_call_id.map(|s| s.to_string()),
                })
                .await
            }
        }
    }

    /// tool_result upsert：同 [`Self::agent_upsert_tool_call`] 的收敛规则。content
    /// 覆盖规则：新 content 非空时覆盖（终态覆盖中间态空占位）；新 content 为空且
    /// 已有非空 content 则不动（空占位不抹掉真实结果）。`parent_tool_call_id`：
    /// 子 agent 归属（同 tool_call），主 agent 传 None；更新路径 COALESCE 补全。
    ///
    /// content 格式（2026-08 M2 起，ACP 路径）：结构化 JSON 字符串
    /// `{"text": string, "status": string, "diffs"?: [...], "locations"?: [...]}`
    /// （空字段省略），status 落库供前端区分失败/成功（修复失败工具刷新后恒显 ✓）。
    /// 存量旧行是纯文本，读取方须向后兼容。中间态空占位（无产出、非异常终态）
    /// 传 ""，不覆盖已落库真实结果；failed 等异常终态即使 text 为空也须传非空
    /// JSON。装配见 [`crate::tool_result::tool_result_persist_content`]。
    pub async fn agent_upsert_tool_result(
        &self,
        id: &str,
        session_id: &str,
        tool_call_id: &str,
        name: Option<&str>,
        content: &str,
        parent_tool_call_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let rows: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
            "SELECT rowid, content FROM agent_messages \
             WHERE session_id = ? AND tool_call_id = ? AND kind = 'tool_result' \
             ORDER BY rowid",
        )
        .bind(session_id)
        .bind(tool_call_id)
        .fetch_all(&self.pool)
        .await?;
        match rows.split_last() {
            Some((max, rest)) => {
                for (rid, _) in rest {
                    sqlx::query("DELETE FROM agent_messages WHERE rowid = ?")
                        .bind(rid)
                        .execute(&self.pool)
                        .await?;
                }
                let (rowid, old_content) = max;
                if !content.is_empty() || old_content.is_empty() {
                    sqlx::query(
                        "UPDATE agent_messages SET content = ?, name = COALESCE(?, name), \
                         parent_tool_call_id = COALESCE(?, parent_tool_call_id) \
                         WHERE rowid = ?",
                    )
                    .bind(content)
                    .bind(name)
                    .bind(parent_tool_call_id)
                    .bind(rowid)
                    .execute(&self.pool)
                    .await?;
                } else if name.is_some() {
                    sqlx::query("UPDATE agent_messages SET name = ? WHERE rowid = ?")
                        .bind(name)
                        .bind(rowid)
                        .execute(&self.pool)
                        .await?;
                }
                Ok(())
            }
            None => {
                self.agent_add_message_v2(&AgentMessageOpts {
                    id: id.to_string(),
                    session_id: session_id.to_string(),
                    role: "assistant".to_string(),
                    content: content.to_string(),
                    tool_calls: None,
                    tool_call_id: Some(tool_call_id.to_string()),
                    name: name.map(|s| s.to_string()),
                    kind: "tool_result".to_string(),
                    parent_tool_call_id: parent_tool_call_id.map(|s| s.to_string()),
                })
                .await
            }
        }
    }

    /// 列出指定会话的全部消息，按插入顺序（rowid）升序。
    pub async fn agent_list_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentMessageRecord>, sqlx::Error> {
        // 按 rowid（插入顺序）而非 created_at 排序：created_at 是秒级精度
        // （datetime('now')），同一秒内多条消息（工具帧密集到达时很常见）之间的
        // 相对顺序由 rowid 兜底，但 created_at 不同秒但插入乱序的场景（ACP 落库
        // 走并发任务，wall-clock 与插入顺序可能错开）下按 created_at 排会把后插入
        // 的行提前。rowid 自增且与插入顺序严格一致，是唯一的正确排序键。
        sqlx::query_as::<_, AgentMessageRecord>(
            "SELECT * FROM agent_messages WHERE session_id = ? ORDER BY rowid",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
    }

    /// 分页读取会话消息（游标翻页）。排序语义与 [`Self::agent_list_messages`] 一致
    /// （按 rowid/插入顺序）：无 `before_id` 时取最近 `limit` 条；带 `before_id`
    /// 时取该消息**之前**（rowid 更小）的最近 `limit` 条，返回均按升序。
    ///
    /// 返回 `(messages, has_more)`：`has_more` 表示游标之前（或最旧一条之前）
    /// 是否还有更早的消息。`limit` 由调用方 clamp（默认 200、上限 500）。
    ///
    /// 游标语义：`before_id` 指向的消息本身**不**包含在返回里；指向不存在的 id
    /// 或不属于本会话的 id 时返回空页且 `has_more = false`。has_more 用「多取一条
    /// （limit+1）」判断：拿到 limit+1 条说明还有更早的没取完。
    pub async fn agent_list_messages_page(
        &self,
        session_id: &str,
        before_id: Option<&str>,
        limit: i64,
    ) -> Result<(Vec<AgentMessageRecord>, bool), sqlx::Error> {
        // 游标解析：before_id → 该消息的 rowid（rowid < before 即「更早」）。
        let before_rowid = match before_id {
            Some(id) => {
                let rowid: Option<i64> = sqlx::query_scalar(
                    "SELECT rowid FROM agent_messages WHERE session_id = ? AND id = ?",
                )
                .bind(session_id)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
                match rowid {
                    Some(r) => Some(r),
                    None => return Ok((Vec::new(), false)),
                }
            }
            None => None,
        };

        let fetch = limit + 1; // 多取一条判断 has_more
        let rows: Vec<AgentMessageRecord> =
            match before_rowid {
                Some(r) => {
                    sqlx::query_as::<_, AgentMessageRecord>(
                        "SELECT * FROM agent_messages WHERE session_id = ? AND rowid < ? \
                     ORDER BY rowid DESC LIMIT ?",
                    )
                    .bind(session_id)
                    .bind(r)
                    .bind(fetch)
                    .fetch_all(&self.pool)
                    .await?
                }
                None => sqlx::query_as::<_, AgentMessageRecord>(
                    "SELECT * FROM agent_messages WHERE session_id = ? ORDER BY rowid DESC LIMIT ?",
                )
                .bind(session_id)
                .bind(fetch)
                .fetch_all(&self.pool)
                .await?,
            };
        let has_more = rows.len() as i64 > limit;
        let mut msgs = if has_more {
            rows.into_iter().take(limit as usize).collect::<Vec<_>>()
        } else {
            rows
        };
        msgs.reverse(); // 倒序取回 → 升序返回
        Ok((msgs, has_more))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_db_datetime() {
        // SQLite datetime('now') 空格格式 → ISO-8601（T 分隔 + Z，UTC 标记）
        assert_eq!(
            normalize_db_datetime("2026-08-14 13:00:00"),
            "2026-08-14T13:00:00Z"
        );
        // 已含 T 的 ISO 字符串原样返回（其它写入方直接存 ISO）
        assert_eq!(
            normalize_db_datetime("2026-08-14T13:00:00Z"),
            "2026-08-14T13:00:00Z"
        );
        // 空串 / 非 19 位格式原样返回（防御：不破坏异常数据）
        assert_eq!(normalize_db_datetime(""), "");
        assert_eq!(normalize_db_datetime("2026-08-14"), "2026-08-14");
    }

    #[tokio::test]
    async fn test_workspace_crud() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "my-proj".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/home/user/proj".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w2".to_owned(),
            name: "dproj".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "docker".to_owned(),
            root_path: "/container/work".to_owned(),
            docker_image: Some("node:20".to_owned()),
            docker_container_id: Some("dev-ctr".to_owned()),
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();

        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.name, "my-proj");
        assert_eq!(ws.client_id, "nas");
        assert_eq!(ws.runtime_type, "host");
        assert_eq!(ws.root_path, "/home/user/proj");
        assert!(ws.docker_image.is_none());
        assert!(ws.docker_container_id.is_none());

        // docker 运行时：image 与 container_id 均持久化
        let ws = db.agent_get_workspace("w2").await.unwrap().unwrap();
        assert_eq!(ws.runtime_type, "docker");
        assert_eq!(ws.docker_image.as_deref(), Some("node:20"));
        assert_eq!(ws.docker_container_id.as_deref(), Some("dev-ctr"));

        assert_eq!(db.agent_list_workspaces().await.unwrap().len(), 2);

        db.agent_update_workspace(
            "w1",
            &AgentWorkspaceUpdateOpts {
                name: "renamed".to_owned(),
                root_path: "/new/path".to_owned(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: None,
                claude_tier_models: None,
                clear_overrides: false,
                clear_tier_models: false,
            },
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.name, "renamed");
        assert_eq!(ws.root_path, "/new/path");

        db.agent_delete_workspace("w1").await.unwrap();
        assert!(db.agent_get_workspace("w1").await.unwrap().is_none());

        // approval_mode / system_prompt 默认值与读写
        let ws = db.agent_get_workspace("w2").await.unwrap().unwrap();
        assert_eq!(ws.approval_mode, "safe");
        assert!(ws.system_prompt.is_none());

        db.agent_delete_workspace("w2").await.unwrap();
        assert!(db.agent_get_workspace("w2").await.unwrap().is_none());
    }

    /// ACP 三个新字段（agent_type/agent_path/llm_model_id）的创建→读取→更新→读取
    /// 完整往返。COALESCE 更新语义：`None` 保持原值。
    #[tokio::test]
    async fn test_workspace_acp_fields_roundtrip() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "acp-proj".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/workspace".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "gemini".to_owned(),
            agent_path: Some("/opt/acp-agent".to_owned()),
            llm_model_id: Some("m1".to_owned()),
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();

        // 创建后读取：三个字段全部落库
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "gemini");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-agent"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m1"));

        // 更新为另一组值（含改变 agent_type）
        db.agent_update_workspace(
            "w1",
            &AgentWorkspaceUpdateOpts {
                name: "acp-proj".to_owned(),
                root_path: "/workspace".to_owned(),
                system_prompt: None,
                approval_mode: None,
                agent_type: Some("claude".to_owned()),
                agent_path: Some("/opt/acp-claude".to_owned()),
                llm_model_id: Some("m2".to_owned()),
                agent_config_overrides: None,
                claude_tier_models: None,
                clear_overrides: false,
                clear_tier_models: false,
            },
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "claude");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-claude"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m2"));

        // COALESCE：None 保持原值
        db.agent_update_workspace(
            "w1",
            &AgentWorkspaceUpdateOpts {
                name: "acp-proj".to_owned(),
                root_path: "/workspace".to_owned(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: None,
                claude_tier_models: None,
                clear_overrides: false,
                clear_tier_models: false,
            },
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "claude");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-claude"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m2"));

        // 可空字段显式清空为 None
        db.agent_update_workspace(
            "w1",
            &AgentWorkspaceUpdateOpts {
                name: "acp-proj".to_owned(),
                root_path: "/workspace".to_owned(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: None,
                llm_model_id: Some("m3".to_owned()),
                agent_config_overrides: None,
                claude_tier_models: None,
                clear_overrides: false,
                clear_tier_models: false,
            },
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "claude");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-claude"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m3"));
    }

    #[tokio::test]
    async fn test_session_crud_and_archive() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", Some("fix bug"), Some("gpt-4o"))
            .await
            .unwrap();
        db.agent_create_session("s2", "w1", None, None)
            .await
            .unwrap();

        let sessions = db.agent_list_sessions("w1").await.unwrap();
        assert_eq!(sessions.len(), 2);

        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.title.as_deref(), Some("fix bug"));
        assert_eq!(s.status, "active");
        assert_eq!(s.model.as_deref(), Some("gpt-4o"));

        db.agent_update_session_title("s1", "renamed session")
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.title.as_deref(), Some("renamed session"));

        db.agent_archive_session("s1").await.unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.status, "archived");

        db.agent_delete_session("s2").await.unwrap();
        assert_eq!(db.agent_list_sessions("w1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_update_session_model() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();

        // 更新为新模型
        db.agent_update_session_model("s1", Some("claude-opus-5"))
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-opus-5"));

        // 空（None）清除，回退默认
        db.agent_update_session_model("s1", None).await.unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert!(s.model.is_none());
    }

    #[tokio::test]
    async fn test_message_append_and_list() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        db.agent_add_message("m1", "s1", "user", "帮我修 bug", None)
            .await
            .unwrap();
        db.agent_add_message(
            "m2",
            "s1",
            "assistant",
            "好的，我先看下代码",
            Some(r#"[{"name":"shell","args":{"cmd":"ls"},"result":"a.rs"}]"#),
        )
        .await
        .unwrap();
        db.agent_add_message("m3", "s1", "user", "继续", None)
            .await
            .unwrap();

        let msgs = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        assert!(msgs[1].tool_calls.as_deref().unwrap().contains("shell"));
        assert_eq!(msgs[2].content, "继续");

        // 删除会话级联删除消息
        db.agent_delete_session("s1").await.unwrap();
        assert!(db.agent_list_messages("s1").await.unwrap().is_empty());
    }

    /// 分页测试种子：插入 n 条 user 消息，id 从 `start` 起连续编号 m{start}..m{start+n-1}
    /// （rowid 升序、id 数值后缀与序号一致）。
    async fn seed_messages_from(db: &Database, session_id: &str, start: i64, n: i64) {
        for i in start..start + n {
            db.agent_add_message(
                &format!("m{i}"),
                session_id,
                "user",
                &format!("msg {i}"),
                None,
            )
            .await
            .unwrap();
        }
    }

    /// 解析 id 的数值后缀（m{n} → n），供按 rowid 顺序断言使用。
    fn msg_seq(id: &str) -> i64 {
        id.trim_start_matches('m').parse::<i64>().unwrap()
    }

    /// 无参取最近 N 条（升序、has_more）+ 带 before 翻页取更早一页。
    #[tokio::test]
    async fn test_message_page_last_n_and_before() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        seed_messages_from(&db, "s1", 0, 250).await;

        // 无 before：最近 200 条（m50..m249），升序，has_more=true
        let (msgs, has_more) = db.agent_list_messages_page("s1", None, 200).await.unwrap();
        assert!(has_more, "250 > 200 应还有更早消息");
        assert_eq!(msgs.len(), 200);
        assert_eq!(msgs.first().unwrap().id, "m50");
        assert_eq!(msgs.last().unwrap().id, "m249");
        assert!(
            msgs.windows(2)
                .all(|w| msg_seq(&w[0].id) < msg_seq(&w[1].id)),
            "返回必须按 rowid（插入顺序）升序"
        );

        // 带 before 翻页：m50 之前还有 50 条 → 全量返回、has_more=false
        let (msgs, has_more) = db
            .agent_list_messages_page("s1", Some("m50"), 200)
            .await
            .unwrap();
        assert!(!has_more);
        assert_eq!(msgs.len(), 50);
        assert_eq!(msgs.first().unwrap().id, "m0");
        assert_eq!(msgs.last().unwrap().id, "m49");

        // 翻页中间一页：before=m150 → 取 m50..m149（正好 100 条），has_more=true
        let (msgs, has_more) = db
            .agent_list_messages_page("s1", Some("m150"), 100)
            .await
            .unwrap();
        assert!(has_more, "m150 之前还有 150 条");
        assert_eq!(msgs.len(), 100);
        assert_eq!(msgs.first().unwrap().id, "m50");
        assert_eq!(msgs.last().unwrap().id, "m149");
    }

    /// has_more 边界：恰好等于 limit / 少于 limit / limit 超过总数。
    #[tokio::test]
    async fn test_message_page_has_more_boundary() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 少于 limit：50 条、limit=200 → 全量、has_more=false
        seed_messages_from(&db, "s1", 0, 50).await;
        let (msgs, has_more) = db.agent_list_messages_page("s1", None, 200).await.unwrap();
        assert!(!has_more);
        assert_eq!(msgs.len(), 50);
        assert_eq!(msgs.first().unwrap().id, "m0");

        // 恰好等于 limit：再补到 200 条、limit=200 → has_more=false
        seed_messages_from(&db, "s1", 50, 150).await; // 现在共 200 条（m0..m199）
        let (msgs, has_more) = db.agent_list_messages_page("s1", None, 200).await.unwrap();
        assert!(!has_more, "恰好等于 limit 不应有更多");
        assert_eq!(msgs.len(), 200);

        // limit+1 条：再补 1 条 → has_more=true，且只返回最近 limit 条
        seed_messages_from(&db, "s1", 200, 1).await; // 现在共 201 条
        let (msgs, has_more) = db.agent_list_messages_page("s1", None, 200).await.unwrap();
        assert!(has_more);
        assert_eq!(msgs.len(), 200);
        assert_eq!(msgs.first().unwrap().id, "m1", "最旧一条 m0 被排除");

        // limit 超过总数：300 条、limit=500 → 全量、has_more=false
        seed_messages_from(&db, "s1", 201, 99).await; // 现在共 300 条
        let (msgs, has_more) = db.agent_list_messages_page("s1", None, 500).await.unwrap();
        assert!(!has_more);
        assert_eq!(msgs.len(), 300);
    }

    /// 空会话与游标指向不存在的 id：都返回空页且 has_more=false。
    #[tokio::test]
    async fn test_message_page_empty_and_missing_cursor() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 空会话
        let (msgs, has_more) = db.agent_list_messages_page("s1", None, 200).await.unwrap();
        assert!(msgs.is_empty());
        assert!(!has_more);

        // before 指向不存在的 id
        seed_messages_from(&db, "s1", 0, 10).await;
        let (msgs, has_more) = db
            .agent_list_messages_page("s1", Some("no-such-id"), 200)
            .await
            .unwrap();
        assert!(msgs.is_empty());
        assert!(!has_more);

        // before 指向属于其他会话的 id：视为游标不存在 → 空页、has_more=false
        db.agent_create_session("s2", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("other-x", "s2", "user", "other", None)
            .await
            .unwrap();
        let (msgs, has_more) = db
            .agent_list_messages_page("s1", Some("other-x"), 200)
            .await
            .unwrap();
        assert!(msgs.is_empty(), "s2 的 other-x 不能作为 s1 的游标");
        assert!(!has_more);

        // 正常游标：s1 的 m5 → 返回 m0..m4（m5 本身不含）
        let (msgs, has_more) = db
            .agent_list_messages_page("s1", Some("m5"), 200)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 5, "m5 之前的 5 条（m0..m4）");
        assert_eq!(msgs.first().unwrap().id, "m0");
        assert_eq!(msgs.last().unwrap().id, "m4");
        assert!(!has_more);
    }

    /// 插入顺序 ≠ created_at 顺序时，列表必须按插入顺序（rowid）返回。
    ///
    /// ACP 路径的落库走并发任务（tool_call/tool_result 直接落，文本/thought 缓冲
    /// 到回合终态才落），wall-clock（created_at，秒级精度）与插入顺序可能错开：
    /// 快速连续的工具帧在同一秒内多条插入时 created_at 相同靠 rowid 兜底没问题，
    /// 但「回合中段的 tool_result 在 N 秒落库、回合末的文本合并在 N+1 秒落库」
    /// 这种正常时序下按 created_at 排序本就对——真正出错的是旧排序键
    /// `ORDER BY created_at, rowid` 在「晚到的帧带着晚 created_at 却应该先显示」
    /// 时仍按 created_at 优先排，会把后插入的行甩到列表尾部、语义上提前。
    ///
    /// 这个测试直接用 SQL 显式篡改 created_at 制造「rowid 升序但 created_at 降序」
    /// 的数据（与 ACP 并发落库的真实效果一致），断言按 rowid 而非 created_at 返回。
    #[tokio::test]
    async fn test_message_list_orders_by_rowid_not_created_at() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        db.agent_add_message("m1", "s1", "user", "第一", None)
            .await
            .unwrap();
        db.agent_add_message("m2", "s1", "assistant", "第二", None)
            .await
            .unwrap();
        db.agent_add_message("m3", "s1", "user", "第三", None)
            .await
            .unwrap();

        // 把 m2 的 created_at 改成未来，m3 保持现在：若按 created_at 排序，
        // m2（未来）会排到 m3（现在）之后；按 rowid 排序则 m2 仍在 m3 之前。
        sqlx::query("UPDATE agent_messages SET created_at = '2999-01-01 00:00:00' WHERE id = 'm2'")
            .execute(&db.pool)
            .await
            .unwrap();

        let msgs = db.agent_list_messages("s1").await.unwrap();
        let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["m1", "m2", "m3"],
            "list must follow insertion order (rowid), not created_at"
        );
    }

    /// 回填 tool_calls 行 arguments（claude-code-acp rawInput 晚到场景）：只重写
    /// id 匹配项的 arguments，其余字段与无关调用不受影响。
    #[tokio::test]
    async fn test_update_tool_call_args() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let calls = serde_json::json!([
            {"id": "c1", "name": "Terminal", "arguments": "{}", "tool_kind": "execute"},
            {"id": "c2", "name": "Read", "arguments": "{\"path\":\"a.rs\"}", "tool_kind": "read"},
        ]);
        db.agent_add_message_v2(&AgentMessageOpts {
            id: "m1".to_owned(),
            session_id: "s1".to_owned(),
            role: "assistant".to_owned(),
            content: "".to_owned(),
            tool_calls: Some(calls.to_string()),
            tool_call_id: Some("c1".to_owned()),
            name: Some("Terminal".to_owned()),
            kind: "tool_calls".to_owned(),
            parent_tool_call_id: None,
        })
        .await
        .unwrap();

        db.agent_update_tool_call_args("s1", "c1", "{\"command\":\"echo hi\"}")
            .await
            .unwrap();

        let msgs = db.agent_list_messages("s1").await.unwrap();
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(msgs[0].tool_calls.as_deref().unwrap()).unwrap();
        // c1 的 arguments 已回填为真实命令
        assert_eq!(parsed[0]["arguments"], "{\"command\":\"echo hi\"}");
        // c1 其余字段不受影响
        assert_eq!(parsed[0]["name"], "Terminal");
        assert_eq!(parsed[0]["tool_kind"], "execute");
        // 无关调用 c2 不受影响
        assert_eq!(parsed[1]["arguments"], "{\"path\":\"a.rs\"}");

        // 不存在的 tool_call_id：无错误、无变更
        db.agent_update_tool_call_args("s1", "nope", "x")
            .await
            .unwrap();
        let msgs = db.agent_list_messages("s1").await.unwrap();
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(msgs[0].tool_calls.as_deref().unwrap()).unwrap();
        assert_eq!(parsed[0]["arguments"], "{\"command\":\"echo hi\"}");
    }

    /// 跨会话隔离：另一会话有相同 tool_call_id（顺序 id 如 call_1）时，
    /// 回填只更新目标会话的行，不污染其它会话的历史卡片。
    #[tokio::test]
    async fn test_update_tool_call_args_scoped_to_session() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_create_session("s2", "w1", None, None)
            .await
            .unwrap();
        let calls = serde_json::json!([
            {"id": "call_1", "name": "Terminal", "arguments": "{}", "tool_kind": "execute"},
        ]);
        for (sid, mid) in [("s1", "m1"), ("s2", "m2")] {
            db.agent_add_message_v2(&AgentMessageOpts {
                id: mid.to_owned(),
                session_id: sid.to_owned(),
                role: "assistant".to_owned(),
                content: "".to_owned(),
                tool_calls: Some(calls.to_string()),
                tool_call_id: Some("call_1".to_owned()),
                name: Some("Terminal".to_owned()),
                kind: "tool_calls".to_owned(),
                parent_tool_call_id: None,
            })
            .await
            .unwrap();
        }

        // 只回填 s1：s2 的 call_1 必须保持原样（旧实现缺 session 约束会把两行都改掉）
        db.agent_update_tool_call_args("s1", "call_1", "{\"command\":\"ls\"}")
            .await
            .unwrap();

        let s1_msgs = db.agent_list_messages("s1").await.unwrap();
        let s1_parsed: Vec<serde_json::Value> =
            serde_json::from_str(s1_msgs[0].tool_calls.as_deref().unwrap()).unwrap();
        assert_eq!(s1_parsed[0]["arguments"], "{\"command\":\"ls\"}");

        let s2_msgs = db.agent_list_messages("s2").await.unwrap();
        let s2_parsed: Vec<serde_json::Value> =
            serde_json::from_str(s2_msgs[0].tool_calls.as_deref().unwrap()).unwrap();
        assert_eq!(
            s2_parsed[0]["arguments"], "{}",
            "other session's tool_calls must not be backfilled"
        );
    }

    /// tool_result upsert 去重：先写中间态空 content，再写终态非空 content →
    /// 只剩 1 行且 content 为终态值。
    #[tokio::test]
    async fn test_upsert_tool_result_dedup_content() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 中间态：空 content（ToolCallUpdate 首帧常无 raw_output）
        db.agent_upsert_tool_result("m1", "s1", "c1", Some("shell"), "", None)
            .await
            .unwrap();
        // 终态：非空 content
        db.agent_upsert_tool_result("m2", "s1", "c1", Some("shell"), "a.rs", None)
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(rows.len(), 1, "upsert should converge to one row: {rows:?}");
        assert_eq!(rows[0].kind, "tool_result");
        assert_eq!(rows[0].content, "a.rs");
        assert_eq!(rows[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(rows[0].name.as_deref(), Some("shell"));
    }

    /// 反向顺序：先非空后空 → 空 content 不得覆盖已有非空结果。
    #[tokio::test]
    async fn test_upsert_tool_result_empty_does_not_clear() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        db.agent_upsert_tool_result("m1", "s1", "c1", Some("shell"), "result", None)
            .await
            .unwrap();
        // 迟到的空占位帧：不覆盖
        db.agent_upsert_tool_result("m2", "s1", "c1", Some("shell"), "", None)
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "result");
    }

    /// M2 结构化 content JSON 落库：agent_upsert_tool_result 原样存储 JSON 字符串
    /// （text/status/diffs/locations），重放读取不丢字段。
    #[tokio::test]
    async fn test_upsert_tool_result_stores_structured_json() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let content = crate::tool_result::tool_result_persist_content(
            Some("a.rs"),
            Some("failed"),
            Some(&serde_json::json!([{"old": "x", "new": "y"}])),
            Some(&serde_json::json!([{"path": "a.rs", "line": 3}])),
        );
        assert!(!content.is_empty());
        db.agent_upsert_tool_result("m1", "s1", "c1", Some("shell"), &content, None)
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(rows.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&rows[0].content).unwrap();
        assert_eq!(v["text"], "a.rs");
        assert_eq!(v["status"], "failed");
        assert_eq!(v["diffs"][0]["old"], "x");
        assert_eq!(v["locations"][0]["line"], 3);
        // 读取方解析：提取 text 应成功（新格式）
        assert_eq!(
            crate::tool_result::tool_result_text(&rows[0].content),
            Some("a.rs".to_string())
        );
    }

    /// 旧纯文本 tool_result 行兼容：解析 helper 返回 None（走旧路径），且
    /// agent_upsert_tool_result 对其原样收敛（非空纯文本覆盖中间态空占位）。
    #[tokio::test]
    async fn test_upsert_tool_result_legacy_plain_text_compat() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 中间态空占位 → 终态旧纯文本（runner 路径/历史数据形态）
        db.agent_upsert_tool_result("m1", "s1", "c1", Some("shell"), "", None)
            .await
            .unwrap();
        db.agent_upsert_tool_result("m2", "s1", "c1", Some("shell"), "plain result", None)
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "plain result");
        assert_eq!(
            crate::tool_result::tool_result_text(&rows[0].content),
            None,
            "旧纯文本行必须走旧路径"
        );
    }

    /// tool_calls upsert：保更长/更完整的 JSON（回放短占位不覆盖已回填参数）。
    #[tokio::test]
    async fn test_upsert_tool_call_keeps_longer_json() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 短 JSON（首帧 rawInput={} 占位）
        let short = r#"[{"id":"c1","name":"shell","arguments":"{}"}]"#;
        db.agent_upsert_tool_call("m1", "s1", "c1", Some("shell"), short, None)
            .await
            .unwrap();
        // 长 JSON（参数/字段更完整）
        let long =
            r#"[{"id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}","tool_kind":"execute"}]"#;
        db.agent_upsert_tool_call("m2", "s1", "c1", Some("shell"), long, None)
            .await
            .unwrap();
        // 更短的 JSON 再写：不得回退已保存的完整 JSON
        db.agent_upsert_tool_call("m3", "s1", "c1", Some("shell"), r#"[{"id":"c1"}]"#, None)
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(rows.len(), 1, "upsert should converge to one row: {rows:?}");
        assert_eq!(rows[0].kind, "tool_calls");
        let json = rows[0].tool_calls.as_deref().unwrap();
        assert!(
            json.contains("tool_kind") && json.contains("ls"),
            "longer json should be kept: {json}"
        );
        assert_eq!(rows[0].name.as_deref(), Some("shell"));
    }

    #[tokio::test]
    async fn test_delete_workspace_cascades() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "hi", None)
            .await
            .unwrap();

        db.agent_delete_workspace("w1").await.unwrap();
        assert!(db.agent_list_sessions("w1").await.unwrap().is_empty());
        assert!(db.agent_list_messages("s1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_message_v2_columns_roundtrip() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // assistant tool_calls 行
        db.agent_add_message_v2(&AgentMessageOpts {
            id: "m1".to_owned(),
            session_id: "s1".to_owned(),
            role: "assistant".to_owned(),
            content: "".to_owned(),
            tool_calls: Some(
                r#"[{"id":"c1","type":"function","function":{"name":"shell","arguments":"{}"}}]"#
                    .to_owned(),
            ),
            tool_call_id: None,
            name: None,
            kind: "tool_calls".to_owned(),
            parent_tool_call_id: None,
        })
        .await
        .unwrap();
        // tool 结果行
        db.agent_add_message_v2(&AgentMessageOpts {
            id: "m2".to_owned(),
            session_id: "s1".to_owned(),
            role: "tool".to_owned(),
            content: "exit_code=0".to_owned(),
            tool_calls: None,
            tool_call_id: Some("c1".to_owned()),
            name: Some("shell".to_owned()),
            kind: "tool_result".to_owned(),
            parent_tool_call_id: None,
        })
        .await
        .unwrap();
        // 旧接口写入 → kind 自动推导
        db.agent_add_message("m3", "s1", "user", "hi", None)
            .await
            .unwrap();
        db.agent_add_message("m4", "s1", "tool", "", Some(r#"[{"name":"shell"}]"#))
            .await
            .unwrap();

        let msgs = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(msgs[0].kind, "tool_calls");
        assert!(msgs[0].tool_call_id.is_none());
        assert_eq!(msgs[1].kind, "tool_result");
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(msgs[1].name.as_deref(), Some("shell"));
        assert_eq!(msgs[2].kind, "message");
        assert_eq!(msgs[3].kind, "tool"); // 旧格式保持 role=tool 的推导
    }

    /// parent_tool_call_id 全链路 roundtrip：普通消息 / tool_call / tool_result
    /// 写入后 SELECT 可读回；无父归属的消息该列为 NULL。
    #[tokio::test]
    async fn test_parent_tool_call_id_roundtrip() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 子 agent 文本（v2 直写 parent）
        db.agent_add_message_v2(&AgentMessageOpts {
            id: "m0".to_owned(),
            session_id: "s1".to_owned(),
            role: "assistant".to_owned(),
            content: "子 agent 输出".to_owned(),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            kind: "message".to_owned(),
            parent_tool_call_id: Some("task_1".to_owned()),
        })
        .await
        .unwrap();
        // 子 agent 内工具调用（upsert 携带 parent）
        let calls = serde_json::json!([{"id": "c1", "name": "shell", "arguments": "{}",
            "parent_tool_call_id": "task_1"}]);
        db.agent_upsert_tool_call(
            "m1",
            "s1",
            "c1",
            Some("shell"),
            &calls.to_string(),
            Some("task_1"),
        )
        .await
        .unwrap();
        db.agent_upsert_tool_result("m2", "s1", "c1", Some("shell"), "ok", Some("task_1"))
            .await
            .unwrap();
        // 主 agent 文本（parent=None → NULL）
        db.agent_add_message("m3", "s1", "assistant", "主 agent", None)
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        let by_kind = |k: &str| rows.iter().find(|r| r.kind == k).unwrap();
        assert_eq!(
            by_kind("message").parent_tool_call_id.as_deref(),
            Some("task_1")
        );
        let tc = by_kind("tool_calls");
        assert_eq!(tc.parent_tool_call_id.as_deref(), Some("task_1"));
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(tc.tool_calls.as_deref().unwrap()).unwrap();
        assert_eq!(parsed[0]["parent_tool_call_id"], "task_1");
        assert_eq!(
            by_kind("tool_result").parent_tool_call_id.as_deref(),
            Some("task_1")
        );
        // 主 agent 消息 parent 为 NULL
        let main_text = rows.iter().find(|r| r.content == "主 agent").unwrap();
        assert!(main_text.parent_tool_call_id.is_none());
    }

    /// upsert 更新路径的 parent 补全：先无 parent 落库，后到的带 parent 帧
    /// COALESCE 补上列值（同一 tool_call_id 归属固定）。
    #[tokio::test]
    async fn test_upsert_parent_backfilled_on_update() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        db.agent_upsert_tool_result("m1", "s1", "c1", Some("shell"), "", None)
            .await
            .unwrap();
        // 中间态已落库，终态带 parent → 更新路径补列
        db.agent_upsert_tool_result("m2", "s1", "c1", Some("shell"), "ok", Some("task_9"))
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "ok");
        assert_eq!(rows[0].parent_tool_call_id.as_deref(), Some("task_9"));
    }

    #[tokio::test]
    async fn test_config_state_upsert_and_clear() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "w".to_owned(),
            client_id: "c1".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/tmp".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 初始为空
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert!(s.config_state.is_none());

        // upsert 两个 key
        db.agent_update_session_config_state("s1", "mode", Some("plan"))
            .await
            .unwrap();
        db.agent_update_session_config_state("s1", "effort", Some("high"))
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        let map: serde_json::Value =
            serde_json::from_str(s.config_state.as_deref().unwrap()).unwrap();
        assert_eq!(map["mode"], "plan");
        assert_eq!(map["effort"], "high");

        // 覆盖已有 key
        db.agent_update_session_config_state("s1", "mode", Some("default"))
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        let map: serde_json::Value =
            serde_json::from_str(s.config_state.as_deref().unwrap()).unwrap();
        assert_eq!(map["mode"], "default");
        assert_eq!(map["effort"], "high");

        // 清空一个 key；清空全部后列回到 NULL
        db.agent_update_session_config_state("s1", "mode", None)
            .await
            .unwrap();
        db.agent_update_session_config_state("s1", "effort", None)
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert!(s.config_state.is_none());
    }

    /// context_used/context_size（ACP usage_update 快照列）覆盖式写入往返：
    /// 初始 NULL → 写入 → 覆盖为最新值（不累计）。
    #[tokio::test]
    async fn test_session_context_usage_snapshot() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "w".to_owned(),
            client_id: "c1".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/tmp".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 初始为空
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert!(s.context_used.is_none());
        assert!(s.context_size.is_none());

        // 写入快照
        db.agent_update_session_context_usage("s1", Some(1234), Some(200_000))
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.context_used, Some(1234));
        assert_eq!(s.context_size, Some(200_000));

        // 覆盖为最新快照（不累计）
        db.agent_update_session_context_usage("s1", Some(5678), Some(200_000))
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.context_used, Some(5678));
    }

    /// agent_config_overrides（v4 列）的创建→读取→更新→清空完整往返。
    /// update 语义：None 保持原值；Some("{}") 显式清空（与 llm_model_id 的
    /// 「不支持清空」不同，见 spec 决策表）。
    #[tokio::test]
    async fn test_workspace_config_overrides_roundtrip() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "acp-proj".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/workspace".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "claude-code".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();

        // 创建时未配置 → None
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert!(ws.agent_config_overrides.is_none());

        // 写入配置
        db.agent_update_workspace(
            "w1",
            &AgentWorkspaceUpdateOpts {
                name: "acp-proj".to_owned(),
                root_path: "/workspace".to_owned(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: Some(r#"{"model":"sonnet","fast":"haiku"}"#.to_owned()),
                claude_tier_models: None,
                clear_overrides: false,
                clear_tier_models: false,
            },
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.agent_config_overrides.as_deref(),
            Some(r#"{"model":"sonnet","fast":"haiku"}"#)
        );

        // COALESCE：None 保持原值
        db.agent_update_workspace(
            "w1",
            &AgentWorkspaceUpdateOpts {
                name: "acp-proj".to_owned(),
                root_path: "/workspace".to_owned(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: None,
                claude_tier_models: None,
                clear_overrides: false,
                clear_tier_models: false,
            },
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.agent_config_overrides.as_deref(),
            Some(r#"{"model":"sonnet","fast":"haiku"}"#)
        );

        // 显式 Some("") 写入空串由 API 层拦截（归一化 None）；DB 层原样写入 "{}"
        // 表示清空——这里验证 DB 层忠实存储传入值
        db.agent_update_workspace(
            "w1",
            &AgentWorkspaceUpdateOpts {
                name: "acp-proj".to_owned(),
                root_path: "/workspace".to_owned(),
                system_prompt: None,
                approval_mode: None,
                agent_type: None,
                agent_path: None,
                llm_model_id: None,
                agent_config_overrides: Some("{}".to_owned()),
                claude_tier_models: None,
                clear_overrides: false,
                clear_tier_models: false,
            },
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_config_overrides.as_deref(), Some("{}"));
    }

    /// GitHub 三列的写语义 + token 加解密 round-trip：API 层加密后落库 →
    /// 读回密文 → 用同一 LlmCipher 解密得到明文；「空串/缺省保持、非空更新」。
    #[tokio::test]
    async fn test_workspace_github_columns_roundtrip() {
        use rust_tunnel_common::crypto::LlmCipher;
        let cipher = LlmCipher::from_master_key([7u8; 32]);
        let token = "ghp_secret_token_123";
        let stored = cipher.encrypt(token);

        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();

        // 初始：三列均 NULL
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert!(ws.github_token.is_none());
        assert!(!ws.github_token_set());
        assert!(ws.github_owner.is_none());
        assert!(ws.github_repo.is_none());

        // 写 token 密文 + owner/repo
        db.agent_set_workspace_github("w1", Some(&stored), Some("octo"), Some("repo"))
            .await
            .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.github_token.as_deref(), Some(stored.as_str()));
        assert!(ws.github_token_set());
        assert_eq!(
            cipher.decrypt(ws.github_token.as_deref().unwrap()).unwrap(),
            token
        );
        assert_eq!(ws.github_owner.as_deref(), Some("octo"));
        assert_eq!(ws.github_repo.as_deref(), Some("repo"));

        // 空串 / None 保持原值；非空更新
        db.agent_set_workspace_github("w1", Some(""), Some(""), None)
            .await
            .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.github_token.as_deref(),
            Some(stored.as_str()),
            "空串保持密文"
        );
        assert_eq!(ws.github_owner.as_deref(), Some("octo"), "空串保持 owner");
        assert_eq!(ws.github_repo.as_deref(), Some("repo"), "None 保持 repo");

        let new_token = cipher.encrypt("ghp_new_token");
        db.agent_set_workspace_github("w1", Some(&new_token), Some("newowner"), Some("newrepo"))
            .await
            .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            cipher.decrypt(ws.github_token.as_deref().unwrap()).unwrap(),
            "ghp_new_token"
        );
        assert_eq!(ws.github_owner.as_deref(), Some("newowner"));
        assert_eq!(ws.github_repo.as_deref(), Some("newrepo"));
    }

    /// 脱敏：序列化 AgentWorkspaceRecord 时 github_token 密文/明文绝不进入 JSON，
    /// 只暴露 github_token_set 布尔位；其余字段（owner/repo）正常下发。
    #[tokio::test]
    async fn test_workspace_github_token_redacted_in_json() {
        use rust_tunnel_common::crypto::LlmCipher;
        let cipher = LlmCipher::from_master_key([9u8; 32]);
        let token = "ghp_ultra_secret_42";
        let stored = cipher.encrypt(token);

        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "p".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/p".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_set_workspace_github("w1", Some(&stored), Some("octo"), Some("repo"))
            .await
            .unwrap();

        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        let json = serde_json::to_value(&ws).unwrap();
        // 明文 token 与密文串都不得出现在 JSON 文本中
        let raw = json.to_string();
        assert!(!raw.contains(token), "明文 token 不得序列化");
        assert!(!raw.contains("enc:v1:"), "密文不得序列化");
        // 布尔位 + owner/repo 正常
        assert_eq!(json["github_token_set"], true);
        assert_eq!(json["github_owner"], "octo");
        assert_eq!(json["github_repo"], "repo");
        // 未配置时布尔位为 false
        db.agent_create_workspace(&AgentWorkspaceCreateOpts {
            id: "w2".to_owned(),
            name: "q".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/q".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: "".to_owned(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        let ws2 = db.agent_get_workspace("w2").await.unwrap().unwrap();
        let json2 = serde_json::to_value(&ws2).unwrap();
        assert_eq!(json2["github_token_set"], false);
        assert_eq!(json2["github_owner"], serde_json::Value::Null);
    }
}
