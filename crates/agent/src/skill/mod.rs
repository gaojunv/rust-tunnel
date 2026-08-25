//! Skill 库核心：清单注入 + use_skill 工具短路 + 蒸馏/手动去重 upsert。
//!
//! Skill **不向量化**（数量少、清单注入无需语义检索、按 name+scope 文本去重），
//! 纯 SQLite + SQL，embedding 未配置也能工作（仅需 LLM 做蒸馏）。仅 `rag` feature
//! 编译（在 `agent/mod.rs` 挂载处用 `#[cfg(feature = "rag")]` 门控）。
//!
//! 注入策略：**清单 + 按需加载**——会话开始注入 name+description 清单（`<skills>`
//! 块），模型调 `use_skill` 工具拉全文。防回环：蒸馏渲染会剥离 `<skills>` 块，
//! 已注入的清单不会被当成对话内容再次蒸馏。

use crate::db::skills::AgentSkillSummary;
use crate::memory::scope_coords;
use crate::{session::SessionRuntime, AgentState};

/// Skill content（Markdown 全文）上限（字符）。
pub const SKILL_CONTENT_MAX_CHARS: usize = 16 * 1024;
/// Skill name 上限（字符）。
pub const SKILL_NAME_MAX_CHARS: usize = 64;
/// Skill description（触发边界+概述，清单注入用）上限（字符）。
pub const SKILL_DESCRIPTION_MAX_CHARS: usize = 200;
/// 清单块硬上限（字符，≈1K tokens），不暴露 UI。
pub const SKILL_LIST_MAX_CHARS: usize = 4096;
/// 蒸馏时单次提炼 Skill 条数上限（prompt 与解析共用）。
pub(crate) const DISTILL_SKILL_MAX: usize = 3;

/// 归一化 Skill 名：trim + to_lowercase（同作用域去重与 use_skill 匹配都基于
/// 归一化名——大小写不敏感；中文名不受影响）。
#[must_use]
pub fn normalize_skill_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// 同作用域同名去重 upsert（distill 与手动共用）：
/// 命中（name 已 normalize 后精确匹配 scope 三元组）→ 更新（content/description
/// 以新为准、tags 并集、enabled/use_count 保持）；否则新建（id=uuid，enabled=1）。
///
/// 返回 Skill id。DB 写失败返回 Err（distill 静默跳过单条，手动创建把错误给 UI）。
#[allow(clippy::too_many_arguments)]
pub async fn upsert_skill_with_dedup(
    memory: &crate::memory::MemoryState,
    name: &str,
    description: &str,
    content: &str,
    scope_type: &str,
    client_id: &str,
    workspace_id: &str,
    tags: &[String],
    source_session_id: &str,
    source_trigger: &str,
) -> Result<String, String> {
    let normalized = normalize_skill_name(name);
    if normalized.is_empty() {
        return Err("skill name must not be empty".into());
    }
    let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".into());
    let existing = memory
        .db
        .skill_get_by_name_scope(&normalized, scope_type, client_id, workspace_id)
        .await
        .map_err(|e| format!("skill lookup failed: {e}"))?;
    if let Some(existing) = existing {
        let merged = crate::memory::merge_tags(&crate::memory::parse_tags(&existing.tags), tags);
        let merged_json = serde_json::to_string(&merged).unwrap_or_else(|_| "[]".into());
        memory
            .db
            .skill_update(
                &existing.id,
                &normalized,
                description,
                content,
                &merged_json,
                scope_type,
                client_id,
                workspace_id,
            )
            .await
            .map_err(|e| format!("skill update failed: {e}"))?;
        return Ok(existing.id);
    }
    let id = format!("{:032x}", rand::random::<u128>());
    memory
        .db
        .skill_insert(
            &id,
            &normalized,
            description,
            content,
            scope_type,
            client_id,
            workspace_id,
            &tags_json,
            source_session_id,
            source_trigger,
        )
        .await
        .map_err(|e| format!("skill insert failed: {e}"))?;
    Ok(id)
}

/// 清单注入：skill_enabled 关闭 / 无可见条目 → None。纯 SQL + 字符串拼接，
/// **零 embedding 依赖**（即便记忆总闸关闭、embedding 未配置也能注入）。
pub async fn retrieve_skill_list_for_session(
    memory: &crate::memory::MemoryState,
    client_id: &str,
    workspace_id: &str,
) -> Option<String> {
    let s = memory.settings().await;
    if s.skill_enabled == 0 {
        return None;
    }
    let max = s.skill_list_max.clamp(1, 50) as usize;
    let rows = memory
        .db
        .skill_injectable(client_id, workspace_id, max as i64)
        .await
        .unwrap_or_default();
    if rows.is_empty() {
        return None;
    }
    build_skill_list_block(&rows, max, SKILL_LIST_MAX_CHARS)
}

