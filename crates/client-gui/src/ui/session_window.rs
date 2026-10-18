//! 独立的远程会话窗口（egui deferred viewport）。
//!
//! # 为什么是独立窗口
//!
//! 把画面塞进 480×520 主窗口的「远程」Tab，等于给画面判了死刑：可用区域被 Tab 栏、横幅、工具条
//! 层层压缩，无法放大、无法全屏，切到别的 Tab 还会丢上下文。deferred viewport 是 egui 给原生多窗口
//! 的正规通道——子窗口有自己的 OS 窗口、自己的事件循环，画面能用整块屏幕。
//!
//! # 与父窗口的分工（决定本模块的形态）
//!
//! - **会话生命周期**（开窗 / 关窗 / 断线 / 重连）在父窗口 `GuiApp::update` → [`RemoteView::reconcile`]
//!   里收敛，因为它必须**每帧**都被观测，而子窗口的回调只在窗口存在时才跑。
//! - 本模块每帧从 [`RemoteView`] 读结果（`window_open` / `inner_size` / 档位），据此决定是否调用
//!   `show_viewport_deferred`（不调用 = 关掉子窗口）并渲染内容。
//! - 回调可能在**别的线程**、或父窗口休眠时执行，故所有共享状态走 `Arc<Mutex<RemoteView>>`。
//!   **注册 `show_viewport_deferred` 前必须先放开这把锁**——`embed_viewports()` 为真时回调会当场
//!   同步执行（`ViewportClass::Embedded`），持锁注册会自死锁。
//!
//! [`RemoteView::reconcile`]: crate::remote::RemoteView::reconcile

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Ui};
use rust_tunnel_common::protocol::RemoteMode;
use rust_tunnel_remote::{
    ActiveSessionInfo, QualityPreset, RemoteManager, RemoteUiState, SessionRole, TransportPath,
};

use crate::remote::{
    display_size, RemoteView, ViewScale, WindowSize, MIN_INNER, TOOLBAR_FALLBACK_HEIGHT,
};
use crate::ui::remote::state_label;

/// 会话窗口的 viewport ID（父窗口定位子窗口、子窗口发给自身都用它）。
/// 用常量字符串哈希：同一进程里唯一即可，不需要再维护一个全局变量。
#[must_use]
pub fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("remote-session")
}

/// 工具条静止多久后自动隐藏。
const TOOLBAR_IDLE: Duration = Duration::from_secs(3);

/// 指针进入窗口上缘这条带内即视为「想看工具条」（工具条隐藏时的手动召回）。
const TOOLBAR_HOT: f32 = 6.0;

/// 取会话窗口的锁（中毒时取回内部值：一个视图锁中毒不该拖垮整个 UI）。
fn lock(view: &Arc<Mutex<RemoteView>>) -> MutexGuard<'_, RemoteView> {
    view.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 窗口图标（与主窗口同源）；只解码一次。
fn window_icon() -> Option<Arc<egui::IconData>> {
    static ICON: OnceLock<Option<Arc<egui::IconData>>> = OnceLock::new();
    ICON.get_or_init(crate::load_window_icon).clone()
}

/// 渲染/维护远程会话窗口。每帧由 `GuiApp::update` 调用一次。
///
/// 只在「本端是控制器 + 有活跃会话 + `window_open`」时注册 viewport；否则直接返回（等于关闭子窗口）。
pub fn show(ctx: &egui::Context, view: &Arc<Mutex<RemoteView>>, manager: &Arc<RemoteManager>) {
    let state = manager.snapshot();
    let Some(active) = state
        .active
        .as_ref()
        .filter(|active| active.role == SessionRole::Controller)
    else {
        return; // 非控制器会话（或无会话）：不注册 viewport
    };
    let title = format!("远程控制 — {} · {}", active.peer_name, state_label(active.state));

    // 锁内取值算好 builder 参数，**锁外**再注册（见模块文档：持锁注册会自死锁）。工具条自动隐藏
    // 开关也在这里读——它由工具条上的开关改动（写在回调里），必须在锁内取到最新镜像。
    let (inner, autohide) = {
        let mut v = lock(view);
        if !v.window_open {
            return; // 用户最小化 / 关窗
        }
        // 初始尺寸只在远端首帧到达前反复重算，之后锁死——否则会每帧和用户拖拽抢窗口。
        if !v.size_initialized {
            let monitor = ctx.input(|i| i.viewport().monitor_size);
            v.inner_size = v.desired_inner(monitor);
            if v.frame_dims().0 > 0 {
                v.size_initialized = true;
            }
        }
        (v.inner_size, v.toolbar_autohide)
    };

    let mut builder = egui::ViewportBuilder::default()
        .with_title(title)
        .with_decorations(true)
        .with_resizable(true)
        .with_inner_size(inner)
        .with_min_inner_size(MIN_INNER);
    if let Some(icon) = window_icon() {
        builder = builder.with_icon(icon);
    }

    let view_cb = Arc::clone(view);
    let manager_cb = Arc::clone(manager);
    ctx.show_viewport_deferred(viewport_id(), builder, move |ctx, class| {
        session_viewport_ui(ctx, class, &view_cb, &manager_cb, autohide);
    });
}

