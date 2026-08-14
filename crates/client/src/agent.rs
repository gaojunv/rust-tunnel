//! Agent executor: executes `AgentCommand`s received over the control channel,
//! sandboxed to the workspace root directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::oneshot;

use rust_tunnel_common::{AgentCommand, AgentResult};

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

/// search 命中条数上限
const SEARCH_MAX_HITS: usize = 200;
/// search 单行内容截断长度（字符）
const SEARCH_MAX_LINE: usize = 500;

/// 将 `grep ... | head -N+1` 的 `Shell` 结果（host/docker 统一分支）转换为
/// `FileContent` 形态。判定基于 stdout/stderr：
/// 因 sh 管道 exit code 取末元素 head（恒为 0），grep 的 exit 1（无命中）/exit 2
/// （错误）均被掩盖，故判定按以下优先级：
/// 1. stderr 非空（无论 exit_code）→ Error 保留 stderr（grep 错误经 stderr 暴露）；
/// 2. exit_code != 0 且 stderr 为空 → 保守 Error（保留 exit_code 信息）；
/// 3. exit 0 且 stdout 为空 → no matches；
/// 4. exit 0 且有 stdout → 命中行。
///
/// 命中行数超过 `SEARCH_MAX_HITS` 时只保留前 N 行并追加截断标记
/// （`head -N+1` 预取一行以检测超限）；单行超过 `SEARCH_MAX_LINE` 字符时截断到
/// 前 500 字符。非 `Shell` 结果（如 spawn 失败）原样透传。
fn grep_search_result(pattern: &str, shell_result: AgentResult) -> AgentResult {
    match shell_result {
        AgentResult::Shell {
            stdout,
            stderr,
            exit_code,
        } => {
            let stderr = stderr.trim();
            if !stderr.is_empty() {
                return AgentResult::Error {
                    message: format!("grep failed: {stderr}"),
                };
            }
            if exit_code != 0 {
                return AgentResult::Error {
                    message: format!("grep failed with exit code {exit_code}"),
                };
            }
            if stdout.trim().is_empty() {
                return AgentResult::FileContent {
                    content: format!("no matches for '{pattern}'"),
                };
            }
            let mut lines: Vec<&str> = stdout.lines().collect();
            let truncated = lines.len() > SEARCH_MAX_HITS;
            if truncated {
                lines.truncate(SEARCH_MAX_HITS);
            }
            let mut content = lines
                .into_iter()
                .map(|line| {
                    if line.chars().count() > SEARCH_MAX_LINE {
                        line.chars().take(SEARCH_MAX_LINE).collect::<String>()
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if truncated {
                content.push_str(&format!("\n[truncated at {SEARCH_MAX_HITS} hits]"));
            }
            AgentResult::FileContent {
                content: truncate_output(content),
            }
        }
        other => other,
    }
}

/// 锚点字符串替换：old_string 恰好出现一次才替换。
async fn patch_file_host(abs: &Path, old_string: &str, new_string: &str) -> AgentResult {
    if old_string.is_empty() {
        return AgentResult::Error {
            message: "old_string must not be empty".into(),
        };
    }
    let content = match tokio::fs::read_to_string(abs).await {
        Ok(c) => c,
        Err(e) => {
            return AgentResult::Error {
                message: format!("read {} failed: {e}", abs.display()),
            }
        }
    };
    let count = content.matches(old_string).count();
    match count {
        0 => AgentResult::Error {
            message: format!("old_string not found in {}", abs.display()),
        },
        1 => {
            let updated = content.replacen(old_string, new_string, 1);
            match tokio::fs::write(abs, updated).await {
                Ok(()) => AgentResult::Success,
                Err(e) => AgentResult::Error {
                    message: format!("write {} failed: {e}", abs.display()),
                },
            }
        }
        n => AgentResult::Error {
            message: format!(
                "old_string matches {n} times in {}; provide more context to make it unique",
                abs.display()
            ),
        },
    }
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
#[derive(Debug)]
struct CmdOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// 通过宿主 `sh -c <cmd>` 执行一条命令。`cwd` 为宿主工作目录；docker 模式下传
/// `None`（工作目录交给 `docker exec -w` 处理）。`stdin_data` 为 `Some` 时经 stdin
/// 管道写入子进程。`cancel_rx` 为 `Some` 时支持中途取消：进程以进程组方式 spawn，
/// 取消或超时都 SIGKILL 整个进程组（含孙进程，避免 `sh` 被杀而 `cargo build` 成孤儿）。
async fn run_host(
    cmd: &str,
    cwd: Option<&Path>,
    stdin_data: Option<&str>,
    timeout: Duration,
    cancel_rx: Option<&mut oneshot::Receiver<()>>,
) -> Result<CmdOutput, String> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        command.process_group(0); // 子进程成为进程组组长，pid == pgid
    }
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

    // 进程组组长 pid（process_group(0) 使组长 pid == pgid）。必须在 wait() 收割前
    // 捕获：`Child::id()` 收割后返回 None，而 drain 超时路径（child 已退出、孙进程
    // 仍持管道写端）仍需 kill 整个进程组。
    #[cfg(unix)]
    let child_pid = child.id();

    // stdin 写入 task（独立于 wait 运行，避免读写互相阻塞）。
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

    // stdout/stderr 读 task：各自持有管道并把输出搬进独立 Vec，child 只被 wait
    // （保留 kill 能力）。wait 结束后 join 回收读出的字节。
    // 边读边限：累计达 MAX_OUTPUT + 1 即停读（丢弃后续字节），避免大输出
    // （如 `cat /dev/zero`）在超时窗口内无限累积 OOM。停读后管道读端随 task
    // 结束关闭，子进程后续写入收到 EPIPE（或由下方 deadline 杀进程组兜底）。
    let stdout_reader = child.stdout.take().map(|mut so| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::with_capacity(MAX_OUTPUT + 1);
            let mut buf = [0u8; 8192];
            loop {
                if out.len() > MAX_OUTPUT {
                    break;
                }
                match so.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                }
            }
            out
        })
    });
    let stderr_reader = child.stderr.take().map(|mut se| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::with_capacity(MAX_OUTPUT + 1);
            let mut buf = [0u8; 8192];
            loop {
                if out.len() > MAX_OUTPUT {
                    break;
                }
                match se.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                }
            }
            out
        })
    });

    // 进程组 kill：取消/超时统一走这里。与 process_group(0) 一致，仅 unix 可用。
    // 取 spawn 时捕获的 child_pid（wait 收割后 Child::id() 为 None）。
    #[cfg(unix)]
    fn kill_group(pid: Option<u32>) {
        if let Some(pid) = pid {
            // SAFETY: 只 kill 本次 spawn 建立的进程组，pid 为组长。
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }

    // wait + stdout/stderr drain 纳入同一 deadline：`sh -c 'nohup server &'` 这类
    // 后台化孙进程继承管道时，sh 虽秒退，读 task 的 join 也不能拖过 timeout。
    let deadline = std::time::Instant::now() + timeout;
    let deadline_ts = tokio::time::Instant::from_std(deadline);

    let outcome = if let Some(cr) = cancel_rx {
        tokio::select! {
            status = child.wait() => status.map_err(|e| format!("wait failed: {e}")),
            _ = cr => Err("command cancelled".to_string()),
            _ = tokio::time::sleep_until(deadline_ts) => Err(format!("command timed out after {}s", timeout.as_secs())),
        }
    } else {
        match tokio::time::timeout_at(deadline_ts, child.wait()).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(e)) => Err(format!("wait failed: {e}")),
            Err(_) => Err(format!("command timed out after {}s", timeout.as_secs())),
        }
    };

    let output = match outcome {
        Ok(status) => {
            if let Some(w) = writer {
                let _ = w.await;
            }
            // drain 同样受 deadline 约束：孙进程继承 stdout/stderr 写端时，
            // join 到点即终止，恢复旧 wait_with_output 的"子进程退出 + 管道
            // 排空"整体纳入超时的语义。
            let drain = async {
                let stdout = match stdout_reader {
                    Some(h) => h.await.unwrap_or_default(),
                    None => Vec::new(),
                };
                let stderr = match stderr_reader {
                    Some(h) => h.await.unwrap_or_default(),
                    None => Vec::new(),
                };
                (stdout, stderr)
            };
            match tokio::time::timeout_at(deadline_ts, drain).await {
                Ok((stdout, stderr)) => CmdOutput {
                    stdout: truncate_output(String::from_utf8_lossy(&stdout).into_owned()),
                    stderr: truncate_output(String::from_utf8_lossy(&stderr).into_owned()),
                    exit_code: status.code().unwrap_or(-1),
                },
                Err(_) => {
                    // writer 已在 drain 前 await 过（child 退出即 EPIPE 收尾），无需再等
                    #[cfg(unix)]
                    {
                        kill_group(child_pid);
                    }
                    let _ = child.wait().await; // 收割（tokio 缓存状态，立即返回）
                    return Err(format!("command timed out after {}s", timeout.as_secs()));
                }
            }
        }
        Err(msg) => {
            #[cfg(unix)]
            {
                kill_group(child_pid);
            }
            let _ = child.wait().await; // 收割僵尸
            if let Some(w) = writer {
                let _ = w.await;
            }
            return Err(msg);
        }
    };
    Ok(output)
}

