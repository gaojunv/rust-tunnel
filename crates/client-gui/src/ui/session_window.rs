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
use rust_tunnel_remote::session::HostStats;
use rust_tunnel_remote::stats::{KeyframeReason, TransportStats};
use rust_tunnel_remote::{
    ActiveSessionInfo, CodecId, QualityPreset, RemoteManager, RemoteUiState, SessionRole,
    TransportPath,
};

use super::{icons::Icon, theme, widgets};
use crate::remote::nv12_renderer::Nv12Callback;
use crate::remote::{display_size, FramePaint, RemoteView, ViewScale, WindowSize, MIN_INNER};
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

/// 首帧尚未实测到工具条矩形时的兜底热区高度（逻辑点）。
///
/// 工具条现在是**浮层**、不再参与布局，因此这个值只用于「指针是否停在工具条上」的几何判据；
/// 实测矩形一到（见 [`RemoteView::toolbar_rect`]）就不再使用它。它也不再承担任何占位职责——
/// 浮层不影响中央面板的可用区域，画面不会因工具条隐现而位移。
const TOOLBAR_FALLBACK_HEIGHT: f32 = widgets::PILL_HEIGHT;

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
    let title = format!(
        "远程控制 — {} · {}",
        active.peer_name,
        state_label(active.state)
    );

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
            v.inner_size = v.desired_inner(monitor, ctx.pixels_per_point());
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

    // 首帧前把默认显示器同步给引擎：输入坐标按**所选显示器**的尺寸映射（见
    // `RemoteView::input_target_size`），而 `reset_session` 会把 `selected_display` 清成 `None`。
    // 选中前它回退到纹理尺寸——「流畅」档整体缩放过半，鼠标落点会明显偏移。旧工具条靠每帧渲染
    // `display_selector` 顺手同步单显示器；现在选择器进了设置弹层，必须在这里补上。
    if v.selected_display().is_none() {
        let displays = v.displays();
        if let Some(display) = displays.first() {
            v.set_selected_display(display.id, manager);
        }
    }

    // 本帧捕获判据：先清零，`capture_input` 真正运行时再置 true（见 `RemoteView::capture_this_frame`）。
    v.capture_this_frame = false;
    let embedded = matches!(class, egui::ViewportClass::Embedded);
    // 会话视口的样式覆盖：必须在**本视口的第一次绘制之前**装上，且守卫活到回调结束（析构时恢复
    // 主窗口样式）。放在 `render` 前而不是回调开头，是因为前面的逻辑只读视图状态、不画任何东西。
    let _style = theme::scoped_session_style(ctx);
    render(&mut v, ctx, manager, active, &state, embedded);
    // 设置弹层在画面**之后**画：它要以浮层身份盖在画面与工具条之上，并吃掉本帧的 Esc
    // （`Modal::should_close` 会消费它，故 `handle_shortcuts` 在弹层打开时不碰 Esc）。
    settings_modal(ctx, &mut v, manager, &state);

    // 本帧没捕获到（模式非完全控制 / 画面还没到 / 独占组合键刚被拦下），或窗口失焦/最小化 →
    // 补发一次输入抬起，避免远端残留按下的键。`release_all` 幂等，多调用无副作用。
    let view_unfocused = vp.minimized.unwrap_or(false) || vp.focused == Some(false);
    if !v.capture_this_frame || view_unfocused {
        v.release_input(manager);
    }

    // 延迟胶囊恒可见、每秒都在刷新读数，所以**总是**安排定时重绘：否则静止画面下心跳更新
    // 不会唤醒视图，胶囊上的延迟会停在上一次重绘的时刻。工具条可见时同理需要它按时淡出。
    ctx.request_repaint_after(Duration::from_millis(400));
}

/// 本端快捷键：`F11` 切全屏；全屏时 `Esc` 退出全屏（非全屏的 `Esc` 照常透传给远端，见
/// [`RemoteView::capture_input`] 对事件流的过滤）。
///
/// 设置弹层打开时**不处理** Esc：那是「关掉弹层」的语义，由 `egui::Modal::should_close` 自己
/// 消费（它 `consume_key` 掉这一帧的 Esc），本端若同时退出全屏，用户按一次 Esc 会连做两件事。
fn handle_shortcuts(ctx: &egui::Context, v: &mut RemoteView) {
    if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
        v.fullscreen = !v.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(v.fullscreen));
    } else if v.fullscreen && !v.settings_open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        // 刻意**不**在这里把 `v.fullscreen` 改成 false：本帧还要靠它让 `capture_input` 吞掉这次
        // Esc（否则它会透传给远端）。下一帧 `vp.fullscreen` 变 false 时自然同步。
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
    }
}

/// 工具条自动隐藏：指针移动、位于工具条**实测矩形**内、或指针按下时显示并重置计时；静止超时后隐藏。
/// 设置弹层打开期间**恒可见**——否则用户改完设置一抬头，工具条已经消失了。
///
/// 热区判据分两态：**可见时**用工具条浮层上一帧的实测矩形（[`RemoteView::toolbar_rect`]，
/// 展开时是贴顶面板、收起/隐藏时是那枚胶囊）；**隐藏时**认上缘窄带 [`TOOLBAR_HOT`]、以及
/// 那枚可见的胶囊本身（隐藏态的实测矩形就是胶囊）。
///
/// # 为什么用上一帧的矩形
///
/// egui 是立即模式：本帧渲染工具条时还不知道本帧的矩形（工具条自己就是被渲染对象）。用上一帧
/// 实测的矩形是标准做法，一帧延迟对「指针是否停在按钮上」无感。首帧尚未测得时用
/// [`toolbar_fallback_rect`] 兜底。
fn update_toolbar_visibility(ctx: &egui::Context, v: &mut RemoteView, autohide: bool) {
    if v.toolbar_collapsed {
        // 收起态 = 顶部只留胶囊：**不参与**自动隐藏——胶囊小到不挡画面，淡出反而让
        // 「只看延迟」落空。展开态可见性留给 [`RemoteView::expand_toolbar`] 重新起算。
        v.toolbar_visible = false;
        return;
    }
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
        modal_open: v.settings_open,
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
    /// 展开时是贴顶面板，收起/隐藏时是那枚小胶囊。
    toolbar_rect: egui::Rect,
    /// 本帧指针是否移动。
    moved: bool,
    /// 本帧是否有指针按下。
    down: bool,
    /// 距上次指针活动的空闲时长。
    idle: Duration,
    /// 设置弹层是否打开（打开时工具条恒可见，见 [`decide_toolbar_visibility`]）。
    modal_open: bool,
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
    if i.modal_open {
        // 设置弹层打开：工具条必须留在原地（它是弹层的召回入口，也是「当前档位」的读数）。
        // 同时重置活动计时——用户关掉弹层后，工具条还该完整停留一个 `TOOLBAR_IDLE` 窗口。
        return ToolbarDecision {
            visible: true,
            reset_activity: true,
        };
    }
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
            // 隐藏时：上缘窄带，或指针停在那枚胶囊上（隐藏态的实测矩形就是胶囊，见 `render`）。
            p.y <= TOOLBAR_HOT || i.toolbar_rect.contains(p)
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

/// 渲染窗口主体。`embedded` 分支（集成不支持多窗口时的兜底）退化为一个 egui 窗口承载同样内容。
///
/// # 浮层，而不是面板（缺陷「隐现挪动/缩放画面」的根除）
///
/// 工具条、重连提示、统计角标一律是 `egui::Area`/`egui::Window` 浮层，**不参与布局**：画面矩形
/// 只由窗口尺寸与缩放档位决定。于是工具条隐现既不改变 `Fit` 档的画面尺寸，也不上移固定档的画面
/// 原点——过去 `TopBottomPanel` 一注册就从中央区域扣掉自身高度，隐藏时占位为 0，画面会跳动；
/// 那套「缓存高度常驻占位」的机制随浮层一并删除了。**别再改回面板**。
///
/// # 顶部延迟胶囊 ↔ 贴顶面板
///
/// 窗口上缘**只画一个**浮层，两者互斥、上缘位置完全一致（都贴 y = 0、都上角直角）：
/// - 收起 / 自动隐藏 → 一枚小胶囊（只有延迟读数 + 下拉箭头），点它展开；
/// - 展开 → 一条贴顶面板（状态 + 延迟 + 档位 + 声音 + 动作），收起靠面板上的 `ChevronUp`。
///
/// 不要把两者同时画出来：延迟会在胶囊和面板里各出现一次，而且「小胶囊 + 大面板」两截叠在
/// 一起既重复又不成形状。
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
                if v.toolbar_collapsed {
                    collapsed_pill(ui, v, state);
                } else if v.toolbar_visible {
                    let bar = theme::toolbar_frame(ui.style()).show(ui, |ui| {
                        toolbar_contents(ui, v, manager, state, active);
                    });
                    // 实测矩形供**下一帧**的热区判据用（见 `update_toolbar_visibility`）。
                    v.toolbar_rect = Some(bar.response.rect);
                }
                if v.reconnect_note.is_some() {
                    theme::card_frame(ui.style()).show(ui, |ui| {
                        reconnect_strip(ui, v, manager);
                    });
                }
                if v.side_panel_open {
                    side_contents(ui, v, manager, state);
                }
                scene(ui, v, manager, active);
            });
        return;
    }

    // 画面先铺满整块中央区域（浮层不影响它的可用矩形）。
    egui::CentralPanel::default()
        .frame(theme::scene_frame(&ctx.style()))
        .show(ctx, |ui| scene(ui, v, manager, active));

    // 顶部只画**一个**浮层，二者互斥：
    // - 面板开着（`!collapsed && visible`）→ 一条贴顶面板，延迟等读数在它自己里面；
    // - 收起 / 自动隐藏 → 一枚同样贴顶的小胶囊，它既是唯一读数，也是重新展开的入口。
    // 两截同时出现既重复显示延迟，又会变成「小胶囊 + 大面板」的怪形状。
    let panel_open = !v.toolbar_collapsed && v.toolbar_visible;
    let rect = if panel_open {
        let bar = egui::Area::new(egui::Id::new("session_toolbar"))
            .order(egui::Order::Foreground)
            // 贴顶：锚点 y = 0，上缘不留缝（面板上角是直角，见 `toolbar_frame_top`）。
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                theme::toolbar_frame_top(ui.style()).show(ui, |ui| {
                    toolbar_contents(ui, v, manager, state, active);
                });
            });
        bar.response.rect
    } else {
        // 点胶囊即展开（`expand_toolbar`），计时也随之重置。
        let pill = egui::Area::new(egui::Id::new("session_toolbar_pill"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| collapsed_pill(ui, v, state));
        pill.response.rect
    };
    // 热区 = 当前那一个浮层的实测矩形（面板或胶囊）。
    v.toolbar_rect = Some(rect);

    // 重连提示浮在工具条下方（同为浮层：它出现/消失也不该挪动画面）。
    if v.reconnect_note.is_some() {
        egui::Area::new(egui::Id::new("session_reconnect"))
            .order(egui::Order::Foreground)
            .anchor(
                egui::Align2::CENTER_TOP,
                egui::vec2(0.0, widgets::PILL_HEIGHT + 44.0),
            )
            .show(ctx, |ui| {
                theme::card_frame(ui.style()).show(ui, |ui| {
                    reconnect_strip(ui, v, manager);
                });
            });
    }

    if v.side_panel_open {
        // 侧栏是**面板**（而不是浮层）：它确实要占据右侧一条，画面随之变窄——这是用户主动展开的
        // 结果，不是工具条那种隐现。标题栏自带关闭按钮，避免用户只能再点一次工具条上的「文件」。
        egui::SidePanel::right("session_side")
            .resizable(true)
            .default_width(300.0)
            .frame(theme::card_frame(&ctx.style()))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("文件与剪贴板").strong());
                    widgets::right_row_sized(ui, 26.0, |ui| {
                        if ui.button("关闭").clicked() {
                            v.side_panel_open = false;
                        }
                    });
                });
                ui.separator();
                side_contents(ui, v, manager, state);
            });
    }

    // 关键帧请求速率：管理面只累计计数，这里按累计值差分每秒结算一次（与帧率同款）。
    // 只有控制器会发关键帧请求；被控端保持 0，角标里不显示。
    if active.role == SessionRole::Controller {
        let total = state
            .keyframe_requests
            .iter()
            .copied()
            .fold(0_u32, u32::saturating_add);
        v.poll_keyframe_rate(total, Instant::now());
    }

    if v.stats_overlay {
        stats_overlay(ctx, v, active, state);
    }
}

