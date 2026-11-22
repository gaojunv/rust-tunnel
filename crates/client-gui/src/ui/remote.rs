//! 「远程」Tab：本机被控与接入策略、已信任对端、会话摘要、在线客户端列表。
//!
//! 画面 / 输入捕获 / 剪贴板 / 文件面板已整体迁到独立会话窗口（[`crate::ui::session_window`]）；本
//! Tab 只留下**摘要卡**：会话状态、延迟、数据面路径、以及「显示窗口 / 中断会话」两个动作。会话窗
//! 口可被最小化（会话继续保留），故这里必须留一个把它召回入口。
//!
//! 信息架构（Fluent，页头 + 顶部两个 Tab）：
//! - **页头**：标题 / 副标题 / 控制通道状态徽标（固定，不随内容滚动）；
//! - **Tab 行**：「本机被控」/「连接到其他设备」二选一（固定，不随内容滚动）；
//! - **本机被控**：允许被控、接入确认策略、自动锁屏、本机元信息、已信任对端、被控方会话摘要；
//! - **连接到其他设备**：重连提示、控制方会话摘要、在线客户端。
//!
//! 为什么用 Tab 而不是继续上下堆叠：这两大区的**语义完全独立**（一个是「别人能连我」，一个是
//! 「我去连别人」），彼此之间没有阅读顺序上的依赖；而默认窗口只有 760 宽、高度也有限，两个大区
//! 叠在一起时第二区几乎整块落在可视区之外（本页此前还没有滚动区）。拆成 Tab 后每个 Tab 只
//! 呈现一条完整任务线，窗口再矮也只需滚动一个区。

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use egui::{RichText, Ui};
use rust_tunnel_common::protocol::{RemoteMode, RemotePeerInfo};
use rust_tunnel_remote::{
    ActiveSessionInfo, ApprovalPolicy, RemoteManager, RemoteUiState, SessionRole, SessionState,
};

use crate::app::AppState;
use crate::remote::{self, PermissionKind, ReconnectTarget, RemoteView};
use crate::ui::icons::{self, Icon};
use crate::ui::session_window::{format_kbps, latency_color, transport_label, viewport_id};
use crate::ui::theme::palette;
use crate::ui::widgets::{self, ButtonKind, Tone};

/// 会话状态中文标签。
pub fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Negotiating => "协商中",
        SessionState::Connecting => "建立连接中",
        SessionState::Active => "已连接",
        SessionState::Ended => "已结束",
    }
}

/// 会话状态对应的语义色调（徽标用）。
fn state_tone(state: SessionState) -> Tone {
    match state {
        SessionState::Active => Tone::Ok,
        SessionState::Connecting | SessionState::Negotiating => Tone::Warn,
        SessionState::Ended => Tone::Neutral,
    }
}

/// 取共享视图的锁（中毒时取回内部值，一个视图锁中毒不该拖垮整个 UI）。
fn lock(view: &Arc<Mutex<RemoteView>>) -> MutexGuard<'_, RemoteView> {
    view.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 「远程」页顶部的两个 Tab。
///
/// 两个大区语义独立（「别人能连我」vs「我去连别人」），默认窗口放不下并排展示，故用 Tab 二选一。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RemoteTab {
    /// 本机被控：被控开关、接入确认、自动锁屏、已信任对端、被控方会话摘要。
    #[default]
    LocalControl,
    /// 连接到其他设备：重连提示、控制方会话摘要、在线客户端列表。
    OtherDevices,
}

/// 「远程」页的跨帧 UI 状态（目前只有选中的 Tab）。
///
/// 由 `GuiApp` 持有并传入 [`show`]：Tab 选择必须在帧之间保持，否则每次重绘都会跳回第一个 Tab。
#[derive(Default)]
pub struct RemotePage {
    /// 当前选中的 Tab（默认「本机被控」）。
    pub tab: RemoteTab,
}

/// 「远程」Tab 的用户操作结果，由 `GuiApp` 同步到引擎与偏好文件。
#[derive(Default)]
pub struct RemoteTabActions {
    /// 用户切换了「允许被远程控制」开关（`Some` 表示有变化）。
    pub allow_remote: Option<bool>,
    /// 用户切换了接入确认策略（`Some` 表示有变化）。
    pub approval_policy: Option<ApprovalPolicy>,
    /// 用户切换了「会话结束后自动锁定本机屏幕」（`Some` 表示有变化）。
    pub auto_lock_on_disconnect: Option<bool>,
}

