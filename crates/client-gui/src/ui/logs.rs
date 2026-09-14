use std::sync::Arc;

use egui::Ui;
use rust_tunnel_client::LogBuffer;

fn level_color(level: &str, visuals: &egui::Visuals) -> Option<egui::Color32> {
    match level {
        "ERROR" => Some(visuals.error_fg_color),
        "WARN" => Some(visuals.warn_fg_color),
        "DEBUG" => Some(visuals.weak_text_color()),
        _ => None, // INFO 用默认文本色，深浅主题均可读
    }
}

pub fn show(ui: &mut Ui, log_buffer: &Arc<LogBuffer>) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("日志");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("清空").clicked() {
                        log_buffer.clear();
                    }
                    ui.label(
                        egui::RichText::new(format!("{} 条", log_buffer.len()))
                            .weak()
                            .small(),
                    );
                });
            });
            ui.add_space(6.0);
            let n = log_buffer.len();
            if n == 0 {
                ui.label(
                    egui::RichText::new("暂无日志（连接后自动采集，含控制/隧道/agent 动态）")
                        .weak()
                        .small(),
                );
                return;
            }
            let max_h = (ui.available_height() - 8.0).clamp(80.0, 360.0);
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(max_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for e in log_buffer.recent(500) {
                        let ts = e.timestamp;
                        let secs = ts / 1_000_000;
                        let h = (secs / 3600) % 24;
                        let m = (secs / 60) % 60;
                        let s = secs % 60;
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("{h:02}:{m:02}:{s:02}"))
                                    .small()
                                    .weak()
                                    .monospace(),
                            );
                            let mut level_text =
                                egui::RichText::new(e.level.clone()).small();
                            if let Some(c) = level_color(&e.level, ui.visuals()) {
                                level_text = level_text.color(c);
                            }
                            ui.label(level_text);
                            ui.label(
                                egui::RichText::new(format!("{} — {}", e.target, e.message))
                                    .small()
                                    .monospace(),
                            );
                        });
                    }
                });
        });
}
