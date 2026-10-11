//! 「远程」Tab：可被远程控制的客户端列表、一键连接、被控横幅与会话窗口。

use std::sync::Arc;

use egui::{RichText, Ui};
use rust_tunnel_common::protocol::RemoteMode;
use rust_tunnel_remote::{Clipboard as _, QualityPreset, RemoteManager, RemoteUiState, SessionRole, SessionState};

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
            // 被控时明确告知对外共享的目录，让用户一眼看到别人能读到什么。
            if let Some(dir) = manager.share_dir() {
                ui.label(RichText::new(format!("对外共享目录：{}", dir.display())).weak().small());
            }
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
                if let Some(dir) = manager.share_dir() {
                    ui.label(RichText::new(format!("对外共享目录：{}", dir.display())).weak().small());
                }
                return;
            }

            // 显示器选择 + 画质档位（都由被控端生效）。
            show_display_selector(ui, view, manager);
            show_quality_selector(ui, manager, state.quality);
            ui.add_space(4.0);

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

            // 剪贴板与文件面板（仅控制器侧有意义：被控端只应答）。
            show_clipboard(ui, view, manager);
            show_files(ui, view, manager, state);
        });
    ui.add_space(8.0);
}

/// 显示器选择：多显示器时逐台可点，单显示器时把当前显示器同步给引擎。
fn show_display_selector(ui: &mut Ui, view: &mut RemoteView, manager: &Arc<RemoteManager>) {
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
                    view.set_selected_display(display.id, manager);
                }
            }
        });
    } else if let Some(display) = displays.first() {
        view.set_selected_display(display.id, manager);
    }
}

/// 画质档位选择（流畅/均衡/高清）；切换经会话命令下发，由被控端生效。
fn show_quality_selector(ui: &mut Ui, manager: &Arc<RemoteManager>, current: QualityPreset) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("画质").weak().small());
        for preset in QualityPreset::all() {
            if ui.selectable_label(current == preset, preset.label()).clicked() {
                manager.set_quality(preset);
            }
        }
    });
}

/// 剪贴板同步：发送本机剪贴板、索取对端剪贴板、展示收到的对端文本。
///
/// 读取本机剪贴板借 [`remote::SystemClipboard`]（arboard）——egui 0.31 只提供**写**系统剪贴板的
/// `Context::copy_text`，没有读取 API，故这里用平台实现读；读不到时可退回文本框里手填的内容。
fn show_clipboard(ui: &mut Ui, view: &mut RemoteView, manager: &Arc<RemoteManager>) {
    // 对端发来的剪贴板：从共享槽取走并留在本地输入态，跨帧展示。
    if let Some(text) = view.take_clipboard() {
        view.inputs_mut().clipboard_in = Some(text);
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.label(RichText::new("剪贴板").strong());
        if ui.button("读取本机").clicked() {
            match remote::SystemClipboard.get() {
                Ok(text) => view.inputs_mut().clipboard_out = text,
                Err(err) => tracing::warn!("读取本机剪贴板失败：{err}"),
            }
        }
        if ui.button("发送本机剪贴板").clicked() {
            let text = view.inputs_mut().clipboard_out.clone();
            manager.send_clipboard(text);
        }
        if ui.button("获取对端剪贴板").clicked() {
            manager.request_clipboard();
        }
    });
    ui.add(
        egui::TextEdit::singleline(&mut view.inputs_mut().clipboard_out)
            .desired_width(f32::INFINITY)
            .hint_text("待发送给对端的文本（可点「读取本机」填入）"),
    );
    if let Some(text) = view.inputs_mut().clipboard_in.clone() {
        ui.horizontal(|ui| {
            ui.label(RichText::new("对端剪贴板").weak().small());
            if ui.button("复制到本机").clicked() {
                ui.ctx().copy_text(text.clone());
            }
            if ui.button("清空").clicked() {
                view.inputs_mut().clipboard_in = None;
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
fn show_files(ui: &mut Ui, view: &mut RemoteView, manager: &Arc<RemoteManager>, state: &RemoteUiState) {
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
                    egui::TextEdit::singleline(&mut view.inputs_mut().upload_local)
                        .desired_width(150.0)
                        .hint_text("本机绝对路径"),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut view.inputs_mut().upload_name)
                        .desired_width(110.0)
                        .hint_text("目标文件名"),
                );
                if ui.button("上传").clicked() {
                    let local = view.inputs_mut().upload_local.clone();
                    let name = view.inputs_mut().upload_name.clone();
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