/// 子窗口每个 pass 的入口。
fn session_viewport_ui(
    ctx: &egui::Context,
    class: egui::ViewportClass,
    view: &Arc<Mutex<RemoteView>>,
    manager: &Arc<RemoteManager>,
    autohide: bool,
) {
    let state = manager.snapshot();
    let Some(active) = state
        .active
        .as_ref()
        .filter(|active| active.role == SessionRole::Controller)
    else {
        return; // 会话已结束（父窗口会停止注册）；这里只是兜底
    };

    let mut v = lock(view);

    // 关窗 = 结束会话：置 `window_open = false` 让父窗口停止注册，`abort_active` 让本端成为「主动
    // 结束」。后者**不会**触发断线自动重连——本端自己收尾的路径根本不写 `session_end`
    // （见 `crates/remote/src/manager.rs` 里该字段的契约），而重连判据只认它。
    let vp = ctx.input(|i| i.viewport().clone());
    if vp.close_requested() {
        v.release_input(manager);
        v.window_open = false;
        manager.abort_active();
        return;
    }

    // 全屏状态以 viewport 为准（用户可能用系统快捷键切的）。
    v.fullscreen = vp.fullscreen.unwrap_or(false);
    handle_shortcuts(ctx, &mut v);
    update_toolbar_visibility(ctx, &mut v, autohide);

    // 本帧捕获判据：先清零，`capture_input` 真正运行时再置 true（见 `RemoteView::capture_this_frame`）。
    v.capture_this_frame = false;
    let embedded = matches!(class, egui::ViewportClass::Embedded);
    render(&mut v, ctx, manager, active, &state, embedded);

    // 本帧没捕获到（模式非完全控制 / 画面还没到 / 独占组合键刚被拦下），或窗口失焦/最小化 →
    // 补发一次输入抬起，避免远端残留按下的键。`release_all` 幂等，多调用无副作用。
    let view_unfocused = vp.minimized.unwrap_or(false) || vp.focused == Some(false);
    if !v.capture_this_frame || view_unfocused {
        v.release_input(manager);
    }

    // 工具条可见时给一个定时重绘，否则静止画面下它永远不会淡出（画面静止时引擎不唤醒重绘）。
    if v.toolbar_visible {
        ctx.request_repaint_after(Duration::from_millis(400));
    }
}

/// 本端快捷键：`F11` 切全屏；全屏时 `Esc` 退出全屏（非全屏的 `Esc` 照常透传给远端，见
/// [`RemoteView::capture_input`] 对事件流的过滤）。
fn handle_shortcuts(ctx: &egui::Context, v: &mut RemoteView) {
    if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
        v.fullscreen = !v.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(v.fullscreen));
    } else if v.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        // 刻意**不**在这里把 `v.fullscreen` 改成 false：本帧还要靠它让 `capture_input` 吞掉这次
        // Esc（否则它会透传给远端）。下一帧 `vp.fullscreen` 变 false 时自然同步。
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
    }
}

/// 工具条自动隐藏：指针移动、位于工具条**实测矩形**内、或指针按下时显示并重置计时；静止超时后隐藏。
///
/// 热区判据分两态：**可见时**用工具条面板上一帧的实测矩形（[`RemoteView::toolbar_rect`]，
/// 覆盖 `horizontal_wrapped` 折行后的第二、三行——否则指针停在第二行按钮上时判据为 false，
/// 工具条会从指针底下消失）；**隐藏时**只认上缘窄带 [`TOOLBAR_HOT`]（移到窗口上缘召回）。
///
/// # 为什么用上一帧的矩形
///
/// egui 是立即模式：本帧渲染工具条时还不知道本帧的矩形（工具条自己就是被渲染对象）。用上一帧
/// 实测的矩形是标准做法，一帧延迟对「指针是否停在按钮上」无感。首帧尚未测得时用
/// [`toolbar_fallback_rect`] 兜底。
fn update_toolbar_visibility(ctx: &egui::Context, v: &mut RemoteView, autohide: bool) {
    let now = Instant::now();
    let (moved, down, pointer) = ctx.input(|i| {
        (
            i.pointer.delta() != egui::Vec2::ZERO,
            i.pointer.any_down(),
            i.pointer.hover_pos(),
        )
    });
    let decision = decide_toolbar_visibility(ToolbarInputs {
        autohide,
        was_visible: v.toolbar_visible,
        pointer,
        toolbar_rect: v.toolbar_rect.unwrap_or_else(toolbar_fallback_rect),
        moved,
        down,
        idle: now.saturating_duration_since(v.last_pointer_move),
    });
    if decision.reset_activity {
        v.last_pointer_move = now;
    }
    v.toolbar_visible = decision.visible;
}

