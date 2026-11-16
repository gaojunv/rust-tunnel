//! 客户端 GUI 自更新：控制通道桥接、下载状态机与就地替换。
//!
//! 数据面复用**既有加密控制通道**，不新开 HTTP 鉴权面——GUI 客户端只有客户端
//! token，没有管理员 JWT，与远程控制控制面的取舍一致（见 `CLAUDE.md`）。服务端
//! 按客户端版本门控：只有 ≥ `MIN_UPDATE_CLIENT_VERSION` 的客户端才会收到
//! `ClientUpdate*` 变体；客户端也只在收到过服务端更新消息后才上行检查/下载请求。
//!
//! 流程：
//! 1. 服务端注册后下发 [`ControlMessage::ClientUpdateAvailable`]（或
//!    `ClientUpdateUpToDate`，它同时是「服务端支持自更新」的能力信号）。
//! 2. GUI 弹窗显示新版本号；用户确认后 `start_download` 发
//!    [`ControlMessage::ClientUpdateRequest`]。
//! 3. 服务端按 256 KiB 分片回传；本模块在内存中拼装、校验 SHA-256、写入临时目录。
//! 4. GUI 线程取走 `take_staged`，调用 [`apply_and_restart`]：
//!    - Windows 不能覆盖正在运行的 exe，故以**下载到的新 exe** 作为 updater
//!      （`--apply-update <target>`）拉起，本进程退出；updater 轮询到文件锁释放后
//!      复制自身并重启目标。
//!    - Unix 直接在同目录写临时文件 + 原子 rename，再拉起新进程。
//!
//! **安全**：控制通道本身已加密且经过客户端 token 鉴权；SHA-256 仍是必需的——
//! 它兜住截断/落盘错误，并且客户端在哈希不匹配时**绝不**替换本机可执行文件。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rust_tunnel_common::{ClientUpdateBridge, ClientUpdateFile, ControlMessage};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::Sender;

/// 自动检查更新的间隔（6 小时）。服务端在每次注册时也会主动推送一次。
pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_hours(6);

/// 单个更新文件的合理上限（512 MiB）：防止异常清单把内存打爆。
const MAX_UPDATE_SIZE: u64 = 512 * 1024 * 1024;

/// 服务端可用的更新项（已按本机平台筛好）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableUpdate {
    /// 版本目录名（如 `v0.15.0`）。
    pub version: String,
    /// 与本机 `os`/`arch` 匹配的可执行文件。
    pub file: ClientUpdateFile,
}

/// 下载进度（UI 只读视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadProgress {
    /// 正在下载的版本。
    pub version: String,
    /// 已接收字节数。
    pub received: u64,
    /// 文件总字节数（来自服务端清单）。
    pub total: u64,
}

/// 校验完成、等待 GUI 线程执行替换的更新包。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedUpdate {
    /// 版本目录名。
    pub version: String,
    /// 已落盘的临时文件路径。
    pub path: PathBuf,
}

/// UI 每帧读取的状态快照。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateSnapshot {
    /// 有可用更新时为 `Some`。
    pub available: Option<AvailableUpdate>,
    /// 最近一次「已是最新」的版本号。
    pub up_to_date: Option<String>,
    /// 在途下载进度。
    pub download: Option<DownloadProgress>,
    /// 最近一次错误（用户关闭或重试后清空）。
    pub error: Option<String>,
    /// 是否收到过服务端更新消息（= 服务端支持自更新；老服务端恒为 `false`）。
    pub supported: bool,
    /// 用户点了「稍后」：隐藏弹窗，但保留 `available` 供设置页再次进入。
    pub dismissed: bool,
    /// 用户「跳过此版本」的版本号（与 `GuiPrefs` 镜像，供托盘线程判断是否唤醒窗口）。
    pub skipped: Option<String>,
}

/// 内部控制通道（`UpdateBridge` 与 `spawn_blocking` 的收尾任务共享）。
#[derive(Default)]
struct Inner {
    /// UI 可见状态。
    shared: Mutex<Shared>,
    /// 当前控制通道上行发送端（重连时热替换）。
    control: Mutex<Option<Sender<ControlMessage>>>,
}

