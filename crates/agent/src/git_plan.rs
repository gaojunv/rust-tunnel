//! git 命令白名单规划：把面板 / LLM 工具构造的 git 参数校验为可安全执行的形式，
//! 并给出风险分级。未知子命令 / 未知 flag / 可疑 pathspec 一律 fail-closed，
//! 保证经隧道下发的 `AgentCommand::GitExec` 参数不会触发 shell 注入或沙箱逃逸。
//!
//! 单数据源：`SUB_TABLE` 同时供 [`plan`]（白名单校验）与 [`classify`]（风险分级）
//! 使用，避免两张表漂移导致"能执行但审批不过 / 审批过但不能执行"。

use thiserror::Error;

/// git 子命令风险分级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitRisk {
    /// 只读：status / diff / log / show / branch list / stash list / remote get-url
    Read,
    /// 普通写操作：commit / add / restore --staged / checkout / pull / push（非 force）/
    /// branch 创建与 -d / reset（非 hard）/ stash push|apply|pop / remote add
    SafeWrite,
    /// 危险写操作：reset --hard / clean -fd / branch -D / stash drop / revert /
    /// push --force。safe 与 auto_write 档都需用户确认（对齐 git_push）。
    DangerousWrite,
}

/// [`plan`] 的输出：规范化后的 git 参数 + 风险分级。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedGit {
    /// 可在客户端直接执行的 git 参数（首个元素为子命令）。
    pub args: Vec<String>,
    pub risk: GitRisk,
}