/// 页头右侧的控制通道状态徽标。
///
/// 三种状态互斥且按「先看连接、再看能力」判定：控制通道没连上时，服务端能力位是陈旧的，
/// 说「未启用远程控制」会误导用户（他看到的其实是断线）。
fn header_badge(ui: &mut Ui, state: &RemoteUiState) {
    if !state.connected {
        widgets::badge(ui, "控制通道未连接", Tone::Neutral, Some(Icon::Pulse));
    } else if !state.capability {
        widgets::badge(ui, "服务端未启用远程控制", Tone::Warn, Some(Icon::Warning));
    } else {
        widgets::badge(ui, "控制通道已连接", Tone::Ok, Some(Icon::Check));
    }
}

/// 渲染「远程」页。返回本帧的用户操作，由调用方（`GuiApp`）同步到引擎、托盘与偏好文件。
///
/// `page` 携带跨帧的 UI 状态（当前 Tab）；页头与 Tab 行留在滚动区外固定，只有 Tab 内容滚动。
pub fn show(
    ui: &mut Ui,
    view: &Arc<Mutex<RemoteView>>,
    manager: Option<&Arc<RemoteManager>>,
    page: &mut RemotePage,
    _app_state: &Arc<AppState>,
) -> RemoteTabActions {
    let Some(manager) = manager else {
        widgets::page_header(
            ui,
            "远程",
            "本机被控、接入策略与在线设备",
            |ui| {
                widgets::badge(ui, "控制通道未连接", Tone::Neutral, Some(Icon::Pulse));
            },
        );
        widgets::empty_state(
            ui,
            Icon::Monitor,
            "远程控制引擎尚未就绪",
            "等待客户端配置 / 连接…",
        );
        return RemoteTabActions::default();
    };
    let state = manager.snapshot();

    widgets::page_header(
        ui,
        "远程",
        "本机被控、接入策略与在线设备",
        |ui| {
            header_badge(ui, &state);
        },
    );

    // Tab 行：直接左对齐放在页头下方（**不要**用裸 `with_layout` 做右对齐——它会吃掉父级剩余
    // 高度，把下面的滚动区压成零高）。
    if let Some(tab) = ui
        .horizontal(|ui| {
            widgets::segmented(
                ui,
                page.tab,
                &[
                    (RemoteTab::LocalControl, "本机被控"),
                    (RemoteTab::OtherDevices, "连接到其他设备"),
                ],
            )
        })
        .inner
    {
        page.tab = tab;
    }
    ui.add_space(10.0);

    // 每个 Tab 各自记住滚动位置：切到另一条任务线时不会停在上一个 Tab 的半截位置。
    let mut actions = RemoteTabActions::default();
    egui::ScrollArea::vertical()
        .id_salt(("remote_page", matches!(page.tab, RemoteTab::OtherDevices)))
        .auto_shrink([false, false])
        .show(ui, |ui| match page.tab {
            RemoteTab::LocalControl => {
                show_permissions(ui);
                actions = show_local_control(ui, &state);
                show_trusted_peers(ui, &state, manager);
                // 被控方的会话摘要只在这个 Tab 出现：它属于「别人正在连我」这条任务线。
                show_session(ui, view, manager, &state, SessionRole::Controlled);
            }
            RemoteTab::OtherDevices => {
                show_reconnect(ui, view, manager);
                // 控制中的会话摘要并进「在线客户端」卡片，直接显示在列表上方（同一张卡，
                // 不再单独占一张卡片）；被控方的摘要仍留在「本机被控」Tab。
                show_peers(ui, &state, manager, view);
            }
        });
    actions
}

