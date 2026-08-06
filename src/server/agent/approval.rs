//! 危险操作审批规则：按 workspace approval_mode 判定工具调用是否需用户确认。
use crate::common::AgentCommand;

/// shell 危险模式（大小写不敏感子串匹配）。仅 auto_write 档用于判定 shell；
/// safe 档下所有写操作都需确认，不经此表。
/// 强制 push / dd 写盘 / 重定向进块设备无法用连续子串可靠表达，由
/// [`is_force_push`] / [`is_dd_write`] / [`redirects_to_device`] 单独判定。
const DANGEROUS_SHELL_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rm -r /",
    "git reset --hard",
    "mkfs",
    "shutdown",
    "reboot",
    "kill -9",
    ":(){ :|:& };:",
];

/// git push 强制推送：含独立 token "push"（git 上下文）且出现 force 标志。
fn is_force_push(cmd_lower: &str) -> bool {
    let tokens: Vec<&str> = cmd_lower.split_whitespace().collect();
    let has_git = tokens.iter().any(|t| *t == "git" || t.ends_with("/git"));
    let has_push = tokens.contains(&"push");
    // force 标志归一化：--force/-f/--force-with-lease（含等号形态 prefix），以及
    // 合并短选项（-uf/-fu 等以 "-" 开头、非 "--"、长度>2 且含 'f'）一律视为 force。
    // 保守方向可接受：普通 -u 长度为 2 被排除，git pushup -f 仍被上方 git+push 门槛挡下。
    let force_flag = |t: &str| {
        matches!(t, "--force" | "-f") || t.starts_with("--force-with-lease")
            || (t.starts_with('-') && !t.starts_with("--") && t.len() > 2 && t.contains('f'))
    };
    has_git && has_push && tokens.iter().any(|&t| force_flag(t))
}

/// git push（任何形态）：命令含 git 上下文（独立 token "git" 或以 "/git" 结尾）且含
/// 独立 token "push"。shell 形态的 push 必须与 GitPush 工具同等对待（矩阵：safe 与
/// auto_write 都需确认），否则模型可用 shell 绕过审批。
fn is_git_push(cmd_lower: &str) -> bool {
    let tokens: Vec<&str> = cmd_lower.split_whitespace().collect();
    let has_git = tokens.iter().any(|t| *t == "git" || t.ends_with("/git"));
    let has_push = tokens.contains(&"push");
    has_git && has_push
}

/// dd 写盘：命令含独立 token "dd" 且任一 token 以 "if=" 开头（参数序无关）。
fn is_dd_write(cmd_lower: &str) -> bool {
    let mut tokens = cmd_lower.split_whitespace();
    let has_dd = tokens.any(|t| t == "dd" || t.ends_with("/dd"));
    has_dd && cmd_lower.split_whitespace().any(|t| t.starts_with("if="))
}

/// 重定向进块设备："> /dev/xxx" 但排除良性的 /dev/null。
fn redirects_to_device(cmd_lower: &str) -> bool {
    cmd_lower.contains("> /dev/") && !cmd_lower.contains("> /dev/null")
}

