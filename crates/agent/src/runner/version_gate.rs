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

/// 首个支持远程控制（`RemoteCapability` / `RemotePeerList` 等控制通道变体）的
/// 客户端版本。
///
/// # 阶段 2 交付顺序：**这一项刻意延后到第二步**
///
/// 远程控制数据面在 0.11.0 从 JPEG 瓦片换成 VP9 4:4:4（`crates/remote` 的
/// `SESSION_PROTO_VERSION: 1 → 2`），而两端靠 `Hello.proto_version` 的硬相等校验实现
/// 「混合版本干净拒绝」。若**同时**把本常量抬到 `(0, 11, 0)`，0.10.x 客户端会在请求阶段就被
/// 拒，UI 里连远程控制入口都没有——这是「功能消失」而不是「优雅降级」。
///
/// 故按计划「交付顺序」分两步发布，把功能真空窗口消掉：
///
/// 1. **先发 0.11 客户端**（`SESSION_PROTO_VERSION = 2`），服务端暂不动。0.11 ↔ 0.11 正常互通；
///    0.11 ↔ 0.10 在**握手阶段**因版本不等被干净拒绝（老服务端仍会下发 `RemoteCapability`，
///    所以走得到握手，用户看到的是明确的「版本不匹配」而不是断链）。
/// 2. **客户端铺开之后**，部署新服务端并**把本常量抬到 `(0, 11, 0)`**。这一步只把「握手时才
///    发现的版本不匹配」提前成请求阶段的明确提示（「对端客户端版本过旧」）。
///
/// **所以这里的 `(0, 10, 0)` 不是漏改**，是刻意的第一步状态。第 2 步是 `(0, 11, 0)`。
/// 抬它之前请确认：`crates/client/Cargo.toml` 与 `crates/client-gui/Cargo.toml` 的版本已经是
/// 0.11.0（客户端上报的是 `CARGO_PKG_VERSION`，而 `release-client.yml` 的 `VERSION` 只取 git
/// tag、不写回 Cargo.toml——漏了那一步会把**所有**客户端一起挡掉）。
pub const MIN_REMOTE_CLIENT_VERSION: (u64, u64, u64) = (0, 10, 0);

/// 客户端版本是否支持远程控制；缺失/非法视为不支持（保守，避免老客户端收到
/// 未知 bincode 变体后反序列化失败断开控制连接）。
///
/// 本门控是**双向**的：服务端只在支持时才下发远程控制变体，客户端也只在收到
/// `RemoteCapability` 后才开始上行新的变体——因此新客户端配老服务端同样安全。
#[must_use]
pub fn client_supports_remote(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= MIN_REMOTE_CLIENT_VERSION)
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

    #[test]
    fn test_client_supports_remote() {
        assert!(client_supports_remote(Some("0.10.0")));
        assert!(client_supports_remote(Some("v0.10.1")));
        assert!(client_supports_remote(Some("1.0.0")));
        // 上一条门控（0.9.0）的客户端不支持远程控制。
        assert!(!client_supports_remote(Some("0.9.0")));
        assert!(!client_supports_remote(Some("0.9.9")));
        assert!(!client_supports_remote(None));
        assert!(!client_supports_remote(Some("garbage")));
        // 回归：agent 模式版本后缀 +agent 不得破坏版本门控。
        assert!(client_supports_remote(Some("0.10.0+agent")));
        assert!(!client_supports_remote(Some("0.9.0+agent")));
    }

    // ── agent_result_to_text WriteOutcome ───────────────────
}