/// 平台权限提示（缺失时给出「去授权」按钮，避免被控时静默黑屏）。
fn show_permissions(ui: &mut Ui) {
    let perms = remote::permissions();
    if !perms.supported || perms.all_granted() {
        return;
    }
    widgets::info_bar_custom(ui, Tone::Warn, |ui| {
        let p = palette(ui.style());
        ui.label(
            RichText::new("远程控制权限不足")
                .font(icons::heading_font(13.5))
                .color(p.text),
        );
        ui.add_space(2.0);
        if !perms.screen_capture {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("缺少「屏幕录制」权限，被控时无法采集画面。")
                        .size(12.0)
                        .color(p.text_weak),
                );
                if widgets::button(ui, "请求授权", None, ButtonKind::Secondary).clicked() {
                    remote::request_permission(PermissionKind::ScreenCapture);
                }
                if widgets::button(ui, "打开系统设置", None, ButtonKind::Secondary).clicked() {
                    remote::open_permission_settings(PermissionKind::ScreenCapture);
                }
            });
        }
        if !perms.accessibility {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("缺少「辅助功能」权限，被控时无法注入鼠标键盘。")
                        .size(12.0)
                        .color(p.text_weak),
                );
                if widgets::button(ui, "请求授权", None, ButtonKind::Secondary).clicked() {
                    remote::request_permission(PermissionKind::Accessibility);
                }
                if widgets::button(ui, "打开系统设置", None, ButtonKind::Secondary).clicked() {
                    remote::open_permission_settings(PermissionKind::Accessibility);
                }
            });
        }
        ui.label(
            RichText::new(
                "点击「请求授权」后系统会弹授权框；若之前拒绝过，请在系统设置里打开开关。\
                 授权后需重启本应用方可生效。",
            )
            .size(12.0)
            .color(p.text_weak),
        );
    });
}

/// 「本机被控」卡片：被控开关、接入确认策略、自动锁屏、本机元信息。
fn show_local_control(ui: &mut Ui, state: &RemoteUiState) -> RemoteTabActions {
    let mut actions = RemoteTabActions::default();
    widgets::card(ui, |ui| {
        // 被控总开关：整行可点。
        let mut enabled = state.enabled;
        if widgets::switch_row(
            ui,
            "允许被远程控制",
            Some("开启后本机屏幕与输入可被持有效 token 的对端接管"),
            &mut enabled,
        ) {
            actions.allow_remote = Some(enabled);
        }

        ui.add_space(6.0);
        widgets::divider(ui);
        ui.add_space(8.0);

        show_approval_policy(ui, state, &mut actions);

        ui.add_space(2.0);
        widgets::divider(ui);
        ui.add_space(8.0);

        // 会话结束后自动锁屏：默认关闭。它改变的是**本机**状态，故必须在接入确认策略旁边
        // 让用户显式选择；纯文件传输会话不触发（会话收尾守卫只在该会话共享画面时创建）。
        let mut auto_lock = state.auto_lock_on_disconnect;
        if widgets::switch_row(
            ui,
            "会话结束后自动锁定本机屏幕",
            Some("被控会话结束（含网络断开）后锁定本机屏幕；仅文件传输会话不触发。"),
            &mut auto_lock,
        ) {
            actions.auto_lock_on_disconnect = Some(auto_lock);
        }

        ui.add_space(6.0);
        widgets::divider(ui);
        ui.add_space(8.0);

        show_local_meta(ui, state);
    });
    ui.add_space(10.0);
    actions
}

/// 接入确认策略选择器与对应的风险说明。
fn show_approval_policy(ui: &mut Ui, state: &RemoteUiState, actions: &mut RemoteTabActions) {
    let p = palette(ui.style());
    ui.horizontal(|ui| {
        ui.label(RichText::new("接入确认").size(12.0).color(p.text_weak));
        ui.add_space(6.0);
        // 默认「需要确认」；「自动接受」是风险更高的无人值守选择。
        if let Some(policy) = widgets::segmented(
            ui,
            state.approval_policy,
            &[
                (ApprovalPolicy::Confirm, "需要确认"),
                (ApprovalPolicy::Auto, "自动接受"),
            ],
        ) {
            actions.approval_policy = Some(policy);
        }
    });
    ui.add_space(6.0);
    match state.approval_policy {
        ApprovalPolicy::Confirm => {
            ui.label(
                RichText::new("需要确认：每次接入都会弹窗，由你决定是否放行。")
                    .size(12.0)
                    .color(p.text_weak),
            );
        }
        ApprovalPolicy::Auto => {
            widgets::info_bar(
                ui,
                Tone::Warn,
                None,
                "自动接受：任何在线客户端都能直接接管本机，不再向你询问（无人值守场景，风险最高）。",
            );
        }
    }
}

