//! 实例（`--instance`）的采集 / 注入 / 剪贴板：`win-platform` 支撑 + **桌面绑定**的专用线程。
//!
//! # 为什么不能沿用 `xcap`/`enigo`（硬结论，别绕回去）
//!
//! `win-platform` 的采集走 `GetDC(NULL)`，`xcap` 走 `GetWindowDC(GetDesktopWindow())`
//! ——两者作用域都是**调用线程所在的桌面**。所以只有挂在目标桌面上的进程才采得到安全桌面。
//! `enigo` 也不能复用：它的 `move_mouse(Coordinate::Abs)` 用
//! `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`（仅主显示器）归一化且**没有**
//! `MOUSEEVENTF_VIRTUALDESK`，多显示器坐标本来就是错的；`win-platform` 的
//! `VirtualScreen::normalize_absolute` + `MOUSEEVENTF_VIRTUALDESK` 才是对的。
//!
//! # 线程模型（关键）
//!
//! 桌面归属是**线程级**状态（`SetThreadDesktop`），而 `win-platform` 的契约写得很死：
//! 挂上桌面之后**不能**在同一线程上换桌面，要换就**销毁整条线程重开**（见
//! `crates/win-platform/src/desktop.rs` 的模块文档与 `InputDesktop` 的实现）。于是这里
//! 分两条线程：
//!
//! - **监管线程**（`desktop-supervisor`，长驻）：以 ≤4Hz 探测输入桌面，发现变化就收掉旧
//!   worker、起一个新 worker。它自己**不挂任何桌面**，只做生命周期管理。
//! - **worker 线程**（`desktop-worker`，**一条线程一个桌面**）：挂到目标桌面后，
//!   **GDI `BitBlt` 采集**、注入、剪贴板在它上面跑——剪贴板同样是窗口站/桌面绑定的，
//!   绝不能留在调用线程上。`Default` 桌面的 WGC/DXGI 混合采集**不**走这条线程
//!   （它由 `win_hybrid` 在引擎的采集线程上做），所以 GPU 采集再慢也不会堵住注入。
//!
//!   例外是**安全桌面的实验性 DXGI 采集**（[`SecureDxgi`]，环境变量
//!   `RUST_TUNNEL_SECURE_DXGI`，默认关）：DXGI 的 duplication 也绑定调用线程所在的
//!   桌面，所以只可能在这条已挂到 `Winlogon`/`Screen-saver` 的线程上试。它只做 CPU
//!   回读，失败即回 `BitBlt`，并在本 worker 内限次放弃。
//!
//! 桌面变化是**低频事件**（锁屏/解锁/切换用户），故「销毁+重建」的开销可以忽略；真正要
//! 守住的是「切换期间绝不丢抬起事件」（见下）。当前挂载的桌面与显示器几何经
//! [`Shared`] 发布给 `win_hybrid`，后者据此在 GPU 快路径与 BitBlt 兜底之间分流。
//!
//! # 采集是请求/应答、注入是有界队列
//!
//! 注入跑在会话循环的 async worker 上，**绝不能阻塞它**（`crates/remote/src/controlled.rs`
//! 明确假设 `InputSink::apply` 是微秒级操作），故注入是「非阻塞投递 + 有界队列」，由 worker
//! 消费；而采集本身就在 `spawn_blocking` 里被调，可以阻塞等待结果（引擎另有 10s 看门狗
//! `CAPTURE_WATCHDOG` 兜底，本模块的等待上限刻意低于它）。
//!
//! # 切换桌面 / 会话结束时绝不丢抬起
//!
//! 旧 worker 退出前会在**它当时挂的桌面上**补发一次「全部抬起」（[`super::HeldInputs`]），
//! 否则旧桌面会残留一个按住不放的键/鼠标键。会话结束（尤其**硬断链**）同理：控制器侧的
//! `InputGate::release_all` 补发的抬起可能到不了，故 [`WinInput`] 实现了
//! [`super::SessionRelease`]，由实例运行时在观察到会话结束时触发。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rust_tunnel_remote::screen::CaptureInfo;
use rust_tunnel_remote::{
    Clipboard, DisplayInfo, InputEvent, InputSink, KeyEvent, MouseButton, RawFrame, RemoteError,
    RemoteResult, ScreenSource,
};
use rust_tunnel_win_platform::capture;
use rust_tunnel_win_platform::desktop::{self, InputDesktop};
use rust_tunnel_win_platform::inject::{self, InjectButton, InjectStep};
use rust_tunnel_win_platform::monitor::{self, Monitor, VirtualScreen};
use rust_tunnel_win_platform::WinError;
// 别名：本模块顶部的 `rust_tunnel_win_platform::capture`（GDI BitBlt）与 win-video 的
// `capture`（WGC/DXGI）重名，二者都在这条线程上被用到。
use rust_tunnel_win_video::capture as video_capture;
use rust_tunnel_win_video::pixels::{bgra_to_rgba, scale_bgra_to_rgba};

use super::desktop_follow::{self, FollowAction, FollowDesktop};
use super::input_queue::{HeldInputs, InputQueue, PushOutcome};
use super::instance_gpu::{self, Geometry};
use super::{keycode, PlatformHandles, SessionRelease};

/// 监管线程的桌面探测间隔（≤4Hz）。桌面变化是低频事件，按事件频率探测会白烧 CPU。
const DESKTOP_POLL_INTERVAL: Duration = Duration::from_millis(300);
/// worker 等待工作的步长（同时也是它检查停止/补发抬起请求的间隔）。
const WORKER_WAIT_STEP: Duration = Duration::from_millis(100);
/// worker 挂载目标桌面失败后的重试间隔（桌面可能正在变化中，值得重试）。
const ATTACH_RETRY_INTERVAL: Duration = Duration::from_millis(200);
/// 采集/剪贴板请求的等待上限。**必须小于**引擎的 10s 采集看门狗（`CAPTURE_WATCHDOG`），
/// 否则 worker 卡死时引擎会先判超时，而调用方还堵在这里。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// 注入队列容量（满时只丢 `MouseMove`，见 [`super::input_queue`]）。
const INPUT_QUEUE_CAP: usize = 256;
/// 安全桌面 DXGI 实验路径连续失败多少次就放弃（本 worker 生命周期内不再尝试）。
const SECURE_DXGI_MAX_FAILURES: u32 = 3;
/// 安全桌面 DXGI 失败后的重试间隔。
const SECURE_DXGI_RETRY: Duration = Duration::from_secs(2);
/// 「慢失败」阈值：单次 `capture()` 超过它即视为首帧超时/重建等待，**立即**放弃本 worker。
///
/// 这条 worker 同时承担注入，连续几次 2 秒的阻塞会让登录界面打字反复卡顿；
/// 快失败（`ACCESS_DENIED` 之类）仍按 [`SECURE_DXGI_MAX_FAILURES`] 限次重试。
const SECURE_DXGI_SLOW_FAILURE: Duration = Duration::from_secs(1);

