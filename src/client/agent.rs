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

/// 将一个字符串包成单个 shell 单引号词（POSIX sh）。内嵌 `'` 用 `'\''` 转义，
/// 可安全地嵌在 `sh -c '...'` 的正文中。
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Docker 模式下把 shell 命令翻译为宿主侧等价命令：
/// `docker exec -w <workdir> <container> sh -c '<cmd>'`。
/// `workdir` 是容器内工作目录（root 相对路径经沙箱解析后的容器绝对路径）。
fn docker_shell_cmd(container: &str, workdir: &str, cmd: &str) -> String {
    format!(
        "docker exec -w {} {} sh -c {}",
        sh_quote(workdir),
        sh_quote(container),
        sh_quote(cmd),
    )
}

/// Docker 模式下把 git 命令翻译为宿主侧等价命令：
/// `docker exec -w <root> <container> git <args...>`，每个参数单引号转义。
fn docker_git_cmd(container: &str, root: &str, args: &[&str]) -> String {
    let mut parts = vec![
        "docker exec -w".to_string(),
        sh_quote(root),
        sh_quote(container),
        "git".to_string(),
    ];
    parts.extend(args.iter().map(|a| sh_quote(a)));
    parts.join(" ")
}

/// 单次宿主命令的输出（已按 MAX_OUTPUT 截断）。
struct CmdOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// 通过宿主 `sh -c <cmd>` 执行一条命令。`cwd` 为宿主工作目录；docker 模式下传
/// `None`（工作目录交给 `docker exec -w` 处理，容器路径在宿主机上不一定存在）。
/// `stdin_data` 为 `Some` 时经 stdin 管道写入子进程（用于 docker write_file）。
async fn run_host(
    cmd: &str,
    cwd: Option<&Path>,
    stdin_data: Option<&str>,
    timeout: Duration,
) -> Result<CmdOutput, String> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    if stdin_data.is_some() {
        command.stdin(std::process::Stdio::piped());
    } else {
        command.stdin(std::process::Stdio::null());
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn shell failed: {e}"))?;

    // 写入 stdin 的任务独立于 wait_with_output 运行，避免读写互相阻塞。
    let writer = match (stdin_data, child.stdin.take()) {
        (Some(data), Some(mut si)) => {
            let data = data.to_string();
            Some(tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = si.write_all(data.as_bytes()).await;
                let _ = si.shutdown().await;
            }))
        }
        _ => None,
    };

    let output = tokio::time::timeout(timeout, child.wait_with_output()).await;
    let output = match output {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            if let Some(w) = writer {
                let _ = w.await;
            }
            return Err(format!("spawn shell failed: {e}"));
        }
        Err(_) => {
            if let Some(w) = writer {
                let _ = w.await;
            }
            return Err(format!("command timed out after {}s", timeout.as_secs()));
        }
    };
    if let Some(w) = writer {
        let _ = w.await;
    }

    Ok(CmdOutput {
        stdout: truncate_output(String::from_utf8_lossy(&output.stdout).into_owned()),
        stderr: truncate_output(String::from_utf8_lossy(&output.stderr).into_owned()),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

/// 执行一条 AgentCommand，全部在 root_path 沙箱内；永不 panic，错误归一为
/// AgentResult::Error。`docker_container = Some(c)` 时所有命令经 `docker exec`
/// 在容器 `c` 内执行（root_path 为容器内路径）；`None` 时直接在宿主机执行。
pub async fn handle_exec_request(
    command: &AgentCommand,
    root_path: &Path,
    timeout: Duration,
    docker_container: Option<&str>,
) -> AgentResult {
    match command {
        AgentCommand::Shell { cmd, cwd } => {
            shell_exec(cmd, cwd.as_deref(), root_path, docker_container, timeout).await
        }
        AgentCommand::ReadFile { path } => match resolve_sandboxed(root_path, path) {
            Ok(p) => match docker_container {
                Some(c) => docker_read_file(c, &p, timeout).await,
                None => match tokio::fs::read_to_string(&p).await {
                    Ok(content) => AgentResult::FileContent {
                        content: truncate_output(content),
                    },
                    Err(e) => AgentResult::Error {
                        message: format!("read {} failed: {e}", p.display()),
                    },
                },
            },
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::WriteFile { path, content } => match resolve_sandboxed(root_path, path) {
            Ok(p) => match docker_container {
                Some(c) => docker_write_file(c, &p, content, timeout).await,
                None => {
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
            },
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::ListDir { path } => match resolve_sandboxed(root_path, path) {
            Ok(p) => match docker_container {
                Some(c) => docker_list_dir(c, &p, timeout).await,
                None => list_dir(&p).await,
            },
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::GitStatus => {
            git_exec(
                &["status", "--short", "--branch"],
                root_path,
                docker_container,
                timeout,
            )
            .await
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
            git_exec(&refs, root_path, docker_container, timeout).await
        }
        AgentCommand::GitCommit { message } => {
            // 先 stage 全部，再 commit；git add 失败立即返回错误
            if let AgentResult::Error { message: err } =
                git_exec(&["add", "-A"], root_path, docker_container, timeout).await
            {
                return AgentResult::Error { message: err };
            }
            git_exec(
                &["commit", "-m", message],
                root_path,
                docker_container,
                timeout,
            )
            .await
        }
        AgentCommand::GitPush => git_exec(&["push"], root_path, docker_container, timeout).await,
        AgentCommand::Search { .. } | AgentCommand::PatchFile { .. } => AgentResult::Error {
            message: "not implemented".into(),
        },
    }
}

async fn shell_exec(
    cmd: &str,
    cwd: Option<&str>,
    root_path: &Path,
    docker_container: Option<&str>,
    timeout: Duration,
) -> AgentResult {
    let workdir = match cwd {
        Some(c) => match resolve_sandboxed(root_path, c) {
            Ok(p) => p,
            Err(e) => return AgentResult::Error { message: e },
        },
        None => root_path.to_path_buf(),
    };
    let (host_cmd, host_cwd) = match docker_container {
        Some(c) => (docker_shell_cmd(c, &workdir.to_string_lossy(), cmd), None),
        None => (cmd.to_string(), Some(workdir.as_path())),
    };
    match run_host(&host_cmd, host_cwd, None, timeout).await {
        Ok(out) => AgentResult::Shell {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.exit_code,
        },
        Err(message) => AgentResult::Error { message },
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

/// 在容器内执行 `cat -- <abs_path>`；非零退出码（如文件不存在）归一为 Error。
async fn docker_read_file(container: &str, abs: &Path, timeout: Duration) -> AgentResult {
    let cmd = format!(
        "docker exec {} cat -- {}",
        sh_quote(container),
        sh_quote(&abs.to_string_lossy()),
    );
    match run_host(&cmd, None, None, timeout).await {
        Ok(out) if out.exit_code == 0 => AgentResult::FileContent {
            content: out.stdout,
        },
        Ok(out) => AgentResult::Error {
            message: format!("read {} failed: {}", abs.display(), out.stderr.trim()),
        },
        Err(message) => AgentResult::Error { message },
    }
}

/// 在容器内执行 `sh -c 'cat > <abs_path>'`，内容经 stdin 写入。
/// 注意：docker 模式下不创建父目录（宿主模式会），MVP 约定目标文件父目录已存在。
async fn docker_write_file(
    container: &str,
    abs: &Path,
    content: &str,
    timeout: Duration,
) -> AgentResult {
    let inner = format!("cat > {}", sh_quote(&abs.to_string_lossy()));
    let cmd = format!(
        "docker exec -i {} sh -c {}",
        sh_quote(container),
        sh_quote(&inner)
    );
    match run_host(&cmd, None, Some(content), timeout).await {
        Ok(out) if out.exit_code == 0 => AgentResult::Success,
        Ok(out) => AgentResult::Error {
            message: format!("write {} failed: {}", abs.display(), out.stderr.trim()),
        },
        Err(message) => AgentResult::Error { message },
    }
}

/// 在容器内执行 `ls -Ap <abs_path>` 并把输出对齐宿主格式（目录带 `/` 后缀、排序）。
async fn docker_list_dir(container: &str, abs: &Path, timeout: Duration) -> AgentResult {
    let cmd = format!(
        "docker exec {} ls -Ap {}",
        sh_quote(container),
        sh_quote(&abs.to_string_lossy()),
    );
    match run_host(&cmd, None, None, timeout).await {
        Ok(out) if out.exit_code == 0 => {
            let mut lines: Vec<String> = out.stdout.lines().map(str::to_string).collect();
            lines.sort();
            AgentResult::FileContent {
                content: lines.join("\n"),
            }
        }
        Ok(out) => AgentResult::Error {
            message: format!("list {} failed: {}", abs.display(), out.stderr.trim()),
        },
        Err(message) => AgentResult::Error { message },
    }
}

/// 在沙箱内执行一条 git 命令；成功时返回 stdout（为空则返回 stderr）作为
/// FileContent，非零退出码归一为 Error。输出同样受 MAX_OUTPUT 截断。
/// `docker_container = Some(c)` 时改为在容器 `c` 内执行（root_path 为容器内路径）。
async fn git_exec(
    args: &[&str],
    root_path: &Path,
    docker_container: Option<&str>,
    timeout: Duration,
) -> AgentResult {
    let output = match docker_container {
        Some(c) => {
            let cmd = docker_git_cmd(c, &root_path.to_string_lossy(), args);
            match run_host(&cmd, None, None, timeout).await {
                Ok(out) => out,
                Err(message) => return AgentResult::Error { message },
            }
        }
        None => {
            let child = tokio::process::Command::new("git")
                .args(args)
                .current_dir(root_path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .output();
            match tokio::time::timeout(timeout, child).await {
                Ok(Ok(out)) => CmdOutput {
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    exit_code: out.status.code().unwrap_or(-1),
                },
                Ok(Err(e)) => {
                    return AgentResult::Error {
                        message: format!("spawn git failed: {e}"),
                    };
                }
                Err(_) => {
                    return AgentResult::Error {
                        message: format!("git command timed out after {}s", timeout.as_secs()),
                    };
                }
            }
        }
    };
    if output.exit_code == 0 {
        AgentResult::FileContent {
            content: truncate_output(if output.stdout.is_empty() {
                output.stderr
            } else {
                output.stdout
            }),
        }
    } else {
        AgentResult::Error {
            message: truncate_output(format!("git {:?} failed: {}", args, output.stderr)),
        }
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
            None,
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
            None,
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
            None,
        )
        .await;
        assert!(matches!(wr, AgentResult::Success));

        let rd = handle_exec_request(
            &AgentCommand::ReadFile {
                path: "hello.txt".into(),
            },
            dir.path(),
            Duration::from_secs(5),
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        )
        .await;

        std::fs::write(dir.path().join("a.txt"), "v2").unwrap();
        let diff = handle_exec_request(
            &AgentCommand::GitDiff { path: None },
            dir.path(),
            Duration::from_secs(10),
            None,
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
            None,
        )
        .await;
        assert!(matches!(result, AgentResult::Error { .. }));
    }

    // ── Docker 命令翻译（纯函数，无需 docker daemon）─────────────────────────

    #[test]
    fn test_sh_quote_wraps_and_escapes() {
        assert_eq!(sh_quote("echo hi"), "'echo hi'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("a'b'c"), "'a'\\''b'\\''c'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn test_docker_shell_cmd_translation() {
        let cmd = docker_shell_cmd("dev-ctr", "/workspace", "echo in-docker");
        assert_eq!(
            cmd,
            "docker exec -w '/workspace' 'dev-ctr' sh -c 'echo in-docker'"
        );
        // 命令含单引号时需转义，保证内层 sh -c 仍收到原始命令
        let cmd = docker_shell_cmd("dev-ctr", "/workspace", "echo it's");
        assert_eq!(
            cmd,
            "docker exec -w '/workspace' 'dev-ctr' sh -c 'echo it'\\''s'"
        );
        // 工作目录含空格
        let cmd = docker_shell_cmd("dev-ctr", "/my work dir", "pwd");
        assert_eq!(cmd, "docker exec -w '/my work dir' 'dev-ctr' sh -c 'pwd'");
    }

    #[test]
    fn test_docker_git_cmd_translation() {
        let cmd = docker_git_cmd("dev-ctr", "/workspace", &["status", "--short"]);
        assert_eq!(
            cmd,
            "docker exec -w '/workspace' 'dev-ctr' git 'status' '--short'"
        );
        // 路径参数含空格
        let cmd = docker_git_cmd("dev-ctr", "/workspace", &["diff", "--", "my file.rs"]);
        assert_eq!(
            cmd,
            "docker exec -w '/workspace' 'dev-ctr' git 'diff' '--' 'my file.rs'"
        );
    }

    // ── Docker 集成测试（需本地 docker daemon）────────────────────────────────

    #[tokio::test]
    #[ignore = "requires local docker daemon"]
    async fn test_docker_shell() {
        // 前置：docker run -d --name agent-test alpine sleep 3600
        // root_path 在 docker 模式下是容器内路径（仅用于沙箱解析，不访问宿主文件系统）
        let root = Path::new("/tmp/agent-docker-root");
        let result = handle_exec_request(
            &AgentCommand::Shell {
                cmd: "echo in-docker".into(),
                cwd: None,
            },
            root,
            Duration::from_secs(10),
            Some("agent-test"),
        )
        .await;
        match result {
            AgentResult::Shell {
                stdout, exit_code, ..
            } => {
                assert_eq!(exit_code, 0);
                assert!(stdout.contains("in-docker"), "stdout = {stdout:?}");
            }
            other => panic!("expected Shell result, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "requires local docker daemon"]
    async fn test_docker_read_write_list() {
        // 前置：docker run -d --name agent-test alpine sleep 3600
        // root_path 在 docker 模式下是容器内路径，宿主机上不存在也无妨
        let root = Path::new("/tmp/agent-docker-root");
        let wr = handle_exec_request(
            &AgentCommand::WriteFile {
                path: "sub/hello.txt".into(),
                content: "docker hi".into(),
            },
            root,
            Duration::from_secs(10),
            Some("agent-test"),
        )
        .await;
        // 父目录不存在 → cat 失败，返回 Error（MVP 行为，文档中已注明）
        assert!(matches!(wr, AgentResult::Error { .. }));

        // 先建目录再写，应当成功
        let mk = handle_exec_request(
            &AgentCommand::Shell {
                cmd: "mkdir -p /tmp/agent-docker-root/sub".into(),
                cwd: None,
            },
            root,
            Duration::from_secs(10),
            Some("agent-test"),
        )
        .await;
        assert!(matches!(mk, AgentResult::Shell { exit_code: 0, .. }));

        let wr = handle_exec_request(
            &AgentCommand::WriteFile {
                path: "sub/hello.txt".into(),
                content: "docker hi".into(),
            },
            root,
            Duration::from_secs(10),
            Some("agent-test"),
        )
        .await;
        assert!(matches!(wr, AgentResult::Success));

        let rd = handle_exec_request(
            &AgentCommand::ReadFile {
                path: "sub/hello.txt".into(),
            },
            root,
            Duration::from_secs(10),
            Some("agent-test"),
        )
        .await;
        match rd {
            AgentResult::FileContent { content } => assert_eq!(content, "docker hi"),
            other => panic!("expected FileContent, got {other:?}"),
        }

        let ls = handle_exec_request(
            &AgentCommand::ListDir { path: ".".into() },
            root,
            Duration::from_secs(10),
            Some("agent-test"),
        )
        .await;
        match ls {
            AgentResult::FileContent { content } => {
                assert!(content.contains("sub/"), "content = {content:?}");
            }
            other => panic!("expected FileContent, got {other:?}"),
        }

        // 清理
        let _ = handle_exec_request(
            &AgentCommand::Shell {
                cmd: "rm -rf /tmp/agent-docker-root".into(),
                cwd: None,
            },
            root,
            Duration::from_secs(10),
            Some("agent-test"),
        )
        .await;
    }
}