/// 浮动工具条的内容（`ui` 位于 `theme::toolbar_frame` 之内）。
///
/// 只放**一眼要看**的东西与七个动作；所有「进去改一次」的设置都搬到 [`settings_modal`]——
/// 工具条曾经塞满选择器，窄窗口下折行成三排，把画面压掉一大块。
fn toolbar_contents(
    ui: &mut Ui,
    v: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    state: &RemoteUiState,
    active: &ActiveSessionInfo,
) {
    let p = theme::palette(ui.style());
    ui.horizontal(|ui| {
        // 状态点：延迟已知时按延迟上色（一眼看出链路好坏），否则用弱色。
        let dot = state.latency_ms.map_or(p.text_weak, latency_color);
        widgets::status_dot(ui, dot, state_label(active.state));

        match state.latency_ms {
            Some(ms) => widgets::pill(ui, &format!("{ms} ms"), latency_color(ms), p.accent_dim),
            None => {
                ui.label(RichText::new("延迟 —").color(p.text_weak).small());
            }
        }
        if let Some(path) = state.transport {
            ui.label(
                RichText::new(transport_label(path))
                    .color(p.text_weak)
                    .small(),
            );
        }

        ui.separator();

        // 当前档位摘要，同时是设置入口（点它和点「设置」等价）。
        // 用无框按钮：它读起来像一行标签，但整条都可点。
        let summary = format!("{} · {} fps", v.quality.label(), v.frame_rate());
        if ui
            .add(
                egui::Button::new(RichText::new(summary).color(p.text_weak).size(12.5))
                    .frame(false),
            )
            .on_hover_text("打开会话设置")
            .clicked()
        {
            v.settings_open = true;
        }

        ui.separator();
        audio_controls(ui, v, manager, state, &p);

        toolbar_actions(ui, v, manager, active, state.transport, &p);
    });
}

/// 工具条上的声音开关。
///
/// - 开关**同时**是本端静音与「让对端停止采集」：停止采集省 ~100 kbps，也少一份隐私暴露；
/// - 播放音量是「进去改一次」的东西，放在[会话设置](settings_sections)里——工具条每多一个
///   常驻控件，窄窗口下就多一次折行，声音只占一个图标；
/// - 对端不可用/被本地否决时按钮置灰，悬停显示原因。
fn audio_controls(
    ui: &mut Ui,
    v: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    state: &RemoteUiState,
    p: &theme::Palette,
) {
    let available = state.audio_available;
    // 图标跟**本地意图**走，而不是对端回报的 `state.audio_enabled`：后者要等一个来回去
    // 更新（甚至可能一直不更新），点下去没有即时反馈就会像「按钮坏了」。对端的真实状态
    // 放在悬停提示与统计角标里。
    let icon = if v.audio_enabled {
        Icon::Speaker
    } else {
        Icon::SpeakerMuted
    };
    let color = (!available).then_some(p.text_disabled);
    let tip = if !available {
        state
            .audio_reason
            .as_deref()
            .unwrap_or("本机或对端没有音频能力")
            .to_owned()
    } else if !v.audio_enabled {
        "开启声音转发".to_owned()
    } else if state.audio_enabled {
        "静音（同时停止对端采集，省带宽）".to_owned()
    } else if let Some(reason) = state.audio_reason.as_deref() {
        // 已请求但没在转发：被控端本地停止、设备故障或还没起播。
        format!("已请求转发，但对端未在发送：{reason}")
    } else {
        "已请求转发，等待对端开始发送".to_owned()
    };
    let response = ui
        .add_enabled_ui(available, |ui| {
            widgets::toolbar_icon_colored(ui, icon, &tip, color)
        })
        .inner;
    if response.clicked() {
        v.audio_enabled = !v.audio_enabled;
        manager.set_audio_enabled(v.audio_enabled);
    }
}

/// 工具条右侧的动作区：一律用图标按钮（文字按钮在窄窗口下会把工具条撑成两三行）。
///
/// 仍按**从左到右**排：`egui::Area` 按内容自适应宽度，`right_to_left` 会把第一个控件推到
/// 「可用宽度」的右缘（Area 的可用宽度是整个视口），整张卡片会被撑到全屏宽。
/// 收起放最前（误触的后果只是缩成一枚延迟胶囊，点一下就能回来），「断开」留在末尾
/// （破坏性操作不该和常用按钮做邻居）。
fn toolbar_actions(
    ui: &mut Ui,
    v: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    active: &ActiveSessionInfo,
    transport: Option<TransportPath>,
    p: &theme::Palette,
) {
    ui.add_space(4.0);
    // 只在中继会话上出现：再点一次也只是「再试一次打洞」，不会打断会话。
    if transport == Some(TransportPath::Relay)
        && widgets::toolbar_icon(
            ui,
            Icon::Route,
            "立即尝试把会话升级到 P2P（当前走中继；失败不影响会话）",
        )
        .clicked()
    {
        manager.trigger_migration();
    }
    if widgets::toolbar_icon(ui, Icon::ChevronUp, "收起成顶部延迟胶囊（点胶囊展开下拉）").clicked()
    {
        v.collapse_toolbar();
    }
    if widgets::toolbar_icon(
        ui,
        Icon::Settings,
        "画质 / 帧率 / 显示器 / 缩放 / 窗口尺寸 / 交互",
    )
    .clicked()
    {
        v.settings_open = true;
    }
    if widgets::toolbar_toggle(ui, Icon::Download, v.side_panel_open, "文件传输与剪贴板").clicked()
    {
        v.side_panel_open = !v.side_panel_open;
    }
    // 锁屏只在「完全控制」模式可用：仅观看/仅文件传输不该改变远端机器状态。
    let can_lock = active.mode == RemoteMode::Control;
    let lock = ui
        .add_enabled_ui(can_lock, |ui| {
            let color = (!can_lock).then_some(p.text_disabled);
            widgets::toolbar_icon_colored(
                ui,
                Icon::Lock,
                "锁定远端屏幕（会话保持连接；锁屏后画面可能无法继续采集）",
                color,
            )
        })
        .inner;
    let lock = if can_lock {
        lock
    } else {
        lock.on_disabled_hover_text("仅『完全控制』模式可锁定远端屏幕")
    };
    if lock.clicked() {
        manager.lock_screen();
    }
    if widgets::toolbar_toggle(
        ui,
        Icon::Monitor,
        v.fullscreen,
        "切换全屏（F11；全屏时 Esc 退出）",
    )
    .clicked()
    {
        v.fullscreen = !v.fullscreen;
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Fullscreen(v.fullscreen));
    }
    if widgets::toolbar_icon(
        ui,
        Icon::ChevronDown,
        "关闭窗口但保留会话；在「远程」Tab 可召回",
    )
    .clicked()
    {
        // 置 false 后父窗口就不再注册这个 viewport，回调从此不再跑——必须**当场**补发一次
        // 输入抬起，否则用户此刻按住的键会在远端一直按着（会话仍在，没人替它收尾）。
        v.release_input(manager);
        v.window_open = false;
    }
    if widgets::toolbar_icon_colored(ui, Icon::Close, "结束本次远程会话", Some(p.err)).clicked()
    {
        manager.abort_active();
    }
}