// ── 请求类型 ────────────────────────────────────────────────────────

/// 一次采集请求（请求/应答）。
struct CaptureRequest {
    /// 目标显示器编号（`DisplayInfo::id`，即 `enumerate_monitors` 的枚举下标）。
    display: u32,
    /// 结果回传通道。
    reply: Sender<RemoteResult<RawFrame>>,
}

/// 一次剪贴板操作。
enum ClipboardOp {
    /// 读取本机剪贴板文本。
    Get,
    /// 写入本机剪贴板文本。
    Set(String),
}

/// 一次剪贴板请求（请求/应答；`set` 成功时回传空串）。
struct ClipboardRequest {
    /// 要做的操作。
    op: ClipboardOp,
    /// 结果回传通道。
    reply: Sender<RemoteResult<String>>,
}

/// 一批待办（worker 一次取走）。
struct Work {
    /// 待注入事件。
    inject: Vec<InputEvent>,
    /// 待处理的采集请求。
    captures: Vec<CaptureRequest>,
    /// 待处理的剪贴板请求。
    clipboard: Vec<ClipboardRequest>,
}

impl Work {
    /// 是否无事可做（无事可做时 worker 才去等）。
    fn is_empty(&self) -> bool {
        self.inject.is_empty() && self.captures.is_empty() && self.clipboard.is_empty()
    }
}

// ── 共享状态 ────────────────────────────────────────────────────────

/// worker 与调用方共享的状态。
struct IoInner {
    /// 待注入事件（非阻塞投递、有界）。
    inject: InputQueue,
    /// 待处理的采集请求。
    captures: VecDeque<CaptureRequest>,
    /// 待处理的剪贴板请求。
    clipboard: VecDeque<ClipboardRequest>,
    /// 显示器列表缓存（`ScreenSource::displays` **只读它**，绝不阻塞）。
    displays: Option<Vec<DisplayInfo>>,
    /// 显示器几何缓存（含原点，供 GPU 采集把实例 id 映射到 xcap id）。
    ///
    /// 与 `displays` 分开存：`DisplayInfo` 冻结在 `HelloAck` 的字节形状里，不能加
    /// 原点字段；而 GPU 路径需要几何来跨两套枚举做匹配。
    monitors: Option<Vec<Monitor>>,
    /// 当前 worker **实际挂着的**桌面（`None` = 尚未挂载/监管线程还没认下目标）。
    ///
    /// 只由监管线程在**成功起了 worker 之后**写入。探测到 `Unsupported` 时保持
    /// 旧值——与「保持当前挂载」的跟随语义一致（见 `desktop_follow::decide`）。
    desktop: Option<FollowDesktop>,
    /// 请求当前 worker 停止（监管线程换桌面、或进程收尾用）。
    stop: bool,
    /// 请求当前 worker 补发一次「全部抬起」（会话结束时由实例运行时触发）。
    release_requested: bool,
}

/// 共享状态 + 唤醒用的条件变量。
///
/// `pub(super)`：GPU 混合采集（`remote::win_hybrid`）要读当前桌面与显示器几何。
pub(super) struct Shared {
    /// 受锁保护的状态。
    inner: Mutex<IoInner>,
    /// 有新活/停止信号时唤醒 worker。
    cv: Condvar,
    /// worker 最近一帧**实际**的采集后端：`0` = GDI `BitBlt`（默认），`1` = 实验性 DXGI
    /// 安全桌面采集（见 [`SecureDxgi`]）。
    ///
    /// 刻意不放进 [`IoInner`]：`capture_info()` 由 1 Hz 心搏调用，必须无锁、非阻塞。
    secure_backend: AtomicU8,
}

impl Shared {
    /// 建共享状态并启动监管线程。
    fn start() -> Arc<Self> {
        let shared = Arc::new(Self {
            inner: Mutex::new(IoInner {
                inject: InputQueue::new(INPUT_QUEUE_CAP),
                captures: VecDeque::new(),
                clipboard: VecDeque::new(),
                displays: None,
                monitors: None,
                desktop: None,
                stop: false,
                release_requested: false,
            }),
            cv: Condvar::new(),
            secure_backend: AtomicU8::new(0),
        });
        let for_thread = Arc::clone(&shared);
        if let Err(err) = std::thread::Builder::new()
            .name("desktop-supervisor".to_owned())
            .spawn(move || supervisor_loop(&for_thread))
        {
            tracing::error!(%err, "起不了桌面监管线程，实例的采集/注入不可用");
        }
        shared
    }

    /// 加锁（容忍中毒：中毒说明某个线程 panic 过，但状态本身仍可用）。
    fn lock(&self) -> MutexGuard<'_, IoInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 显示器缓存（可能尚未就绪）。
    fn cached_displays(&self) -> Option<Vec<DisplayInfo>> {
        self.lock().displays.clone()
    }

    /// 更新显示器缓存（worker 在挂载成功/枚举成功后调用）。
    fn set_displays(&self, displays: Vec<DisplayInfo>) {
        self.lock().displays = Some(displays);
    }

    /// 更新显示器几何缓存（与 `set_displays` 同一次枚举发布）。
    pub(super) fn set_monitors(&self, monitors: Vec<Monitor>) {
        self.lock().monitors = Some(monitors);
    }

    /// 显示器几何缓存（可能尚未就绪；GPU 采集只读它，绝不阻塞）。
    pub(super) fn cached_monitors(&self) -> Option<Vec<Monitor>> {
        self.lock().monitors.clone()
    }

    /// 发布 worker 实际挂着的桌面（监管线程在 worker 起来之后调用）。
    ///
    /// 刻意**不在**探测到 `Unsupported` 时清除：那一刻 worker 仍挂在旧桌面上
    /// （`decide` 返回 `NoChange`），清掉会让 GPU 路径误判成安全桌面。
    pub(super) fn set_desktop(&self, desktop: Option<FollowDesktop>) {
        self.lock().desktop = desktop;
    }

    /// 当前 worker 实际挂着的桌面（`None` = 尚未挂载）。
    pub(super) fn desktop_kind(&self) -> Option<FollowDesktop> {
        self.lock().desktop
    }

    /// 发布最近一帧的采集后端（`true` = 实验性 DXGI，`false` = GDI `BitBlt`）。
    fn set_secure_backend(&self, dxgi: bool) {
        self.secure_backend.store(u8::from(dxgi), Ordering::Relaxed);
    }

