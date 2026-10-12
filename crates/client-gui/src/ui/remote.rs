//! 「远程」Tab：可被远程控制的客户端列表、一键连接、被控横幅与会话摘要。
//!
//! 画面 / 输入捕获 / 剪贴板 / 文件面板已整体迁到独立会话窗口（[`crate::ui::session_window`]）；本
//! Tab 只留下**摘要卡**：会话状态、延迟、数据面路径、以及「显示窗口 / 中断会话」两个动作。会话窗
//! 口可被最小化（会话继续保留），故这里必须留一个把它召回入口。

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use egui::{RichText, Ui};
use rust_tunnel_common::protocol::RemoteMode;
use rust_tunnel_remote::{ApprovalPolicy, RemoteManager, RemoteUiState, SessionRole, SessionState};

use crate::app::AppState;
use crate::remote::{self, PermissionKind, ReconnectTarget, RemoteView};
use crate::ui::session_window::{latency_color, transport_label, viewport_id};

/// 会话状态中文标签。
pub fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Negotiating => "协商中",
        SessionState::Connecting => "建立连接中",
        SessionState::Active => "已连接",
        SessionState::Ended => "已结束",
    }
}

/// 取共享视图的锁（中毒时取回内部值，一个视图锁中毒不该拖垮整个 UI）。
fn lock(view: &Arc<Mutex<RemoteView>>) -> MutexGuard<'_, RemoteView> {
    view.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 「远程」Tab 的用户操作结果，由 `GuiApp` 同步到引擎与偏好文件。
#[derive(Default)]
pub struct RemoteTabActions {
    /// 用户切换了「允许被远程控制」开关（`Some` 表示有变化）。
    pub allow_remote: Option<bool>,
    /// 用户切换了接入确认策略（`Some` 表示有变化）。
    pub approval_policy: Option<ApprovalPolicy>,
}

/// 渲染「远程」Tab。返回本帧的用户操作，由调用方（`GuiApp`）同步到引擎、托盘与偏好文件。
pub fn show(
    ui: &mut Ui,
    view: &Arc<Mutex<RemoteView>>,
    manager: Option<&Arc<RemoteManager>>,
    _app_state: &Arc<AppState>,
) -> RemoteTabActions {
    let Some(manager) = manager else {
        ui.add_space(10.0);
        ui.label("远程控制引擎尚未就绪（等待客户端配置 / 连接）…");
        return RemoteTabActions::default();
    };
    let state = manager.snapshot();

    show_permissions(ui);
    let actions = show_switch(ui, &state);
    show_reconnect(ui, view, manager);
    show_session(ui, view, manager, &state);
    show_trusted_peers(ui, &state, manager);
    show_peers(ui, &state, manager, view);
    actions
}

/// 平台权限提示（缺失时给出「去授权」按钮，避免被控时静默黑屏）。
fn show_permissions(ui: &mut Ui) {
    let perms = remote::permissions();
    if !perms.supported || perms.all_granted() {
        return;
    }
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.colored_label(ui.visuals().warn_fg_color, "远程控制权限不足");
            if !perms.screen_capture {
                ui.horizontal(|ui| {
                    ui.label("缺少「屏幕录制」权限，被控时无法采集画面。");
                    if ui.button("去授权").clicked() {
                        remote::open_permission_settings(PermissionKind::ScreenCapture);
                    }
                });
            }
            if !perms.accessibility {
                ui.horizontal(|ui| {
                    ui.label("缺少「辅助功能」权限，被控时无法注入鼠标键盘。");
                    if ui.button("去授权").clicked() {
                        remote::open_permission_settings(PermissionKind::Accessibility);
                    }
                });
            }
            ui.label(RichText::new("授权后需重启本应用方可生效。").weak().small());
        });
    ui.add_space(8.0);
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
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgba_unmultiplied(220, 160, 40, 30))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().warn_fg_color, note);
                if active {
                    if ui.button("立即重试").clicked() {
                        if let Some(target) = lock(view).reconnect.retry_now(std::time::Instant::now()) {
                            manager.request_connect(&target.client_name, target.mode);
                        }
                    }
                    if ui.button("停止").clicked() {
                        let mut v = lock(view);
                        v.reconnect.cancel();
                        v.reconnect_note = None;
                        v.reconnect_active = false;
                    }
                }
            });
        });
    ui.add_space(8.0);
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
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgba_unmultiplied(200, 60, 60, 40))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    RichText::new(format!("正在被「{}」远程控制", active.peer_name)).strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("立即中断").clicked() {
                        manager.abort_active();
                    }
                });
            });
            // 延迟与数据面路径是被控方最关心的两项（卡不卡、走的直连还是中继）。
            ui.horizontal(|ui| {
                show_latency(ui, &state);
            });
            // 被控时明确告知对外共享的目录，让用户一眼看到别人能读到什么。
            if let Some(dir) = manager.share_dir() {
                ui.label(RichText::new(format!("对外共享目录：{}", dir.display())).weak().small());
            }
        });
    ui.add_space(8.0);
}

