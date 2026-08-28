//! 映射 Tab。

use egui::Ui;
use rust_tunnel_client::ClientStatus;

pub fn show(ui: &mut Ui, status: &ClientStatus) {
    ui.heading("映射");
    ui.separator();
    let Some(summary) = &status.mapping_summary else {
        ui.label("暂无映射摘要（等待服务端推送或该客户端未被任何规则引用）。");
        return;
    };
    if summary.rules.is_empty() {
        ui.label("暂无映射（该客户端未被任何规则引用）。");
        if summary.truncated {
            ui.label("（已截断）");
        }
        return;
    }
    for rule in &summary.rules {
        ui.collapsing(format!("{} — {} [{}]", rule.name, rule.listen, rule.id), |ui| {
            if !rule.domains.is_empty() {
                ui.label(format!("域名：{}", rule.domains.join(", ")));
            }
            ui.label(format!("TLS：{}", if rule.tls_enabled { "是" } else { "否" }));
            for route in &rule.routes {
                ui.label(format!("路径：{}", route.path));
                for backend in &route.backends {
                    let tag = backend.client_name.as_deref().unwrap_or("-");
                    ui.label(format!(
                        "  后端 {} {} (client={}, weight={})",
                        backend.kind, backend.addr, tag, backend.weight
                    ));
                }
            }
        });
    }
    if summary.truncated {
        ui.label("（因 1MB 上限已截断）");
    }
}
