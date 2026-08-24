//! 角色解析工具：task 参数解析、@role 前缀解析、子 agent 提示词构建、
//! 可用角色清单注入。
//!
//! 本模块为纯函数 + 单测，不依赖 async/DB——DB 查询结果作为
//! `&[AgentRoleRecord]` 传入，由 `runner.rs` / `ws.rs` 负责异步加载。

use crate::db::roles::AgentRoleRecord;

/// 子 agent 独立系统提示词（runner.rs 定义的默认值常量引用）。
pub const SUBAGENT_DEFAULT_SYSTEM_PROMPT: &str =
    "You are a sub-agent delegated a specific task. Work autonomously using tools, \
     then output a concise final summary of findings/actions. All paths are relative \
     to the workspace root.";

/// 子 agent 系统提示词后缀（委托说明段；追加到角色 system_prompt 之后）。
pub const SUBAGENT_SYSTEM_PROMPT_SUFFIX: &str =
    "\n\n---\n\n## Delegation\nUse the `task` tool to delegate exploration/research \
     subtasks (code searches, multi-file reading, investigations) to a sub-agent with \
     isolated context. It returns only a summary, keeping the main context clean. \
     Prefer task for open-ended questions that would require many tool calls.";

/// 解析 DB 中的 JSON 数组字符串（`tools_allow` / `tools_deny`）为工具名列表。
/// NULL/空字符串/非合法 JSON → None；空数组 `[]` → Some(空 vec)。
pub fn parse_tools_list(json: Option<&str>) -> Option<Vec<String>> {
    let raw = json?.trim();
    if raw.is_empty() || raw == "null" {
        return None;
    }
    serde_json::from_str::<Vec<String>>(raw).ok().filter(|v| {
        // 全空元素视为无有效项
        v.iter().all(|s| !s.trim().is_empty())
    })
}

/// 解析用户消息中的 `@<role-name>` 前缀。
///
/// - `content` 以 `@<name>` 开头（后随空白、换行或结尾）。
/// - `name` 匹配 `visible_roles` 中的某个角色（按 `name` 字段，大小写不敏感）。
/// - 命中 → 返回 `Some((role_name, stripped_content))`（前缀 + 后续空白已剥离）。
/// - 未命中（`@src/main.rs` 等文件路径） → `None`（调用方走原 @文件引用逻辑）。
pub fn parse_at_role_prefix<'a>(
    content: &'a str,
    visible_roles: &[AgentRoleRecord],
) -> Option<(String, &'a str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with('@') {
        return None;
    }
    let after_at = &trimmed[1..];
    // 角色名终止符：空白、换行、字符串结尾；不允许紧跟 `/` 或 `.`（文件路径特征）
    let name_end = after_at
        .char_indices()
        .find(|&(_, c)| c.is_whitespace() || c == '\n' || c == '\r')
        .map(|(i, _)| i)
        .or({
            if after_at.is_empty() {
                None
            } else {
                Some(after_at.len())
            }
        })?;
    let candidate = &after_at[..name_end];
    if candidate.is_empty() {
        return None;
    }
    // 文件路径特征：含 `/` 或 `.` → 不匹配角色（@src/main.rs 不是 @role）
    if candidate.contains('/') || candidate.contains('.') {
        return None;
    }
    // 在可见角色中查找（大小写不敏感）
    let candidate_lower = candidate.to_ascii_lowercase();
    let matched = visible_roles.iter().find(|r| {
        r.name.to_ascii_lowercase() == candidate_lower && (r.mode == "primary" || r.mode == "all")
    })?;
    let stripped =
        content[content.len() - content.trim_start().len() + 1 + name_end..].trim_start();
    Some((matched.name.clone(), stripped))
}

