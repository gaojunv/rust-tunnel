use egui::Ui;
use rust_tunnel_client::ClientStatus;

fn status_dot(ui: &mut Ui, label: &str, connected: bool) {
    let color = if connected {
        egui::Color32::from_rgb(34, 170, 85)
    } else {
        egui::Color32::from_rgb(170, 170, 170)
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(label);
    });
}

pub fn show(ui: &mut Ui, status: &ClientStatus) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.heading("连接状态");
            ui.add_space(6.0);
            status_dot(
                ui,
                if status.connected {
                    "已连接"
                } else {
                    "离线"
                },
                status.connected,
            );
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
                ui.colored_label(egui::Color32::from_rgb(200, 40, 40), format!("错误: {e}"));
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