/// 本机元信息：控制通道 / 服务端能力 / 本机 ID（仅非空时显示）/ 平台 / 最近错误。
fn show_local_meta(ui: &mut Ui, state: &RemoteUiState) {
    widgets::key_value(
        ui,
        "控制通道",
        if state.connected {
            "已连接"
        } else {
            "未连接"
        },
        false,
    );
    widgets::key_value(
        ui,
        "服务端能力",
        if state.capability {
            "已启用远程控制"
        } else {
            "未启用（服务端未开启 RustDesk）"
        },
        false,
    );
    if !state.rd_id.is_empty() {
        widgets::key_value(ui, "本机 ID", state.rd_id.as_str(), true);
    }
    widgets::key_value(ui, "平台", state.self_platform.as_str(), false);
    if let Some(err) = &state.last_error {
        ui.add_space(6.0);
        widgets::info_bar(ui, Tone::Err, None, &format!("最近错误：{err}"));
    }
}

/// 已信任对端列表：逐条可移除、可一键全清。列表为空时整段隐藏。
///
/// 信任**只存内存**（见 [`RemoteUiState::trusted_peers`]），故明确告知用户退出即失效。
fn show_trusted_peers(ui: &mut Ui, state: &RemoteUiState, manager: &Arc<RemoteManager>) {
    if state.trusted_peers.is_empty() {
        return;
    }
    widgets::card(ui, |ui| {
        let p = palette(ui.style());
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("已信任的对端")
                    .font(icons::heading_font(16.0))
                    .color(p.text),
            );
            widgets::right_row(ui, |ui| {
                if widgets::button(ui, "全部清除", None, ButtonKind::Secondary).clicked() {
                    manager.clear_trusted();
                }
            });
        });
        ui.label(
            RichText::new("信任仅本次运行有效，退出应用即清除。")
                .size(12.0)
                .color(p.text_weak),
        );
        ui.add_space(6.0);
        for name in &state.trusted_peers {
            ui.horizontal(|ui| {
                ui.label(RichText::new(name.as_str()).size(13.5).color(p.text));
                widgets::right_row_sized(ui, 24.0, |ui| {
                    let remove = widgets::icon_button_colored(ui, Icon::Trash, "移除", Some(p.err));
                    if remove.clicked() {
                        manager.untrust(name);
                    }
                });
            });
        }
    });
    ui.add_space(10.0);
}

/// 断线自动重连提示条（与独立窗口里那条同源，这里给没打开窗口时的可见入口）。
fn show_reconnect(ui: &mut Ui, view: &Arc<Mutex<RemoteView>>, manager: &Arc<RemoteManager>) {
    let (note, active) = {
        let v = lock(view);
        (v.reconnect_note.clone(), v.reconnect_active)
    };
    let Some(note) = note else {
        return;
    };
    let p = palette(ui.style());
    widgets::info_bar_custom(ui, Tone::Warn, |ui| {
        ui.label(RichText::new(note).size(13.0).color(p.text));
        if active {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if widgets::button(ui, "立即重试", None, ButtonKind::Secondary).clicked() {
                    if let Some(target) = lock(view).reconnect.retry_now(Instant::now()) {
                        manager.request_connect(&target.client_name, target.mode);
                    }
                }
                if widgets::button(ui, "停止", None, ButtonKind::Subtle).clicked() {
                    let mut v = lock(view);
                    v.reconnect.cancel();
                    v.reconnect_note = None;
                    v.reconnect_active = false;
                }
            });
        }
    });
}

