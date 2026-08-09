//! Agent workbench persistence: workspaces / sessions / messages.
use super::Database;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWorkspaceRecord {
    pub id: String,
    pub name: String,
    pub client_id: String,
    pub runtime_type: String,
    pub root_path: String,
    pub docker_image: Option<String>,
    pub docker_container_id: Option<String>,
    pub approval_mode: String,
    pub system_prompt: Option<String>,
    // ACP 远程 agent 字段。列由 `migrate_agent_workspaces_v3`（schema.rs）落地；
    // `#[sqlx(default)]` 保证旧库未跑 v3 迁移前 `SELECT *` 仍可解码（agent_type 为
    // NOT NULL 列，默认空串）。`agent_create/update_workspace` 均已支持读写这三列。
    #[sqlx(default)]
    #[serde(default)]
    pub agent_type: String,
    #[sqlx(default)]
    #[serde(default)]
    pub agent_path: Option<String>,
    #[sqlx(default)]
    #[serde(default)]
    pub llm_model_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentSessionRecord {
    pub id: String,
    pub workspace_id: String,
    pub title: Option<String>,
    pub status: String,
    pub model: Option<String>,
    /// ACP 会话配置状态（JSON map：config_id → value；仅用户显式切换过的项）
    pub config_state: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentMessageRecord {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    pub kind: String,
    pub created_at: String,
}

impl Database {
    // ── Workspace CRUD ──────────────────────────────────────────

    /// 创建 agent workspace。`agent_type` 为 ACP 远程 agent 类型（非空列，默认空串），
    /// `agent_path`/`llm_model_id` 可空。调用方暂未接入请求 DTO 时传 `""` / `None` 占位。
    #[allow(clippy::too_many_arguments)]
    pub async fn agent_create_workspace(
        &self,
        id: &str,
        name: &str,
        client_id: &str,
        runtime_type: &str,
        root_path: &str,
        docker_image: Option<&str>,
        docker_container_id: Option<&str>,
        agent_type: &str,
        agent_path: Option<&str>,
        llm_model_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO agent_workspaces
                (id, name, client_id, runtime_type, root_path, docker_image, docker_container_id,
                 agent_type, agent_path, llm_model_id)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(client_id)
        .bind(runtime_type)
        .bind(root_path)
        .bind(docker_image)
        .bind(docker_container_id)
        .bind(agent_type)
        .bind(agent_path)
        .bind(llm_model_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_get_workspace(
        &self,
        id: &str,
    ) -> Result<Option<AgentWorkspaceRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkspaceRecord>("SELECT * FROM agent_workspaces WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn agent_list_workspaces(&self) -> Result<Vec<AgentWorkspaceRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkspaceRecord>(
            "SELECT * FROM agent_workspaces ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// 更新 agent workspace 的可变字段。ACP 三个新字段（agent_type/agent_path/llm_model_id）
    /// 采用 COALESCE 语义：`None` 保持原值，`Some` 写入新值，与 `approval_mode` 一致。
    #[allow(clippy::too_many_arguments)]
    pub async fn agent_update_workspace(
        &self,
        id: &str,
        name: &str,
        root_path: &str,
        system_prompt: Option<&str>,
        approval_mode: Option<&str>,
        agent_type: Option<&str>,
        agent_path: Option<&str>,
        llm_model_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_workspaces SET name = ?, root_path = ?, system_prompt = ?, \
             approval_mode = COALESCE(?, approval_mode), \
             agent_type = COALESCE(?, agent_type), \
             agent_path = COALESCE(?, agent_path), \
             llm_model_id = COALESCE(?, llm_model_id), \
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(name)
        .bind(root_path)
        .bind(system_prompt)
        .bind(approval_mode)
        .bind(agent_type)
        .bind(agent_path)
        .bind(llm_model_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_delete_workspace(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_workspaces WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Session CRUD ────────────────────────────────────────────

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

    pub async fn agent_get_session(
        &self,
        id: &str,
    ) -> Result<Option<AgentSessionRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentSessionRecord>("SELECT * FROM agent_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

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
                map.insert(config_id.to_string(), serde_json::Value::String(v.to_string()));
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

    pub async fn agent_archive_session(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET status = 'archived', updated_at = datetime('now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_delete_session(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Messages ────────────────────────────────────────────────

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
        self.agent_add_message_v2(id, session_id, role, content, tool_calls, None, None, kind)
            .await
    }

    /// 新格式消息写入（全列）。`kind` 取值：message / tool_calls / tool_result / summary。
    #[allow(clippy::too_many_arguments)]
    pub async fn agent_add_message_v2(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        name: Option<&str>,
        kind: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_messages (id, session_id, role, content, tool_calls, tool_call_id, name, kind) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(session_id)
        .bind(role)
        .bind(content)
        .bind(tool_calls)
        .bind(tool_call_id)
        .bind(name)
        .bind(kind)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_list_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentMessageRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentMessageRecord>(
            "SELECT * FROM agent_messages WHERE session_id = ? ORDER BY created_at, rowid",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用：给 workspace 补写 llm_model_id（正式 CRUD 写入口随 Task 7
    /// 提供）。llm_bridge 测试用它构造已配置链路。
    #[cfg(test)]
    impl Database {
        pub(crate) async fn agent_set_workspace_llm_model_id(
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
    }

    #[tokio::test]
    async fn test_workspace_crud() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1",
            "my-proj",
            "nas",
            "host",
            "/home/user/proj",
            None,
            None,
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_workspace(
            "w2",
            "dproj",
            "nas",
            "docker",
            "/container/work",
            Some("node:20"),
            Some("dev-ctr"),
            "",
            None,
            None,
        )
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

        db.agent_update_workspace("w1", "renamed", "/new/path", None, None, None, None, None)
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
        db.agent_create_workspace(
            "w1",
            "acp-proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "gemini",
            Some("/opt/acp-agent"),
            Some("m1"),
        )
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
            "acp-proj",
            "/workspace",
            None,
            None,
            Some("claude"),
            Some("/opt/acp-claude"),
            Some("m2"),
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "claude");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-claude"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m2"));

        // COALESCE：None 保持原值
        db.agent_update_workspace("w1", "acp-proj", "/workspace", None, None, None, None, None)
            .await
            .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "claude");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-claude"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m2"));

        // 可空字段显式清空为 None
        db.agent_update_workspace(
            "w1",
            "acp-proj",
            "/workspace",
            None,
            None,
            None,
            None,
            Some("m3"),
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
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
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
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
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
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
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

    #[tokio::test]
    async fn test_delete_workspace_cascades() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
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
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None, "", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // assistant tool_calls 行
        db.agent_add_message_v2(
            "m1",
            "s1",
            "assistant",
            "",
            Some(r#"[{"id":"c1","type":"function","function":{"name":"shell","arguments":"{}"}}]"#),
            None,
            None,
            "tool_calls",
        )
        .await
        .unwrap();
        // tool 结果行
        db.agent_add_message_v2(
            "m2",
            "s1",
            "tool",
            "exit_code=0",
            None,
            Some("c1"),
            Some("shell"),
            "tool_result",
        )
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

    #[tokio::test]
    async fn test_config_state_upsert_and_clear() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "w", "c1", "host", "/tmp", None, None, "", None, None)
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
}