/// 该 shell 命令是否命中危险模式。
pub fn is_dangerous_shell(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    DANGEROUS_SHELL_PATTERNS.iter().any(|p| lower.contains(p))
        || is_force_push(&lower)
        || is_git_push(&lower) // 对 auto_write 而言所有 push 都是"危险"需确认；safe 档本来就全确认
        || is_dd_write(&lower)
        || redirects_to_device(&lower)
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
        AgentCommand::Shell { cmd, .. } => mode == "safe" || is_dangerous_shell(cmd),
        AgentCommand::WriteFile { .. }
        | AgentCommand::PatchFile { .. }
        | AgentCommand::GitCommit { .. } => mode == "safe",
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
        AgentCommand::Shell {
            cmd: cmd.into(),
            cwd: None,
        }
    }

    #[test]
    fn test_full_auto_never_approves() {
        assert!(!needs_approval("full_auto", &shell("rm -rf /")));
        assert!(!needs_approval("full_auto", &AgentCommand::GitPush));
        assert!(!needs_approval(
            "full_auto",
            &AgentCommand::WriteFile {
                path: "a".into(),
                content: "x".into()
            }
        ));
    }

    #[test]
    fn test_safe_mode_readonly_free() {
        assert!(!needs_approval(
            "safe",
            &AgentCommand::ReadFile { path: "a".into() }
        ));
        assert!(!needs_approval(
            "safe",
            &AgentCommand::ListDir { path: ".".into() }
        ));
        assert!(!needs_approval(
            "safe",
            &AgentCommand::Search {
                pattern: "x".into(),
                path: ".".into(),
                include: None
            }
        ));
        assert!(!needs_approval("safe", &AgentCommand::GitStatus));
        assert!(!needs_approval(
            "safe",
            &AgentCommand::GitDiff { path: None }
        ));
    }

    #[test]
    fn test_safe_mode_writes_and_shell_need_approval() {
        assert!(needs_approval("safe", &shell("ls")));
        assert!(needs_approval(
            "safe",
            &AgentCommand::WriteFile {
                path: "a".into(),
                content: "x".into()
            }
        ));
        assert!(needs_approval(
            "safe",
            &AgentCommand::PatchFile {
                path: "a".into(),
                old_string: "o".into(),
                new_string: "n".into()
            }
        ));
        assert!(needs_approval(
            "safe",
            &AgentCommand::GitCommit {
                message: "m".into()
            }
        ));
        assert!(needs_approval("safe", &AgentCommand::GitPush));
    }

    #[test]
    fn test_auto_write_mode() {
        // 写操作自动放行
        assert!(!needs_approval(
            "auto_write",
            &AgentCommand::WriteFile {
                path: "a".into(),
                content: "x".into()
            }
        ));
        assert!(!needs_approval(
            "auto_write",
            &AgentCommand::GitCommit {
                message: "m".into()
            }
        ));
        // 普通 shell 自动放行
        assert!(!needs_approval("auto_write", &shell("npm test")));
        // 危险 shell 需确认
        assert!(needs_approval("auto_write", &shell("rm -rf node_modules")));
        assert!(needs_approval(
            "auto_write",
            &shell("git push -f origin main")
        ));
        // 核心回归：shell 形态 git push（非 force）同样需确认——与 GitPush 工具同等对待，
        // 否则模型可用 shell 绕过审批矩阵
        assert!(needs_approval("auto_write", &shell("git push origin main")));
        assert!(needs_approval("auto_write", &shell("git push -u origin main")));
        // 非 push 形态不误伤
        assert!(!needs_approval("auto_write", &shell("git pushup origin")));
        assert!(!needs_approval("auto_write", &shell("git-push origin")));
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
        // shell 形态 push 视为危险（与 GitPush 工具同等对待）
        assert!(is_dangerous_shell("git push origin main"));
        assert!(!is_dangerous_shell("npm run rebuild")); // 含 "rebuild" 不含 "reboot"
        // 非 push 不误伤
        assert!(!is_dangerous_shell("git pushup origin"));
        assert!(!is_dangerous_shell("git-push origin"));
    }

    #[test]
    fn test_is_force_push() {
        // 选项后置/前置均命中
        assert!(is_force_push("git push origin main --force"));
        assert!(is_force_push("git push -f origin"));
        assert!(is_force_push("git push origin main -f"));
        assert!(is_force_push("git push --force-with-lease"));
        // 等号形态 prefix 命中（--force-with-lease=ref）
        assert!(is_force_push("git push --force-with-lease=main"));
        // 合并短选项含 f 命中（-uf / -fu）
        assert!(is_force_push("git push -uf origin main"));
        assert!(is_force_push("git push -fu origin main"));
        // 普通推送 / 非独立 "push" token 不命中
        assert!(!is_force_push("git push origin main"));
        assert!(!is_force_push("git push -u origin main"));
        assert!(!is_force_push("git pushup -f"));
        assert!(!is_force_push("git pull -f"));
    }

    #[test]
    fn test_is_git_push() {
        // 任何形态的 git push（含 -C 变体）都命中
        assert!(is_git_push("git push origin main"));
        assert!(is_git_push("git push -u origin main"));
        assert!(is_git_push("git -C /path/to/repo push origin main"));
        // 非 push 不命中：pushup 非独立 token、git-push 连字符、push 出现在别处
        assert!(!is_git_push("git pushup origin"));
        assert!(!is_git_push("git-push origin"));
        assert!(!is_git_push("git commit -m 'push stuff'"));
        assert!(!is_git_push("git status"));
    }

    #[test]
    fn test_is_dd_write() {
        // 参数序无关均命中
        assert!(is_dangerous_shell("dd if=/dev/zero of=/dev/sda"));
        assert!(is_dangerous_shell("sudo dd bs=1M if=x of=y"));
        // 非独立 token 或只有 of= 不命中
        assert!(!is_dangerous_shell("ddd if=x"));
        assert!(!is_dangerous_shell("dd of=x"));
    }

    #[test]
    fn test_redirects_to_device() {
        // 良性 /dev/null 排除
        assert!(!is_dangerous_shell("npm run build > /dev/null 2>&1"));
        assert!(!is_dangerous_shell("echo hi 2> /dev/null"));
        // 真实块设备命中
        assert!(is_dangerous_shell("dd of=x > /dev/sda"));
        assert!(is_dangerous_shell("cat x > /dev/mmcblk0"));
    }

    #[test]
    fn test_approval_summary() {
        assert_eq!(approval_summary(&shell("npm install")), "npm install");
        assert_eq!(
            approval_summary(&AgentCommand::WriteFile {
                path: "src/a.rs".into(),
                content: "x".into()
            }),
            "src/a.rs"
        );
        assert_eq!(approval_summary(&AgentCommand::GitPush), "git push");
        let long = shell(&"x".repeat(200));
        assert!(approval_summary(&long).chars().count() <= 121);
    }

    use tokio::sync::mpsc;

    async fn test_state() -> crate::server::agent::AgentState {
        let db = crate::server::db::Database::new(":memory:").await.unwrap();
        let server_state = crate::server::control::ServerState::with_db(db);
        server_state.agent_state.expect("agent_state initialized")
    }

    #[tokio::test]
    async fn test_request_approval_approve_flow() {
        let state = test_state().await;
        let (tx, mut rx) = mpsc::channel(8);
        let st = state.clone();
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "shell", "npm install", "{}", &tx)
                .await
        });
        // 收到 approval_request 帧
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame["type"], "approval_request");
        assert_eq!(frame["tool"], "shell");
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        // 模拟 WS 收到批准响应
        state.resolve_approval("s1", &req_id, true, false).await;
        assert!(handle.await.unwrap());
    }

    #[tokio::test]
    async fn test_request_approval_deny_flow() {
        let state = test_state().await;
        let (tx, mut rx) = mpsc::channel(8);
        let st = state.clone();
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "git_push", "git push", "{}", &tx)
                .await
        });
        let frame = rx.recv().await.unwrap();
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        state.resolve_approval("s1", &req_id, false, false).await;
        assert!(!handle.await.unwrap());
    }

    #[tokio::test]
    async fn test_request_approval_remember_writes_session_allowed() {
        // 协议备注修订：remember=true 且批准时，resolve_approval 内部写 session_allowed，
        // 后续同 session 同类工具免审批。
        let state = test_state().await;
        let (tx, mut rx) = mpsc::channel(8);
        let st = state.clone();
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "shell", "npm install", "{}", &tx)
                .await
        });
        let frame = rx.recv().await.unwrap();
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        state.resolve_approval("s1", &req_id, true, true).await;
        assert!(handle.await.unwrap());
        assert!(state.is_allowed_for_session("s1", "shell").await);
    }

    #[tokio::test]
    async fn test_request_approval_unknown_response_ignored() {
        let state = test_state().await;
        // 未知 request_id：不 panic、不误唤醒
        state
            .resolve_approval("s1", "nonexistent", true, false)
            .await;
    }

    #[tokio::test]
    async fn test_session_remember_set() {
        let state = test_state().await;
        assert!(!state.is_allowed_for_session("s1", "shell").await);
        state.remember_for_session("s1", "shell").await;
        assert!(state.is_allowed_for_session("s1", "shell").await);
        // 不同 session 互不影响
        assert!(!state.is_allowed_for_session("s2", "shell").await);
        // 不同工具互不影响
        assert!(!state.is_allowed_for_session("s1", "git_push").await);
    }

    #[tokio::test]
    async fn test_request_approval_abort_cleans_pending() {
        // 泄漏回归：turn future 被 drop（cancel/断连）时，ApprovalGuard 必须清掉 pending 条目，
        // 否则 approvals 表永久残留。
        let state = test_state().await;
        let (tx, _rx) = mpsc::channel(8);
        let st = state.clone();
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "shell", "npm install", "{}", &tx)
                .await
        });
        // 等条目真正插入并挂起在 oneshot 上（避免竞态：abort 前必须已 insert）
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.abort();
        // 短暂 yield 让运行时 drop task，触发 guard 的 Drop 清理
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.pending_approvals_count().await, 0);
    }
}
