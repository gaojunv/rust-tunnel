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

use super::{theme, widgets};
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
const TOOLBAR_FALLBACK_HEIGHT: f32 = 48.0;

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

    // 工具条可见时给一个定时重绘，否则静止画面下它永远不会淡出（画面静止时引擎不唤醒重绘）。
    // 收起态同样需要：那枚延迟胶囊是唯一读数，心跳更新时视图未必会被重绘唤醒。
    if v.toolbar_visible || v.toolbar_collapsed {
        ctx.request_repaint_after(Duration::from_millis(400));
    }
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
/// 热区判据分两态：**可见时**用工具条浮层上一帧的实测矩形（[`RemoteView::toolbar_rect`]）；
/// **隐藏时**只认上缘窄带 [`TOOLBAR_HOT`]（移到窗口上缘召回）。
///
/// # 为什么用上一帧的矩形
///
/// egui 是立即模式：本帧渲染工具条时还不知道本帧的矩形（工具条自己就是被渲染对象）。用上一帧
/// 实测的矩形是标准做法，一帧延迟对「指针是否停在按钮上」无感。首帧尚未测得时用
/// [`toolbar_fallback_rect`] 兜底。
fn update_toolbar_visibility(ctx: &egui::Context, v: &mut RemoteView, autohide: bool) {
    if v.toolbar_collapsed {
        // 收起态只画一枚延迟胶囊：它已经小到不挡画面，**不参与**自动隐藏——淡出之后用户就看不到
        // 本来就只想看的那条延迟了。展开态可见性留给 [`RemoteView::expand_toolbar`] 重新起算。
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

/// 渲染窗口主体。`embedded` 分支（集成不支持多窗口时的兜底）退化为一个 egui 窗口承载同样内容。
///
/// # 浮层，而不是面板（缺陷「隐现挪动/缩放画面」的根除）
///
/// 工具条、重连提示、统计角标一律是 `egui::Area`/`egui::Window` 浮层，**不参与布局**：画面矩形
/// 只由窗口尺寸与缩放档位决定。于是工具条隐现既不改变 `Fit` 档的画面尺寸，也不上移固定档的画面
/// 原点——过去 `TopBottomPanel` 一注册就从中央区域扣掉自身高度，隐藏时占位为 0，画面会跳动；
/// 那套「缓存高度常驻占位」的机制随浮层一并删除了。**别再改回面板**。
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

    if v.toolbar_collapsed {
        // 收起态：**只有**一枚延迟胶囊（点它展开）。刻意不套 `toolbar_frame`——一层底板 + 内边距
        // 会把这块放大到原来的三分之一，而「不挡画面」正是收起的全部意义。
        let bar = egui::Area::new(egui::Id::new("session_toolbar_collapsed"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 8.0))
            .show(ctx, |ui| collapsed_pill(ui, v, state));
        v.toolbar_rect = Some(bar.response.rect);
    } else if v.toolbar_visible {
        // 浮在画面上缘居中：圆角半透明卡片，只留「状态 + 质量 + 当前档位 + 六个动作」。
        let bar = egui::Area::new(egui::Id::new("session_toolbar"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 10.0))
            .show(ctx, |ui| {
                theme::toolbar_frame(ui.style()).show(ui, |ui| {
                    toolbar_contents(ui, v, manager, state, active);
                });
            });
        v.toolbar_rect = Some(bar.response.rect);
    } else {
        // 隐藏时只在上缘留一条 28×3 的细提示（不可交互）：告诉用户工具条还在、上缘可召回，
        // 但 3px 的高度不会挡住画面，也不会吃掉点击。
        let p = theme::palette(&ctx.style());
        egui::Area::new(egui::Id::new("session_toolbar_hint"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 4.0))
            .interactable(false)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 3.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(2), p.stroke_strong);
            });
    }

    // 重连提示浮在工具条下方（同为浮层：它出现/消失也不该挪动画面）。
    if v.reconnect_note.is_some() {
        egui::Area::new(egui::Id::new("session_reconnect"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 64.0))
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
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
/// 只放**一眼要看**的东西与六个动作；所有「进去改一次」的设置都搬到 [`settings_modal`]——
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
                ui.label(RichText::new("延迟 —").weak().small());
            }
        }
        if let Some(path) = state.transport {
            ui.label(RichText::new(transport_label(path)).weak().small());
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

        // 动作区**从左到右**排：`egui::Area` 按内容自适应宽度，`right_to_left` 会把第一个控件
        // 推到「可用宽度」的右缘（Area 的可用宽度是整个视口），整张卡片会被撑到全屏宽。
        // 断开放在最后：破坏性操作不该是手指落下的第一站。
        ui.add_space(4.0);
        // 收起放最前：误触的后果只是工具条缩成一枚延迟胶囊，点一下就能回来；「断开」仍留在末尾，
        // 破坏性操作不该和常用按钮做邻居。
        if widgets::toolbar_button(ui, "收起", "收起到只显示延迟（点延迟胶囊可展开）").clicked() {
            v.collapse_toolbar();
        }
        if widgets::toolbar_button(ui, "设置", "画质 / 帧率 / 显示器 / 缩放 / 窗口尺寸 / 交互")
            .clicked()
        {
            v.settings_open = true;
        }
        if ui
            .selectable_label(v.side_panel_open, "文件")
            .on_hover_text("文件传输与剪贴板")
            .clicked()
        {
            v.side_panel_open = !v.side_panel_open;
        }
        if ui
            .selectable_label(v.fullscreen, "全屏")
            .on_hover_text("切换全屏（F11；全屏时 Esc 退出）")
            .clicked()
        {
            v.fullscreen = !v.fullscreen;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(v.fullscreen));
        }
        if widgets::toolbar_button(ui, "最小化", "关闭窗口但保留会话；在「远程」Tab 可召回")
            .clicked()
        {
            // 置 false 后父窗口就不再注册这个 viewport，回调从此不再跑——必须**当场**补发一次
            // 输入抬起，否则用户此刻按住的键会在远端一直按着（会话仍在，没人替它收尾）。
            v.release_input(manager);
            v.window_open = false;
        }
        if widgets::danger_button(ui, "断开", "结束本次远程会话").clicked() {
            manager.abort_active();
        }
    });
}