/// 被控期间的常驻横幅（醒目提示 + 一键中断）。
///
/// 由 `GuiApp` 在**所有 Tab 顶部**调用，确保被控时无论用户切到哪个页都能看到并随时中断。
pub fn controlled_banner(ui: &mut Ui, manager: Option<&Arc<RemoteManager>>) {
    let Some(manager) = manager else {
        return;
    };
    let state = manager.snapshot();
    let Some(active) = state.active.as_ref() else {
        return;
    };
    if active.role != SessionRole::Controlled {
        return;
    }
    let p = palette(ui.style());
    widgets::info_bar_custom(ui, Tone::Err, |ui| {
        ui.horizontal(|ui| {
            let title = if state.audio_enabled {
                format!("正在被「{}」远程控制 · 画面与声音", active.peer_name)
            } else {
                format!("正在被「{}」远程控制", active.peer_name)
            };
            ui.label(
                RichText::new(title)
                    .font(icons::heading_font(13.5))
                    .color(p.text),
            );
            widgets::right_row(ui, |ui| {
                // 声音是独立于画面的权限：用户可以在**不中断会话**的前提下只停声音。
                // 这同时写会话否决位（`audio_allowed`），被控端音频任务 20 ms 内停下。
                if state.audio_enabled
                    && widgets::button(ui, "停止转发声音", None, ButtonKind::Secondary).clicked()
                {
                    manager.set_audio_allowed(false);
                }
                if widgets::button(ui, "立即中断", None, ButtonKind::Danger).clicked() {
                    manager.abort_active();
                }
            });
        });
        // 延迟与数据面路径是被控方最关心的两项（卡不卡、走的直连还是中继）。
        // 关键帧请求速率是被控端**收到**的请求（控制方发起），这里的横幅不展示。
        show_latency(ui, &state, None);
        // 被控时明确告知对外共享的目录，让用户一眼看到别人能读到什么。
        if let Some(dir) = manager.share_dir() {
            ui.label(
                RichText::new(format!("对外共享目录：{}", dir.display()))
                    .size(12.0)
                    .color(p.text_weak),
            );
        }
    });
}

/// 会话摘要卡（画面与输入已迁到独立窗口，这里只留状态与动作）。
///
/// `role` 指定本 Tab 关心的是哪一种角色的会话：只有 `active.role == role` 时才绘制摘要。
/// 角色不符时**不做任何事**（也不消费关闭原因）——两个 Tab 各管一条任务线，在错误的 Tab 里
/// 显示对方的会话会让用户无从判断自己到底在控制谁；而「会话已结束」提示也只由该角色的 Tab
/// 取走，否则切到另一个 Tab 会被它的 `take_closed` 吞掉。
fn show_session(
    ui: &mut Ui,
    view: &Arc<Mutex<RemoteView>>,
    manager: &Arc<RemoteManager>,
    state: &RemoteUiState,
    role: SessionRole,
) {
    let Some(active) = state.active.as_ref() else {
        // 没有活动会话：谁先渲染谁取走结束原因（同一时刻只渲染一个 Tab）。
        if let Some(reason) = lock(view).take_closed() {
            widgets::info_bar(ui, Tone::Warn, Some("会话已结束"), reason.as_str());
        }
        return;
    };
    if active.role != role {
        return;
    }

    widgets::card(ui, |ui| session_body(ui, view, manager, state, active));
    ui.add_space(10.0);
}

/// 会话摘要的卡片内容（「控制中」与「被控中」共用）。
///
/// 独立的 [`show_session`] 把它包进一张卡片；「连接到其他设备」Tab 则把它直接画进
/// 「在线客户端」卡片里，让活动会话显示在客户端列表上方，不再单独占一张卡。
fn session_body(
    ui: &mut Ui,
    view: &Arc<Mutex<RemoteView>>,
    manager: &Arc<RemoteManager>,
    state: &RemoteUiState,
    active: &ActiveSessionInfo,
) {
    // 关键帧请求速率：管理面只累计计数，这里按「累计值差分」每秒结算一次（与帧率同款）。
    // 只有控制器会发关键帧请求；被控端保持 `None`，不显示这一项。
    let keyframe_rate = (active.role == SessionRole::Controller).then(|| {
        let total = state
            .keyframe_requests
            .iter()
            .copied()
            .fold(0u32, u32::saturating_add);
        let mut view = lock(view);
        view.poll_keyframe_rate(total, Instant::now());
        view.keyframe_rate
    });

    let p = palette(ui.style());
    // 注意与 [`show_session`] 的参数 `role`（筛选用）区分：这里是要显示的文案。
    let role_label = match active.role {
        SessionRole::Controller => "控制中",
        SessionRole::Controlled => "被控中",
    };
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(role_label)
                .font(icons::heading_font(16.0))
                .color(p.text),
        );
        widgets::badge(
            ui,
            state_label(active.state),
            state_tone(active.state),
            None,
        );
        ui.label(
            RichText::new(format!("对端：{}", active.peer_name))
                .size(12.0)
                .color(p.text_weak),
        );
    });
    ui.add_space(6.0);
    show_latency(ui, state, keyframe_rate);

    if active.role == SessionRole::Controlled {
        ui.add_space(4.0);
        ui.label(
            RichText::new("本机画面正发送给控制方；如需终止请点「中断会话」。")
                .size(12.0)
                .color(p.text_weak),
        );
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        // 控制器才有独立窗口：没打开（最小化 / 关窗后未结束）时给个召回入口。
        if active.role == SessionRole::Controller
            && !lock(view).window_open
            && widgets::button(ui, "显示窗口", None, ButtonKind::Secondary).clicked()
        {
            lock(view).window_open = true;
            ui.ctx()
                .send_viewport_cmd_to(viewport_id(), egui::ViewportCommand::Focus);
        }
        if widgets::button(ui, "中断会话", None, ButtonKind::Danger).clicked() {
            manager.abort_active();
        }
    });
}

