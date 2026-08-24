//! 危险操作审批规则：按 workspace approval_mode 判定工具调用是否需用户确认。
use rust_tunnel_common::AgentCommand;

use super::git_plan::{self, GitRisk};

/// shell 危险模式（大小写不敏感子串匹配）。仅 auto_write 档用于判定 shell；
/// safe 档下所有写操作都需确认，不经此表。
/// 匹配前先对命令做空白归一化（任意连续空白折叠为单空格），故 `rm  -rf /`、
/// `kill\t-9` 等空白变体同样命中。
/// 强制 push / dd 写盘 / 重定向进块设备 / kill 危险信号 / 递归 rm 无法用连续子串
/// 可靠表达，由 [`is_force_push`] / [`is_git_push`] / [`is_dd_write`] /
/// [`is_kill_dangerous`] / [`is_recursive_rm`] / [`redirects_to_device`] 单独判定。
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
        matches!(t, "--force" | "-f")
            || t.starts_with("--force-with-lease")
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

/// git 危险子命令（GitExec DangerousWrite 档）token 级判定：clean -fd / branch -D /
/// stash drop / revert / push --force / reset --hard。与 [`is_git_push`] 同模式
/// （git 上下文 + token 级扫描，容忍 `-C <path>` 等全局选项），防止 shell 形态
/// 绕过 GitExec 审批矩阵（safe 与 auto_write 档都需确认）。
///
/// **入参必须是空白归一化后的原文（不转小写）**：git 的 `-D`（强制删除分支）与
/// `-d`（安全删除）是大小写敏感 flag，转小写会让 `-D` 退化成 `-d`、绕过危险判定。
/// 子命令本身用 `eq_ignore_ascii_case` 宽松匹配（git 子命令实际全小写，多一层
/// 保险）；flag 一律大小写敏感。
fn is_git_dangerous_subcommand(cmd: &str) -> bool {
    let force_flag = |t: &str| {
        matches!(t, "--force" | "-f")
            || t.starts_with("--force-with-lease")
            || (t.starts_with('-') && !t.starts_with("--") && t.len() > 2 && t.contains('f'))
    };
    let clean_danger = |t: &str| {
        t.starts_with("--force")
            || (t.starts_with('-')
                && !t.starts_with("--")
                && !t.starts_with("-n")
                && t.contains('f'))
    };
    let branch_danger = |t: &str| matches!(t, "-D" | "--delete") || t.starts_with("--force");
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let has_git = tokens.iter().any(|t| *t == "git" || t.ends_with("/git"));
    if !has_git {
        return false;
    }
    let has_sub = |sub: &str| tokens.iter().any(|t| t.eq_ignore_ascii_case(sub));
    has_sub("revert")
        || (has_sub("clean") && tokens.iter().any(|&t| clean_danger(t)))
        || (has_sub("branch") && tokens.iter().any(|&t| branch_danger(t)))
        || (has_sub("stash") && tokens.iter().any(|t| t.eq_ignore_ascii_case("drop")))
        || (has_sub("reset") && tokens.iter().any(|t| t.starts_with("--hard")))
        || (has_sub("push") && tokens.iter().any(|&t| force_flag(t)))
}

/// dd 写盘：命令含独立 token "dd"（或以 "/dd" 结尾）且目标为 /dev/ 设备
/// （任一 token 以 "of=/dev/" 开头且非 of=/dev/null）。不要求 if=——
/// `dd of=/dev/sda < /tmp/img`（无 if=，数据经 stdin 进块设备）同样命中。
/// 仅凭 if= 不再触发：`dd if=/dev/zero of=/tmp/out.img` 写普通文件并不危险。
fn is_dd_write(cmd_lower: &str) -> bool {
    let has_dd = cmd_lower
        .split_whitespace()
        .any(|t| t == "dd" || t.ends_with("/dd"));
    has_dd
        && cmd_lower.split_whitespace().any(|t| {
            t.starts_with("of=/dev/") && !t.starts_with("of=/dev/null")
        })
}

/// 命令把 /dev/ 设备节点作为裸目标参数（如 `cp x /dev/sda`、`tee /dev/sda`、
/// `wipefs /dev/sda`）：任一 token 以 "/dev/" 开头且非 /dev/null 即危险。
/// 只豁免 /dev/null（数据黑洞，写入无害）；/dev/stdout、/dev/stderr、/dev/stdin、
/// /dev/tty 等伪设备不豁免——写 tty 可向终端注入按键，同样可被恶意利用，故
/// 保守方向一律拦截（宁可多拦截，不放过设备写形态）。读取形态（如 `cat
/// /dev/urandom`）也会被拦截，属预期内的过拦截，auto_write 下弹一次确认即可。
fn writes_to_device(cmd_lower: &str) -> bool {
    cmd_lower
        .split_whitespace()
        .any(|t| t.starts_with("/dev/") && t != "/dev/null")
}