/// 桥接内部的可变状态（与 [`UpdateSnapshot`] 的只读视图分开）。
#[derive(Default)]
struct Shared {
    /// 可用更新。
    available: Option<AvailableUpdate>,
    /// 已是最新的版本。
    up_to_date: Option<String>,
    /// 在途下载（含已接收的完整字节）。
    download: Option<Download>,
    /// 错误文本。
    error: Option<String>,
    /// 是否见过服务端更新消息。
    supported: bool,
    /// 「稍后」标记。
    dismissed: bool,
    /// 校验完成、待替换。
    staged: Option<StagedUpdate>,
    /// 最近一次检查时间（自动/手动/服务端主动推送都算）。
    last_check: Option<Instant>,
    /// 手动检查在途：用于把「没有适用包」这类无声失败显式告诉用户。
    check_pending: bool,
    /// 「跳过此版本」的镜像（真实持久化在 `GuiPrefs`）。
    skipped: Option<String>,
}

/// 在途下载。
struct Download {
    /// 服务端回显的传输 ID。
    transfer_id: u32,
    /// 目标版本。
    version: String,
    /// 目标文件元数据。
    file: ClientUpdateFile,
    /// 已接收字节数（同时也是下一分片的期望偏移）。
    received: u64,
    /// 已接收的完整内容。
    buffer: Vec<u8>,
}

/// GUI 客户端自更新桥接。
///
/// 由 `AppState` 持有；`crates/client` 的控制通道读循环通过
/// [`ClientUpdateBridge`] 把消息交给它，GUI 每帧用 [`Self::snapshot`] 取视图。
#[derive(Clone, Default)]
pub struct UpdateBridge {
    /// 共享状态。
    inner: Arc<Inner>,
}

impl UpdateBridge {
    /// 新建空桥接。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 取 UI 快照（克隆量很小：至多一个文件元数据）。
    #[must_use]
    pub fn snapshot(&self) -> UpdateSnapshot {
        let shared = self.lock_shared();
        UpdateSnapshot {
            available: shared.available.clone(),
            up_to_date: shared.up_to_date.clone(),
            download: shared.download.as_ref().map(|d| DownloadProgress {
                version: d.version.clone(),
                received: d.received,
                total: d.file.size,
            }),
            error: shared.error.clone(),
            supported: shared.supported,
            dismissed: shared.dismissed,
            skipped: shared.skipped.clone(),
        }
    }

    /// 同步「跳过此版本」的镜像（真实持久化由 GUI 写入 `GuiPrefs`）。
    ///
    /// 托盘线程在窗口隐藏时也要判断该不该唤醒窗口，而它拿不到 GUI 线程的 `prefs`，
    /// 故在桥接里留一份只读镜像。
    pub fn set_skipped(&self, version: Option<String>) {
        self.lock_shared().skipped = version;
    }

    /// 用户点「稍后」：隐藏弹窗（不丢 `available`）。
    pub fn dismiss(&self) {
        self.lock_shared().dismissed = true;
    }

    /// 取消「稍后」隐藏（例如用户取消了「跳过此版本」）。
    pub fn undismiss(&self) {
        self.lock_shared().dismissed = false;
    }

    /// 清除错误文本。
    pub fn clear_error(&self) {
        self.lock_shared().error = None;
    }

    /// 记录一次替换失败（GUI 线程调用）。
    pub fn fail(&self, message: impl Into<String>) {
        let mut shared = self.lock_shared();
        shared.error = Some(message.into());
    }

    /// 取走校验完成的更新包（GUI 每帧调用；成功替换后进程会退出）。
    #[must_use]
    pub fn take_staged(&self) -> Option<StagedUpdate> {
        self.lock_shared().staged.take()
    }