/// 顶部延迟胶囊（工具条面板的收起形态）：一枚贴着上缘的小胶囊，显示当前链路延迟。
///
/// 「只显示延迟」是用户的原话，所以这里**不**再放状态点、路径、档位摘要——多一个字符就多挡一点
/// 画面。延迟未知时显示 `延迟 —`，形状与有值时一致（收起的工具条不会因为一次心跳丢包而跳大小）。
///
/// 点它 = 展开面板（[`RemoteView::expand_toolbar`]）。面板展开时**不再画胶囊**——延迟在面板里
/// 已经有一份，两截同显既重复又难看；收起是面板上的「收起」按钮（`ChevronUp`）或自动隐藏。
fn collapsed_pill(ui: &mut Ui, v: &mut RemoteView, state: &RemoteUiState) {
    let p = theme::palette(ui.style());
    let (text, color) = state.latency_ms.map_or_else(
        || ("延迟 —".to_owned(), p.text_weak),
        |ms| (format!("{ms} ms"), latency_color(ms)),
    );
    if widgets::pill_button(ui, &text, color, "展开工具条").clicked() {
        v.expand_toolbar();
    }
}

/// 会话设置弹层：旧工具条上所有「进去改一次」的项都在这里。
///
/// 用 `egui::Modal`（而非自绘浮窗）是为了让输入天然被隔断：模态层铺满整屏并吃掉背景点击，
/// [`RemoteView::capture_input`] 又在 `settings_open` 时直接返回——于是弹层里的每一次点击与
/// 按键都不会被注入远端（把 Ctrl+A 之类的本地操作发给远端是灾难）。关掉弹层的那一次指针活动
/// 会重置自动隐藏计时，否则用户改完设置、手没动，工具条会在松手 3 秒后立刻消失。
fn settings_modal(
    ctx: &egui::Context,
    v: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    state: &RemoteUiState,
) {
    if !v.settings_open {
        return;
    }
    let resp = egui::Modal::new(egui::Id::new("session_settings")).show(ctx, |ui| {
        ui.set_width(460.0);
        let mut close = widgets::modal_title(ui, "会话设置");
        ui.separator();
        // 限高滚动：小窗口里也不会把底部的「关闭」挤出屏幕。
        egui::ScrollArea::vertical()
            .max_height(420.0)
            .show(ui, |ui| settings_sections(ui, v, manager));
        ui.separator();

        // 底部摘要：当前链路的几项读数，带单位，省得用户为了看一眼延迟去开统计角标。
        let mut parts = Vec::new();
        if let Some(ms) = state.latency_ms {
            parts.push(format!("延迟 {ms} ms"));
        }
        if let Some(path) = state.transport {
            parts.push(transport_label(path).to_owned());
        }
        let (width, height) = v.frame_dims();
        if width > 0 && height > 0 {
            parts.push(format!("{width}×{height}"));
        }
        if let Some(ms) = state.decode_ms {
            parts.push(format!("解码 {ms} ms"));
        }
        if let Some(kbps) = state.recv_kbps {
            parts.push(format!("收码率 {}", format_kbps(kbps)));
        }
        ui.label(RichText::new(parts.join(" · ")).weak().small());
        ui.add_space(4.0);
        widgets::right_row(ui, |ui| {
            if widgets::primary_button(ui, "关闭").clicked() {
                close = true;
            }
            if widgets::secondary_button(ui, "测延迟")
                .on_hover_text("立刻探测一次往返延迟")
                .clicked()
            {
                manager.ping();
            }
        });
        close
    });
    // `should_close()` 处理 Esc 与「点背景关闭」；它会在 Esc 按下时**消费**该键，故
    // `handle_shortcuts` 在弹层打开时不再看 Esc（否则按一次会连退出全屏）。
    if resp.inner || resp.should_close() {
        v.settings_open = false;
        v.last_pointer_move = Instant::now();
    }
}

/// 设置弹层的四个小节（画面 / 交互 / 声音 / 界面）。
///
/// 从 [`settings_modal`] 拆出来只为控制函数长度；内容与「逐项改一次」的语义完全对应
/// （各控件在改动时才下发，不做每帧轮询）。四个小节顺序展开，再拆只会把共享的 `v`/`manager`
/// 反复传一遍，故整体豁免行长上限。
#[expect(
    clippy::too_many_lines,
    reason = "四个设置小节顺序展开；再拆分只会把共享的 v/manager 反复传递"
)]
fn settings_sections(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>) {
    widgets::section(ui, "画面", |ui| {
        widgets::labeled_row(ui, "画质", |ui| {
            let picked = widgets::segmented(
                ui,
                v.quality,
                &[
                    (QualityPreset::Smooth, "流畅"),
                    (QualityPreset::Balanced, "均衡"),
                    (QualityPreset::Hd, "高清"),
                ],
            );
            if let Some(preset) = picked {
                v.set_quality(preset, manager);
            }
        });
        widgets::labeled_row(ui, "帧率上限", |ui| {
            // 帧率是**上限**：QoS 闭环仍可在拥塞时降到 15。
            let picked = widgets::segmented(ui, v.frame_rate(), &[(30u8, "30"), (60u8, "60")]);
            if let Some(fps) = picked {
                v.set_frame_rate(fps, manager);
            }
        });
        widgets::labeled_row(ui, "显示器", |ui| {
            display_selector(ui, v, manager);
        });
        widgets::labeled_row(ui, "缩放", |ui| {
            // 固定档忽略可用区域、溢出交给滚动区（见 `ViewScale`）。比例是**远端像素 :
            // 物理像素**：「100%」在任何 DPI 下都不会被放大（其余档位同理）。
            egui::ComboBox::from_id_salt("session_scale")
                .selected_text(v.scale.label())
                .show_ui(ui, |ui| {
                    for scale in ViewScale::ALL {
                        ui.selectable_value(&mut v.scale, scale, scale.label());
                    }
                });
            let hint = match v.scale {
                ViewScale::Fit => "等比铺满窗口（可放大可缩小）",
                ViewScale::FitNoUpscale => "只缩不放：窗口更大时按 1:1 居中留白",
                _ => "按物理像素，100% = 不缩放；超出窗口时中键拖拽平移",
            };
            ui.label(RichText::new(hint).weak().small());
        });
        // 放大增强：只影响本机 GPU 上屏（FSR1 或 双线性+CAS），1:1 与缩小时着色器自动不生效。
        // 改的是**持久化镜像**，由父窗口的 `sync_display_prefs` 回写 gui.toml。
        widgets::switch_row(
            ui,
            "放大时增强",
            Some("远端画面被放大时做 FSR1 / CAS 增强；1:1 与缩小时自动不生效"),
            &mut v.sharpen,
        );
        widgets::labeled_row(ui, "放大算法", |ui| {
            egui::ComboBox::from_id_salt("session_upscale_mode")
                .selected_text(v.upscale_mode.label())
                .show_ui(ui, |ui| {
                    for mode in crate::prefs::UpscaleMode::ALL {
                        ui.selectable_value(&mut v.upscale_mode, mode, mode.label());
                    }
                });
        });
        widgets::labeled_row(ui, "锐化强度", |ui| {
            // 关闭时滑杆置灰但保留数值：用户下次打开开关不用重调。
            ui.add_enabled(
                v.sharpen,
                egui::Slider::new(&mut v.sharpen_strength, 0..=100).suffix("%"),
            )
            .on_hover_text("默认 42%；过高会让文字出现描边感");
        });
        widgets::labeled_row(ui, "窗口尺寸", |ui| {
            let mut picked = v.window_size;
            egui::ComboBox::from_id_salt("session_window_size")
                .selected_text(picked.label())
                .show_ui(ui, |ui| {
                    for size in WindowSize::ALL {
                        ui.selectable_value(&mut picked, size, size.label());
                    }
                });
            if picked != v.window_size {
                // 改档即重算内尺寸并锁死（`size_initialized`），下一帧 `patch` 会下发。
                v.window_size = picked;
                let monitor = ui.ctx().input(|i| i.viewport().monitor_size);
                v.inner_size = v.desired_inner(monitor, ui.ctx().pixels_per_point());
                v.size_initialized = true;
            }
        });
    });

    widgets::section(ui, "交互", |ui| {
        // 关同步就丢弃去重基线：下次开启会以当时的剪贴板重新开始，而不是补发
        // 关闭期间积压的变化。
        if widgets::switch_row(ui, "剪贴板自动同步", None, &mut v.clipboard_sync_enabled)
            && !v.clipboard_sync_enabled
        {
            v.clipboard_sync = None;
        }
        widgets::switch_row(
            ui,
            "独占键盘",
            Some("键盘事件全部发给远端；Ctrl/Cmd+Alt+Shift+K 退出"),
            &mut v.exclusive_keyboard,
        );
    });

    audio_section(ui, v, manager);

    widgets::section(ui, "界面", |ui| {
        // 收起 = 画面之上只留一枚延迟胶囊。它是**持久化**的显式选择（工具条上的「收起」也改它），
        // 这里是同一状态的第二个入口：收起后工具条虽然不见了，设置里仍看得到、也改得回来。
        // 走一对方法而不是裸改字段：展开时还要置可见并重置自动隐藏计时，否则开启自动隐藏的
        // 用户一取消勾选，工具条会停在「隐藏」状态，非得再挪一下鼠标才出来。
        if widgets::switch_row(
            ui,
            "收起工具条",
            Some("只显示延迟胶囊；点它可展开"),
            &mut v.toolbar_collapsed,
        ) {
            if v.toolbar_collapsed {
                v.collapse_toolbar();
            } else {
                v.expand_toolbar();
            }
        }
        // 自动隐藏只作用于展开态（收起态本来就不参与），因此在收起期间禁用，免得用户以为
        // 两个开关在打架。
        // 自动隐藏开关：这是**唯一**能让用户关掉「工具条偶尔消失」的入口。改动写进
        // 运行时镜像，由父窗口的 `sync_display_prefs` 回写 gui.toml。
        // `switch_row` 不感知 `Ui` 的 enabled 状态，故收起时用副标题说明「为什么不生效」。
        let hint = if v.toolbar_collapsed {
            "收起工具条时不生效"
        } else {
            "静止后淡出；移到窗口上缘召回"
        };
        ui.add_enabled_ui(!v.toolbar_collapsed, |ui| {
            widgets::switch_row(ui, "工具条自动隐藏", Some(hint), &mut v.toolbar_autohide);
        });
        widgets::switch_row(ui, "显示统计角标", None, &mut v.stats_overlay);
        // 详细档：三组指标（链路 / 被控端 / 画面）。这里只改**持久化镜像**，由父窗口的
        // `sync_display_prefs` 回写 gui.toml；角标关闭时也可先勾选，打开角标即按详细档渲染。
        widgets::switch_row(ui, "统计角标显示详细指标", None, &mut v.stats_detail);
    });
}