/// git 参数规划错误（fail-closed 路径统一归一为 [`GitPlanError`]）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitPlanError {
    #[error("missing git subcommand")]
    MissingSubcommand,
    #[error("unsupported git subcommand: {0}")]
    UnknownSubcommand(String),
    #[error("unsupported git argument or flag: {0}")]
    UnknownArg(String),
    #[error("missing required argument for git {0}")]
    MissingArg(&'static str),
    #[error("invalid value '{0}' for {1}")]
    InvalidValue(String, &'static str),
    #[error("pathspec '{0}' may escape the workspace")]
    PathEscapes(String),
    #[error("pathspec '{0}' must not start with '-'")]
    PathStartsWithDash(String),
}

/// 子命令静态表（单数据源）：git 子命令 → (plan 白名单是否允许, 基础风险)。
/// 危险子情形（reset --hard、branch -D、push --force 等）由 [`classify`] 在
/// 基础风险上细化。`clean` 仅在分类表（shell 旁路防护 / 审批用），plan 拒绝。
const SUB_TABLE: &[(&str, bool, GitRisk)] = &[
    ("status", true, GitRisk::Read),
    ("diff", true, GitRisk::Read),
    ("log", true, GitRisk::Read),
    ("show", true, GitRisk::Read),
    ("branch", true, GitRisk::Read),
    ("checkout", true, GitRisk::SafeWrite),
    ("pull", true, GitRisk::SafeWrite),
    ("push", true, GitRisk::SafeWrite),
    ("revert", true, GitRisk::DangerousWrite),
    ("reset", true, GitRisk::SafeWrite),
    ("stash", true, GitRisk::SafeWrite),
    ("remote", true, GitRisk::SafeWrite),
    ("commit", true, GitRisk::SafeWrite),
    ("add", true, GitRisk::SafeWrite),
    ("restore", true, GitRisk::SafeWrite),
    ("clean", false, GitRisk::DangerousWrite), // 分类表内但 plan 白名单拒绝
];

/// push 是否带 force 标志（--force / -f / --force-with-lease[=...]，含合并短选项
/// -uf/-fu 等）。语义与 approval.rs 的 `is_force_push` 对齐（保守方向一致）。
fn is_push_force_flag(t: &str) -> bool {
    t == "--force"
        || t == "-f"
        || t.starts_with("--force-with-lease")
        || (t.starts_with('-') && !t.starts_with("--") && t.len() > 2 && t.contains('f'))
}

/// 按子命令 + 参数判定风险。未知子命令保守按 DangerousWrite（fail-closed，
/// 审批兜底方向）。与 [`plan`] 共用 `SUB_TABLE` 单数据源。
pub fn classify(args: &[String]) -> GitRisk {
    let Some(sub) = args.first() else {
        return GitRisk::DangerousWrite;
    };
    let sub = sub.as_str();
    let base = SUB_TABLE
        .iter()
        .find(|(s, ..)| *s == sub)
        .map(|(_, _, risk)| *risk)
        .unwrap_or(GitRisk::DangerousWrite);
    match sub {
        "branch" => {
            // list（--list/--format/-a/-r/-v 或纯 `branch`）保持 Read；创建（裸名）
            // 与 -d 为 SafeWrite；-D / --delete --force 为 DangerousWrite。
            let has_force = args
                .iter()
                .skip(1)
                .any(|a| a == "-D" || a == "--force" || a == "-f");
            let has_delete = args
                .iter()
                .skip(1)
                .any(|a| a == "-d" || a == "-D" || a == "--delete");
            if has_delete {
                if has_force {
                    GitRisk::DangerousWrite
                } else {
                    GitRisk::SafeWrite
                }
            } else if args.iter().skip(1).any(|a| !a.starts_with('-')) {
                GitRisk::SafeWrite // 创建（含 -f 强制创建）
            } else {
                GitRisk::Read
            }
        }
        "push" => {
            if args.iter().any(|a| is_push_force_flag(a)) {
                GitRisk::DangerousWrite
            } else {
                base
            }
        }
        "reset" => {
            if args.iter().any(|a| a == "--hard") {
                GitRisk::DangerousWrite
            } else {
                base
            }
        }
        "stash" => {
            if args.iter().any(|a| a == "drop") {
                GitRisk::DangerousWrite
            } else if args.iter().any(|a| a == "list") {
                GitRisk::Read
            } else {
                base
            }
        }
        "remote" => {
            if args.iter().any(|a| a == "get-url") {
                GitRisk::Read
            } else {
                base
            }
        }
        _ => base,
    }
}

/// 校验一个 pathspec：非空、不以 `-` 开头（防 flag 注入）、不含 `..` 上溢组件。
fn check_path(p: &str) -> Result<(), GitPlanError> {
    if p.is_empty() {
        return Err(GitPlanError::InvalidValue(String::new(), "pathspec"));
    }
    if p.starts_with('-') {
        return Err(GitPlanError::PathStartsWithDash(p.to_string()));
    }
    if p == ".." || p.starts_with("../") || p.contains("/../") {
        return Err(GitPlanError::PathEscapes(p.to_string()));
    }
    Ok(())
}

/// 校验一个裸 token（remote 名 / URL / stash 引用等）：非空、非 flag、非 `..`。
fn check_bare_token(t: &str) -> Result<(), GitPlanError> {
    if t.is_empty() {
        return Err(GitPlanError::InvalidValue(String::new(), "bare token"));
    }
    if t.starts_with('-') {
        return Err(GitPlanError::UnknownArg(t.to_string()));
    }
    if t == ".." || t.starts_with("../") || t.contains("/../") {
        return Err(GitPlanError::PathEscapes(t.to_string()));
    }
    Ok(())
}

/// 把 `rest` 在第一个 `--` 处拆开：之前为参数段，之后为 pathspec 段。
/// 无 `--` 时路径段为空。pathspec 必须用 `--` 分隔（任务约束）。
fn split_paths(rest: &[String]) -> (&[String], &[String]) {
    match rest.iter().position(|t| t == "--") {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &[]),
    }
}

/// 校验 `git stash` 的 stash 引用形态：`stash@{<digits>}`。
fn check_stash_ref(s: &str) -> Result<(), GitPlanError> {
    let Some(inner) = s.strip_prefix("stash@{").and_then(|x| x.strip_suffix('}')) else {
        return Err(GitPlanError::InvalidValue(s.to_string(), "stash ref"));
    };
    if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_digit()) {
        return Err(GitPlanError::InvalidValue(s.to_string(), "stash ref"));
    }
    Ok(())
}

