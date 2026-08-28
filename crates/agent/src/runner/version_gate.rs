//! 客户端版本能力门控：各特性所需的最低客户端协议版本。

/// 首个支持 Search/PatchFile 命令的客户端版本（随本特性发布 bump）。
pub(crate) const MIN_SEARCH_PATCH_CLIENT_VERSION: (u64, u64, u64) = (0, 2, 0);

/// 解析 "x.y.z"（允许 v 前缀）为数字三元组；非严格 semver 输入返回 None。
/// 客户端在 agent 模式下上报 `{CARGO_PKG_VERSION}+agent`，故解析前须剥离
/// semver 构建元数据（`+`）与预发布（`-`）后缀。
#[must_use]
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    // 顺序：先 strip 'v' 前缀，再切掉 +（构建元数据）/ -（预发布）后缀。
    let s = s.split(['+', '-']).next().unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// 客户端版本是否支持 search/patch；版本缺失/非法视为不支持（保守策略，
/// 避免老客户端收到未知 bincode 变体后反序列化失败断开控制连接）。
pub(crate) fn client_supports_search_patch(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_SEARCH_PATCH_CLIENT_VERSION)
}

/// 首个带回环 PTY 服务（交互式终端）的客户端版本。
pub(crate) const MIN_TERMINAL_CLIENT_VERSION: (u64, u64, u64) = (0, 3, 0);

/// 客户端版本是否支持交互式终端（PTY 服务）；缺失/非法视为不支持。
pub fn client_supports_terminal(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_TERMINAL_CLIENT_VERSION)
}

/// 首个支持 `AgentExecCancel`（真取消）的客户端版本。
pub(crate) const MIN_CANCEL_CLIENT_VERSION: (u64, u64, u64) = (0, 4, 0);

/// 客户端版本是否支持真取消（AgentExecCancel）；缺失/非法视为不支持（保守，
/// 避免老客户端收到未知 bincode 变体断开控制连接）。
pub fn client_supports_cancel(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_CANCEL_CLIENT_VERSION)
}

/// 首个支持 `AgentCommand::GitExec`（通用 git 参数）的客户端版本。
/// 面板 Git 功能与新增 git_* LLM 工具（stage/log/branch/checkout 等）都依赖它。
pub(crate) const MIN_GIT_EXEC_CLIENT_VERSION: (u64, u64, u64) = (0, 5, 0);

/// 客户端版本是否支持通用 git 命令（GitExec）；缺失/非法视为不支持（保守，
/// 避免老客户端收到未知 bincode 变体断开控制连接）。
pub fn client_supports_git_exec(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_GIT_EXEC_CLIENT_VERSION)
}

/// 首个支持 `AgentCommand::ShellWithTimeout`（可配超时 shell）的客户端版本。
pub(crate) const MIN_SHELL_TIMEOUT_CLIENT_VERSION: (u64, u64, u64) = (0, 6, 0);

/// 客户端版本是否支持 ShellWithTimeout；缺失/非法视为不支持（保守，
/// 避免老客户端收到未知 bincode 变体断开控制连接）。
pub fn client_supports_shell_timeout(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_SHELL_TIMEOUT_CLIENT_VERSION)
}

/// 首个支持 `AgentCommand::ReadFileRange`（read_file 行区间）的客户端版本。
pub(crate) const MIN_READ_RANGE_CLIENT_VERSION: (u64, u64, u64) = (0, 7, 0);

/// 客户端版本是否支持 ReadFileRange；缺失/非法视为不支持（保守，避免老客户端
/// 收到未知 bincode 变体断开控制连接）。
pub fn client_supports_read_range(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_READ_RANGE_CLIENT_VERSION)
}

/// 首个支持 `AgentCommand::EditFile` / `WriteFile2` 的客户端版本。
pub(crate) const MIN_EDIT_CLIENT_VERSION: (u64, u64, u64) = (0, 8, 0);

/// 客户端版本是否支持 EditFile / WriteFile2；缺失/非法视为不支持（保守，
/// 避免老客户端收到未知 bincode 变体断开控制连接）。
pub fn client_supports_edit(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_EDIT_CLIENT_VERSION)
}

/// 首个支持 `ClientMappingSummary` 的客户端版本。
pub const MIN_MAPPING_SUMMARY_CLIENT_VERSION: (u64, u64, u64) = (0, 9, 0);