/// 收起态的唯一内容：一枚延迟胶囊，点它展开工具条。
///
/// 「只显示延迟」是用户的原话，所以这里**不**再放状态点、路径、档位摘要——多一个字符就多挡一点
/// 画面。延迟未知时显示 `延迟 —`，形状与有值时一致（收起的工具条不会因为一次心跳丢包而跳大小）。
fn collapsed_pill(ui: &mut Ui, v: &mut RemoteView, state: &RemoteUiState) {
    let p = theme::palette(ui.style());
    let (text, color) = state.latency_ms.map_or_else(
        || ("延迟 —".to_owned(), p.text_weak),
        |ms| (format!("{ms} ms"), latency_color(ms)),
    );
    if widgets::pill_button(ui, &text, color, "点击展开工具条").clicked() {
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
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("关闭").clicked() {
                close = true;
            }
            if ui
                .button("测延迟")
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

/// 设置弹层的三个小节（画面 / 交互 / 界面）。
///
/// 从 [`settings_modal`] 拆出来只为控制函数长度；内容与「逐项改一次」的语义完全对应
/// （各控件在改动时才下发，不做每帧轮询）。
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
            // 固定档忽略可用区域、溢出交给滚动区（见 `ViewScale`）。
            egui::ComboBox::from_id_salt("session_scale")
                .selected_text(v.scale.label())
                .show_ui(ui, |ui| {
                    for scale in ViewScale::ALL {
                        ui.selectable_value(&mut v.scale, scale, scale.label());
                    }
                });
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
                v.inner_size = v.desired_inner(monitor);
                v.size_initialized = true;
            }
        });
    });

    widgets::section(ui, "交互", |ui| {
        if ui
            .checkbox(&mut v.clipboard_sync_enabled, "剪贴板自动同步")
            .changed()
            && !v.clipboard_sync_enabled
        {
            // 关同步就丢弃去重基线：下次开启会以当时的剪贴板重新开始，而不是补发
            // 关闭期间积压的变化。
            v.clipboard_sync = None;
        }
        ui.checkbox(
            &mut v.exclusive_keyboard,
            "独占键盘（Ctrl/Cmd+Alt+Shift+K 退出）",
        );
    });

    widgets::section(ui, "界面", |ui| {
        // 收起 = 画面之上只留一枚延迟胶囊。它是**持久化**的显式选择（工具条上的「收起」也改它），
        // 这里是同一状态的第二个入口：收起后工具条虽然不见了，设置里仍看得到、也改得回来。
        // 走一对方法而不是裸改字段：展开时还要置可见并重置自动隐藏计时，否则开启自动隐藏的
        // 用户一取消勾选，工具条会停在「隐藏」状态，非得再挪一下鼠标才出来。
        if ui
            .checkbox(&mut v.toolbar_collapsed, "收起工具条（只显示延迟）")
            .changed()
        {
            if v.toolbar_collapsed {
                v.collapse_toolbar();
            } else {
                v.expand_toolbar();
            }
        }
        // 自动隐藏只作用于展开态（收起态本来就不参与），因此在收起期间灰掉，免得用户以为
        // 两个开关在打架。
        // 自动隐藏开关：这是**唯一**能让用户关掉「工具条偶尔消失」的入口。改动写进
        // 运行时镜像，由父窗口的 `sync_display_prefs` 回写 gui.toml。
        ui.add_enabled(
            !v.toolbar_collapsed,
            egui::Checkbox::new(
                &mut v.toolbar_autohide,
                "工具条自动隐藏（移到窗口上缘召回）",
            ),
        );
        ui.checkbox(&mut v.stats_overlay, "显示统计角标");
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

/// 画面区域：按缩放档位决定绘制尺寸。`Fit` 等比铺满可用区域；固定档位用滚动区承载溢出。
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
    let want = display_size(v.scale, avail, (width, height));
    if v.scale == ViewScale::Fit {
        paint_image(ui, v, manager, active, &paint, want);
    } else {
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                paint_image(ui, v, manager, active, &paint, want);
            });
    }
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
    size: egui::Vec2,
) {
    // 仍然要占位分配这个区域（布局与可交互性不能变），但输入捕获不再依赖 `response`
    // （见 `RemoteView::capture_input`：`hovered()` 会被 egui 的交互簿记影响）。
    let (rect, _response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
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
                },
            ));
        }
    }
    // 只有「完全控制」模式捕获输入：仅观看 / 仅文件传输不该注入键鼠。
    if active.mode == RemoteMode::Control {
        // 坐标按所选显示器的尺寸映射（由 `capture_input` 内部取），与纹理是否缩放无关。
        v.capture_input(ui, manager, rect);
    }
}

