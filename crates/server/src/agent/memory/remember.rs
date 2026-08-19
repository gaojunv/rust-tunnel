//! remember 工具：模型在会话中主动保存记忆。服务端本地短路（**不进
//! `AgentCommand` 协议**——bincode 索引反序列化会破坏），默认 scope=workspace，
//! 不落审批（低危 + 面板可见可控）。
//!
//! 两条入口共享 [`remember_execute`]：
//! - 内置 runner 路径（[`remember_from_agent`]）：agent 在服务端运行，工具调用被
//!   runner 短路拦截；
//! - ACP 路径（`crate::agent::mcp`）：agent 进程在客户端，经 MCP-over-HTTP 回环
//!   到服务端调用，同样落本地保存。

use super::{
    upsert_memory_with_dedup, MemoryState, MEMORY_CONTENT_MAX_CHARS, MAX_TAGS, TAG_MAX_CHARS,
};
use crate::agent::{session::SessionRuntime, AgentState};

/// 解析并校验 remember 参数（不触 DB/向量，纯字符串处理）：返回 `(content, scope,
/// tags)`。校验失败 → Err（错误文本由调用方喂回模型）。
/// 与 `remember_execute` 分离供 `remember_from_agent` 在无 memory 时复用——保持
/// "参数校验错误优先于 memory 未启用"的既有错误顺序（`remember_rejects_*` 测试依赖）。
fn parse_remember_args(args_json: &str) -> Result<(String, String, Vec<String>), String> {
    let args: serde_json::Value =
        serde_json::from_str(args_json).map_err(|e| format!("invalid arguments: {e}"))?;
    let content = args
        .get("content")
        .and_then(serde_json::Value::as_str)
        .map_or("", str::trim);
    if content.is_empty() {
        return Err("remember requires a non-empty 'content' string".into());
    }
    if content.len() > MEMORY_CONTENT_MAX_CHARS {
        return Err(format!(
            "content too long (>{MEMORY_CONTENT_MAX_CHARS} chars)"
        ));
    }
    let scope = args
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("workspace");
    if !matches!(scope, "workspace" | "client" | "global") {
        return Err(format!(
            "invalid scope '{scope}' (expected workspace|client|global)"
        ));
    }
    let tags: Vec<String> = args
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if tags.len() > MAX_TAGS {
        return Err(format!("too many tags (>{MAX_TAGS})"));
    }
    if let Some(over) = tags.iter().find(|t| t.len() > TAG_MAX_CHARS) {
        return Err(format!("tag too long (>{TAG_MAX_CHARS} chars): '{over}'"));
    }
    Ok((content.to_string(), scope.to_string(), tags))
}

/// 处理一次 remember 工具调用。参数校验失败 / enabled 关闭 / embedding 失败 /
/// 落库失败 → Err（错误文本由调用方喂回模型）；成功返回摘要文本。
/// 与 agent 解耦：调用方只需提供 `MemoryState` 与会话三坐标（runner 与 MCP 共用）。
pub(crate) async fn remember_execute(
    memory: &MemoryState,
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    args_json: &str,
) -> Result<String, String> {
    let (content, scope, tags) = parse_remember_args(args_json)?;
    // enabled 检查（先于任何副作用）。
    let s = memory.settings().await;
    if s.enabled == 0 {
        return Err("AI memory is disabled".into());
    }
    let Some(emb) = memory.embedder().await else {
        return Err("memory embedding not configured".into());
    };
    let (scope_type, cid, wid) = super::scope_coords(&scope, client_id, workspace_id);
    let id = upsert_memory_with_dedup(
        memory,
        &s,
        &emb,
        &content,
        &scope_type,
        &cid,
        &wid,
        &tags,
        1.0,
        session_id,
        "remember",
    )
    .await?;
    Ok(format!("memory saved (id={id}, scope={scope})"))
}

/// 内置 runner 路径：解引用 `agent.memory`（未启用 → Err）后委托共享核心
/// [`remember_execute`]。行为与 ACP/MCP 入口一致。
///
/// # Errors
/// 参数非法、memory 未启用 / 关闭 / embedding 未配置或失败、落库失败时返回
/// Err（错误文本由调用方喂回模型）。
pub async fn remember_from_agent(
    agent: &AgentState,
    rt: &SessionRuntime,
    args_json: &str,
) -> Result<String, String> {
    // 参数校验先于 memory 解引用：无 memory 时校验错误同样优先返回（既有行为），
    // 校验通过才报 "AI memory is not enabled"。
    parse_remember_args(args_json)?;
    let Some(memory) = &agent.memory else {
        return Err("AI memory is not enabled".into());
    };
    remember_execute(memory, &rt.client_id, &rt.workspace_id, &rt.session_id, args_json).await
}

#[cfg(all(test, feature = "rag"))]
mod tests {
    use super::*;
    use crate::db::Database;