/// kill 危险信号：独立 token `kill`（或以 `/kill` 结尾，排除 killall/killall5）
/// 后跟以 `-` 开头的危险信号参数（-9 / -KILL / -SIGKILL / -SIGTERM / -STOP 等）。
fn is_kill_dangerous(cmd_lower: &str) -> bool {
    const DANGEROUS_SIGNALS: &[&str] = &[
        "-9",
        "-sighup",
        "-sigint",
        "-sigkill",
        "-sigterm",
        "-sigstop",
        "-sigcont",
        "-hup",
        "-int",
        "-kill",
        "-term",
        "-stop",
        "-abrt",
        "-segv",
    ];
    let tokens: Vec<&str> = cmd_lower.split_whitespace().collect();
    for (i, t) in tokens.iter().enumerate() {
        if *t == "kill" || t.ends_with("/kill") {
            if let Some(&next) = tokens.get(i + 1) {
                if DANGEROUS_SIGNALS.contains(&next) {
                    return true;
                }
            }
        }
    }
    false
}

/// rm 递归删除：独立 token `rm`（或以 `/rm` 结尾）后带递归标志
/// （-r/-R/--recursive，或合并短选项含 'r'）。任何递归 rm 都视为危险（即便无 -f），
/// 因为 `rm -r` 即可静默删除整个目录树。
fn is_recursive_rm(cmd_lower: &str) -> bool {
    let tokens: Vec<&str> = cmd_lower.split_whitespace().collect();
    for (i, t) in tokens.iter().enumerate() {
        if *t != "rm" && !t.ends_with("/rm") {
            continue;
        }
        for &flag in &tokens[i + 1..] {
            let recursive = matches!(flag, "-r" | "-R")
                || flag.starts_with("--recursive")
                || (flag.starts_with('-')
                    && !flag.starts_with("--")
                    && flag.len() > 2
                    && flag.contains('r'));
            if recursive {
                return true;
            }
        }
    }
    false
}

/// 重定向进块设备：`>` 后（跳过空白）直接是 `/dev/xxx` 但排除良性的 `/dev/null`。
/// 不依赖 `>` 与 `/dev/` 之间是否有空格，故 `>/dev/sda`、`2>/dev/sda` 同样命中；
/// `>>` 的第二个 `>` 也参与检测。
fn redirects_to_device(cmd_lower: &str) -> bool {
    for (i, &b) in cmd_lower.as_bytes().iter().enumerate() {
        if b == b'>' {
            let path_start = cmd_lower[i + 1..].trim_start();
            if path_start.starts_with("/dev/") && !path_start.starts_with("/dev/null") {
                return true;
            }
        }
    }
    false
}

/// 该 shell 命令是否命中危险模式。
pub fn is_dangerous_shell(cmd: &str) -> bool {
    // 空白归一化：任意连续空白（空格/tab/换行）折叠为单个空格，使 `rm  -rf /`、
    // `kill\t-9` 等空白变体无法绕过子串匹配。归一化后同一字符序列的语义不变。
    let normalized = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = normalized.to_lowercase();
    DANGEROUS_SHELL_PATTERNS.iter().any(|p| lower.contains(p))
        || is_force_push(&lower)
        || is_git_push(&lower) // 对 auto_write 而言所有 push 都是"危险"需确认；safe 档本来就全确认
        // clean -fd / branch -D / stash drop / revert / reset --hard：注意传原文
        // （不转小写），`-D` 与 `-d` 是大小写敏感 flag。
        || is_git_dangerous_subcommand(&normalized)
        || is_dd_write(&lower)
        || is_kill_dangerous(&lower)
        || is_recursive_rm(&lower)
        || redirects_to_device(&lower)
        || writes_to_device(&lower)
}

/// 命令是否"破坏性"：即使会话已 allow_always 记住该工具，破坏性命令仍不可免审
/// （防"记住 shell"整条绕过安全网——`rm -rf /`、`dd of=/dev/sda` 不得因一次
/// allow_always 就静默放行）。Shell → 按危险命令判定；GitPush → 永远破坏性
/// （推送不可撤销地改写远程，safe/auto_write 下都应确认）；其余工具（文件读写、
/// 提交等）由审批矩阵单独覆盖，allow_always 记住后免审语义不受影响。
pub fn command_is_destructive(cmd: &AgentCommand) -> bool {
    match cmd {
        AgentCommand::Shell { cmd, .. } | AgentCommand::ShellWithTimeout { cmd, .. } => {
            is_dangerous_shell(cmd)
        }
        AgentCommand::GitPush => true,
        _ => false,
    }
}