/// 构建子 agent 系统提示词：角色有自定义 `system_prompt` 则用之，
/// 追加委托说明后缀；无自定义则回退默认值。
pub fn subagent_system_prompt(role: Option<&AgentRoleRecord>) -> String {
    match role {
        Some(r) if !r.system_prompt.trim().is_empty() => {
            format!("{}{SUBAGENT_SYSTEM_PROMPT_SUFFIX}", r.system_prompt.trim())
        }
        _ => {
            format!("{SUBAGENT_DEFAULT_SYSTEM_PROMPT}{SUBAGENT_SYSTEM_PROMPT_SUFFIX}")
        }
    }
}

/// 生成注入 system prompt 的"可用子代理角色清单"块，供 task 工具
/// description 动态拼接。内容为角色名 + 描述的列表。
pub fn task_schema_roles_block(roles: &[AgentRoleRecord]) -> String {
    if roles.is_empty() {
        return String::new();
    }
    let mut out = String::from("### Available Sub-Agent Roles\n");
    for r in roles {
        out.push_str(&format!(
            "- **{}**: {}\n",
            r.name,
            if r.description.is_empty() {
                "(no description)"
            } else {
                &r.description
            }
        ));
    }
    out
}

// ── 角色字段校验与视图（API handler 薄壳化的 service 层）──────────────
//
// 以下函数原位于 server 装配层 mgmt/api/agent/roles.rs，批次 8d 下沉。
// 全部为纯函数：校验失败返回 Err(面向用户的消息文本)，由 handler 映射为
// HTTP 400；不依赖 async/DB。

/// 合法 scope_type 值。
pub const VALID_SCOPES: [&str; 3] = ["global", "client", "workspace"];
/// 合法 mode 值。
pub const VALID_MODES: [&str; 3] = ["subagent", "primary", "all"];

/// 合法工具名集合（与本 crate `tools` 实际注册的工具名为准）。
/// 注意：`read_file_range` 不是独立工具（是 read_file 的行区间参数变体），不在此列。
pub const VALID_TOOL_NAMES: &[&str] = &[
    "shell",
    "read_file",
    "write_file",
    "patch_file",
    "edit_file",
    "list_dir",
    "search",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
    "git_branch",
    "git_commit",
    "git_push",
    "git_stage",
    "git_unstage",
    "git_checkout",
    "git_pull",
    "git_revert",
    "git_reset",
    "git_stash",
    "code_outline",
    "read_symbol",
    "task",
    "todo_write",
    "remember",
    "use_skill",
];

/// 校验 name 为合法 kebab-case（小写字母/数字/短横线，非空，≤64 字符）。
///
/// # Errors
/// 校验失败时返回面向用户的消息文本。
pub fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name is required".to_string());
    }
    if name.chars().count() > 64 {
        return Err("name must be at most 64 chars".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(
            "name must be kebab-case (lowercase letters, digits, hyphens only)".to_string(),
        );
    }
    // 不得以短横线开头或结尾
    if name.starts_with('-') || name.ends_with('-') {
        return Err("name must not start or end with a hyphen".to_string());
    }
    Ok(())
}

/// 校验 mode 值。
///
/// # Errors
/// 校验失败时返回面向用户的消息文本。
pub fn validate_mode(mode: &str) -> Result<(), String> {
    if !VALID_MODES.contains(&mode) {
        return Err(format!("mode must be one of: {}", VALID_MODES.join(", ")));
    }
    Ok(())
}

/// 校验 scope_type 值。
///
/// # Errors
/// 校验失败时返回面向用户的消息文本。
pub fn validate_scope(scope: &str) -> Result<(), String> {
    if !VALID_SCOPES.contains(&scope) {
        return Err("scope_type must be one of: global, client, workspace".to_string());
    }
    Ok(())
}

/// 校验工具名列表：每个元素必须是合法工具名。
///
/// # Errors
/// 含空名或非法工具名时返回面向用户的消息文本。
pub fn validate_tool_list(tools: &[String], field_name: &str) -> Result<(), String> {
    for tool in tools {
        let t = tool.trim();
        if t.is_empty() {
            return Err(format!("{field_name} contains empty tool name"));
        }
        if !VALID_TOOL_NAMES.contains(&t) {
            return Err(format!(
                "{field_name} contains invalid tool name: '{t}'. Valid names: {}",
                VALID_TOOL_NAMES.join(", ")
            ));
        }
    }
    Ok(())
}

