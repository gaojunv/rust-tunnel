//! 设置 Tab（服务器/密码/token 等，落盘到默认 TOML + 可选钥匙串）。

use egui::Ui;

use rust_tunnel_client::ClientConfig;

use crate::config_store;

#[derive(Debug, Clone)]
pub struct SettingsState {
    pub server: String,
    pub name: String,
    pub password: String,
    pub save_to_keyring: bool,
    pub tls: bool,
    pub tls_insecure: bool,
    pub status_msg: Option<String>,
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
            status_msg: None,
        }
    }
}

impl SettingsState {
    pub fn from_config(cfg: &ClientConfig) -> Self {
        Self {
            server: cfg.server.clone(),
            name: cfg.name.clone().unwrap_or_default(),
            password: cfg.password.clone(),
            save_to_keyring: false,
            tls: cfg.tls,
            tls_insecure: cfg.tls_insecure,
            status_msg: None,
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
        cfg
    }
}

pub fn show(ui: &mut Ui, state: &mut SettingsState, current_config: Option<&ClientConfig>) {
    ui.heading("设置");
    ui.separator();
    ui.label("服务器地址（host:port）");
    ui.text_edit_singleline(&mut state.server);
    ui.label("客户端名（留空则用系统 hostname）");
    ui.text_edit_singleline(&mut state.name);
    ui.label("密码 / Token");
    ui.text_edit_singleline(&mut state.password);
    ui.checkbox(&mut state.save_to_keyring, "存入系统钥匙串");
    ui.checkbox(&mut state.tls, "启用 TLS");
    ui.checkbox(&mut state.tls_insecure, "跳过证书校验（TOFU）");
    ui.separator();
    if ui.button("保存").clicked() {
        let cfg = state.to_config(current_config);
        match config_store::save_to_default_path(&cfg) {
            Ok(()) => {
                if state.save_to_keyring && !cfg.password.is_empty() {
                    let cname = cfg.name.as_deref().unwrap_or("");
                    if let Err(e) = config_store::write_password_to_keyring(&cfg.server, cname, &cfg.password) {
                        state.status_msg = Some(format!("配置已保存，但钥匙串写入失败：{e}"));
                    } else {
                        state.status_msg = Some("已保存（密码已写入钥匙串）".to_string());
                    }
                } else {
                    state.status_msg = Some(format!("已保存到 {}", config_store::default_path().display()));
                }
            }
            Err(e) => state.status_msg = Some(format!("保存失败：{e}")),
        }
    }
    if let Some(msg) = &state.status_msg {
        ui.label(msg.as_str());
    }
    ui.separator();
    ui.label("修改后需重启客户端生效（或点托盘重连）。");
}
