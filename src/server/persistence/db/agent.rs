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
    pub created_at: String,
}

impl Database {
    // ── Workspace CRUD ──────────────────────────────────────────

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
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO agent_workspaces
                (id, name, client_id, runtime_type, root_path, docker_image, docker_container_id)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(client_id)
        .bind(runtime_type)
        .bind(root_path)
        .bind(docker_image)
        .bind(docker_container_id)
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

    pub async fn agent_update_workspace(
        &self,
        id: &str,
        name: &str,
        root_path: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_workspaces SET name = ?, root_path = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(name)
        .bind(root_path)
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
        sqlx::query(
            "INSERT INTO agent_messages (id, session_id, role, content, tool_calls) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(session_id)
        .bind(role)
        .bind(content)
        .bind(tool_calls)
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

        db.agent_update_workspace("w1", "renamed", "/new/path")
            .await
            .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.name, "renamed");
        assert_eq!(ws.root_path, "/new/path");

        db.agent_delete_workspace("w1").await.unwrap();
        assert!(db.agent_get_workspace("w1").await.unwrap().is_none());
        db.agent_delete_workspace("w2").await.unwrap();
        assert!(db.agent_get_workspace("w2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_session_crud_and_archive() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
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
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
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
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
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
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
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
}