/// 校验创建/更新请求的公共字段。
///
/// # Errors
/// 任一字段校验失败时返回面向用户的消息文本。
pub fn validate_role_fields(
    name: &str,
    description: &str,
    scope: &str,
    mode: &str,
    tools_allow: Option<&[String]>,
    tools_deny: Option<&[String]>,
) -> Result<(), String> {
    validate_name(name)?;
    validate_scope(scope)?;
    validate_mode(mode)?;
    if description.chars().count() > 500 {
        return Err("description must be at most 500 chars".to_string());
    }
    if let Some(tools) = tools_allow {
        validate_tool_list(tools, "tools_allow")?;
    }
    if let Some(tools) = tools_deny {
        validate_tool_list(tools, "tools_deny")?;
    }
    Ok(())
}

/// scope 坐标归一化：global → (global, "", "")。
#[must_use]
pub fn scope_coords(scope: &str, client_id: &str, workspace_id: &str) -> (String, String, String) {
    match scope {
        "global" => ("global".to_string(), String::new(), String::new()),
        "client" => (
            "client".to_string(),
            client_id.to_string(),
            String::new(),
        ),
        "workspace" => (
            "workspace".to_string(),
            client_id.to_string(),
            workspace_id.to_string(),
        ),
        _ => (
            scope.to_string(),
            client_id.to_string(),
            workspace_id.to_string(),
        ),
    }
}