/// 执行一条 AgentCommand，全部在 root_path 沙箱内；永不 panic，错误归一为
/// AgentResult::Error。`docker_container = Some(c)` 时所有命令经 `docker exec`
/// 在容器 `c` 内执行（root_path 为容器内路径）；`None` 时直接在宿主机执行。
///
/// 取消边界：仅 shell/search 可取消（走 `cancel_rx` 进程组 kill），其余命令
/// （read/write/list/git）收到 AgentExecCancel 静默忽略、按原超时跑完。
pub async fn handle_exec_request(
    command: &AgentCommand,
    root_path: &Path,
    timeout: Duration,
    docker_container: Option<&str>,
    cancel_rx: Option<&mut oneshot::Receiver<()>>,
) -> AgentResult {
    match command {
        AgentCommand::Shell { cmd, cwd } => {
            shell_exec(
                cmd,
                cwd.as_deref(),
                root_path,
                docker_container,
                timeout,
                cancel_rx,
            )
            .await
        }
        AgentCommand::ReadFile { path } => match resolve_sandboxed(root_path, path) {
            Ok(p) => match docker_container {
                Some(c) => docker_read_file(c, &p, timeout).await,
                None => read_file_host(&p).await,
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
        AgentCommand::Search {
            pattern,
            path,
            include,
        } => {
            if pattern.is_empty() {
                return AgentResult::Error {
                    message: "pattern must not be empty".into(),
                };
            }
            let workdir = match resolve_sandboxed(root_path, path) {
                Ok(p) => p,
                Err(e) => return AgentResult::Error { message: e },
            };
            let include_arg = include
                .as_deref()
                .map(|g| format!("--include={}", sh_quote(g)))
                .unwrap_or_default();
            // host 与 docker 统一走 grep ERE：-E 正则、-rn 带行号、-I 跳过二进制、
            // --exclude-dir=.git 跳过版本库。head 取 N+1 行预检超限，grep_search_result
            // 裁剪并追加截断标记。docker 分支由 shell_exec 的 container 参数加 docker exec。
            let cmd = format!(
                "grep -rnE -I --exclude-dir=.git {include_arg} -- {} . | head -{}",
                sh_quote(pattern),
                SEARCH_MAX_HITS + 1,
            );
            let shell_result =
                shell_exec(&cmd, None, &workdir, docker_container, timeout, cancel_rx).await;
            grep_search_result(pattern, shell_result)
        }
        AgentCommand::PatchFile {
            path,
            old_string,
            new_string,
        } => match resolve_sandboxed(root_path, path) {
            Ok(abs) => match docker_container {
                // docker：读全文 → 本地匹配替换 → 写回（复用既有原语）
                Some(c) => {
                    let content = match docker_read_file(c, &abs, timeout).await {
                        AgentResult::FileContent { content } => content,
                        other => return other,
                    };
                    if old_string.is_empty() {
                        return AgentResult::Error {
                            message: "old_string must not be empty".into(),
                        };
                    }
                    match content.matches(old_string.as_str()).count() {
                        0 => AgentResult::Error {
                            message: format!("old_string not found in {path}"),
                        },
                        1 => {
                            let updated = content.replacen(old_string, new_string, 1);
                            docker_write_file(c, &abs, &updated, timeout).await
                        }
                        n => AgentResult::Error {
                            message: format!(
                                "old_string matches {n} times in {path}; provide more context to make it unique"
                            ),
                        },
                    }
                }
                None => patch_file_host(&abs, old_string, new_string).await,
            },
            Err(e) => AgentResult::Error { message: e },
        },
    }
}

async fn shell_exec(
    cmd: &str,
    cwd: Option<&str>,
    root_path: &Path,
    docker_container: Option<&str>,
    timeout: Duration,
    cancel_rx: Option<&mut oneshot::Receiver<()>>,
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
    match run_host(&host_cmd, host_cwd, None, timeout, cancel_rx).await {
        Ok(out) => AgentResult::Shell {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.exit_code,
        },
        Err(message) => AgentResult::Error { message },
    }
}

/// list_dir 单次返回条目上限：超出只保留前 N 条并在末尾标记真实总数，
/// 避免海量目录（node_modules/target）join 后撑爆 1MB 控制帧上限。
const LIST_DIR_MAX_ENTRIES: usize = 5000;

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
    let mut total = 0usize;
    while let Ok(Some(entry)) = entries.next_entry().await {
        total += 1;
        if lines.len() < LIST_DIR_MAX_ENTRIES {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            lines.push(if is_dir { format!("{name}/") } else { name });
        }
    }
    lines.sort();
    let truncated_entries = total > LIST_DIR_MAX_ENTRIES;
    let mut content = lines.join("\n");
    if truncated_entries {
        content.push_str(&format!("\n[truncated, total {total} entries]"));
    }
    AgentResult::FileContent {
        content: truncate_output(content),
    }
}

/// 宿主模式读取文件：用 `take(MAX_OUTPUT + 1)` 只读上限字节，避免整读大文件
/// （如 5GB 日志）OOM。读满上限即视为截断，复用 `truncate_output` 保留头尾。
/// 无效 UTF-8 按 lossy 替换（与 `run_host` 的管道读取语义一致）。
async fn read_file_host(abs: &Path) -> AgentResult {
    use tokio::io::AsyncReadExt;
    let file = match tokio::fs::File::open(abs).await {
        Ok(f) => f,
        Err(e) => {
            return AgentResult::Error {
                message: format!("read {} failed: {e}", abs.display()),
            };
        }
    };
    let mut buf = Vec::with_capacity(MAX_OUTPUT + 1);
    let n = match file
        .take((MAX_OUTPUT + 1) as u64)
        .read_to_end(&mut buf)
        .await
    {
        Ok(n) => n,
        Err(e) => {
            return AgentResult::Error {
                message: format!("read {} failed: {e}", abs.display()),
            };
        }
    };
    let mut content = String::from_utf8_lossy(&buf).into_owned();
    if n > MAX_OUTPUT {
        content = truncate_output(content);
    }
    AgentResult::FileContent { content }
}

/// 在容器内执行 `cat -- <abs_path>`；非零退出码（如文件不存在）归一为 Error。
async fn docker_read_file(container: &str, abs: &Path, timeout: Duration) -> AgentResult {
    let cmd = format!(
        "docker exec {} cat -- {}",
        sh_quote(container),
        sh_quote(&abs.to_string_lossy()),
    );
    match run_host(&cmd, None, None, timeout, None).await {
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
    match run_host(&cmd, None, Some(content), timeout, None).await {
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
    match run_host(&cmd, None, None, timeout, None).await {
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
///
/// 取消边界：git 命令不进 `exec_cancels` 可取消路径（规格 YAGNI：快速命令不改造），
/// 取消信号（AgentExecCancel）到达时被忽略、命令跑完 ≤120s（exec 超时兜底）。
async fn git_exec(
    args: &[&str],
    root_path: &Path,
    docker_container: Option<&str>,
    timeout: Duration,
) -> AgentResult {
    let output = match docker_container {
        Some(c) => {
            let cmd = docker_git_cmd(c, &root_path.to_string_lossy(), args);
            match run_host(&cmd, None, None, timeout, None).await {
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
    async fn test_list_dir_truncates_many_entries() {
        let dir = tempfile::tempdir().unwrap();
        // 创建 LIST_DIR_MAX_ENTRIES + 1 个文件，验证条数上限截断
        for i in 0..=LIST_DIR_MAX_ENTRIES {
            std::fs::write(dir.path().join(format!("f{i:05}")), "").unwrap();
        }
        let rd = handle_exec_request(
            &AgentCommand::ListDir { path: ".".into() },
            dir.path(),
            Duration::from_secs(10),
            None,
            None,
        )
        .await;
        let AgentResult::FileContent { content } = rd else {
            panic!("expected FileContent, got {rd:?}");
        };
        // 保留前 N 条 + 末尾截断标记（标记真实总数）
        assert!(content.contains(&format!(
            "\n[truncated, total {} entries]",
            LIST_DIR_MAX_ENTRIES + 1
        )));
        assert_eq!(content.lines().count(), LIST_DIR_MAX_ENTRIES + 1);
        assert!(!content.contains("f05000"));
    }

    #[tokio::test]
    async fn test_list_dir_truncates_huge_output_bytes() {
        let dir = tempfile::tempdir().unwrap();
        // 2000 个 ~205 字符长文件名：未超条数上限，但 join 后 > MAX_OUTPUT，
        // 验证字节级截断兜底（头尾保留、内容有界）
        for i in 0..2000 {
            let name = format!("n{i:04}-{}", "x".repeat(200));
            std::fs::write(dir.path().join(&name), "").unwrap();
        }
        let rd = handle_exec_request(
            &AgentCommand::ListDir { path: ".".into() },
            dir.path(),
            Duration::from_secs(10),
            None,
            None,
        )
        .await;
        let AgentResult::FileContent { content } = rd else {
            panic!("expected FileContent, got {rd:?}");
        };
        assert!(content.len() <= MAX_OUTPUT + 64, "len = {}", content.len());
        assert!(content.contains("[truncated]"));
        assert!(content.starts_with("n0000-"));
        assert!(content.ends_with("x"));
    }

    #[tokio::test]
    async fn test_read_file_large_returns_truncated() {
        let dir = tempfile::tempdir().unwrap();
        // 200KB 文件：宿主分支必须只读上限字节并截断，不能整读（防 OOM）
        std::fs::write(dir.path().join("big.log"), "A".repeat(MAX_OUTPUT * 2)).unwrap();
        let rd = handle_exec_request(
            &AgentCommand::ReadFile {
                path: "big.log".into(),
            },
            dir.path(),
            Duration::from_secs(10),
            None,
            None,
        )
        .await;
        let AgentResult::FileContent { content } = rd else {
            panic!("expected FileContent, got {rd:?}");
        };
        assert!(content.len() <= MAX_OUTPUT + 64, "len = {}", content.len());
        assert!(content.contains("[truncated]"));
        // truncate_output 保留头尾
        assert!(content.starts_with("AAAAA"));
        assert!(content.ends_with("AAAAA"));
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
            None,
        )
        .await;
        match result {
            AgentResult::Error { message } => assert!(message.contains("timed out")),
            other => panic!("expected timeout Error, got {other:?}"),
        }
    }

    /// 统计 `ps` 仍可见的 `sleep 30` 进程（用于断言进程组整体被杀）。
    /// 优先 `ps`；若环境无 `ps` 则回退到 `/proc` 扫描各进程 cmdline。
    fn leftover_sleep30_procs() -> Vec<String> {
        match std::process::Command::new("ps")
            .args(["-eo", "pid,pgid,args"])
            .output()
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.contains("sleep 30"))
                .map(str::to_string)
                .collect(),
            Err(_) => {
                // /proc 回退：遍历 PID 目录读 cmdline，拼回整行后判断
                let mut found = Vec::new();
                if let Ok(entries) = std::fs::read_dir("/proc") {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
                            continue;
                        };
                        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                            continue;
                        };
                        let joined = raw
                            .split(|&b| b == 0)
                            .filter(|c| !c.is_empty())
                            .collect::<Vec<_>>()
                            .join(&b' ');
                        if String::from_utf8_lossy(&joined).contains("sleep 30") {
                            found.push(pid.to_string());
                        }
                    }
                }
                found
            }
        }
    }

    #[tokio::test]
    async fn test_run_host_cancel_kills_process_group() {
        use tokio::sync::oneshot;
        let (tx, mut rx) = oneshot::channel();
        // 起一个会生成子进程的 sleep，验证进程组整体被杀
        let handle = tokio::spawn(async move {
            run_host(
                "sleep 30 & wait",
                None,
                None,
                Duration::from_secs(60),
                Some(&mut rx),
            )
            .await
        });
        // 给 spawn + 进程组建立留时间
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = tx.send(());
        let res = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("cancel should not hang")
            .unwrap(); // 移除 JoinError 层，得到 run_host 的 Result
        assert!(res.is_err(), "cancel should yield Err, got {:?}", res);

        // cancel 返回后进程组已整体 SIGKILL；留 500ms 让 kill 生效并等 init 收割
        // 僵尸，再确认孙进程 `sleep 30` 无残留（在 Rust 里过滤 ps 输出，
        // 等价于 `grep "[s]leep 30"` 的避免自匹配技巧）。
        tokio::time::sleep(Duration::from_millis(500)).await;
        let leftover = leftover_sleep30_procs();
        assert!(
            leftover.is_empty(),
            "grandchild `sleep 30` should be killed with the process group, leftover: {leftover:?}"
        );
    }

    #[tokio::test]
    async fn test_run_host_timeout_covers_backgrounded_grandchild() {
        // `sleep 30 &` 让 sh 秒退、孙进程继承 stdout 管道写端：drain 必须受同一
        // deadline 约束并在到点时 kill 进程组，否则 run_host 会挂到孙进程退出
        // （30s）而旁路 timeout。
        let start = std::time::Instant::now();
        let res = run_host("sleep 30 &", None, None, Duration::from_millis(300), None).await;
        let msg = res.expect_err("expected timeout Err");
        assert!(msg.contains("timed out"), "unexpected message: {msg}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "run_host should return at the deadline, elapsed={:?}",
            start.elapsed()
        );
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
            None,
        )
        .await;

        std::fs::write(dir.path().join("a.txt"), "v2").unwrap();
        let diff = handle_exec_request(
            &AgentCommand::GitDiff { path: None },
            dir.path(),
            Duration::from_secs(10),
            None,
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
            None,
        )
        .await;
        assert!(matches!(result, AgentResult::Error { .. }));
    }

    // ── Search / PatchFile ────────────────────────────────────────────────────

    fn temp_workspace(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-test-{:016x}", rand::random::<u64>()));
        for (rel, content) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn test_search_finds_literal_matches() {
        let root = temp_workspace(&[
            ("src/a.rs", "fn main() {}\nfn helper() {}\n"),
            ("src/b.rs", "no match here\n"),
            ("notes.txt", "main entry\n"),
        ]);
        let result = search_exec(&root, "main", "src", Some("*.rs"), None).await;
        let AgentResult::FileContent { content } = result else {
            panic!("expected FileContent");
        };
        // grep 在起始目录内执行，命中路径相对起始目录（与 docker 分支一致）
        assert!(content.contains("a.rs:1:fn main() {}"));
        assert!(!content.contains("notes.txt")); // include 过滤 + 起始目录过滤
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn test_search_skips_binary_and_git() {
        let root = temp_workspace(&[
            (".git/config", "main"),
            ("bin.dat", "main\u{0}binary"),
            ("ok.txt", "main\n"),
        ]);
        // grep -I 跳过二进制、--exclude-dir=.git 跳过版本库
        let result = search_exec(&root, "main", ".", None, None).await;
        let AgentResult::FileContent { content } = result else {
            panic!("expected FileContent");
        };
        assert!(content.contains("ok.txt"));
        assert!(!content.contains("config"));
        assert!(!content.contains("bin.dat"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn test_search_no_match_returns_empty() {
        let root = temp_workspace(&[("a.txt", "hello\n")]);
        let result = search_exec(&root, "zzz", ".", None, None).await;
        let AgentResult::FileContent { content } = result else {
            panic!("expected FileContent");
        };
        assert!(content.contains("no matches"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn test_search_rejects_escaping_path() {
        let root = temp_workspace(&[]);
        let result = search_exec(&root, "x", "../etc", None, None).await;
        assert!(matches!(result, AgentResult::Error { .. }));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn test_search_rejects_empty_pattern_on_both_paths() {
        // 统一前置校验点：docker/host 两条路径在进入分支前都拒绝空 pattern，
        // docker 路径无需真实 docker daemon
        let root = temp_workspace(&[("a.txt", "hello\n")]);
        let cmd = AgentCommand::Search {
            pattern: String::new(),
            path: ".".into(),
            include: None,
        };
        for container in [None, Some("agent-test")] {
            let result =
                handle_exec_request(&cmd, &root, Duration::from_secs(10), container, None).await;
            let AgentResult::Error { message } = result else {
                panic!("expected Error for container = {container:?}");
            };
            assert_eq!(message, "pattern must not be empty");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// 经 handle_exec_request 执行 Search（host/docker 共用路径）。
    async fn search_exec(
        root: &Path,
        pattern: &str,
        path: &str,
        include: Option<&str>,
        container: Option<&str>,
    ) -> AgentResult {
        handle_exec_request(
            &AgentCommand::Search {
                pattern: pattern.into(),
                path: path.into(),
                include: include.map(str::to_string),
            },
            root,
            Duration::from_secs(10),
            container,
            None,
        )
        .await
    }

    #[tokio::test]
    async fn test_search_regex_matches() {
        let root = temp_workspace(&[
            ("src/a.rs", "fn main() {}\nfn helper() {}\nlet x = 1;\n"),
            ("src/b.rs", "not a function\n"),
            ("notes.txt", "fn something() {}\n"),
        ]);
        let result = search_exec(&root, r"fn\s+\w+\(", "src", Some("*.rs"), None).await;
        let AgentResult::FileContent { content } = result else {
            panic!("expected FileContent");
        };
        // ERE 正则命中函数定义行（旧字面量实现搜不到此模式）；
        // 命中路径相对起始目录 src（与 docker 分支一致）
        assert!(content.contains("a.rs:1:fn main() {}"));
        assert!(content.contains("a.rs:2:fn helper() {}"));
        // include 过滤与起始目录过滤
        assert!(!content.contains("b.rs"));
        assert!(!content.contains("notes.txt"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn test_search_literal_still_works() {
        // 无特殊字符的字面量模式是 ERE 子集，行为不变
        let root = temp_workspace(&[
            ("src/a.rs", "fn main() {}\n"),
            ("notes.txt", "main entry\n"),
        ]);
        let result = search_exec(&root, "main", ".", None, None).await;
        let AgentResult::FileContent { content } = result else {
            panic!("expected FileContent");
        };
        assert!(content.contains("src/a.rs:1:fn main() {}"));
        assert!(content.contains("notes.txt:1:main entry"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn test_search_special_chars_need_escape() {
        let root = temp_workspace(&[("a.rs", "main(x) {}\n")]);
        // "main(" 是不闭合的分组，ERE 下 grep 报错 → 归一为 Error
        let bad = search_exec(&root, "main(", ".", None, None).await;
        assert!(matches!(bad, AgentResult::Error { .. }));
        // "main\(" 转义后按字面量命中
        let ok = search_exec(&root, r"main\(", ".", None, None).await;
        let AgentResult::FileContent { content } = ok else {
            panic!("expected FileContent");
        };
        assert!(content.contains("a.rs:1:main(x) {}"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn test_search_invalid_regex_returns_grep_error() {
        let root = temp_workspace(&[("a.rs", "fn main() {}\n")]);
        let result = search_exec(&root, "[unclosed", ".", None, None).await;
        match result {
            AgentResult::Error { message } => {
                assert!(message.contains("grep"), "message = {message:?}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn test_patch_unique_replacement() {
        let root = temp_workspace(&[("a.rs", "fn old() {}\nrest\n")]);
        let result = patch_file_host(&root.join("a.rs"), "fn old()", "fn new()").await;
        assert!(matches!(result, AgentResult::Success));
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "fn new() {}\nrest\n"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn test_patch_not_found_and_ambiguous() {
        let root = temp_workspace(&[("a.rs", "dup\ndup\n")]);
        let r1 = patch_file_host(&root.join("a.rs"), "missing", "x").await;
        let AgentResult::Error { message } = r1 else {
            panic!()
        };
        assert!(message.contains("not found"));

        let r2 = patch_file_host(&root.join("a.rs"), "dup", "x").await;
        let AgentResult::Error { message } = r2 else {
            panic!()
        };
        assert!(message.contains("2 times"));

        let r3 = patch_file_host(&root.join("a.rs"), "", "x").await;
        assert!(matches!(r3, AgentResult::Error { .. }));
        std::fs::remove_dir_all(&root).ok();
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

    #[test]
    fn test_grep_search_result_exit_0_maps_to_file_content() {
        let result = grep_search_result(
            "main",
            AgentResult::Shell {
                stdout: "a.rs:1:fn main()\nb.rs:2:main".into(),
                stderr: String::new(),
                exit_code: 0,
            },
        );
        let AgentResult::FileContent { content } = result else {
            panic!("expected FileContent");
        };
        assert_eq!(content, "a.rs:1:fn main()\nb.rs:2:main");
    }

    #[test]
    fn test_grep_search_result_empty_stdout_is_no_matches() {
        // sh 管道 `grep ... | head -N+1` 的 exit code 取管道末元素 head（恒为 0），
        // grep 的 exit 1（无命中）被掩盖，因此判定基于 stdout/stderr：
        // exit 0 + stdout 为空 → 无命中。
        let result = grep_search_result(
            "zzz",
            AgentResult::Shell {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            },
        );
        let AgentResult::FileContent { content } = result else {
            panic!("expected FileContent");
        };
        assert_eq!(content, "no matches for 'zzz'");
    }

    #[test]
    fn test_grep_search_result_truncates_at_max_hits() {
        // N+1 行输入（模拟 head -N+1 预取）：保留前 N 行并追加与 host 一致的截断标记
        let hits: Vec<String> = (0..=SEARCH_MAX_HITS)
            .map(|i| format!("f{i}.rs:1:hit"))
            .collect();
        let result = grep_search_result(
            "hit",
            AgentResult::Shell {
                stdout: hits.join("\n"),
                stderr: String::new(),
                exit_code: 0,
            },
        );
        let AgentResult::FileContent { content } = result else {
            panic!("expected FileContent");
        };
        assert_eq!(content.lines().count(), SEARCH_MAX_HITS + 1);
        assert!(content.contains("f0.rs:1:hit"));
        assert!(!content.contains(&format!("f{}.rs", SEARCH_MAX_HITS)));
        assert!(content.ends_with(&format!("\n[truncated at {} hits]", SEARCH_MAX_HITS)));
    }

    #[test]
    fn test_grep_search_result_truncates_long_line() {
        // 单行超过 SEARCH_MAX_LINE 时截断到前 500 字符（对齐宿主旧实现语义）
        let long = "x".repeat(SEARCH_MAX_LINE + 100);
        let result = grep_search_result(
            "x",
            AgentResult::Shell {
                stdout: format!("f.rs:1:{long}"),
                stderr: String::new(),
                exit_code: 0,
            },
        );
        let AgentResult::FileContent { content } = result else {
            panic!("expected FileContent");
        };
        let line = content.lines().next().unwrap();
        assert_eq!(line.chars().count(), SEARCH_MAX_LINE);
    }

    #[test]
    fn test_grep_search_result_stderr_yields_error() {
        // sh 管道 exit code 取 head（恒 0），grep 错误（目录不存在/权限拒绝）经
        // stderr 暴露：stderr 非空（无论 exit code 是否 0）→ 返回 Error 保留 stderr。
        let result = grep_search_result(
            "main",
            AgentResult::Shell {
                stdout: "a.rs:1:fn main()".into(),
                stderr: "grep: .: No such file or directory".into(),
                exit_code: 0,
            },
        );
        let AgentResult::Error { message } = result else {
            panic!("expected Error");
        };
        assert!(message.contains("No such file"), "message = {message:?}");
    }

    #[test]
    fn test_grep_search_result_nonzero_exit_empty_stderr_is_error() {
        // 保守回退：exit_code != 0 且 stderr 为空 → Error 并保留 exit_code 信息。
        let result = grep_search_result(
            "main",
            AgentResult::Shell {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 1,
            },
        );
        let AgentResult::Error { message } = result else {
            panic!("expected Error");
        };
        assert!(message.contains("exit code 1"), "message = {message:?}");
    }

    #[test]
    fn test_grep_search_result_passes_through_non_shell() {
        // spawn 失败等 Error 结果原样透传，不做 shell 解释
        let result = grep_search_result(
            "main",
            AgentResult::Error {
                message: "spawn shell failed".into(),
            },
        );
        assert!(matches!(result, AgentResult::Error { .. }));
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        )
        .await;
    }
}
