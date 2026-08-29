use egui::Ui;
use rust_tunnel_client::ClientStatus;

pub fn show(ui: &mut Ui, status: &ClientStatus) {
    let Some(summary) = &status.mapping_summary else {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.heading("映射");
                ui.add_space(4.0);
                ui.label(egui::RichText::new("暂无映射摘要（等待服务端推送或该客户端未被任何规则引用）").weak());
            });
        return;
    };
    if summary.rules.is_empty() {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.heading("映射");
                ui.add_space(4.0);
                ui.label(egui::RichText::new("暂无映射（该客户端未被任何规则引用）").weak());
                if summary.truncated {
                    ui.label(egui::RichText::new("已截断").weak().small());
                }
            });
        return;
    }
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.heading("映射");
            ui.add_space(6.0);
            for rule in &summary.rules {
                egui::CollapsingHeader::new(format!("{} — {}  [{}]", rule.name, rule.listen, rule.id))
                    .default_open(false)
                    .show(ui, |ui| {
                        if !rule.domains.is_empty() {
                            ui.label(egui::RichText::new(format!("域名: {}", rule.domains.join(", "))).small());
                        }
                        ui.label(egui::RichText::new(format!("TLS: {}", if rule.tls_enabled { "是" } else { "否" })).small());
                        for route in &rule.routes {
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new(format!("路径: {}", route.path)).small().strong());
                            for backend in &route.backends {
                                let tag = backend.client_name.as_deref().unwrap_or("-");
                                ui.label(
                                    egui::RichText::new(format!(
                                        "后端 {} {} (client={}, weight={})",
                                        backend.kind, backend.addr, tag, backend.weight
                                    ))
                                    .small()
                                    .monospace(),
                                );
                            }
                        }
                    });
                ui.add_space(4.0);
            }
            if summary.truncated {
                ui.label(egui::RichText::new("因 1MB 上限已截断").weak().small());
            }
        });
}