/// 延迟（按阈值上色）与数据面路径；本端作为**被控端**时另有编码侧读数（阶段 3 QoS）。
///
/// `keyframe_rate` 是控制器侧的关键帧请求速率（次/秒）；被控端传 `None`。
fn show_latency(ui: &mut Ui, state: &RemoteUiState, keyframe_rate: Option<u32>) {
    ui.horizontal_wrapped(|ui| {
        match state.latency_ms {
            Some(ms) => {
                ui.colored_label(
                    latency_color(ms),
                    RichText::new(format!("延迟 {ms} ms")).small(),
                );
            }
            None => {
                ui.label(RichText::new("延迟 —").weak().small());
            }
        }
        if let Some(path) = state.transport {
            ui.label(
                RichText::new(format!("路径 {}", transport_label(path)))
                    .weak()
                    .small(),
            );
        }
        // 被控端编码侧读数（由编码任务每秒经进程内通道上报）：编码耗时 / 实测帧率 / 当前生效
        // 码率，以及出码拆解（关键帧 vs 普通帧的码率）与平均变化面积。控制器侧恒为 `None`。
        if let Some(ms) = state.encode_ms {
            // 采集耗时用微秒上报、按毫秒显示一位小数：GPU 采集常 <1 ms，整数毫秒会恒显示 0。
            let capture = state.encode_capture_us.map_or(String::new(), |us| {
                format!(" · 采集 {:.1} ms", f64::from(us) / 1000.0)
            });
            let fps = state
                .encode_fps
                .map_or("—".to_owned(), |f| format!("{f} fps"));
            let kbps = state.encode_kbps.map_or("—".to_owned(), format_kbps);
            let mut parts = vec![format!("编码 {ms} ms{capture}"), fps, kbps];
            if let Some(n) = state.encode_skipped.filter(|n| *n > 0) {
                parts.push(format!("跳过 {n}/s"));
            }
            // 出码拆解：关键帧与普通帧分开显示，才看得出带宽尖峰是不是 IDR 造成的。
            if let Some(n) = state.encode_keyframes.filter(|n| *n > 0) {
                parts.push(format!("关键帧 {n}/s"));
            }
            if let Some(kbps) = state.encode_idr_kbps.filter(|n| *n > 0) {
                parts.push(format!("关键帧 {}", format_kbps(kbps)));
            }
            // 单个关键帧的平均大小：`累计关键帧字节 / 关键帧数 / 1024`。同为「关键帧 x kbps」时，
            // 大量小 IDR 与少量巨大 IDR 的观感完全不同，这个数才是带宽尖峰的真正尺度。
            if let (Some(bytes), Some(n)) = (
                state.encode_idr_bytes,
                state.encode_keyframes.filter(|n| *n > 0),
            ) {
                let avg_kb = bytes / (u64::from(n) * 1024);
                parts.push(format!("关键帧平均 {avg_kb} KB"));
            }
            if let Some(kbps) = state.encode_p_kbps.filter(|n| *n > 0) {
                parts.push(format!("普通帧 {}", format_kbps(kbps)));
            }
            if let Some(permille) = state.encode_changed_permille.filter(|n| *n > 0) {
                parts.push(format!("变化 {}", format_permille_percent(permille)));
            }
            ui.label(RichText::new(parts.join(" · ")).weak().small());
        }
        // 控制器侧的关键帧请求速率：这一项说明「画面在靠补 IDR 恢复」的频率，配合上面的丢包
        // 读数可区分「对端没出帧」与「本端解码链反复断裂」。0/s 时不占版面。
        if let Some(rate) = keyframe_rate.filter(|n| *n > 0) {
            ui.label(RichText::new(format!("关键帧请求 {rate}/s")).weak().small());
        }
        // 控制器侧的补丁速率：补丁模式下画面靠脏矩形增量更新，全帧几乎不再出现，这一项说明
        // 增量通道在工作（与上面的关键帧请求同为弱化小字）。0/s 时不占版面。
        if let Some(n) = state.decode_patches.filter(|n| *n > 0) {
            ui.label(RichText::new(format!("补丁 {n}/s")).weak().small());
        }
    });
}

