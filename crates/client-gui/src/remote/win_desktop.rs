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
//! - **worker 线程**（`desktop-worker`，**一条线程一个桌面**）：挂到目标桌面后，采集、
//!   注入、剪贴板**全部在它上面**跑——剪贴板同样是窗口站/桌面绑定的，绝不能留在调用线程上。
//!
//! 桌面变化是**低频事件**（锁屏/解锁/切换用户），故「销毁+重建」的开销可以忽略；真正要
//! 守住的是「切换期间绝不丢抬起事件」（见下）。
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
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use rust_tunnel_remote::{
    Clipboard, DisplayInfo, InputEvent, InputSink, KeyEvent, MouseButton, RawFrame, RemoteError,
    RemoteResult, ScreenSource,
};
use rust_tunnel_win_platform::capture;
use rust_tunnel_win_platform::desktop::{self, InputDesktop};
use rust_tunnel_win_platform::inject::{self, InjectButton, InjectStep};
use rust_tunnel_win_platform::monitor::{self, Monitor, VirtualScreen};
use rust_tunnel_win_platform::WinError;

use super::desktop_follow::{self, FollowAction, FollowDesktop};
use super::input_queue::{HeldInputs, InputQueue, PushOutcome};
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
    /// 请求当前 worker 停止（监管线程换桌面、或进程收尾用）。
    stop: bool,
    /// 请求当前 worker 补发一次「全部抬起」（会话结束时由实例运行时触发）。
    release_requested: bool,
}

/// 共享状态 + 唤醒用的条件变量。
struct Shared {
    /// 受锁保护的状态。
    inner: Mutex<IoInner>,
    /// 有新活/停止信号时唤醒 worker。
    cv: Condvar,
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
                stop: false,
                release_requested: false,
            }),
            cv: Condvar::new(),
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
        self.lock().captures.push_back(CaptureRequest { display, reply });
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
        self.lock().clipboard.push_back(ClipboardRequest { op, reply });
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

/// 探测**当前输入桌面**并分类。
///
/// 走 `InputDesktop::open()` → 读名字 → **立即丢弃**：`Drop` 会在未挂载时关闭句柄，
/// 故这次探测不泄漏句柄（这正是「不要每次操作前 `OpenInputDesktop`」那条约束的落点——
/// 每事件一次会泄漏，每 300ms 一次不会）。
fn probe_input_desktop() -> Option<FollowDesktop> {
    match InputDesktop::open() {
        Ok(desktop) => Some(desktop_follow::follow_of_name(desktop.name())),
        Err(err) => {
            tracing::debug!(%err, "探测输入桌面失败（非 SYSTEM 进程锁屏时会这样）");
            None
        }
    }
}

/// 监管线程主体：按 ≤4Hz 探测输入桌面，桌面变化（或 worker 意外退出）就重建 worker。
///
/// 它**自己从不挂桌面**（不调 `SetThreadDesktop`），所以可以在整个进程生命周期内长驻，
/// 不必随桌面变化重建。
fn supervisor_loop(shared: &Arc<Shared>) {
    let mut worker: Option<JoinHandle<()>> = None;
    let mut current: Option<FollowDesktop> = None;
    loop {
        let action = desktop_follow::decide(current, probe_input_desktop());
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
            tracing::info!(
                desktop = target.as_str(),
                worker_died,
                "重建采集/注入线程"
            );
            worker = spawn_worker(Arc::clone(shared), target);
            // 只在**确实起了线程**时才认下这个目标：起失败时保持 `None`，下一轮会重试。
            current = worker.as_ref().map(|_| target);
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
    let mut desktop = InputDesktop::open().map_err(|err| format!("OpenInputDesktop 失败：{err}"))?;
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
    let Some(mut desktop) = attach_with_retry(shared, target) else {
        tracing::info!(desktop = target.as_str(), "worker 未挂载即被要求停止");
        return;
    };
    tracing::info!(desktop = target.as_str(), "采集/注入/剪贴板线程已挂到目标桌面");

    // 显示器几何在这条线程上枚举一次（本线程挂的桌面上的几何）。
    let (monitors, vs) = enumerate_screen();
    publish_displays(shared, &monitors);

    let mut held = HeldInputs::default();
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
            serve_capture(&monitors, req);
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
        Ok(outcome) => tracing::warn!(summary = %outcome.summary(), "部分注入事件未被接受（UIPI / 桌面 ACL？）"),
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
fn serve_capture(monitors: &[Monitor], req: CaptureRequest) {
    // 解构取走 `reply`（`Sender::send` 只借 `&self`，不解构的话 `req` 不会被消耗）。
    let CaptureRequest { display, reply } = req;
    let result = match monitors.iter().find(|m| m.id == display) {
        Some(monitor) => capture::capture_monitor(monitor)
            .map(|frame| RawFrame {
                display,
                width: frame.width,
                height: frame.height,
                rgba: frame.rgba(),
            })
            .map_err(platform_err),
        None => Err(RemoteError::Platform(format!("显示器 {display} 不存在"))),
    };
    // 接收端可能已超时放弃（`recv_timeout`）；发送失败不是错误。
    let _ = reply.send(result);
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

/// `win-platform` 支撑的采集源（实例用）。
struct WinScreen {
    /// 与 worker 共享的状态。
    io: Arc<Shared>,
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
        self.io.clipboard(ClipboardOp::Set(text.to_owned())).map(|_| ())
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
    let screen = Arc::new(WinScreen { io });
    PlatformHandles {
        screen,
        input: input.clone(),
        clipboard,
        release: Some(input),
    }
}
