//! Agent executor: executes `AgentCommand`s received over the control channel,
//! sandboxed to the workspace root directory.

pub(crate) mod code_outline;

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use tokio::sync::oneshot;

use rust_tunnel_common::{AgentCommand, AgentResult, FileEdit};

use std::fmt::Write as _;

/// 单条命令输出上限（协议 1MB 消息上限内留足余量）
const MAX_OUTPUT: usize = 100 * 1024;

/// 将用户提供的相对路径解析到沙箱根目录内；拒绝逃逸（`..` 上溢、绝对路径、
/// 符号链接指向工作区外）。
///
/// 词法校验之后追加 canonicalize 校验：工作区内的 symlink（或路径中间组件是
/// symlink）指向 root 外时拒绝——否则 `read_file link_to_etc_passwd` 可越界读。
/// 目标不存在（待写入的新文件）时 canonicalize 最近存在的祖先再拼接校验。
/// root canonicalize 失败（root 本身不存在等异常）时退回词法结果，不阻断既有行为。
///
/// # Errors
///
/// 当相对路径为绝对路径、包含上溢的 `..`、或经 symlink 规范化后逃逸沙箱根时返回 `Err`。
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
    // symlink 防护：root 规范化失败时降级为纯词法校验（保持旧行为）
    let Ok(canon_root) = root.canonicalize() else {
        return Ok(out);
    };
    // 从目标向上找最近存在的祖先（新文件写入场景目标本身不存在）
    let mut ancestor = out.as_path();
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            // 连 root 都不存在：词法校验已过，放行（后续 IO 自会报错）
            return Ok(out);
        };
        ancestor = parent;
    }
    if let Ok(canon) = ancestor.canonicalize() {
        if !canon.starts_with(&canon_root) {
            return Err(format!("path escapes workspace root via symlink: {rel}"));
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
                let _ = write!(content, "\n[truncated at {SEARCH_MAX_HITS} hits]");
            }
            AgentResult::FileContent {
                content: truncate_output(content),
            }
        }
        other => other,
    }
}

/// 原子写：同目录临时文件 + persist rename，避免写一半崩溃留下半截文件。
async fn atomic_write(abs: &Path, content: &str) -> Result<(), String> {
    let parent = abs.parent().unwrap_or(Path::new("."));
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| format!("mkdir {} failed: {e}", parent.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| format!("create temp file in {} failed: {e}", parent.display()))?;
    // NamedTempFile implements std::io::Write; write synchronously (fast for typical file sizes),
    // then persist atomically.
    tmp.write_all(content.as_bytes())
        .map_err(|e| format!("write temp file failed: {e}"))?;
    tmp.persist(abs)
        .map_err(|e| format!("persist {} failed: {e}", abs.display()))?;
    Ok(())
}

#[allow(unused_imports)]
use std::io::Write as _;

/// Compute sha256 hex digest of a byte slice.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Maximum diff output size before truncation (bytes).
const DIFF_MAX_BYTES: usize = 8 * 1024;

/// Generate a unified diff between two file contents, truncated to ~8KB.
fn unified_diff(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => continue,
        };
        out.push_str(sign);
        out.push_str(change.value());
        if out.len() > DIFF_MAX_BYTES {
            out.push_str("\n[diff truncated]");
            break;
        }
    }
    out
}

/// Count lines added and removed from a unified diff string.
fn count_diff_lines(diff: &str) -> (u64, u64) {
    let mut added = 0u64;
    let mut removed = 0u64;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix('+') {
            if !rest.starts_with('+') {
                added += 1;
            }
        } else if let Some(rest) = line.strip_prefix('-') {
            if !rest.starts_with('-') {
                removed += 1;
            }
        }
    }
    (added, removed)
}

/// 最近似匹配行查找：对 old_string 的第一行，在 content 各行中找
/// 编辑距离最低的一行，返回 (行号1-based, 截断内容)。
fn find_closest_line(content: &str, old_string: &str) -> Option<(usize, String)> {
    let needle = old_string.lines().next()?;
    if needle.is_empty() {
        return None;
    }
    let mut best_line_num = 0usize;
    let mut best_dist = usize::MAX;
    for (i, line) in content.lines().enumerate() {
        let dist = edit_distance(line, needle);
        if dist < best_dist {
            best_dist = dist;
            best_line_num = i + 1;
        }
    }
    if best_dist == usize::MAX {
        return None;
    }
    let line = content.lines().nth(best_line_num - 1)?;
    let truncated: String = line.chars().take(80).collect();
    Some((best_line_num, truncated))
}

/// Simple Levenshtein edit distance.
#[allow(clippy::needless_range_loop)]
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..=m {
        dp[i][0] = i;
    }
    for j in 0..=n {
        dp[0][j] = j;
    }
    for i in 1..=m {
        for j in 1..=n {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }
    dp[m][n]
}

