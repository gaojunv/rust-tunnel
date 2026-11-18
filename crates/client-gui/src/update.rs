//! 客户端自更新：控制通道桥接、下载状态机与就地替换。
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
//! 4. GUI 线程取走 `take_staged`，调用 [`spawn_apply`]：
//!    - **Windows** 不能覆盖正在运行的 exe，故以**下载到的新 exe** 作为 updater
//!      （`--apply-update <target>`）拉起，本进程退出；updater 轮询到文件锁释放后
//!      复制自身并重启目标。
//!    - **Unix** 直接在同目录写临时文件 + 原子 rename，再拉起新进程。
//!
//! # 无人值守服务一起更新
//!
//! Windows 上还有第二个产物：`rust-tunnel-client-service.exe`（SYSTEM 服务）。
//! 只替换 GUI 会留下一个跑旧版本的无人值守实例，而数据面 `SESSION_PROTO_VERSION`
//! 是**硬相等**校验的——新旧两端互相远程控制会在握手阶段被拒。因此更新是**成对**的：
//!
//! - **GUI 承载身份时**（服务未装/已停止）：下载 GUI + 服务两个 exe，先**提权**执行新
//!   服务 exe 的 `--install --gui-exe <新 GUI>`（它会停服务、把两个 exe 复制进
//!   `%ProgramFiles%`、重新注册，并按更新前的运行状态决定是否启动），成功后才替换
//!   GUI；任何一步失败（含用户取消 UAC）**两端都不更新**，保持版本一致。
//! - **服务承载身份时**（GUI 被动、不注册）：无头实例自己接上同一条更新通道，下载两个
//!   exe 后**以 SYSTEM 身份**就地换掉 `%ProgramFiles%` 下的两个副本，然后退出，由服务
//!   立即拉起新实例。服务 exe 映像在下次服务启动（通常重启后）才切换，但真正承载业务
//!   的是实例 exe，所以新版本当拍就生效。这条路径不需要 UAC。
//!
//! # 安全
//!
//! 控制通道本身已加密且经过客户端 token 鉴权；SHA-256 仍是必需的——它兜住截断/落盘
//! 错误，并且客户端在哈希不匹配时**绝不**替换本机可执行文件。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rust_tunnel_common::{ClientUpdateBridge, ClientUpdateFile, ControlMessage};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::Sender;

/// 自动检查更新的间隔（6 小时）。服务端在每次注册时也会主动推送一次。
pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_hours(6);

/// 单个更新文件的合理上限（512 MiB）：防止异常清单把内存打爆。
const MAX_UPDATE_SIZE: u64 = 512 * 1024 * 1024;

/// 无人值守实例自动更新失败后的重试冷却。
///
/// 实例在**每次重连**都会收到一次更新通告；若本次失败（下载中断、`%ProgramFiles%`
/// 写入失败），没有冷却就会在断线重连的循环里反复下载几十 MB。5 分钟足够让一次
/// 真故障暴露在日志里，又不至于让机器长期停在旧版本。
const INSTANCE_RETRY_COOLDOWN: Duration = Duration::from_mins(5);

/// GUI 主程序在归档里的文件名前缀（服务端 `is_auto_update_file` 同一把尺子）。
const GUI_FILE_PREFIX: &str = "rust-tunnel-client-gui-";

/// 无人值守服务 exe 在归档里的文件名前缀。
const SERVICE_FILE_PREFIX: &str = "rust-tunnel-client-service-";

/// 服务端可用的更新项（已按本机平台筛好）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableUpdate {
    /// 版本目录名（如 `v0.20.0`）。
    pub version: String,
    /// 与本机 `os`/`arch` 匹配的 GUI 主程序。
    pub file: ClientUpdateFile,
    /// 与本机 `os`/`arch` 匹配的无人值守服务 exe。
    ///
    /// 只有**本机确实装了服务**（或当前是无人值守实例）且服务端下发了它时才是
    /// `Some`。为 `None` 时行为与旧版逐字节一致：只更新 GUI。
    pub service: Option<ClientUpdateFile>,
}

/// 本轮下载里单个文件的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateTarget {
    /// GUI 主程序（同时是 `%ProgramFiles%` 里实例 exe 的来源）。
    Gui,
    /// 无人值守服务 exe。
    Service,
}

/// 待下载文件（角色 + 元数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedFile {
    /// 这个文件要替换什么。
    target: UpdateTarget,
    /// 服务端清单里的元数据。
    file: ClientUpdateFile,
}

/// 下载进度（UI 只读视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadProgress {
    /// 正在下载的版本。
    pub version: String,
    /// 当前文件名。
    pub name: String,
    /// 已接收字节数。
    pub received: u64,
    /// 文件总字节数（来自服务端清单）。
    pub total: u64,
    /// 当前是第几个文件（从 1 开始）。
    pub index: u32,
    /// 本轮一共几个文件。
    pub total_files: u32,
}

/// 校验完成、等待 GUI 线程执行替换的更新包。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedUpdate {
    /// 版本目录名。
    pub version: String,
    /// 已落盘的 GUI 主程序临时文件。
    pub gui_path: PathBuf,
    /// GUI 文件的期望摘要（替换前**再校验一次**，见 [`verify_staged`]）。
    pub gui_sha256: String,
    /// 已落盘的服务 exe 临时文件（未选择更新服务时为 `None`）。
    pub service_path: Option<PathBuf>,
    /// 服务 exe 的期望摘要。
    pub service_sha256: Option<String>,
}

/// 已落盘、带期望摘要的单个更新文件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct StagedFile {
    /// 临时文件路径。
    path: PathBuf,
    /// 服务端清单里的 SHA-256。
    sha256: String,
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
    /// 正在执行替换（服务更新 + 重启）。后台线程在跑，弹窗据此显示「正在更新」。
    pub applying: bool,
}

