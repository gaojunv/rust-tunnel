use std::sync::Arc;

use egui::Ui;

use rust_tunnel_client::ClientConfig;

use crate::service_ui::{
    host_config_advisory, host_config_path_display, host_config_presence, owner_label,
    presence_label, start_blocked_reason, ServiceOp, ServiceOps, SERVICE_SUPPORTED,
};
use crate::service_watch::ServicePresence;
use crate::{app::AppState, config_store};

#[allow(
    clippy::struct_excessive_bools,
    reason = "设置面板布尔开关组，拆为 enum 反而割裂布局"
)]
#[derive(Debug, Clone)]
pub struct SettingsState {
    pub server: String,
    pub name: String,
    pub password: String,
    pub save_to_keyring: bool,
    pub tls: bool,
    pub tls_insecure: bool,
    pub enable_agent: bool,
    pub status_msg: Option<String>,
    pub remember_password: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            server: String::new(),
            name: String::new(),
            password: String::new(),
            save_to_keyring: true,
            tls: true,
            tls_insecure: true,
            enable_agent: false,
            status_msg: None,
            // 默认掩码显示密码（勾选"显示密码"才明文），防肩窥。
            remember_password: false,
        }
    }
}

impl SettingsState {
    #[must_use]
    pub fn from_config(cfg: &ClientConfig) -> Self {
        Self {
            server: cfg.server.clone(),
            name: cfg.name.clone().unwrap_or_default(),
            password: cfg.password.clone(),
            save_to_keyring: false,
            tls: cfg.tls,
            tls_insecure: cfg.tls_insecure,
            enable_agent: cfg.enable_agent,
            status_msg: None,
            remember_password: false,
        }
    }

    fn to_config(&self, prev: Option<&ClientConfig>) -> ClientConfig {
        let mut cfg = prev.cloned().unwrap_or_default();
        cfg.server.clone_from(&self.server);
        cfg.name = if self.name.is_empty() {
            None
        } else {
            Some(self.name.clone())
        };
        cfg.password.clone_from(&self.password);
        cfg.tls = self.tls;
        cfg.tls_insecure = self.tls_insecure;
        cfg.enable_agent = self.enable_agent;
        cfg
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "设置面板单屏表单，拆分会割裂布局"
)]
pub fn show(ui: &mut Ui, state: &mut SettingsState, app_state: &Arc<AppState>) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.heading("设置");
            ui.add_space(4.0);
            egui::Grid::new("settings_grid")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.label("服务器地址");
                    ui.text_edit_singleline(&mut state.server);
                    ui.end_row();
                    ui.label("客户端名");
                    ui.text_edit_singleline(&mut state.name);
                    ui.end_row();
                    ui.label("密码 / Token");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.password)
                            .password(!state.remember_password),
                    );
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut state.remember_password, "显示密码");
                ui.checkbox(&mut state.save_to_keyring, "存入系统钥匙串");
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut state.tls, "启用 TLS");
                ui.checkbox(&mut state.tls_insecure, "跳过证书校验 (TOFU)");
            });
            ui.add_space(4.0);
            ui.checkbox(
                &mut state.enable_agent,
                "启用 Agent 执行器（远程命令/文件操作）",
            );
            ui.add_space(10.0);
            let can_save = !state.server.trim().is_empty() && !state.password.trim().is_empty();
            ui.add_enabled_ui(can_save, |ui| {
                if ui.button("保存并重连").clicked() {
                    let current = app_state.config_snapshot();
                    let cfg = state.to_config(current.as_ref());
                    if cfg.server.trim().is_empty() || cfg.password.trim().is_empty() {
                        state.status_msg = Some("服务器与密码必填".to_string());
                    } else {
                        // 先写钥匙串：成功则落盘 TOML 不含明文密码（加载时自动回填）；
                        // 失败则明文落盘并显式告警。内存态始终保留真实密码供重连。
                        let mut disk_cfg = cfg.clone();
                        let mut keyring_stored = false;
                        let mut warn_msg: Option<String> = None;
                        let cname = cfg.name.as_deref().unwrap_or("").to_string();
                        if state.save_to_keyring {
                            match config_store::write_password_to_keyring(
                                &cfg.server,
                                &cname,
                                &cfg.password,
                            ) {
                                Ok(()) => {
                                    keyring_stored = true;
                                    disk_cfg.password.clear();
                                }
                                Err(e) => {
                                    warn_msg =
                                        Some(format!("钥匙串写入失败（密码已明文落盘）: {e}"));
                                }
                            }
                        } else {
                            config_store::delete_password_from_keyring(&cfg.server, &cname);
                        }
                        match config_store::save_to_default_path(&disk_cfg) {
                            Ok(()) => {
                                let base = if let Some(w) = warn_msg {
                                    format!("{w}，正在重连…")
                                } else if keyring_stored {
                                    "已保存（密码存于钥匙串），正在重连…".to_string()
                                } else {
                                    "已保存，正在重连…".to_string()
                                };
                                state.status_msg = Some(base);
                                // 更新内存态并触发控制链路重载（无需重启）
                                app_state.set_config(Some(cfg));
                                app_state
                                    .reconnect_requested
                                    .store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                            Err(e) => state.status_msg = Some(format!("保存失败: {e}")),
                        }
                    }
                }
            });
            if !can_save {
                ui.label(
                    egui::RichText::new("请填写服务器与密码后保存")
                        .weak()
                        .small(),
                );
            }
            if let Some(msg) = state.status_msg.clone() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(msg).small());
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "配置路径: {}",
                    config_store::default_path().display()
                ))
                .weak()
                .small(),
            );
        });
}