/// 统计角标（右上角悬浮）：延迟 / 路径 / 分辨率 / 帧率 / 会话时长。
fn stats_overlay(
    ctx: &egui::Context,
    v: &RemoteView,
    active: &ActiveSessionInfo,
    state: &RemoteUiState,
) {
    let (width, height) = v.frame_dims();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(active.started_at_ms, |d| {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
        });
    let secs = now_ms.saturating_sub(active.started_at_ms) / 1000;
    egui::Window::new("会话统计")
        .id(egui::Id::new("session_stats_overlay"))
        .title_bar(false)
        .resizable(false)
        .anchor(egui::Align2::RIGHT_TOP, [-10.0, 10.0])
        .frame(theme::card_frame(&ctx.style()))
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
                    ui.label(
                        state
                            .decode_ms
                            .map_or("—".to_owned(), |ms| format!("{ms} ms")),
                    );
                    ui.end_row();
                    ui.label(RichText::new("收码率").weak().small());
                    ui.label(state.recv_kbps.map_or("—".to_owned(), format_kbps));
                    ui.end_row();
                    // 收包/丢包：用来区分「对端没出帧」与「本端解码队列在丢帧」——后者会触发
                    // 关键帧请求，表现为画面卡顿而码率不低。
                    ui.label(RichText::new("收包").weak().small());
                    ui.label(
                        state
                            .decode_packets
                            .map_or("—".to_owned(), |n| format!("{n}/s")),
                    );
                    ui.end_row();
                    ui.label(RichText::new("丢包").weak().small());
                    ui.label(
                        state
                            .decode_dropped
                            .map_or("—".to_owned(), |n| format!("{n}/s")),
                    );
                    ui.end_row();
                    // 补丁批数：协议 v6 的脏矩形增量通道。补丁模式下全帧几乎消失，这一项
                    // 代替「收包」说明画面仍在更新（被控端/VP9 会话恒为 —）。
                    ui.label(RichText::new("补丁").weak().small());
                    ui.label(
                        state
                            .decode_patches
                            .map_or("—".to_owned(), |n| format!("{n}/s")),
                    );
                    ui.end_row();
                    // 关键帧请求速率：持续 >0 说明解码链在反复断裂（丢包/队列溢出/解码失败）。
                    if v.keyframe_rate > 0 {
                        ui.label(RichText::new("关键帧请求").weak().small());
                        ui.label(format!("{}/s", v.keyframe_rate));
                        ui.end_row();
                    }
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
}