/// 按审批模式判定工具调用是否需用户确认。非法 mode 按 "safe" 处理（保守）。
/// `"plan"` 模式下：只读工具免审，写操作/危险 shell 一律需确认（模型理论上
/// 看不到写工具 schema，此分支为防御性兜底）。
pub fn needs_approval(mode: &str, cmd: &AgentCommand) -> bool {
    let mode = match mode {
        "safe" | "auto_write" | "full_auto" | "plan" => mode,
        _ => "safe",
    };
    if mode == "full_auto" {
        return false;
    }
    // Plan 模式：只读工具免审，写工具/危险 shell 一律需确认（与 safe 模式行为一致）。
    // Shell 不在 plan schema 中，但保留防御：非危险 shell 免审，危险 shell 需确认。
    if mode == "plan" {
        let result = match cmd {
            AgentCommand::ReadFile { .. }
            | AgentCommand::ListDir { .. }
            | AgentCommand::Search { .. }
            | AgentCommand::GitStatus
            | AgentCommand::GitDiff { .. } => false,
            AgentCommand::Shell { cmd, .. } | AgentCommand::ShellWithTimeout { cmd, .. } => {
                is_dangerous_shell(cmd)
            }
            AgentCommand::GitExec { args } => {
                // Read 免审，其余需确认：Err 按保守需审
                !matches!(git_plan::plan(args), Ok(p) if p.risk == GitRisk::Read)
            }
            _ => true, // WriteFile/PatchFile/GitCommit/GitPush 一律需确认
        };
        return result;
    }
    match cmd {
        AgentCommand::ReadFile { .. }
        | AgentCommand::ReadFileRange { .. }
        | AgentCommand::ListDir { .. }
        | AgentCommand::Search { .. }
        | AgentCommand::GitStatus
        | AgentCommand::GitDiff { .. }
        | AgentCommand::CodeOutline { .. }
        | AgentCommand::ReadSymbol { .. } => false,
        AgentCommand::Shell { cmd, .. } | AgentCommand::ShellWithTimeout { cmd, .. } => {
            mode == "safe" || is_dangerous_shell(cmd)
        }
        AgentCommand::WriteFile { .. }
        | AgentCommand::PatchFile { .. }
        | AgentCommand::EditFile { .. }
        | AgentCommand::WriteFile2 { .. }
        | AgentCommand::GitCommit { .. } => mode == "safe",
        AgentCommand::GitPush => true, // safe 与 auto_write 都需确认
        AgentCommand::GitExec { args } => match git_plan::plan(args) {
            // 与 classify 共用 git_plan 单数据源：Read 免审；SafeWrite 对齐
            // git_commit（safe 档需审）；DangerousWrite 对齐 git_push（safe 与
            // auto_write 都需审）。
            Ok(planned) => match planned.risk {
                GitRisk::Read => false,
                GitRisk::SafeWrite => mode == "safe",
                GitRisk::DangerousWrite => mode == "safe" || mode == "auto_write",
            },
            // 非法参数正常在 parse 阶段已拦截；兜底按需审处理（保守）。
            Err(_) => true,
        },
    }
}

/// 命令是否只读（可并发执行、不涉审批、无需 workspace_lock）：
/// 与 needs_approval 对 ReadFile/ListDir/Search/GitStatus/GitDiff/GitExec(Read)
/// 恒返回 false 的集合一致（full_auto 的全局放行不参与——写命令仍须串行保序）。
pub fn is_readonly_command(cmd: &AgentCommand) -> bool {
    match cmd {
        AgentCommand::ReadFile { .. }
        | AgentCommand::ReadFileRange { .. }
        | AgentCommand::ListDir { .. }
        | AgentCommand::Search { .. }
        | AgentCommand::GitStatus
        | AgentCommand::GitDiff { .. }
        | AgentCommand::CodeOutline { .. }
        | AgentCommand::ReadSymbol { .. } => true,
        AgentCommand::GitExec { args } => matches!(
            git_plan::plan(args),
            Ok(planned) if planned.risk == GitRisk::Read
        ),
        _ => false,
    }
}