    /// 手动检查更新。
    ///
    /// # Errors
    /// 服务端尚未证明支持自更新（老服务端/当前未注册）或上行队列已满时返回错误文本。
    pub fn check_now(&self) -> Result<(), String> {
        let sender = self.control_sender()?;
        {
            let mut shared = self.lock_shared();
            if !shared.supported {
                return Err("当前服务端或连接不支持自动更新".to_owned());
            }
            shared.check_pending = true;
            shared.error = None;
        }
        if let Err(e) = sender.try_send(ControlMessage::ClientUpdateCheck) {
            // 发送失败就没有响应会来：撤掉「手动检查在途」标记，否则下一次响应
            // 会被误当成这次检查的结果。`last_check` 也不推进——下一帧/下一轮还能重试。
            let mut shared = self.lock_shared();
            shared.check_pending = false;
            return Err(format!("检查更新请求发送失败：{e}"));
        }
        self.lock_shared().last_check = Some(Instant::now());
        Ok(())
    }

    /// 到点自动检查（GUI 每帧调用；`auto` 来自偏好开关）。
    pub fn maybe_check(&self, auto: bool) {
        if !auto {
            return;
        }
        let due = {
            let shared = self.lock_shared();
            shared.supported
                && shared.download.is_none()
                && shared.staged.is_none()
                && shared
                    .last_check
                    .is_none_or(|t| t.elapsed() >= UPDATE_CHECK_INTERVAL)
        };
        if due {
            // 失败无需打扰用户：下一轮还会再试，注册时服务端也会主动推。
            let _ = self.check_now();
        }
    }

    /// 开始下载当前可用的更新文件。
    ///
    /// # Errors
    /// 没有可用更新、控制通道未就绪或上行队列已满时返回错误文本。
    pub fn start_download(&self) -> Result<(), String> {
        let sender = self.control_sender()?;
        let mut shared = self.lock_shared();
        if shared.download.is_some() {
            return Ok(());
        }
        let Some(available) = shared.available.clone() else {
            return Err("没有可用的更新".to_owned());
        };
        let file = available.file;
        if file.size > MAX_UPDATE_SIZE {
            let msg = format!("更新包过大（{} bytes），已拒绝", file.size);
            shared.error = Some(msg.clone());
            return Err(msg);
        }
        let transfer_id = next_transfer_id();
        shared.download = Some(Download {
            transfer_id,
            version: available.version.clone(),
            file: file.clone(),
            received: 0,
            buffer: Vec::new(),
        });
        shared.error = None;
        shared.dismissed = false;
        drop(shared);
        sender
            .try_send(ControlMessage::ClientUpdateRequest {
                transfer_id,
                version: available.version,
                file: file.name,
            })
            .map_err(|e| {
                let msg = format!("下载请求发送失败：{e}");
                let mut shared = self.lock_shared();
                shared.download = None;
                shared.error = Some(msg.clone());
                msg
            })
    }

    /// 上行的控制通道发送端；未连接/老连接时返回错误。
    fn control_sender(&self) -> Result<Sender<ControlMessage>, String> {
        self.inner
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "控制通道未就绪（尚未连接服务器）".to_owned())
    }

    /// 锁定共享状态（毒锁按 inner 处理，UI 不能因此崩溃）。
    fn lock_shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.inner
            .shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl ClientUpdateBridge for UpdateBridge {
    fn attach(&self, sender: Sender<ControlMessage>) {
        *self
            .inner
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sender);
    }

    fn handle_control_message(&self, msg: ControlMessage) -> bool {
        match msg {
            ControlMessage::ClientUpdateAvailable { version, files } => {
                self.on_available(version, files);
                true
            }
            ControlMessage::ClientUpdateUpToDate { version } => {
                self.on_up_to_date(version);
                true
            }
            ControlMessage::ClientUpdateChunk {
                transfer_id,
                offset,
                data,
                last,
            } => {
                self.on_chunk(transfer_id, offset, &data, last);
                true
            }
            ControlMessage::ClientUpdateFailed {
                transfer_id,
                message,
            } => {
                self.on_failed(transfer_id, &message);
                true
            }
            _ => false,
        }
    }

    fn on_disconnected(&self) {
        *self
            .inner
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        let mut shared = self.lock_shared();
        if shared.download.take().is_some() {
            shared.error = Some("连接已断开，更新下载中断".to_owned());
        }
        shared.check_pending = false;
    }
}

