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
    /// `workspace` 审批模式（`safe`/`auto_write`/`full_auto`），`load` 时从 workspace `record` 读取。
    pub approval_mode: String,
    /// AGENTS.md 内容缓存：None = 尚未尝试读取；Some("") = 已读但不存在/为空；
    /// Some(非空) = 已注入。runner 据此决定是否发起首次读取。
    pub agents_md: Option<String>,
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

        let mut messages = vec![ChatMessage::text(
            "system",
            build_system_prompt(workspace.system_prompt.as_deref(), None),
        )];
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
        sanitize_tool_pairs(&mut messages);

        Ok(Self {
            session_id: session_id.to_string(),
            workspace_id: session.workspace_id,
            client_id: workspace.client_id,
            runtime_type: workspace.runtime_type,
            root_path: workspace.root_path,
            docker_container: workspace.docker_container_id,
            model: session.model.unwrap_or_else(|| default_model.to_string()),
            approval_mode: workspace.approval_mode.clone(),
            agents_md: None,
            messages,
        })
    }
}

const SYSTEM_PROMPT: &str = "You are an AI programming assistant running inside a workspace on a remote machine. Use the provided tools (shell/read_file/write_file/list_dir/git_*) to inspect and modify the project. Prefer small, verifiable steps: read before write, run tests after changes. All paths are relative to the workspace root.";

/// AGENTS.md 注入上限（字节），超出截断。
const AGENTS_MD_MAX_BYTES: usize = 20 * 1024;

/// 截断 AGENTS.md 到注入上限（UTF-8 边界安全）。
pub fn truncate_agents_md(content: &str) -> String {
    if content.len() <= AGENTS_MD_MAX_BYTES {
        return content.to_string();
    }
    let mut cut = AGENTS_MD_MAX_BYTES;
    while !content.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated]", &content[..cut])
}

/// 三层合成系统提示词：内置 → workspace 自定义 → AGENTS.md。
/// 非空段按序用分隔线拼成单条 system 消息（多 system 消息有 provider 兼容性风险）。
pub fn build_system_prompt(workspace_prompt: Option<&str>, agents_md: Option<&str>) -> String {
    let mut parts = vec![SYSTEM_PROMPT.to_string()];
    if let Some(ws) = workspace_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(ws.to_string());
    }
    if let Some(md) = agents_md.map(str::trim).filter(|s| !s.is_empty()) {
        let truncated = truncate_agents_md(md);
        parts.push(format!("# Project instructions (AGENTS.md):\n{truncated}"));
    }
    parts.join("\n\n---\n\n")
}