/// 组装 `<skills>...</skills>` 清单块：按 use_count DESC 排序，预算（条数上限 +
/// 字符上限）内只保留完整行，**绝不半截**某条 skill。无条目 → None。
#[must_use]
pub fn build_skill_list_block(
    items: &[AgentSkillSummary],
    max_items: usize,
    max_chars: usize,
) -> Option<String> {
    let mut sorted: Vec<&AgentSkillSummary> = items.iter().collect();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.use_count));
    let mut s = String::from("<skills>\n以下是本工作区可复用的技能清单（name + 触发边界描述）。需要时调用 use_skill 工具传入 name 拉取全文：\n");
    let mut added = 0usize;
    for skill in sorted {
        if added >= max_items {
            break;
        }
        let item = format!(
            "- {name}: {description}\n",
            name = skill.name,
            description = skill.description
        );
        if s.len() + item.len() > max_chars && added > 0 {
            break;
        }
        s.push_str(&item);
        added += 1;
    }
    if added == 0 {
        return None;
    }
    s.push_str("</skills>");
    Some(s)
}

/// use_skill 工具短路：`{name}` → 作用域可见（workspace > client > global 优先级）、
/// 大小写不敏感匹配 + enabled 的 Skill 全文，命中 bump_use。未匹配 / 禁用 / 未开启
/// skill 库均返回 Err（错误文本由调用方喂回模型）；不进 AgentCommand 协议、不落审批。
pub async fn use_skill_from_agent(
    agent: &AgentState,
    rt: &SessionRuntime,
    args_json: &str,
) -> Result<String, String> {
    let args: serde_json::Value =
        serde_json::from_str(args_json).map_err(|e| format!("invalid arguments: {e}"))?;
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map_or("", str::trim);
    if name.is_empty() {
        return Err("use_skill requires a non-empty 'name' string".into());
    }
    if name.len() > SKILL_NAME_MAX_CHARS {
        return Err(format!(
            "skill name too long (>{SKILL_NAME_MAX_CHARS} chars)"
        ));
    }
    let Some(memory) = &agent.memory else {
        return Err("AI skill library is not enabled".into());
    };
    let s = memory.settings().await;
    if s.skill_enabled == 0 {
        return Err("skill library is disabled".into());
    }
    let normalized = normalize_skill_name(name);
    // 作用域优先级：workspace → client → global。
    for scope in ["workspace", "client", "global"] {
        let (scope_type, client_id, workspace_id) =
            scope_coords(scope, &rt.client_id, &rt.workspace_id);
        let row = memory
            .db
            .skill_get_by_name_scope(&normalized, &scope_type, &client_id, &workspace_id)
            .await
            .map_err(|e| format!("skill lookup failed: {e}"))?;
        if let Some(row) = row {
            if row.enabled == 0 {
                return Err(format!("skill '{name}' is disabled"));
            }
            let _ = memory.db.skill_bump_use(&row.id).await;
            return Ok(row.content);
        }
    }
    Err(format!("skill not found: {name}"))
}

