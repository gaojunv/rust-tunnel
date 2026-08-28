//! 日志 Tab（最近 500 行，环形缓冲）。

use std::sync::Arc;

use egui::Ui;
use rust_tunnel_client::LogBuffer;

pub fn show(ui: &mut Ui, log_buffer: &Arc<LogBuffer>) {
    ui.heading("日志");
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("清空").clicked() {
            log_buffer.clear();
        }
        ui.label(format!("{} 条", log_buffer.len()));
    });
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for e in log_buffer.recent(500) {
                ui.label(format!("[{}] {} {}: {}", e.level, e.target, e.timestamp, e.message));
            }
        });
}