/// 工具条可见性决策的输入（摊平成纯数据，便于表格驱动单测）。
#[derive(Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "输入就是若干个独立的布尔条件（开关 / 上一帧状态 / 指针动作），拆枚举只会更难读"
)]
struct ToolbarInputs {
    /// 是否开启自动隐藏。
    autohide: bool,
    /// 上一帧工具条是否可见。
    was_visible: bool,
    /// 指针当前位置（不在窗口内时为 `None`）。
    pointer: Option<egui::Pos2>,
    /// 工具条**上一帧**的实测矩形（见 [`update_toolbar_visibility`] 对一帧延迟的说明）。
    toolbar_rect: egui::Rect,
    /// 本帧指针是否移动。
    moved: bool,
    /// 本帧是否有指针按下。
    down: bool,
    /// 距上次指针活动的空闲时长。
    idle: Duration,
}

/// 决策结果。
struct ToolbarDecision {
    /// 本帧工具条是否可见。
    visible: bool,
    /// 是否应把「最近活动时刻」重置为现在。
    reset_activity: bool,
}

/// 工具条自动隐藏的**纯决策**：给定上一帧状态与指针，算出本帧是否可见、是否应重置活动计时。
///
/// 把输入摊平成 [`ToolbarInputs`] 后，三态（常驻 / 指针在工具条上 / 空闲超时）都能用表格断言
/// 钉死（见本模块单测）——这正是缺陷「工具条莫名消失」的机械锁。
fn decide_toolbar_visibility(i: ToolbarInputs) -> ToolbarDecision {
    if !i.autohide {
        // 关闭自动隐藏：恒可见，且无需重置计时。
        return ToolbarDecision {
            visible: true,
            reset_activity: false,
        };
    }
    let hot = i.pointer.is_some_and(|p| {
        if i.was_visible {
            // 可见时：整条**实测**矩形都算热区（含折行后的第二、三行）。
            i.toolbar_rect.contains(p)
        } else {
            // 隐藏时：只认上缘窄带。
            p.y <= TOOLBAR_HOT
        }
    });
    if i.moved || hot || i.down {
        ToolbarDecision {
            visible: true,
            reset_activity: true,
        }
    } else if i.idle >= TOOLBAR_IDLE {
        ToolbarDecision {
            visible: false,
            reset_activity: false,
        }
    } else {
        // 无活动但尚未超时：保持当前状态。
        ToolbarDecision {
            visible: i.was_visible,
            reset_activity: false,
        }
    }
}

/// 首帧尚未实测到工具条矩形时的兜底热区：窗口上缘一条 [`TOOLBAR_FALLBACK_HEIGHT`] 高的满宽带。
fn toolbar_fallback_rect() -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(1.0e6, TOOLBAR_FALLBACK_HEIGHT),
    )
}

/// 工具条面板**隐藏**时要用缓存高度占位（返回 `Some(h)`）；可见时返回 `None`（高度由内容实测）。
///
/// # 为什么隐藏时也必须占位（缺陷「隐现挪动/缩放画面」的机械锁）
///
/// `TopBottomPanel` 注册时会从中央区域扣除自身高度。过去隐藏时整条面板不注册（占位 = 0），
/// 中央区域因此变高，`scene()` 用新的 `available_size()` 重算画面——`Fit` 档画面**尺寸改变**、
/// 固定档画面**原点上移**，隐现瞬间的点击还会落到远端错误坐标。本函数保证隐藏分支**永不**返回
/// `Some(0.0)`：占位高度恒为缓存高度，于是两种可见性下中央区域完全一致。
fn toolbar_reserve(cached_height: f32, visible: bool) -> Option<f32> {
    (!visible).then_some(cached_height)
}

