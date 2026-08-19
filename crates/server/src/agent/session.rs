//! Session runtime: rebuilds conversation state from DB.
use crate::db::Database;
use crate::llm::{ChatMessage, LlmState};

/// 全局默认模型的 server_setting key（agent.rs 的 PUT /api/agent/default-model 共用）。
pub const DEFAULT_MODEL_KEY: &str = "agent_default_model";

/// 把 workspace 的 `llm_model_id`（可为 `model:<id>` / `group:<id>` / 历史裸值）
/// 解析为网关可解析的模型引用（model_name / 组名 / 原样名字）。
///
/// - `model:<id>` → 查 `llm_models` 得 model_name（模型不存在/禁用报错）。
/// - `group:<id>` → 查 `llm_model_groups` 得组名（组不存在/禁用报错）。
/// - 历史裸值：命中 `llm_models.id` → model_name；未命中 → 原样直通（可能是
///   alias/model_name/组名，交给网关 `resolve_with_failover` 解析）。
/// - `None`/空 → Ok(None)（未配置）。
pub async fn resolve_workspace_model_ref(
    db: &Database,
    llm_model_id: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(raw) = llm_model_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Some(id) = raw.strip_prefix("model:") {
        let m = db
            .llm_get_model(id)
            .await
            .map_err(|e| format!("db error reading model: {e}"))?
            .ok_or_else(|| format!("llm model not found: {id}"))?;
        if m.enabled == 0 {
            return Err(format!("llm model disabled: {id}"));
        }
        return Ok(Some(m.model_name));
    }
    if let Some(id) = raw.strip_prefix("group:") {
        let g = db
            .llm_get_model_group(id)
            .await
            .map_err(|e| format!("db error reading model group: {e}"))?
            .ok_or_else(|| format!("llm model group not found: {id}"))?;
        if g.enabled == 0 {
            return Err(format!("llm model group disabled: {id}"));
        }
        return Ok(Some(g.name));
    }
    // 历史裸值：命中 llm_models.id → model_name（保持既有「禁用即报错」语义）
    if let Some(m) = db
        .llm_get_model(raw)
        .await
        .map_err(|e| format!("db error reading model: {e}"))?
    {
        if m.enabled == 0 {
            return Err(format!("llm model disabled: {raw}"));
        }
        return Ok(Some(m.model_name));
    }
    // 未命中：原样直通（网关按 alias/model_name/组名解析）
    Ok(Some(raw.to_string()))
}

/// 统一模型解析（内置 runner 与 ACP 两条路径共用）：
/// `session.model` → `workspace.llm_model_id`（经 [`resolve_workspace_model_ref`]）
/// → 全局默认（`agent_default_model`）→ 第一个可用模型。
/// 返回网关可解析的模型引用（model_name / 别名 / 组名）。
/// `llm` 用于「第一个可用」兜底；`None` 时该层跳过。
pub async fn resolve_effective_model(
    db: &Database,
    llm: Option<&LlmState>,
    session_id: &str,
) -> Result<String, String> {
    let session = db
        .agent_get_session(session_id)
        .await
        .map_err(|e| format!("db error reading session: {e}"))?
        .ok_or_else(|| format!("agent session not found: {session_id}"))?;
    // 1. session.model：前端 ModelSelect 存 alias/model_name/组名，直接作网关引用
    if let Some(m) = session
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(m.to_string());
    }
    // 2. workspace.llm_model_id
    let ws = db
        .agent_get_workspace(&session.workspace_id)
        .await
        .map_err(|e| format!("db error reading workspace: {e}"))?
        .ok_or_else(|| "agent workspace not found".to_string())?;
    if let Some(r) = resolve_workspace_model_ref(db, ws.llm_model_id.as_deref()).await? {
        return Ok(r);
    }
    // 3. 全局默认
    if let Ok(Some(d)) = db.load_server_setting(DEFAULT_MODEL_KEY).await {
        let d = d.trim();
        if !d.is_empty() {
            return Ok(d.to_string());
        }
    }
    // 4. 第一个可用模型
    if let Some(llm) = llm {
        if let Ok(models) = crate::llm::router::list_available_models(llm).await {
            if let Some(first) = models.first() {
                if let Some(name) = first.get("id").and_then(|v| v.as_str()) {
                    return Ok(name.to_string());
                }
            }
        }
    }
    Err("no LLM model configured".into())
}