/// 清洗孤儿工具消息，保证 assistant tool_calls 与 tool 结果一一配对。
///
/// 两种孤儿形态（都是 OpenAI/Anthropic 非法序列，一旦混入上下文模型直接 400）：
/// 1. 孤儿 tool 结果（tool_call_id 无配对 tool_calls）：压缩切割边界恰好把
///    assistant tool_calls 行压掉、留下配对的 tool 结果行时产生（find_cut_point
///    只在切割点落在 tool 序列中间时对齐，切割点落在 assistant tool_calls 行
///    正后方时对齐不到）。
/// 2. 缺结果的 tool_calls（assistant 声明的 tool_call 没有对应 tool 结果）：
///    工具执行中途连接断开/取消导致 runner 只落了部分 tool 结果。处理方式：
///    补齐一条 "[interrupted: tool execution did not complete]" 占位结果——
///    保留 assistant 意图（对摘要/上下文有信息量），避免整条丢弃。
fn sanitize_tool_pairs(messages: &mut Vec<ChatMessage>) {
    use std::collections::HashSet;
    let mut declared: HashSet<String> = HashSet::new();
    let mut removed_orphan_tools = 0usize;
    let mut patched_missing = 0usize;

    // 第一遍：丢弃孤儿 tool 结果（tool_call_id 未在此前任何 assistant tool_calls 中声明）。
    messages.retain(|m| {
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                if let Some(id) = c.get("id").and_then(|v| v.as_str()) {
                    declared.insert(id.to_string());
                }
            }
            return true;
        }
        if m.role == "tool" {
            let paired = m
                .tool_call_id
                .as_ref()
                .is_some_and(|id| declared.contains(id));
            if !paired {
                removed_orphan_tools += 1;
            }
            return paired;
        }
        true
    });

    // 第二遍：assistant tool_calls 中声明但缺结果的，原位补齐占位 tool 结果。
    // （OpenAI 要求每个 tool_call 都有紧跟的 tool 结果，缺一条即 400。）
    let mut i = 0;
    while i < messages.len() {
        let Some(calls) = messages[i].tool_calls.clone() else {
            i += 1;
            continue;
        };
        // 紧跟其后、已配对的 tool 结果 id 集合
        let mut j = i + 1;
        let mut have: HashSet<&str> = HashSet::new();
        while j < messages.len() && messages[j].role == "tool" {
            if let Some(id) = messages[j].tool_call_id.as_deref() {
                have.insert(id);
            }
            j += 1;
        }
        let missing: Vec<(String, String)> = calls
            .iter()
            .filter_map(|c| {
                let id = c.get("id")?.as_str()?.to_string();
                if have.contains(id.as_str()) {
                    return None;
                }
                let name = c
                    .pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                Some((id, name))
            })
            .collect();
        if !missing.is_empty() {
            patched_missing += missing.len();
            let placeholders: Vec<ChatMessage> = missing
                .into_iter()
                .map(|(id, name)| ChatMessage {
                    role: "tool".into(),
                    content: Some("[interrupted: tool execution did not complete]".to_string()),
                    tool_calls: None,
                    tool_call_id: Some(id),
                    name: Some(name),
                })
                .collect();
            let insert_at = j;
            messages.splice(insert_at..insert_at, placeholders);
            i = insert_at;
        }
        i += 1;
    }

    if removed_orphan_tools > 0 || patched_missing > 0 {
        tracing::warn!(
            removed_orphan_tools,
            patched_missing,
            "sanitize_tool_pairs: cleaned unpaired tool messages"
        );
    }
}

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
    async fn test_load_patches_missing_tool_results() {
        // 工具执行中断：assistant 声明了 2 个 tool_calls，只有第 1 个有结果落库。
        // 清洗后应为第 2 个补齐占位结果，序列合法（无 400）。
        // 取消链路（AgentExecCancel 停止回合）依赖 sanitize_tool_pairs 占位补齐。
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "改两处", None)
            .await
            .unwrap();
        db.agent_add_message_v2(
            "m2",
            "s1",
            "assistant",
            "",
            Some(
                r#"[{"id":"c1","type":"function","function":{"name":"shell","arguments":"{}"}},{"id":"c2","type":"function","function":{"name":"read_file","arguments":"{}"}}]"#,
            ),
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
            "ok",
            None,
            Some("c1"),
            Some("shell"),
            "tool_result",
        )
        .await
        .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        // system + user + assistant(tool_calls) + tool(c1) + tool(c2 占位)
        assert_eq!(rt.messages.len(), 5);
        assert_eq!(rt.messages[3].tool_call_id.as_deref(), Some("c1"));
        let patched = &rt.messages[4];
        assert_eq!(patched.role, "tool");
        assert_eq!(patched.tool_call_id.as_deref(), Some("c2"));
        assert_eq!(patched.name.as_deref(), Some("read_file"));
        assert!(patched.content.as_deref().unwrap().contains("interrupted"));
    }

    #[tokio::test]
    async fn test_load_drops_orphan_tool_results() {
        // 压缩切割点落在 assistant tool_calls 行正后方：tool 结果保留但配对行被压掉。
        // 清洗后孤儿 tool 结果应被丢弃。
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None, None)
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "q", None)
            .await
            .unwrap();
        // summary 之后只有孤儿 tool 结果（tool_calls 行落在 summary 之前被跳过）
        db.agent_add_message_v2(
            "m2",
            "s1",
            "user",
            "[上下文摘要] ...",
            None,
            None,
            None,
            "summary",
        )
        .await
        .unwrap();
        db.agent_add_message_v2(
            "m3",
            "s1",
            "tool",
            "ok",
            None,
            Some("c1"),
            Some("shell"),
            "tool_result",
        )
        .await
        .unwrap();
        db.agent_add_message("m4", "s1", "assistant", "继续", None)
            .await
            .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        let roles: Vec<&str> = rt.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant"]);
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

    #[test]
    fn test_build_system_prompt_layers() {
        // 仅内置
        let p = build_system_prompt(None, None);
        assert!(p.contains("AI programming assistant"));
        assert!(!p.contains("AGENTS.md"));

        // + workspace 段
        let p = build_system_prompt(Some("Use Rust 2024 edition."), None);
        assert!(p.contains("AI programming assistant"));
        assert!(p.contains("Use Rust 2024 edition."));
        assert!(p.contains("\n\n---\n\n"));

        // + AGENTS.md 段
        let p = build_system_prompt(Some("ws-rules"), Some("# Project\nAlways run tests."));
        assert!(p.contains("ws-rules"));
        assert!(p.contains("# Project instructions (AGENTS.md):"));
        assert!(p.contains("Always run tests."));
        // 顺序：内置 → workspace → AGENTS.md
        let builtin_pos = p.find("AI programming assistant").unwrap();
        let ws_pos = p.find("ws-rules").unwrap();
        let md_pos = p.find("Project instructions").unwrap();
        assert!(builtin_pos < ws_pos && ws_pos < md_pos);

        // 空白 workspace 段视为无
        let p = build_system_prompt(Some("   "), None);
        assert!(!p.contains("---"));
    }

    #[test]
    fn test_build_system_prompt_truncates_agents_md() {
        let big = "x".repeat(25 * 1024);
        let p = build_system_prompt(None, Some(&big));
        assert!(p.contains("[truncated]"));
        // 截断后 AGENTS.md 段不超过 20KB + 标记
        assert!(p.len() < 21 * 1024 + SYSTEM_PROMPT.len() + 200);
    }
}