/// 渲染窗口主体。`embedded` 分支（集成不支持多窗口时的兜底）退化为一个 egui 窗口承载同样内容。
fn render(
    v: &mut RemoteView,
    ctx: &egui::Context,
    manager: &Arc<RemoteManager>,
    active: &ActiveSessionInfo,
    state: &RemoteUiState,
    embedded: bool,
) {
    if embedded {
        egui::Window::new("远程控制")
            .id(egui::Id::new("session_window_embedded"))
            .default_size([900.0, 620.0])
            .show(ctx, |ui| {
                if v.toolbar_visible {
                    toolbar(ui, v, manager, state);
                } else {
                    // 与 deferred 路径同理：隐藏也占位，画面不因工具条隐现而上下位移。
                    ui.add_space(v.toolbar_height);
                }
                reconnect_strip(ui, v, manager);
                if v.side_panel_open {
                    side_contents(ui, v, manager, state);
                }
                scene(ui, v, manager, active);
            });
        return;
    }

    // 工具条**常驻注册**（缺陷「隐现挪动/缩放画面」的修复）：无论可见与否都注册这个面板。
    // `TopBottomPanel` 注册时会从中央区域扣除自身高度；过去隐藏时整条 `.show()` 都不执行，
    // 中央区域于是变高、上边缘上移，`scene()` 用新的 `available_size()` 重算画面（`Fit` 档尺寸变、
    // 固定档原点上移），隐现瞬间的点击还会落到远端错误坐标。常驻后隐藏时面板内不画内容、只用
    // 缓存高度占位，`CentralPanel` 的可用矩形恒定。
    let reserve = toolbar_reserve(v.toolbar_height, v.toolbar_visible);
    let mut content_height: Option<f32> = None;
    let panel = egui::TopBottomPanel::top("session_toolbar").show(ctx, |ui| {
        if let Some(height) = reserve {
            // 隐藏：不画内容，但占住缓存高度（`set_min_height` 作用在**内容**上，故缓存的是内容高度）。
            ui.set_min_height(height);
        } else {
            toolbar(ui, v, manager, state);
            // 记下**内容**高度（不含面板 frame 内边距）：隐藏时以同一个值 `set_min_height`，
            // 面板总高才与可见时严格一致。
            content_height = Some(ui.min_rect().height());
        }
    });
    // 实测矩形供**下一帧**的热区判据用（egui 立即模式，见 `update_toolbar_visibility`）。
    v.toolbar_rect = Some(panel.response.rect);
    // 只在测得正值时覆盖缓存高度：退化的 0 会让隐藏时占位也为 0，把缺陷放回来。
    if let Some(height) = content_height.filter(|h| *h > 0.0) {
        v.toolbar_height = height;
    }
    if v.reconnect_note.is_some() {
        egui::TopBottomPanel::top("session_reconnect").show(ctx, |ui| {
            reconnect_strip(ui, v, manager);
        });
    }
    if v.side_panel_open {
        egui::SidePanel::right("session_side")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                side_contents(ui, v, manager, state);
            });
    }
    egui::CentralPanel::default().show(ctx, |ui| {
        scene(ui, v, manager, active);
    });
    if v.stats_overlay {
        stats_overlay(ctx, v, active, state);
    }
}

/// 顶部工具条：连接质量常显 + 显示器 / 画质 / 窗口尺寸 / 缩放 / 全屏 / 独占键盘 / 剪贴板同步 /
/// 测延迟 / 统计角标 / 侧栏 / 最小化窗口 / 中断会话。
fn toolbar(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>, state: &RemoteUiState) {
    ui.horizontal_wrapped(|ui| {
        quality_indicator(ui, state);
        if let Some(active) = state.active.as_ref() {
            ui.label(RichText::new(format!("· {}", state_label(active.state))).weak().small());
        }
        ui.separator();

        display_selector(ui, v, manager);
        quality_selector(ui, manager, state.quality);
        window_size_selector(ui, v);
        scale_selector(ui, v);
        ui.separator();

        if ui
            .selectable_label(v.fullscreen, "全屏")
            .on_hover_text("切换全屏（F11；全屏时 Esc 退出）")
            .clicked()
        {
            v.fullscreen = !v.fullscreen;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(v.fullscreen));
        }
        if ui
            .selectable_label(v.exclusive_keyboard, "独占键盘")
            .on_hover_text("键盘全部发给远端；Ctrl/Cmd+Alt+Shift+K 退出")
            .clicked()
        {
            v.exclusive_keyboard = !v.exclusive_keyboard;
        }
        if ui
            .selectable_label(v.clipboard_sync_enabled, "剪贴板同步")
            .on_hover_text("自动在本机与远端之间同步剪贴板文本")
            .clicked()
        {
            v.clipboard_sync_enabled = !v.clipboard_sync_enabled;
            if !v.clipboard_sync_enabled {
                v.clipboard_sync = None;
            }
        }
        if ui.button("测延迟").on_hover_text("立刻探测一次往返延迟").clicked() {
            manager.ping();
        }
        if ui.selectable_label(v.stats_overlay, "统计角标").clicked() {
            v.stats_overlay = !v.stats_overlay;
        }
        if ui.selectable_label(v.side_panel_open, "侧栏").clicked() {
            v.side_panel_open = !v.side_panel_open;
        }
        // 自动隐藏开关：这是**唯一**能让用户关掉「工具条偶尔消失」的入口（偏好默认值改了也只对
        // 全新配置生效——老用户的 gui.toml 早已把旧默认钉死）。改动写进运行时镜像，由父窗口的
        // `sync_display_prefs` 回写 gui.toml。
        if ui
            .selectable_label(v.toolbar_autohide, "自动隐藏")
            .on_hover_text("指针静止 3 秒后隐藏工具条（移到窗口上缘召回）；关闭后工具条常驻")
            .clicked()
        {
            v.toolbar_autohide = !v.toolbar_autohide;
        }
        ui.separator();
        if ui
            .button("最小化窗口")
            .on_hover_text("关闭窗口但保留会话；在「远程」Tab 可召回")
            .clicked()
        {
            // 置 false 后父窗口就不再注册这个 viewport，回调从此不再跑——必须**当场**补发一次
            // 输入抬起，否则用户此刻按住的键会在远端一直按着（会话仍在，没人替它收尾）。
            v.release_input(manager);
            v.window_open = false;
        }
        if ui.button("中断会话").clicked() {
            manager.abort_active();
        }
    });
}