/// 「声音」小节：本地播放音量。
///
/// 工具条上只留扬声器开关（每多一个常驻控件，窄窗口就多一次折行）；音量是「想起来才调
/// 一次」的东西，放这里。它是本地增益、不上线，对端仍在发流。
fn audio_section(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>) {
    widgets::section(ui, "声音", |ui| {
        widgets::labeled_row(ui, "播放音量", |ui| {
            let mut volume = v.audio_volume;
            if ui
                .add(egui::Slider::new(&mut volume, 0..=100).suffix("%"))
                .on_hover_text("本机播放音量，不改变对端采集；静音请用工具条扬声器")
                .changed()
            {
                v.audio_volume = volume;
                manager.set_audio_volume(u32::from(volume));
            }
        });
    });
}

/// 显示器选择：多显示器时下拉切换，单显示器时把当前显示器同步给引擎。
fn display_selector(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>) {
    let displays = v.displays();
    if displays.len() > 1 {
        let current = v
            .selected_display()
            .or_else(|| displays.first().map(|d| d.id));
        let mut picked = current;
        egui::ComboBox::from_id_salt("session_display")
            .selected_text(
                current
                    .and_then(|id| displays.iter().find(|d| d.id == id))
                    .map_or("—", |d| d.name.as_str()),
            )
            .show_ui(ui, |ui| {
                for display in &displays {
                    ui.selectable_value(&mut picked, Some(display.id), display.name.as_str());
                }
            });
        if let Some(id) = picked.filter(|id| Some(*id) != current) {
            v.set_selected_display(id, manager);
        }
    } else if let Some(display) = displays.first() {
        // 只有一台：没有可选的内容，但仍要把编号同步给引擎（否则输入坐标映射不到显示器）。
        ui.label(display.name.as_str());
        v.set_selected_display(display.id, manager);
    } else {
        ui.label(RichText::new("—").weak());
    }
}

/// 重连提示条：文案 + 「立即重试 / 停止」（外层 frame 由调用方给）。
fn reconnect_strip(ui: &mut Ui, v: &mut RemoteView, manager: &Arc<RemoteManager>) {
    let Some(note) = v.reconnect_note.clone() else {
        return;
    };
    let warn = theme::palette(ui.style()).warn;
    ui.horizontal(|ui| {
        ui.colored_label(warn, note);
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
}

/// 画面区域：按缩放档位决定绘制尺寸。两个「适应窗口」档都等比适配可用区域
/// （`Fit` 铺满、`FitNoUpscale` 只缩不放）；固定档位用滚动区承载溢出，并支持中键拖拽平移。
fn scene(
    ui: &mut Ui,
    v: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    active: &ActiveSessionInfo,
) {
    let Some(paint) = v.upload(ui.ctx()) else {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("等待画面…").weak());
            ui.spinner();
        });
        return;
    };
    let (width, height) = paint.size();
    let avail = ui.available_size();
    // 缩放比例以**物理像素**为基准（见 `view_scale` 的模块文档）：不传 DPI 的话，
    // 150% 屏幕上「100%」会被画成 1.5 倍的双线性放大，「适应窗口」也会整体偏大。
    let want = display_size(v.scale, avail, (width, height), ui.ctx().pixels_per_point());
    if v.scale.is_fit() {
        // 「适应窗口」等比适配可用区域（`Fit` 可放大/缩小，`FitNoUpscale` 只缩不放）。
        // 比例按**物理像素**算，绘制矩形再对齐到整物理像素（见 `paint_image`）。
        let (_id, outer) = ui.allocate_space(avail);
        let rect = egui::Rect::from_center_size(outer.center(), want.min(avail));
        paint_image(ui, v, manager, active, &paint, rect);
    } else {
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // 内容**至少铺满视口**：画面比窗口小时才能居中（ScrollArea 的内容默认左上对齐），
                // 比窗口大时内容就是画面本身，溢出交给滚动条。
                let viewport = ui.available_size();
                let content = want.max(viewport);
                let (outer, _) = ui.allocate_exact_size(content, egui::Sense::click_and_drag());
                let rect = egui::Rect::from_center_size(outer.center(), want.min(content));
                paint_image(ui, v, manager, active, &paint, rect);
                // 平移放在绘制之后：`capture_input` 会按远端光标形状设置图标，这里再把
                // 「抓手」盖上去。只有内容真的溢出时才有得平移。
                pan_with_middle(
                    ui,
                    v,
                    rect,
                    content - viewport,
                    active.mode == RemoteMode::Control,
                );
            });
    }
}

/// 中键拖拽平移（仅当画面溢出视口）。
///
/// 完全控制模式下，中键**按下**由 [`RemoteView::capture_input`] 门控：它不把这次按下转达
/// 远端，而是登记 `pan_anchor`（拖动过就只平移，原地松开才补发一次中键点按）。这里只负责把
/// 指针位移换算成滚动区偏移。
///
/// 仅观看 / 仅文件传输模式不注入远端输入，没有「中键该不该转达」的问题，故直接按「指针在
/// 画面上」判定——否则这两种模式下中键平移会整个失效。用 `ScrollAnimation::none()` 而不是
/// 缓动：拖拽必须跟手，缓动会有橡皮筋感。
fn pan_with_middle(
    ui: &Ui,
    v: &RemoteView,
    rect: egui::Rect,
    overflow: egui::Vec2,
    injecting: bool,
) {
    if overflow.x <= 0.5 && overflow.y <= 0.5 {
        return;
    }
    let (down, delta, pos) = ui.input(|i| {
        (
            i.pointer.button_down(egui::PointerButton::Middle),
            i.pointer.delta(),
            i.pointer.latest_pos(),
        )
    });
    if !down || delta == egui::Vec2::ZERO {
        return;
    }
    // 注入模式下只认锚点（工具条上按下中键不该平移）；非注入模式看指针是否在画面最上层。
    let engaged = if injecting {
        v.pan_anchor.is_some()
    } else {
        pos.is_some_and(|p| rect.contains(p) && ui.ctx().layer_id_at(p) == Some(ui.layer_id()))
    };
    if !engaged {
        return;
    }
    // `scroll_with_delta(d)` 让滚动偏移减少 `d`（见 egui `ScrollArea` 的实现），
    // 所以直接传指针位移就是「内容跟着指针一起走」。
    ui.scroll_with_delta_animation(delta, egui::style::ScrollAnimation::none());
    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
}