/// 启用无人值守被控端前的**必读风险**（纯文案常量，机械锁见本文件单测）。
///
/// 这不是装饰：这几种后果用户无法从界面上自行推断，而每一条都是**不可逆或难以察觉**的
/// （接入被静默拒掉、本窗口悄悄失去能力、一条本机提权原语、SAS 注入不了、token 要手工写）。
/// 所以它们必须原样出现在按钮旁边，而不是塞进文档等人去翻。
const RISK_NOTES: &str = "\
⚠ 启用前必读

1) 「每次询问（Confirm）」在无人值守下等于拒掉一切：服务实例没有人能点确认弹窗，每个接入请求
   都会在 45 秒后超时被拒（remote 的 APPROVAL_TIMEOUT）。要无人值守可用，只能在 host.toml 里写
   approval = \"Auto\"——注意它对任何对端都自动放行，没有「只放行名单里的对端」这种模式。

2) 服务运行期间本窗口转为被动：它不再注册，因而失去远程控制能力、连接状态与接入确认弹窗；
   状态页与托盘上显示的都不再代表实际被控状态。

3) 服务实例以 SYSTEM 完整性注入输入——这正是它能驱动登录界面与 UAC 同意对话框的原因，但同一
   能力也构成一条「管理员 → SYSTEM」的本机提权原语。并且在代码里无法区分「驱动 UAC 同意对话框」
   与「驱动登录界面」：两者都在安全桌面上。

4) SAS（Ctrl+Alt+Del）无法被合成注入，所以别指望完全接管登录流程。

5) host.toml 含服务端全局 token，必须由运维手工写入（本程序不生成它）；路径见上方「配置文件」。";

