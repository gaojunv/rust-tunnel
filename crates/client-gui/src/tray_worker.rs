//! 托盘事件的独立消费线程：**窗口隐藏后仍要能响应托盘**。
//!
//! 为什么不能在 `eframe::App::update` 里轮询托盘事件：关窗时我们把原生窗口
//! `set_visible(false)` 隐藏，而 Windows 只给**可见**窗口投递 `WM_PAINT`（winit 源码里
//! 明写了这一点）。eframe 的 `update` 由 `WindowEvent::RedrawRequested` 驱动，
//! `Context::request_repaint_after` 最终也只是 `RedrawWindow(..., RDW_INTERNALPAINT)`；
//! 窗口一隐藏就再也拿不到帧。于是菜单事件虽然进了 muda 的队列，却没有任何人来抽干它——
//! 表现就是「点托盘没反应、菜单里的『退出』也没反应」。
//!
//! 因此托盘事件由本模块的线程持有：
//! - 窗口级操作（显示/聚焦/退出）直接走 Win32，不依赖 egui 是否在跑帧；
//! - 需要 UI 状态的操作（切 Tab、重连、被控开关）经 [`TrayBridge`] 交给下一帧。
//!
//! 窗口可见时这条路径同样有效（只是「下一帧」来得很快），所以不存在两套逻辑。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::app::AppState;
use crate::prefs;
use crate::tray::{self, TrayAction};

/// 托盘事件轮询间隔。50 ms 足够让点击「跟手」，又几乎不占 CPU（每轮只是两次
/// `try_recv` + 一次远程状态快照）。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// 退出请求的兜底时限：用户点了「退出」之后，进程**必须**结束。
const QUIT_WATCHDOG: Duration = Duration::from_secs(5);

/// `pending_tab` 的哨兵值（`usize::MAX` 不可能是合法 Tab 下标）。
const NO_TAB: usize = usize::MAX;

/// 托盘线程与 GUI 之间的共享状态。
///
/// 托盘线程**不持有** `GuiApp`（它在 eframe 的闭包里，且 `muda::CheckMenuItem` 是
/// `Rc`、不能跨线程）。所有需要 UI 的结果都写在这里，由 `update` 每帧取走。
pub struct TrayBridge {
    /// 主窗口句柄（0 = 尚未拿到）。仅 Windows 使用：隐藏窗口的还原必须走原生调用。
    #[cfg(target_os = "windows")]
    hwnd: AtomicIsize,
    /// eframe 的 egui 上下文（创建窗口后注入）。跨线程调用是 egui 支持的用法。
    ctx: OnceLock<egui::Context>,
    /// 待应用的 Tab 下标（[`NO_TAB`] = 无）。
    pending_tab: AtomicUsize,
    /// 「允许被远程控制」的镜像。muda 会自己翻转菜单勾选态，托盘线程读不到
    /// `CheckMenuItem`（`Rc`，非 `Send`），故用同一方向的翻转来预测新状态；
    /// GUI 每帧再用真实勾选态把它校正回来。
    allow_remote: AtomicBool,
}

impl TrayBridge {
    /// 构造桥（`allow_remote` 为持久化偏好的初值）。
    #[must_use]
    pub fn new(allow_remote: bool) -> Self {
        Self {
            #[cfg(target_os = "windows")]
            hwnd: AtomicIsize::new(0),
            ctx: OnceLock::new(),
            pending_tab: AtomicUsize::new(NO_TAB),
            allow_remote: AtomicBool::new(allow_remote),
        }
    }

    /// eframe 创建窗口后调用一次：记下 HWND 与 egui 上下文。
    pub fn attach(&self, cc: &eframe::CreationContext<'_>) {
        #[cfg(target_os = "windows")]
        {
            use raw_window_handle::HasWindowHandle as _;
            if let Ok(handle) = cc.window_handle() {
                if let raw_window_handle::RawWindowHandle::Win32(win) = handle.as_raw() {
                    self.hwnd.store(win.hwnd.get(), Ordering::SeqCst);
                }
            }
        }
        // 重复调用无副作用（`OnceLock::set` 第二次返回 Err，忽略即可）。
        let _ = self.ctx.set(cc.egui_ctx.clone());
    }

    /// 登记「下一帧切到某个 Tab」。只保留最后一次请求，避免连续点击攒一堆过期的切换。
    fn set_tab(&self, tab: usize) {
        self.pending_tab.store(tab, Ordering::SeqCst);
    }

    /// 取走待应用的 Tab（`update` 每帧调用）。
    #[must_use]
    pub fn take_pending_tab(&self) -> Option<usize> {
        match self.pending_tab.swap(NO_TAB, Ordering::SeqCst) {
            NO_TAB => None,
            tab => Some(tab),
        }
    }

