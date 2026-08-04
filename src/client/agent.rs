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
    let head = &s[..MAX_OUTPUT / 2];
    let tail = &s[s.len() - MAX_OUTPUT / 2..];
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
        // Git 系列在 Task 10 实现，先返回不支持
        AgentCommand::GitStatus
        | AgentCommand::GitDiff { .. }
        | AgentCommand::GitCommit { .. }
        | AgentCommand::GitPush => AgentResult::Error {
            message: "git commands not yet supported".into(),
        },
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
}