/// 连接质量：延迟（按阈值上色）与数据面路径，工具条常显。
fn quality_indicator(ui: &mut Ui, state: &RemoteUiState) {
    match state.latency_ms {
        Some(ms) => {
            ui.colored_label(latency_color(ms), RichText::new(format!("{ms} ms")).strong());
        }
        None => {
            ui.label(RichText::new("延迟 —").weak());
        }
    }
    if let Some(path) = state.transport {
        ui.label(RichText::new(transport_label(path)).small());
    }
}

/// 重连提示条：文案 + 「立即重试 / 停止」。
fn reconnect_strip(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>) {
    let Some(note) = v.reconnect_note.clone() else {
        return;
    };
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgba_unmultiplied(220, 160, 40, 30))
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().warn_fg_color, note);
                if v.reconnect_active {
                    if ui.button("立即重试").clicked() {
                        if let Some(target) = v.reconnect.retry_now(Instant::now()) {
                            manager.request_connect(&target.client_name, target.mode);
                        }
                    }
                    if ui.button("停止").clicked() {
                        v.reconnect.cancel();
                        v.reconnect_note = None;
                        v.reconnect_active = false;
                    }
                }
            });
        });
}

/// 显示器选择：多显示器时逐台可点，单显示器时把当前显示器同步给引擎。
fn display_selector(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>) {
    let displays = v.displays();
    if displays.len() > 1 {
        let current = v.selected_display().or_else(|| displays.first().map(|d| d.id));
        ui.label(RichText::new("显示器").weak().small());
        for display in &displays {
            if ui
                .selectable_label(current == Some(display.id), display.name.as_str())
                .clicked()
            {
                v.set_selected_display(display.id, manager);
            }
        }
    } else if let Some(display) = displays.first() {
        v.set_selected_display(display.id, manager);
    }
}

/// 画质档位选择（流畅/均衡/高清）；切换经会话命令下发，由被控端生效。
fn quality_selector(ui: &mut Ui, manager: &Arc<RemoteManager>, current: QualityPreset) {
    ui.label(RichText::new("画质").weak().small());
    for preset in QualityPreset::all() {
        if ui.selectable_label(current == preset, preset.label()).clicked() {
            manager.set_quality(preset);
        }
    }
}

/// 窗口尺寸档位（自适应 / 固定）；切换即重算 `inner_size`，`patch` 会据此下发 `InnerSize`。
fn window_size_selector(ui: &mut Ui, v: &mut RemoteView) {
    ui.label(RichText::new("窗口").weak().small());
    for size in WindowSize::ALL {
        if ui.selectable_label(v.window_size == size, size.label()).clicked() {
            v.window_size = size;
            let monitor = ui.ctx().input(|i| i.viewport().monitor_size);
            v.inner_size = v.desired_inner(monitor);
            v.size_initialized = true;
        }
    }
}

/// 画面缩放档位（适应窗口 / 固定百分比）。
fn scale_selector(ui: &mut Ui, v: &mut RemoteView) {
    ui.label(RichText::new("缩放").weak().small());
    for scale in ViewScale::ALL {
        if ui.selectable_label(v.scale == scale, scale.label()).clicked() {
            v.scale = scale;
        }
    }
}

/// 画面区域：按缩放档位决定绘制尺寸。`Fit` 等比铺满可用区域；固定档位用滚动区承载溢出。
fn scene(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>, active: &ActiveSessionInfo) {
    let Some((texture, width, height)) = v.upload(ui.ctx()) else {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("等待画面…").weak());
            ui.spinner();
        });
        return;
    };
    let avail = ui.available_size();
    let want = display_size(v.scale, avail, (width, height));
    if v.scale == ViewScale::Fit {
        paint_image(ui, v, manager, active, (texture, width, height), want);
    } else {
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            paint_image(ui, v, manager, active, (texture, width, height), want);
        });
    }
}