impl UpdateBridge {
    /// 处理服务端下发的可用更新。
    fn on_available(&self, version: String, files: Vec<ClientUpdateFile>) {
        let mut shared = self.lock_shared();
        shared.supported = true;
        shared.last_check = Some(Instant::now());
        let manual = std::mem::take(&mut shared.check_pending);

        // 已有在途下载或已下载待替换的包时，不让新通告打断它——否则会出现
        // 「下载的是旧版本、弹窗显示新版本」的错位。应用重启后还会再收到一次通告。
        if shared.download.is_some() || shared.staged.is_some() {
            return;
        }

        let Some(new_key) = version_key(&version) else {
            if manual {
                shared.error = Some(format!("服务端返回了无法解析的版本号：{version}"));
            }
            return;
        };
        let current_key = version_key(env!("CARGO_PKG_VERSION"));
        if current_key.is_some_and(|current| new_key <= current) {
            shared.up_to_date = Some(version);
            return;
        }

        let Some(file) = files
            .into_iter()
            .find(|f| platform_matches(f) && !f.sha256.is_empty() && f.size > 0)
        else {
            if manual {
                shared.error = Some("服务端暂无适用于本机平台的可自动安装更新包".to_owned());
            }
            return;
        };
        // 已有同一版本的在途下载/已下载包时不要重置进度。
        if shared
            .download
            .as_ref()
            .is_some_and(|d| d.version == version)
        {
            return;
        }
        if shared.staged.as_ref().is_some_and(|s| s.version == version) {
            return;
        }
        shared.available = Some(AvailableUpdate { version, file });
        shared.up_to_date = None;
        shared.error = None;
        shared.dismissed = false;
    }

    /// 处理「已是最新」。
    fn on_up_to_date(&self, version: String) {
        let mut shared = self.lock_shared();
        shared.supported = true;
        shared.last_check = Some(Instant::now());
        shared.check_pending = false;
        shared.error = None;
        // 服务端可能还停在旧版本：若它报的「最新」比自己还旧，不要显示成「已是最新 v旧」。
        let latest = version_key(&version);
        let current = version_key(env!("CARGO_PKG_VERSION"));
        if matches!((latest, current), (Some(l), Some(c)) if l < c) {
            return;
        }
        shared.up_to_date = Some(version);
    }

    /// 处理一个下载分片。
    fn on_chunk(&self, transfer_id: u32, offset: u64, data: &[u8], last: bool) {
        let mut shared = self.lock_shared();
        let Some(download) = shared.download.as_mut() else {
            return;
        };
        if download.transfer_id != transfer_id {
            // 旧传输的尾部：忽略，且不能把当前下载弄坏。
            return;
        }
        if offset != download.received {
            let msg = format!(
                "更新分片乱序（期望偏移 {}，收到 {}）",
                download.received, offset
            );
            shared.download = None;
            shared.error = Some(msg);
            return;
        }
        let Some(next) = download
            .received
            .checked_add(u64::try_from(data.len()).unwrap_or(u64::MAX))
        else {
            shared.download = None;
            shared.error = Some("更新包大小溢出".to_owned());
            return;
        };
        if next > download.file.size {
            shared.download = None;
            shared.error = Some("更新包内容超过服务端声明的长度".to_owned());
            return;
        }
        download.buffer.extend_from_slice(data);
        download.received = next;

        if !last {
            return;
        }
        // 收尾：把 Download 摘出来，避免在控制通道读循环里持锁做哈希/落盘。
        let Some(download) = shared.download.take() else {
            return;
        };
        drop(shared);
        let inner = Arc::clone(&self.inner);
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::spawn_blocking(move || finish_download(&inner, download));
        } else {
            finish_download(&self.inner, download);
        }
    }

    /// 处理服务端拒绝/中止。
    fn on_failed(&self, transfer_id: u32, message: &str) {
        let mut shared = self.lock_shared();
        if shared
            .download
            .as_ref()
            .is_some_and(|d| d.transfer_id == transfer_id)
        {
            shared.download = None;
            shared.error = Some(format!("更新下载失败：{message}"));
        }
        shared.check_pending = false;
    }
}