/// 会话摘要卡（画面与输入已迁到独立窗口，这里只留状态与动作）。
fn show_session(
    ui: &mut Ui,
    view: &Arc<Mutex<RemoteView>>,
    manager: &Arc<RemoteManager>,
    state: &RemoteUiState,
) {
    let Some(active) = state.active.as_ref() else {
        if let Some(reason) = lock(view).take_closed() {
            ui.colored_label(ui.visuals().warn_fg_color, format!("会话已结束：{reason}"));
            ui.add_space(8.0);
        }
        return;
    };

    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let role = match active.role {
                    SessionRole::Controller => "控制中",
                    SessionRole::Controlled => "被控中",
                };
                ui.label(RichText::new(format!("远程会话 · {role} · {}", state_label(active.state))).strong());
                ui.label(RichText::new(format!("对端：{}", active.peer_name)).weak().small());
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                show_latency(ui, state);
            });

            if active.role == SessionRole::Controlled {
                ui.add_space(4.0);
                ui.label(RichText::new("本机画面正发送给控制方；如需终止请点「中断会话」。").weak());
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                // 控制器才有独立窗口：没打开（最小化 / 关窗后未结束）时给个召回入口。
                if active.role == SessionRole::Controller
                    && !lock(view).window_open
                    && ui.button("显示窗口").clicked()
                {
                    lock(view).window_open = true;
                    ui.ctx().send_viewport_cmd_to(viewport_id(), egui::ViewportCommand::Focus);
                }
                if ui.button("中断会话").clicked() {
                    manager.abort_active();
                }
            });
        });
    ui.add_space(8.0);
}

/// 延迟（按阈值上色）与数据面路径。
fn show_latency(ui: &mut Ui, state: &RemoteUiState) {
    match state.latency_ms {
        Some(ms) => {
            ui.colored_label(latency_color(ms), RichText::new(format!("延迟 {ms} ms")).small());
        }
        None => {
            ui.label(RichText::new("延迟 —").weak().small());
        }
    }
    if let Some(path) = state.transport {
        ui.label(RichText::new(format!("路径 {}", transport_label(path))).weak().small());
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
pub fn approval_modal(ctx: &egui::Context, remember: &mut bool, manager: Option<&Arc<RemoteManager>>) {
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
        ui.set_width(340.0);
        ui.heading("远程接入请求");
        ui.add_space(6.0);
        ui.label(RichText::new(format!("「{}」请求接入本机", pending.from_name)).strong());
        ui.add_space(4.0);
        egui::Grid::new("remote_approval_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new("请求内容").weak().small());
                ui.label(mode_label(pending.mode));
                ui.end_row();
                ui.label(RichText::new("剩余时间").weak().small());
                ui.label(format!("{} 秒后自动拒绝", remaining_secs(pending.deadline_ms)));
                ui.end_row();
            });
        // 三种模式的风险各不相同，都要写明白——「仅文件传输」听起来无害，实际是读共享目录
        // 全文并写入其中；「仅观看」也不等于没风险（对方能看到你的屏幕）。批准是知情同意，
        // 不能只显示一个英文枚举名让用户猜。
        let risk = match pending.mode {
            RemoteMode::Control => "批准后对方可看到你的屏幕，并操作你的鼠标键盘。",
            RemoteMode::ViewOnly => "批准后对方可看到你的屏幕（但不能操作鼠标键盘）。",
            RemoteMode::FileTransfer => "批准后对方可读取共享目录中的文件，并可向其中写入文件。",
        };
        ui.add_space(4.0);
        ui.colored_label(ui.visuals().warn_fg_color, RichText::new(risk).small());
        ui.add_space(6.0);
        ui.checkbox(remember, "本次运行内不再询问该对端");
        ui.label(RichText::new("信任仅在本次运行内有效，退出应用即清除。").weak().small());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("允许").clicked() {
                manager.approve_pending(*remember);
            }
            if ui.button("拒绝").clicked() {
                manager.reject_pending(None);
            }
        });
    });
}