    /// 最近一帧是否来自实验性 DXGI 采集。默认 `false`（BitBlt）。
    fn secure_backend(&self) -> bool {
        self.secure_backend.load(Ordering::Relaxed) != 0
    }

    /// 是否有**待注入**的输入事件（非阻塞，只读队列长度）。
    ///
    /// `serve_capture` 用它决定本轮要不要跳过实验性 DXGI：那条路径的首帧/重建可能
    /// 阻塞 worker 线程 2 秒以上，而这条线程同时承担注入——登录界面打字的手感优先。
    fn has_pending_input(&self) -> bool {
        !self.lock().inject.is_empty()
    }

    /// 请当前 worker 停止（它会先补发「全部抬起」）。
    fn request_stop(&self) {
        self.lock().stop = true;
        self.cv.notify_all();
    }

    /// 清除停止标志（监管线程为下一个 worker 复位）。
    fn clear_stop(&self) {
        self.lock().stop = false;
    }

    /// 当前是否被请求停止。
    fn stop_requested(&self) -> bool {
        self.lock().stop
    }

    /// 请求补发一次「全部抬起」（会话结束时由实例运行时调用）。
    fn request_release_all(&self) {
        self.lock().release_requested = true;
        self.cv.notify_all();
    }

    /// 取出并清空「补发抬起」请求。
    fn take_release_request(&self) -> bool {
        std::mem::take(&mut self.lock().release_requested)
    }

    /// **非阻塞**投递一条输入事件。
    fn push_input(&self, event: InputEvent) -> PushOutcome {
        let outcome = self.lock().inject.push(event);
        self.cv.notify_one();
        outcome
    }

    /// 取走一批待办。
    fn take_work(&self) -> Work {
        let mut guard = self.lock();
        Work {
            inject: guard.inject.drain(),
            captures: guard.captures.drain(..).collect(),
            clipboard: guard.clipboard.drain(..).collect(),
        }
    }

    /// 无活可干时睡一会儿（被唤醒或到点返回）。
    fn wait_for_work(&self, timeout: Duration) {
        let guard = self.lock();
        let idle = guard.inject.is_empty()
            && guard.captures.is_empty()
            && guard.clipboard.is_empty()
            && !guard.stop
            && !guard.release_requested;
        if idle {
            let _ = self.cv.wait_timeout(guard, timeout);
        }
    }

    /// 请求一次采集并阻塞等结果（调用方在 `spawn_blocking` 里，允许阻塞）。
    fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
        let (reply, rx) = channel();
        self.lock()
            .captures
            .push_back(CaptureRequest { display, reply });
        self.cv.notify_all();
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(RemoteError::Platform(
                "采集请求超时（桌面线程无响应）".to_owned(),
            )),
        }
    }

    /// 请求一次剪贴板操作并阻塞等结果。
    fn clipboard(&self, op: ClipboardOp) -> RemoteResult<String> {
        let (reply, rx) = channel();
        self.lock()
            .clipboard
            .push_back(ClipboardRequest { op, reply });
        self.cv.notify_all();
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(RemoteError::Platform(
                "剪贴板请求超时（桌面线程无响应）".to_owned(),
            )),
        }
    }
}

// ── 监管线程：桌面跟随 ──────────────────────────────────────────────

/// 探测**当前输入桌面**并分类（分类 + 原始桌面名）。
///
/// 走 `InputDesktop::open()` → 读名字 → **立即丢弃**：`Drop` 会在未挂载时关闭句柄，
/// 故这次探测不泄漏句柄（这正是「不要每次操作前 `OpenInputDesktop`」那条约束的落点——
/// 每事件一次会泄漏，每 300ms 一次不会）。
///
/// 原始名字一并回传，只为**诊断**：`Unsupported` 有好几种（`Disconnect` / 未知名），
/// 没有名字时日志只能说「不支持」，现场根本分不清是哪种桌面把跟随卡住了。
fn probe_input_desktop() -> Option<(FollowDesktop, String)> {
    match InputDesktop::open() {
        Ok(desktop) => {
            let name = desktop.name().to_owned();
            Some((desktop_follow::follow_of_name(&name), name))
        }
        Err(err) => {
            tracing::debug!(%err, "探测输入桌面失败（非 SYSTEM 进程锁屏时会这样）");
            None
        }
    }
}

/// 监管线程主体：按 ≤4Hz 探测输入桌面，桌面变化（或 worker 意外退出）就重建 worker。
///
/// 它**自己从不挂桌面**（不调 `SetThreadDesktop`），所以可以在整个进程生命周期内长驻，
/// 不必随桌面变化重建。跟随目标（`Default` / `Winlogon` / `Screen-saver`）由
/// [`super::desktop_follow`] 决定；这里只负责按决策重建 worker。
fn supervisor_loop(shared: &Arc<Shared>) {
    let mut worker: Option<JoinHandle<()>> = None;
    let mut current: Option<FollowDesktop> = None;
    // 上一次「输入桌面不在跟随目标里」时见到的桌面名，**只用于日志去重**：探测是
    // 300ms 一次，而 `Disconnect`/未知名可能长时间保持，不去重就是每秒三行刷屏。
    let mut last_unsupported: Option<String> = None;
    loop {
        let observed = probe_input_desktop();
        if let Some((kind, name)) = observed.as_ref() {
            if matches!(kind, FollowDesktop::Unsupported) {
                if last_unsupported.as_deref() != Some(name.as_str()) {
                    tracing::info!(desktop = %name, "输入桌面不是跟随目标，保持当前挂载");
                    last_unsupported = Some(name.clone());
                }
            } else {
                // 回到可跟随目标（含 Screen-saver）：下一种 Unsupported 要重新记一次。
                last_unsupported = None;
            }
        }
        let action = desktop_follow::decide(current, observed.as_ref().map(|(kind, _)| *kind));
        // worker 意外退出（panic）也必须重建：否则实例会**静默**失去采集/注入能力——
        // 服务端看到的是一个在线但怎么都控不了的本机。
        let worker_died = worker.as_ref().is_some_and(JoinHandle::is_finished);
        if matches!(action, FollowAction::Attach(_)) || worker_died {
            // 先收掉旧 worker：它会先在**旧**桌面上补发「全部抬起」再退出。
            if let Some(handle) = worker.take() {
                shared.request_stop();
                let _ = handle.join();
            }
            // 复位停止标志，供新 worker 使用。
            shared.clear_stop();
            let target = match action {
                FollowAction::Attach(target) => target,
                // 桌面没变、只是 worker 死了：重建到当前目标。
                FollowAction::NoChange => current.unwrap_or(FollowDesktop::Default),
            };
            tracing::info!(desktop = target.as_str(), worker_died, "重建采集/注入线程");
            worker = spawn_worker(Arc::clone(shared), target);
            // 只在**确实起了线程**时才认下这个目标：起失败时保持 `None`，下一轮会重试。
            current = worker.as_ref().map(|_| target);
            if worker.is_some() {
                // 采集策略据此判断能不能走 WGC/DXGI；必须晚于 worker 起来（那时挂载
                // 已成功或正在重试），且只在真正认下目标时写。
                shared.set_desktop(Some(target));
            }
        }
        std::thread::sleep(DESKTOP_POLL_INTERVAL);
    }
}