/// 画一张画面并（完全控制模式时）在其上捕获输入。`image` 为 `(纹理, 远端像素宽, 高)`。
fn paint_image(
    ui: &mut Ui,
    v: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    active: &ActiveSessionInfo,
    image: (egui::TextureId, u32, u32),
    size: egui::Vec2,
) {
    let (texture, _width, _height) = image;
    // 仍然要占位分配这个区域（布局与可交互性不能变），但输入捕获不再依赖 `response`
    // （见 `RemoteView::capture_input`：`hovered()` 会被 egui 的交互簿记影响）。
    let (rect, _response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    ui.painter().image(
        texture,
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    // 只有「完全控制」模式捕获输入：仅观看 / 仅文件传输不该注入键鼠。
    if active.mode == RemoteMode::Control {
        // 坐标按所选显示器的尺寸映射（由 `capture_input` 内部取），与纹理是否缩放无关。
        v.capture_input(ui, manager, rect);
    }
}

/// 统计角标（右上角悬浮）：延迟 / 路径 / 分辨率 / 帧率 / 会话时长。
fn stats_overlay(ctx: &egui::Context, v: &RemoteView, active: &ActiveSessionInfo, state: &RemoteUiState) {
    let (width, height) = v.frame_dims();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(active.started_at_ms, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let secs = now_ms.saturating_sub(active.started_at_ms) / 1000;
    egui::Window::new("会话统计")
        .id(egui::Id::new("session_stats_overlay"))
        .title_bar(false)
        .resizable(false)
        .anchor(egui::Align2::RIGHT_TOP, [-10.0, 10.0])
        .show(ctx, |ui| {
            egui::Grid::new("session_stats_grid")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("延迟").weak().small());
                    match state.latency_ms {
                        Some(ms) => ui.colored_label(latency_color(ms), format!("{ms} ms")),
                        None => ui.label("—"),
                    };
                    ui.end_row();
                    ui.label(RichText::new("路径").weak().small());
                    ui.label(state.transport.map_or("—", transport_label));
                    ui.end_row();
                    ui.label(RichText::new("分辨率").weak().small());
                    ui.label(format!("{width}×{height}"));
                    ui.end_row();
                    ui.label(RichText::new("帧率").weak().small());
                    ui.label(format!("{} fps", v.fps));
                    ui.end_row();
                    // 解码耗时 / 收码率：控制器侧本地实测（阶段 3 QoS 观测量）。
                    ui.label(RichText::new("解码").weak().small());
                    ui.label(state.decode_ms.map_or("—".to_owned(), |ms| format!("{ms} ms")));
                    ui.end_row();
                    ui.label(RichText::new("收码率").weak().small());
                    ui.label(state.recv_kbps.map_or("—".to_owned(), format_kbps));
                    ui.end_row();
                    // 输入速率：本端**捕获并注入**的事件数（不是远端是否响应）。用来一眼分辨
                    // 「UI 压根没产生事件」与「事件已注入但远端没反应」。
                    ui.label(RichText::new("输入").weak().small());
                    ui.label(format!("{}/s", v.input_rate));
                    ui.end_row();
                    ui.label(RichText::new("时长").weak().small());
                    ui.label(format!("{:02}:{:02}", secs / 60, secs % 60));
                    ui.end_row();
                });
        });
}

/// 右侧面板内容：剪贴板 + 文件传输（整体从旧「远程」Tab 迁来，逻辑不变）。
fn side_contents(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>, state: &RemoteUiState) {
    show_clipboard(ui, v, manager);
    show_files(ui, v, manager, state);
}

/// 剪贴板同步：发送本机剪贴板、索取对端剪贴板、展示收到的对端文本。
///
/// 读取本机剪贴板借 [`crate::remote::SystemClipboard`]（arboard）——egui 0.31 只提供**写**系统
/// 剪贴板的 `Context::copy_text`，没有读取 API。这里只**发起**异步读取（结果由父窗口的
/// [`RemoteView::poll_clipboard`] 每帧取回并填入输入框），绝不阻塞 UI 线程。
fn show_clipboard(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>) {
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(RichText::new("剪贴板").strong());
        if ui.button("读取本机").clicked() {
            v.request_local_clipboard();
        }
        if ui.button("发送本机剪贴板").clicked() {
            let text = v.inputs_mut().clipboard_out.clone();
            manager.send_clipboard(text);
        }
        if ui.button("获取对端剪贴板").clicked() {
            manager.request_clipboard();
        }
    });
    ui.add(
        egui::TextEdit::singleline(&mut v.inputs_mut().clipboard_out)
            .desired_width(f32::INFINITY)
            .hint_text("待发送给对端的文本（可点「读取本机」填入）"),
    );
    if let Some(text) = v.inputs_mut().clipboard_in.clone() {
        ui.horizontal(|ui| {
            ui.label(RichText::new("对端剪贴板").weak().small());
            if ui.button("复制到本机").clicked() {
                ui.ctx().copy_text(text.clone());
            }
            if ui.button("清空").clicked() {
                v.inputs_mut().clipboard_in = None;
            }
        });
        // 只读展示：传一份临时副本并关掉交互，用户仍可选中复制。
        let mut shown = text;
        ui.add(
            egui::TextEdit::multiline(&mut shown)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .interactive(false),
        );
    }
}