/// 把千分比显示成百分比（保留一位小数，整数时省略 `.0`）：`30` → `3%`、`3` → `0.3%`。
fn format_permille_percent(permille: u32) -> String {
    let whole = permille / 10;
    let tenths = permille % 10;
    if tenths == 0 {
        format!("{whole}%")
    } else {
        format!("{whole}.{tenths}%")
    }
}

/// 会话模式的中文说明。用户被要求交出的是键鼠控制权，故把「要什么权限」写清楚，
/// 「仅观看」与「完全控制」是性质不同的请求，不能只显示一个英文枚举名。
fn mode_label(mode: RemoteMode) -> &'static str {
    match mode {
        RemoteMode::Control => "完全控制（画面 + 鼠标键盘）",
        RemoteMode::ViewOnly => "仅观看",
        RemoteMode::FileTransfer => "仅文件传输",
    }
}

/// 距自动拒绝还剩多少秒（`deadline_ms` 为 epoch 毫秒；已过点时为 0）。
fn remaining_secs(deadline_ms: u64) -> u64 {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    deadline_ms.saturating_sub(now_ms) / 1000
}

/// 待确认的远程接入弹窗（安全提示，**必须在所有 Tab 之上**）。
///
/// 由 `GuiApp` 每帧调用（与 [`controlled_banner`] 同理）：请求到达时用户可能正在任何页面、
/// 窗口甚至已隐藏到托盘，安全提示绝不能只在「远程」Tab 可见。
///
/// 只展示 [`rust_tunnel_remote::PendingApproval`] 携带的字段——其中**不含**握手 token。
/// 倒计时随 `GuiApp::update` 的 `request_repaint_after` 自然刷新，无需额外唤醒。
///
/// `remember` 是「本次运行内不再询问该对端」的勾选态，由 `GuiApp` 持有（独立窗口后视图不再适合
/// 承载它——弹窗每帧都要可写，而视图已被共享给子窗口回调）。
pub fn approval_modal(
    ctx: &egui::Context,
    remember: &mut bool,
    manager: Option<&Arc<RemoteManager>>,
) {
    let Some(manager) = manager else {
        return;
    };
    let state = manager.snapshot();
    let Some(pending) = state.pending_approval.as_ref() else {
        // 请求已消失（批准/拒绝/超时/断线）：清掉勾选态，别让它带到下一条请求。
        *remember = false;
        return;
    };

    egui::Modal::new(egui::Id::new("remote_approval")).show(ctx, |ui| {
        ui.set_width(380.0);
        // 右上角关闭与「拒绝」同义：安全提示弹窗的默认必须是**不放行**。
        if widgets::modal_title(ui, "远程接入请求") {
            manager.reject_pending(None);
            return;
        }
        ui.add_space(2.0);
        ui.label(
            RichText::new(format!("「{}」请求接入本机", pending.from_name))
                .font(icons::heading_font(15.0)),
        );
        ui.add_space(6.0);
        widgets::key_value(ui, "请求内容", mode_label(pending.mode), false);
        widgets::key_value(
            ui,
            "剩余时间",
            &format!("{} 秒后自动拒绝", remaining_secs(pending.deadline_ms)),
            false,
        );
        ui.add_space(6.0);
        // 三种模式的风险各不相同，都要写明白——「仅文件传输」听起来无害，实际是读共享目录
        // 全文并写入其中；「仅观看」也不等于没风险（对方能看到你的屏幕）。批准是知情同意，
        // 不能只显示一个英文枚举名让用户猜。
        let risk = match pending.mode {
            RemoteMode::Control => "批准后对方可看到你的屏幕，并操作你的鼠标键盘。",
            RemoteMode::ViewOnly => "批准后对方可看到你的屏幕（但不能操作鼠标键盘）。",
            RemoteMode::FileTransfer => "批准后对方可读取共享目录中的文件，并可向其中写入文件。",
        };
        widgets::info_bar(ui, Tone::Warn, None, risk);
        widgets::switch_row(ui, "本次运行内不再询问该对端", None, remember);
        ui.label(
            RichText::new("信任仅在本次运行内有效，退出应用即清除。")
                .size(12.0)
                .color(palette(ui.style()).text_weak),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            widgets::right_row(ui, |ui| {
                if widgets::button(ui, "拒绝", None, ButtonKind::Secondary).clicked() {
                    manager.reject_pending(None);
                }
                if widgets::button(ui, "允许", None, ButtonKind::Primary).clicked() {
                    manager.approve_pending(*remember);
                }
            });
        });
    });
}

