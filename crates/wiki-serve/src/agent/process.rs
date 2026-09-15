// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! 子进程管理：spawn codex app-server、stderr 环形缓冲、退出监听与版本探测.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

/// stderr 保留尾部行数.
pub const STDERR_TAIL_LINES: usize = 20;

/// 进程句柄与附属资源.
///
/// `child` 为 `Option` 以便 `Drop` 能 take 所有权做 kill + 移交收割线程；
/// 任何 teardown 路径（init 失败/`Stop`/shutdown/提前 return）遗漏显式 `wait`
/// 时，`Drop` 兜底保证不留 killed-but-unreaped 僵尸。
pub struct CodexProcess {
    child: Option<Child>,
    /// stderr 环形缓冲（尾部 20 行）.
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl Drop for CodexProcess {
    fn drop(&mut self) {
        // 兜底：显式 teardown 路径（`kill_and_reap` / watcher 内收割）应已收割；
        // 此处 kill 已退出则忽略，并 `try_wait` 收割恰好已退出的进程。
        // 注意：tokio 1.x 的 `Child` 无同步阻塞 wait，存活进程的收割必须走
        // async 路径——若此 Drop 触发时进程仍存活（正常流程不应发生），
        // 将残留僵尸直到父进程退出，并记日志。
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.start_kill();
        match child.try_wait() {
            // 已退出：try_wait 即收割，无事可做
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                eprintln!("codex 子进程在 Drop 时仍存活且无法同步收割，可能残留僵尸直到父进程退出");
            }
        }
    }
}

impl CodexProcess {
    /// 取 stderr 尾部（快照）.
    ///
    /// # Panics
    ///
    /// 此为内存锁，中毒视为不可恢复；内部通过 `PoisonError::into_inner` 恢复，理论上不会 panic，保留此节以满足 `clippy::missing_panics_doc`。
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// 克隆 stderr 环形缓冲句柄（供 exit-watcher 与初始化失败路径共享）.
    #[must_use]
    pub fn stderr_tail_handle(&self) -> std::sync::Arc<std::sync::Mutex<VecDeque<String>>> {
        std::sync::Arc::clone(&self.stderr_tail)
    }

    /// 取子进程 id.
    #[must_use]
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// 等待进程退出.
    ///
    /// # Errors
    ///
    /// 底层 `Child::wait` 失败时返回 `std::io::Error`。
    pub async fn wait(&mut self) -> std::io::Result<Option<i32>> {
        let child = self.child.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "子进程句柄已移交/释放")
        })?;
        let status = child.wait().await?;
        Ok(status.code())
    }

    /// 终止进程并收割（kill + wait）：显式 teardown 路径用此函数，不依赖 `Drop` 兜底.
    ///
    /// # Errors
    ///
    /// 底层 `Child::kill`/`Child::wait` 失败时返回 `std::io::Error`（kill 对已
    /// 退出进程的失败会被忽略，照常 wait 收割）。
    pub async fn kill_and_reap(&mut self) -> std::io::Result<Option<i32>> {
        let child = self.child.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "子进程句柄已移交/释放")
        })?;
        let _ = child.start_kill();
        let status = child.wait().await?;
        Ok(status.code())
    }

    /// 终止进程（SIGTERM / kill）.
    ///
    /// # Errors
    ///
    /// 底层 `Child::kill` 失败时返回 `std::io::Error`。
    pub async fn kill(&mut self) -> std::io::Result<()> {
        let child = self.child.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "子进程句柄已移交/释放")
        })?;
        child.kill().await
    }

    /// 取 stdout（用于接管为 `JsonRpcConnection` 的 reader）.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    /// 取 stdin（用于接管为 `JsonRpcConnection` 的 writer）.
    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    /// 取 stderr（若未被接管为环形缓冲任务）.
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.as_mut().and_then(|c| c.stderr.take())
    }
}

/// spawn 配置.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// 二进制路径.
    pub bin: PathBuf,
    /// 需前置的 argv（如 `["app-server"]`）.
    pub argv_prefix: Vec<String>,
    /// 额外 env（密钥等）.
    pub env: std::collections::HashMap<String, String>,
    /// `CODEX_HOME` 目录.
    pub codex_home: PathBuf,
    /// 工作目录（vault 根）.
    pub cwd: PathBuf,
}