/// 起一条 worker 线程（一条线程一个桌面）。
fn spawn_worker(shared: Arc<Shared>, target: FollowDesktop) -> Option<JoinHandle<()>> {
    match std::thread::Builder::new()
        .name("desktop-worker".to_owned())
        .spawn(move || worker_loop(&shared, target))
    {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::error!(%err, desktop = target.as_str(), "起不了桌面 worker 线程");
            None
        }
    }
}

// ── worker 线程：采集 / 注入 / 剪贴板 ───────────────────────────────

/// 把当前线程挂到目标桌面。
///
/// **fail-closed**：`OpenInputDesktop` 成功只说明**拿到**了句柄，线程未必真的切过去了；
/// 挂载后必须重读 `GetThreadDesktop` 比对（见 `win-platform` 的 `desktop` 契约），
/// 不相等就不许注入——在错误的桌面上做出来的成功读数毫无意义。
fn try_attach(target: FollowDesktop) -> Result<InputDesktop, String> {
    let mut desktop =
        InputDesktop::open().map_err(|err| format!("OpenInputDesktop 失败：{err}"))?;
    if desktop_follow::follow_of_name(desktop.name()) != target {
        return Err(format!(
            "当前输入桌面是 {}，不是目标 {}（桌面又变了）",
            desktop.name(),
            target.as_str()
        ));
    }
    desktop
        .attach_to_current_thread()
        .map_err(|err| format!("SetThreadDesktop 失败：{err}"))?;
    // 挂载成功之后，**任何**失败路径都必须显式关掉句柄：`InputDesktop` 的 `Drop` 对
    // 已挂载的句柄刻意不关（见 win-platform 契约），不显式关就会泄漏一个 HDESK。
    let name = match desktop::current_thread_desktop_name() {
        Ok(name) => name,
        Err(err) => {
            desktop.close_on_exit();
            return Err(format!("读线程桌面名失败：{err}"));
        }
    };
    if desktop_follow::follow_of_name(&name) != target {
        desktop.close_on_exit();
        return Err(format!(
            "挂载后线程桌面是 {name}，不等于目标 {}",
            target.as_str()
        ));
    }
    Ok(desktop)
}

/// 反复尝试挂载目标桌面，直到成功或被要求停止。
fn attach_with_retry(shared: &Shared, target: FollowDesktop) -> Option<InputDesktop> {
    loop {
        if shared.stop_requested() {
            return None;
        }
        match try_attach(target) {
            Ok(desktop) => return Some(desktop),
            Err(message) => tracing::debug!(desktop = target.as_str(), "{message}"),
        }
        std::thread::sleep(ATTACH_RETRY_INTERVAL);
    }
}

/// worker 线程主体。
///
/// # 为什么整段在一个函数里
///
/// 采集/注入/剪贴板都必须在**这一条**线程上、按**同一顺序**执行；把它们拆成回调只会让
/// 「谁在哪个线程上」这件事更难看清。桌面包的局部状态（`held` / `monitors` / `vs`）也
/// 天然属于这条线程。
#[allow(
    clippy::too_many_lines,
    reason = "桌面 worker 的完整生命周期（挂载 → 服务 → 收尾补发抬起 → 回收句柄），拆开反而看不清线程归属"
)]
fn worker_loop(shared: &Shared, target: FollowDesktop) {
    // 遥测口径先复位：必须在挂载之前做——旧 worker 可能刚用 DXGI 采过帧，而新 worker
    // 挂载失败/重试期间不该继续上报旧口径。第一帧 DXGI 真正成功之前它就是 BitBlt。
    shared.set_secure_backend(false);
    let Some(mut desktop) = attach_with_retry(shared, target) else {
        tracing::info!(desktop = target.as_str(), "worker 未挂载即被要求停止");
        return;
    };
    tracing::info!(
        desktop = target.as_str(),
        "采集/注入/剪贴板线程已挂到目标桌面"
    );

    // 显示器几何在这条线程上枚举一次（本线程挂的桌面上的几何）。
    let (monitors, vs) = enumerate_screen();
    publish_displays(shared, &monitors);
    // 同一份几何也给 GPU 混合采集：它需要原点来把实例 id 映射到 xcap id。
    shared.set_monitors(monitors.clone());

    let mut held = HeldInputs::default();
    // 安全桌面的实验性 DXGI 采集（`Default` 桌面不启用：那条路径由 `win_hybrid` 管）。
    let mut secure = SecureDxgi::for_worker(target);
    loop {
        if shared.stop_requested() {
            // 切换桌面 / 退出前补发一次「全部抬起」——在**当前**桌面上，否则旧桌面残留卡键。
            release_held(&mut held, &monitors, &vs);
            break;
        }
        // 会话结束（尤其硬断链）时实例运行时也会请求一次「全部抬起」。
        if shared.take_release_request() {
            release_held(&mut held, &monitors, &vs);
        }

        let work = shared.take_work();
        // 先记下「这一批是不是空的」：下面的 `for req in work.captures / work.clipboard`
        // 会把字段移走，之后就不能再借 `work` 了。
        let idle = work.is_empty();
        if !work.inject.is_empty() {
            for event in &work.inject {
                held.observe(event);
            }
            inject_events(&work.inject, &monitors, &vs);
        }
        for req in work.captures {
            serve_capture(&monitors, &mut secure, shared, req);
        }
        for req in work.clipboard {
            serve_clipboard(req);
        }
        if idle {
            shared.wait_for_work(WORKER_WAIT_STEP);
        }
    }

    // 尽力回收桌面句柄：`InputDesktop` 的 `Drop` 对已挂载的句柄刻意不关（见 win-platform
    // 契约），不显式关的话每次桌面切换都会留下一个 HDESK。
    //
    // **已知局限（如实）**：`CloseDesktop` 对「仍被本线程使用」的桌面会失败，而此刻本线程
    // 还挂着它（线程正要退出），所以这一关**可能不生效**——那就每次桌面切换泄漏一个句柄。
    // 要彻底消除需要二选一：给 `InputDesktop` 加 `unsafe impl Send`（HDESK 是进程级句柄，
    // 移交给监管线程在 `join()` 之后关），或在 win-platform 暴露一个「关原始句柄」的入口。
    // 两者都动 win-platform 的安全面，故留给该 crate 的负责人定夺（见交付报告）。
    desktop.close_on_exit();
    tracing::info!(desktop = target.as_str(), "采集/注入/剪贴板线程退出");
}