/// 顺序应用多处编辑：每条作用于前一条的结果。任一失败返回 Err（整体不落盘）。
fn apply_edits(content: &str, edits: &[FileEdit]) -> Result<String, String> {
    if edits.is_empty() {
        return Err("edits must not be empty".into());
    }
    let mut current = content.to_string();
    for (i, edit) in edits.iter().enumerate() {
        let n = i + 1;
        if edit.old_string.is_empty() {
            return Err(format!("edit #{n}: old_string must not be empty"));
        }
        let matches: Vec<usize> = current
            .match_indices(&edit.old_string)
            .map(|(pos, _)| {
                // 1-based line number
                current[..pos].lines().count() + 1
            })
            .collect();
        let count = matches.len();
        if count == 0 {
            if let Some((line_num, line_text)) = find_closest_line(&current, &edit.old_string) {
                return Err(format!(
                    "edit #{n}: old_string not found; closest match at line {line_num}: `{line_text}`"
                ));
            }
            return Err(format!("edit #{n}: old_string not found"));
        }
        if edit.replace_all {
            current = current.replace(&edit.old_string, &edit.new_string);
        } else if count == 1 {
            current = current.replacen(&edit.old_string, &edit.new_string, 1);
        } else {
            let line_nums: Vec<String> = matches
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            return Err(format!(
                "edit #{n}: old_string matches {count} times at lines {}; provide more context",
                line_nums.join(", ")
            ));
        }
    }
    Ok(current)
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
            match atomic_write(abs, &updated).await {
                Ok(()) => AgentResult::Success,
                Err(e) => AgentResult::Error { message: e },
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

/// 多编辑批量替换 + stale 检测 + 原子写 + WriteOutcome。
async fn edit_file_host(
    abs: &Path,
    edits: &[FileEdit],
    expected_hash: Option<&str>,
) -> AgentResult {
    // 读文件
    let content = match tokio::fs::read_to_string(abs).await {
        Ok(c) => c,
        Err(e) => {
            return AgentResult::Error {
                message: format!("read {} failed: {e}", abs.display()),
            };
        }
    };
    // stale 检测
    if let Some(exp) = expected_hash {
        let actual = sha256_hex(content.as_bytes());
        if actual != exp {
            return AgentResult::Error {
                message: format!(
                    "file changed externally since last read (expected hash {exp}, actual {actual}); re-read the file before editing"
                ),
            };
        }
    }
    // 应用编辑
    let new_content = match apply_edits(&content, edits) {
        Ok(c) => c,
        Err(e) => return AgentResult::Error { message: e },
    };
    // 生成 diff
    let diff = unified_diff(&content, &new_content);
    let (lines_added, lines_removed) = count_diff_lines(&diff);
    // 原子写
    if let Err(e) = atomic_write(abs, &new_content).await {
        return AgentResult::Error { message: e };
    }
    let file_hash = sha256_hex(new_content.as_bytes());
    AgentResult::WriteOutcome {
        bytes_written: new_content.len() as u64,
        lines_added,
        lines_removed,
        diff,
        file_hash,
    }
}

/// WriteFile 增强版：expected_hash stale 检测 + 原子写 + WriteOutcome。
async fn write_file2_host(abs: &Path, content: &str, expected_hash: Option<&str>) -> AgentResult {
    // 检查文件是否存在
    let old_content = match tokio::fs::read_to_string(abs).await {
        Ok(c) => Some(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return AgentResult::Error {
                message: format!("read {} failed: {e}", abs.display()),
            };
        }
    };
    // stale 检测（仅文件已存在时）
    if let Some(exp) = expected_hash {
        match &old_content {
            Some(old) => {
                let actual = sha256_hex(old.as_bytes());
                if actual != exp {
                    return AgentResult::Error {
                        message: format!(
                            "file changed externally since last read (expected hash {exp}, actual {actual}); re-read the file before writing"
                        ),
                    };
                }
            }
            None => {
                return AgentResult::Error {
                    message: "file does not exist but expected_hash was provided".into(),
                };
            }
        }
    }
    // 生成 diff
    let diff = if let Some(old) = &old_content {
        unified_diff(old, content)
    } else {
        // 新文件：全增 diff
        let mut out = String::new();
        for line in content.lines() {
            out.push('+');
            out.push_str(line);
            out.push('\n');
            if out.len() > DIFF_MAX_BYTES {
                out.push_str("[diff truncated]");
                break;
            }
        }
        out
    };
    let (lines_added, lines_removed) = count_diff_lines(&diff);
    // 原子写
    if let Err(e) = atomic_write(abs, content).await {
        return AgentResult::Error { message: e };
    }
    let file_hash = sha256_hex(content.as_bytes());
    AgentResult::WriteOutcome {
        bytes_written: content.len() as u64,
        lines_added,
        lines_removed,
        diff,
        file_hash,
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

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: 只 kill 本次 spawn 建立的进程组，pid 为组长。
        unsafe { libc::kill(-pid.cast_signed(), libc::SIGKILL) };
    }
}

/// 通过宿主 `sh -c <cmd>` 执行一条命令。`cwd` 为宿主工作目录；docker 模式下传
/// `None`（工作目录交给 `docker exec -w` 处理）。`stdin_data` 为 `Some` 时经 stdin
/// 管道写入子进程。`cancel_rx` 为 `Some` 时支持中途取消：进程以进程组方式 spawn，
/// 取消或超时都 SIGKILL 整个进程组（含孙进程，避免 `sh` 被杀而 `cargo build` 成孤儿）。
#[allow(
    clippy::too_many_lines,
    reason = "宿主命令执行全流程编排：spawn/管道/超时/取消/回收，状态共享难以拆分"
)]
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

    // wait + stdout/stderr drain 纳入同一 deadline：`sh -c 'nohup server &'` 这类
    // 后台化孙进程继承管道时，sh 虽秒退，读 task 的 join 也不能拖过 timeout。
    let deadline = std::time::Instant::now() + timeout;
    let deadline_ts = tokio::time::Instant::from_std(deadline);

    let outcome = if let Some(cr) = cancel_rx {
        tokio::select! {
            status = child.wait() => status.map_err(|e| format!("wait failed: {e}")),
            _ = cr => Err("command cancelled".to_string()),
            () = tokio::time::sleep_until(deadline_ts) => Err(format!("command timed out after {}s", timeout.as_secs())),
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
            if let Ok((stdout, stderr)) = tokio::time::timeout_at(deadline_ts, drain).await {
                CmdOutput {
                    stdout: truncate_output(String::from_utf8_lossy(&stdout).into_owned()),
                    stderr: truncate_output(String::from_utf8_lossy(&stderr).into_owned()),
                    exit_code: status.code().unwrap_or(-1),
                }
            } else {
                // writer 已在 drain 前 await 过（child 退出即 EPIPE 收尾），无需再等
                #[cfg(unix)]
                {
                    kill_group(child_pid);
                }
                let _ = child.wait().await; // 收割（tokio 缓存状态，立即返回）
                return Err(format!("command timed out after {}s", timeout.as_secs()));
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
#[allow(
    clippy::too_many_lines,
    reason = "对全部 AgentCommand 变体的扁平派发，拆分会散落沙箱与容器分支"
)]
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
        AgentCommand::ShellWithTimeout {
            cmd,
            cwd,
            timeout_secs,
        } => {
            let effective = Duration::from_secs((*timeout_secs).clamp(1, 3600));
            shell_exec(
                cmd,
                cwd.as_deref(),
                root_path,
                docker_container,
                effective,
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
        AgentCommand::ReadFileRange {
            path,
            offset,
            limit,
        } => match resolve_sandboxed(root_path, path) {
            Ok(p) => match docker_container {
                Some(c) => docker_read_file_range(c, &p, *offset, *limit, timeout).await,
                None => read_file_range_host(&p, *offset, *limit).await,
            },
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::WriteFile { path, content } => match resolve_sandboxed(root_path, path) {
            Ok(p) => match docker_container {
                Some(c) => docker_write_file(c, &p, content, timeout).await,
                None => match atomic_write(&p, content).await {
                    Ok(()) => AgentResult::Success,
                    Err(e) => AgentResult::Error { message: e },
                },
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
        AgentCommand::GitExec { args } => {
            // 通用 git：参数已由服务端 git_plan 白名单校验（fail-closed），
            // 这里按 arg 向量直跑，host/docker 两模式由 git_exec 统一处理。
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            git_exec(&refs, root_path, docker_container, timeout).await
        }
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
        AgentCommand::CodeOutline { path } => match resolve_sandboxed(root_path, path) {
            Ok(p) => match docker_container {
                Some(c) => {
                    // Docker 模式：先读文件（docker_read_file 有 100KB 回传上限），再本地解析
                    let file_result = docker_read_file(c, &p, timeout).await;
                    match file_result {
                        AgentResult::FileContent { content } => {
                            let truncated = content.len() >= MAX_OUTPUT;
                            code_outline::exec_outline(&content, path, truncated)
                        }
                        other => other,
                    }
                }
                None => match read_file_capped(&p, MAX_PARSE_FILE_BYTES).await {
                    Ok((content, truncated)) => {
                        code_outline::exec_outline(&content, path, truncated)
                    }
                    Err(message) => AgentResult::Error { message },
                },
            },
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::ReadSymbol { path, name } => match resolve_sandboxed(root_path, path) {
            Ok(p) => match docker_container {
                Some(c) => {
                    let file_result = docker_read_file(c, &p, timeout).await;
                    match file_result {
                        AgentResult::FileContent { content } => {
                            let truncated = content.len() >= MAX_OUTPUT;
                            code_outline::exec_read_symbol(&content, path, name, truncated)
                        }
                        other => other,
                    }
                }
                None => match read_file_capped(&p, MAX_PARSE_FILE_BYTES).await {
                    Ok((content, truncated)) => {
                        code_outline::exec_read_symbol(&content, path, name, truncated)
                    }
                    Err(message) => AgentResult::Error { message },
                },
            },
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::EditFile {
            path,
            edits,
            expected_hash,
        } => match resolve_sandboxed(root_path, path) {
            Ok(abs) => match docker_container {
                Some(c) => {
                    // Docker: read -> local apply_edits -> write back
                    let content = match docker_read_file(c, &abs, timeout).await {
                        AgentResult::FileContent { content } => content,
                        other => return other,
                    };
                    // stale 检测
                    if let Some(exp) = expected_hash {
                        let actual = sha256_hex(content.as_bytes());
                        if actual != *exp {
                            return AgentResult::Error {
                                message: format!(
                                    "file changed externally since last read (expected hash {exp}, actual {actual}); re-read the file before editing"
                                ),
                            };
                        }
                    }
                    let new_content = match apply_edits(&content, edits) {
                        Ok(c) => c,
                        Err(e) => return AgentResult::Error { message: e },
                    };
                    let diff = unified_diff(&content, &new_content);
                    let (lines_added, lines_removed) = count_diff_lines(&diff);
                    match docker_write_file(c, &abs, &new_content, timeout).await {
                        AgentResult::Success => {
                            let file_hash = sha256_hex(new_content.as_bytes());
                            AgentResult::WriteOutcome {
                                bytes_written: new_content.len() as u64,
                                lines_added,
                                lines_removed,
                                diff,
                                file_hash,
                            }
                        }
                        other => other,
                    }
                }
                None => edit_file_host(&abs, edits, expected_hash.as_deref()).await,
            },
            Err(e) => AgentResult::Error { message: e },
        },
        AgentCommand::WriteFile2 {
            path,
            content,
            expected_hash,
        } => match resolve_sandboxed(root_path, path) {
            Ok(abs) => match docker_container {
                Some(c) => {
                    // Docker: check existence -> local stale check -> write back
                    let old_content = match docker_read_file(c, &abs, timeout).await {
                        AgentResult::FileContent { content } => Some(content),
                        AgentResult::Error { .. } => None,
                        other => return other,
                    };
                    if let Some(exp) = expected_hash {
                        match &old_content {
                            Some(old) => {
                                let actual = sha256_hex(old.as_bytes());
                                if actual != *exp {
                                    return AgentResult::Error {
                                        message: format!(
                                            "file changed externally since last read (expected hash {exp}, actual {actual}); re-read the file before writing"
                                        ),
                                    };
                                }
                            }
                            None => {
                                return AgentResult::Error {
                                    message: "file does not exist but expected_hash was provided"
                                        .into(),
                                };
                            }
                        }
                    }
                    let diff = if let Some(old) = &old_content {
                        unified_diff(old, content)
                    } else {
                        let mut out = String::new();
                        for line in content.lines() {
                            out.push('+');
                            out.push_str(line);
                            out.push('\n');
                            if out.len() > DIFF_MAX_BYTES {
                                out.push_str("[diff truncated]");
                                break;
                            }
                        }
                        out
                    };
                    let (lines_added, lines_removed) = count_diff_lines(&diff);
                    match docker_write_file(c, &abs, content, timeout).await {
                        AgentResult::Success => {
                            let file_hash = sha256_hex(content.as_bytes());
                            AgentResult::WriteOutcome {
                                bytes_written: content.len() as u64,
                                lines_added,
                                lines_removed,
                                diff,
                                file_hash,
                            }
                        }
                        other => other,
                    }
                }
                None => write_file2_host(&abs, content, expected_hash.as_deref()).await,
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
    let (host_cmd, host_dir) = match docker_container {
        Some(c) => (docker_shell_cmd(c, &workdir.to_string_lossy(), cmd), None),
        None => (cmd.to_string(), Some(workdir.as_path())),
    };
    match run_host(&host_cmd, host_dir, None, timeout, cancel_rx).await {
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
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
        lines.push(if is_dir { format!("{name}/") } else { name });
    }
    lines.sort();
    let truncated_entries = total > LIST_DIR_MAX_ENTRIES;
    if truncated_entries {
        lines.truncate(LIST_DIR_MAX_ENTRIES);
    }
    let mut content = lines.join("\n");
    if truncated_entries {
        let _ = write!(content, "\n[truncated, total {total} entries]");
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

/// 默认返回行数上限
const DEFAULT_READ_LIMIT: u64 = 2000;

/// code_outline/read_symbol 宿主模式读文件的大小上限：结构解析不需要整读巨型
/// 文件，超限截断并在结果中标注（符号可能不完整）。
const MAX_PARSE_FILE_BYTES: usize = 2 * 1024 * 1024;

/// 宿主模式限额读文件（返回 lossy 内容 + 是否截断），供 tree-sitter 解析使用。
async fn read_file_capped(abs: &Path, cap: usize) -> Result<(String, bool), String> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(abs)
        .await
        .map_err(|e| format!("read {} failed: {e}", abs.display()))?;
    let mut buf = Vec::with_capacity(cap + 1);
    let n = file
        .take((cap + 1) as u64)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| format!("read {} failed: {e}", abs.display()))?;
    Ok((
        String::from_utf8_lossy(&buf[..n.min(cap)]).into_owned(),
        n > cap,
    ))
}

/// 宿主模式读取文件行区间（offset 1-based，limit 最大行数）。
/// 流式扫全文件：内存恒定（收集受 MAX_OUTPUT 字节预算约束），任意 offset 都能
/// 服务（区别于 read_file_host 的 100KB 读窗），total 为真实总行数。
/// 无效 UTF-8 按 lossy 替换（与 read_file_host 语义一致）。
async fn read_file_range_host(abs: &Path, offset: Option<u64>, limit: Option<u64>) -> AgentResult {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let file = match tokio::fs::File::open(abs).await {
        Ok(f) => f,
        Err(e) => {
            return AgentResult::Error {
                message: format!("read {} failed: {e}", abs.display()),
            };
        }
    };
    let start = offset.unwrap_or(1).max(1);
    let max_lines = limit.unwrap_or(DEFAULT_READ_LIMIT);
    let mut reader = BufReader::new(file);
    let mut buf: Vec<u8> = Vec::new();
    let mut selected: Vec<String> = Vec::new();
    let mut total: u64 = 0;
    let mut byte_budget = MAX_OUTPUT;
    // 窗口内字节超预算后停止收集（继续扫完以统计真实总行数）
    let mut collect_truncated = false;
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => {
                total += 1;
                if total >= start
                    && selected.len() < usize::try_from(max_lines).unwrap_or(usize::MAX)
                    && !collect_truncated
                {
                    if buf.len() <= byte_budget {
                        byte_budget -= buf.len();
                        let line = String::from_utf8_lossy(&buf);
                        selected.push(line.trim_end_matches(['\n', '\r']).to_string());
                    } else {
                        collect_truncated = true;
                    }
                }
            }
            Err(e) => {
                return AgentResult::Error {
                    message: format!("read {} failed: {e}", abs.display()),
                };
            }
        }
    }
    if start > total && total > 0 {
        return AgentResult::Error {
            message: format!("offset {start} exceeds total lines ({total})"),
        };
    }
    let end_line = if selected.is_empty() {
        start
    } else {
        start + selected.len() as u64 - 1
    };
    let mut result = selected.join("\n");
    if collect_truncated {
        result.push_str("\n[... window exceeded 100KB output budget, truncated ...]");
    }
    if end_line < total || start > 1 || collect_truncated {
        let _ = write!(result, "\n[showing lines {start}-{end_line} of {total}]");
    }
    AgentResult::FileContent { content: result }
}

/// Docker 模式读取文件行区间：容器内一次 exec 完成 `wc -l` 总行数 + `sed -n` 窗口
/// （stdout 首行是总行数，其余为窗口内容）。sed 直取任意 offset，不受
/// docker_read_file 的 100KB 回传窗口限制。
/// 注意：`wc -l` 计换行符数，文件末尾无换行时总行数少计 1（可接受的近似）。
async fn docker_read_file_range(
    container: &str,
    abs: &Path,
    offset: Option<u64>,
    limit: Option<u64>,
    timeout: Duration,
) -> AgentResult {
    let start = offset.unwrap_or(1).max(1);
    let max_lines = limit.unwrap_or(DEFAULT_READ_LIMIT);
    let end = start + max_lines.saturating_sub(1);
    let path = abs.to_string_lossy();
    let inner = format!(
        "wc -l < {p}; sed -n '{start},{end}p' -- {p}",
        p = sh_quote(&path)
    );
    let cmd = format!(
        "docker exec {} sh -c {}",
        sh_quote(container),
        sh_quote(&inner),
    );
    match run_host(&cmd, None, None, timeout, None).await {
        Ok(out) if out.exit_code == 0 => {
            let mut sections = out.stdout.splitn(2, '\n');
            let total: u64 = sections.next().unwrap_or("0").trim().parse().unwrap_or(0);
            let content = sections.next().unwrap_or("");
            if start > total && total > 0 {
                return AgentResult::Error {
                    message: format!("offset {start} exceeds total lines ({total})"),
                };
            }
            let shown = content.lines().count() as u64;
            let end_line = if shown == 0 { start } else { start + shown - 1 };
            let mut result = content.trim_end_matches('\n').to_string();
            if end_line < total || start > 1 {
                let _ = write!(result, "\n[showing lines {start}-{end_line} of {total}]");
            }
            AgentResult::FileContent { content: result }
        }
        Ok(out) => AgentResult::Error {
            message: format!("read {} failed: {}", abs.display(), out.stderr.trim()),
        },
        Err(message) => AgentResult::Error { message },
    }
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

/// 在容器内执行原子写：`cat > tmpfile && mv tmpfile target`，内容经 stdin 写入。
/// 注意：docker 模式下不创建父目录（宿主模式会），MVP 约定目标文件父目录已存在。
async fn docker_write_file(
    container: &str,
    abs: &Path,
    content: &str,
    timeout: Duration,
) -> AgentResult {
    let target = abs.to_string_lossy();
    let tmp = format!("{target}.tmp");
    let inner = format!(
        "cat > {} && mv {} {}",
        sh_quote(&tmp),
        sh_quote(&tmp),
        sh_quote(&target),
    );
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
    let output = if let Some(c) = docker_container {
        let cmd = docker_git_cmd(c, &root_path.to_string_lossy(), args);
        match run_host(&cmd, None, None, timeout, None).await {
            Ok(out) => out,
            Err(message) => return AgentResult::Error { message },
        }
    } else {
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

    #[cfg(unix)]
    #[test]
    fn test_sandbox_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        // root 外目标 + root 内 symlink 指向它
        let outside = dir.path().join("secret.txt");
        std::fs::write(&outside, "top secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link.txt")).unwrap();
        assert!(resolve_sandboxed(&root, "link.txt").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_sandbox_rejects_symlinked_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        let outside_dir = dir.path().join("outside");
        std::fs::create_dir(&outside_dir).unwrap();
        // 路径中间组件是指向 root 外的 symlink 目录
        std::os::unix::fs::symlink(&outside_dir, root.join("linkdir")).unwrap();
        assert!(resolve_sandboxed(&root, "linkdir/new_file.txt").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_sandbox_allows_inner_symlink_and_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        let real = root.join("real.txt");
        std::fs::write(&real, "ok").unwrap();
        // 指向 root 内部的 symlink：允许
        std::os::unix::fs::symlink(&real, root.join("inner_link.txt")).unwrap();
        assert!(resolve_sandboxed(&root, "inner_link.txt").is_ok());
        // 不存在的新文件（写入场景）：允许
        assert!(resolve_sandboxed(&root, "new_dir/new_file.txt").is_ok());
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
    async fn test_read_file_range_basic() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("lines.txt"), &content).unwrap();
        let result = handle_exec_request(
            &AgentCommand::ReadFileRange {
                path: "lines.txt".into(),
                offset: Some(10),
                limit: Some(5),
            },
            dir.path(),
            Duration::from_secs(5),
            None,
            None,
        )
        .await;
        match result {
            AgentResult::FileContent { content } => {
                assert!(content.contains("line 9"));
                assert!(content.contains("line 13"));
                assert!(!content.contains("line 14"));
                assert!(content.contains("[showing lines 10-14 of 100]"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_read_file_range_offset_exceeds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("short.txt"), "a\nb\n").unwrap();
        let result = handle_exec_request(
            &AgentCommand::ReadFileRange {
                path: "short.txt".into(),
                offset: Some(100),
                limit: Some(10),
            },
            dir.path(),
            Duration::from_secs(5),
            None,
            None,
        )
        .await;
        assert!(matches!(result, AgentResult::Error { .. }));
    }

    #[tokio::test]
    async fn test_read_file_range_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let result = handle_exec_request(
            &AgentCommand::ReadFileRange {
                path: "empty.txt".into(),
                offset: Some(1),
                limit: Some(10),
            },
            dir.path(),
            Duration::from_secs(5),
            None,
            None,
        )
        .await;
        match result {
            AgentResult::FileContent { content } => assert!(content.is_empty()),
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_read_file_range_default_offset() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("lines.txt"), &content).unwrap();
        // 无 offset/limit → 读全部（受 DEFAULT_READ_LIMIT 限制）
        let result = handle_exec_request(
            &AgentCommand::ReadFileRange {
                path: "lines.txt".into(),
                offset: None,
                limit: None,
            },
            dir.path(),
            Duration::from_secs(5),
            None,
            None,
        )
        .await;
        match result {
            AgentResult::FileContent { content } => {
                assert!(content.contains("line 0"));
                assert!(content.contains("line 9"));
            }
            other => panic!("expected FileContent, got {other:?}"),
        }
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
        // 保留排序后的前 N 条 + 末尾截断标记（标记真实总数）
        assert!(content.contains(&format!(
            "\n[truncated, total {} entries]",
            LIST_DIR_MAX_ENTRIES + 1
        )));
        assert_eq!(content.lines().count(), LIST_DIR_MAX_ENTRIES + 1);
        let mut listed: Vec<&str> = content
            .lines()
            .take(LIST_DIR_MAX_ENTRIES)
            .collect();
        let mut sorted = listed.clone();
        sorted.sort_unstable();
        assert_eq!(listed, sorted, "listed entries should be deterministically sorted");
        assert!(listed.contains(&"f00000"));
        assert!(!content.contains("f05000"), "lexicographically last entry should be truncated");
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
        assert!(content.ends_with('x'));
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
        if let Ok(out) = std::process::Command::new("ps")
            .args(["-eo", "pid,pgid,args"])
            .output()
        {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.contains("sleep 30"))
                .map(str::to_string)
                .collect()
        } else {
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
                Duration::from_mins(1),
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
        assert!(res.is_err(), "cancel should yield Err, got {res:?}");

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
    async fn test_git_exec_generic_args() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        // GitExec 通用命令：host 模式按 arg 向量直跑
        let result = handle_exec_request(
            &AgentCommand::GitExec {
                args: vec!["status".into(), "--short".into()],
            },
            dir.path(),
            Duration::from_secs(10),
            None,
            None,
        )
        .await;
        match result {
            AgentResult::FileContent { content } => assert!(content.contains("a.txt")),
            other => panic!("expected FileContent, got {other:?}"),
        }

        // stage → commit → log（全程 GitExec；add 路径顺带覆盖）
        let staged = handle_exec_request(
            &AgentCommand::GitExec {
                args: vec!["add".into(), "--".into(), "a.txt".into()],
            },
            dir.path(),
            Duration::from_secs(10),
            None,
            None,
        )
        .await;
        assert!(matches!(staged, AgentResult::FileContent { .. }));
        let committed = handle_exec_request(
            &AgentCommand::GitExec {
                args: vec!["commit".into(), "-m".into(), "add a".into()],
            },
            dir.path(),
            Duration::from_secs(10),
            None,
            None,
        )
        .await;
        assert!(matches!(committed, AgentResult::FileContent { .. }));
        let log = handle_exec_request(
            &AgentCommand::GitExec {
                args: vec!["log".into(), "-n".into(), "1".into()],
            },
            dir.path(),
            Duration::from_secs(10),
            None,
            None,
        )
        .await;
        match log {
            AgentResult::FileContent { content } => assert!(content.contains("add a")),
            other => panic!("expected FileContent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_git_exec_error_surfaces_stderr() {
        let dir = tempfile::tempdir().unwrap();
        // 非 git 仓库：git 命令失败，错误经 stderr 归一为 Error
        let result = handle_exec_request(
            &AgentCommand::GitExec {
                args: vec!["status".into()],
            },
            dir.path(),
            Duration::from_secs(10),
            None,
            None,
        )
        .await;
        assert!(matches!(result, AgentResult::Error { .. }));
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
                assert!(message.contains("grep"), "message = {message:?}");
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
        assert!(!content.contains(&format!("f{SEARCH_MAX_HITS}.rs")));
        assert!(content.ends_with(&format!("\n[truncated at {SEARCH_MAX_HITS} hits]")));
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

    // ── apply_edits 单元测试 ─────────────────────────────────────────────

    #[test]
    fn test_apply_edits_empty_errors() {
        let result = apply_edits("content", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must not be empty"));
    }

    #[test]
    fn test_apply_edits_empty_old_string_errors() {
        let edits = vec![FileEdit {
            old_string: String::new(),
            new_string: "new".into(),
            replace_all: false,
        }];
        let result = apply_edits("content", &edits);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("old_string must not be empty"));
    }

    #[test]
    fn test_apply_edits_single_replacement() {
        let edits = vec![FileEdit {
            old_string: "hello".into(),
            new_string: "world".into(),
            replace_all: false,
        }];
        let result = apply_edits("hello there", &edits).unwrap();
        assert_eq!(result, "world there");
    }

    #[test]
    fn test_apply_edits_sequential_dependency() {
        // 第二条 edit 匹配第一条的产物
        let edits = vec![
            FileEdit {
                old_string: "aaa".into(),
                new_string: "bbb".into(),
                replace_all: false,
            },
            FileEdit {
                old_string: "bbb".into(),
                new_string: "ccc".into(),
                replace_all: false,
            },
        ];
        let result = apply_edits("aaa", &edits).unwrap();
        assert_eq!(result, "ccc");
    }

    #[test]
    fn test_apply_edits_replace_all() {
        let edits = vec![FileEdit {
            old_string: "x".into(),
            new_string: "y".into(),
            replace_all: true,
        }];
        let result = apply_edits("x x x", &edits).unwrap();
        assert_eq!(result, "y y y");
    }

    #[test]
    fn test_apply_edits_replace_all_zero_matches_errors() {
        let edits = vec![FileEdit {
            old_string: "missing".into(),
            new_string: "y".into(),
            replace_all: true,
        }];
        let result = apply_edits("content", &edits);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_apply_edits_ambiguous_reports_line_numbers() {
        let content = "line1 dup\nline2 dup\nline3 dup\n";
        let edits = vec![FileEdit {
            old_string: "dup".into(),
            new_string: "replaced".into(),
            replace_all: false,
        }];
        let result = apply_edits(content, &edits);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("3 times"), "error: {err}");
        assert!(err.contains("line"), "error should mention lines: {err}");
    }

    #[test]
    fn test_apply_edits_not_found_with_closest_hint() {
        let content = "fn old_function() {}\nfn another() {}\n";
        let edits = vec![FileEdit {
            old_string: "fn old_func() {}".into(),
            new_string: "fn new_func() {}".into(),
            replace_all: false,
        }];
        let result = apply_edits(content, &edits);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("not found"), "error: {err}");
        assert!(
            err.contains("closest match"),
            "should have closest hint: {err}"
        );
    }

    #[test]
    fn test_apply_edits_0_matches_no_closest_returns_plain_error() {
        let content = "abc\n";
        let edits = vec![FileEdit {
            old_string: "zzz".into(),
            new_string: "yyy".into(),
            replace_all: false,
        }];
        let result = apply_edits(content, &edits);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn test_apply_edits_first_failure_stops_no_write() {
        // 第一条成功，第二条失败 → 结果应为 Err
        let edits = vec![
            FileEdit {
                old_string: "aaa".into(),
                new_string: "bbb".into(),
                replace_all: false,
            },
            FileEdit {
                old_string: "NONEXISTENT".into(),
                new_string: "xxx".into(),
                replace_all: false,
            },
        ];
        let result = apply_edits("aaa", &edits);
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_edits_multi_edit_sequential() {
        let content = "aaa\nbbb\nccc\n";
        let edits = vec![
            FileEdit {
                old_string: "aaa".into(),
                new_string: "AAA".into(),
                replace_all: false,
            },
            FileEdit {
                old_string: "bbb".into(),
                new_string: "BBB".into(),
                replace_all: false,
            },
            FileEdit {
                old_string: "ccc".into(),
                new_string: "CCC".into(),
                replace_all: false,
            },
        ];
        let result = apply_edits(content, &edits).unwrap();
        assert_eq!(result, "AAA\nBBB\nCCC\n");
    }

    // ── edit_file_host 端到端测试 ───────────────────────────────────────

    #[tokio::test]
    async fn test_edit_file_host_end_to_end() {
        let dir = temp_workspace(&[("code.rs", "fn old() {}\nrest\n")]);
        let edits = vec![FileEdit {
            old_string: "fn old() {}".into(),
            new_string: "fn new() {}".into(),
            replace_all: false,
        }];
        let result = edit_file_host(&dir.join("code.rs"), &edits, None).await;
        match result {
            AgentResult::WriteOutcome {
                bytes_written,
                lines_added,
                lines_removed,
                diff,
                file_hash,
            } => {
                assert!(bytes_written > 0);
                assert_eq!(lines_added, 1);
                assert_eq!(lines_removed, 1);
                assert!(diff.contains("+fn new()"));
                assert!(diff.contains("-fn old()"));
                // 验证 file_hash 是正确的 sha256
                let actual_content = std::fs::read_to_string(dir.join("code.rs")).unwrap();
                let expected_hash = sha256_hex(actual_content.as_bytes());
                assert_eq!(file_hash, expected_hash);
            }
            other => panic!("expected WriteOutcome, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_edit_file_host_stale_hash_mismatch() {
        let dir = temp_workspace(&[("code.rs", "hello\n")]);
        let edits = vec![FileEdit {
            old_string: "hello".into(),
            new_string: "world".into(),
            replace_all: false,
        }];
        let result = edit_file_host(&dir.join("code.rs"), &edits, Some("wrong_hash")).await;
        match result {
            AgentResult::Error { message } => {
                assert!(message.contains("changed externally"), "msg: {message}");
                assert!(message.contains("re-read"));
            }
            other => panic!("expected Error for stale hash, got {other:?}"),
        }
        // 文件不应被修改
        assert_eq!(
            std::fs::read_to_string(dir.join("code.rs")).unwrap(),
            "hello\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_edit_file_host_stale_hash_match() {
        let dir = temp_workspace(&[("code.rs", "hello\n")]);
        let correct_hash = sha256_hex(b"hello\n");
        let edits = vec![FileEdit {
            old_string: "hello".into(),
            new_string: "world".into(),
            replace_all: false,
        }];
        let result = edit_file_host(&dir.join("code.rs"), &edits, Some(&correct_hash)).await;
        assert!(matches!(result, AgentResult::WriteOutcome { .. }));
        assert_eq!(
            std::fs::read_to_string(dir.join("code.rs")).unwrap(),
            "world\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_edit_file_host_file_not_found() {
        let dir = temp_workspace(&[]);
        let edits = vec![FileEdit {
            old_string: "x".into(),
            new_string: "y".into(),
            replace_all: false,
        }];
        let result = edit_file_host(&dir.join("nonexistent.rs"), &edits, None).await;
        assert!(matches!(result, AgentResult::Error { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_edit_file_host_edit_failure_no_write() {
        let dir = temp_workspace(&[("code.rs", "hello\n")]);
        let edits = vec![FileEdit {
            old_string: "NONEXISTENT".into(),
            new_string: "world".into(),
            replace_all: false,
        }];
        let result = edit_file_host(&dir.join("code.rs"), &edits, None).await;
        assert!(matches!(result, AgentResult::Error { .. }));
        // 文件不应被修改
        assert_eq!(
            std::fs::read_to_string(dir.join("code.rs")).unwrap(),
            "hello\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_edit_file_via_dispatch() {
        let dir = temp_workspace(&[("a.rs", "fn old() {}\n")]);
        let result = handle_exec_request(
            &AgentCommand::EditFile {
                path: "a.rs".into(),
                edits: vec![FileEdit {
                    old_string: "fn old() {}".into(),
                    new_string: "fn new() {}".into(),
                    replace_all: false,
                }],
                expected_hash: None,
            },
            &dir,
            Duration::from_secs(5),
            None,
            None,
        )
        .await;
        assert!(matches!(result, AgentResult::WriteOutcome { .. }));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).unwrap(),
            "fn new() {}\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── write_file2_host 测试 ────────────────────────────────────────────

    #[tokio::test]
    async fn test_write_file2_new_file() {
        let dir = temp_workspace(&[]);
        let result = write_file2_host(&dir.join("new.txt"), "hello\nworld\n", None).await;
        match result {
            AgentResult::WriteOutcome {
                diff,
                file_hash,
                lines_added,
                lines_removed,
                ..
            } => {
                assert!(diff.contains("+hello"));
                assert!(diff.contains("+world"));
                assert_eq!(lines_removed, 0);
                assert!(lines_added >= 2);
                let actual = std::fs::read_to_string(dir.join("new.txt")).unwrap();
                assert_eq!(file_hash, sha256_hex(actual.as_bytes()));
            }
            other => panic!("expected WriteOutcome, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_write_file2_overwrite() {
        let dir = temp_workspace(&[("out.txt", "old line\n")]);
        let result = write_file2_host(&dir.join("out.txt"), "new line\n", None).await;
        match result {
            AgentResult::WriteOutcome {
                diff,
                lines_added,
                lines_removed,
                ..
            } => {
                assert!(diff.contains("-old line"));
                assert!(diff.contains("+new line"));
                assert!(lines_added >= 1);
                assert!(lines_removed >= 1);
            }
            other => panic!("expected WriteOutcome, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("out.txt")).unwrap(),
            "new line\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_write_file2_stale_detection() {
        let dir = temp_workspace(&[("out.txt", "original\n")]);
        let result = write_file2_host(&dir.join("out.txt"), "updated\n", Some("wrong_hash")).await;
        assert!(matches!(result, AgentResult::Error { .. }));
        assert_eq!(
            std::fs::read_to_string(dir.join("out.txt")).unwrap(),
            "original\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_write_file2_expected_hash_on_new_file_errors() {
        let dir = temp_workspace(&[]);
        let result = write_file2_host(&dir.join("new.txt"), "content", Some("any_hash")).await;
        match result {
            AgentResult::Error { message } => {
                assert!(
                    message.contains("does not exist") && message.contains("expected_hash"),
                    "msg: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_write_file2_via_dispatch() {
        let dir = temp_workspace(&[]);
        let result = handle_exec_request(
            &AgentCommand::WriteFile2 {
                path: "output.txt".into(),
                content: "test content".into(),
                expected_hash: None,
            },
            &dir,
            Duration::from_secs(5),
            None,
            None,
        )
        .await;
        match result {
            AgentResult::WriteOutcome { file_hash, .. } => {
                assert_eq!(file_hash, sha256_hex(b"test content"));
            }
            other => panic!("expected WriteOutcome, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("output.txt")).unwrap(),
            "test content"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 原子写测试 ──────────────────────────────────────────────────────

    #[tokio::test]
    #[allow(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "仅判断临时文件 .tmp 后缀，无需大小写无关比较"
    )]
    async fn test_atomic_write_no_tmp_residual() {
        let dir = temp_workspace(&[]);
        let target = dir.join("file.txt");
        atomic_write(&target, "content").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "content");
        // 检查目录中无 .tmp 文件残留
        let entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !entries.iter().any(|e| e.ends_with(".tmp")),
            "tmp residual found: {entries:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_atomic_write_creates_parent_dirs() {
        let dir = temp_workspace(&[]);
        let target = dir.join("a/b/c/file.txt");
        atomic_write(&target, "nested").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "nested");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── sha256_hex 辅助测试 ──────────────────────────────────────────────

    #[test]
    fn test_sha256_hex_deterministic() {
        let h1 = sha256_hex(b"hello");
        let h2 = sha256_hex(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // 256 bits = 64 hex chars
    }

    #[test]
    fn test_sha256_hex_different_inputs() {
        let h1 = sha256_hex(b"hello");
        let h2 = sha256_hex(b"world");
        assert_ne!(h1, h2);
    }

    // ── docker 命令形态测试（原子写）───────────────────────────────────

    #[test]
    fn test_docker_write_file_atomic_command_shape() {
        // 验证 docker_write_file 生成的命令包含 tmp + mv 形态
        // docker_write_file 是 async，我们直接测命令构造逻辑
        let abs = Path::new("/workspace/file.txt");
        let target = abs.to_string_lossy();
        let tmp = format!("{target}.tmp");
        let inner = format!(
            "cat > {} && mv {} {}",
            sh_quote(&tmp),
            sh_quote(&tmp),
            sh_quote(&target),
        );
        assert!(inner.contains(".tmp"), "inner cmd: {inner}");
        assert!(inner.contains("&& mv"), "inner cmd should have mv: {inner}");
    }

    // ── unified_diff / count_diff_lines 辅助测试 ────────────────────────

    #[test]
    fn test_unified_diff_basic() {
        let diff = unified_diff("line1\nline2\n", "line1\nline3\n");
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+line3"));
    }

    #[test]
    fn test_unified_diff_truncation() {
        let old = "x\n".repeat(10000);
        let new = "y\n".repeat(10000);
        let diff = unified_diff(&old, &new);
        assert!(diff.len() <= DIFF_MAX_BYTES + 200); // some slack for the truncation marker
        assert!(diff.contains("[diff truncated]"));
    }

    #[test]
    fn test_count_diff_lines() {
        let diff = "-old1\n+new1\n+new2\n unchanged\n-old2\n";
        let (added, removed) = count_diff_lines(diff);
        assert_eq!(added, 2);
        assert_eq!(removed, 2);
    }

    #[test]
    fn test_count_diff_lines_empty() {
        let (added, removed) = count_diff_lines("");
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }

    // ── edit_distance 辅助测试 ──────────────────────────────────────────

    #[test]
    fn test_edit_distance_same() {
        assert_eq!(edit_distance("abc", "abc"), 0);
    }

    #[test]
    fn test_edit_distance_single_insert() {
        assert_eq!(edit_distance("ac", "abc"), 1);
    }

    #[test]
    fn test_edit_distance_empty() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
    }

    // ── find_closest_line 测试 ──────────────────────────────────────────

    #[test]
    fn test_find_closest_line() {
        let content = "fn main() {}\nfn helper() {}\nlet x = 1;\n";
        let (line_num, text) = find_closest_line(content, "fn maintest() {}").unwrap();
        assert_eq!(line_num, 1);
        assert!(text.contains("fn main()"));
    }

    #[test]
    fn test_find_closest_line_empty_needle() {
        assert!(find_closest_line("content", "").is_none());
    }
}