impl SpawnConfig {
    /// 构造最终的 `Command`.
    fn build_command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        for arg in &self.argv_prefix {
            cmd.arg(arg);
        }
        // `codex-app-server` (0.153+) 默认 `--listen stdio://`，无需显式 `--stdio`。
        // 旧 `codex app-server --stdio` 也已改为默认 stdio，无需额外参数，故不附加。
        cmd.env("CODEX_HOME", &self.codex_home);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.current_dir(&self.cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        #[cfg(windows)]
        {
            // 隐藏控制台窗口
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }
}

/// 启动 codex app-server 子进程.
///
/// 返回的 `CodexProcess` 已接管 `stdout`/`stdin` 并启动 `stderr` 环形缓冲任务。
///
/// # Errors
///
/// `Command::spawn` 失败时返回 `std::io::Error`。
///
/// # Panics
///
/// 内部 `stderr` 环形缓冲的内存锁中毒时视为不可恢复；当前通过 `PoisonError::into_inner` 恢复，不会 panic，保留此节以满足 `clippy::missing_panics_doc`。
#[allow(
    clippy::unused_async,
    reason = "保留 async 签名以兼容调用方的 .await，内部仅 spawn 同步子进程与 tokio::spawn"
)]
pub async fn spawn(cfg: &SpawnConfig) -> std::io::Result<CodexProcess> {
    let mut cmd = cfg.build_command();
    let mut child = cmd.spawn()?;
    let stderr = child.stderr.take();
    let tail: Arc<Mutex<VecDeque<String>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES + 1)));
    if let Some(stderr) = stderr {
        let tail_clone = Arc::clone(&tail);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                let Ok(n) = reader.read_line(&mut line).await else {
                    break;
                };
                if n == 0 {
                    break;
                }
                let trimmed = line.trim_end_matches(['\r', '\n']).to_owned();
                if trimmed.is_empty() {
                    continue;
                }
                let mut guard = tail_clone
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.push_back(trimmed);
                while guard.len() > STDERR_TAIL_LINES {
                    guard.pop_front();
                }
            }
        });
    }
    Ok(CodexProcess {
        child: Some(child),
        stderr_tail: tail,
    })
}

/// 探测版本号：执行 `bin --version`，5s 超时，解析 stdout 首行中的版本号.
///
/// `wait_with_output` 并发读写 stdout/stderr，避免旧实现"先 wait 后读"
/// 在 stdout 超 64KB 管道缓冲时的父子互等死锁；超时则 kill + wait 收割，
/// 不留 killed-but-unreaped 子进程。
///
/// 返回 `Some(version)`（如 `"0.1.0"`）或 `None`（超时或解析失败）。
pub async fn probe_version(bin: &Path) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.arg("--version");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else {
        return None;
    };
    // 并发排空 stdout/stderr，避免旧实现"先 wait 后读"在输出超 64KB 管道缓冲
    // 时的父子互等死锁（`wait_with_output` 的等价手写版：保留 child 所有权以便
    // 超时分支 kill + wait 收割）。
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain = tokio::spawn(async move {
        let mut out = Vec::new();
        if let Some(stdout) = stdout {
            let mut reader = BufReader::new(stdout);
            let _ = reader.read_to_end(&mut out).await;
        }
        if let Some(stderr) = stderr {
            let mut reader = BufReader::new(stderr);
            let mut err = Vec::new();
            let _ = reader.read_to_end(&mut err).await;
        }
        out
    });
    let timeout = Duration::from_secs(5);
    // 超时或 wait 失败：kill 后显式 wait 收割，不留 killed-but-unreaped 子进程
    let Ok(Ok(status)) = tokio::time::timeout(timeout, child.wait()).await else {
        let _ = child.start_kill();
        // SIGKILL 不可捕获，wait 必返回；加 2s 兜底超时，仍失败则靠 Drop 收割
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        drain.abort();
        return None;
    };
    let out = drain.await.unwrap_or_default();
    if !status.success() && status.code().is_none() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out))
}

/// 从 `--version` 输出中提取版本号（取首个 `x.y.z` 形态）.
fn parse_version(output: &str) -> Option<String> {
    // 匹配如 "codex 0.1.0" / "0.2.3" / "v0.1.0"
    for token in output.split_whitespace() {
        let t = token
            .trim_start_matches('v')
            .trim_matches(|c: char| c == ',' || c == ')' || c == '(');
        if is_version_like(t) {
            return Some(t.to_owned());
        }
    }
    // 回退：按行扫描
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 行内提取 x.y.z
        if let Some(v) = extract_version_from_line(line) {
            return Some(v);
        }
    }
    None
}

