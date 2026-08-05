//! Session runtime: rebuilds conversation state from DB.
use crate::server::db::Database;
use crate::server::llm::ChatMessage;

pub struct SessionRuntime {
    pub session_id: String,
    pub workspace_id: String,
    pub client_id: String,
    pub runtime_type: String,
    pub root_path: String,
    /// Container to exec into when `runtime_type == "docker"`; None means the
    /// workspace is docker-typed but has no container yet (or host runtime).
    pub docker_container: Option<String>,
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

        // 上下文压缩：只重放最后一个 summary 行及之后的消息（LLM 视角）。
        // 被压缩的原始消息保留在 DB，UI 历史仍可见完整记录。
        let start = records
            .iter()
            .rposition(|r| r.kind == "summary")
            .unwrap_or(0);

        let mut messages = vec![ChatMessage::text("system", SYSTEM_PROMPT)];
        for r in &records[start..] {
            match r.kind.as_str() {
                // 旧格式（kind='tool' 的合并行、assistant tool_calls 未持久化）重放会产生
                // 非法 OpenAI 序列（tool 消息无 tool_call_id），故跳过。新格式
                // （tool_calls/tool_result 行）恢复完整结构；summary 行之后才是有效上下文。
                "tool" => continue,
                // 迁移前遗留行：SQLite DEFAULT 使 role='tool' 的旧行 kind='message'，
                // 不能落入 _ 分支被当作普通工具文本消息重放，同样跳过。
                "message" if r.role == "tool" => continue,
                // assistant 的工具调用记录：恢复原始 tool_calls JSON。
                "tool_calls" => messages.push(ChatMessage {
                    role: "assistant".into(),
                    content: None,
                    tool_calls: r
                        .tool_calls
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(s).ok()),
                    tool_call_id: None,
                    name: None,
                }),
                // 单条工具结果：恢复 tool_call_id/name，与 tool_calls 配对。
                "tool_result" => messages.push(ChatMessage {
                    role: "tool".into(),
                    content: Some(r.content.clone()),
                    tool_calls: None,
                    tool_call_id: r.tool_call_id.clone(),
                    name: r.name.clone(),
                }),
                // message / summary：普通文本消息。
                _ => messages.push(ChatMessage::text(&r.role, &r.content)),
            }
        }

        Ok(Self {
            session_id: session_id.to_string(),
            workspace_id: session.workspace_id,
            client_id: workspace.client_id,
            runtime_type: workspace.runtime_type,
            root_path: workspace.root_path,
            docker_container: workspace.docker_container_id,
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
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
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
    async fn test_load_session_skips_tool_rows() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "改代码", None)
            .await
            .unwrap();
        db.agent_add_message("m2", "s1", "tool", "", Some(r#"[{"name":"shell"}]"#))
            .await
            .unwrap();
        db.agent_add_message("m3", "s1", "assistant", "已完成", None)
            .await
            .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        // system + user + assistant — the tool row must be skipped
        let roles: Vec<&str> = rt.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant"]);
    }

    #[tokio::test]
    async fn test_load_session_model_fallback() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        let rt = SessionRuntime::load(&db, "s1", "fallback-model")
            .await
            .unwrap();
        assert_eq!(rt.model, "fallback-model");
    }

    #[tokio::test]
    async fn test_load_session_docker_workspace() {
        let db = Database::new(":memory:").await.unwrap();
        // docker 运行时，container 已启动并登记 id
        db.agent_create_workspace(
            "w1",
            "p",
            "nas",
            "docker",
            "/container/work",
            Some("node:20"),
            Some("dev-ctr"),
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        let rt = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        assert_eq!(rt.runtime_type, "docker");
        assert_eq!(rt.docker_container.as_deref(), Some("dev-ctr"));
        assert_eq!(rt.root_path, "/container/work");

        // docker 运行时但容器未启动（container_id 为空）
        db.agent_create_workspace("w2", "p", "nas", "docker", "/x", Some("node:20"), None)
            .await
            .unwrap();
        db.agent_create_session("s2", "w2", None, None)
            .await
            .unwrap();
        let rt = SessionRuntime::load(&db, "s2", "m").await.unwrap();
        assert_eq!(rt.runtime_type, "docker");
        assert_eq!(rt.docker_container, None);
    }

    #[tokio::test]
    async fn test_load_nonexistent_session() {
        let db = Database::new(":memory:").await.unwrap();
        assert!(SessionRuntime::load(&db, "ghost", "m").await.is_err());
    }

    #[tokio::test]
    async fn test_load_replays_new_format_tool_structure() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "看下文件", None)
            .await
            .unwrap();
        db.agent_add_message_v2(
            "m2",
            "s1",
            "assistant",
            "",
            Some(r#"[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}]"#),
            None,
            None,
            "tool_calls",
        )
        .await
        .unwrap();
        db.agent_add_message_v2(
            "m3",
            "s1",
            "tool",
            "fn main(){}",
            None,
            Some("c1"),
            Some("read_file"),
            "tool_result",
        )
        .await
        .unwrap();
        db.agent_add_message("m4", "s1", "assistant", "文件里是 main 函数", None)
            .await
            .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        // system + user + assistant(tool_calls) + tool + assistant
        assert_eq!(rt.messages.len(), 5);
        assert_eq!(rt.messages[2].role, "assistant");
        assert!(rt.messages[2].tool_calls.is_some());
        assert_eq!(rt.messages[2].content, None);
        assert_eq!(rt.messages[3].role, "tool");
        assert_eq!(rt.messages[3].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(rt.messages[3].name.as_deref(), Some("read_file"));
        assert_eq!(rt.messages[3].content.as_deref(), Some("fn main(){}"));
    }

    #[tokio::test]
    async fn test_load_resumes_from_last_summary() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "早期对话", None)
            .await
            .unwrap();
        db.agent_add_message("m2", "s1", "assistant", "早期回复", None)
            .await
            .unwrap();
        db.agent_add_message_v2(
            "m3",
            "s1",
            "user",
            "[上下文摘要] 之前讨论了 X",
            None,
            None,
            None,
            "summary",
        )
        .await
        .unwrap();
        db.agent_add_message("m4", "s1", "user", "近期问题", None)
            .await
            .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        // system + summary + 近期 user；summary 之前的消息被跳过
        let roles: Vec<&str> = rt.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "user"]);
        assert!(rt.messages[1]
            .content
            .as_deref()
            .unwrap()
            .contains("上下文摘要"));
        assert_eq!(rt.messages[2].content.as_deref(), Some("近期问题"));
    }

    #[tokio::test]
    async fn test_load_skips_legacy_tool_rows() {
        // 迁移前遗留行：SQLite DEFAULT 把 kind 补成 'message'，role='tool' 的旧合并行
        // 必须被跳过（不能落入普通文本消息分支产生非法 OpenAI 序列）。
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "改代码", None)
            .await
            .unwrap();
        db.agent_add_message_v2("m2", "s1", "tool", "", None, None, None, "message")
            .await
            .unwrap();
        db.agent_add_message("m3", "s1", "assistant", "已完成", None)
            .await
            .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        let roles: Vec<&str> = rt.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant"]);
        assert_eq!(rt.messages.len(), 3);
    }
}