/// 内部控制通道（`UpdateBridge` 与 `spawn_blocking` 的收尾任务共享）。
#[derive(Default)]
struct Inner {
    /// UI 可见状态。
    shared: Mutex<Shared>,
    /// 当前控制通道上行发送端（重连时热替换）。
    control: Mutex<Option<Sender<ControlMessage>>>,
    /// 无人值守实例的退出标志；`None` = GUI 模式。
    ///
    /// 实例模式下更新成功后要**退出进程**，让服务立刻拉起 `%ProgramFiles%` 里的新 exe
    /// ——这是唯一能让正在运行的 exe 映像换掉的干净办法。
    instance_exit: Option<Arc<AtomicBool>>,
    /// 强制把服务 exe 纳入更新（实例模式恒为真；测试用它绕过平台判断）。
    ///
    /// GUI 模式下真正是否下载服务 exe 在**收到通告时**再算一次（`service_installed()`）：
    /// 服务是运行中才装的，构造时的快照会过期。
    want_service: bool,
    /// 测试用的「服务是否已安装」覆盖值；生产构建恒为 `None`（现查 SCM）。
    ///
    /// 没有它，单测结果会取决于跑测试的那台机器上有没有装服务——一条本来只验证
    /// 「下载队列」的用例会在装了服务的开发机上莫名其妙地走「服务必须一起更新」分支。
    service_installed_override: Option<bool>,
}

impl Inner {
    /// 当前是否是无人值守实例（无头、由 SYSTEM 服务拉起）。
    fn is_instance(&self) -> bool {
        self.instance_exit.is_some()
    }

    /// 本机是否装着服务（生产：现查 SCM；测试：用覆盖值保持确定性）。
    fn service_present(&self) -> bool {
        self.service_installed_override
            .unwrap_or_else(service_installed)
    }
}

/// 桥接内部的可变状态（与 [`UpdateSnapshot`] 的只读视图分开）。
///
/// 几个布尔字段是彼此独立的状态位（见过服务端消息 / 用户稍后 / 手动检查在途 /
/// 正在替换），拆成枚举只会让每次读写都多一层 match，答案并不会更清楚。
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "四个独立状态位，拆成状态机会让下载状态机更难读"
)]
struct Shared {
    /// 可用更新。
    available: Option<AvailableUpdate>,
    /// 已是最新的版本。
    up_to_date: Option<String>,
    /// 在途下载（当前文件）。
    download: Option<Download>,
    /// 本轮还没下载的文件（GUI 之后是服务 exe）。
    queue: VecDeque<PlannedFile>,
    /// 本轮一共要下载几个文件（进度显示）。
    batch_total: u32,
    /// 已完成校验并落盘的 GUI 文件。
    staged_gui: Option<StagedFile>,
    /// 已完成校验并落盘的服务 exe。
    staged_service: Option<StagedFile>,
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
    /// 替换正在进行（后台线程）。
    applying: bool,
    /// 实例模式最近一次自动更新的尝试时间（冷却用）。
    instance_attempt: Option<Instant>,
}