/// 校验并落盘一个已完成下载。
fn finish_download(inner: &Inner, download: Download) {
    let actual = sha256_hex(&download.buffer);
    if !actual.eq_ignore_ascii_case(&download.file.sha256) {
        set_error(
            inner,
            format!(
                "更新包校验失败（期望 {}，实际 {actual}），已拒绝安装",
                download.file.sha256
            ),
        );
        return;
    }
    if download.file.size != download.received {
        set_error(
            inner,
            format!(
                "更新包不完整（声明 {} bytes，收到 {} bytes）",
                download.file.size, download.received
            ),
        );
        return;
    }
    let path = match stage_file(&download.version, &download.buffer) {
        Ok(path) => path,
        Err(e) => {
            set_error(inner, format!("更新包写入失败：{e}"));
            return;
        }
    };
    let mut shared = inner.shared.lock().unwrap_or_else(PoisonError::into_inner);
    shared.staged = Some(StagedUpdate {
        version: download.version,
        path,
    });
    shared.error = None;
}

/// 把校验通过的字节写到临时目录，返回落盘路径。
fn stage_file(version: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let path = staging_path(version);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// 更新包的临时落盘路径。
///
/// 放在系统临时目录而不是 exe 同目录：同目录可能不可写（Program Files），而
/// Windows 的 updater 本来就是「从临时目录复制到目标」，跨目录不影响。
fn staging_path(version: &str) -> PathBuf {
    let name = if cfg!(windows) {
        "rust-tunnel-client-gui-update.exe"
    } else {
        "rust-tunnel-client-gui-update"
    };
    // 版本号来自服务端，做一次最小净化，防止 `..` 之类拼出目录穿越。
    let safe_version: String = version
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        .collect();
    let safe_version = if safe_version.is_empty() {
        "unknown".to_owned()
    } else {
        safe_version
    };
    std::env::temp_dir()
        .join("rust-tunnel-update")
        .join(safe_version)
        .join(name)
}

/// 设置错误并清掉在途下载。
fn set_error(inner: &Inner, message: String) {
    let mut shared = inner.shared.lock().unwrap_or_else(PoisonError::into_inner);
    shared.download = None;
    shared.error = Some(message);
}

/// 计算 SHA-256 小写十六进制摘要。
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 生成一个传输 ID（同一时刻只有一个传输，用它区分旧分片）。
fn next_transfer_id() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    // 混入进程号：两个客户端进程同一纳秒并发时也不会撞。
    nanos ^ (std::process::id() << 16)
}

/// 本机平台是否匹配该产物。
fn platform_matches(file: &ClientUpdateFile) -> bool {
    file.os == std::env::consts::OS && file.arch == std::env::consts::ARCH
}

