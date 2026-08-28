//! 状态 Tab。

use egui::Ui;
use rust_tunnel_client::ClientStatus;

pub fn show(ui: &mut Ui, status: &ClientStatus) {
    ui.heading("连接状态");
    ui.separator();
    ui.label(format!(
        "连接：{}",
        if status.connected { "● 已连接" } else { "○ 离线" }
    ));
    if let Some(at) = status.connected_at {
        if let Ok(d) = at.elapsed() {
            ui.label(format!("在线时长：{}s", d.as_secs()));
        }
    }
    ui.label(format!("服务器：{}", status.server));
    ui.label(format!("客户端名：{}", status.client_name));
    ui.label(format!("版本：{}", status.version));
    if let Some(rtt) = status.rtt_ms {
        ui.label(format!("RTT：{rtt:.1} ms"));
    }
    ui.label(format!(
        "隧道：active {} / pending {} / recent {}",
        status.active_tunnels,
        status.pending_tunnels,
        status.recent_tunnels.len()
    ));
    if let Some(e) = &status.last_error {
        ui.colored_label(egui::Color32::RED, format!("错误：{e}"));
    }
}