/// 白名单规划：未知子命令 / 未知 flag fail-closed；返回规范参数与风险分级。
pub fn plan(args: &[String]) -> Result<PlannedGit, GitPlanError> {
    let Some(sub) = args.first() else {
        return Err(GitPlanError::MissingSubcommand);
    };
    let sub = sub.as_str();
    let entry = SUB_TABLE
        .iter()
        .find(|(s, ..)| *s == sub)
        .ok_or_else(|| GitPlanError::UnknownSubcommand(sub.to_string()))?;
    if !entry.1 {
        // 分类表内但白名单拒绝（如 clean）：fail-closed，不开放执行。
        return Err(GitPlanError::UnknownSubcommand(sub.to_string()));
    }
    let rest = &args[1..];

    match sub {
        "status" | "pull" => {
            if let Some(t) = rest.first() {
                return Err(GitPlanError::UnknownArg(t.clone()));
            }
        }
        "diff" => {
            let (flags, paths) = split_paths(rest);
            let mut saw_cached = false;
            for t in flags {
                if t == "--cached" && !saw_cached {
                    saw_cached = true;
                } else {
                    return Err(GitPlanError::UnknownArg(t.clone()));
                }
            }
            for p in paths {
                check_path(p)?;
            }
        }
        "log" => {
            // 允许 `-n <num>`（/--max-count）与 `--format=`/`--pretty=`（面板结构化
            // 输出）；其余一律拒绝。不开放路径过滤（callers 均不用）。
            let mut i = 0;
            while i < rest.len() {
                let t = &rest[i];
                if t == "-n" || t == "--max-count" {
                    let v = rest.get(i + 1).ok_or(GitPlanError::MissingArg("-n"))?;
                    if v.is_empty() || !v.chars().all(|c| c.is_ascii_digit()) {
                        return Err(GitPlanError::InvalidValue(v.clone(), "-n"));
                    }
                    i += 2;
                } else if t.starts_with("--format=") || t.starts_with("--pretty=") {
                    i += 1;
                } else {
                    return Err(GitPlanError::UnknownArg(t.clone()));
                }
            }
        }
        "show" => {
            // 至多一个裸 rev（非 flag）；flag / `--` 路径拒绝。
            let mut saw_rev = false;
            for t in rest {
                if t.starts_with('-') || saw_rev {
                    return Err(GitPlanError::UnknownArg(t.clone()));
                }
                saw_rev = true;
            }
        }
        "branch" => {
            let mut name: Option<&str> = None;
            let mut delete_flag = false;
            let mut force_flag = false;
            for t in rest {
                match t.as_str() {
                    "-d" | "--delete" => delete_flag = true,
                    "-D" | "--force" => force_flag = true,
                    "-f" | "-l" | "-a" | "-r" | "-v" => {}
                    _ if t.starts_with("--format=") || t.starts_with("--verbose") => {}
                    _ if t == "--list" => {}
                    _ if t.starts_with('-') => return Err(GitPlanError::UnknownArg(t.clone())),
                    _ => {
                        if name.is_some() {
                            return Err(GitPlanError::UnknownArg(t.clone()));
                        }
                        name = Some(t);
                    }
                }
            }
            if (delete_flag || force_flag) && name.is_none() {
                return Err(GitPlanError::MissingArg("branch name"));
            }
        }
        "checkout" => {
            // `-b <name>` 或裸 `<name>`；不接受 pathspec 形态。
            let mut i = 0;
            let mut name: Option<&str> = None;
            while i < rest.len() {
                let t = &rest[i];
                if t == "-b" && name.is_none() {
                    let n = rest
                        .get(i + 1)
                        .ok_or(GitPlanError::MissingArg("branch name"))?;
                    check_bare_token(n)?;
                    name = Some(n);
                    i += 2;
                } else if !t.starts_with('-') && name.is_none() {
                    check_bare_token(t)?;
                    name = Some(t);
                    i += 1;
                } else {
                    return Err(GitPlanError::UnknownArg(t.clone()));
                }
            }
            if name.is_none() {
                return Err(GitPlanError::MissingArg("branch name"));
            }
        }
        "push" => {
            for t in rest {
                if !is_push_force_flag(t) {
                    return Err(GitPlanError::UnknownArg(t.clone()));
                }
            }
        }
        "revert" => {
            if rest.len() != 1 || rest[0].starts_with('-') {
                if rest.is_empty() {
                    return Err(GitPlanError::MissingArg("rev"));
                }
                return Err(GitPlanError::UnknownArg(rest[0].clone()));
            }
        }
        "reset" => {
            let mut mode: Option<&str> = None;
            let mut rev: Option<&str> = None;
            for t in rest {
                if let Some(m) = t.strip_prefix("--") {
                    if matches!(m, "soft" | "mixed" | "hard") && mode.is_none() {
                        mode = Some(m);
                    } else {
                        return Err(GitPlanError::UnknownArg(t.clone()));
                    }
                } else if t.starts_with('-') {
                    return Err(GitPlanError::UnknownArg(t.clone()));
                } else if rev.is_none() {
                    check_bare_token(t)?;
                    rev = Some(t);
                } else {
                    return Err(GitPlanError::UnknownArg(t.clone()));
                }
            }
            let _ = (mode, rev);
        }
        "stash" => {
            let action = rest
                .first()
                .ok_or(GitPlanError::MissingArg("stash action"))?;
            let rest2 = &rest[1..];
            match action.as_str() {
                "list" => {
                    if let Some(t) = rest2.first() {
                        return Err(GitPlanError::UnknownArg(t.clone()));
                    }
                }
                "push" => {
                    let mut i = 0;
                    while i < rest2.len() {
                        if rest2[i] == "-m" {
                            if i + 1 >= rest2.len() {
                                return Err(GitPlanError::MissingArg("message"));
                            }
                            i += 2;
                        } else {
                            return Err(GitPlanError::UnknownArg(rest2[i].clone()));
                        }
                    }
                }
                "apply" | "pop" | "drop" => {
                    if let Some(s) = rest2.first() {
                        check_stash_ref(s)?;
                    }
                    if rest2.len() > 1 {
                        return Err(GitPlanError::UnknownArg(rest2[1].clone()));
                    }
                }
                other => return Err(GitPlanError::UnknownArg(other.to_string())),
            }
        }
        "remote" => {
            let action = rest
                .first()
                .ok_or(GitPlanError::MissingArg("remote action"))?;
            match action.as_str() {
                "get-url" => {
                    if rest.len() < 2 {
                        return Err(GitPlanError::MissingArg("remote name"));
                    }
                    if rest.len() > 2 {
                        return Err(GitPlanError::UnknownArg(rest[2].clone()));
                    }
                    check_bare_token(&rest[1])?;
                }
                "add" => {
                    if rest.len() < 3 {
                        return Err(GitPlanError::MissingArg("remote name and url"));
                    }
                    if rest.len() > 3 {
                        return Err(GitPlanError::UnknownArg(rest[3].clone()));
                    }
                    check_bare_token(&rest[1])?;
                    check_bare_token(&rest[2])?;
                }
                other => return Err(GitPlanError::UnknownArg(other.to_string())),
            }
        }
        "commit" => {
            // 规范形态：commit -m <message>
            if rest.len() != 2 || rest[0] != "-m" {
                let t = rest.first().cloned().unwrap_or_else(|| "-m".to_string());
                return Err(GitPlanError::UnknownArg(t));
            }
            if rest[1].is_empty() {
                return Err(GitPlanError::InvalidValue(String::new(), "message"));
            }
        }
        "add" => {
            let (flags, paths) = split_paths(rest);
            if let Some(t) = flags.first() {
                return Err(GitPlanError::UnknownArg(t.clone()));
            }
            if paths.is_empty() {
                return Err(GitPlanError::MissingArg("paths"));
            }
            for p in paths {
                check_path(p)?;
            }
        }
        "restore" => {
            let (flags, paths) = split_paths(rest);
            if flags.len() != 1 || flags[0] != "--staged" {
                let t = flags
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "--staged".to_string());
                return Err(GitPlanError::UnknownArg(t));
            }
            if paths.is_empty() {
                return Err(GitPlanError::MissingArg("paths"));
            }
            for p in paths {
                check_path(p)?;
            }
        }
        other => return Err(GitPlanError::UnknownSubcommand(other.to_string())),
    }

    Ok(PlannedGit {
        args: args.to_vec(),
        risk: classify(args),
    })
}