/// 在线客户端卡片：可被远程控制的客户端列表 + 一键连接。
fn show_peers(
    ui: &mut Ui,
    state: &RemoteUiState,
    manager: &Arc<RemoteManager>,
    view: &Arc<Mutex<RemoteView>>,
) {
    widgets::card(ui, |ui| {
        let p = palette(ui.style());
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("在线客户端")
                    .font(icons::heading_font(16.0))
                    .color(p.text),
            );
            let count = state.peers.len().to_string();
            widgets::badge(ui, count.as_str(), Tone::Neutral, None);
        });
        ui.add_space(8.0);
        // 控制中的会话摘要直接画在列表上方（同一张卡）：用户切到这个 Tab 时，先看到
        // 「我正在控制谁」，再看到还能连接哪些设备。会话结束后同一位置给出结束原因。
        if let Some(active) = state
            .active
            .as_ref()
            .filter(|active| active.role == SessionRole::Controller)
        {
            session_body(ui, view, manager, state, active);
            ui.add_space(8.0);
            widgets::divider(ui);
            ui.add_space(8.0);
        } else if state.active.is_none() {
            if let Some(reason) = lock(view).take_closed() {
                widgets::info_bar(ui, Tone::Warn, Some("会话已结束"), reason.as_str());
                ui.add_space(8.0);
            }
        }
        if !state.capability {
            widgets::empty_state(
                ui,
                Icon::Server,
                "服务端未启用远程控制（RustDesk）",
                "启用后会在这里列出可连接的在线客户端。",
            );
            return;
        }
        if state.peers.is_empty() {
            widgets::empty_state(
                ui,
                Icon::Monitor,
                "暂无开启被控的在线客户端",
                "对端需要打开「允许被远程控制」并连接到同一服务端。",
            );
            return;
        }
        for (idx, peer) in state.peers.iter().enumerate() {
            if idx > 0 {
                ui.add_space(4.0);
            }
            peer_row(ui, peer, view, manager);
        }
    });
    ui.add_space(10.0);
}

/// 单个在线客户端行：Monitor 图标 + 显示名 + 平台/屏数徽标，右侧「仅观看 / 连接」。
fn peer_row(
    ui: &mut Ui,
    peer: &RemotePeerInfo,
    view: &Arc<Mutex<RemoteView>>,
    manager: &Arc<RemoteManager>,
) {
    let p = palette(ui.style());
    ui.horizontal(|ui| {
        icons::icon(ui, Icon::Monitor, 16.0, p.text_weak);
        ui.label(
            RichText::new(peer.display_name.as_str())
                .font(icons::heading_font(14.0))
                .color(p.text),
        );
        widgets::badge(
            ui,
            &format!("{} · {} 屏", peer.platform, peer.displays),
            Tone::Neutral,
            None,
        );
        widgets::right_row(ui, |ui| {
            // 右对齐布局里先加的在最右：「连接」在右、「仅观看」在左。
            if widgets::button(ui, "连接", None, ButtonKind::Primary).clicked() {
                connect(view, manager, &peer.client_name, RemoteMode::Control);
            }
            if widgets::button(ui, "仅观看", None, ButtonKind::Secondary).clicked() {
                connect(view, manager, &peer.client_name, RemoteMode::ViewOnly);
            }
        });
    });
}

/// 发起连接，并记下重连目标（断线时据此自动重连到同一个对端与模式）。
fn connect(
    view: &Arc<Mutex<RemoteView>>,
    manager: &Arc<RemoteManager>,
    client_name: &str,
    mode: RemoteMode,
) {
    lock(view).reconnect.note_connect(ReconnectTarget {
        client_name: client_name.to_owned(),
        mode,
    });
    manager.request_connect(client_name, mode);
}