/// 是否存在任一显式模型配置（`session.model` / `workspace.llm_model_id` /
/// 全局默认）。ACP `ensure_session` spawn 前门禁用——实际解析由
/// [`resolve_effective_model`] 按 session 从 DB 完成，这里只防「spawn 后才发现
/// 无模型」。session 不存在时视为无 session.model。
pub async fn has_any_model_config(
    db: &Database,
    session_id: &str,
    workspace_llm_model_id: Option<&str>,
) -> Result<bool, String> {
    let session = db
        .agent_get_session(session_id)
        .await
        .map_err(|e| format!("db error reading session: {e}"))?;
    if session
        .as_ref()
        .and_then(|s| s.model.as_deref())
        .is_some_and(|m| !m.trim().is_empty())
    {
        return Ok(true);
    }
    if workspace_llm_model_id.is_some_and(|s| !s.trim().is_empty()) {
        return Ok(true);
    }
    if let Ok(Some(d)) = db.load_server_setting(DEFAULT_MODEL_KEY).await {
        if !d.trim().is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

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
    /// `workspace` 审批模式（`safe`/`auto_write`/`full_auto`/`plan`），`load` 时从 workspace `record` 读取。
    pub approval_mode: String,
    /// 任务清单（todo_write 工具维护）：内存态，全量替换语义。
    /// 会话恢复不持久化（重连/刷新后清空），复杂度与收益权衡。
    pub todos: Vec<super::tools::TodoItem>,
    /// AGENTS.md 内容缓存：None = 尚未尝试读取；Some("") = 已读但不存在/为空；
    /// Some(非空) = 已注入。runner 据此决定是否发起首次读取。
    pub agents_md: Option<String>,
    /// AI 记忆注入缓存：None = 尚未检索；Some("") = 无可用记忆；Some(非空) = 已注入
    /// 到 `messages[0]`（system 单条，不落库）。每会话只检索一次（对齐 agents_md）。
    pub memory_block: Option<String>,
    /// Skill 清单注入缓存：None = 尚未检索；Some("") = 无可用技能（或 skill 库
    /// 未开启）；Some(非空) = 已注入 `<skills>` 块到 `messages[0]`。与 memory_block
    /// 同模式：每会话只检索一次（纯 SQL，零 embedding 依赖）。
    pub skill_list_block: Option<String>,
    pub messages: Vec<ChatMessage>,
    /// 子 agent 深度：0 = 主循环，1 = 子 agent（防止递归委托）。
    pub depth: u8,
    /// 子 agent 的父 tool_call_id（WS 帧 parent 注入用；主 rt 为 None）。
    pub parent_tool_call_id: Option<String>,
    /// 文件内容 SHA-256 哈希缓存（path → hex），用于 stale 检测：
    /// read_file 完整读取后记录，WriteOutcome.file_hash 刷新，stale 错误时清除。
    pub file_hashes: std::collections::HashMap<String, String>,
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
                    reasoning_content: None,
                    tool_calls: r
                        .tool_calls
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(s).ok()),
                    tool_call_id: None,
                    name: None,
                }),
                // 单条工具结果：恢复 tool_call_id/name，与 tool_calls 配对。
                // M2 起 ACP 路径落库 content 为 JSON `{"text","status",...}`（见
                // agent/tool_result.rs）——重放给 LLM 只取 text 字段；存量旧行是
                // 纯文本，tool_result_text 返回 None 时原样使用（向后兼容）。
                "tool_result" => messages.push(ChatMessage {
                    role: "tool".into(),
                    content: Some(
                        crate::agent::tool_result::tool_result_text(&r.content)
                            .unwrap_or_else(|| r.content.clone()),
                    ),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: r.tool_call_id.clone(),
                    name: r.name.clone(),
                }),
                // message / summary：普通文本消息。
                // thought 行是 DeepSeek reasoning_content 落库，不回传上游 LLM 上下文。
                "message" if r.name.as_deref() == Some("thought") => continue,
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
            // 模型优先级：session.model → workspace.llm_model_id → 全局默认。
            // workspace 引用解析失败（model/group 不存在或禁用）→ 整个 load 报错，
            // 不静默回退默认（显式配置了就必须可用）。
            model: {
                if let Some(m) = session
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    m.to_string()
                } else if let Some(r) =
                    resolve_workspace_model_ref(db, workspace.llm_model_id.as_deref()).await?
                {
                    r
                } else {
                    default_model.to_string()
                }
            },
            approval_mode: workspace.approval_mode.clone(),
            agents_md: None,
            memory_block: None,
            skill_list_block: None,
            todos: vec![],
            messages,
            depth: 0,
            parent_tool_call_id: None,
            file_hashes: std::collections::HashMap::new(),
        })
    }

    /// 构造子 agent 运行时：复制父会话关键字段，messages 只含 system + user(prompt)，
    /// 不注入 AGENTS.md / memory / skill（子循环独立上下文）。
    pub fn subagent(
        parent: &SessionRuntime,
        system_prompt: String,
        task_prompt: &str,
        parent_tool_call_id: &str,
    ) -> Self {
        Self {
            session_id: parent.session_id.clone(),
            workspace_id: parent.workspace_id.clone(),
            client_id: parent.client_id.clone(),
            runtime_type: parent.runtime_type.clone(),
            root_path: parent.root_path.clone(),
            docker_container: parent.docker_container.clone(),
            model: parent.model.clone(),
            approval_mode: parent.approval_mode.clone(),
            todos: vec![],
            agents_md: Some(String::new()),
            memory_block: Some(String::new()),
            skill_list_block: Some(String::new()),
            messages: vec![
                ChatMessage::text("system", system_prompt),
                ChatMessage::text("user", task_prompt),
            ],
            depth: parent.depth + 1,
            parent_tool_call_id: Some(parent_tool_call_id.to_string()),
            file_hashes: std::collections::HashMap::new(),
        }
    }
}

