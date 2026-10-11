//! 「远程」Tab：可被远程控制的客户端列表、一键连接、被控横幅与会话窗口。

use std::sync::Arc;

use egui::{RichText, Ui};
use rust_tunnel_common::protocol::RemoteMode;
use rust_tunnel_remote::{RemoteManager, RemoteUiState, SessionRole, SessionState};

use crate::app::AppState;
use crate::remote::{self, PermissionKind, RemoteView};

/// 会话状态中文标签。
fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Negotiating => "协商中",
        SessionState::Connecting => "建立连接中",
        SessionState::Active => "已连接",
        SessionState::Ended => "已结束",
    }
}

/// 渲染「远程」Tab。返回 `Some(新状态)` 表示用户切换了「允许被远程控制」开关，
/// 由调用方（`GuiApp`）同步到引擎、托盘与偏好文件。
pub fn show(
    ui: &mut Ui,
    view: &mut RemoteView,
    manager: Option<&Arc<RemoteManager>>,
    _app_state: &Arc<AppState>,
) -> Option<bool> {
    let Some(manager) = manager else {
        ui.add_space(10.0);
        ui.label("远程控制引擎尚未就绪（等待客户端配置 / 连接）…");
        return None;
    };
    let state = manager.snapshot();

    show_permissions(ui);
    let toggle = show_switch(ui, &state);
    show_session(ui, view, manager, &state);
    show_peers(ui, &state, manager);
    toggle
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
        });
    ui.add_space(8.0);
}

/// 「允许被远程控制」开关与本机状态。
fn show_switch(ui: &mut Ui, state: &RemoteUiState) -> Option<bool> {
    let mut toggle = None;
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let mut enabled = state.enabled;
            ui.horizontal(|ui| {
                if ui.checkbox(&mut enabled, "允许被远程控制").changed() {
                    toggle = Some(enabled);
                }
                ui.label(RichText::new("开启后本机屏幕与输入可被持有效 token 的对端接管").weak().small());
            });
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
    toggle
}

/// 会话窗口（画面 + 输入捕获 + 工具条）。
#[expect(
    clippy::cast_precision_loss,
    reason = "像素尺寸有界，f32 精度足够"
)]
fn show_session(
    ui: &mut Ui,
    view: &mut RemoteView,
    manager: &Arc<RemoteManager>,
    state: &RemoteUiState,
) {
    let Some(active) = state.active.as_ref() else {
        view.reset_session();
        if let Some(reason) = view.take_closed() {
            ui.colored_label(ui.visuals().warn_fg_color, format!("会话已结束：{reason}"));
            ui.add_space(8.0);
        }
        return;
    };

    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(10, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let role = match active.role {
                    SessionRole::Controller => "控制中",
                    SessionRole::Controlled => "被控中",
                };
                ui.label(RichText::new(format!("远程会话 · {role} · {}", state_label(active.state))).strong());
                ui.label(RichText::new(format!("对端：{}", active.peer_name)).weak().small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("中断会话").clicked() {
                        manager.abort_active();
                    }
                });
            });
            ui.add_space(6.0);

            // 被控端不接收画面，只提示状态（画面/输入捕获是控制端的事）。
            if active.role == SessionRole::Controlled {
                ui.label(RichText::new("本机画面正发送给控制方；如需终止请点「中断会话」。").weak());
                return;
            }

            // 多显示器选择。
            let displays = view.displays();
            if displays.len() > 1 {
                let current = view
                    .selected_display()
                    .or_else(|| displays.first().map(|d| d.id));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("显示器").weak().small());
                    for display in &displays {
                        if ui
                            .selectable_label(current == Some(display.id), display.name.as_str())
                            .clicked()
                        {
                            view.set_selected_display(display.id);
                        }
                    }
                });
                ui.add_space(4.0);
            } else if let Some(display) = displays.first() {
                view.set_selected_display(display.id);
            }

            // 画面：等比缩放铺满可用宽度，高度上限 420。
            if let Some((texture, width, height)) = view.upload(ui.ctx()) {
                let avail = ui.available_size();
                let avail = egui::vec2(avail.x.max(80.0), avail.y.clamp(60.0, 420.0));
                let scale = (avail.x / width as f32)
                    .min(avail.y / height as f32)
                    .max(0.01);
                let size = egui::vec2(
                    (width as f32 * scale).max(1.0),
                    (height as f32 * scale).max(1.0),
                );
                let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
                ui.painter().image(
                    texture,
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
                // 仅控制器 + 完全控制模式捕获输入。
                if active.role == SessionRole::Controller && active.mode == RemoteMode::Control {
                    view.capture_input(ui, manager, &response, rect, width, height);
                }
            } else {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("等待画面…").weak());
                });
                ui.add_space(24.0);
            }
        });
    ui.add_space(8.0);
}

/// 可被远程控制的在线客户端列表 + 一键连接。
fn show_peers(ui: &mut Ui, state: &RemoteUiState, manager: &Arc<RemoteManager>) {
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
                            manager.request_connect(&peer.client_name, RemoteMode::ViewOnly);
                        }
                        if ui.button("连接").clicked() {
                            manager.request_connect(&peer.client_name, RemoteMode::Control);
                        }
                    });
                });
            }
        });
}