/// 画一张画面并（完全控制模式时）在其上捕获输入。
///
/// 纹理路径直接 `painter().image`；NV12 路径挂一个 `egui_wgpu` 回调（Y/UV 纹理 + 着色器，
/// GPU 完成 BT.709 转换与缩放）。回调只在 wgpu 渲染器下被调用——`RemoteView::upload` 只在
/// 拿到 `wgpu_render_state` 时才返回 [`FramePaint::Nv12`]，故 Glow 下不会走到这里。
fn paint_image(
    ui: &mut Ui,
    v: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    active: &ActiveSessionInfo,
    image: &FramePaint,
    rect: egui::Rect,
) {
    // 绘制矩形对齐到物理像素网格：起点/尺寸是小数时，双线性采样的相位落在纹素之间，
    // 整幅画面（尤其文字）会各向异性发虚。
    let ppp = ui.ctx().pixels_per_point();
    let rect = snap_to_physical(rect, ppp);
    match image {
        FramePaint::Texture { id, .. } => {
            ui.painter().image(
                *id,
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        FramePaint::Nv12 {
            frame,
            target_format,
            ..
        } => {
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                rect,
                Nv12Callback {
                    frame: Arc::clone(frame),
                    target_format: *target_format,
                    // 开关只决定传 0 还是滑杆当前强度；着色器仍只在画面被放大时才真正锐化。
                    // FSR 模式下它是 RCAS 的 `con`，CAS 模式下是 CAS 的 `sharpness`，语义一致。
                    sharpen: if v.sharpen {
                        crate::remote::nv12_renderer::cas_sharpen_strength(v.sharpen_strength)
                    } else {
                        0.0
                    },
                    use_fsr: v.upscale_mode == crate::prefs::UpscaleMode::Fsr,
                    out_size: physical_size(rect, ppp),
                },
            ));
        }
    }
    v.view_scale_permille = view_scale_permille(rect, image.size(), ppp);
    // 只有「完全控制」模式捕获输入：仅观看 / 仅文件传输不该注入键鼠。
    if active.mode == RemoteMode::Control {
        // 坐标按所选显示器的尺寸映射（由 `capture_input` 内部取），与纹理是否缩放无关。
        v.capture_input(ui, manager, rect);
    }
}

/// 画面绘制比例（千分比）：1 个远端像素实际占多少物理像素 ×1000。
///
/// 1000 = 严格 1:1（无重采样）；大于 1000 是放大（双线性，必然发虚）；小于 1000 是缩小
/// （欠采样，文字会闪）。统计角标显示它，用来区分「编码糊」与「显示缩放糊」。
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "比例有界（画面尺寸远小于 i64/f32 精度上限），且只用于显示"
)]
fn view_scale_permille(rect: egui::Rect, image: (u32, u32), pixels_per_point: f32) -> u32 {
    let (width, _) = image;
    if width == 0 {
        return 0;
    }
    let ratio = rect.width() * pixels_per_point / width as f32;
    if !ratio.is_finite() || ratio <= 0.0 {
        return 0;
    }
    u32::try_from((f64::from(ratio) * 1000.0).round() as i64).unwrap_or(0)
}

/// 把绘制矩形对齐到**物理像素**网格（起点与尺寸都取整到 1 物理像素的整数倍）。
///
/// 远端画面是逐像素内容：只要采样点不在纹素中心、或缩放比不是整数，双线性插值就会把
/// 每个像素和邻居混一次——文字边缘首先受害。egui 的几何量是逻辑点，在 125%/150% 这类
/// 非整数 DPI 下几乎必然带小数，故在真正绘制前对齐一次。
fn snap_to_physical(rect: egui::Rect, pixels_per_point: f32) -> egui::Rect {
    let ppp = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let min_px = (rect.min.to_vec2() * ppp).round();
    let size_px = (rect.size() * ppp).round().max(egui::Vec2::splat(1.0));
    egui::Rect::from_min_size((min_px / ppp).to_pos2(), size_px / ppp)
}

/// 回调 rect 的**物理像素**尺寸（FSR 的离屏目标按它建）。
///
/// `snap_to_physical` 已把 rect 对齐到整物理像素，故这里 round 只是消掉浮点尾差；
/// `max(1.0)` 保证不会出现 0 尺寸纹理（窗口被压到 0 时也安全）。
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "尺寸有界（屏幕远小于 u32 上限），max(1.0) 后非负，仅用于建纹理"
)]
fn physical_size(rect: egui::Rect, ppp: f32) -> (u32, u32) {
    let w = (rect.width() * ppp).round().max(1.0) as u32;
    let h = (rect.height() * ppp).round().max(1.0) as u32;
    (w, h)
}

/// 统计角标（右上角悬浮）。简洁 / 详细两档，数据全部来自本端读数与被控端每秒上报的
/// [`HostStats`]（协议 v8）。
///
/// # 为什么是 `egui::Area` + `interactable(false)`，而不是 `egui::Window`
///
/// 角标盖在画面上，但用户的点击必须**落到远桌面**（点角标附近不该变成「点了本地 UI」）。
/// [`RemoteView::capture_input`] 判定「指针是否在画面上」用的是
/// `ctx.layer_id_at(pos) == Some(画面面板层)`；egui 0.31 的 `Memory::layer_id_at` **只遍历
/// `AreaState::interactable` 为真的层**（见 `egui::Memory::layer_id_at` 的实现）。`egui::Window`
/// 永远是可交互层，会把角标下方一片区域吞掉；`Area` 调 `.interactable(false)` 后则被直接跳过，
/// 点击自然穿透到画面。**不要**在角标里放任何按钮/拖动/滚动控件——它们会把层重新变成可交互。
fn stats_overlay(
    ctx: &egui::Context,
    v: &RemoteView,
    active: &ActiveSessionInfo,
    state: &RemoteUiState,
) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(active.started_at_ms, |d| {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
        });
    let secs = now_ms.saturating_sub(active.started_at_ms) / 1000;
    egui::Area::new(egui::Id::new("session_stats_overlay"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 10.0))
        .interactable(false)
        .show(ctx, |ui| {
            theme::stats_frame(ui.style()).show(ui, |ui| {
                if v.stats_detail {
                    stats_detail(ui, v, active, state, secs);
                } else {
                    stats_compact(ui, state, secs);
                }
            });
        });
}

/// 简洁档：一眼要看的十行。未知项一律 `—`。
fn stats_compact(ui: &mut Ui, state: &RemoteUiState, secs: u64) {
    let host = state.host.as_ref();
    egui::Grid::new("session_stats_compact")
        .num_columns(2)
        .spacing([14.0, 4.0])
        .show(ui, |ui| {
            stat_label(ui, "延迟");
            latency_cell(ui, state.latency_ms);
            ui.end_row();

            stat_label(ui, "路径");
            ui.label(state.transport.map_or(UNKNOWN, transport_label));
            ui.end_row();

            // 编码器 / 采集：后端 + GPU/CPU。被控端上报（B1/B6/B7）。
            stat_label(ui, "编码器");
            ui.label(host_field(host, |h| {
                format!("{} · {}", dash(&h.encoder_backend), gpu_cpu(h.gpu_encode))
            }));
            ui.end_row();

            stat_label(ui, "采集");
            ui.label(host_field(host, |h| {
                format!("{} · {}", dash(&h.capture_backend), gpu_cpu(h.gpu_capture))
            }));
            ui.end_row();

            stat_label(ui, "采集耗时");
            ui.label(host_field(host, |h| format_us(h.capture_us)));
            ui.end_row();

            stat_label(ui, "编码耗时");
            ui.label(host_field(host, |h| format!("{} ms", h.encode_ms)));
            ui.end_row();

            stat_label(ui, "实测 fps");
            ui.label(host_field(host, |h| format!("{} fps", h.fps)));
            ui.end_row();

            // 「实际出码」= 真正发出去的字节速率（`sent_kbps`），不是 QoS 的目标值。
            stat_label(ui, "码率");
            ui.label(host_field(host, |h| format_kbps(h.sent_kbps)));
            ui.end_row();

            // 端到端：被控端采集完成 → 本端解码完成（`SessionStats::Decode.e2e_ms`）。
            stat_label(ui, "端到端");
            ui.label(e2e_cell(state.e2e_ms));
            ui.end_row();

            stat_label(ui, "时长");
            ui.label(format_duration(secs));
            ui.end_row();
        });
}

/// 详细档：链路 / 被控端 / 画面三组。第一行横排两组，第二行单独放「画面」，
/// 这样窄窗口下也不会把三张表挤成不可读的一列。
fn stats_detail(
    ui: &mut Ui,
    v: &RemoteView,
    active: &ActiveSessionInfo,
    state: &RemoteUiState,
    secs: u64,
) {
    let host = state.host.as_ref();
    ui.horizontal_top(|ui| {
        stats_group(ui, "链路", |ui| link_grid(ui, v, state, secs));
        ui.add_space(18.0);
        stats_group(ui, "被控端", |ui| host_grid(ui, host));
    });
    ui.add_space(6.0);
    stats_group(ui, "画面", |ui| local_grid(ui, v, active, state));
}

/// 一组指标：小标题 + 一张两列 Grid。
fn stats_group(ui: &mut Ui, title: &str, contents: impl FnOnce(&mut Ui)) {
    let p = theme::palette(ui.style());
    ui.vertical(|ui| {
        ui.label(RichText::new(title).strong().small().color(p.text));
        contents(ui);
    });
}

