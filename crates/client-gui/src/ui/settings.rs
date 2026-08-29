use std::sync::Arc;

use egui::Ui;

use rust_tunnel_client::ClientConfig;

use crate::{app::AppState, config_store};

#[allow(clippy::struct_excessive_bools, reason = "设置面板布尔开关组，拆为 enum 反而割裂布局")]
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
            remember_password: true,
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
            remember_password: true,
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
            ui.checkbox(&mut state.enable_agent, "启用 Agent 执行器（远程命令/文件操作）");
            ui.add_space(10.0);
            let can_save = !state.server.trim().is_empty() && !state.password.trim().is_empty();
            ui.add_enabled_ui(can_save, |ui| {
                if ui.button("保存并重连").clicked() {
                    let current = app_state.config_snapshot();
                    let mut cfg = state.to_config(current.as_ref());
                    if cfg.server.trim().is_empty() || cfg.password.trim().is_empty() {
                        state.status_msg = Some("服务器与密码必填".to_string());
                    } else {
                        match config_store::save_to_default_path(&cfg) {
                            Ok(()) => {
                                if state.save_to_keyring && !cfg.password.is_empty() {
                                    let cname = cfg.name.as_deref().unwrap_or("");
                                    if let Err(e) = config_store::write_password_to_keyring(
                                        &cfg.server, cname, &cfg.password,
                                    ) {
                                        state.status_msg =
                                            Some(format!("配置已保存，但钥匙串失败: {e}"));
                                    } else {
                                        state.status_msg =
                                            Some("已保存并已写入钥匙串，正在重连…".to_string());
                                    }
                                } else if !state.save_to_keyring {
                                    let cname = cfg.name.as_deref().unwrap_or("");
                                    config_store::delete_password_from_keyring(
                                        &cfg.server, cname,
                                    );
                                    state.status_msg =
                                        Some("已保存，正在重连…".to_string());
                                } else {
                                    state.status_msg =
                                        Some(format!("已保存到 {}", config_store::default_path().display()));
                                }
                                // 更新内存态并触发控制链路重载（无需重启）
                                cfg.password.clone_from(&state.password);
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
                ui.label(egui::RichText::new("请填写服务器与密码后保存").weak().small());
            }
            if let Some(msg) = state.status_msg.clone() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(msg).small());
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("配置路径: {}", config_store::default_path().display()))
                    .weak()
                    .small(),
            );
        });
}
