//! rust-tunnel 桌面托盘客户端（winit + tray-icon + eframe）。

#![allow(clippy::missing_docs_in_private_items, reason = "GUI 二进制")]

mod app;
mod autostart;
mod config_store;
mod fonts;
mod notify;
mod tray;
mod ui;

use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use eframe::egui;

use rust_tunnel_client::{ClientConfig, ClientStatus, LogBuffer, ReconnectPolicy};

struct GuiApp {
    app_state: Arc<app::AppState>,
    settings: ui::settings::SettingsState,
    selected_tab: usize,
}

impl GuiApp {
    fn new(app_state: Arc<app::AppState>) -> Self {
        let settings = app_state
            .config_snapshot()
            .as_ref()
            .map(ui::settings::SettingsState::from_config)
            .unwrap_or_default();
        Self {
            app_state,
            settings,
            selected_tab: 0,
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 关闭拦截：点 × 仅隐藏到托盘，不退出进程；仅托盘"退出"置 should_exit 后才真正关闭
        if ctx.input(|i| i.viewport().close_requested()) {
            let should_exit = self
                .app_state
                .should_exit
                .load(std::sync::atomic::Ordering::SeqCst);
            if !should_exit {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                return;
            }
        }
        for action in tray::poll_menu_actions() {
            match action {
                tray::TrayAction::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.selected_tab = 0;
                }
                tray::TrayAction::Settings => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.selected_tab = 2;
                }
                tray::TrayAction::Reconnect => {
                    self.app_state
                        .reconnect_requested
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                tray::TrayAction::Quit => {
                    self.app_state
                        .should_exit
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        let status = self.app_state.status_rx.borrow().clone();

        egui::TopBottomPanel::top("tabs")
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    for (idx, label) in ["状态", "映射", "设置", "日志"].iter().enumerate() {
                        let sel = self.selected_tab == idx;
                        if ui.selectable_label(sel, *label).clicked() {
                            self.selected_tab = idx;
                        }
                    }
                });
                ui.add_space(2.0);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(10.0);
            match self.selected_tab {
                0 => ui::status::show(ui, &status),
                1 => ui::mappings::show(ui, &status),
                2 => ui::settings::show(ui, &mut self.settings, &self.app_state),
                3 => ui::logs::show(ui, &self.app_state.log_buffer),
                _ => {}
            }
        });