#[cfg(all(test, feature = "rag"))]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::llm::LlmState;
    use crate::memory::MemoryState;

    fn summary(id: &str, name: &str, description: &str, use_count: i64) -> AgentSkillSummary {
        AgentSkillSummary {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            scope_type: "workspace".into(),
            client_id: "c1".into(),
            workspace_id: "w1".into(),
            tags: "[]".into(),
            enabled: 1,
            source_session_id: String::new(),
            source_trigger: String::new(),
            use_count,
            last_used_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    async fn memory_state() -> (Database, MemoryState) {
        let db = Database::new(":memory:").await.unwrap();
        let (_dir, store) = crate::memory::test_store();
        let llm = LlmState::new(None, None);
        let memory = MemoryState::new(db.clone(), store, None, llm);
        (db, memory)
    }

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
            wiki_list_block: None,
            roles_block: None,
            messages: Vec::new(),
            todos: Vec::new(),
            depth: 0,
            parent_tool_call_id: None,
            file_hashes: std::collections::HashMap::new(),
            active_role: None,
        }
    }

    #[test]
    fn normalize_trims_and_lowercases() {
        assert_eq!(normalize_skill_name("  Deploy-App "), "deploy-app");
        assert_eq!(normalize_skill_name("中文技能"), "中文技能");
        assert_eq!(normalize_skill_name("  "), "");
    }

    #[test]
    fn build_block_sorts_by_uses_and_respects_budget() {
        let items = vec![
            summary("a", "alpha", "描述 a", 1),
            summary("b", "beta", "描述 b", 5),
            summary("c", "gamma", "描述 c", 3),
        ];
        let block = build_skill_list_block(&items, 10, SKILL_LIST_MAX_CHARS).unwrap();
        assert!(block.starts_with("<skills>"));
        assert!(block.ends_with("</skills>"));
        // use_count DESC：beta(5) 在 alpha(1) 之前
        assert!(block.find("beta").unwrap() < block.find("alpha").unwrap());
        assert!(block.contains("gamma"));

        // max_items=1：只保留 use_count 最高的一条
        let one = build_skill_list_block(&items, 1, SKILL_LIST_MAX_CHARS).unwrap();
        assert!(one.contains("beta"));
        assert!(!one.contains("alpha"));

        // 极小字符预算：至少保留首条（不返回空）
        let tiny = build_skill_list_block(&items, 10, 5).unwrap();
        assert!(tiny.contains("beta"));
        // 预算超限时完整行原则：不半截任何一行
        assert!(tiny
            .lines()
            .all(|l| !l.ends_with(':') || l.starts_with('-')));

        // 空列表 → None
        assert!(build_skill_list_block(&[], 10, SKILL_LIST_MAX_CHARS).is_none());
    }

    #[tokio::test]
    async fn upsert_dedup_updates_same_scope_keeps_use_count() {
        let (db, memory) = memory_state().await;
        let id1 = upsert_skill_with_dedup(
            &memory,
            "Deploy-App",
            "描述一",
            "内容一",
            "workspace",
            "c1",
            "w1",
            &["deploy".into()],
            "s1",
            "distill",
        )
        .await
        .unwrap();
        db.skill_bump_use(&id1).await.unwrap();

        // 同名（归一化）同作用域 → 更新既有：content/description 以新为准、tags 并集、
        // use_count 保持
        let id2 = upsert_skill_with_dedup(
            &memory,
            "deploy-app",
            "描述二",
            "内容二",
            "workspace",
            "c1",
            "w1",
            &["release".into()],
            "s1",
            "distill",
        )
        .await
        .unwrap();
        assert_eq!(id1, id2, "同作用域同名应更新而非新建");
        let row = db.skill_get_by_id(&id1).await.unwrap().unwrap();
        assert_eq!(row.name, "deploy-app", "name 归一化落库");
        assert_eq!(row.description, "描述二");
        assert_eq!(row.content, "内容二");
        assert_eq!(row.use_count, 1, "use_count 保持");
        let tags = crate::memory::parse_tags(&row.tags);
        assert!(
            tags.contains(&"deploy".into()) && tags.contains(&"release".into()),
            "tags 并集"
        );

        // 异作用域同名 → 新建
        let id3 = upsert_skill_with_dedup(
            &memory,
            "deploy-app",
            "全局",
            "全局内容",
            "global",
            "",
            "",
            &[],
            "s1",
            "distill",
        )
        .await
        .unwrap();
        assert_ne!(id1, id3);
        let all = db
            .skill_list(None, None, None, None, None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn retrieve_list_gated_and_scope_visible() {
        let (db, memory) = memory_state().await;
        // skill_enabled 关闭 → None
        assert!(retrieve_skill_list_for_session(&memory, "c1", "w1")
            .await
            .is_none());

        // 开启
        let mut s = db.memory_get_settings().await.unwrap();
        s.skill_enabled = 1;
        db.memory_upsert_settings(&s).await.unwrap();

        db.skill_insert(
            "g1",
            "global-skill",
            "全局技能",
            "内容",
            "global",
            "",
            "",
            "[]",
            "",
            "manual",
        )
        .await
        .unwrap();
        db.skill_insert(
            "w1a",
            "release-check",
            "发布前检查",
            "内容",
            "workspace",
            "c1",
            "w1",
            "[]",
            "",
            "manual",
        )
        .await
        .unwrap();
        db.skill_insert(
            "w2",
            "other-ws",
            "别的工作区",
            "内容",
            "workspace",
            "c1",
            "w2",
            "[]",
            "",
            "manual",
        )
        .await
        .unwrap();
        db.skill_bump_use("g1").await.unwrap();

        let block = retrieve_skill_list_for_session(&memory, "c1", "w1")
            .await
            .unwrap();
        assert!(block.starts_with("<skills>"));
        assert!(block.contains("global-skill"));
        assert!(block.contains("release-check"));
        assert!(!block.contains("other-ws"), "其他 workspace 不可见");
        assert!(block.contains("全局技能"), "清单含 description");

        // 全部停用 → None
        db.skill_toggle_enabled("g1").await.unwrap();
        db.skill_toggle_enabled("w1a").await.unwrap();
        assert!(retrieve_skill_list_for_session(&memory, "c1", "w1")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn use_skill_case_insensitive_priority_and_bump() {
        let (db, memory) = memory_state().await;
        let mut s = db.memory_get_settings().await.unwrap();
        s.skill_enabled = 1;
        db.memory_upsert_settings(&s).await.unwrap();

        // 三个作用域同名（优先 workspace）
        db.skill_insert(
            "ws1",
            "deploy-app",
            "工作区",
            "工作区内容",
            "workspace",
            "c1",
            "w1",
            "[]",
            "",
            "manual",
        )
        .await
        .unwrap();
        db.skill_insert(
            "cl1",
            "deploy-app",
            "客户端",
            "客户端内容",
            "client",
            "c1",
            "",
            "[]",
            "",
            "manual",
        )
        .await
        .unwrap();
        db.skill_insert(
            "g1",
            "deploy-app",
            "全局",
            "全局内容",
            "global",
            "",
            "",
            "[]",
            "",
            "manual",
        )
        .await
        .unwrap();

        let agent = crate::AgentState::new(
            std::sync::Arc::new(crate::test_helpers::TestRegistry::new(&db)),
            db.clone(),
        )
        .with_memory(memory);

        // 大小写不敏感 + workspace 优先级
        let r = rt("s1", "c1", "w1");
        let out = use_skill_from_agent(&agent, &r, r#"{"name": "Deploy-App"}"#)
            .await
            .unwrap();
        assert_eq!(out, "工作区内容");
        // workspace 命中 bump
        assert_eq!(
            db.skill_get_by_id("ws1").await.unwrap().unwrap().use_count,
            1
        );
        assert_eq!(
            db.skill_get_by_id("cl1").await.unwrap().unwrap().use_count,
            0
        );

        // client 作用域会话：client 命中
        let r = rt("s1", "c1", "w2");
        let out = use_skill_from_agent(&agent, &r, r#"{"name": "deploy-app"}"#)
            .await
            .unwrap();
        assert_eq!(out, "客户端内容");
        assert_eq!(
            db.skill_get_by_id("cl1").await.unwrap().unwrap().use_count,
            1
        );

        // global 作用域会话：global 命中
        let r = rt("s1", "other-client", "w9");
        let out = use_skill_from_agent(&agent, &r, r#"{"name": "deploy-app"}"#)
            .await
            .unwrap();
        assert_eq!(out, "全局内容");

        // 未匹配 → Err
        let err = use_skill_from_agent(&agent, &r, r#"{"name": "ghost"}"#)
            .await
            .unwrap_err();
        assert!(err.contains("not found"), "err: {err}");

        // 禁用 → Err
        db.skill_toggle_enabled("ws1").await.unwrap();
        let r = rt("s1", "c1", "w1");
        let err = use_skill_from_agent(&agent, &r, r#"{"name": "deploy-app"}"#)
            .await
            .unwrap_err();
        assert!(err.contains("disabled"), "err: {err}");

        // 参数校验
        assert!(use_skill_from_agent(&agent, &r, r"{}").await.is_err());
        assert!(use_skill_from_agent(&agent, &r, r#"{"name": "  "}"#)
            .await
            .is_err());
        let big = "x".repeat(SKILL_NAME_MAX_CHARS + 1);
        let args = serde_json::json!({"name": big}).to_string();
        assert!(use_skill_from_agent(&agent, &r, &args)
            .await
            .unwrap_err()
            .contains("too long"));
    }

    #[tokio::test]
    async fn use_skill_disabled_library_errors() {
        let (db, memory) = memory_state().await;
        let agent = crate::AgentState::new(
            std::sync::Arc::new(crate::test_helpers::TestRegistry::new(&db)),
            db,
        )
        .with_memory(memory);
        let r = rt("s1", "c1", "w1");
        // skill_enabled 默认 0 → Err
        let err = use_skill_from_agent(&agent, &r, r#"{"name": "x"}"#)
            .await
            .unwrap_err();
        assert!(err.contains("disabled"), "err: {err}");
    }
}