    /// 「允许被远程控制」的镜像值。
    #[must_use]
    pub fn allow_remote(&self) -> bool {
        self.allow_remote.load(Ordering::SeqCst)
    }

    /// 更新镜像（UI 改动与托盘改动两个方向都走这里）。
    pub fn set_allow_remote(&self, on: bool) {
        self.allow_remote.store(on, Ordering::SeqCst);
    }

    /// 显示并聚焦主窗口。
    ///
    /// Windows 上走 `ShowWindow`/`SetForegroundWindow`：窗口是通过
    /// `ViewportCommand::Visible(false)` 隐藏的，egui 的 `Visible(true)` 命令要等下一帧
    /// 才被处理，而隐藏窗口恰恰没有下一帧——必须由这里把窗口先「叫醒」。
    pub fn show(&self) {
        #[cfg(target_os = "windows")]
        {
            let hwnd = self.hwnd.load(Ordering::SeqCst);
            if hwnd != 0 {
                native::show(hwnd);
            }
        }
        if let Some(ctx) = self.ctx.get() {
            // 这两条命令在 Windows 上**不是**冗余：原生 `ShowWindow` 绕过了 winit 的
            // `WindowFlags::VISIBLE` 缓存，若不同步，下一次 `Visible(false)`（点 ×）
            // 会因缓存仍是「已隐藏」而变成空操作，窗口就关不掉了。
            // 首个可用帧会应用它们并更新缓存；窗口本身已由原生调用显示出来。
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    }
}

/// 启动托盘事件线程。
///
/// 线程随进程存活；「退出」由本线程自己触发，故不需要停止信号。
pub fn spawn(bridge: Arc<TrayBridge>, app_state: Arc<AppState>) {
    let spawned = std::thread::Builder::new()
        .name("tray-events".to_owned())
        .spawn(move || run(&bridge, &app_state));
    if let Err(e) = spawned {
        // `update` 里已经不再轮询托盘事件，线程起不来就没人消费它们了——把原因写进
        // 日志，别让现场只看到「托盘失灵」。
        tracing::error!("托盘事件线程启动失败（托盘操作将无响应）：{e}");
    }
}

fn run(bridge: &TrayBridge, app_state: &AppState) {
    // 只在「待确认请求首次出现」时抢焦点：用户拒绝/批准前可能反复出现，
    // 每 50 ms 把窗口弹一次会让人没法操作。
    let mut last_pending: Option<String> = None;
    loop {
        for action in tray::poll_tray_actions() {
            handle(bridge, app_state, action);
        }
        poll_pending_approval(bridge, app_state, &mut last_pending);
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 隐藏状态下 `update` 不跑：待确认请求没人渲染，超时后会静默拒绝。这里在它
/// **首次出现**时把窗口叫回来，让 `update` 恢复正常并弹出确认框（安全提示不能
/// 依赖用户先去点托盘）。
fn poll_pending_approval(
    bridge: &TrayBridge,
    app_state: &AppState,
    last_pending: &mut Option<String>,
) {
    let pending = app_state
        .remote()
        .and_then(|m| m.snapshot().pending_approval)
        .map(|p| p.session_id);
    if pending.is_some() && *last_pending != pending {
        tracing::info!("有远程接入请求待确认，显示主窗口");
        bridge.set_tab(2);
        bridge.show();
    }
    *last_pending = pending;
}

fn handle(bridge: &TrayBridge, app_state: &AppState, action: TrayAction) {
    match action {
        TrayAction::Restore => bridge.show(),
        TrayAction::Show => {
            bridge.set_tab(0);
            bridge.show();
        }
        // 「设置」与「服务设置」同落地（设置就是那个 Tab），下标见 `show_tabs`：
        // 状态/映射/远程/设置/日志。
        TrayAction::Settings | TrayAction::Service => {
            bridge.set_tab(3);
            bridge.show();
        }
        TrayAction::Reconnect => {
            tracing::info!("托盘请求重连");
            app_state.reconnect_requested.store(true, Ordering::SeqCst);
        }
        TrayAction::ToggleAllowRemote => toggle_allow_remote(bridge, app_state),
        TrayAction::Quit => quit(bridge, app_state),
    }
}

/// 托盘里的「允许被远程控制」：muda 已翻转勾选态，这里按同一方向翻转镜像并立即生效。
///
/// 不能等 `update`：窗口隐藏时没有帧，等下去等于点了没反应。落盘也在这里做，
/// 否则下次启动会回到旧值。
fn toggle_allow_remote(bridge: &TrayBridge, app_state: &AppState) {
    let on = !bridge.allow_remote();
    bridge.set_allow_remote(on);
    if let Some(manager) = app_state.remote() {
        manager.set_enabled(on);
    } else {
        // 引擎还没构造好（启动极早期）：偏好已落盘，下次启动生效。
        tracing::warn!("远程引擎尚未就绪，被控开关仅写入偏好");
    }
    if let Err(e) = prefs::update_allow_remote(on) {
        tracing::warn!("被控开关保存失败：{e}");
    }
    tracing::info!("允许被远程控制：{}", if on { "开" } else { "关" });
}

/// 退出应用。
///
/// 隐藏窗口必须先显示：`intercept_close` 只在 `update` 里运行，而隐藏窗口没有帧；
/// 显示后下一帧看到 `close_requested`，因 `should_exit` 已置位而放行关闭。
///
/// 另配一个兜底线程：用户点了「退出」，进程就一定要结束——哪怕 GUI 侧因为任何原因
/// （渲染线程卡死、多视口未收敛等）没能走完关闭流程。
fn quit(bridge: &TrayBridge, app_state: &AppState) {
    tracing::info!("托盘请求退出");
    app_state.should_exit.store(true, Ordering::SeqCst);
    // 兜底线程先起：`ShowWindow` 对另一线程的窗口是**同步**调用，如果 GUI 线程已经卡死，
    // 它会把本线程一起拖住——那时就没人能执行下面的进程退出了。
    let spawned = std::thread::Builder::new()
        .name("quit-watchdog".to_owned())
        .spawn(|| {
            std::thread::sleep(QUIT_WATCHDOG);
            tracing::warn!("退出请求 {QUIT_WATCHDOG:?} 内未被处理，强制结束进程");
            std::process::exit(0);
        });
    if let Err(e) = spawned {
        tracing::warn!("退出兜底线程启动失败：{e}");
    }
    // 隐藏窗口没有帧，`intercept_close` 不会运行；先显示，下一帧才会看到
    // `close_requested` 并因 `should_exit` 放行关闭。
    bridge.show();
    if let Some(ctx) = bridge.ctx.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        ctx.request_repaint();
    }
}

/// Windows 原生窗口操作。
#[cfg(target_os = "windows")]
mod native {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };

    /// 显示（必要时还原）并尽量把窗口带到前台。
    pub fn show(hwnd: isize) {
        let hwnd = hwnd as HWND;
        if hwnd.is_null() {
            return;
        }
        // SAFETY: `hwnd` 来自 eframe 的 `RawWindowHandle`，是本进程尚未销毁的窗口。
        // 这些调用都只作用于该窗口；最坏情况是前台锁让 `SetForegroundWindow` 失败。
        unsafe {
            // 先判最小化：窗口可能「隐藏 + 最小化」（最小化状态下点任务栏关闭），
            // 此时 `SW_SHOW` 只会让它以最小化形态回到任务栏，用户还得再点一次。
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            } else if IsWindowVisible(hwnd) == 0 {
                // 隐藏状态：直接显示，保留它原来的最大化/普通状态（`SW_RESTORE`
                // 会把最大化的窗口缩回去，退出时会看到窗口尺寸跳一下）。
                ShowWindow(hwnd, SW_SHOW);
            }
            if SetForegroundWindow(hwnd) != 0 {
                return;
            }
            // 前台锁拒绝时：临时附加到当前前台线程的输入队列，再抢一次。
            // 这是 Win32 文档给出的标准做法；失败也不致命（窗口已可见）。
            let foreground = GetForegroundWindow();
            if !foreground.is_null() {
                let foreground_thread = GetWindowThreadProcessId(foreground, std::ptr::null_mut());
                let self_thread = GetCurrentThreadId();
                if foreground_thread != 0 && foreground_thread != self_thread {
                    AttachThreadInput(self_thread, foreground_thread, 1);
                    SetForegroundWindow(hwnd);
                    AttachThreadInput(self_thread, foreground_thread, 0);
                }
            }
            BringWindowToTop(hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    //! 桥的纯逻辑单测（不建窗口、不碰托盘，故任意平台可跑）。

    use super::*;

    #[test]
    fn pending_tab_is_consumed_exactly_once() {
        let bridge = TrayBridge::new(false);
        assert_eq!(bridge.take_pending_tab(), None, "初始应为空");

        bridge.set_tab(3);
        assert_eq!(bridge.take_pending_tab(), Some(3));
        assert_eq!(bridge.take_pending_tab(), None, "取走后必须清空");

        // 只保留最后一次请求（连续点击不应把过期的切换排队）。
        bridge.set_tab(1);
        bridge.set_tab(4);
        assert_eq!(bridge.take_pending_tab(), Some(4));
    }

    #[test]
    fn allow_remote_mirror_roundtrips() {
        let bridge = TrayBridge::new(false);
        assert!(!bridge.allow_remote());
        bridge.set_allow_remote(true);
        assert!(bridge.allow_remote());
        assert!(TrayBridge::new(true).allow_remote());
    }
}