/// 文件面板：浏览被控端共享目录、下载、上传、查看传输进度。
#[expect(clippy::cast_precision_loss, reason = "传输字节数有界，f32 进度条精度足够")]
fn show_files(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>, state: &RemoteUiState) {
    ui.separator();
    egui::CollapsingHeader::new("文件传输")
        .default_open(false)
        .show(ui, |ui| {
            let shown_dir = if state.remote_dir.is_empty() {
                "/".to_owned()
            } else {
                state.remote_dir.clone()
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("当前目录：{shown_dir}")).weak().small());
                if ui.button("上级").clicked() {
                    manager.list_remote_dir(&parent_virtual(&state.remote_dir));
                }
                if ui.button("刷新").clicked() {
                    manager.list_remote_dir(&state.remote_dir);
                }
            });

            if state.remote_entries.is_empty() {
                ui.label(RichText::new("（空目录或尚未加载，点「刷新」）").weak().small());
            }
            for entry in &state.remote_entries {
                ui.horizontal(|ui| {
                    let label = if entry.is_dir {
                        format!("[目录] {}", entry.name)
                    } else {
                        entry.name.clone()
                    };
                    if ui.selectable_label(false, label).clicked() {
                        if entry.is_dir {
                            manager.list_remote_dir(&entry.path);
                        } else {
                            manager.download_remote_file(&entry.path);
                        }
                    }
                    if !entry.is_dir {
                        ui.label(RichText::new(format!("{} B", entry.size)).weak().small());
                    }
                });
            }

            ui.add_space(4.0);
            // 上传：无原生文件选择器（不为此引入文件对话框依赖），只能手填本机绝对路径。
            ui.horizontal(|ui| {
                ui.label(RichText::new("上传").weak().small());
                ui.add(
                    egui::TextEdit::singleline(&mut v.inputs_mut().upload_local)
                        .desired_width(150.0)
                        .hint_text("本机绝对路径"),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut v.inputs_mut().upload_name)
                        .desired_width(110.0)
                        .hint_text("目标文件名"),
                );
                if ui.button("上传").clicked() {
                    let local = v.inputs_mut().upload_local.clone();
                    let name = v.inputs_mut().upload_name.clone();
                    if !local.is_empty() && !name.is_empty() {
                        let remote = join_virtual(&state.remote_dir, &name);
                        manager.upload_local_file(std::path::PathBuf::from(local), &remote);
                    }
                }
            });

            if let Some(note) = &state.transfer_note {
                ui.label(RichText::new(note.as_str()).weak().small());
            }
            for transfer in &state.transfers {
                ui.horizontal(|ui| {
                    ui.label(format!("{}（{}）", transfer.name, transfer.direction.label()));
                    let fraction = transfer.done as f32 / transfer.total.max(1) as f32;
                    ui.add(egui::ProgressBar::new(fraction).desired_width(120.0));
                    if ui.button("取消").clicked() {
                        manager.cancel_transfer(transfer.id);
                    }
                });
            }

            if let Some(dir) = manager.share_dir() {
                ui.label(RichText::new(format!("本机对外共享：{}", dir.display())).weak().small());
            }
        });
}

/// 延迟分档配色：`<60ms` 绿、`<150ms` 黄、其余红。
pub(crate) fn latency_color(ms: u32) -> Color32 {
    if ms < 60 {
        Color32::from_rgb(80, 200, 120)
    } else if ms < 150 {
        Color32::from_rgb(230, 190, 60)
    } else {
        Color32::from_rgb(230, 90, 90)
    }
}

/// 数据面路径的中文标签。
pub(crate) fn transport_label(path: TransportPath) -> &'static str {
    match path {
        TransportPath::Direct => "P2P 直连",
        TransportPath::Relay => "中继",
    }
}

/// 码率显示：`≥1000 kbps` 用 Mbps（一位小数），否则 kbps。
pub(crate) fn format_kbps(kbps: u32) -> String {
    if kbps >= 1000 {
        format!("{:.1} Mbps", f64::from(kbps) / 1000.0)
    } else {
        format!("{kbps} kbps")
    }
}

/// 虚拟路径的上一级（已在根时返回根 `""`）。
fn parent_virtual(path: &str) -> String {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some((parent, _)) => parent.to_owned(),
        None => String::new(),
    }
}

