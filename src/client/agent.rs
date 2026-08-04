//! Agent executor: executes `AgentCommand`s received over the control channel,
//! sandboxed to the workspace root directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::common::{AgentCommand, AgentResult};

/// 单条命令输出上限（协议 1MB 消息上限内留足余量）
const MAX_OUTPUT: usize = 100 * 1024;

/// 将用户提供的相对路径解析到沙箱根目录内；拒绝逃逸（`..` 上溢、绝对路径）。
///
/// 不做符号链接检查：root_path 本身由服务器侧工作区配置指定（可信），
/// 这里防的是 LLM 生成的路径意外逃逸。
pub fn resolve_sandboxed(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(format!("absolute path not allowed: {rel}"));
    }
    let mut out = root.to_path_buf();
    for comp in rel_path.components() {
        match comp {
            std::path::Component::Normal(c) => out.push(c),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() || !out.starts_with(root) {
                    return Err(format!("path escapes workspace root: {rel}"));
                }
            }
            _ => return Err(format!("unsupported path component in: {rel}")),
        }
    }
    Ok(out)
}

fn truncate_output(s: String) -> String {
    if s.len() <= MAX_OUTPUT {
        return s;
    }
    // 快照到合法字符边界，避免在 UTF-8 多字节序列中间切分导致 panic
    let half = MAX_OUTPUT / 2;
    let mut head_end = half;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - half;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let head = &s[..head_end];
    let tail = &s[tail_start..];
    format!("{head}\n[truncated]\n{tail}")
}

/// 执行一条 AgentCommand，全部在 root_path 沙箱内；永不 panic，错误归一为 AgentResult::Error。
pub async fn handle_exec_request(
    command: &AgentCommand,
    root_path: &Path,
    timeout: Duration,
) -> AgentResult {
    match command {
        AgentCommand::Shell { cmd, cwd } => {
            shell_exec(cmd, cwd.as_deref(), root_path, timeout).await
        }
        AgentCommand::ReadFile { path } => match resolve_sandboxed(root_path, path) {
            Ok(p) => match tokio::fs::read_to_string(&p).await {
                Ok(content) => AgentResult::FileContent {
                    content: truncate_output(content),
                },
                Err(e) => AgentResult::Error {
                    message: format!("read {} failed: {e}", p.display()),
                },
            },
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::WriteFile { path, content } => match resolve_sandboxed(root_path, path) {
            Ok(p) => {
                if let Some(parent) = p.parent() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        return AgentResult::Error {
                            message: format!("mkdir {} failed: {e}", parent.display()),
                        };
                    }
                }
                match tokio::fs::write(&p, content).await {
                    Ok(()) => AgentResult::Success,
                    Err(e) => AgentResult::Error {
                        message: format!("write {} failed: {e}", p.display()),
                    },
                }
            }
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::ListDir { path } => match resolve_sandboxed(root_path, path) {
            Ok(p) => list_dir(&p).await,
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::GitStatus => {
            git_exec(&["status", "--short", "--branch"], root_path, timeout).await
        }
        AgentCommand::GitDiff { path } => {
            let mut args: Vec<String> = vec!["diff".into()];
            if let Some(path) = path {
                match resolve_sandboxed(root_path, path).and_then(|abs| {
                    abs.strip_prefix(root_path)
                        .map(|r| r.to_string_lossy().into_owned())
                        .map_err(|_| format!("path not under root: {path}"))
                }) {
                    Ok(rel) => {
                        args.push("--".into());
                        args.push(rel);
                    }
                    Err(e) => return AgentResult::Error { message: e },
                }
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            git_exec(&refs, root_path, timeout).await
        }
        AgentCommand::GitCommit { message } => {
            // 先 stage 全部，再 commit；git add 失败立即返回错误
            if let AgentResult::Error { message: err } =
                git_exec(&["add", "-A"], root_path, timeout).await
            {
                return AgentResult::Error { message: err };
            }
            git_exec(&["commit", "-m", message], root_path, timeout).await
        }
        AgentCommand::GitPush => git_exec(&["push"], root_path, timeout).await,
    }
}

async fn shell_exec(
    cmd: &str,
    cwd: Option<&str>,
    root_path: &Path,
    timeout: Duration,
) -> AgentResult {
    let workdir = match cwd {
        Some(c) => match resolve_sandboxed(root_path, c) {
            Ok(p) => p,
            Err(e) => return AgentResult::Error { message: e },
        },
        None => root_path.to_path_buf(),
    };
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(&workdir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(out)) => AgentResult::Shell {
            stdout: truncate_output(String::from_utf8_lossy(&out.stdout).into_owned()),
            stderr: truncate_output(String::from_utf8_lossy(&out.stderr).into_owned()),
            exit_code: out.status.code().unwrap_or(-1),
        },
        Ok(Err(e)) => AgentResult::Error {
            message: format!("spawn shell failed: {e}"),
        },
        Err(_) => AgentResult::Error {
            message: format!("command timed out after {}s", timeout.as_secs()),
        },
    }
}

async fn list_dir(path: &Path) -> AgentResult {
    let mut entries = match tokio::fs::read_dir(path).await {
        Ok(rd) => rd,
        Err(e) => {
            return AgentResult::Error {
                message: format!("list {} failed: {e}", path.display()),
            };
        }
    };
    let mut lines = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        lines.push(if is_dir { format!("{name}/") } else { name });
    }
    lines.sort();
    AgentResult::FileContent {
        content: lines.join("\n"),
    }
}