/// 角色 JSON 视图（含全字段；tools_allow/tools_deny 从 JSON 字符串还原为数组）。
#[must_use]
pub fn role_json(r: &AgentRoleRecord) -> serde_json::Value {
    let tools_allow: Option<Vec<String>> =
        r.tools_allow.as_deref().and_then(|s| serde_json::from_str(s).ok());
    let tools_deny: Option<Vec<String>> =
        r.tools_deny.as_deref().and_then(|s| serde_json::from_str(s).ok());
    serde_json::json!({
        "id": r.id,
        "name": r.name,
        "description": r.description,
        "system_prompt": r.system_prompt,
        "tools_allow": tools_allow,
        "tools_deny": tools_deny,
        "model_override": r.model_override,
        "mode": r.mode,
        "scope_type": r.scope_type,
        "client_id": r.client_id,
        "workspace_id": r.workspace_id,
        "is_builtin": r.is_builtin != 0,
        "enabled": r.enabled != 0,
        "created_at": crate::db::agent::normalize_db_datetime(&r.created_at),
        "updated_at": crate::db::agent::normalize_db_datetime(&r.updated_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_role(name: &str, mode: &str) -> AgentRoleRecord {
        AgentRoleRecord {
            id: format!("r-{name}"),
            name: name.to_string(),
            description: format!("{name} description"),
            system_prompt: String::new(),
            tools_allow: None,
            tools_deny: None,
            model_override: None,
            mode: mode.to_string(),
            scope_type: "global".to_string(),
            client_id: String::new(),
            workspace_id: String::new(),
            is_builtin: 0,
            enabled: 1,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    // ── parse_tools_list ────────────────────────────────────

    #[test]
    fn test_parse_tools_list_none() {
        assert!(parse_tools_list(None).is_none());
    }

    #[test]
    fn test_parse_tools_list_empty_string() {
        assert!(parse_tools_list(Some("")).is_none());
        assert!(parse_tools_list(Some("  ")).is_none());
        assert!(parse_tools_list(Some("null")).is_none());
    }

    #[test]
    fn test_parse_tools_list_valid() {
        let result = parse_tools_list(Some(r#"["read_file","search"]"#)).unwrap();
        assert_eq!(result, vec!["read_file", "search"]);
    }

    #[test]
    fn test_parse_tools_list_empty_array() {
        let result = parse_tools_list(Some("[]")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_tools_list_invalid_json() {
        assert!(parse_tools_list(Some("not json")).is_none());
    }

    // ── parse_at_role_prefix ────────────────────────────────

    #[test]
    fn test_at_role_prefix_match() {
        let roles = vec![
            make_role("general", "subagent"),
            make_role("explore", "subagent"),
            make_role("coder", "primary"),
        ];
        let result = parse_at_role_prefix("@coder review this function", &roles);
        let (name, rest) = result.unwrap();
        assert_eq!(name, "coder");
        assert_eq!(rest, "review this function");
    }

    #[test]
    fn test_at_role_prefix_no_match_file_path() {
        let roles = vec![make_role("general", "subagent")];
        // @src/main.rs 含 / 和 . → 不匹配角色
        assert!(parse_at_role_prefix("@src/main.rs read it", &roles).is_none());
    }

    #[test]
    fn test_at_role_prefix_no_match_unknown_role() {
        let roles = vec![make_role("general", "subagent")];
        assert!(parse_at_role_prefix("@unknown do something", &roles).is_none());
    }

    #[test]
    fn test_at_role_prefix_subagent_mode_not_matchable() {
        // subagent-only 角色不能被 @ 选择（mode 不含 primary/all）
        let roles = vec![make_role("general", "subagent")];
        assert!(parse_at_role_prefix("@general hi", &roles).is_none());
    }

    #[test]
    fn test_at_role_prefix_all_mode_matches() {
        let roles = vec![make_role("tester", "all")];
        let result = parse_at_role_prefix("@tester test this", &roles);
        let (name, rest) = result.unwrap();
        assert_eq!(name, "tester");
        assert_eq!(rest, "test this");
    }

    #[test]
    fn test_at_role_prefix_case_insensitive() {
        let roles = vec![make_role("Coder", "primary")];
        let result = parse_at_role_prefix("@coder task", &roles);
        let (name, _) = result.unwrap();
        assert_eq!(name, "Coder");
    }

    #[test]
    fn test_at_role_prefix_no_at() {
        let roles = vec![make_role("general", "primary")];
        assert!(parse_at_role_prefix("hello world", &roles).is_none());
    }

    #[test]
    fn test_at_role_prefix_at_end() {
        let roles = vec![make_role("coder", "primary")];
        let result = parse_at_role_prefix("@coder", &roles);
        let (name, rest) = result.unwrap();
        assert_eq!(name, "coder");
        assert_eq!(rest, "");
    }

    // ── subagent_system_prompt ──────────────────────────────

    #[test]
    fn test_subagent_prompt_with_role_custom() {
        let mut role = make_role("x", "subagent");
        role.system_prompt = "You are a code reviewer.".to_string();
        let prompt = subagent_system_prompt(Some(&role));
        assert!(prompt.starts_with("You are a code reviewer."));
        assert!(prompt.contains("Delegation"));
    }

    #[test]
    fn test_subagent_prompt_with_role_empty_prompt() {
        let role = make_role("x", "subagent"); // empty system_prompt
        let prompt = subagent_system_prompt(Some(&role));
        assert!(prompt.starts_with(SUBAGENT_DEFAULT_SYSTEM_PROMPT));
        assert!(prompt.contains("Delegation"));
    }

    #[test]
    fn test_subagent_prompt_no_role() {
        let prompt = subagent_system_prompt(None);
        assert_eq!(
            prompt,
            format!("{SUBAGENT_DEFAULT_SYSTEM_PROMPT}{SUBAGENT_SYSTEM_PROMPT_SUFFIX}")
        );
    }

    // ── task_schema_roles_block ─────────────────────────────

    #[test]
    fn test_roles_block_empty() {
        assert!(task_schema_roles_block(&[]).is_empty());
    }

    #[test]
    fn test_roles_block_content() {
        let roles = vec![
            make_role("general", "subagent"),
            make_role("explore", "subagent"),
        ];
        let block = task_schema_roles_block(&roles);
        assert!(block.contains("### Available Sub-Agent Roles"));
        assert!(block.contains("- **general**"));
        assert!(block.contains("- **explore**"));
    }
}