        let needs_repaint = !status.connected || status.last_error.is_some();
        if needs_repaint {
            ctx.request_repaint_after(Duration::from_millis(300));
        } else {
            ctx.request_repaint_after(Duration::from_millis(600));
        }
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::map_unwrap_or,
    reason = "GUI 托盘启动编排，一次性初始化链路拆分无益"
)]
fn spawn_runtime(
    app_state: Arc<app::AppState>,
    status_tx: Arc<tokio::sync::watch::Sender<ClientStatus>>,
    log_buffer: Arc<LogBuffer>,
    reconnect_flag: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("无法创建 tokio 运行时: {e}");
                return;
            }
        };
        // GUI 初始化：先把磁盘配置写入 AppState.config 供状态面板"服务器/客户端名"即时回显
        fn fill_keyring(c: &mut ClientConfig) {
            if c.password.is_empty() {
                let cname = c.name.as_deref().unwrap_or("");
                if let Some(pw) = config_store::read_password_from_keyring(&c.server, cname) {
                    c.password = pw;
                }
            }
        }
        let mut bootstrap = config_store::load_from_default_path()
            .or_else(|| ClientConfig::load().ok());
        if let Some(ref mut c) = bootstrap {
            fill_keyring(c);
            app_state.set_config(Some(c.clone()));
            let mut s = app_state.status_rx.borrow().clone();
            s.server.clone_from(&c.server);
            s.client_name = c.name.clone().unwrap_or_default();
            s.version = env!("CARGO_PKG_VERSION").to_string();
            let _ = app_state.status_tx.send(s);
        }
        // GUI 全局日志初始化：立即可见（否则日志面板在首次连接前为空）
        {
            use rust_tunnel_client::{logs::ClientLogLayer, log_buffer::LogBuffer as LB};
            use rust_tunnel_common::init_logging_with_layer;
            let lb: Arc<LB> = log_buffer.clone();
            let layer = ClientLogLayer::new();
            layer.set_log_buffer(lb);
            // 用当前 log 级别（若有配置否则 info）初始化；重复 init 内部 try_init 会静默忽略
            let lvl = bootstrap.as_ref().map(|c| c.log.as_str()).unwrap_or("info");
            init_logging_with_layer(lvl, layer);
                    tracing::info!("GUI 已启动");
                    tracing::info!("配置路径: {}", config_store::default_path().display());
                }
                rt.block_on(async move {
            // 初始配置：优先 GUI 默认路径，其次 CLI env/TOML
            let mut config = if let Some(c) = config_store::load_from_default_path() {
                c
            } else {
                match ClientConfig::load() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("no config: {e}; waiting for settings");
                        // 无配置时保持离线，等待用户在设置 Tab 保存
                        loop {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            if reconnect_flag.load(std::sync::atomic::Ordering::SeqCst) {
                                reconnect_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                                if let Some(c) = config_store::load_from_default_path() {
                                    break c;
                                }
                                if let Ok(c) = ClientConfig::load() {
                                    break c;
                                }
                            }
                        }
                    }
                }
            };

            // 钥匙串回填：若密码为空且钥匙串有值则回填
            if config.password.is_empty() {
                let cname = config.name.as_deref().unwrap_or("");
                if let Some(pw) = config_store::read_password_from_keyring(&config.server, cname) {
                    config.password = pw;
                }
            }

            let mut policy = ReconnectPolicy::new();
            let mut first_success = true;

            loop {
                let cfg = config.clone();
                let tx = status_tx.clone();
                let lb = log_buffer.clone();
                let res = rust_tunnel_client::control::run_client_with_status(cfg, Some(tx), Some(lb)).await;

                match res {
                    Ok(()) => {
                        policy.reset();
                        tracing::warn!("控制通道已关闭，准备重连");
                    }
                    Err(e) if !ReconnectPolicy::should_reconnect(&e) => {
                        tracing::error!("注册被拒（不可重连）：{e}");
                        notify::send("rust-tunnel", &format!("注册被拒：{e}"));
                        // 标记错误到 status 供面板展示
                        let mut s = app_state.status_rx.borrow().clone();
                        s.last_error = Some(e.to_string());
                        let _ = app_state.status_tx.send(s);
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("连接错误：{e}");
                        let mut s = app_state.status_rx.borrow().clone();
                        s.connected = false;
                        s.last_error = Some(e.to_string());
                        let _ = app_state.status_tx.send(s);
                        if first_success {
                            notify::send("rust-tunnel", &format!("已断开：{e}"));
                        }
                    }
                }

                first_success = false;
                let backoff = policy.next_backoff();
                tracing::info!("{backoff}s 后重连…");

                // 可中断的退避等待：托盘“重连”立即打断
                let mut waited = 0u64;
                while waited < backoff {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if reconnect_flag.load(std::sync::atomic::Ordering::SeqCst) {
                        reconnect_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                        if let Some(c) = config_store::load_from_default_path().or_else(|| ClientConfig::load().ok()) {
                            // 钥匙串回填
                            let mut c2 = c;
                            if c2.password.is_empty() {
                                let cn = c2.name.as_deref().unwrap_or("");
                                if let Some(pw) = config_store::read_password_from_keyring(&c2.server, cn) {
                                    c2.password = pw;
                                }
                            }
                            config = c2;
                        }
                        policy.reset();
                        break;
                    }
                    waited += 1;
                }
            }
        });
    });
}

fn main() -> anyhow::Result<()> {
    // 无桌面环境时退化为提示（供容器/CI 体检）
    let has_display = std::env::var("DISPLAY").is_ok()
        || std::env::var("WAYLAND_DISPLAY").is_ok()
        || cfg!(target_os = "macos")
        || cfg!(target_os = "windows");
    if !has_display {
        eprintln!("未检测到桌面环境（DISPLAY/WAYLAND_DISPLAY），请在桌面系统运行或使用 rust-tunnel-client。");
        // 仍允许 cargo check 体检通过，运行时再退出
        if std::env::var("RUST_TUNNEL_GUI_HEADLESS_OK").is_err() {
            std::process::exit(0);
        }
    }

    let initial_status = ClientStatus::new(String::new(), String::new(), String::new());
    let app_state = Arc::new(app::AppState::new(initial_status));
    let status_tx = app_state.status_tx.clone();
    let log_buffer = app_state.log_buffer.clone();
    let reconnect_flag = app_state.reconnect_requested.clone();

    // 后台 tokio 运行时（控制通道/隧道）
    spawn_runtime(
        app_state.clone(),
        status_tx,
        log_buffer,
        reconnect_flag,
    );

    // 托盘在 eframe 初始化前需有事件循环；tray-icon 内部自行处理跨平台事件循环差异，
    // 此处先构建托盘（持有生命周期），再起 eframe。
    let _tray: Option<tray_icon::TrayIcon> = match tray::build_tray() {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!("托盘初始化失败（无桌面环境时可忽略）：{e}");
            if std::env::var("RUST_TUNNEL_GUI_HEADLESS_OK").is_ok() {
                None
            } else {
                return Err(anyhow::anyhow!("托盘初始化失败: {e}"));
            }
        }
    };

    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([480.0, 520.0])
        .with_min_inner_size([380.0, 400.0])
        .with_visible(true);

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Rust Tunnel",
        options,
        Box::new(move |cc| {
            fonts::setup_fonts(&cc.egui_ctx);
            Ok(Box::new(GuiApp::new(app_state)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(())
}