/// 「设置」Tab 的「无人值守被控端」一节（**独立于 [`show`]**：那已经背着
/// `too_many_lines` 豁免，再往里塞只会更糟）。
///
/// 调用方在 [`show`] 之后调用它。它显示服务状态与身份归属、给出装/卸/启/停按钮，并把
/// [`RISK_NOTES`] 原样摆在按钮旁边——用户不能对这些后果感到意外。
#[allow(
    clippy::too_many_lines,
    reason = "服务一节：状态 + 按钮 + 必读风险，拆散会割裂「看懂后果」这条主线"
)]
pub fn show_service_section(ui: &mut Ui, app_state: &Arc<AppState>, service: &mut ServiceOps) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.heading("无人值守被控端（Windows 服务）");
            ui.add_space(4.0);
            if !SERVICE_SUPPORTED {
                // 非 Windows 上这一整套都不成立：没有 Windows 服务、没有安全桌面、没有 UAC。
                ui.label(
                    egui::RichText::new(
                        "此功能仅在 Windows 上可用：它靠一个以 LocalSystem 运行的 Windows 服务，\
                         把被控实例拉进控制台会话，从而覆盖登录界面与提权窗口（如任务管理器）。",
                    )
                    .weak(),
                );
                return;
            }

            let presence = service.presence();
            egui::Grid::new("service_grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("服务状态").weak().small());
                    ui.label(presence_label(presence));
                    ui.end_row();
                    ui.label(egui::RichText::new("客户端身份").weak().small());
                    ui.label(owner_label(app_state.service_owner()));
                    ui.end_row();
                    ui.label(egui::RichText::new("配置文件").weak().small());
                    ui.label(
                        egui::RichText::new(host_config_path_display())
                            .monospace()
                            .small(),
                    );
                    ui.end_row();
                });

            ui.add_space(6.0);
            let busy = service.in_flight();
            // `Absent` = 确定没装；其余（含 `Unknown`）按「已装」处理，多给几个按钮总比
            // 在查询偶发失败时把「卸载」藏起来强。
            let installed = presence != ServicePresence::Absent;
            // 三态：只有**确定**缺失才拦（`Unknown` 放行——本窗口多半读不到那个目录，
            // 见 `HostConfigPresence`）。
            let config_presence = host_config_presence();
            let blocked = start_blocked_reason(config_presence);
            let advisory = host_config_advisory(config_presence);
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(busy.is_none() && !installed, |ui| {
                    if ui.button("安装服务").clicked() {
                        service.spawn_op(ServiceOp::Install);
                    }
                });
                // 「启动」在 host.toml **确定**缺失时禁用：服务一起来本窗口就转被动，
                // 而实例读不到配置也注册不了——整机就此失去身份（见 start_blocked_reason）。
                // `Unknown` 放行：读不到目录不等于文件不存在（见 HostConfigPresence）。
                ui.add_enabled_ui(busy.is_none() && installed && blocked.is_none(), |ui| {
                    if ui.button("启动服务").clicked() {
                        service.spawn_op(ServiceOp::Start);
                    }
                });
                ui.add_enabled_ui(busy.is_none() && installed, |ui| {
                    if ui.button("停止服务").clicked() {
                        service.spawn_op(ServiceOp::Stop);
                    }
                    if ui.button("卸载服务").clicked() {
                        service.spawn_op(ServiceOp::Uninstall);
                    }
                });
            });
            if let Some(reason) = blocked {
                ui.add_space(2.0);
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    egui::RichText::new(reason).small(),
                );
            }
            // `Unknown` 的补充说明是**信息**不是警告：它是「本窗口读不到那个目录」这件事
            // 的解释，不是「你做错了什么」。故用弱色而非警告色。
            if let Some(note) = advisory {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(note).weak().small());
            }
            if let Some(op) = busy {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("正在{}…（界面保持可用，完成后结果显示在这里）", op.label()));
                });
            }
            if let Some(message) = service.last_message() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(message).small());
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.colored_label(
                ui.visuals().warn_fg_color,
                egui::RichText::new("⚠ 启用前必读").strong(),
            );
            ui.add_space(2.0);
            ui.label(egui::RichText::new(RISK_NOTES).small());
        });
}

#[cfg(test)]
mod tests {
    //! 纯文案的机械锁：这五条风险是**要求的内容**，删掉或改淡都是回归。

    use super::RISK_NOTES;

    #[test]
    fn risk_notes_state_every_required_consequence() {
        for needle in [
            // 1) Confirm 在无人值守下拒掉一切，超时值 45s。
            "45",
            "Auto",
            "Confirm",
            // 2) 本窗口失去远程控制能力/连接状态/确认弹窗。
            "失去远程控制能力",
            "不再注册",
            // 3) SYSTEM 完整性 = 管理员→SYSTEM 提权原语，且与登录界面不可区分。
            "SYSTEM",
            "提权原语",
            "无法区分",
            // 4) SAS 注入不了。
            "Ctrl+Alt+Del",
            // 5) host.toml 含全局 token、要手工写。
            "host.toml",
            "全局 token",
            "手工写入",
        ] {
            assert!(RISK_NOTES.contains(needle), "风险说明里缺 {needle}");
        }
    }
}