/// 在途下载。
struct Download {
    /// 服务端回显的传输 ID。
    transfer_id: u32,
    /// 目标版本。
    version: String,
    /// 这个文件要替换什么。
    planned: PlannedFile,
    /// 已接收字节数（同时也是下一分片的期望偏移）。
    received: u64,
    /// 已接收的完整内容。
    buffer: Vec<u8>,
    /// 当前是第几个文件（从 1 开始）。
    index: u32,
    /// 本轮一共几个文件。
    total: u32,
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
    /// 新建 GUI 模式的桥接（本机装了无人值守服务时会连服务 exe 一起下载）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                shared: Mutex::new(Shared::default()),
                control: Mutex::new(None),
                instance_exit: None,
                want_service: false,
                // 单测里固定成「没装服务」：结果不能取决于开发机装没装。
                service_installed_override: cfg!(test).then_some(false),
            }),
        }
    }

    /// 无人值守实例（`--instance`）用的桥接：**自动**下载并就地替换，不弹任何 UI。
    ///
    /// `exit` 在替换成功后置位，实例主线程据此退出，服务随即拉起新 exe。
    #[must_use]
    pub fn for_instance(exit: Arc<AtomicBool>) -> Self {
        Self {
            inner: Arc::new(Inner {
                shared: Mutex::new(Shared::default()),
                control: Mutex::new(None),
                instance_exit: Some(exit),
                // 实例存在的前提就是服务已安装：两个产物必须一起换，版本才对得上。
                want_service: true,
                service_installed_override: Some(true),
            }),
        }
    }

    /// 测试用：GUI 模式但强制连服务 exe 一起下载（本机没装服务也能测下载队列）。
    #[cfg(test)]
    fn for_test_with_service() -> Self {
        let mut bridge = Self::new();
        let inner = Arc::get_mut(&mut bridge.inner).expect("刚构造，独占");
        inner.want_service = true;
        bridge
    }

    /// 取 UI 快照（克隆量很小：至多两个文件元数据）。
    #[must_use]
    pub fn snapshot(&self) -> UpdateSnapshot {
        let shared = self.lock_shared();
        UpdateSnapshot {
            available: shared.available.clone(),
            up_to_date: shared.up_to_date.clone(),
            download: shared.download.as_ref().map(|d| DownloadProgress {
                version: d.version.clone(),
                name: d.planned.file.name.clone(),
                received: d.received,
                total: d.planned.file.size,
                index: d.index,
                total_files: d.total,
            }),
            error: shared.error.clone(),
            supported: shared.supported,
            dismissed: shared.dismissed,
            skipped: shared.skipped.clone(),
            applying: shared.applying,
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

    /// 标记「正在执行替换」（GUI 线程在起后台任务前调用）。
    pub fn mark_applying(&self) {
        self.lock_shared().applying = true;
    }

    /// 清除「正在执行替换」（替换失败后调用）。
    pub fn clear_applying(&self) {
        self.lock_shared().applying = false;
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

    /// 开始下载当前可用的更新文件（GUI 在前、服务 exe 在后）。
    ///
    /// # Errors
    /// 没有可用更新、更新包已就绪、控制通道未就绪或上行队列已满时返回错误文本。
    pub fn start_download(&self) -> Result<(), String> {
        let sender = self.control_sender()?;
        let mut shared = self.lock_shared();
        if shared.download.is_some() {
            return Ok(());
        }
        if shared.staged.is_some() {
            return Err("更新包已下载，等待安装".to_owned());
        }
        let Some(available) = shared.available.clone() else {
            return Err("没有可用的更新".to_owned());
        };
        if available.file.size == 0 || available.file.size > MAX_UPDATE_SIZE {
            let msg = format!("更新包过大（{} bytes），已拒绝", available.file.size);
            shared.error = Some(msg.clone());
            return Err(msg);
        }

        let mut queue = VecDeque::with_capacity(2);
        queue.push_back(PlannedFile {
            target: UpdateTarget::Gui,
            file: available.file.clone(),
        });
        if let Some(service) = available.service.clone() {
            if service.size > 0 && service.size <= MAX_UPDATE_SIZE {
                queue.push_back(PlannedFile {
                    target: UpdateTarget::Service,
                    file: service,
                });
                // 两个文件都放行不了时宁可不更新：半套更新正是版本错位的来源。
            } else {
                let msg = format!("服务更新包过大（{} bytes），已拒绝", service.size);
                shared.error = Some(msg.clone());
                return Err(msg);
            }
        }
        let total = u32::try_from(queue.len()).unwrap_or(u32::MAX);
        let Some(first) = queue.pop_front() else {
            return Err("更新包清单为空".to_owned());
        };
        let transfer_id = next_transfer_id();
        shared.batch_total = total;
        shared.queue = queue;
        shared.staged_gui = None;
        shared.staged_service = None;
        shared.download = Some(Download {
            transfer_id,
            version: available.version.clone(),
            planned: first,
            received: 0,
            buffer: Vec::new(),
            index: 1,
            total,
        });
        shared.error = None;
        shared.dismissed = false;
        shared.applying = false;
        let file = shared
            .download
            .as_ref()
            .map_or_else(String::new, |d| d.planned.file.name.clone());
        let version = available.version;
        drop(shared);

        sender
            .try_send(ControlMessage::ClientUpdateRequest {
                transfer_id,
                version,
                file,
            })
            .map_err(|e| {
                let msg = format!("下载请求发送失败：{e}");
                let mut shared = self.lock_shared();
                shared.download = None;
                shared.queue.clear();
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
        let interrupted = shared.download.take().is_some();
        shared.queue.clear();
        // 半套落盘的文件必须丢掉：GUI 与服务 exe 只换一个就是版本错位。
        shared.staged_gui = None;
        shared.staged_service = None;
        if interrupted {
            shared.error = Some("连接已断开，更新下载中断".to_owned());
        }
        shared.check_pending = false;
    }
}

impl UpdateBridge {
    /// 处理服务端下发的可用更新。
    fn on_available(&self, version: String, files: Vec<ClientUpdateFile>) {
        let instance = self.inner.is_instance();
        let auto_start = 'offer: {
            let mut shared = self.lock_shared();
            shared.supported = true;
            shared.last_check = Some(Instant::now());
            let manual = std::mem::take(&mut shared.check_pending);

            // 已有在途下载或已下载待替换的包时，不让新通告打断它——否则会出现
            // 「下载的是旧版本、弹窗显示新版本」的错位。应用重启后还会再收到一次通告。
            if shared.download.is_some() || shared.staged.is_some() || shared.applying {
                break 'offer false;
            }

            let Some(new_key) = version_key(&version) else {
                if manual {
                    shared.error = Some(format!("服务端返回了无法解析的版本号：{version}"));
                }
                break 'offer false;
            };
            let current_key = version_key(env!("CARGO_PKG_VERSION"));
            if current_key.is_some_and(|current| new_key <= current) {
                shared.up_to_date = Some(version);
                break 'offer false;
            }

            // 服务可能在 GUI 启动之后才装上：这里现算一次，别用构造时的快照。
            let installed = self.inner.service_present();
            let want_service = instance || self.inner.want_service || installed;
            let (Some(file), service) = select_offer(files, want_service) else {
                if manual {
                    shared.error = Some("服务端暂无适用于本机平台的可自动安装更新包".to_owned());
                }
                break 'offer false;
            };
            // 实例只能「两个一起换」：只拿到 GUI 产物时保持旧版本继续跑，绝不半套更新。
            if instance && service.is_none() {
                tracing::debug!(
                    version = %version,
                    "服务端未下发服务 exe；无人值守实例跳过本次更新以保持两端同版本"
                );
                break 'offer false;
            }
            // GUI 侧同理：本机**确实装着服务**却只拿到 GUI 产物（典型：老服务端/归档缺
            // 服务 exe）。只换 GUI 会留下一个跑旧版的无人值守实例，而服务端不会再推同一个
            // 版本，两端就此永久错位——宁可不更新，并把原因摆到弹窗里。
            if installed && service.is_none() {
                let message = "服务端没有提供 Windows 无人值守服务的更新包。为保证客户端与服务\
                               版本一致，本次没有更新；请先把客户端归档升级到包含服务 exe 的版本。";
                if manual {
                    shared.error = Some(message.to_owned());
                } else {
                    // 自动检查不弹窗：这是服务端归档的配置问题，6 小时弹一次只会烦人。
                    tracing::warn!(version = %version, "{message}");
                }
                break 'offer false;
            }

            shared.available = Some(AvailableUpdate {
                version,
                file,
                service,
            });
            shared.up_to_date = None;
            shared.error = None;
            shared.dismissed = false;
            true
        };

        if auto_start && instance {
            let due = {
                let mut shared = self.lock_shared();
                let due = shared
                    .instance_attempt
                    .is_none_or(|at| at.elapsed() >= INSTANCE_RETRY_COOLDOWN);
                if due {
                    shared.instance_attempt = Some(Instant::now());
                }
                due
            };
            if due {
                // 实例没有用户可点「立即更新」：收到服务端通告就自动开始下载。
                if let Err(e) = self.start_download() {
                    tracing::warn!("无人值守实例自动更新未能开始：{e}");
                }
            }
        }
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
            shared.queue.clear();
            shared.error = Some(msg);
            return;
        }
        let Some(next) = download
            .received
            .checked_add(u64::try_from(data.len()).unwrap_or(u64::MAX))
        else {
            shared.download = None;
            shared.queue.clear();
            shared.error = Some("更新包大小溢出".to_owned());
            return;
        };
        if next > download.planned.file.size {
            shared.download = None;
            shared.queue.clear();
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
            shared.queue.clear();
            shared.error = Some(format!("更新下载失败：{message}"));
        }
        shared.check_pending = false;
    }
}

/// 从服务端清单里挑出 GUI 与服务两个产物（都按本机平台筛）。
///
/// `want_service == false` 时**不认**服务 exe：老服务端/未装服务的机器拿到的行为
/// 与旧版本完全一致（只更新 GUI）。
fn select_offer(
    files: Vec<ClientUpdateFile>,
    want_service: bool,
) -> (Option<ClientUpdateFile>, Option<ClientUpdateFile>) {
    let mut gui = None;
    let mut service = None;
    for file in files {
        if !platform_matches(&file) || file.sha256.is_empty() || file.size == 0 {
            continue;
        }
        // 服务 exe 只可能是 `.exe`；GUI 的 zip/dmg 由服务端 `is_auto_update_file` 挡掉
        // （线上 `ClientUpdateFile` 不带扩展名字段，客户端这边只认文件名前缀）。
        if file.name.starts_with(GUI_FILE_PREFIX) {
            if gui.is_none() {
                gui = Some(file);
            }
        } else if want_service && file.name.starts_with(SERVICE_FILE_PREFIX) && service.is_none() {
            service = Some(file);
        }
    }
    (gui, service)
}

/// 校验并落盘一个已完成下载。
fn finish_download(inner: &Inner, download: Download) {
    let actual = sha256_hex(&download.buffer);
    if !actual.eq_ignore_ascii_case(&download.planned.file.sha256) {
        set_error(
            inner,
            format!(
                "更新包校验失败（期望 {}，实际 {actual}），已拒绝安装",
                download.planned.file.sha256
            ),
        );
        return;
    }
    if download.planned.file.size != download.received {
        set_error(
            inner,
            format!(
                "更新包不完整（声明 {} bytes，收到 {} bytes）",
                download.planned.file.size, download.received
            ),
        );
        return;
    }
    let target = download.planned.target;
    let path = match stage_file(&download.version, target, &download.buffer) {
        Ok(path) => path,
        Err(e) => {
            set_error(inner, format!("更新包写入失败：{e}"));
            return;
        }
    };

    let staged = StagedFile {
        path,
        sha256: download.planned.file.sha256.clone(),
    };
    let (gui, service, more) = {
        let mut shared = inner.shared.lock().unwrap_or_else(PoisonError::into_inner);
        match target {
            UpdateTarget::Gui => shared.staged_gui = Some(staged),
            UpdateTarget::Service => shared.staged_service = Some(staged),
        }
        shared.error = None;
        (
            shared.staged_gui.clone(),
            shared.staged_service.clone(),
            !shared.queue.is_empty(),
        )
    };

    if inner.is_instance() {
        // 实例：两个都到齐才动手；任何一步失败都继续跑旧版本（下次通告再试）。
        if let (Some(gui), Some(service)) = (gui, service) {
            apply_instance_update(inner, &download.version, &gui.path, &service.path);
        }
        return;
    }
    if more {
        advance_queue(inner);
        return;
    }
    let Some(gui) = gui else {
        set_error(inner, "更新包缺失 GUI 主程序".to_owned());
        return;
    };
    let mut shared = inner.shared.lock().unwrap_or_else(PoisonError::into_inner);
    shared.staged = Some(StagedUpdate {
        version: download.version,
        gui_path: gui.path,
        gui_sha256: gui.sha256,
        service_path: service.as_ref().map(|staged| staged.path.clone()),
        service_sha256: service.map(|staged| staged.sha256),
    });
}

/// 下载完一个文件后，按队列继续下一个（收尾任务只有 `&Inner`，故是自由函数）。
fn advance_queue(inner: &Inner) {
    let sender = {
        let guard = inner.control.lock().unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    };
    let Some(sender) = sender else {
        set_error(inner, "连接已断开，更新下载中断".to_owned());
        return;
    };
    let mut shared = inner.shared.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(planned) = shared.queue.pop_front() else {
        return;
    };
    let total = shared.batch_total;
    let index = total.saturating_sub(u32::try_from(shared.queue.len()).unwrap_or(0));
    let Some(version) = shared.available.as_ref().map(|a| a.version.clone()) else {
        return;
    };
    let transfer_id = next_transfer_id();
    let name = planned.file.name.clone();
    shared.download = Some(Download {
        transfer_id,
        version: version.clone(),
        planned,
        received: 0,
        buffer: Vec::new(),
        index,
        total,
    });
    drop(shared);
    if let Err(e) = sender.try_send(ControlMessage::ClientUpdateRequest {
        transfer_id,
        version,
        file: name,
    }) {
        set_error(inner, format!("下载请求发送失败：{e}"));
    }
}

/// 无人值守实例：把两个新 exe 换进 `%ProgramFiles%`，成功后让实例退出。
///
/// 只有**两个文件都成功换入**才置退出标志；任何失败都保留旧版本继续运行，
/// 并把错误写进日志（实例侧没有 UI）。
fn apply_instance_update(inner: &Inner, version: &str, gui: &Path, service: &Path) {
    #[cfg(windows)]
    {
        match swap_in_installed_exes(gui, service) {
            Ok(()) => {
                tracing::warn!(
                    version,
                    "无人值守实例已就地更新（GUI + 服务副本），退出以让服务拉起新版本"
                );
                if let Some(exit) = &inner.instance_exit {
                    exit.store(true, Ordering::SeqCst);
                }
            }
            Err(e) => {
                tracing::error!(version, "无人值守实例自动更新失败，继续运行旧版本：{e}");
                let mut shared = inner.shared.lock().unwrap_or_else(PoisonError::into_inner);
                shared.staged_gui = None;
                shared.staged_service = None;
            }
        }
    }
    #[cfg(not(windows))]
    {
        // 实例只在 Windows 存在；这里让参数在其它平台上不触发 unused 警告。
        let _ = (inner, version, gui, service);
    }
}

/// 把两个临时文件换进 `%ProgramFiles%\rust-tunnel\` 下的固定位置。
///
/// **先服务、后实例**：服务换失败时实例还没动，直接报错即可；实例换失败时把服务
/// 从 `.old` 回滚——绝不留半套。反过来（先实例后服务）一旦服务失败，磁盘上就是
/// 新实例 + 旧服务，而实例重启后版本已是最新，服务端不会再下发同一个版本，旧服务
/// 就此永久卡住。
#[cfg(windows)]
fn swap_in_installed_exes(gui: &Path, service: &Path) -> Result<(), String> {
    let gui_target = rust_tunnel_win_platform::paths::installed_instance_exe();
    let service_target = rust_tunnel_win_platform::paths::installed_service_exe();
    let service_swap = swap_in_file(service, &service_target)?;
    match swap_in_file(gui, &gui_target) {
        Ok(gui_swap) => {
            // 旧映像可能还在被运行中的进程占用——删不掉是预期，下次更新会再试。
            let _ = std::fs::remove_file(&service_swap.old);
            let _ = std::fs::remove_file(&gui_swap.old);
            Ok(())
        }
        Err(e) => {
            // 回滚服务 exe：新文件没被任何进程使用，可以直接删掉再把旧名换回来。
            let _ = std::fs::remove_file(&service_target);
            let _ = std::fs::rename(&service_swap.old, &service_target);
            Err(e)
        }
    }
}

/// 一次换名留下的旧文件位置（回滚用）。
#[cfg(any(windows, test))]
struct SwapOutcome {
    /// 被换下来的旧文件（可能仍在被运行中的映像占用）。
    old: PathBuf,
}

/// 用 `source` 原子地替换 `target`，返回被换下来的旧文件路径。
///
/// Windows 不允许覆盖正在运行的 exe，但**允许改名**；所以先把新文件写成目标目录里的
/// `.new`，再把 `target` 改名成 `.old`，最后把 `.new` 改名成 `target`。整个过程在同一
/// 目录内，跨卷/权限问题在第一步就会暴露。
#[cfg(any(windows, test))]
fn swap_in_file(source: &Path, target: &Path) -> Result<SwapOutcome, String> {
    let file_name = target
        .file_name()
        .ok_or_else(|| format!("目标没有文件名：{}", target.display()))?
        .to_string_lossy()
        .into_owned();
    let parent = target
        .parent()
        .ok_or_else(|| format!("目标没有父目录：{}", target.display()))?;
    let new = parent.join(format!(".{file_name}.new"));
    let mut old = parent.join(format!(".{file_name}.old"));

    std::fs::copy(source, &new)
        .map_err(|err| format!("写入新版本 {} 失败：{err}", new.display()))?;
    // 上一次的 `.old` 可能还在（它自己的映像已经退出），能删就删；删不掉（仍被某个
    // 映像占用）就换一个唯一名字——`rename` 在 Windows 上**不会覆盖**已存在的目标。
    let _ = std::fs::remove_file(&old);
    if old.exists() {
        old = parent.join(format!(".{file_name}.old-{}", next_transfer_id()));
    }
    if target.exists() {
        std::fs::rename(target, &old)
            .map_err(|err| format!("暂存旧版本 {} 失败：{err}", target.display()))?;
    }
    if let Err(err) = std::fs::rename(&new, target) {
        // 把旧文件换回去，别留下一个没有目标 exe 的安装目录。
        let _ = std::fs::rename(&old, target);
        let _ = std::fs::remove_file(&new);
        return Err(format!("换入新版本 {} 失败：{err}", target.display()));
    }
    Ok(SwapOutcome { old })
}

/// 把校验通过的字节写到临时目录，返回落盘路径。
fn stage_file(version: &str, target: UpdateTarget, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let path = staging_path(version, target);
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
fn staging_path(version: &str, target: UpdateTarget) -> PathBuf {
    let name = match target {
        UpdateTarget::Gui if cfg!(windows) => "rust-tunnel-client-gui-update.exe",
        UpdateTarget::Gui => "rust-tunnel-client-gui-update",
        UpdateTarget::Service => "rust-tunnel-client-service-update.exe",
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
    shared.queue.clear();
    shared.error = Some(message);
}

/// 计算 SHA-256 小写十六进制摘要。
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 生成一个传输 ID（同一时刻只有一个传输，用它区分旧分片）。
fn next_transfer_id() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // 进程内单调计数器：同一纳秒内连开两个传输（GUI + 服务）时也能区分。
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    // 混入进程号与计数器：两个客户端进程同一纳秒并发时也不会撞。
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    nanos ^ (std::process::id() << 16) ^ counter.wrapping_mul(0x9E37_79B9)
}

/// 本机平台是否匹配该产物。
fn platform_matches(file: &ClientUpdateFile) -> bool {
    file.os == std::env::consts::OS && file.arch == std::env::consts::ARCH
}

/// 本机是否**安装过**无人值守服务。非 Windows 恒为 `false`（那个概念不存在）。
///
/// 走 **SCM** 而不是 `%ProgramFiles%\rust-tunnel\...is_file()`：`--install` 会把那个目录
/// 的 DACL 收敛到「仅 SYSTEM + Administrators」，未提权的 GUI **连 stat 都不进去**
/// （`ACCESS_DENIED` 会被 `is_file()` 折叠成 `false`）。SCM 的「查询服务状态」对普通用户
/// 开放，才是可靠信号。
///
/// 查询本身失败按「没装」处理：宁可漏一次服务更新，也绝不能把一次普通的 GUI 更新变成
/// 「顺手把服务装上」——`--install` 是会注册 AUTO_START 服务的。
#[must_use]
pub fn service_installed() -> bool {
    #[cfg(windows)]
    {
        match rust_tunnel_win_platform::service::query_state() {
            Ok(state) => state.is_installed(),
            Err(err) => {
                tracing::debug!(%err, "查询服务状态失败，本次不把服务纳入更新");
                false
            }
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
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

/// 替换本机程序并重启；返回后调用方应退出当前进程。
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

/// 在后台线程执行整包替换，结果经通道回灌给 GUI 线程。
///
/// 为什么不在 UI 线程直接做：Windows 上要先提权跑一次服务安装器（`--install`），
/// 那会弹 UAC 并阻塞到子进程结束（复制/加固/停服务，二十秒量级）。放在 UI 线程上
/// 就是「界面无响应」。
///
/// 顺序是**先服务、后 GUI**，并且服务失败时**不**替换 GUI：
/// - GUI 与服务实例的数据面协议版本硬相等，只更新一半会让远程控制直接握手失败；
/// - 用户取消 UAC 也走这条：两端都留在旧版本，下次更新时再试。
#[must_use]
pub fn spawn_apply(staged: StagedUpdate) -> Receiver<Result<(), String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = apply_updates(&staged);
        // 发送失败只可能是 GUI 已退出——不是错误。
        let _ = tx.send(result);
    });
    rx
}

/// [`spawn_apply`] 的同步主体。
///
/// **落盘后、提权前再校验一次**：暂存文件在用户临时目录里，从写入到用户点下 UAC
/// 「是」之间有一段真实的时间窗。第二次哈希把「同账户进程在这段窗口里替换文件」
/// 的攻击面压到最小（真正的 TOCTOU 只能靠签名/受保护目录解决，本仓不引入签名）。
fn apply_updates(staged: &StagedUpdate) -> Result<(), String> {
    verify_staged(&staged.gui_path, &staged.gui_sha256)?;
    #[cfg(windows)]
    {
        match (
            staged.service_path.as_deref(),
            staged.service_sha256.as_deref(),
        ) {
            (Some(path), Some(sha)) => {
                verify_staged(path, sha)?;
                install_service_update(path, &staged.gui_path, sha, &staged.gui_sha256)?;
            }
            (Some(_), None) => {
                return Err(String::from("服务更新包缺少校验和，已拒绝安装"));
            }
            (None, _) => {}
        }
    }
    #[cfg(not(windows))]
    {
        debug_assert!(staged.service_path.is_none(), "非 Windows 没有无人值守服务");
    }
    apply_and_restart(&staged.gui_path)
}

/// 安装前复验一个暂存文件：内容必须仍与下载时校验过的摘要一致。
fn verify_staged(path: &Path, expected: &str) -> Result<(), String> {
    let bytes =
        std::fs::read(path).map_err(|err| format!("读取更新包 {} 失败：{err}", path.display()))?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "更新包 {} 在安装前发生变化（期望 {expected}，实际 {actual}），已中止",
            path.display()
        ));
    }
    Ok(())
}