    fn rt(session_id: &str, client_id: &str, workspace_id: &str) -> SessionRuntime {
        SessionRuntime {
            session_id: session_id.into(),
            workspace_id: workspace_id.into(),
            client_id: client_id.into(),
            runtime_type: "host".into(),
            root_path: "/p".into(),
            docker_container: None,
            model: "m".into(),
            approval_mode: "safe".into(),
            agents_md: None,
            memory_block: None,
            skill_list_block: None,
            roles_block: None,
            messages: Vec::new(),
            todos: Vec::new(),
            depth: 0,
            parent_tool_call_id: None,
            file_hashes: std::collections::HashMap::new(),
            active_role: None,
        }
    }

    async fn agent_with_memory(db: Database) -> AgentState {
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 1;
        s.emb_base_url = "http://localhost:1/v1".into();
        s.emb_api_key = "key".into();
        s.emb_model = "m".into();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();
        crate::agent::AgentState::new(
            crate::control::client_registry::ClientRegistry::new(db.clone()),
            db.clone(),
        )
        .with_memory(memory)
    }

    async fn agent_without_memory(db: Database) -> AgentState {
        crate::agent::AgentState::new(
            crate::control::client_registry::ClientRegistry::new(db.clone()),
            db,
        )
    }

    #[tokio::test]
    async fn remember_requires_content() {
        let db = Database::new(":memory:").await.unwrap();
        let agent = agent_without_memory(db).await;
        let r = rt("s1", "c1", "w1");
        assert!(remember_from_agent(&agent, &r, r#"{"content": "  "}"#).await.is_err());
        assert!(remember_from_agent(&agent, &r, r#"{"scope": "workspace"}"#).await.is_err());
        assert!(remember_from_agent(&agent, &r, "not json").await.is_err());
    }

    #[tokio::test]
    async fn remember_rejects_oversized_and_bad_scope() {
        let db = Database::new(":memory:").await.unwrap();
        let agent = agent_without_memory(db).await;
        let r = rt("s1", "c1", "w1");
        let big = "x".repeat(MEMORY_CONTENT_MAX_CHARS + 1);
        let args = serde_json::json!({"content": big}).to_string();
        assert!(remember_from_agent(&agent, &r, &args).await.unwrap_err().contains("too long"));
        let args = r#"{"content": "ok", "scope": "bogus"}"#;
        assert!(remember_from_agent(&agent, &r, args).await.unwrap_err().contains("invalid scope"));
    }

    #[tokio::test]
    async fn remember_rejects_too_many_or_long_tags() {
        let db = Database::new(":memory:").await.unwrap();
        let agent = agent_without_memory(db).await;
        let r = rt("s1", "c1", "w1");
        let tags: Vec<&str> = (0..9)
            .map(|i| {
                let s: &str = format!("t{i}").leak();
                s
            })
            .collect();
        let args = serde_json::json!({"content": "ok", "tags": tags}).to_string();
        let err = remember_from_agent(&agent, &r, &args).await.unwrap_err();
        assert!(err.contains("too many tags"), "err: {err}");
        let long_tag = "y".repeat(TAG_MAX_CHARS + 1);
        let args = serde_json::json!({"content": "ok", "tags": [long_tag]}).to_string();
        let err = remember_from_agent(&agent, &r, &args).await.unwrap_err();
        assert!(err.contains("tag too long"), "err: {err}");
    }

    #[tokio::test]
    async fn remember_embedding_not_configured_is_err() {
        // enabled=1 但 base_url 不可达 → embedding 失败 → Err（错误喂回模型）。
        let db = Database::new(":memory:").await.unwrap();
        let agent = agent_with_memory(db).await;
        let r = rt("s1", "c1", "w1");
        let args = r#"{"content": "用户偏好简洁", "scope": "workspace", "tags": ["rust"]}"#;
        let err = remember_from_agent(&agent, &r, args).await.unwrap_err();
        assert!(err.contains("embedding") || err.contains("memory"), "err: {err}");
    }

    #[tokio::test]
    async fn remember_disabled_returns_err() {
        let db = Database::new(":memory:").await.unwrap();
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        // enabled 默认 0
        let agent = crate::agent::AgentState::new(
            crate::control::client_registry::ClientRegistry::new(db.clone()),
            db.clone(),
        )
        .with_memory(memory);
        let r = rt("s1", "c1", "w1");
        let err = remember_from_agent(&agent, &r, r#"{"content": "x"}"#).await.unwrap_err();
        assert!(err.contains("disabled"), "err: {err}");
    }

    #[tokio::test]
    async fn remember_without_memory_returns_err() {
        let db = Database::new(":memory:").await.unwrap();
        let agent = agent_without_memory(db).await;
        let r = rt("s1", "c1", "w1");
        let err = remember_from_agent(&agent, &r, r#"{"content": "x"}"#).await.unwrap_err();
        assert!(err.contains("not enabled"), "err: {err}");
    }
}