/// 补发一次「全部抬起」（幂等：没有按住项时是空操作）。
fn release_held(held: &mut HeldInputs, monitors: &[Monitor], vs: &VirtualScreen) {
    if held.is_empty() {
        return;
    }
    let releases = held.release_all();
    tracing::info!(count = releases.len(), "补发「全部抬起」");
    inject_events(&releases, monitors, vs);
}

/// 枚举显示器与虚拟桌面几何。
fn enumerate_screen() -> (Vec<Monitor>, VirtualScreen) {
    let monitors = monitor::enumerate_monitors().unwrap_or_default();
    let vs = VirtualScreen::from_monitors(&monitors).unwrap_or(VirtualScreen {
        origin_x: 0,
        origin_y: 0,
        width: 0,
        height: 0,
    });
    (monitors, vs)
}

/// 把显示器列表发布到共享缓存（`ScreenSource::displays` 只读它）。
fn publish_displays(shared: &Shared, monitors: &[Monitor]) {
    let displays: Vec<DisplayInfo> = monitors.iter().map(monitor_to_info).collect();
    if !displays.is_empty() {
        shared.set_displays(displays);
    }
}

/// `win-platform` 的显示器 → 引擎的 [`DisplayInfo`]（id 即枚举下标，两端一致）。
fn monitor_to_info(monitor: &Monitor) -> DisplayInfo {
    DisplayInfo {
        id: monitor.id,
        width: monitor.width,
        height: monitor.height,
        name: format!("显示器 {}", monitor.id.saturating_add(1)),
        primary: monitor.primary,
    }
}

/// 把事件序列注入到当前桌面。
fn inject_events(events: &[InputEvent], monitors: &[Monitor], vs: &VirtualScreen) {
    let steps = events_to_steps(events, monitors);
    if steps.is_empty() {
        return;
    }
    match inject::send_steps(&steps, vs) {
        Ok(outcome) if outcome.fully_accepted() => {}
        // UIPI 拦截的典型形态：事件一个没进去，但 `GetLastError` 常常是 0，
        // **只有计数**能看出来（见 `win-platform::inject` 的模块文档）。
        Ok(outcome) => {
            tracing::warn!(summary = %outcome.summary(), "部分注入事件未被接受（UIPI / 桌面 ACL？）");
        }
        Err(err) => tracing::warn!(%err, "注入调用失败"),
    }
}

/// 引擎输入事件 → `win-platform` 注入步骤。
///
/// 滚轮方向按 **`enigo` 的既有语义**换算，保证实例与 GUI 路径对同一个
/// [`InputEvent::Wheel`] 产生**同样的物理滚动**：`enigo` 竖向发
/// `mouseData = -dy * WHEEL_DELTA`、横向发 `dx * WHEEL_DELTA`（见 `enigo-0.6.1` 的
/// `win_impl.rs::scroll`），而 `win-platform` 的 `Wheel`/`HWheel` 直接把 `delta` 当
/// `mouseData`。
fn events_to_steps(events: &[InputEvent], monitors: &[Monitor]) -> Vec<InjectStep> {
    let mut steps = Vec::new();
    for event in events {
        match event {
            InputEvent::MouseMove { x, y, display } => {
                let Some(monitor) = monitors.iter().find(|m| m.id == *display) else {
                    // 控制器手里的显示器列表可能比本机旧（拔显示器/改分辨率）：忽略这次移动，
                    // 绝不猜一个别的显示器——那会把鼠标送到错误的位置。
                    // 注意先把值取进局部变量：`tracing` 的宏展开里有 `display` 这个名字，
                    // 直接在宏参数里写 `display` 会被它盖住（报「fn item 不能解引用」）。
                    let missing = *display;
                    tracing::debug!("控制器请求了不存在的显示器 {missing}，忽略这次移动");
                    continue;
                };
                let (gx, gy) = monitor.to_global(*x, *y);
                steps.push(InjectStep::MoveTo { x: gx, y: gy });
            }
            InputEvent::MouseButton { button, pressed } => steps.push(InjectStep::Button {
                button: inject_button(*button),
                pressed: *pressed,
            }),
            InputEvent::Wheel { dx, dy } => {
                if *dy != 0 {
                    steps.push(InjectStep::Wheel {
                        delta: dy.saturating_neg().saturating_mul(inject::WHEEL_DELTA),
                    });
                }
                if *dx != 0 {
                    steps.push(InjectStep::HWheel {
                        delta: dx.saturating_mul(inject::WHEEL_DELTA),
                    });
                }
            }
            InputEvent::Key(event) => {
                if event.code == keycode::TEXT {
                    // 文本键是「点按」语义：走 `KEYEVENTF_UNICODE`，不受键盘布局影响。
                    if let Some(text) = event.text.as_deref() {
                        steps.push(InjectStep::Text {
                            utf16: text.encode_utf16().collect(),
                        });
                    }
                } else if let Some(vk) = super::code_to_vk(event.code) {
                    steps.push(InjectStep::KeyVk {
                        vk,
                        pressed: event.pressed,
                    });
                }
            }
        }
    }
    steps
}

/// 引擎鼠标键 → `win-platform` 鼠标键。
fn inject_button(button: MouseButton) -> InjectButton {
    match button {
        MouseButton::Left => InjectButton::Left,
        MouseButton::Right => InjectButton::Right,
        MouseButton::Middle => InjectButton::Middle,
    }
}