/// 解析远端 URL 为 (owner, repo)（阶段 2「PR 关联」预备）。支持
/// `https://github.com/o/r(.git)`、`http://...`、`git@github.com:o/r(.git)` 与
/// `ssh://git@github.com/o/r(.git)` 形态；`.git` 后缀与尾部 `/` 剥离。
pub fn parse_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let path = if let Some(rest) = url.strip_prefix("https://") {
        let (_, p) = rest.split_once('/')?;
        p
    } else if let Some(rest) = url.strip_prefix("http://") {
        let (_, p) = rest.split_once('/')?;
        p
    } else if let Some(rest) = url.strip_prefix("ssh://git@") {
        let (_, p) = rest.split_once('/')?;
        p
    } else if let Some(rest) = url.strip_prefix("git@") {
        // git@github.com:o/r.git —— host 后是 ':' 而非 '/'
        rest.split_once(':')?.1
    } else {
        return None;
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((owner, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn ok(args: &[&str]) -> PlannedGit {
        plan(&s(args)).unwrap_or_else(|e| panic!("plan {args:?} should be Ok, got {e}"))
    }

    fn err(args: &[&str]) -> GitPlanError {
        plan(&s(args)).expect_err(&format!("plan {args:?} should fail"))
    }

    #[test]
    fn test_plan_status_pull() {
        assert_eq!(ok(&["status"]).args, s(&["status"]));
        assert_eq!(ok(&["pull"]).args, s(&["pull"]));
        assert!(matches!(
            err(&["status", "-v"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["pull", "origin"]),
            GitPlanError::UnknownArg(_)
        ));
    }

    #[test]
    fn test_plan_diff_with_cached_and_paths() {
        assert_eq!(ok(&["diff"]).args, s(&["diff"]));
        assert_eq!(ok(&["diff", "--cached"]).args, s(&["diff", "--cached"]));
        assert_eq!(
            ok(&["diff", "--cached", "--", "src/main.rs"]).args,
            s(&["diff", "--cached", "--", "src/main.rs"])
        );
        // 路径必须用 `--` 分隔：裸路径 fail-closed
        assert!(matches!(
            err(&["diff", "src/main.rs"]),
            GitPlanError::UnknownArg(_)
        ));
        // 路径注入拒绝
        assert!(matches!(
            err(&["diff", "--", "../etc/passwd"]),
            GitPlanError::PathEscapes(_)
        ));
        assert!(matches!(
            err(&["diff", "--", "-r"]),
            GitPlanError::PathStartsWithDash(_)
        ));
    }

    #[test]
    fn test_plan_log() {
        assert_eq!(ok(&["log"]).args, s(&["log"]));
        assert_eq!(ok(&["log", "-n", "50"]).args, s(&["log", "-n", "50"]));
        assert_eq!(
            ok(&["log", "--format=%H", "-n", "5"]).args,
            s(&["log", "--format=%H", "-n", "5"])
        );
        assert!(matches!(
            err(&["log", "-n", "abc"]),
            GitPlanError::InvalidValue(_, _)
        ));
        assert!(matches!(
            err(&["log", "--grep=x"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["log", "--", "a.txt"]),
            GitPlanError::UnknownArg(_)
        ));
    }

    #[test]
    fn test_plan_show() {
        assert_eq!(ok(&["show"]).args, s(&["show"]));
        assert_eq!(ok(&["show", "HEAD"]).args, s(&["show", "HEAD"]));
        assert_eq!(ok(&["show", "abc1234"]).args, s(&["show", "abc1234"]));
        assert_eq!(ok(&["show", "HEAD~2"]).args, s(&["show", "HEAD~2"]));
        assert!(matches!(
            err(&["show", "--stat"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["show", "a", "b"]),
            GitPlanError::UnknownArg(_)
        ));
    }

    #[test]
    fn test_plan_branch() {
        assert_eq!(ok(&["branch"]).args, s(&["branch"]));
        assert_eq!(ok(&["branch", "--list"]).args, s(&["branch", "--list"]));
        assert_eq!(
            ok(&["branch", "--format=%(refname)"]).args,
            s(&["branch", "--format=%(refname)"])
        );
        assert_eq!(ok(&["branch", "feature"]).args, s(&["branch", "feature"]));
        assert_eq!(
            ok(&["branch", "-d", "feature"]).args,
            s(&["branch", "-d", "feature"])
        );
        assert_eq!(
            ok(&["branch", "-D", "feature"]).args,
            s(&["branch", "-D", "feature"])
        );
        assert!(matches!(
            err(&["branch", "-d"]),
            GitPlanError::MissingArg(_)
        ));
        assert!(matches!(
            err(&["branch", "--grep=x"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["branch", "a", "b"]),
            GitPlanError::UnknownArg(_)
        ));
    }

    #[test]
    fn test_plan_checkout() {
        assert_eq!(ok(&["checkout", "main"]).args, s(&["checkout", "main"]));
        assert_eq!(
            ok(&["checkout", "-b", "feature"]).args,
            s(&["checkout", "-b", "feature"])
        );
        assert!(matches!(err(&["checkout"]), GitPlanError::MissingArg(_)));
        assert!(matches!(
            err(&["checkout", "-b"]),
            GitPlanError::MissingArg(_)
        ));
        assert!(matches!(
            err(&["checkout", "--", "a.txt"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["checkout", "-b", "a", "b"]),
            GitPlanError::UnknownArg(_)
        ));
    }

    #[test]
    fn test_plan_push() {
        assert_eq!(ok(&["push"]).args, s(&["push"]));
        assert_eq!(
            ok(&["push", "--force-with-lease"]).args,
            s(&["push", "--force-with-lease"])
        );
        assert_eq!(
            ok(&["push", "--force-with-lease=main"]).args,
            s(&["push", "--force-with-lease=main"])
        );
        assert!(matches!(
            err(&["push", "origin"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["push", "--delete"]),
            GitPlanError::UnknownArg(_)
        ));
    }

    #[test]
    fn test_plan_revert_reset() {
        assert_eq!(ok(&["revert", "abc123"]).args, s(&["revert", "abc123"]));
        assert!(matches!(err(&["revert"]), GitPlanError::MissingArg(_)));
        assert!(matches!(
            err(&["revert", "-m", "1"]),
            GitPlanError::UnknownArg(_)
        ));

        assert_eq!(ok(&["reset"]).args, s(&["reset"]));
        assert_eq!(ok(&["reset", "HEAD~1"]).args, s(&["reset", "HEAD~1"]));
        assert_eq!(ok(&["reset", "--hard"]).args, s(&["reset", "--hard"]));
        assert_eq!(
            ok(&["reset", "--soft", "HEAD~2"]).args,
            s(&["reset", "--soft", "HEAD~2"])
        );
        assert_eq!(
            ok(&["reset", "--mixed", "HEAD"]).args,
            s(&["reset", "--mixed", "HEAD"])
        );
        assert!(matches!(
            err(&["reset", "--hardcore"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["reset", "a", "b"]),
            GitPlanError::UnknownArg(_)
        ));
        // pathspec 形态（git reset -- <path>）不开放：unstage 走 restore --staged
        assert!(matches!(
            err(&["reset", "--", "a.txt"]),
            GitPlanError::UnknownArg(_)
        ));
    }

    #[test]
    fn test_plan_stash() {
        assert_eq!(ok(&["stash", "list"]).args, s(&["stash", "list"]));
        assert_eq!(ok(&["stash", "push"]).args, s(&["stash", "push"]));
        assert_eq!(
            ok(&["stash", "push", "-m", "wip"]).args,
            s(&["stash", "push", "-m", "wip"])
        );
        assert_eq!(
            ok(&["stash", "apply", "stash@{1}"]).args,
            s(&["stash", "apply", "stash@{1}"])
        );
        assert_eq!(
            ok(&["stash", "pop", "stash@{0}"]).args,
            s(&["stash", "pop", "stash@{0}"])
        );
        assert_eq!(ok(&["stash", "drop"]).args, s(&["stash", "drop"]));
        assert_eq!(
            ok(&["stash", "drop", "stash@{3}"]).args,
            s(&["stash", "drop", "stash@{3}"])
        );
        assert!(matches!(err(&["stash"]), GitPlanError::MissingArg(_)));
        assert!(matches!(
            err(&["stash", "bogus"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["stash", "drop", "stash@{x}"]),
            GitPlanError::InvalidValue(_, _)
        ));
        assert!(matches!(
            err(&["stash", "push", "extra"]),
            GitPlanError::UnknownArg(_)
        ));
    }

    #[test]
    fn test_plan_remote() {
        assert_eq!(
            ok(&["remote", "get-url", "origin"]).args,
            s(&["remote", "get-url", "origin"])
        );
        assert_eq!(
            ok(&["remote", "add", "upstream", "https://github.com/o/r.git"]).args,
            s(&["remote", "add", "upstream", "https://github.com/o/r.git"])
        );
        assert!(matches!(
            err(&["remote", "remove", "x"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(err(&["remote"]), GitPlanError::MissingArg(_)));
        assert!(matches!(
            err(&["remote", "get-url"]),
            GitPlanError::MissingArg(_)
        ));
    }

    #[test]
    fn test_plan_commit_add_restore() {
        assert_eq!(
            ok(&["commit", "-m", "fix"]).args,
            s(&["commit", "-m", "fix"])
        );
        assert!(matches!(err(&["commit"]), GitPlanError::UnknownArg(_)));
        assert!(matches!(
            err(&["commit", "-a", "-m", "x"]),
            GitPlanError::UnknownArg(_)
        ));

        assert_eq!(ok(&["add", "--", "a.rs"]).args, s(&["add", "--", "a.rs"]));
        assert_eq!(
            ok(&["add", "--", "a.rs", "b.rs"]).args,
            s(&["add", "--", "a.rs", "b.rs"])
        );
        assert!(matches!(err(&["add"]), GitPlanError::MissingArg(_)));
        assert!(matches!(err(&["add", "a.rs"]), GitPlanError::UnknownArg(_)));
        assert!(matches!(err(&["add", "-A"]), GitPlanError::UnknownArg(_)));
        assert!(matches!(
            err(&["add", "--", ".."]),
            GitPlanError::PathEscapes(_)
        ));

        assert_eq!(
            ok(&["restore", "--staged", "--", "a.rs"]).args,
            s(&["restore", "--staged", "--", "a.rs"])
        );
        assert!(matches!(
            err(&["restore", "--staged", "a.rs"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["restore", "--", "a.rs"]),
            GitPlanError::UnknownArg(_)
        ));
        assert!(matches!(
            err(&["restore", "--staged", "--"]),
            GitPlanError::MissingArg(_)
        ));
    }

    #[test]
    fn test_plan_unknown_and_empty() {
        assert!(matches!(err(&[]), GitPlanError::MissingSubcommand));
        assert!(matches!(
            err(&["clean", "-fd"]),
            GitPlanError::UnknownSubcommand(_)
        ));
        assert!(matches!(
            err(&["blame"]),
            GitPlanError::UnknownSubcommand(_)
        ));
        assert!(matches!(
            err(&["rm", "-rf"]),
            GitPlanError::UnknownSubcommand(_)
        ));
    }

    #[test]
    fn test_classify_risk_levels() {
        // Read
        for args in [
            &["status"][..],
            &["diff", "--cached"][..],
            &["log", "-n", "10"][..],
            &["show", "HEAD"][..],
            &["branch"][..],
            &["branch", "--list"][..],
            &["branch", "--format=%s"][..],
            &["stash", "list"][..],
            &["remote", "get-url", "origin"][..],
        ] {
            assert_eq!(classify(&s(args)), GitRisk::Read, "args = {args:?}");
        }
        // SafeWrite
        for args in [
            &["commit", "-m", "x"][..],
            &["add", "--", "a.rs"][..],
            &["restore", "--staged", "--", "a.rs"][..],
            &["checkout", "main"][..],
            &["checkout", "-b", "f"][..],
            &["pull"][..],
            &["push"][..],
            &["branch", "feature"][..],
            &["branch", "-d", "feature"][..],
            &["reset"][..],
            &["reset", "--soft", "HEAD"][..],
            &["stash", "push"][..],
            &["stash", "apply", "stash@{0}"][..],
            &["stash", "pop", "stash@{0}"][..],
            &["remote", "add", "u", "https://github.com/o/r"][..],
        ] {
            assert_eq!(classify(&s(args)), GitRisk::SafeWrite, "args = {args:?}");
        }
        // DangerousWrite
        for args in [
            &["reset", "--hard"][..],
            &["clean", "-fd"][..],
            &["branch", "-D", "feature"][..],
            &["branch", "--delete", "--force", "feature"][..],
            &["stash", "drop", "stash@{0}"][..],
            &["revert", "abc123"][..],
            &["push", "--force"][..],
            &["push", "-f"][..],
            &["push", "--force-with-lease"][..],
        ] {
            assert_eq!(
                classify(&s(args)),
                GitRisk::DangerousWrite,
                "args = {args:?}"
            );
        }
    }

    #[test]
    fn test_plan_risk_consistent_with_classify() {
        let cases: &[&[&str]] = &[
            &["status"],
            &["diff", "--cached"],
            &["log", "-n", "5"],
            &["branch", "-D", "x"],
            &["push"],
            &["push", "--force-with-lease"],
            &["reset", "--hard"],
            &["stash", "drop"],
        ];
        for c in cases {
            let p = ok(c);
            assert_eq!(p.risk, classify(&p.args), "case = {c:?}");
        }
    }

    #[test]
    fn test_parse_remote_url_https() {
        assert_eq!(
            parse_remote_url("https://github.com/octo/repo"),
            Some(("octo".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_remote_url("https://github.com/octo/repo.git"),
            Some(("octo".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_remote_url("https://github.com/octo/repo/"),
            Some(("octo".to_string(), "repo".to_string()))
        );
    }

    #[test]
    fn test_parse_remote_url_ssh_and_git() {
        assert_eq!(
            parse_remote_url("git@github.com:octo/repo.git"),
            Some(("octo".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_remote_url("git@github.com:octo/repo"),
            Some(("octo".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_remote_url("ssh://git@github.com/octo/repo.git"),
            Some(("octo".to_string(), "repo".to_string()))
        );
    }

    #[test]
    fn test_parse_remote_url_invalid() {
        assert_eq!(parse_remote_url(""), None);
        assert_eq!(parse_remote_url("not a url"), None);
        assert_eq!(parse_remote_url("https://github.com/only-owner"), None);
        assert_eq!(
            parse_remote_url("https://github.com/octo/repo/nested"),
            None
        );
        assert_eq!(parse_remote_url("file:///local/path"), None);
    }
}