/// 提权执行新服务 exe 的 `--install`，把两个 exe 换进 `%ProgramFiles%`。
///
/// **不传 `--host-config`**：`host.toml` 已经在 `%ProgramData%` 里，`--install` 只在
/// 显式传入时才覆写它；这里要保留服务端地址与 token。
///
/// **带上两个摘要**：提权进程在复制前会各自复验一遍（`--gui-sha256` /
/// `--self-sha256`）。UAC 提示可能停留很久，同账户进程足以在这段时间里替换暂存文件；
/// 只在 GUI 侧校验是把「校验」和「使用」分在了两个信任边界上。
///
/// **按更新前的运行状态决定 `--start`**：服务原本在跑就恢复运行；原本停着就保持停止
/// （不要因为一次 GUI 更新把客户端身份突然挪到服务实例上）。
#[cfg(windows)]
fn install_service_update(
    service_installer: &Path,
    gui_exe: &Path,
    service_sha256: &str,
    gui_sha256: &str,
) -> Result<(), String> {
    use rust_tunnel_win_platform::elevate::{self, ElevationOutcome};
    use rust_tunnel_win_platform::service::{self, ServiceState};

    // 下载之后、安装之前服务被卸掉了：没有可更新的东西，别弹一次无意义的 UAC。
    if !service_installed() {
        return Ok(());
    }
    let was_running = matches!(
        service::query_state(),
        Ok(ServiceState::Running
            | ServiceState::StartPending
            | ServiceState::StopPending
            | ServiceState::Paused)
    );

    let mut args = vec![
        String::from("--install"),
        String::from("--gui-exe"),
        gui_exe.to_string_lossy().into_owned(),
        String::from("--gui-sha256"),
        gui_sha256.to_owned(),
        // 安装器复验的是**它自己这份 exe**（`current_exe()`，即暂存的服务 exe）。
        String::from("--self-sha256"),
        service_sha256.to_owned(),
    ];
    if was_running {
        args.push(String::from("--start"));
    }

    match elevate::run_elevated(service_installer, &args) {
        Ok(ElevationOutcome::Completed { exit_code: 0 }) => Ok(()),
        Ok(ElevationOutcome::Completed { exit_code }) => Err(format!(
            "Windows 无人值守服务更新失败（安装器退出码 {exit_code}）。\
             为保证版本一致，客户端也没有更新；请重试。"
        )),
        Ok(ElevationOutcome::Cancelled) => Err(String::from(
            "已取消管理员授权。为保证客户端与 Windows 无人值守服务版本一致，\
             本次两者都没有更新；可以稍后重试。",
        )),
        Err(err) => Err(format!(
            "无法启动服务更新程序：{err}。为保证版本一致，客户端也没有更新。"
        )),
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

    /// 服务端下发的一个文件描述（GUI 或服务）。
    fn wire_file(name: &str, payload: &[u8]) -> ClientUpdateFile {
        ClientUpdateFile {
            name: name.into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            size: u64::try_from(payload.len()).unwrap(),
            sha256: sha256_hex(payload),
        }
    }

    /// 版本目录名。
    fn staged_version(tag: &str) -> String {
        format!("v9.9.9-{tag}-{}", std::process::id())
    }

    /// 下载分片按偏移拼装、SHA 校验通过后进入 staged。
    #[test]
    fn chunks_assemble_verify_and_stage() {
        let (bridge, _rx) = temp_bridge();
        let payload = b"new-binary-contents".to_vec();
        let version = staged_version("gui");
        // 先证明支持能力（真实流程里由服务端消息置位）。
        assert!(
            bridge.handle_control_message(ControlMessage::ClientUpdateUpToDate {
                version: "v1.0.0".into(),
            })
        );
        assert!(
            bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
                version: version.clone(),
                files: vec![wire_file(
                    "rust-tunnel-client-gui-windows-x86_64.exe",
                    &payload
                )],
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
        assert!(staged.service_path.is_none());
        assert_eq!(std::fs::read(&staged.gui_path).unwrap(), payload);
        let _ = std::fs::remove_dir_all(staged.gui_path.parent().unwrap());
    }

    /// 服务 exe 在同一轮更新里**排在 GUI 之后**下载，两个都校验通过才成包。
    #[test]
    fn service_file_is_downloaded_after_the_gui() {
        let bridge = UpdateBridge::for_test_with_service();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        bridge.attach(tx);
        let gui_payload = b"gui-binary".to_vec();
        let service_payload = b"service-binary".to_vec();
        let version = staged_version("pair");
        bridge.handle_control_message(ControlMessage::ClientUpdateUpToDate {
            version: "v1.0.0".into(),
        });
        bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
            version: version.clone(),
            files: vec![
                wire_file("rust-tunnel-client-gui-windows-x86_64.exe", &gui_payload),
                wire_file(
                    "rust-tunnel-client-service-windows-x86_64.exe",
                    &service_payload,
                ),
            ],
        });
        let snapshot = bridge.snapshot();
        assert_eq!(
            snapshot
                .available
                .as_ref()
                .and_then(|a| a.service.as_ref())
                .map(|f| f.name.as_str()),
            Some("rust-tunnel-client-service-windows-x86_64.exe")
        );
        bridge.start_download().unwrap();
        // 第一个请求：GUI。
        let first = rx.try_recv().expect("应有第一条下载请求");
        let ControlMessage::ClientUpdateRequest {
            transfer_id,
            file,
            version: req_version,
        } = first
        else {
            panic!("期望 ClientUpdateRequest，得到 {first:?}");
        };
        assert_eq!(file, "rust-tunnel-client-gui-windows-x86_64.exe");
        assert_eq!(req_version, version);
        assert!(
            bridge.handle_control_message(ControlMessage::ClientUpdateChunk {
                transfer_id,
                offset: 0,
                data: gui_payload,
                last: true,
            })
        );
        // 第一个完成后应自动请求第二个：服务 exe。
        let second = rx.try_recv().expect("应自动请求服务 exe");
        let ControlMessage::ClientUpdateRequest {
            transfer_id, file, ..
        } = second
        else {
            panic!("期望 ClientUpdateRequest，得到 {second:?}");
        };
        assert_eq!(file, "rust-tunnel-client-service-windows-x86_64.exe");
        let progress = bridge.snapshot().download.expect("进度应仍在");
        assert_eq!((progress.index, progress.total_files), (2, 2));
        assert!(
            bridge.handle_control_message(ControlMessage::ClientUpdateChunk {
                transfer_id,
                offset: 0,
                data: service_payload,
                last: true,
            })
        );
        let staged = bridge.take_staged().expect("两个文件都到齐才成包");
        assert_eq!(staged.version, version);
        assert_eq!(std::fs::read(&staged.gui_path).unwrap(), b"gui-binary");
        let service_path = staged.service_path.expect("应带服务 exe");
        assert_eq!(std::fs::read(&service_path).unwrap(), b"service-binary");
        let _ = std::fs::remove_dir_all(staged.gui_path.parent().unwrap());
    }

    /// 服务端没下发服务 exe 时，只装一个文件也不该 panic / 卡住。
    #[test]
    fn gui_only_offer_still_works_with_service_enabled() {
        let bridge = UpdateBridge::for_test_with_service();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        bridge.attach(tx);
        let payload = b"gui-only".to_vec();
        let version = staged_version("guionly");
        bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
            version: version.clone(),
            files: vec![wire_file("rust-tunnel-client-gui-macos-aarch64", &payload)],
        });
        bridge.start_download().unwrap();
        let first = rx.try_recv().unwrap();
        let ControlMessage::ClientUpdateRequest { transfer_id, .. } = first else {
            panic!("期望 ClientUpdateRequest");
        };
        bridge.handle_control_message(ControlMessage::ClientUpdateChunk {
            transfer_id,
            offset: 0,
            data: payload,
            last: true,
        });
        let staged = bridge.take_staged().unwrap();
        assert!(staged.service_path.is_none());
        let _ = std::fs::remove_dir_all(staged.gui_path.parent().unwrap());
    }

    /// SHA 不匹配必须拒绝（绝不进入 staged）。
    #[test]
    fn bad_hash_is_rejected() {
        let (bridge, _rx) = temp_bridge();
        let payload = b"tampered".to_vec();
        let version = staged_version("bad");
        let mut file = wire_file("rust-tunnel-client-gui-windows-x86_64.exe", &payload);
        file.sha256 = "0".repeat(64);
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

    /// 选包规则：GUI 必选；服务 exe 只在 `want_service` 时认。
    #[test]
    fn select_offer_respects_want_service_and_platform() {
        let gui = ClientUpdateFile {
            name: "rust-tunnel-client-gui-windows-x86_64.exe".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            size: 1,
            sha256: "a".repeat(64),
        };
        let service = ClientUpdateFile {
            name: "rust-tunnel-client-service-windows-x86_64.exe".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            size: 1,
            sha256: "b".repeat(64),
        };
        let (some_gui, no_service) = select_offer(vec![gui.clone(), service.clone()], false);
        assert!(some_gui.is_some());
        assert!(no_service.is_none());
        let (_, some_service) = select_offer(vec![gui, service], true);
        assert!(some_service.is_some());
    }

    /// 偏移乱序必须中止当前下载并报错。
    #[test]
    fn out_of_order_chunk_aborts() {
        let (bridge, _rx) = temp_bridge();
        let payload = b"0123456789".to_vec();
        let version = staged_version("order");
        bridge.handle_control_message(ControlMessage::ClientUpdateUpToDate {
            version: "v1.0.0".into(),
        });
        bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
            version,
            files: vec![wire_file("rust-tunnel-client-gui-x", &payload)],
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
        for target in [UpdateTarget::Gui, UpdateTarget::Service] {
            let path = staging_path("../../etc", target);
            // 净化后可能还留着点号（如 `....etc`），但它必须是**单个文件名段**，
            // 不能出现 `..` 路径组件。
            assert!(path
                .components()
                .all(|c| !matches!(c, std::path::Component::ParentDir)));
            assert!(path.starts_with(std::env::temp_dir()));
        }
    }

    /// 本机装着服务、服务端却没给服务产物：**不更新**，并且把原因说出来。
    ///
    /// 这一格守的是「两端版本一致」：只换 GUI 会留下跑旧版的无人值守实例，而服务端
    /// 不会重复下发同一个版本，之后再也没有机会补齐。
    #[test]
    fn installed_service_without_a_service_artifact_blocks_the_update() {
        let mut bridge = UpdateBridge::for_test_with_service();
        Arc::get_mut(&mut bridge.inner)
            .expect("刚构造，独占")
            .service_installed_override = Some(true);
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        bridge.attach(tx);
        bridge.handle_control_message(ControlMessage::ClientUpdateUpToDate {
            version: "v1.0.0".into(),
        });
        // 手动检查才弹错误（自动检查只写日志）；这里走手动路径以便断言文案。
        bridge.check_now().unwrap();
        bridge.handle_control_message(ControlMessage::ClientUpdateAvailable {
            version: staged_version("blocked"),
            files: vec![wire_file(
                "rust-tunnel-client-gui-windows-x86_64.exe",
                b"gui",
            )],
        });
        let snap = bridge.snapshot();
        assert!(snap.available.is_none(), "不该允许半套更新");
        assert!(snap.error.unwrap().contains("服务"));
    }

    /// 安装前复验：内容变了就必须拒绝（这是「暂存文件在 UAC 期间被替换」的兜底）。
    #[test]
    fn verify_staged_rejects_tampering() {
        let dir = std::env::temp_dir().join(format!(
            "rust-tunnel-verify-{}-{}",
            std::process::id(),
            next_transfer_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payload.exe");
        std::fs::write(&path, b"payload").unwrap();
        let expected = sha256_hex(b"payload");
        assert!(verify_staged(&path, &expected).is_ok());
        std::fs::write(&path, b"tampered").unwrap();
        assert!(verify_staged(&path, &expected)
            .unwrap_err()
            .contains("发生变化"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `swap_in_file` 的换名次序：新文件先落 `.new`，旧文件改名 `.old`，再换入。
    #[test]
    fn swap_in_file_replaces_the_target_and_keeps_the_old_copy() {
        let dir = std::env::temp_dir().join(format!(
            "rust-tunnel-swap-{}-{}",
            std::process::id(),
            next_transfer_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.exe");
        let target = dir.join("target.exe");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&target, b"old").unwrap();

        let outcome = swap_in_file(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(&outcome.old).unwrap(), b"old");
        assert!(!dir.join(".target.exe.new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目标不存在时直接换入（全新安装），不该失败。
    #[test]
    fn swap_in_file_works_when_the_target_is_absent() {
        let dir = std::env::temp_dir().join(format!(
            "rust-tunnel-swap-absent-{}-{}",
            std::process::id(),
            next_transfer_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.exe");
        let target = dir.join("target.exe");
        std::fs::write(&source, b"new").unwrap();
        swap_in_file(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