/// 服务一次采集请求（在本线程的桌面上 `BitBlt`）。
///
/// 先试实验性 DXGI（仅安全桌面 worker 启用，见 [`SecureDxgi`]）；未启用/失败才回退
/// GDI `BitBlt`——后者在安全桌面上是唯一**可靠**的路径，因此永远是兜底。
fn serve_capture(
    monitors: &[Monitor],
    secure: &mut SecureDxgi,
    shared: &Shared,
    req: CaptureRequest,
) {
    // 解构取走 `reply`（`Sender::send` 只借 `&self`，不解构的话 `req` 不会被消耗）。
    let CaptureRequest { display, reply } = req;
    let Some(monitor) = monitors.iter().find(|m| m.id == display) else {
        let _ = reply.send(Err(RemoteError::Platform(format!(
            "显示器 {display} 不存在"
        ))));
        return;
    };
    // 有输入待注入时**本轮不试 DXGI**：这条 worker 同时承担注入，而 DXGI 的首帧/重建
    // 可能阻塞 2 秒以上（见 [`SecureDxgi`]）——登录界面打字的手感优先，下一拍没输入
    // 时再回到 DXGI。
    let dxgi = if shared.has_pending_input() {
        None
    } else {
        secure.capture(display, monitors)
    };
    if let Some(frame) = dxgi {
        // 遥测如实：这一帧真的来自 DXGI。
        shared.set_secure_backend(true);
        let _ = reply.send(Ok(frame));
        return;
    }
    // 回退帧（包括「实验开关没开」的常态）：口径回到 BitBlt。
    shared.set_secure_backend(false);
    let result = capture::capture_monitor(monitor)
        .map(|frame| RawFrame {
            display,
            width: frame.width,
            height: frame.height,
            rgba: frame.rgba(),
            gpu: None,
            // GDI `BitBlt` 没有合成器变化信号，按旧语义恒为「可能变化」。
            changed: true,
            // 也没有便宜的像素级变化面积：按满帧处理（不压制码率）。
            changed_permille: 1000,
        })
        .map_err(platform_err);
    // 接收端可能已超时放弃（`recv_timeout`）；发送失败不是错误。
    let _ = reply.send(result);
}

/// 安全桌面（`Winlogon` / `Screen-saver`）上的**实验性** DXGI 采集。
///
/// # 为什么是「实验性、机会主义」
///
/// 安全桌面上 WGC 不可用（捕获项依赖会话的合成器），而 DXGI Desktop Duplication
/// 能不能用取决于驱动与系统：公开实现里有人成功，也有人一律
/// `DXGI_ERROR_ACCESS_DENIED`。所以这里**只在显式打开开关时**尝试
/// （`RUST_TUNNEL_SECURE_DXGI`，默认关），任何失败立刻回 GDI `BitBlt`，并在本
/// worker 生命周期内限次放弃——它不能让「登录界面本来就慢」变得更慢。
///
/// # 明确的范围
///
/// 只做 **CPU 回读**（[`video_capture::CaptureSource::capture`] → BGRA），**不碰**
/// GPU 纹理 / 零拷贝：安全桌面上没有可用的硬件编码路径，这条路径的收益只是
/// 「DXGI 回读比 GDI `BitBlt` 快」，不是编码链路升级。因此 `RawFrame.gpu` 恒为
/// `None`，引擎的 `gpu_capture` 也保持 `false`。
///
/// # 状态归属
///
/// 全部字段都属于**这一条 worker 线程**（不跨线程、不加锁）：worker 在桌面切换时
/// 本来就会被销毁重建，下次锁屏会重新给 DXGI 一次机会。
struct SecureDxgi {
    /// 本 worker 是否启用（开关打开 **且** 目标桌面不是 `Default`）。
    enabled: bool,
    /// 已打开的 DXGI 采集源（懒打开）。
    source: Option<Box<dyn video_capture::CaptureSource>>,
    /// 当前源绑定的实例显示器 id（控制器切显示器时必须重开）。
    display: Option<u32>,
    /// 连续失败次数（成功一帧清零）。
    failures: u32,
    /// 重试退避截止时刻：之前不再尝试（避免每次采集都去撞同一个失败）。
    retry_at: Option<Instant>,
    /// 连续失败到上限后置位：本 worker 剩余生命周期内不再尝试。
    disabled: bool,
}

impl SecureDxgi {
    /// 由目标桌面构造（**每个 worker 启动时读一次环境变量**）。
    fn for_worker(target: FollowDesktop) -> Self {
        // `Default` 桌面不启用：那条路径由 `win_hybrid` 的 WGC/DXGI + GPU 纹理负责，
        // 这里再叠一层只会打架。
        let enabled = target != FollowDesktop::Default && env_flag("RUST_TUNNEL_SECURE_DXGI");
        if enabled {
            tracing::info!(
                desktop = target.as_str(),
                "安全桌面 DXGI 采集已开启（实验性：CPU 回读，失败回退 GDI BitBlt）"
            );
        }
        Self {
            enabled,
            source: None,
            display: None,
            failures: 0,
            retry_at: None,
            disabled: false,
        }
    }

    /// 尝试用 DXGI 采一帧；任何失败（或不该尝试）都返回 `None`，由调用方回 BitBlt。
    fn capture(&mut self, display_id: u32, monitors: &[Monitor]) -> Option<RawFrame> {
        if !self.enabled || self.disabled {
            return None;
        }
        // 控制器切了显示器：旧源绑的是另一块 DXGI 输出，必须重开。失败计数与退避
        // 按显示器重新记账（那是另一块屏的账），但 `disabled` **刻意不重置**——
        // 它是 **worker 级**的「这条实验路径已经卡过太久，本 worker 内不再试」，
        // 换显示器不该让登录界面再挨一次 2 秒的阻塞。
        if self.display != Some(display_id) {
            self.source = None;
            self.display = Some(display_id);
            self.failures = 0;
            self.retry_at = None;
        }
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return None;
        }
        if self.source.is_none() {
            match Self::open(display_id, monitors) {
                Ok(source) => self.source = Some(source),
                Err(err) => {
                    self.record_failure(display_id, &err);
                    return None;
                }
            }
        }
        let source = self.source.as_mut()?;
        let started = Instant::now();
        match source.capture() {
            Ok(frame) => {
                // 成功一帧：清零失败计数与退避。
                self.failures = 0;
                self.retry_at = None;
                Some(secure_frame(display_id, &frame))
            }
            Err(err) => {
                let elapsed = started.elapsed();
                if elapsed >= SECURE_DXGI_SLOW_FAILURE {
                    // **慢失败立即放弃**：`capture()` 只在首帧超时（2s）或设备丢失后
                    // 重建等待时才会卡这么久。这条 worker 同时承担注入，再来两次
                    // 意味着登录界面打字被反复卡 2 秒——本 worker 内直接不再试。
                    self.source = None;
                    if !self.disabled {
                        self.disabled = true;
                        tracing::warn!(
                            display_id,
                            elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                            error = %err,
                            "安全桌面 DXGI 采集慢失败，本 worker 剩余生命周期回退 GDI BitBlt"
                        );
                    }
                } else {
                    self.record_failure(display_id, &err.to_string());
                }
                None
            }
        }
    }

    /// 懒打开目标显示器的 DXGI 采集源。
    fn open(
        display_id: u32,
        monitors: &[Monitor],
    ) -> Result<Box<dyn video_capture::CaptureSource>, String> {
        let enumerated = video_capture::enumerate_monitors()
            .map_err(|err| format!("win-video 枚举显示器失败: {err}"))?;
        let info = match_video_monitor(display_id, monitors, &enumerated)
            .ok_or_else(|| format!("显示器 {display_id} 没有对应的 DXGI 输出"))?;
        video_capture::open_dxgi(info).map_err(|err| format!("打开 DXGI 采集源失败: {err}"))
    }

    /// 记一次失败：丢源、退避、累计到上限后放弃（本 worker 内）。
    fn record_failure(&mut self, display_id: u32, error: &str) {
        self.source = None;
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(Instant::now() + SECURE_DXGI_RETRY);
        if self.failures >= SECURE_DXGI_MAX_FAILURES {
            if !self.disabled {
                self.disabled = true;
                tracing::warn!(
                    display_id,
                    failures = self.failures,
                    error,
                    "安全桌面 DXGI 采集连续失败，本 worker 剩余生命周期回退 GDI BitBlt"
                );
            }
        } else {
            tracing::debug!(
                display_id,
                failures = self.failures,
                error,
                "安全桌面 DXGI 采集失败，本帧回退 GDI BitBlt"
            );
        }
    }
}

