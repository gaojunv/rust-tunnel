//! 危险操作审批规则：按 workspace approval_mode 判定工具调用是否需用户确认。
use crate::common::AgentCommand;

/// shell 危险模式（大小写不敏感子串匹配）。仅 auto_write 档用于判定 shell；
/// safe 档下所有写操作都需确认，不经此表。
const DANGEROUS_SHELL_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rm -r /",
    "git push --force",
    "git push -f",
    "git reset --hard",
    "mkfs",
    "dd if=",
    "shutdown",
    "reboot",
    "kill -9",
    "> /dev/",
    ":(){ :|:& };:",
];

/// 该 shell 命令是否命中危险模式。
pub fn is_dangerous_shell(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    DANGEROUS_SHELL_PATTERNS.iter().any(|p| lower.contains(p))
}

/// 按审批模式判定工具调用是否需用户确认。非法 mode 按 "safe" 处理（保守）。
pub fn needs_approval(mode: &str, cmd: &AgentCommand) -> bool {
    let mode = match mode {
        "safe" | "auto_write" | "full_auto" => mode,
        _ => "safe",
    };
    if mode == "full_auto" {
        return false;
    }
    match cmd {
        AgentCommand::ReadFile { .. }
        | AgentCommand::ListDir { .. }
        | AgentCommand::Search { .. }
        | AgentCommand::GitStatus
        | AgentCommand::GitDiff { .. } => false,
        AgentCommand::Shell { cmd, .. } => {
            mode == "safe" || is_dangerous_shell(cmd)
        }
        AgentCommand::WriteFile { .. } | AgentCommand::PatchFile { .. } | AgentCommand::GitCommit { .. } => {
            mode == "safe"
        }
        AgentCommand::GitPush => true, // safe 与 auto_write 都需确认
    }
}

/// 审批卡片摘要（一行）：shell→cmd（截断）、文件类→path、git_commit→message、git_push→固定文案。
pub fn approval_summary(cmd: &AgentCommand) -> String {
    const MAX: usize = 120;
    let truncate = |s: &str| {
        if s.chars().count() > MAX {
            format!("{}…", s.chars().take(MAX).collect::<String>())
        } else {
            s.to_string()
        }
    };
    match cmd {
        AgentCommand::Shell { cmd, .. } => truncate(cmd),
        AgentCommand::WriteFile { path, .. } | AgentCommand::PatchFile { path, .. } => path.clone(),
        AgentCommand::GitCommit { message } => truncate(message),
        AgentCommand::GitPush => "git push".to_string(),
        _ => String::new(), // 只读工具不会进入审批路径
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(cmd: &str) -> AgentCommand {
        AgentCommand::Shell { cmd: cmd.into(), cwd: None }
    }

    #[test]
    fn test_full_auto_never_approves() {
        assert!(!needs_approval("full_auto", &shell("rm -rf /")));
        assert!(!needs_approval("full_auto", &AgentCommand::GitPush));
        assert!(!needs_approval("full_auto", &AgentCommand::WriteFile { path: "a".into(), content: "x".into() }));
    }

    #[test]
    fn test_safe_mode_readonly_free() {
        assert!(!needs_approval("safe", &AgentCommand::ReadFile { path: "a".into() }));
        assert!(!needs_approval("safe", &AgentCommand::ListDir { path: ".".into() }));
        assert!(!needs_approval("safe", &AgentCommand::Search { pattern: "x".into(), path: ".".into(), include: None }));
        assert!(!needs_approval("safe", &AgentCommand::GitStatus));
        assert!(!needs_approval("safe", &AgentCommand::GitDiff { path: None }));
    }

    #[test]
    fn test_safe_mode_writes_and_shell_need_approval() {
        assert!(needs_approval("safe", &shell("ls")));
        assert!(needs_approval("safe", &AgentCommand::WriteFile { path: "a".into(), content: "x".into() }));
        assert!(needs_approval("safe", &AgentCommand::PatchFile { path: "a".into(), old_string: "o".into(), new_string: "n".into() }));
        assert!(needs_approval("safe", &AgentCommand::GitCommit { message: "m".into() }));
        assert!(needs_approval("safe", &AgentCommand::GitPush));
    }

    #[test]
    fn test_auto_write_mode() {
        // 写操作自动放行
        assert!(!needs_approval("auto_write", &AgentCommand::WriteFile { path: "a".into(), content: "x".into() }));
        assert!(!needs_approval("auto_write", &AgentCommand::GitCommit { message: "m".into() }));
        // 普通 shell 自动放行
        assert!(!needs_approval("auto_write", &shell("npm test")));
        // 危险 shell 需确认
        assert!(needs_approval("auto_write", &shell("rm -rf node_modules")));
        assert!(needs_approval("auto_write", &shell("git push -f origin main")));
        // git_push 始终确认
        assert!(needs_approval("auto_write", &AgentCommand::GitPush));
    }

    #[test]
    fn test_invalid_mode_falls_back_to_safe() {
        assert!(needs_approval("garbage", &shell("ls")));
        assert!(needs_approval("", &AgentCommand::GitPush));
    }

    #[test]
    fn test_dangerous_shell_patterns() {
        assert!(is_dangerous_shell("sudo rm -rf /var"));
        assert!(is_dangerous_shell("RM -RF build")); // 大小写不敏感
        assert!(is_dangerous_shell("dd if=/dev/zero of=/dev/sda"));
        assert!(is_dangerous_shell("kill -9 1234"));
        assert!(!is_dangerous_shell("rm file.txt"));
        assert!(!is_dangerous_shell("git push origin main"));
        assert!(!is_dangerous_shell("npm run rebuild")); // 含 "rebuild" 不含 "reboot"
    }

    #[test]
    fn test_approval_summary() {
        assert_eq!(approval_summary(&shell("npm install")), "npm install");
        assert_eq!(
            approval_summary(&AgentCommand::WriteFile { path: "src/a.rs".into(), content: "x".into() }),
            "src/a.rs"
        );
        assert_eq!(approval_summary(&AgentCommand::GitPush), "git push");
        let long = shell(&"x".repeat(200));
        assert!(approval_summary(&long).chars().count() <= 121);
    }
}