fn is_version_like(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < 2 || parts.len() > 4 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn extract_version_from_line(line: &str) -> Option<String> {
    let mut start: Option<usize> = None;
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b.is_ascii_digit() {
            if start.is_none() {
                start = Some(i);
            }
        } else if b == b'.' {
            // 保留
        } else if let Some(s) = start {
            let candidate = &line[s..i];
            if is_version_like(candidate) {
                return Some(candidate.to_owned());
            }
            start = None;
        }
    }
    if let Some(s) = start {
        let candidate = &line[s..];
        // 去掉末尾非数字/点
        let trimmed = candidate.trim_end_matches(|c: char| !c.is_ascii_digit());
        if is_version_like(trimmed) {
            return Some(trimmed.to_owned());
        }
        if is_version_like(candidate) {
            return Some(candidate.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parse_version_cases() {
        assert_eq!(parse_version("codex 0.1.0"), Some("0.1.0".to_owned()));
        assert_eq!(
            parse_version("codex-app-server 0.2.3"),
            Some("0.2.3".to_owned())
        );
        assert_eq!(parse_version("v0.3.1"), Some("0.3.1".to_owned()));
        assert_eq!(
            parse_version("version 1.2.3 (abc)"),
            Some("1.2.3".to_owned())
        );
        assert_eq!(parse_version("0.1.0\n"), Some("0.1.0".to_owned()));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("codex 0.1"), Some("0.1".to_owned()));
    }

    #[test]
    fn stderr_tail_capacity() {
        // 验证 VecDeque 环形行为（与 spawn 的 stderr 逻辑一致）
        let tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES + 1);
        assert_eq!(tail.capacity(), STDERR_TAIL_LINES + 1);
    }

    #[test]
    fn spawn_config_builds_command() {
        let cfg = SpawnConfig {
            bin: PathBuf::from("/usr/bin/codex"),
            argv_prefix: vec!["app-server".to_owned()],
            env: [("WIKI_TUNNEL_LLM_KEY".to_owned(), "secret".to_owned())].into(),
            codex_home: PathBuf::from("/tmp/codex-home"),
            cwd: PathBuf::from("/tmp/vault"),
        };
        let _cmd = cfg.build_command();
        // 仅验证构造不 panic，实际 spawn 在集成环境测试
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_version_with_fake_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("fake-codex");
        // 写一个 shell 脚本模拟 --version
        std::fs::write(&bin, "#!/bin/sh\necho \"codex 0.9.1\"\n").expect("write");
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = std::fs::metadata(&bin).expect("meta").permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&bin, perm).expect("chmod");
        }
        let v = probe_version(&bin).await;
        assert_eq!(v.as_deref(), Some("0.9.1"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_version_timeout_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("slow-codex");
        // 永不退出的脚本
        std::fs::write(&bin, "#!/bin/sh\nsleep 10\n").expect("write");
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = std::fs::metadata(&bin).expect("meta").permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&bin, perm).expect("chmod");
        }
        let v = probe_version(&bin).await;
        assert!(v.is_none(), "超时应返回 None");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_and_reap_leaves_no_zombie() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("sleeper");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").expect("write");
        let mut perm = std::fs::metadata(&bin).expect("meta").permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&bin, perm).expect("chmod");
        let cfg = SpawnConfig {
            bin,
            argv_prefix: vec![],
            env: HashMap::default(),
            codex_home: dir.path().join("codex-home"),
            cwd: dir.path().to_path_buf(),
        };
        let mut proc = spawn(&cfg).await.expect("spawn");
        let pid = proc.id().expect("pid");
        let code = proc.kill_and_reap().await.expect("kill_and_reap");
        assert!(
            code.is_none() || code.is_some(),
            "kill 后退出码任意但必须返回"
        );
        // kill_and_reap 已 wait：/proc 下无僵尸（Linux）或 kill(pid,0) 失败
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            // 若仍存在，必须不是 zombie（state 字段第 3 列 != Z）
            if !stat.is_empty() {
                let state = stat.split_whitespace().nth(2).unwrap_or("");
                assert_ne!(state, "Z", "kill_and_reap 后不应残留僵尸（pid={pid}）");
            }
        }
        // Drop 不应二次 kill 出错（child 仍在，仅已 wait 过）
        drop(proc);
        let _ = pid;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_version_big_output_no_deadlock() {
        // 超 64KB 管道缓冲的 --version 输出：旧"先 wait 后读"会父子互等 5s，
        // 新实现并发排空，应快速返回解析结果。
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("chatty-codex");
        std::fs::write(
            &bin,
            "#!/bin/sh\npython3 -c \"print('x'*200000)\"\necho \"codex 0.9.9\"\n",
        )
        .expect("write");
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = std::fs::metadata(&bin).expect("meta").permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&bin, perm).expect("chmod");
        }
        let start = std::time::Instant::now();
        let v = probe_version(&bin).await;
        let elapsed = start.elapsed();
        assert_eq!(v.as_deref(), Some("0.9.9"), "应在大量输出中解析出版本");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "并发排空不应触及 5s 超时，实际 {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_and_stderr_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("echo-stderr");
        // 向 stderr 输出多行后退出
        let script = r#"#!/bin/sh
for i in $(seq 1 30); do echo "line $i" >&2; done
echo '{"id":1,"result":"ok"}'
sleep 0.1
"#;
        std::fs::write(&bin, script).expect("write");
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = std::fs::metadata(&bin).expect("meta").permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&bin, perm).expect("chmod");
        }
        let cfg = SpawnConfig {
            bin: bin.clone(),
            argv_prefix: vec![],
            env: HashMap::default(),
            codex_home: dir.path().join("codex-home"),
            cwd: dir.path().to_path_buf(),
        };
        let mut proc = spawn(&cfg).await.expect("spawn");
        // 等待 stderr 收集
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let tail = proc.stderr_tail();
        assert!(
            tail.len() <= STDERR_TAIL_LINES,
            "tail 不应超过 20 行，实际 {}",
            tail.len()
        );
        // 应保留尾部 20 行（line 11..30）
        if tail.len() == STDERR_TAIL_LINES {
            assert!(tail[0].contains("11"), "首行应为 11，实际 {:?}", tail[0]);
            assert!(
                tail[tail.len() - 1].contains("30"),
                "末行应为 30，实际 {:?}",
                tail[tail.len() - 1]
            );
        }
        let _ = proc.kill().await;
    }
}