/// 在沙箱内执行一条 git 命令；成功时返回 stdout（为空则返回 stderr）作为
/// FileContent，非零退出码归一为 Error。输出同样受 MAX_OUTPUT 截断。
async fn git_exec(args: &[&str], root_path: &Path, timeout: Duration) -> AgentResult {
    let child = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            if out.status.success() {
                AgentResult::FileContent {
                    content: truncate_output(if stdout.is_empty() { stderr } else { stdout }),
                }
            } else {
                AgentResult::Error {
                    message: truncate_output(format!("git {:?} failed: {stderr}", args)),
                }
            }
        }
        Ok(Err(e)) => AgentResult::Error {
            message: format!("spawn git failed: {e}"),
        },
        Err(_) => AgentResult::Error {
            message: format!("git command timed out after {}s", timeout.as_secs()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_resolves_normal_path() {
        let root = Path::new("/workspace");
        let p = resolve_sandboxed(root, "src/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/workspace/src/main.rs"));
    }

    #[test]
    fn test_sandbox_rejects_parent_escape() {
        let root = Path::new("/workspace");
        assert!(resolve_sandboxed(root, "../etc/passwd").is_err());
        assert!(resolve_sandboxed(root, "src/../../etc/passwd").is_err());
        assert!(resolve_sandboxed(root, "/etc/passwd").is_err());
    }

    #[test]
    fn test_sandbox_allows_dot_and_subdir() {
        let root = Path::new("/workspace");
        assert!(resolve_sandboxed(root, ".").is_ok());
        assert!(resolve_sandboxed(root, "a/./b").is_ok());
    }

    #[tokio::test]
    async fn test_shell_executes_in_root() {
        let dir = tempfile::tempdir().unwrap();
        let result = handle_exec_request(
            &AgentCommand::Shell {
                cmd: "pwd".into(),
                cwd: None,
            },
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        match result {
            AgentResult::Shell {
                stdout, exit_code, ..
            } => {
                assert_eq!(exit_code, 0);
                // macOS /tmp symlink: compare canonicalized
                let got = PathBuf::from(stdout.trim());
                let want = dir.path().canonicalize().unwrap();
                assert_eq!(got.canonicalize().unwrap(), want);
            }
            other => panic!("expected Shell result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_shell_captures_exit_code_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let result = handle_exec_request(
            &AgentCommand::Shell {
                cmd: "echo oops 1>&2; exit 3".into(),
                cwd: None,
            },
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        match result {
            AgentResult::Shell {
                stderr, exit_code, ..
            } => {
                assert_eq!(exit_code, 3);
                assert!(stderr.contains("oops"));
            }
            other => panic!("expected Shell result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_write_and_read_file() {
        let dir = tempfile::tempdir().unwrap();
        let wr = handle_exec_request(
            &AgentCommand::WriteFile {
                path: "hello.txt".into(),
                content: "hi there".into(),
            },
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(wr, AgentResult::Success));

        let rd = handle_exec_request(
            &AgentCommand::ReadFile {
                path: "hello.txt".into(),
            },
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        match rd {
            AgentResult::FileContent { content } => assert_eq!(content, "hi there"),
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_read_file_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let rd = handle_exec_request(
            &AgentCommand::ReadFile {
                path: "../outside.txt".into(),
            },
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(rd, AgentResult::Error { .. }));
    }

    #[tokio::test]
    async fn test_list_dir_returns_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let rd = handle_exec_request(
            &AgentCommand::ListDir { path: ".".into() },
            dir.path(),
            Duration::from_secs(5),
        )
        .await;
        match rd {
            AgentResult::FileContent { content } => {
                assert!(content.contains("a.txt"));
                assert!(content.contains("sub/"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_shell_timeout_kills() {
        let dir = tempfile::tempdir().unwrap();
        let result = handle_exec_request(
            &AgentCommand::Shell {
                cmd: "sleep 30".into(),
                cwd: None,
            },
            dir.path(),
            Duration::from_millis(200),
        )
        .await;
        match result {
            AgentResult::Error { message } => assert!(message.contains("timed out")),
            other => panic!("expected timeout Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_output_truncated_at_100kb() {
        let dir = tempfile::tempdir().unwrap();
        // 生成 200KB 输出
        let result = handle_exec_request(
            &AgentCommand::Shell {
                cmd: "head -c 204800 /dev/zero | tr '\\0' 'A'".into(),
                cwd: None,
            },
            dir.path(),
            Duration::from_secs(10),
        )
        .await;
        match result {
            AgentResult::Shell { stdout, .. } => {
                assert!(stdout.len() <= 110_000, "stdout len = {}", stdout.len());
                assert!(stdout.contains("[truncated]"));
            }
            other => panic!("expected Shell result, got {other:?}"),
        }
    }

    #[test]
    fn test_truncate_output_multibyte_utf8() {
        // 102,402B of '汉' (3 bytes each), just over MAX_OUTPUT: head cut at
        // byte 51200 (51200 % 3 == 2) and tail cut at byte 51202 (51202 % 3 == 1)
        // both land mid-char, exercising the char-boundary snapping
        let s = "汉".repeat(34_134);
        let out = truncate_output(s);
        assert!(out.contains("[truncated]"));
        // must not panic and must stay valid UTF-8 (String guarantees this if no panic)
        assert!(out.len() <= MAX_OUTPUT + 64);
    }

    #[test]
    fn test_truncate_output_short_unchanged() {
        let s = "hello".to_string();
        assert_eq!(truncate_output(s.clone()), s);
    }

    fn init_git_repo(dir: &Path) {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
        };
        assert!(run(&["init"]).status.success());
        assert!(run(&["config", "user.name", "t"]).status.success());
        assert!(run(&["config", "user.email", "t@t"]).status.success());
    }

    #[tokio::test]
    async fn test_git_status_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let status = handle_exec_request(
            &AgentCommand::GitStatus,
            dir.path(),
            Duration::from_secs(10),
        )
        .await;
        match status {
            AgentResult::FileContent { content } => assert!(content.contains("a.txt")),
            other => panic!("expected FileContent, got {other:?}"),
        }

        let commit = handle_exec_request(
            &AgentCommand::GitCommit {
                message: "add a".into(),
            },
            dir.path(),
            Duration::from_secs(10),
        )
        .await;
        match commit {
            AgentResult::FileContent { content } => assert!(content.contains("add a")),
            other => panic!("expected FileContent, got {other:?}"),
        }

        // 提交后工作区干净
        let status = handle_exec_request(
            &AgentCommand::GitStatus,
            dir.path(),
            Duration::from_secs(10),
        )
        .await;
        match status {
            AgentResult::FileContent { content } => assert!(!content.contains("a.txt")),
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_git_diff() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "v1").unwrap();
        handle_exec_request(
            &AgentCommand::GitCommit {
                message: "v1".into(),
            },
            dir.path(),
            Duration::from_secs(10),
        )
        .await;

        std::fs::write(dir.path().join("a.txt"), "v2").unwrap();
        let diff = handle_exec_request(
            &AgentCommand::GitDiff { path: None },
            dir.path(),
            Duration::from_secs(10),
        )
        .await;
        match diff {
            AgentResult::FileContent { content } => {
                assert!(content.contains("v1"));
                assert!(content.contains("v2"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_git_not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let result = handle_exec_request(
            &AgentCommand::GitStatus,
            dir.path(),
            Duration::from_secs(10),
        )
        .await;
        assert!(matches!(result, AgentResult::Error { .. }));
    }
}