/// 把当前虚拟目录与文件名拼成完整虚拟路径。
fn join_virtual(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

#[cfg(test)]
mod tests {
    //! 只覆盖**纯逻辑**：工具条可见性状态机与「隐现不改画面」的几何不变量。真实的 egui 布局
    //! （面板注册、实测矩形）无法在无头环境验证，只能由这些单测锁住判定规则。

    use super::*;

    /// 造一条位于窗口上缘、指定高度的工具条矩形（满宽）。
    fn toolbar_rect(height: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, height))
    }

    /// 造一份决策输入（默认指针未移动、未按下）。
    fn inputs(
        autohide: bool,
        was_visible: bool,
        pointer: Option<egui::Pos2>,
        height: f32,
        idle: Duration,
    ) -> ToolbarInputs {
        ToolbarInputs {
            autohide,
            was_visible,
            pointer,
            toolbar_rect: toolbar_rect(height),
            moved: false,
            down: false,
            idle,
        }
    }

    #[test]
    fn autohide_off_keeps_toolbar_always_visible() {
        // autohide=false：无论指针在哪、空闲多久，都可见，且不重置活动计时。
        for pointer in [None, Some(egui::pos2(500.0, 400.0))] {
            for idle in [Duration::ZERO, Duration::from_secs(60)] {
                let d = decide_toolbar_visibility(inputs(false, false, pointer, 44.0, idle));
                assert!(d.visible, "关闭自动隐藏后工具条必须常驻");
                assert!(!d.reset_activity);
            }
        }
    }

    #[test]
    fn pointer_on_wrapped_second_row_keeps_toolbar_visible() {
        // 缺陷 3 的机械锁：工具条折行成两行（实测矩形高 88），指针停在第二行（y≈70）时仍算热区。
        // 旧判据硬编码 `y <= 44`，此时为 false → 工具条会从指针底下消失。
        let d = decide_toolbar_visibility(inputs(
            true,
            true,
            Some(egui::pos2(300.0, 70.0)),
            88.0,
            Duration::from_secs(10),
        ));
        assert!(d.visible, "指针停在折行后的第二行按钮上，工具条不该消失");
        assert!(d.reset_activity, "停在工具条上应重置空闲计时");
    }

    #[test]
    fn idle_pointer_away_hides_toolbar() {
        // 指针不在工具条矩形内且空闲超时 → 隐藏。
        let d = decide_toolbar_visibility(inputs(
            true,
            true,
            Some(egui::pos2(500.0, 400.0)),
            44.0,
            TOOLBAR_IDLE,
        ));
        assert!(!d.visible);
        assert!(!d.reset_activity);
    }

    #[test]
    fn pointer_at_top_edge_recalls_hidden_toolbar() {
        // 隐藏后指针移到上缘窄带（y <= TOOLBAR_HOT）→ 重新可见。
        let d = decide_toolbar_visibility(inputs(
            true,
            false,
            Some(egui::pos2(500.0, 2.0)),
            44.0,
            Duration::from_secs(10),
        ));
        assert!(d.visible);
    }

    #[test]
    fn hidden_toolbar_still_reserves_its_height() {
        // 缺陷 2 的机械锁（状态机侧）：隐藏时占位高度是缓存高度，绝不是 0；可见时返回 None
        // （高度由内容实测）。
        let hidden = toolbar_reserve(TOOLBAR_FALLBACK_HEIGHT, false);
        assert!(hidden.is_some_and(|h| (h - TOOLBAR_FALLBACK_HEIGHT).abs() < f32::EPSILON));
        assert!(toolbar_reserve(TOOLBAR_FALLBACK_HEIGHT, true).is_none());
    }

    #[test]
    fn scene_rect_is_independent_of_toolbar_visibility() {
        // 缺陷 2 的机械锁（几何侧）：工具条常驻占位，可见与隐藏得到**同一**中央可用尺寸，
        // 从而对同一 `avail`/图像尺寸给出同一 `display_size`——`Fit` 档不再改画面尺寸、固定档
        // 不再移位。
        // 中央区域刻意选**高度受限**的比例：这样工具条高度才会改变 Fit 档的缩放比，
        // 反例才有意义（宽度受限时两个 avail 会算出同一尺寸，断言形同虚设）。
        let central = egui::vec2(1280.0, 600.0);
        let image = (1920u32, 1080u32);
        // 可见时高度由内容决定（模型里取缓存值）；隐藏时用 `toolbar_reserve` 的占位高度。
        let visible_h = TOOLBAR_FALLBACK_HEIGHT;
        let hidden_h = toolbar_reserve(TOOLBAR_FALLBACK_HEIGHT, false).unwrap_or(0.0);
        let avail_visible = egui::vec2(central.x, central.y - visible_h);
        let avail_hidden = egui::vec2(central.x, central.y - hidden_h);
        assert_eq!(avail_visible, avail_hidden, "可见性不该改变中央可用尺寸");
        for scale in ViewScale::ALL {
            assert_eq!(
                display_size(scale, avail_visible, image),
                display_size(scale, avail_hidden, image),
                "{scale:?} 档下工具条可见性不该改变画面矩形",
            );
        }
        // 反例（缺陷 2 的原行为）：隐藏时面板不注册（占位 0）→ 中央区域变高 → Fit 档画面尺寸改变。
        let avail_buggy_hidden = central;
        assert_ne!(
            display_size(ViewScale::Fit, avail_buggy_hidden, image),
            display_size(ViewScale::Fit, avail_visible, image),
            "占位为 0 时 Fit 档画面尺寸会变——这正是被修掉的缺陷",
        );
    }
}