/// 「链路」组：延迟 / 抖动 / 路径 / 可靠层重传与乱序 / 端到端 / 接收侧读数。
fn link_grid(ui: &mut Ui, v: &RemoteView, state: &RemoteUiState, secs: u64) {
    let ts = state.transport_stats.as_ref();
    egui::Grid::new("session_stats_link")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            stat_label(ui, "延迟");
            latency_cell(ui, state.latency_ms);
            ui.end_row();

            stat_label(ui, "抖动");
            ui.label(opt_or_dash(state.jitter_ms.map(|ms| format!("{ms} ms"))));
            ui.end_row();

            stat_label(ui, "路径");
            ui.label(state.transport.map_or(UNKNOWN, transport_label));
            ui.end_row();

            // 可靠层（只有 UDP 打洞路径有；直连/中继恒为 —）。
            stat_label(ui, "重传");
            ui.label(transport_field(ts, |t| {
                format!("RTO {} · 快速 {}", t.rto, t.fast_retransmit)
            }));
            ui.end_row();

            stat_label(ui, "乱序");
            ui.label(transport_field(ts, |t| t.out_of_order.to_string()));
            ui.end_row();

            stat_label(ui, "重复 / 窗口外");
            ui.label(transport_field(ts, |t| {
                format!("{} / {}", t.duplicate, t.out_of_window)
            }));
            ui.end_row();

            stat_label(ui, "端到端（到达）");
            ui.label(e2e_cell(state.e2e_ms));
            ui.end_row();

            stat_label(ui, "端到端（上屏）");
            ui.label(opt_or_dash(
                screen_e2e_ms(v, state).map(|ms| format!("{ms} ms")),
            ));
            ui.end_row();

            stat_label(ui, "收码率");
            ui.label(opt_or_dash(state.recv_kbps.map(format_kbps)));
            ui.end_row();

            stat_label(ui, "收包");
            ui.label(opt_or_dash(state.decode_packets.map(|n| format!("{n}/s"))));
            ui.end_row();

            stat_label(ui, "丢包");
            ui.label(opt_or_dash(state.decode_dropped.map(|n| format!("{n}/s"))));
            ui.end_row();

            stat_label(ui, "补丁");
            ui.label(opt_or_dash(state.decode_patches.map(|n| format!("{n}/s"))));
            ui.end_row();

            stat_label(ui, "关键帧请求");
            ui.label(format!("{}/s", v.keyframe_rate));
            ui.end_row();

            // C3 的「原因」：控制器侧按原因累计（会话内），只列出非零项；全零时显示 —。
            stat_label(ui, "关键帧原因");
            ui.label(keyframe_reason_summary(&state.keyframe_requests));
            ui.end_row();

            stat_label(ui, "输入");
            ui.label(format!("{}/s", v.input_rate));
            ui.end_row();

            stat_label(ui, "解码积压");
            ui.label(opt_or_dash(state.decode_backlog.map(|n| format!("{n} 帧"))));
            ui.end_row();

            stat_label(ui, "音频");
            ui.label(audio_summary(state));
            ui.end_row();

            stat_label(ui, "时长");
            ui.label(format_duration(secs));
            ui.end_row();
        });
}

/// 统计角标的音频摘要：可用性/启停/播放读数。
///
/// `audio_underruns` / `audio_dropped` 是会话内**累计值**（不换算成每秒），
/// 因为抖动缓冲的自适应本来就以会话为尺度。
fn audio_summary(state: &RemoteUiState) -> String {
    if !state.audio_available {
        return state
            .audio_reason
            .clone()
            .unwrap_or_else(|| "不可用".to_owned());
    }
    if !state.audio_enabled {
        return state
            .audio_reason
            .clone()
            .unwrap_or_else(|| "未开启".to_owned());
    }
    format!(
        "Opus 96 kbps · 收包 {} · 缓冲 {} ms · 欠载 {} · 队列丢 {}",
        state.audio_received,
        state.audio_buffer_ms,
        state.audio_underruns,
        state.audio_dropped
    )
}

/// 「被控端」组：身份、编码器/采集方式、每秒遥测与 C2/C3/C5 增量。
#[expect(
    clippy::too_many_lines,
    reason = "一张平铺的遥测表：按字段拆函数只会让「漏了一项」更难发现"
)]
fn host_grid(ui: &mut Ui, host: Option<&HostStats>) {
    egui::Grid::new("session_stats_host")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            stat_label(ui, "平台");
            ui.label(host_field(host, |h| dash(&h.platform).to_owned()));
            ui.end_row();

            stat_label(ui, "编码器");
            ui.label(host_field(host, |h| dash(&h.encoder_backend).to_owned()));
            ui.end_row();

            stat_label(ui, "编码器名称");
            ui.label(host_field(host, |h| dash(&h.encoder_name).to_owned()));
            ui.end_row();

            stat_label(ui, "编码器适配器");
            ui.label(host_field(host, |h| dash(&h.encoder_adapter).to_owned()));
            ui.end_row();

            stat_label(ui, "编码 GPU 输入");
            ui.label(host_field(host, |h| gpu_cpu(h.gpu_encode).to_owned()));
            ui.end_row();

            stat_label(ui, "采集方式");
            ui.label(host_field(host, |h| dash(&h.capture_backend).to_owned()));
            ui.end_row();

            stat_label(ui, "采集适配器");
            ui.label(host_field(host, |h| dash(&h.capture_adapter).to_owned()));
            ui.end_row();

            stat_label(ui, "GPU 零拷贝（采集）");
            ui.label(host_field(host, |h| gpu_cpu(h.gpu_capture).to_owned()));
            ui.end_row();

            stat_label(ui, "GPU 禁用");
            ui.label(host_field(host, |h| {
                if h.gpu_disabled {
                    dash(&h.gpu_disabled_reason).to_owned()
                } else {
                    "否".to_owned()
                }
            }));
            ui.end_row();

            stat_label(ui, "原生分辨率");
            ui.label(host_field(host, |h| yes_no(h.native_resolution).to_owned()));
            ui.end_row();

            stat_label(ui, "采集耗时");
            ui.label(host_field(host, |h| format_us(h.capture_us)));
            ui.end_row();

            stat_label(ui, "编码耗时");
            ui.label(host_field(host, |h| format!("{} ms", h.encode_ms)));
            ui.end_row();

            stat_label(ui, "实测 fps");
            ui.label(host_field(host, |h| format!("{} fps", h.fps)));
            ui.end_row();

            stat_label(ui, "目标 / 实际码率");
            ui.label(host_field(host, |h| {
                format!(
                    "{} / {}",
                    format_kbps(h.bitrate_kbps),
                    format_kbps(h.sent_kbps)
                )
            }));
            ui.end_row();

            stat_label(ui, "变化面积");
            ui.label(host_field(host, |h| format_permille(h.changed_permille)));
            ui.end_row();

            stat_label(ui, "活跃档");
            ui.label(host_field(host, |h| h.activity.label().to_owned()));
            ui.end_row();

            stat_label(ui, "缩放");
            ui.label(host_field(host, |h| {
                format_permille(u32::from(h.scale_permille))
            }));
            ui.end_row();

            stat_label(ui, "补丁模式");
            ui.label(host_field(host, |h| yes_no(h.patch_mode).to_owned()));
            ui.end_row();

            stat_label(ui, "关键帧");
            ui.label(host_field(host, |h| format!("{}/s", h.keyframes)));
            ui.end_row();

            stat_label(ui, "关键帧码率");
            ui.label(host_field(host, |h| format_kbps(h.idr_kbps)));
            ui.end_row();

            stat_label(ui, "关键帧平均");
            ui.label(host_field(host, |h| {
                format_avg_keyframe(h.idr_bytes, h.keyframes)
            }));
            ui.end_row();

            stat_label(ui, "普通帧码率");
            ui.label(host_field(host, |h| format_kbps(h.p_kbps)));
            ui.end_row();

            stat_label(ui, "跳过");
            ui.label(host_field(host, |h| format!("{}/s", h.skipped)));
            ui.end_row();

            stat_label(ui, "队列深度");
            ui.label(host_field(host, |h| format!("{} 帧", h.out_queue_depth)));
            ui.end_row();

            stat_label(ui, "背压跳过");
            ui.label(host_field(host, |h| format!("{}/s", h.out_queue_skips)));
            ui.end_row();

            stat_label(ui, "编码器重建");
            ui.label(host_field(host, |h| format!("{}/s", h.encoder_rebuilds)));
            ui.end_row();

            stat_label(ui, "强制 IDR");
            ui.label(host_field(host, |h| format!("{}/s", h.forced_idrs)));
            ui.end_row();

            stat_label(ui, "采集失败");
            ui.label(host_field(host, |h| format!("{}/s", h.capture_failures)));
            ui.end_row();

            stat_label(ui, "采集回退");
            ui.label(host_field(host, |h| format!("{}/s", h.capture_fallbacks)));
            ui.end_row();

            stat_label(ui, "打开回退（WGC→DXGI）");
            ui.label(host_field(host, |h| {
                format!("{}/s", h.capture_open_fallbacks)
            }));
            ui.end_row();

            stat_label(ui, "无新帧");
            ui.label(host_field(host, |h| format!("{}/s", h.no_new_frame)));
            ui.end_row();
        });
}

