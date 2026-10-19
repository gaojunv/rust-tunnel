use egui::Ui;
use rust_tunnel_client::ClientStatus;

use crate::service_watch::IdentityOwner;

fn status_dot(ui: &mut Ui, label: &str, state: crate::tray::TrayState) {
    let dark = ui.visuals().dark_mode;
    let color = match state {
        crate::tray::TrayState::Connected => {
            if dark {
                egui::Color32::from_rgb(80, 200, 120)
            } else {
                egui::Color32::from_rgb(20, 140, 70)
            }
        }
        // 重连中与服务接管都用警示色：前者是真在重连，后者是本窗口未注册（原因由下面的横幅解释）。
        crate::tray::TrayState::Reconnecting | crate::tray::TrayState::ServiceOwned => {
            ui.visuals().warn_fg_color
        }
        crate::tray::TrayState::Offline => {
            if dark {
                egui::Color32::from_rgb(130, 130, 130)
            } else {
                egui::Color32::from_rgb(150, 150, 150)
            }
        }
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(label);
    });
}

/// 服务接管身份时的醒目横幅（与 `ui::remote::controlled_banner` 同风格）。
///
/// **必须显眼**：被动期间本窗口既没有远程控制能力、也收不到接入确认弹窗（它压根没注册），
/// 状态面板里的连接信息也不再代表实际被控状态。这些是设计使然，但**绝不能是静默的**——
/// 否则用户只会看到「明明在线却说没连接」这类无法自解释的现象。
fn service_owned_banner(ui: &mut Ui) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgba_unmultiplied(200, 160, 40, 40))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                egui::RichText::new("⚠ Windows 服务正在承载客户端身份").strong(),
            );
            ui.add_space(4.0);
            ui.label(crate::service_watch::passive_notice());
        });
    ui.add_space(8.0);
}

pub fn show(ui: &mut Ui, status: &ClientStatus, owner: IdentityOwner) {
    if owner.is_service() {
        service_owned_banner(ui);
    }
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.heading("连接状态");
            ui.add_space(6.0);
            let tray_state = crate::tray::TrayState::from_status_with_service(status, owner);
            let label = match tray_state {
                crate::tray::TrayState::Connected => "已连接",
                crate::tray::TrayState::Reconnecting => "重连中",
                crate::tray::TrayState::Offline => "离线",
                crate::tray::TrayState::ServiceOwned => "服务接管",
            };
            status_dot(ui, label, tray_state);
            egui::Grid::new("status_grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("服务器").weak().small());
                    ui.label(status.server.as_str());
                    ui.end_row();
                    ui.label(egui::RichText::new("客户端名").weak().small());
                    ui.label(status.client_name.as_str());
                    ui.end_row();
                    ui.label(egui::RichText::new("版本").weak().small());
                    ui.label(status.version.as_str());
                    ui.end_row();
                    if let Some(ms) = status.rtt_ms {
                        ui.label(egui::RichText::new("RTT").weak().small());
                        ui.label(format!("{ms:.0} ms"));
                        ui.end_row();
                    }
                    if let Some(at) = status.connected_at {
                        if let Ok(d) = at.elapsed() {
                            ui.label(egui::RichText::new("在线时长").weak().small());
                            ui.label(format!("{}s", d.as_secs()));
                            ui.end_row();
                        }
                    }
                });
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(2.0);
            egui::Grid::new("tunnel_grid")
                .num_columns(3)
                .spacing([14.0, 4.0])
                .show(ui, |ui| {
                    ui.label(format!("活跃 {}", status.active_tunnels));
                    ui.label(format!("等待 {}", status.pending_tunnels));
                    ui.label(format!("最近 {} 条", status.recent_tunnels.len()));
                    ui.end_row();
                });
            if let Some(e) = &status.last_error {
                ui.add_space(6.0);
                ui.colored_label(ui.visuals().error_fg_color, format!("错误: {e}"));
            }
        });
    if !status.recent_tunnels.is_empty() {
        ui.add_space(10.0);
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("最近隧道").strong().small());
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(92.0)
                    .show(ui, |ui| {
                        for (id, addr, _) in status.recent_tunnels.iter().take(8) {
                            ui.label(
                                egui::RichText::new(format!("{id}: {addr}"))
                                    .small()
                                    .monospace(),
                            );
                        }
                    });
            });
    }
}