/// 客户端版本是否支持 `ClientMappingSummary`；缺失/非法视为不支持（保守，
/// 避免老客户端收到未知 bincode 变体断开控制连接）。
pub fn client_supports_mapping_summary(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_MAPPING_SUMMARY_CLIENT_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.10.3"), Some((1, 10, 3)));
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0))); // 允许 v 前缀
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("abc"), None);
        // agent 模式客户端上报 `{CARGO_PKG_VERSION}+agent`：构建元数据须剥离
        assert_eq!(parse_version("0.2.0+agent"), Some((0, 2, 0)));
        // 预发布后缀同样剥离（robustness）
        assert_eq!(parse_version("0.2.0-rc.1"), Some((0, 2, 0)));
        assert_eq!(parse_version("v0.2.0+agent"), Some((0, 2, 0)));
    }

    #[test]
    fn test_client_supports_search_patch() {
        assert!(!client_supports_search_patch(Some("0.1.0")));
        assert!(client_supports_search_patch(Some("0.2.0")));
        assert!(client_supports_search_patch(Some("1.0.0")));
        assert!(!client_supports_search_patch(None)); // 缺失视为过旧
        assert!(!client_supports_search_patch(Some("garbage")));
        // 回归：agent 模式版本后缀 +agent 不得破坏版本门控
        assert!(client_supports_search_patch(Some("0.2.0+agent")));
        assert!(!client_supports_search_patch(Some("0.1.0+agent")));
    }

    #[test]
    fn test_client_supports_terminal() {
        assert!(!client_supports_terminal(Some("0.2.0")));
        assert!(client_supports_terminal(Some("0.3.0")));
        assert!(client_supports_terminal(Some("1.0.0")));
        assert!(!client_supports_terminal(None)); // 缺失/离线视为不支持
        assert!(!client_supports_terminal(Some("garbage")));
        // 回归：agent 模式版本后缀 +agent 不得破坏版本门控
        assert!(client_supports_terminal(Some("0.3.0+agent")));
        assert!(!client_supports_terminal(Some("0.2.0+agent")));
    }

    #[test]
    fn test_client_supports_cancel() {
        assert!(client_supports_cancel(Some("0.4.0")));
        assert!(client_supports_cancel(Some("v0.4.1")));
        assert!(!client_supports_cancel(Some("0.3.9")));
        assert!(!client_supports_cancel(Some("0.3.0+agent")));
        assert!(!client_supports_cancel(None));
        assert!(!client_supports_cancel(Some("garbage")));
    }

    #[test]
    fn test_client_supports_git_exec() {
        assert!(client_supports_git_exec(Some("0.5.0")));
        assert!(client_supports_git_exec(Some("v0.5.1")));
        assert!(client_supports_git_exec(Some("1.0.0")));
        assert!(!client_supports_git_exec(Some("0.4.9")));
        assert!(!client_supports_git_exec(Some("0.4.0+agent")));
        assert!(!client_supports_git_exec(None));
        assert!(!client_supports_git_exec(Some("garbage")));
        // 回归：agent 模式版本后缀 +agent 不得破坏版本门控
        assert!(client_supports_git_exec(Some("0.5.0+agent")));
        assert!(!client_supports_git_exec(Some("0.4.0")));
    }

    #[test]
    fn test_client_supports_read_range() {
        assert!(client_supports_read_range(Some("0.7.0")));
        assert!(client_supports_read_range(Some("v0.7.1")));
        assert!(client_supports_read_range(Some("1.0.0")));
        assert!(!client_supports_read_range(Some("0.6.9")));
        assert!(!client_supports_read_range(Some("0.6.0+agent")));
        assert!(!client_supports_read_range(None));
        assert!(!client_supports_read_range(Some("garbage")));
        // 回归：agent 模式版本后缀 +agent 不得破坏版本门控
        assert!(client_supports_read_range(Some("0.7.0+agent")));
        assert!(!client_supports_read_range(Some("0.6.0+agent")));
    }

    #[test]
    fn test_client_supports_edit_version_boundaries() {
        assert!(!client_supports_edit(Some("0.7.9")));
        assert!(client_supports_edit(Some("0.8.0")));
        assert!(client_supports_edit(Some("1.0.0")));
        assert!(!client_supports_edit(None));
        assert!(!client_supports_edit(Some("abc")));
    }

    // ── agent_result_to_text WriteOutcome ───────────────────
}