/// 把工具调用标记数组划分为「连续只读段」与「串行单调用」交替段。
/// 纯函数便于单测（并发/顺序分组逻辑不依赖 I/O）。
pub fn partition_tool_calls(flags: &[bool]) -> Vec<(usize, usize, bool)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < flags.len() {
        let start = i;
        if flags[i] {
            while i < flags.len() && flags[i] {
                i += 1;
            }
            out.push((start, i, true));
        } else {
            i += 1;
            out.push((start, i, false));
        }
    }
    out
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
        AgentCommand::Shell { cmd, .. } | AgentCommand::ShellWithTimeout { cmd, .. } => {
            truncate(cmd)
        }
        AgentCommand::WriteFile { path, .. }
        | AgentCommand::PatchFile { path, .. }
        | AgentCommand::EditFile { path, .. }
        | AgentCommand::WriteFile2 { path, .. } => path.clone(),
        AgentCommand::GitCommit { message } => truncate(message),
        AgentCommand::GitPush => "git push".to_string(),
        AgentCommand::GitExec { args } => {
            // 摘要：子命令 + 截断参数（"git reset --hard" 形态一眼可辨）。
            // 对含 "git " 前缀的完整串截断，保证总长 ≤ MAX+1。
            truncate(&format!("git {}", args.join(" ")))
        }
        _ => String::new(), // 只读工具不会进入审批路径
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApprovalOption, ApprovalResult};

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
        assert!(needs_approval(
            "auto_write",
            &shell("git push -u origin main")
        ));
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

    fn git_exec(args: &[&str]) -> AgentCommand {
        AgentCommand::GitExec {
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }

    #[test]
    fn test_git_exec_read_free_all_modes() {
        for mode in ["safe", "auto_write", "full_auto"] {
            assert!(!needs_approval(mode, &git_exec(&["status"])));
            assert!(!needs_approval(mode, &git_exec(&["diff", "--cached"])));
            assert!(!needs_approval(mode, &git_exec(&["log", "-n", "10"])));
            assert!(!needs_approval(mode, &git_exec(&["show", "HEAD"])));
            assert!(!needs_approval(mode, &git_exec(&["branch", "--list"])));
            assert!(!needs_approval(mode, &git_exec(&["stash", "list"])));
            assert!(!needs_approval(mode, &git_exec(&["remote", "get-url", "origin"])));
        }
    }

    #[test]
    fn test_git_exec_safe_write_needs_safe_mode_only() {
        assert!(needs_approval("safe", &git_exec(&["commit", "-m", "x"])));
        assert!(!needs_approval("auto_write", &git_exec(&["commit", "-m", "x"])));
        assert!(!needs_approval("full_auto", &git_exec(&["commit", "-m", "x"])));
        assert!(needs_approval("safe", &git_exec(&["checkout", "-b", "f"])));
        assert!(!needs_approval("auto_write", &git_exec(&["checkout", "-b", "f"])));
        assert!(needs_approval("safe", &git_exec(&["add", "--", "a.rs"])));
        assert!(!needs_approval("auto_write", &git_exec(&["add", "--", "a.rs"])));
    }

    #[test]
    fn test_git_exec_dangerous_needs_safe_and_auto_write() {
        for args in [
            &["reset", "--hard"][..],
            &["branch", "-D", "f"][..],
            &["stash", "drop", "stash@{0}"][..],
            &["revert", "abc123"][..],
            &["push", "--force"][..],
            &["push", "--force-with-lease"][..],
        ] {
            assert!(needs_approval("safe", &git_exec(args)), "args = {args:?}");
            assert!(
                needs_approval("auto_write", &git_exec(args)),
                "auto_write args = {args:?}"
            );
            assert!(!needs_approval("full_auto", &git_exec(args)), "args = {args:?}");
        }
    }

    #[test]
    fn test_git_exec_invalid_args_conservative_approval() {
        // 非法参数正常在 parse 阶段拦截；落到审批层时按需审处理（保守方向）。
        assert!(needs_approval("safe", &git_exec(&["clean", "-fd"])));
        assert!(needs_approval("safe", &git_exec(&["rm", "-rf"])));
        assert!(needs_approval("auto_write", &git_exec(&["clean", "-fd"])));
        assert!(needs_approval("auto_write", &git_exec(&["rm", "-rf"])));
        // full_auto 恒放行（模式早退，非法参数不例外）
        assert!(!needs_approval("full_auto", &git_exec(&["rm", "-rf"])));
    }

    #[test]
    fn test_shell_dangerous_git_subcommand_bypass() {
        // auto_write 档下，shell 形态调用 GitExec DangerousWrite 子命令必须需审
        // （否则模型可用 shell 绕过审批矩阵）。
        assert!(is_git_dangerous_subcommand("git clean -fd"));
        assert!(is_git_dangerous_subcommand("git clean -fdx"));
        assert!(is_git_dangerous_subcommand("git branch -D feature"));
        assert!(is_git_dangerous_subcommand("git branch --delete --force feature"));
        assert!(is_git_dangerous_subcommand("git stash drop stash@{0}"));
        assert!(is_git_dangerous_subcommand("git revert abc123"));
        assert!(is_git_dangerous_subcommand("git push --force origin main"));
        assert!(is_git_dangerous_subcommand("git reset --hard HEAD"));
        assert!(is_git_dangerous_subcommand("git -C /repo reset --hard HEAD"));
        // 非危险形态不误伤
        assert!(!is_git_dangerous_subcommand("git status"));
        assert!(!is_git_dangerous_subcommand("git clean -n")); // 干跑
        assert!(!is_git_dangerous_subcommand("git branch -d merged-feature")); // 安全删除
        assert!(!is_git_dangerous_subcommand("git branch feature")); // 创建
        assert!(!is_git_dangerous_subcommand("git stash list"));
        assert!(!is_git_dangerous_subcommand("git stash push -m wip"));
        assert!(!is_git_dangerous_subcommand("git reset --soft HEAD~1")); // 非 hard
        assert!(!is_git_dangerous_subcommand("git push origin main"));
        assert!(!is_git_dangerous_subcommand("git pushup -f"));
        // 无 git 上下文不命中
        assert!(!is_git_dangerous_subcommand("branch -D x"));
        assert!(!is_git_dangerous_subcommand("npm run reset --hard"));
    }

    #[test]
    fn test_shell_dangerous_git_subcommand_needs_approval_in_auto_write() {
        for cmd in [
            "git clean -fd",
            "git branch -D feature",
            "git stash drop",
            "git revert abc123",
            "git push --force origin main",
            "git reset --hard HEAD",
        ] {
            assert!(
                needs_approval("auto_write", &shell(cmd)),
                "auto_write shell '{cmd}' must need approval"
            );
        }
        // 安全形态不误伤
        assert!(!needs_approval("auto_write", &shell("git branch -d merged")));
        assert!(!needs_approval("auto_write", &shell("git status")));
        assert!(!needs_approval("auto_write", &shell("git clean -n")));
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
        // 目标为 /dev/ 设备（of=/dev/<非null>）即命中，参数序无关、不要求 if=
        assert!(is_dangerous_shell("dd if=/dev/zero of=/dev/sda"));
        assert!(is_dangerous_shell("sudo dd bs=1M of=/dev/sda"));
        assert!(is_dangerous_shell("dd of=/dev/sda < /tmp/disk.img")); // 无 if=，stdin 进块设备
        // 写普通文件不命中（仅凭 if= 不再触发）
        assert!(!is_dangerous_shell("dd if=/dev/zero of=/tmp/out.img"));
        assert!(!is_dangerous_shell("sudo dd bs=1M if=x of=y"));
        // 非独立 token 或 of= 目标非设备不命中
        assert!(!is_dangerous_shell("ddd if=x"));
        assert!(!is_dangerous_shell("dd of=x"));
        assert!(!is_dangerous_shell("dd of=/dev/null"));
    }

    #[test]
    fn test_device_write_forms() {
        // BUG A 回归：写设备形态不得因缺 if= / 缺 `>` 重定向而静默放行
        assert!(is_dangerous_shell("dd of=/dev/sda < /tmp/disk.img"));
        assert!(is_dangerous_shell("cp /tmp/img /dev/sda"));
        assert!(is_dangerous_shell("tee /dev/sda < x"));
        assert!(is_dangerous_shell("wipefs /dev/sda"));
        assert!(is_dangerous_shell("sudo wipefs -a /dev/sdb"));
        assert!(is_dangerous_shell("mkfs.ext4 /dev/sda")); // mkfs 模式
        // 良性：/dev/null 数据黑洞豁免、普通文件目标不命中
        assert!(!is_dangerous_shell("cp /etc/hostname /dev/null"));
        assert!(!is_dangerous_shell("cp a b"));
        assert!(!is_dangerous_shell("npm run build > /dev/null 2>&1"));
    }

    #[test]
    fn test_command_is_destructive() {
        // BUG B 回归：allow_always 记住 shell 后，破坏性命令仍须确认
        assert!(command_is_destructive(&shell("rm -rf /")));
        assert!(command_is_destructive(&shell("dd of=/dev/sda")));
        assert!(!command_is_destructive(&shell("ls")));
        assert!(!command_is_destructive(&shell("npm test")));
        assert!(command_is_destructive(&AgentCommand::GitPush));
        // 非 shell 工具不判破坏性（allow_always 免审语义不受影响）
        assert!(!command_is_destructive(&AgentCommand::WriteFile {
            path: "a".into(),
            content: "x".into()
        }));
        assert!(!command_is_destructive(&AgentCommand::GitCommit {
            message: "m".into()
        }));
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
    fn test_whitespace_variant_bypass() {
        // 空格变体绕过：连续空白折叠为单空格后应命中
        assert!(is_dangerous_shell("rm  -rf /"));
        assert!(is_dangerous_shell("rm\t-rf\t/"));
        assert!(is_dangerous_shell("git reset  --hard HEAD"));
    }

    #[test]
    fn test_kill_signal_variants() {
        // 多空格 / 信号名变体 / SIG 前缀变体
        assert!(is_dangerous_shell("kill  -9  1234"));
        assert!(is_dangerous_shell("kill -KILL 1234"));
        assert!(is_dangerous_shell("kill -SIGKILL 1234"));
        assert!(is_dangerous_shell("sudo kill -TERM 1234"));
        // 独立 token 门槛：killall / killall5 不误杀
        assert!(!is_dangerous_shell("killall firefox"));
        assert!(!is_dangerous_shell("killall5"));
        assert!(!is_dangerous_shell("kill 1234")); // 无信号参数不命中
    }

    #[test]
    fn test_redirect_no_space() {
        // 无空格重定向（直接 `>/dev/...`、fd 重定向、空命令重定向）
        assert!(is_dangerous_shell("echo x >/dev/sda"));
        assert!(is_dangerous_shell("echo x 2>/dev/sda"));
        assert!(is_dangerous_shell(">/dev/sda"));
        // 良性的 /dev/null 在无空格形态下也不误报
        assert!(!is_dangerous_shell("echo x >/dev/null"));
        assert!(!is_dangerous_shell("echo x >/dev/null 2>&1"));
    }

    #[test]
    fn test_recursive_rm_detection() {
        // 任何递归 rm 都危险：单独 -r/-R/--recursive、长选项组合、合并短选项
        assert!(is_dangerous_shell("rm -r node_modules"));
        assert!(is_dangerous_shell("rm -R node_modules"));
        assert!(is_dangerous_shell("rm --recursive --force node_modules"));
        assert!(is_dangerous_shell("rm -R -f node_modules"));
        assert!(is_dangerous_shell("rm -r --force node_modules"));
        assert!(is_dangerous_shell("rm -rfv node_modules")); // 合并短选项含 r
        // 非递归 rm / 非独立 token 不误伤
        assert!(!is_dangerous_shell("rm file.txt"));
        assert!(!is_dangerous_shell("rm -f file.txt"));
        assert!(!is_dangerous_shell("rmdir -r dir"));
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
        assert_eq!(
            approval_summary(&git_exec(&["reset", "--hard", "HEAD"])),
            "git reset --hard HEAD"
        );
        assert_eq!(
            approval_summary(&git_exec(&["commit", "-m", "fix bug"])),
            "git commit -m fix bug"
        );
        let long = shell(&"x".repeat(200));
        assert!(approval_summary(&long).chars().count() <= 121);
        // GitExec 摘要同样截断
        let long_args: Vec<String> = vec!["commit".into(), "-m".into(), "x".repeat(300)];
        assert!(approval_summary(&AgentCommand::GitExec { args: long_args })
            .chars()
            .count()
            <= 121);
    }

    #[test]
    fn test_is_readonly_command() {
        assert!(is_readonly_command(&AgentCommand::ReadFile { path: "a".into() }));
        assert!(is_readonly_command(&AgentCommand::ListDir { path: ".".into() }));
        assert!(is_readonly_command(&AgentCommand::Search {
            pattern: "x".into(),
            path: ".".into(),
            include: None,
        }));
        assert!(is_readonly_command(&AgentCommand::GitStatus));
        assert!(is_readonly_command(&AgentCommand::GitDiff { path: None }));
        // GitExec Read 档
        assert!(is_readonly_command(&git_exec(&["status"])));
        assert!(is_readonly_command(&git_exec(&["log", "-n", "5"])));
        assert!(is_readonly_command(&git_exec(&["diff"])));
        // 写操作/危险操作
        assert!(!is_readonly_command(&shell("ls")));
        assert!(!is_readonly_command(&AgentCommand::WriteFile { path: "a".into(), content: "x".into() }));
        assert!(!is_readonly_command(&AgentCommand::GitCommit { message: "m".into() }));
        assert!(!is_readonly_command(&AgentCommand::GitPush));
        // GitExec 写档
        assert!(!is_readonly_command(&git_exec(&["add", "--", "a.rs"])));
        assert!(!is_readonly_command(&git_exec(&["reset", "--hard"])));
    }

    #[test]
    fn test_read_file_range_readonly_free() {
        assert!(!needs_approval(
            "safe",
            &AgentCommand::ReadFileRange { path: "a.rs".into(), offset: Some(1), limit: Some(100) }
        ));
    }

    #[test]
    fn test_read_file_range_is_readonly() {
        assert!(is_readonly_command(&AgentCommand::ReadFileRange {
            path: "a.rs".into(), offset: Some(10), limit: Some(20)
        }));
    }

    #[test]
    fn test_code_outline_read_symbol_readonly() {
        assert!(!needs_approval("safe", &AgentCommand::CodeOutline { path: "a.rs".into() }));
        assert!(!needs_approval("safe", &AgentCommand::ReadSymbol { path: "a.rs".into(), name: "main".into() }));
        assert!(is_readonly_command(&AgentCommand::CodeOutline { path: "a.rs".into() }));
        assert!(is_readonly_command(&AgentCommand::ReadSymbol { path: "a.rs".into(), name: "main".into() }));
    }

    #[test]
    fn test_partition_tool_calls() {
        let flags = [false, true, true, false, true];
        let segs = partition_tool_calls(&flags);
        assert_eq!(segs, vec![(0, 1, false), (1, 3, true), (3, 4, false), (4, 5, true)]);

        let flags2 = [true, true, true];
        let segs2 = partition_tool_calls(&flags2);
        assert_eq!(segs2, vec![(0, 3, true)]);

        let flags3 = [false, false];
        let segs3 = partition_tool_calls(&flags3);
        assert_eq!(segs3, vec![(0, 1, false), (1, 2, false)]);

        let segs4 = partition_tool_calls(&[]);
        assert!(segs4.is_empty());
    }

    use tokio::sync::mpsc;

    async fn test_state() -> crate::AgentState {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        crate::AgentState::new(
            std::sync::Arc::new(crate::test_helpers::TestRegistry::new(&db)),
            db,
        )
    }

    #[tokio::test]
    async fn test_request_approval_approve_flow() {
        let state = test_state().await;
        let (tx, mut rx) = mpsc::channel(8);
        let st = state.clone();
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "shell", "npm install", "{}", &[], &tx)
                .await
        });
        // 收到 approval_request 帧
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame["type"], "approval_request");
        assert_eq!(frame["tool"], "shell");
        // 无选项：帧带空 options 数组
        assert_eq!(
            frame["options"].as_array().map(Vec::len),
            Some(0),
            "no-option approval should carry empty options"
        );
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        // 模拟 WS 收到批准响应
        state
            .resolve_approval("s1", &req_id, true, None, false)
            .await;
        assert!(handle.await.unwrap().approved());
    }

    #[tokio::test]
    async fn test_request_approval_deny_flow() {
        let state = test_state().await;
        let (tx, mut rx) = mpsc::channel(8);
        let st = state.clone();
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "git_push", "git push", "{}", &[], &tx)
                .await
        });
        let frame = rx.recv().await.unwrap();
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        state
            .resolve_approval("s1", &req_id, false, None, false)
            .await;
        assert!(!handle.await.unwrap().approved());
    }

    #[tokio::test]
    async fn test_request_approval_remember_writes_session_allowed() {
        // 协议备注修订：remember=true 且批准时，resolve_approval 内部写 session_allowed，
        // 后续同 session 同类工具免审批。
        let state = test_state().await;
        let (tx, mut rx) = mpsc::channel(8);
        let st = state.clone();
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "shell", "npm install", "{}", &[], &tx)
                .await
        });
        let frame = rx.recv().await.unwrap();
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        state
            .resolve_approval("s1", &req_id, true, None, true)
            .await;
        assert!(handle.await.unwrap().approved());
        assert!(state.is_allowed_for_session("s1", "shell").await);
    }

    #[tokio::test]
    async fn test_request_approval_options_flow() {
        // ACP options 透传：帧携带选项；用户选中某 option_id 后 resolve 返回 Selected。
        let state = test_state().await;
        let (tx, mut rx) = mpsc::channel(8);
        let st = state.clone();
        let options = vec![
            ApprovalOption {
                id: "opt_allow_once".into(),
                label: "允许一次".into(),
                kind: "allow_once".into(),
            },
            ApprovalOption {
                id: "opt_allow_always".into(),
                label: "总是允许".into(),
                kind: "allow_always".into(),
            },
            ApprovalOption {
                id: "opt_reject".into(),
                label: "拒绝".into(),
                kind: "reject_once".into(),
            },
        ];
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "shell", "npm install", "{}", &options, &tx)
                .await
        });
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame["type"], "approval_request");
        let frame_options = frame["options"].as_array().expect("options array");
        assert_eq!(frame_options.len(), 3);
        assert_eq!(frame_options[0]["id"], "opt_allow_once");
        assert_eq!(frame_options[0]["kind"], "allow_once");
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        // 用户选中 allow_always 选项：回传 option_id
        state
            .resolve_approval(
                "s1",
                &req_id,
                false, // option_id 优先，approved 被忽略
                Some("opt_allow_always".into()),
                false,
            )
            .await;
        let result = handle.await.unwrap();
        assert_eq!(
            result,
            ApprovalResult::Selected("opt_allow_always".to_string())
        );
    }

    #[tokio::test]
    async fn test_request_approval_options_selected_remember_writes_session_allowed() {
        // 前端在用户点击 allow_always 选项时附带 remember='session'：服务端据此
        // 记住本会话同类工具免审批（option_id 非空即可触发 remember）。
        let state = test_state().await;
        let (tx, mut rx) = mpsc::channel(8);
        let st = state.clone();
        let options = vec![ApprovalOption {
            id: "allow_always".into(),
            label: "总是允许".into(),
            kind: "allow_always".into(),
        }];
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "shell", "npm install", "{}", &options, &tx)
                .await
        });
        let frame = rx.recv().await.unwrap();
        let req_id = frame["request_id"].as_str().unwrap().to_string();
        state
            .resolve_approval("s1", &req_id, false, Some("allow_always".into()), true)
            .await;
        assert_eq!(
            handle.await.unwrap(),
            ApprovalResult::Selected("allow_always".to_string())
        );
        assert!(state.is_allowed_for_session("s1", "shell").await);
    }

    #[tokio::test]
    async fn test_request_approval_unknown_response_ignored() {
        let state = test_state().await;
        // 未知 request_id：不 panic、不误唤醒
        state
            .resolve_approval("s1", "nonexistent", true, None, false)
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
        // 泄漏回归：turn future 被 drop（cancel/断连）时，PendingGuard 必须清掉 pending 条目，
        // 否则 approvals 表永久残留。
        let state = test_state().await;
        let (tx, _rx) = mpsc::channel(8);
        let st = state.clone();
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "shell", "npm install", "{}", &[], &tx)
                .await
        });
        // 等条目真正插入并挂起在 oneshot 上（避免竞态：abort 前必须已 insert）
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.abort();
        // 短暂 yield 让运行时 drop task，触发 guard 的 Drop 清理
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.pending_approvals_count().await, 0);
    }

    #[tokio::test]
    async fn test_request_approval_send_failure_denies_fast() {
        // 回归（评审 Finding 2）：前端已断开（WS 通道接收端被 drop）时，
        // approval_request 帧发送失败应立即按拒绝返回，而不是等满 5 分钟超时——
        // 否则调用方（ACP 连接任务的请求处理器）被长期占用，阻塞 agent 下一个
        // 工具调用。
        let state = test_state().await;
        let (tx, rx) = mpsc::channel(8);
        drop(rx); // 模拟前端断开：接收端被 drop，send 立即 Err
        let st = state.clone();
        let handle = tokio::spawn(async move {
            st.request_approval("s1", "shell", "npm install", "{}", &[], &tx)
                .await
        });
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("request_approval must return promptly when send fails")
            .expect("task panicked");
        assert_eq!(
            result,
            ApprovalResult::Denied,
            "send failure should be deny"
        );
        // pending 条目已清理，无泄漏
        assert_eq!(state.pending_approvals_count().await, 0);
    }

    #[test]
    fn test_plan_mode_readonly_free() {
        // plan 模式下只读工具免审
        assert!(!needs_approval("plan", &AgentCommand::ReadFile { path: "a".into() }));
        assert!(!needs_approval("plan", &AgentCommand::ListDir { path: ".".into() }));
        assert!(!needs_approval("plan", &AgentCommand::Search {
            pattern: "x".into(),
            path: ".".into(),
            include: None
        }));
        assert!(!needs_approval("plan", &AgentCommand::GitStatus));
        assert!(!needs_approval("plan", &AgentCommand::GitDiff { path: None }));
        assert!(!needs_approval("plan", &git_exec(&["status"])));
        assert!(!needs_approval("plan", &git_exec(&["log", "-n", "5"])));
    }

    #[test]
    fn test_plan_mode_writes_need_approval() {
        // plan 模式下写工具一律需确认（防御性——模型理论上看不到写工具 schema）
        // 注意：只读 shell（如 ls）在 plan 模式下免审（与 safe 模式行为一致）；
        // 非危险 shell 在 plan 模式下也不需确认——模型看不到 shell schema，此
        // 分支仅为 parse 层防御性兜底（plan_mode_guard 拦截 shell 工具名）。
        assert!(!needs_approval("plan", &shell("ls")));
        assert!(!needs_approval("plan", &shell("git status")));
        assert!(!needs_approval("plan", &shell("echo x > file.txt"))); // 非危险 shell
        // 危险 shell 需确认
        assert!(needs_approval("plan", &shell("rm -rf /")));
        // 写工具需确认
        assert!(needs_approval("plan", &AgentCommand::WriteFile {
            path: "a".into(),
            content: "x".into()
        }));
        assert!(needs_approval("plan", &AgentCommand::PatchFile {
            path: "a".into(),
            old_string: "o".into(),
            new_string: "n".into()
        }));
        assert!(needs_approval("plan", &AgentCommand::GitCommit {
            message: "m".into()
        }));
        assert!(needs_approval("plan", &AgentCommand::GitPush));
    }

    // ── EditFile / WriteFile2 审批矩阵 ──────────────────────

    #[test]
    fn test_edit_file_write_file2_approval_matrix() {
        // EditFile: safe 档需审，auto_write 免审，full_auto 免审
        assert!(needs_approval("safe", &AgentCommand::EditFile {
            path: "a.rs".into(), edits: vec![], expected_hash: None,
        }));
        assert!(!needs_approval("auto_write", &AgentCommand::EditFile {
            path: "a.rs".into(), edits: vec![], expected_hash: None,
        }));
        assert!(!needs_approval("full_auto", &AgentCommand::EditFile {
            path: "a.rs".into(), edits: vec![], expected_hash: None,
        }));
        // WriteFile2: 同 WriteFile 矩阵
        assert!(needs_approval("safe", &AgentCommand::WriteFile2 {
            path: "a.rs".into(), content: "x".into(), expected_hash: None,
        }));
        assert!(!needs_approval("auto_write", &AgentCommand::WriteFile2 {
            path: "a.rs".into(), content: "x".into(), expected_hash: None,
        }));
        assert!(!needs_approval("full_auto", &AgentCommand::WriteFile2 {
            path: "a.rs".into(), content: "x".into(), expected_hash: None,
        }));
    }

    #[test]
    fn test_edit_file_write_file2_plan_mode_needs_approval() {
        assert!(needs_approval("plan", &AgentCommand::EditFile {
            path: "a.rs".into(), edits: vec![], expected_hash: None,
        }));
        assert!(needs_approval("plan", &AgentCommand::WriteFile2 {
            path: "a.rs".into(), content: "x".into(), expected_hash: None,
        }));
    }

    #[test]
    fn test_edit_file_write_file2_not_readonly() {
        assert!(!is_readonly_command(&AgentCommand::EditFile {
            path: "a.rs".into(), edits: vec![], expected_hash: None,
        }));
        assert!(!is_readonly_command(&AgentCommand::WriteFile2 {
            path: "a.rs".into(), content: "x".into(), expected_hash: None,
        }));
    }

    #[test]
    fn test_edit_file_write_file2_not_destructive() {
        assert!(!command_is_destructive(&AgentCommand::EditFile {
            path: "a.rs".into(), edits: vec![], expected_hash: None,
        }));
        assert!(!command_is_destructive(&AgentCommand::WriteFile2 {
            path: "a.rs".into(), content: "x".into(), expected_hash: None,
        }));
    }

    #[test]
    fn test_approval_summary_new_variants() {
        assert_eq!(approval_summary(&AgentCommand::EditFile {
            path: "src/main.rs".into(), edits: vec![], expected_hash: None,
        }), "src/main.rs");
        assert_eq!(approval_summary(&AgentCommand::WriteFile2 {
            path: "out.txt".into(), content: "x".into(), expected_hash: None,
        }), "out.txt");
    }
}