/// 读一个布尔型现场开关：**未设置 / 空串 / `"0"` 为关闭**，其它值为开启。
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| {
        let text = value.to_string_lossy();
        let text = text.trim();
        !text.is_empty() && text != "0"
    })
}

/// 实例显示器 id → win-video 的显示器枚举项（**几何匹配优先、枚举下标兜底**）。
///
/// 两套枚举都源自 `EnumDisplayMonitors` 且跑在同一个进程（DPI 感知上下文一致），
/// 几何通常逐值相同；复用 [`instance_gpu::map_display_ids`] 保证与 `win_hybrid` 的
/// id 映射规则完全一致（包括「同一条候选只能被一个显示器认领」）。
fn match_video_monitor<'a>(
    display_id: u32,
    monitors: &[Monitor],
    candidates: &'a [video_capture::MonitorInfo],
) -> Option<&'a video_capture::MonitorInfo> {
    let platform: Vec<(u32, Geometry)> = monitors
        .iter()
        .map(|monitor| {
            (
                monitor.id,
                Geometry {
                    x: monitor.origin_x,
                    y: monitor.origin_y,
                    width: monitor.width,
                    height: monitor.height,
                },
            )
        })
        .collect();
    let video: Vec<(u32, Geometry)> = candidates
        .iter()
        .filter_map(|monitor| {
            Some((
                u32::try_from(monitor.index).ok()?,
                Geometry {
                    x: monitor.left,
                    y: monitor.top,
                    width: monitor.width,
                    height: monitor.height,
                },
            ))
        })
        .collect();
    let mapped = instance_gpu::map_display_ids(&platform, &video)
        .get(&display_id)
        .copied()?;
    candidates
        .iter()
        .find(|monitor| u32::try_from(monitor.index).ok() == Some(mapped))
}

/// DXGI 帧 → 引擎帧：按与 BitBlt 路径**一致**的规则封顶到
/// [`MAX_ENCODE_WIDTH`](rust_tunnel_remote::video::MAX_ENCODE_WIDTH) 并转 RGBA。
///
/// 尺寸一律走 [`instance_gpu::clamp_dims`]（整数除法 + 偶数对齐）：`win_hybrid` 的
/// BitBlt 路径用的是同一个纯函数，两条路径逐像素一致——否则来回切换时尺寸差 1–2 像素
/// 会触发编码器重建 + IDR。
///
/// 缩放失败（极少见）时按**原始尺寸**交付：两个返回尺寸必须与 `rgba` 的实际长度
/// 一致，否则编码器会按错尺寸解像素——宁可慢，不可错。
fn secure_frame(display_id: u32, frame: &video_capture::CapturedFrame) -> RawFrame {
    let max = rust_tunnel_remote::video::MAX_ENCODE_WIDTH;
    let (width, height) = instance_gpu::clamp_dims(frame.width, frame.height, max);
    let (width, height, rgba) = if (width, height) == (frame.width, frame.height) {
        (width, height, bgra_to_rgba(&frame.bgra))
    } else {
        match scale_bgra_to_rgba(&frame.bgra, frame.width, frame.height, width, height) {
            Ok(rgba) => (width, height, rgba),
            Err(err) => {
                tracing::debug!(error = %err, "安全桌面 DXGI 帧缩放失败，按原尺寸交付");
                (frame.width, frame.height, bgra_to_rgba(&frame.bgra))
            }
        }
    };
    RawFrame {
        display: display_id,
        width,
        height,
        rgba,
        // 明确：这条路径只有 CPU 回读，没有零拷贝。
        gpu: None,
        // DXGI 回读没有合成器变化信号（也省掉逐帧 memcmp），按「可能变化」处理。
        changed: true,
        changed_permille: 1000,
    }
}

/// 服务一次剪贴板请求（在**本线程**上读写本机剪贴板）。
///
/// 剪贴板同样是窗口站绑定的：留在调用线程上会在错误的地方读写（锁屏时尤其明显）。
fn serve_clipboard(req: ClipboardRequest) {
    let ClipboardRequest { op, reply } = req;
    let result = match op {
        ClipboardOp::Get => read_clipboard(),
        // `set` 成功时回传空串（接收方只关心成功与否）。
        ClipboardOp::Set(text) => write_clipboard(&text).map(|()| String::new()),
    };
    let _ = reply.send(result);
}

/// 读本机剪贴板文本（`arboard`，在 worker 线程上执行）。
///
/// `arboard` 的 Windows 后端只是 `OpenClipboard(null)` + `GetClipboardData`，**不建窗口、
/// 不需要消息循环**，故可以安全地跑在这条已挂桌面的线程上（`clipboard-win-5.4.1` 的
/// `Clipboard::new` 实现）。
fn read_clipboard() -> RemoteResult<String> {
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.get_text())
        .map_err(|err| RemoteError::Platform(format!("读取剪贴板失败: {err}")))
}

/// 写本机剪贴板文本（`arboard`，在 worker 线程上执行）。
fn write_clipboard(text: &str) -> RemoteResult<()> {
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text.to_owned()))
        .map_err(|err| RemoteError::Platform(format!("写入剪贴板失败: {err}")))
}

/// `win-platform` 错误 → 引擎错误。
#[expect(
    clippy::needless_pass_by_value,
    reason = "直接作为 map_err 的函数指针使用，按值接收更贴合签名"
)]
fn platform_err(err: WinError) -> RemoteError {
    RemoteError::Platform(format!("win-platform: {err}"))
}