/// 解析 `x.y.z` / `vx.y.z`（剥掉 `+build` / `-pre` 后缀）。
fn version_key(version: &str) -> Option<(u64, u64, u64)> {
    let s = version.strip_prefix('v').unwrap_or(version);
    let s = s.split(['+', '-']).next().unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// 替换本机程序并重启，返回后调用方应退出当前进程。
///
/// - **Windows**：以 `staged`（下载到的新 exe）作为 updater 拉起
///   `--apply-update <current_exe>`。当前进程的 exe 映像被系统锁住，不能直接
///   覆盖；新进程是独立文件，可以从容等待旧进程退出后再复制。
/// - **Unix**：同目录写临时文件 + 原子 `rename`（运行中的可执行文件允许被
///   rename 覆盖），随后直接拉起新进程。
///
/// # Errors
/// 临时文件复制、权限设置、重命名或子进程拉起失败时返回错误文本。
pub fn apply_and_restart(staged: &Path) -> Result<(), String> {
    let current =
        std::env::current_exe().map_err(|e| format!("无法确定当前可执行文件路径：{e}"))?;
    if !staged.is_file() {
        return Err(format!("更新包不存在：{}", staged.display()));
    }

    #[cfg(windows)]
    {
        std::process::Command::new(staged)
            .arg("--apply-update")
            .arg(&current)
            .spawn()
            .map_err(|e| format!("无法启动更新程序：{e}"))?;
        Ok(())
    }

    #[cfg(not(windows))]
    {
        let file_name = current
            .file_name()
            .ok_or_else(|| "当前可执行文件没有文件名".to_owned())?
            .to_string_lossy()
            .into_owned();
        let parent = current
            .parent()
            .ok_or_else(|| "当前可执行文件没有父目录".to_owned())?;
        let tmp = parent.join(format!(".{file_name}.new"));
        std::fs::copy(staged, &tmp)
            .map_err(|e| format!("写入新程序失败（{}）：{e}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("设置可执行权限失败：{e}"))?;
        }
        std::fs::rename(&tmp, &current)
            .map_err(|e| format!("替换当前程序失败（{}）：{e}", current.display()))?;
        std::process::Command::new(&current)
            .spawn()
            .map_err(|e| format!("重启失败：{e}"))?;
        Ok(())
    }
}

/// `--apply-update <target>` 模式：等待旧进程释放文件锁，复制自身并重启目标。
///
/// **只在 Windows 使用**（Unix 走原子 rename）。它运行的是**下载到的新 exe**，
/// 因此复制自身到目标路径不会遇到「覆盖正在运行的自己」的问题。
///
/// # Errors
/// 等待超时、复制或拉起新进程失败时返回错误。
pub fn run_apply_update(target: &Path) -> anyhow::Result<()> {
    let self_exe = std::env::current_exe()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // 直接尝试复制：旧进程还持有 exe 映像时 copy 会以共享冲突失败，
        // 等到它退出后自然成功。不要先 `open(write)` 再 copy——那会引入
        // 「探测通过、复制时又被别人抢走」的窗口。
        match std::fs::copy(&self_exe, target) {
            Ok(_) => break,
            Err(e) => {
                if Instant::now() >= deadline {
                    anyhow::bail!("等待旧进程退出超时（{}）：{e}", target.display());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    std::process::Command::new(target).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个已附着控制通道的桥接。返回的接收端必须活到测试结束：`try_send`
    /// 在接收端被丢弃后会立即失败。
    fn temp_bridge() -> (UpdateBridge, tokio::sync::mpsc::Receiver<ControlMessage>) {
        let bridge = UpdateBridge::new();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        bridge.attach(tx);
        (bridge, rx)
    }

    /// 下载分片按偏移拼装、SHA 校验通过后进入 staged。
    #[test]
    fn chunks_assemble_verify_and_stage() {
        let (bridge, _rx) = temp_bridge();
        let payload = b"new-binary-contents".to_vec();
        let sha = sha256_hex(&payload);
        let version = "v9.9.9-test".to_owned();
        let file = ClientUpdateFile {
            name: "rust-tunnel-client-gui-windows-x86_64.exe".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            size: u64::try_from(payload.len()).unwrap(),
            sha256: sha,
        };
        // 先证明支持能力（真实流程里由服务端消息置位）。
        assert!(
            bridge.handle_control_message(ControlMessage::ClientUpdateUpToDate {
                version: "v1.0.0".into(),
            })
        );
        assert!(
            bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
                version: version.clone(),
                files: vec![file],
            })
        );
        bridge.start_download().unwrap();
        let transfer_id = bridge.lock_shared().download.as_ref().unwrap().transfer_id;
        let (a, b) = payload.split_at(5);
        assert!(
            bridge.handle_control_message(ControlMessage::ClientUpdateChunk {
                transfer_id,
                offset: 0,
                data: a.to_vec(),
                last: false,
            })
        );
        assert!(
            bridge.handle_control_message(ControlMessage::ClientUpdateChunk {
                transfer_id,
                offset: a.len() as u64,
                data: b.to_vec(),
                last: true,
            })
        );
        let staged = bridge.take_staged().expect("应已校验并落盘");
        assert_eq!(staged.version, version);
        assert_eq!(std::fs::read(&staged.path).unwrap(), payload);
        let _ = std::fs::remove_dir_all(staged.path.parent().unwrap().parent().unwrap());
    }

    /// SHA 不匹配必须拒绝（绝不进入 staged）。
    #[test]
    fn bad_hash_is_rejected() {
        let (bridge, _rx) = temp_bridge();
        let payload = b"tampered".to_vec();
        let version = "v9.9.9-bad".to_owned();
        let file = ClientUpdateFile {
            name: "rust-tunnel-client-gui-windows-x86_64.exe".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            size: u64::try_from(payload.len()).unwrap(),
            sha256: "0".repeat(64),
        };
        bridge.handle_control_message(ControlMessage::ClientUpdateUpToDate {
            version: "v1.0.0".into(),
        });
        bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
            version,
            files: vec![file],
        });
        bridge.start_download().unwrap();
        let transfer_id = bridge.lock_shared().download.as_ref().unwrap().transfer_id;
        bridge.handle_control_message(ControlMessage::ClientUpdateChunk {
            transfer_id,
            offset: 0,
            data: payload,
            last: true,
        });
        assert!(bridge.take_staged().is_none());
        assert!(bridge.snapshot().error.unwrap().contains("校验失败"));
    }

    /// 版本不高于本机时只更新「已是最新」，不弹更新。
    #[test]
    fn same_or_older_version_is_not_offered() {
        let (bridge, _rx) = temp_bridge();
        bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
            version: "v0.0.1".into(),
            files: vec![ClientUpdateFile {
                name: "rust-tunnel-client-gui-x".into(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                size: 1,
                sha256: "a".repeat(64),
            }],
        });
        let snap = bridge.snapshot();
        assert!(snap.available.is_none());
        assert_eq!(snap.up_to_date.as_deref(), Some("v0.0.1"));
        assert!(snap.supported);
    }

    /// 平台不匹配的产物不会被选中。
    #[test]
    fn wrong_platform_is_not_selected() {
        let (bridge, _rx) = temp_bridge();
        bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
            version: "v9.9.9".into(),
            files: vec![ClientUpdateFile {
                name: "rust-tunnel-client-gui-other".into(),
                os: "plan9".into(),
                arch: "vax".into(),
                size: 1,
                sha256: "a".repeat(64),
            }],
        });
        assert!(bridge.snapshot().available.is_none());
    }

    /// 偏移乱序必须中止当前下载并报错。
    #[test]
    fn out_of_order_chunk_aborts() {
        let (bridge, _rx) = temp_bridge();
        let payload = b"0123456789".to_vec();
        let version = "v9.9.9-order".to_owned();
        bridge.handle_control_message(ControlMessage::ClientUpdateUpToDate {
            version: "v1.0.0".into(),
        });
        bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
            version,
            files: vec![ClientUpdateFile {
                name: "rust-tunnel-client-gui-x".into(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                size: u64::try_from(payload.len()).unwrap(),
                sha256: sha256_hex(&payload),
            }],
        });
        bridge.start_download().unwrap();
        let transfer_id = bridge.lock_shared().download.as_ref().unwrap().transfer_id;
        bridge.handle_control_message(ControlMessage::ClientUpdateChunk {
            transfer_id,
            offset: 3,
            data: b"x".to_vec(),
            last: false,
        });
        let snap = bridge.snapshot();
        assert!(snap.download.is_none());
        assert!(snap.error.unwrap().contains("乱序"));
    }

    #[test]
    fn version_key_parses() {
        assert_eq!(version_key("v0.15.0"), Some((0, 15, 0)));
        assert_eq!(version_key("1.2.3+agent"), Some((1, 2, 3)));
        assert_eq!(version_key("1.2"), Some((1, 2, 0)));
        assert_eq!(version_key("garbage"), None);
    }

    #[test]
    fn staging_path_rejects_traversal() {
        let path = staging_path("../../etc");
        // 净化后可能还留着点号（如 `....etc`），但它必须是**单个文件名段**，
        // 不能出现 `..` 路径组件。
        assert!(path
            .components()
            .all(|c| !matches!(c, std::path::Component::ParentDir)));
        assert!(path.starts_with(std::env::temp_dir()));
    }
}
