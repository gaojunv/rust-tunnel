//! Session runtime: rebuilds conversation state from DB.
use crate::server::db::Database;
use crate::server::llm::ChatMessage;

pub struct SessionRuntime {
    pub session_id: String,
    pub workspace_id: String,
    pub client_id: String,
    pub root_path: String,
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

impl SessionRuntime {
    /// Rebuild conversation state from the database.
    /// `default_model` is used when the session has no model set.
    pub async fn load(
        db: &Database,
        session_id: &str,
        default_model: &str,
    ) -> Result<Self, String> {
        let session = db
            .agent_get_session(session_id)
            .await
            .map_err(|e| format!("db error: {e}"))?
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let workspace = db
            .agent_get_workspace(&session.workspace_id)
            .await
            .map_err(|e| format!("db error: {e}"))?
            .ok_or_else(|| format!("workspace not found: {}", session.workspace_id))?;

        let records = db
            .agent_list_messages(session_id)
            .await
            .map_err(|e| format!("db error: {e}"))?;
        let mut messages = vec![ChatMessage::text("system", SYSTEM_PROMPT)];
        for r in records {
            messages.push(ChatMessage {
                role: r.role,
                content: Some(r.content),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }

        Ok(Self {
            session_id: session_id.to_string(),
            workspace_id: session.workspace_id,
            client_id: workspace.client_id,
            root_path: workspace.root_path,
            model: session.model.unwrap_or_else(|| default_model.to_string()),
            messages,
        })
    }
}

const SYSTEM_PROMPT: &str = "You are an AI programming assistant running inside a workspace on a remote machine. Use the provided tools (shell/read_file/write_file/list_dir/git_*) to inspect and modify the project. Prefer small, verifiable steps: read before write, run tests after changes. All paths are relative to the workspace root.";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_load_session_rebuilds_history() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "帮我改代码", None)
            .await
            .unwrap();
        db.agent_add_message(
            "m2",
            "s1",
            "assistant",
            "好的",
            Some(r#"[{"name":"shell","args":"{\"cmd\":\"ls\"}","result":"a.rs"}]"#),
        )
        .await
        .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "default-model")
            .await
            .unwrap();
        assert_eq!(rt.workspace_id, "w1");
        assert_eq!(rt.client_id, "nas");
        assert_eq!(rt.root_path, "/p");
        assert_eq!(rt.model, "gpt-4o");
        assert_eq!(rt.messages.len(), 3);
        assert_eq!(rt.messages[0].role, "system");
        assert_eq!(rt.messages[1].role, "user");
    }

    #[tokio::test]
    async fn test_load_session_model_fallback() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None).await.unwrap();
        let rt = SessionRuntime::load(&db, "s1", "fallback-model")
            .await
            .unwrap();
        assert_eq!(rt.model, "fallback-model");
    }

    #[tokio::test]
    async fn test_load_nonexistent_session() {
        let db = Database::new(":memory:").await.unwrap();
        assert!(SessionRuntime::load(&db, "ghost", "m").await.is_err());
    }
}