/// 「画面 / 本端」组：codec、尺寸、本端解码与渲染、显示帧率、模式与显示器。
fn local_grid(ui: &mut Ui, v: &RemoteView, active: &ActiveSessionInfo, state: &RemoteUiState) {
    let display = selected_display_info(v);
    let display_count = v.displays().len();
    egui::Grid::new("session_stats_local")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            stat_label(ui, "对端");
            ui.label(dash(&active.peer_name));
            ui.end_row();

            stat_label(ui, "对端平台");
            ui.label(dash(&active.peer_platform));
            ui.end_row();

            stat_label(ui, "对端 ID");
            ui.label(dash(&active.peer_rd_id));
            ui.end_row();

            stat_label(ui, "模式");
            ui.label(mode_short(active.mode));
            ui.end_row();

            stat_label(ui, "codec");
            ui.label(codec_label(state.codec));
            ui.end_row();

            stat_label(ui, "编码尺寸");
            ui.label(opt_or_dash(
                state.encode_dims.map(|(w, h)| format!("{w}×{h}")),
            ));
            ui.end_row();

            stat_label(ui, "显示尺寸");
            ui.label(
                display
                    .as_ref()
                    .map_or(UNKNOWN.to_owned(), |(w, h, _)| format!("{w}×{h}")),
            );
            ui.end_row();

            // 画面绘制比例：1:1 时是「编码糊」还是「显示缩放糊」一眼可分（见 `view_scale`）。
            stat_label(ui, "显示缩放");
            ui.label(match v.view_scale_permille {
                0 => UNKNOWN.to_owned(),
                1000 => "100%（1:1 物理像素）".to_owned(),
                permille if permille > 1000 => {
                    format!("{:.0}%（放大，双线性会发虚）", f64::from(permille) / 10.0)
                }
                permille => format!("{:.0}%（缩小）", f64::from(permille) / 10.0),
            });
            ui.end_row();

            stat_label(ui, "显示器");
            ui.label(display.as_ref().map_or_else(
                || UNKNOWN.to_owned(),
                |(_, _, name)| format!("{} · 共 {display_count} 台", dash(name)),
            ));
            ui.end_row();

            stat_label(ui, "显示 fps / 上限");
            ui.label(format!("{} fps / {} fps", v.fps, v.frame_rate()));
            ui.end_row();

            stat_label(ui, "本端解码");
            ui.label(dash_opt(state.decode_backend.as_deref()));
            ui.end_row();

            stat_label(ui, "解码适配器");
            ui.label(dash_opt(state.decode_adapter.as_deref()));
            ui.end_row();

            stat_label(ui, "解码硬件");
            ui.label(opt_yes_no(state.decode_hardware));
            ui.end_row();

            stat_label(ui, "解码耗时");
            ui.label(opt_or_dash(state.decode_ms.map(|ms| format!("{ms} ms"))));
            ui.end_row();

            stat_label(ui, "渲染后端");
            ui.label(v.render_backend());
            ui.end_row();

            stat_label(ui, "缩放档位");
            ui.label(v.scale.label());
            ui.end_row();
        });
}

/// 角标行标签（弱化小字）。
fn stat_label(ui: &mut Ui, text: &str) {
    let p = theme::palette(ui.style());
    ui.label(RichText::new(text).small().color(p.text_weak));
}

/// 未知项占位符。
const UNKNOWN: &str = "—";

/// 空串 / 缺值都显示 `—`。
fn dash(text: &str) -> &str {
    if text.is_empty() {
        UNKNOWN
    } else {
        text
    }
}

/// `Option<&str>` 版的 [`dash`]（`None` 与空串一律 `—`）。
fn dash_opt(text: Option<&str>) -> &str {
    text.map_or(UNKNOWN, dash)
}

/// `Option<T>` 转字符串；`None` → `—`。
fn opt_or_dash<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| UNKNOWN.to_owned(), |v| v.to_string())
}

/// 端到端延迟的单元格。
///
/// `SessionStats::Decode.e2e_ms` 用 `0` 表示「时钟偏差未估计 / 对端没带时间戳」（见
/// `crates/remote/src/stats.rs`），所以这里把 `0` 一并当作未知——否则会话开头那一秒会显示
/// 一个并不可信的「0 ms」。
fn e2e_cell(ms: Option<u32>) -> String {
    match ms {
        Some(ms) if ms > 0 => format!("{ms} ms"),
        _ => UNKNOWN.to_owned(),
    }
}

/// 对被控端遥测取一个字段；`None`（尚未上报）→ `—`。
fn host_field(host: Option<&HostStats>, f: impl FnOnce(&HostStats) -> String) -> String {
    host.map_or_else(|| UNKNOWN.to_owned(), f)
}

/// 对可靠层快照取一个字段；直连/中继路径没有可靠层（`None`）→ `—`。
fn transport_field(
    ts: Option<&TransportStats>,
    f: impl FnOnce(&TransportStats) -> String,
) -> String {
    ts.map_or_else(|| UNKNOWN.to_owned(), f)
}

/// 延迟单元格：已知时按档位着色，未知显示 `—`。
fn latency_cell(ui: &mut Ui, latency: Option<u32>) {
    match latency {
        Some(ms) => {
            ui.colored_label(latency_color(ms), format!("{ms} ms"));
        }
        None => {
            ui.label(UNKNOWN);
        }
    }
}

/// 编码器 / 采集的 GPU/CPU 标记。
fn gpu_cpu(gpu: bool) -> &'static str {
    if gpu {
        "GPU"
    } else {
        "CPU"
    }
}

/// 是 / 否。
fn yes_no(value: bool) -> &'static str {
    if value {
        "是"
    } else {
        "否"
    }
}

/// 是 / 否 / 未知。
fn opt_yes_no(value: Option<bool>) -> &'static str {
    match value {
        Some(v) => yes_no(v),
        None => UNKNOWN,
    }
}

/// codec 的展示名（A1）。
fn codec_label(codec: Option<CodecId>) -> &'static str {
    match codec {
        Some(CodecId::H264) => "H.264 4:2:0",
        Some(CodecId::H265) => "H.265 4:2:0",
        Some(CodecId::Vp9) => "VP9 4:4:4",
        None => UNKNOWN,
    }
}

/// 会话模式短标签（详细档一行放得下；完整说明见 `ui::remote::mode_label`）。
fn mode_short(mode: RemoteMode) -> &'static str {
    match mode {
        RemoteMode::Control => "完全控制",
        RemoteMode::ViewOnly => "仅观看",
        RemoteMode::FileTransfer => "仅文件传输",
    }
}

/// 微秒转毫秒（一位小数）：GPU 采集常在 1 ms 以内，整数毫秒会恒显示 0。
fn format_us(us: u32) -> String {
    format!("{:.1} ms", f64::from(us) / 1000.0)
}

/// 千分比转百分数（一位小数）。
fn format_permille(permille: u32) -> String {
    format!("{:.1}%", f64::from(permille) / 10.0)
}

/// 关键帧平均大小（KB）：`idr_bytes / keyframes`，窗口内没有关键帧时 `—`。
#[expect(
    clippy::cast_precision_loss,
    reason = "字节数只用于展示平均 KB，f64 精度足够"
)]
fn format_avg_keyframe(idr_bytes: u64, keyframes: u32) -> String {
    if keyframes == 0 {
        UNKNOWN.to_owned()
    } else {
        format!("{:.1} KB", idr_bytes as f64 / 1024.0 / f64::from(keyframes))
    }
}

/// 关键帧请求的**原因累计**摘要（C3）：只列非零原因，全零显示 `—`。
///
/// 计数来自 [`RemoteUiState::keyframe_requests`]，由管理器按
/// [`KeyframeReason::index`] 累计；这里用 [`KeyframeReason::from_index`] 反查标签，
/// 不依赖数组顺序。
fn keyframe_reason_summary(counts: &[u32; KeyframeReason::COUNT]) -> String {
    let mut parts = Vec::new();
    for (index, count) in counts.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        if let Some(reason) = KeyframeReason::from_index(index) {
            parts.push(format!("{} {}", reason.label(), count));
        }
    }
    if parts.is_empty() {
        UNKNOWN.to_owned()
    } else {
        parts.join(" · ")
    }
}