const SYSTEM_PROMPT: &str = "You are an AI programming assistant running inside a workspace on a remote machine. Use the provided tools (shell/read_file/write_file/list_dir/git_*) to inspect and modify the project. Prefer small, verifiable steps: read before write, run tests after changes. All paths are relative to the workspace root.\n\n## Delegation\nUse the `task` tool to delegate exploration/research subtasks (code searches, multi-file reading, investigations) to a sub-agent with isolated context. It returns only a summary, keeping the main context clean. Prefer task for open-ended questions that would require many tool calls.";

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
                    reasoning_content: None,
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

    /// 建 provider + model（固定 prov-1 启用）。
    async fn save_named_model(db: &Database, model_id: &str, model_name: &str, enabled: bool) {
        db.llm_save_provider(
            "prov-1",
            "test-provider",
            "deepseek",
            "https://llm.example.test",
            "sk-test-123",
            None,
            None,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model(model_id, "prov-1", model_name, "", "", enabled, None)
            .await
            .unwrap();
    }

    // ── resolve_workspace_model_ref ─────────────────────────────

    #[tokio::test]
    async fn test_workspace_ref_none_is_ok() {
        let db = Database::new(":memory:").await.unwrap();
        assert_eq!(resolve_workspace_model_ref(&db, None).await.unwrap(), None);
        assert_eq!(
            resolve_workspace_model_ref(&db, Some("  ")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_workspace_ref_model_prefix() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "deepseek-chat", true).await;
        assert_eq!(
            resolve_workspace_model_ref(&db, Some("model:m1"))
                .await
                .unwrap(),
            Some("deepseek-chat".to_string())
        );
    }

    #[tokio::test]
    async fn test_workspace_ref_group_prefix() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "model-a", true).await;
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1)])
            .await
            .unwrap();
        assert_eq!(
            resolve_workspace_model_ref(&db, Some("group:g1"))
                .await
                .unwrap(),
            Some("router".to_string())
        );
    }

    #[tokio::test]
    async fn test_workspace_ref_legacy_uuid_hit() {
        // 历史裸 model id：命中 llm_models.id → model_name
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "model-1", "gpt-legacy", true).await;
        assert_eq!(
            resolve_workspace_model_ref(&db, Some("model-1"))
                .await
                .unwrap(),
            Some("gpt-legacy".to_string())
        );
    }

    #[tokio::test]
    async fn test_workspace_ref_legacy_name_passthrough() {
        // 历史裸值未命中 llm_models.id（可能是 alias/model_name/组名）→ 原样直通
        let db = Database::new(":memory:").await.unwrap();
        assert_eq!(
            resolve_workspace_model_ref(&db, Some("deepseek-chat"))
                .await
                .unwrap(),
            Some("deepseek-chat".to_string())
        );
    }

    #[tokio::test]
    async fn test_workspace_ref_model_disabled_errors() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "deepseek-chat", false).await;
        let err = resolve_workspace_model_ref(&db, Some("model:m1"))
            .await
            .unwrap_err();
        assert!(err.contains("disabled"), "err: {err}");
    }

    #[tokio::test]
    async fn test_workspace_ref_group_disabled_errors() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "model-a", true).await;
        db.llm_create_model_group("g1", "router", false)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1)])
            .await
            .unwrap();
        let err = resolve_workspace_model_ref(&db, Some("group:g1"))
            .await
            .unwrap_err();
        assert!(err.contains("disabled"), "err: {err}");
    }

    #[tokio::test]
    async fn test_workspace_ref_model_not_found_errors() {
        let db = Database::new(":memory:").await.unwrap();
        let err = resolve_workspace_model_ref(&db, Some("model:ghost"))
            .await
            .unwrap_err();
        assert!(err.contains("not found"), "err: {err}");
    }

    // ── resolve_effective_model 优先级 ──────────────────────────

    async fn seed_ws(db: &Database, ws_id: &str, llm_model_id: Option<&str>) {
        db.agent_create_workspace(
            ws_id, "proj", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        if let Some(mid) = llm_model_id {
            db.agent_set_workspace_llm_model_id(ws_id, mid)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_effective_model_session_wins() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "ws-model", true).await;
        seed_ws(&db, "w1", Some("model:m1")).await;
        db.agent_create_session("s1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();

        let name = resolve_effective_model(&db, None, "s1").await.unwrap();
        assert_eq!(name, "gpt-4o", "session 模型应优先于 workspace");
    }

    #[tokio::test]
    async fn test_effective_model_workspace_fallback() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "deepseek-chat", true).await;
        seed_ws(&db, "w1", Some("model:m1")).await;
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let name = resolve_effective_model(&db, None, "s1").await.unwrap();
        assert_eq!(name, "deepseek-chat");
    }

    #[tokio::test]
    async fn test_effective_model_global_default_fallback() {
        let db = Database::new(":memory:").await.unwrap();
        seed_ws(&db, "w1", None).await;
        db.save_server_setting(DEFAULT_MODEL_KEY, "claude-opus-5")
            .await
            .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let name = resolve_effective_model(&db, None, "s1").await.unwrap();
        assert_eq!(name, "claude-opus-5");
    }

    #[tokio::test]
    async fn test_effective_model_first_available() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "deepseek-chat", true).await;
        seed_ws(&db, "w1", None).await;
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let state = crate::llm::LlmState::new(Some(db.clone()), None);
        let name = resolve_effective_model(&db, Some(&state), "s1")
            .await
            .unwrap();
        assert_eq!(name, "deepseek-chat", "无显式配置 → 第一个可用模型");
    }

    #[tokio::test]
    async fn test_effective_model_none_configured_errors() {
        let db = Database::new(":memory:").await.unwrap();
        seed_ws(&db, "w1", None).await;
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let err = resolve_effective_model(&db, None, "s1").await.unwrap_err();
        assert!(err.contains("no LLM model configured"), "err: {err}");
    }

    #[tokio::test]
    async fn test_effective_model_session_not_found() {
        let db = Database::new(":memory:").await.unwrap();
        let err = resolve_effective_model(&db, None, "ghost")
            .await
            .unwrap_err();
        assert!(err.contains("session not found"), "err: {err}");
    }

    // ── has_any_model_config ────────────────────────────────────

    #[tokio::test]
    async fn test_has_model_config_session_model() {
        let db = Database::new(":memory:").await.unwrap();
        seed_ws(&db, "w1", None).await;
        db.agent_create_session("s1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
        assert!(has_any_model_config(&db, "s1", None).await.unwrap());
    }

    #[tokio::test]
    async fn test_has_model_config_workspace_ref() {
        let db = Database::new(":memory:").await.unwrap();
        seed_ws(&db, "w1", Some("model:m1")).await;
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        assert!(has_any_model_config(&db, "s1", Some("model:m1"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_has_model_config_none() {
        let db = Database::new(":memory:").await.unwrap();
        seed_ws(&db, "w1", None).await;
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        assert!(!has_any_model_config(&db, "s1", None).await.unwrap());
    }

    // ── load 的 workspace 层 ────────────────────────────────────

    #[tokio::test]
    async fn test_load_uses_workspace_model_when_session_unset() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "deepseek-chat", true).await;
        seed_ws(&db, "w1", Some("model:m1")).await;
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "default-model")
            .await
            .unwrap();
        assert_eq!(rt.model, "deepseek-chat");
    }

    #[tokio::test]
    async fn test_load_session_model_beats_workspace() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "deepseek-chat", true).await;
        seed_ws(&db, "w1", Some("model:m1")).await;
        db.agent_create_session("s1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "default-model")
            .await
            .unwrap();
        assert_eq!(rt.model, "gpt-4o");
    }

    #[tokio::test]
    async fn test_load_workspace_model_disabled_errors() {
        let db = Database::new(":memory:").await.unwrap();
        save_named_model(&db, "m1", "deepseek-chat", false).await;
        seed_ws(&db, "w1", Some("model:m1")).await;
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let err = match SessionRuntime::load(&db, "s1", "default-model").await {
            Err(e) => e,
            Ok(rt) => panic!("expected error, got model={}", rt.model),
        };
        assert!(err.contains("disabled"), "err: {err}");
    }

    #[tokio::test]
    async fn test_load_session_rebuilds_history() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
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
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
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
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
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
            "",
            None,
            None,
            None,
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
        db.agent_create_workspace(
            "w2",
            "p",
            "nas",
            "docker",
            "/x",
            Some("node:20"),
            None,
            "",
            None,
            None,
            None,
        )
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
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
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
            None,
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
            None,
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
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
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
            None,
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
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
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
            None,
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
            None,
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
    async fn test_load_tool_result_structured_json_extracts_text() {
        // M2 契约：ACP 路径落库的 tool_result content 是 JSON
        // `{"text","status","diffs"?,...}`——load 重放给 LLM 只取 text 字段
        // （否则把 JSON 壳当工具结果内容）；旧纯文本行原样使用（向后兼容）。
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "改代码", None)
            .await
            .unwrap();
        // 新格式行 + 旧纯文本行各一
        db.agent_add_message_v2(
            "m2",
            "s1",
            "assistant",
            "",
            Some(r#"[{"id":"c1","type":"function","function":{"name":"shell","arguments":"{}"}}]"#),
            None,
            None,
            "tool_calls",
            None,
        )
        .await
        .unwrap();
        db.agent_add_message_v2(
            "m3",
            "s1",
            "tool",
            r#"{"text":"a.rs","status":"failed","diffs":[{"old":"x","new":"y"}]}"#,
            None,
            Some("c1"),
            Some("shell"),
            "tool_result",
            None,
        )
        .await
        .unwrap();
        db.agent_add_message_v2(
            "m4",
            "s1",
            "assistant",
            "",
            Some(r#"[{"id":"c2","type":"function","function":{"name":"read_file","arguments":"{}"}}]"#),
            None,
            None,
            "tool_calls",
            None,
        )
        .await
        .unwrap();
        db.agent_add_message_v2(
            "m5",
            "s1",
            "tool",
            "旧格式纯文本",
            None,
            Some("c2"),
            Some("read_file"),
            "tool_result",
            None,
        )
        .await
        .unwrap();

        let rt = SessionRuntime::load(&db, "s1", "m").await.unwrap();
        // system + user + assistant + tool(c1) + assistant + tool(c2)
        assert_eq!(rt.messages.len(), 6);
        let c1 = &rt.messages[3];
        assert_eq!(c1.role, "tool");
        assert_eq!(
            c1.content.as_deref(),
            Some("a.rs"),
            "新 JSON 格式应只提取 text: {:?}",
            c1.content
        );
        assert_eq!(c1.tool_call_id.as_deref(), Some("c1"));
        let c2 = &rt.messages[5];
        assert_eq!(c2.content.as_deref(), Some("旧格式纯文本"), "旧行原样");
    }

    #[tokio::test]
    async fn test_load_drops_orphan_tool_results() {
        // 压缩切割点落在 assistant tool_calls 行正后方：tool 结果保留但配对行被压掉。
        // 清洗后孤儿 tool 结果应被丢弃。
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
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
            None,
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
            None,
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
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "改代码", None)
            .await
            .unwrap();
        db.agent_add_message_v2("m2", "s1", "tool", "", None, None, None, "message", None)
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