// ── 引擎侧的三个 trait 实现 ─────────────────────────────────────────

/// `win-platform` 支撑的采集源（实例的 GDI `BitBlt` 路径）。
///
/// `pub(super)`：GPU 混合采集（`remote::win_hybrid`）把它当作安全桌面与所有
/// 回退场景的兜底。
pub(super) struct WinScreen {
    /// 与 worker 共享的状态。
    io: Arc<Shared>,
}

impl WinScreen {
    /// 由共享状态构造（不启动任何线程；线程由 [`screen_and_input`] 启动）。
    pub(super) fn new(io: Arc<Shared>) -> Self {
        Self { io }
    }
}

impl ScreenSource for WinScreen {
    /// 显示器列表。
    ///
    /// # 绝不阻塞
    ///
    /// 它被 `RemoteManager::display_count()` **同步**调用，而后者被 `announce()` 调用——
    /// 可能在 tokio worker 或 UI 线程上。所以这里**只读缓存**（由 worker 刷新），缓存还没
    /// 就绪时用一次本地探测兜底（`EnumDisplayMonitors` 是同步快调用、与桌面无关）。
    /// **绝不返回空 `Vec`**：`RemoteAnnounce.displays = 0` 会让服务端认为本机不可被控。
    fn displays(&self) -> Vec<DisplayInfo> {
        if let Some(cached) = self.io.cached_displays() {
            if !cached.is_empty() {
                return cached;
            }
        }
        let local = local_displays();
        if !local.is_empty() {
            return local;
        }
        // 连枚举都失败（罕见）：给一条占位，保住「可被控」这个状态；真正采集时会报错。
        vec![fallback_display()]
    }

    fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
        self.io.capture(display)
    }

    /// 被控端遥测口径：这条源负责 `BitBlt`（安全桌面与所有回退场景），**除了**
    /// 实验性安全桌面 DXGI 真正产出过帧的情况——那时如实报 `DXGI`（仍是 CPU 回读，
    /// 没有零拷贝，`gpu_capture` 保持 `false`）。`Default` 桌面的 WGC/DXGI 遥测由
    /// `win_hybrid` 按最近一帧的实际来源选择。
    fn capture_info(&self) -> CaptureInfo {
        let backend = if self.io.secure_backend() {
            "DXGI"
        } else {
            "BitBlt"
        };
        CaptureInfo {
            backend: backend.to_owned(),
            ..CaptureInfo::default()
        }
    }
}

/// 本地（不经过 worker）枚举显示器——只用于 `displays()` 的兜底。
fn local_displays() -> Vec<DisplayInfo> {
    monitor::enumerate_monitors().map_or_else(
        |_| Vec::new(),
        |monitors| monitors.iter().map(monitor_to_info).collect(),
    )
}

/// 枚举失败时的占位显示器（见 [`WinScreen::displays`] 的说明）。
fn fallback_display() -> DisplayInfo {
    DisplayInfo {
        id: 0,
        width: 1920,
        height: 1080,
        name: "显示器 1".to_owned(),
        primary: true,
    }
}

/// `win-platform` 支撑的注入目标（实例用）。
struct WinInput {
    /// 与 worker 共享的状态。
    io: Arc<Shared>,
}

impl WinInput {
    /// **非阻塞**投递一条事件（投递失败不会发生——丢的只可能是 `MouseMove`，
    /// 见 [`super::input_queue`]），故不返回 `Result`，由各 trait 方法包成 `Ok`。
    fn push(&self, event: InputEvent) {
        match self.io.push_input(event) {
            PushOutcome::Queued | PushOutcome::EvictedMove => {}
            PushOutcome::DroppedMove | PushOutcome::Overflowed => {
                tracing::debug!("输入队列背压，事件被丢弃或超界");
            }
        }
    }
}

impl InputSink for WinInput {
    fn mouse_move(&self, x: i32, y: i32, display: u32) -> RemoteResult<()> {
        self.push(InputEvent::MouseMove { x, y, display });
        Ok(())
    }

    fn mouse_button(&self, button: MouseButton, pressed: bool) -> RemoteResult<()> {
        self.push(InputEvent::MouseButton { button, pressed });
        Ok(())
    }

    fn wheel(&self, dx: i32, dy: i32) -> RemoteResult<()> {
        self.push(InputEvent::Wheel { dx, dy });
        Ok(())
    }

    fn key(&self, event: KeyEvent) -> RemoteResult<()> {
        self.push(InputEvent::Key(event));
        Ok(())
    }

    fn lock_screen(&self) -> RemoteResult<()> {
        // 不需要挂载桌面：`LockWorkStation` 作用于**进程所在的交互式会话**，而实例进程
        // 本就以 SYSTEM 运行在控制台会话里（桌面挂载只影响采集/注入）。
        super::lock_platform_screen()
    }

    /// 覆写默认分发：直接投递整条事件，避免再拆一遍（引擎调用的就是这个入口）。
    fn apply(&self, event: InputEvent) -> RemoteResult<()> {
        self.push(event);
        Ok(())
    }
}

impl SessionRelease for WinInput {
    /// 请求 worker 补发一次「全部抬起」。
    ///
    /// 走队列/标志而不是直接注入：必须由 worker 在**它当前挂的桌面**上执行，并与其它注入
    /// 保持同一条线程与顺序。
    fn release_all(&self) {
        self.io.request_release_all();
    }
}

/// `win-platform` 支撑的剪贴板（实例用；操作在 worker 线程上执行）。
struct WinClipboard {
    /// 与 worker 共享的状态。
    io: Arc<Shared>,
}

impl Clipboard for WinClipboard {
    fn get(&self) -> RemoteResult<String> {
        self.io.clipboard(ClipboardOp::Get)
    }

    fn set(&self, text: &str) -> RemoteResult<()> {
        self.io
            .clipboard(ClipboardOp::Set(text.to_owned()))
            .map(|_| ())
    }
}

/// 构造实例的平台实现三件套（并启动监管线程）。
#[must_use]
pub(super) fn screen_and_input() -> PlatformHandles {
    let io = Shared::start();
    let input = Arc::new(WinInput {
        io: Arc::clone(&io),
    });
    let clipboard = Arc::new(WinClipboard {
        io: Arc::clone(&io),
    });
    // 混合采集：Default 桌面走 WGC/DXGI + GPU 零拷贝，安全桌面/失败时回退
    // `WinScreen` 的 GDI `BitBlt`（见 `remote::win_hybrid`）。
    let screen = super::win_hybrid::screen(Arc::clone(&io));
    PlatformHandles {
        screen,
        input: input.clone(),
        clipboard,
        release: Some(input),
    }
}