/// 会话时长 `分:秒`。
fn format_duration(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// 当前所选显示器的 `(宽, 高, 名称)`；尚未下发显示器列表时为 `None`。
fn selected_display_info(v: &RemoteView) -> Option<(u32, u32, String)> {
    let selected = v.selected_display();
    v.displays()
        .into_iter()
        .find(|d| Some(d.id) == selected)
        .map(|d| (d.width, d.height, d.name))
}

/// 「采集 → 上屏」端到端延迟（毫秒）。
///
/// 采集时刻来自被控端单调时钟，必须用 `clock_offset_us` 换算到本端时钟；缺时间戳或缺
/// 时钟偏移时返回 `None`。差值饱和到 0：时钟抖动可能让本地算出的「未来」时刻略早于采集，
/// 负延迟没有意义。
fn screen_e2e_ms(v: &RemoteView, state: &RemoteUiState) -> Option<u32> {
    let capture_ts_us = v.last_capture_ts_us()?;
    let offset_us = state.clock_offset_us?;
    Some(e2e_ms_from(
        rust_tunnel_remote::clock::now_us(),
        offset_us,
        capture_ts_us,
    ))
}

/// 「采集 → 现在」的毫秒数（纯算术，便于单测）。
///
/// `offset_us` 是「对端时钟 − 本端时钟」，故 `now + offset - capture` 才是同一个时钟域里的差。
/// 负值（时钟抖动 / 时间戳略超前）饱和到 0。
fn e2e_ms_from(now_us: u64, offset_us: i64, capture_ts_us: u64) -> u32 {
    let elapsed_us =
        (i128::from(now_us) + i128::from(offset_us) - i128::from(capture_ts_us)).max(0);
    u32::try_from(elapsed_us / 1000).unwrap_or(u32::MAX)
}

/// 右侧面板内容：剪贴板 + 文件传输（整体从旧「远程」Tab 迁来，逻辑不变）。
fn side_contents(
    ui: &mut Ui,
    v: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    state: &RemoteUiState,
) {
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
#[expect(
    clippy::cast_precision_loss,
    reason = "传输字节数有界，f32 进度条精度足够"
)]
fn show_files(
    ui: &mut Ui,
    v: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    state: &RemoteUiState,
) {
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
                ui.label(
                    RichText::new(format!("当前目录：{shown_dir}"))
                        .weak()
                        .small(),
                );
                if ui.button("上级").clicked() {
                    manager.list_remote_dir(&parent_virtual(&state.remote_dir));
                }
                if ui.button("刷新").clicked() {
                    manager.list_remote_dir(&state.remote_dir);
                }
            });

            if state.remote_entries.is_empty() {
                ui.label(
                    RichText::new("（空目录或尚未加载，点「刷新」）")
                        .weak()
                        .small(),
                );
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
                    ui.label(format!(
                        "{}（{}）",
                        transfer.name,
                        transfer.direction.label()
                    ));
                    let fraction = transfer.done as f32 / transfer.total.max(1) as f32;
                    ui.add(egui::ProgressBar::new(fraction).desired_width(120.0));
                    if ui.button("取消").clicked() {
                        manager.cancel_transfer(transfer.id);
                    }
                });
            }

            if let Some(dir) = manager.share_dir() {
                ui.label(
                    RichText::new(format!("本机对外共享：{}", dir.display()))
                        .weak()
                        .small(),
                );
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
        TransportPath::Punch => "P2P 打洞",
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
    //! 纯逻辑单测：工具条可见性状态机与数据面路径标签。真实的 egui 布局（浮层锚点、弹层模态层）
    //! 无法在无头环境验证，只能由这些单测锁住判定规则。

    use super::*;

    /// 像素对齐：起点与尺寸都落在整物理像素上（否则双线性采样相位落在纹素之间，字发虚）。
    #[test]
    fn snap_to_physical_rounds_both_origin_and_size() {
        // 150% 屏幕：1 物理像素 = 2/3 逻辑点。
        let rect = egui::Rect::from_min_size(egui::pos2(10.3, 4.9), egui::vec2(801.2, 601.7));
        let snapped = snap_to_physical(rect, 1.5);
        for value in [
            snapped.min.x * 1.5,
            snapped.min.y * 1.5,
            snapped.width() * 1.5,
            snapped.height() * 1.5,
        ] {
            assert!(
                (value - value.round()).abs() < 1e-3,
                "应落在整物理像素上：{value}"
            );
        }
        // 非法 DPI 按 1.0 处理，不产生 NaN。
        let fallback = snap_to_physical(rect, f32::NAN);
        assert!(fallback.min.x.is_finite() && fallback.width().is_finite());
    }

    /// 绘制比例读数：1:1 物理像素恰好是 1000‰，放大/缩小分别落在两侧。
    #[test]
    fn view_scale_permille_reports_physical_ratio() {
        // 1920 宽的远端画成 1920 物理像素（1280 逻辑点 @150%）= 1:1。
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1280.0, 720.0));
        assert_eq!(view_scale_permille(rect, (1920, 1080), 1.5), 1000);
        // 画成一半大小。
        let half = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(640.0, 360.0));
        assert_eq!(view_scale_permille(half, (1920, 1080), 1.5), 500);
        // 未知尺寸 → 0（UI 显示「—」）。
        assert_eq!(view_scale_permille(rect, (0, 0), 1.5), 0);
    }

    /// 三条路径的标签**必须互不相同**。
    ///
    /// `transport_label` 的 match 已经是穷尽的（加变体会编译失败），但穷尽**管不住写错值**：
    /// 把 `Punch` 写成「中继」照样编译通过，而它恰好是用户唯一能看见的「字节有没有过服务器」
    /// 的信号——中继与打洞的差别正是本项目要暴露给用户的那一件事。这条锁住「别把两条路合并
    /// 成一个标签」。
    #[test]
    fn transport_labels_are_distinct() {
        let labels = [
            transport_label(TransportPath::Punch),
            transport_label(TransportPath::Direct),
            transport_label(TransportPath::Relay),
        ];
        for (i, a) in labels.iter().enumerate() {
            assert!(!a.is_empty(), "标签不得为空：{labels:?}");
            for b in &labels[i + 1..] {
                assert_ne!(a, b, "不同路径不得共用同一个标签：{labels:?}");
            }
        }
    }

    /// 造一条位于窗口上缘、指定高度的工具条矩形（满宽）。
    fn toolbar_rect(height: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, height))
    }

    /// 造一份决策输入（默认指针未移动、未按下、无设置弹层）。
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
            modal_open: false,
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

    /// 可见的延迟胶囊也算热区（自动隐藏后指针停在胶囊上，y 超出 6px 的上缘窄带也要重新展开）。
    /// 隐藏态的实测矩形就是胶囊（见 `render`）。
    #[test]
    fn pointer_on_latency_pill_recalls_hidden_toolbar() {
        let mut i = inputs(
            true,
            false,
            Some(egui::pos2(500.0, 12.0)),
            widgets::PILL_HEIGHT,
            Duration::from_secs(10),
        );
        i.toolbar_rect = egui::Rect::from_min_size(
            egui::pos2(460.0, 0.0),
            egui::vec2(80.0, widgets::PILL_HEIGHT),
        );
        let d = decide_toolbar_visibility(i);
        assert!(d.visible, "指针停在可见的延迟胶囊上应展开面板");
        assert!(d.reset_activity);
    }

    #[test]
    fn modal_open_keeps_toolbar_visible() {
        // 设置弹层打开时工具条恒可见（它是弹层的召回入口与档位读数），并重置空闲计时——
        // 关掉弹层后工具条还该完整停留一个 `TOOLBAR_IDLE` 窗口，而不是立刻消失。
        for was_visible in [true, false] {
            for idle in [Duration::ZERO, Duration::from_secs(60)] {
                let mut i = inputs(
                    true,
                    was_visible,
                    Some(egui::pos2(500.0, 400.0)),
                    44.0,
                    idle,
                );
                i.modal_open = true;
                let d = decide_toolbar_visibility(i);
                assert!(
                    d.visible,
                    "弹层打开时工具条必须可见（was_visible={was_visible}）"
                );
                assert!(d.reset_activity, "弹层打开时应重置空闲计时");
            }
        }
    }

    /// 端到端换算：两端时钟经 offset 对齐后求差；负值饱和到 0（时钟抖动不该显示负延迟）。
    #[test]
    fn e2e_ms_aligns_clocks_and_saturates_to_zero() {
        // 采集于对端 9.0s，本端此刻 10.0s，offset=0 → 1000 ms。
        assert_eq!(e2e_ms_from(10_000_000, 0, 9_000_000), 1000);
        // 对端时钟快 2s（offset=+2s）：采集时刻 11.0s 与本端 10.0s 是同一瞬间 → 约 1s。
        assert_eq!(e2e_ms_from(10_000_000, 2_000_000, 11_000_000), 1000);
        // 采集时间戳晚于「现在 + offset」→ 0，而不是下溢成一个巨大的数。
        assert_eq!(e2e_ms_from(10_000_000, 0, 12_000_000), 0);
        // 极值不 panic（饱和到 u32::MAX）。
        assert_eq!(e2e_ms_from(u64::MAX, i64::MAX, 0), u32::MAX);
        assert_eq!(e2e_ms_from(0, i64::MIN, u64::MAX), 0);
    }

    /// 角标里的固定文案：未知项必须是占位符，不能 panic、不能显示空串。
    #[test]
    fn stats_labels_cover_unknown_values() {
        assert_eq!(dash(""), UNKNOWN);
        assert_eq!(dash("xcap"), "xcap");
        assert_eq!(dash_opt(None), UNKNOWN);
        assert_eq!(dash_opt(Some("")), UNKNOWN);
        assert_eq!(codec_label(None), UNKNOWN);
        assert_eq!(codec_label(Some(CodecId::H264)), "H.264 4:2:0");
        assert_eq!(codec_label(Some(CodecId::H265)), "H.265 4:2:0");
        assert_eq!(codec_label(Some(CodecId::Vp9)), "VP9 4:4:4");
        assert_eq!(opt_yes_no(None), UNKNOWN);
        assert_eq!(opt_yes_no(Some(false)), "否");
        // `e2e_ms == 0` 是「尚未估计」的哨兵，不能显示成一个真实的 0 ms。
        assert_eq!(e2e_cell(None), UNKNOWN);
        assert_eq!(e2e_cell(Some(0)), UNKNOWN);
        assert_eq!(e2e_cell(Some(42)), "42 ms");
        assert_eq!(format_us(500), "0.5 ms");
        assert_eq!(format_permille(125), "12.5%");
        assert_eq!(format_avg_keyframe(0, 0), UNKNOWN);
        assert_eq!(format_duration(125), "02:05");
    }
}