/// 「允许被远程控制」开关、接入确认策略与本机状态。
fn show_switch(ui: &mut Ui, state: &RemoteUiState) -> RemoteTabActions {
    let mut actions = RemoteTabActions::default();
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let mut enabled = state.enabled;
            ui.horizontal(|ui| {
                if ui.checkbox(&mut enabled, "允许被远程控制").changed() {
                    actions.allow_remote = Some(enabled);
                }
                ui.label(RichText::new("开启后本机屏幕与输入可被持有效 token 的对端接管").weak().small());
            });
            ui.add_space(4.0);

            // 接入确认策略：默认「需要确认」；「自动接受」是风险更高的无人值守选择，故用警示色。
            ui.horizontal(|ui| {
                ui.label(RichText::new("接入确认").weak().small());
                let mut policy = state.approval_policy;
                if ui
                    .selectable_label(policy == ApprovalPolicy::Confirm, "需要确认")
                    .clicked()
                {
                    policy = ApprovalPolicy::Confirm;
                }
                if ui
                    .selectable_label(policy == ApprovalPolicy::Auto, "自动接受")
                    .clicked()
                {
                    policy = ApprovalPolicy::Auto;
                }
                if policy != state.approval_policy {
                    actions.approval_policy = Some(policy);
                }
            });
            if state.approval_policy == ApprovalPolicy::Auto {
                ui.label(
                    RichText::new("自动接受：任何在线客户端都能直接接管本机，不再向你询问（无人值守场景，风险最高）。")
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
            } else {
                ui.label(RichText::new("需要确认：每次接入都会弹窗，由你决定是否放行。").weak().small());
            }
            ui.add_space(4.0);

            egui::Grid::new("remote_meta")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("控制通道").weak().small());
                    ui.label(if state.connected { "已连接" } else { "未连接" });
                    ui.end_row();
                    ui.label(RichText::new("服务端能力").weak().small());
                    ui.label(if state.capability {
                        "已启用远程控制"
                    } else {
                        "未启用（服务端未开启 RustDesk）"
                    });
                    ui.end_row();
                    if !state.rd_id.is_empty() {
                        ui.label(RichText::new("本机 ID").weak().small());
                        ui.label(state.rd_id.as_str());
                        ui.end_row();
                    }
                    ui.label(RichText::new("平台").weak().small());
                    ui.label(state.self_platform.as_str());
                    ui.end_row();
                });
            if let Some(err) = &state.last_error {
                ui.add_space(4.0);
                ui.colored_label(ui.visuals().error_fg_color, format!("最近错误：{err}"));
            }
        });
    ui.add_space(8.0);
    actions
}

/// 已信任对端列表：逐条可移除、可一键全清。列表为空时整段隐藏。
///
/// 信任**只存内存**（见 [`RemoteUiState::trusted_peers`]），故明确告知用户退出即失效。
fn show_trusted_peers(ui: &mut Ui, state: &RemoteUiState, manager: &Arc<RemoteManager>) {
    if state.trusted_peers.is_empty() {
        return;
    }
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("已信任的对端").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("全部清除").clicked() {
                        manager.clear_trusted();
                    }
                });
            });
            ui.label(RichText::new("信任仅本次运行有效，退出应用即清除。").weak().small());
            ui.add_space(4.0);
            for name in &state.trusted_peers {
                ui.horizontal(|ui| {
                    ui.label(name.as_str());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("移除").clicked() {
                            manager.untrust(name);
                        }
                    });
                });
            }
        });
    ui.add_space(8.0);
}

/// 可被远程控制的在线客户端列表 + 一键连接。
fn show_peers(ui: &mut Ui, state: &RemoteUiState, manager: &Arc<RemoteManager>, view: &Arc<Mutex<RemoteView>>) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.label(RichText::new("可被远程控制的在线客户端").strong());
            ui.add_space(4.0);
            if !state.capability {
                ui.label(RichText::new("服务端未启用远程控制（RustDesk）").weak());
                return;
            }
            if state.peers.is_empty() {
                ui.label(RichText::new("暂无开启被控的在线客户端").weak());
                return;
            }
            for peer in &state.peers {
                ui.horizontal(|ui| {
                    ui.label(peer.display_name.as_str());
                    ui.label(
                        RichText::new(format!("{} · {} 屏", peer.platform, peer.displays))
                            .weak()
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("仅观看").clicked() {
                            connect(view, manager, &peer.client_name, RemoteMode::ViewOnly);
                        }
                        if ui.button("连接").clicked() {
                            connect(view, manager, &peer.client_name, RemoteMode::Control);
                        }
                    });
                });
            }
        });
}

/// 发起连接，并记下重连目标（断线时据此自动重连到同一个对端与模式）。
fn connect(view: &Arc<Mutex<RemoteView>>, manager: &Arc<RemoteManager>, client_name: &str, mode: RemoteMode) {
    lock(view).reconnect.note_connect(ReconnectTarget {
        client_name: client_name.to_owned(),
        mode,
    });
    manager.request_connect(client_name, mode);
}
