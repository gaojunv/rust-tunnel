//! rust-tunnel 桌面托盘客户端（winit + tray-icon + eframe）。

#![allow(clippy::missing_docs_in_private_items, reason = "GUI 二进制")]
// 单测豁免（与各 lib crate 的 `lib.rs` 头部一致）：restriction 组在测试里放宽。
// 本 crate 是 bin，没有 lib.rs，故这行放在这里。
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
// Release 构建在 Windows 上隐藏控制台窗口（双击启动仅显示 GUI）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod config_store;
mod fonts;
mod instance;
mod notify;
mod prefs;
mod remote;
mod service_ui;
mod service_watch;
mod tray;
mod ui;

use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;

use rust_tunnel_client::{ClientConfig, ClientStatus, LogBuffer, ReconnectPolicy};
use rust_tunnel_remote::{ApprovalPolicy, EncodeOptions, RemoteManager, RemoteManagerConfig, SessionRole};

use service_watch::IdentityOwner;

pub(crate) fn load_window_icon() -> Option<std::sync::Arc<egui::IconData>> {
    let bytes: &[u8] = include_bytes!("../icons/icon.png");
    let Some((rgba, w, h)) = tray::load_icon_rgba(bytes) else {
        tracing::warn!("窗口图标解码失败，回退为系统默认图标");
        return None;
    };
    Some(std::sync::Arc::new(egui::IconData {
        rgba,
        width: w,
        height: h,
    }))
}
struct GuiApp {
    app_state: Arc<app::AppState>,
    settings: ui::settings::SettingsState,
    selected_tab: usize,
    /// 托盘句柄（保持 `TrayIcon` 生命周期，随窗口同生共死）。
    tray: Option<tray::TrayHandle>,
    /// 上次已写入托盘的文案签名（首行/服务器行/tooltip/服务行），逐字段比较避免每帧写托盘。
    last_tray_texts: Option<(String, String, String, String)>,
    /// GUI 偏好（主题等），切换时落盘 gui.toml。
    prefs: prefs::GuiPrefs,
    /// 远程会话窗口的 GUI 侧状态。**共享**给独立会话窗口的回调（它可能在别的线程执行），
    /// 故用 `Arc<Mutex<..>>`；临界区必须短，且绝不在持锁时注册 deferred viewport。
    remote_view: Arc<Mutex<remote::RemoteView>>,
    /// 接入确认弹窗的「本次运行内不再询问该对端」勾选态。放在这里而非 `RemoteView`：弹窗每帧都要
    /// 可写，而视图已共享给子窗口回调，不该再承载与画面无关的弹窗状态。
    approval_remember: bool,
    /// 上一帧待确认请求的会话 ID。用于检测 `None → Some` 跳变，仅在**首次出现**时
    /// 强制显示并聚焦窗口（见 `update`），避免每帧与用户抢焦点。
    last_pending_approval: Option<String>,
    /// 无人值守被控端的操作与状态（在途操作 / 最近结果 / 服务状态缓存），跨帧存活。
    service_ui: service_ui::ServiceOps,
}

impl GuiApp {
    fn new(
        app_state: Arc<app::AppState>,
        tray: Option<tray::TrayHandle>,
        prefs: prefs::GuiPrefs,
    ) -> Self {
        let settings = app_state
            .config_snapshot()
            .as_ref()
            .map(ui::settings::SettingsState::from_config)
            .unwrap_or_default();
        let remote_view = Arc::new(Mutex::new(remote::RemoteView::from_prefs(&prefs)));
        Self {
            app_state,
            settings,
            selected_tab: 0,
            tray,
            last_tray_texts: None,
            prefs,
            remote_view,
            approval_remember: false,
            last_pending_approval: None,
            service_ui: service_ui::ServiceOps::default(),
        }
    }

    /// 应用「允许被远程控制」的新状态：同步到引擎、托盘勾选与偏好文件。
    ///
    /// 托盘与 UI 两处入口都收敛到这里，避免两处各写一份导致引擎/托盘脱节。
    fn apply_allow_remote(&mut self, on: bool) {
        self.prefs.allow_remote = on;
        if let Some(manager) = self.app_state.remote() {
            manager.set_enabled(on);
        }
        if let Some(handle) = self.tray.as_ref() {
            handle.allow_remote_item.set_checked(on);
        }
        if let Err(e) = prefs::save(&self.prefs) {
            tracing::warn!("被控开关保存失败：{e}");
        }
        tracing::info!("允许被远程控制：{}", if on { "开" } else { "关" });
    }

    /// 应用接入确认策略的新状态：同步到引擎并落盘 gui.toml。
    fn apply_approval_policy(&mut self, policy: ApprovalPolicy) {
        self.prefs.remote_approval = policy;
        if let Some(manager) = self.app_state.remote() {
            manager.set_approval_policy(policy);
        }
        if let Err(e) = prefs::save(&self.prefs) {
            tracing::warn!("接入确认策略保存失败：{e}");
        }
        tracing::info!(
            "接入确认策略：{}",
            match policy {
                ApprovalPolicy::Auto => "自动接受",
                ApprovalPolicy::Confirm => "需要确认",
            }
        );
    }
}

/// 应用主题偏好：egui 绘制 + 原生标题栏外观。
fn apply_theme(ctx: &egui::Context, pref: prefs::ThemePref) {
    ctx.set_theme(pref.to_egui());
    let native = match pref {
        prefs::ThemePref::System => egui::SystemTheme::SystemDefault,
        prefs::ThemePref::Light => egui::SystemTheme::Light,
        prefs::ThemePref::Dark => egui::SystemTheme::Dark,
    };
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(native));
}

impl GuiApp {
    /// 关闭拦截：点 × 仅隐藏到托盘，不退出进程；仅托盘"退出"置 should_exit 后才真正关闭。
    /// 返回 true 表示已消费关闭请求，调用方直接 return。
    fn intercept_close(&self, ctx: &egui::Context) -> bool {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return false;
        }
        let should_exit = self
            .app_state
            .should_exit
            .load(std::sync::atomic::Ordering::SeqCst);
        if should_exit {
            return false;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        true
    }

    /// 消费托盘菜单动作队列并执行对应窗口/状态操作。
    fn drain_tray_actions(&mut self, ctx: &egui::Context) {
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
                    // 「设置」Tab 下标（见 show_tabs 的标签数组：状态/映射/远程/设置/日志）。
                    self.selected_tab = 3;
                }
                tray::TrayAction::Service => {
                    // 与 `Settings` 同落地（设置就是那个 Tab）；菜单里分成两项只为让语义对上
                    // 用户意图——「服务设置…」直接把用户带到无人值守被控端那一节。
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.selected_tab = 3;
                }
                tray::TrayAction::Reconnect => {
                    self.app_state
                        .reconnect_requested
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                tray::TrayAction::ToggleAllowRemote => {
                    // 从 muda 读翻转后的新状态（不维护镜像，见 TrayHandle 文档）。
                    let enabled = self
                        .tray
                        .as_ref()
                        .is_some_and(|h| h.allow_remote_item.is_checked());
                    // 收敛入口：同步引擎（set_enabled）+ 偏好文件。
                    self.apply_allow_remote(enabled);
                }
                tray::TrayAction::Quit => {
                    self.app_state
                        .should_exit
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// 托盘状态菜单/Tooltip 随连接态刷新（逐字段比较，仅写入变化字段）。
    ///
    /// `pending_approval` 为真时覆盖 tooltip：此时窗口可能已隐藏到托盘，tooltip 是用户
    /// 唯一能看到的线索，提示有接入请求待确认。
    fn refresh_tray(&mut self, status: &ClientStatus, pending_approval: bool) {
        // 服务接管身份时托盘不能再说「已连接/离线」——本窗口根本没注册，那两句话都是假的。
        // 归属直接从 `AppState` 读（由 `service_watch` 的轮询任务持续刷新）。
        let owner = self.app_state.service_owner();
        let tooltip = if pending_approval {
            "rust-tunnel — 有远程接入请求待确认".to_owned()
        } else {
            tray::tooltip_text_with_service(status, owner)
        };
        // 服务状态行（只读项）：装/没装、跑/没跑由它给出（1 秒缓存，见 `service_ui::ServiceOps`）。
        let service_text = service_ui::service_menu_text(self.service_ui.presence());
        let sig = (
            tray::status_menu_text_with_service(status, owner),
            tray::server_menu_text(status),
            tooltip,
            service_text,
        );
        if self.last_tray_texts.as_ref() == Some(&sig) {
            return;
        }
        if let Some(handle) = self.tray.as_ref() {
            let prev = self.last_tray_texts.as_ref();
            if prev.map(|p| &p.0) != Some(&sig.0) {
                handle.status_item.set_text(&sig.0);
            }
            if prev.map(|p| &p.1) != Some(&sig.1) {
                handle.server_item.set_text(&sig.1);
            }
            if prev.map(|p| &p.3) != Some(&sig.3) {
                handle.service_item.set_text(&sig.3);
            }
            if prev.map(|p| &p.2) != Some(&sig.2) {
                if let Err(e) = handle.icon.set_tooltip(Some(sig.2.clone())) {
                    tracing::warn!("托盘 tooltip 更新失败：{e}");
                }
            }
        }
        self.last_tray_texts = Some(sig);
    }

    /// Tab 栏右侧的主题切换按钮（循环三态，落盘 gui.toml）。
    fn show_theme_button(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let cur = self.prefs.theme;
            let effective = match ui.ctx().theme() {
                egui::Theme::Dark => "深色",
                egui::Theme::Light => "浅色",
            };
            let btn = ui
                .small_button(format!("{} {}", cur.icon(), cur.label()))
                .on_hover_text(format!(
                    "切换主题（当前生效：{effective}，偏好：{}）",
                    cur.label()
                ));
            if btn.clicked() {
                let next = cur.next();
                self.prefs.theme = next;
                apply_theme(ui.ctx(), next);
                if let Err(e) = prefs::save(&self.prefs) {
                    tracing::warn!("GUI 偏好保存失败：{e}");
                }
            }
        });
    }

    /// 顶部 Tab 栏（含右侧主题按钮）。
    fn show_tabs(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("tabs")
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    for (idx, label) in ["状态", "映射", "远程", "设置", "日志"].iter().enumerate()
                    {
                        let sel = self.selected_tab == idx;
                        if ui.selectable_label(sel, *label).clicked() {
                            self.selected_tab = idx;
                        }
                    }
                    self.show_theme_button(ui);
                });
                ui.add_space(2.0);
            });
    }
    /// 把工具条里改过的展示档位（缩放 / 窗口尺寸 / 角标 / 剪贴板同步）回写 `gui.toml`。
    ///
    /// 这些字段在 [`remote::RemoteView`] 上是运行时镜像——改动发生在 deferred viewport 的回调里
    /// （与 `update` 不在同一时刻），只能靠每帧比对把差异落盘。`save` 只在确有变化时才写。
    fn sync_display_prefs(&mut self) {
        let view = self
            .remote_view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut changed = false;
        if self.prefs.remote_view_scale != view.scale {
            self.prefs.remote_view_scale = view.scale;
            changed = true;
        }
        if self.prefs.remote_window_size != view.window_size {
            self.prefs.remote_window_size = view.window_size;
            changed = true;
        }
        if self.prefs.remote_stats_overlay != view.stats_overlay {
            self.prefs.remote_stats_overlay = view.stats_overlay;
            changed = true;
        }
        if self.prefs.remote_clipboard_sync != view.clipboard_sync_enabled {
            self.prefs.remote_clipboard_sync = view.clipboard_sync_enabled;
            changed = true;
        }
        drop(view);
        if changed {
            if let Err(e) = prefs::save(&self.prefs) {
                tracing::warn!("GUI 偏好保存失败：{e}");
            }
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.intercept_close(ctx) {
            return;
        }
        self.drain_tray_actions(ctx);
        // 抽干服务操作的结果通道：一次 --install 要跑约二十秒，结果只能在后续帧里看到。
        self.service_ui.poll();

        let status = self.app_state.status_rx.borrow().clone();

        // 远程引擎就绪后安装帧通道（幂等）；引擎由后台运行时线程注入 AppState。
        let remote_manager = self.app_state.remote();
        // 一帧只取一次远程状态快照：托盘 tooltip、会话生命周期收敛与重连都基于它。
        let remote_state = remote_manager.as_ref().map(|m| m.snapshot());
        let pending_id = remote_state
            .as_ref()
            .and_then(|s| s.pending_approval.as_ref().map(|p| p.session_id.clone()));
        self.refresh_tray(&status, pending_id.is_some());
        self.show_tabs(ctx);

        // 待确认请求首次出现：窗口可能已隐藏到托盘（见 `intercept_close`），安全提示绝不能
        // 静默超时，故强制显示并聚焦。仅在 None→Some 跳变时做，避免每帧与用户抢焦点。
        if pending_id.is_some() && self.last_pending_approval != pending_id {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.last_pending_approval = pending_id;

        // 远程会话的每帧收敛（父窗口负责，因为它必须每帧都跑，而子窗口回调只在窗口存在时才跑）：
        // 安装帧通道、开窗 / 关窗 / 断线重连、剪贴板同步、帧率结算。
        if let Some(manager) = remote_manager.as_ref() {
            let now = Instant::now();
            let mut view = self
                .remote_view
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            view.ensure_sink(ctx, manager);
            if let Some(state) = remote_state.as_ref() {
                view.reconcile(manager, state, self.prefs.remote_auto_reconnect, now);
                let active_controller = state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.role == SessionRole::Controller);
                view.poll_clipboard(manager, active_controller, now);
            }
            view.poll_fps(now);
        }

        let mut remote_actions = ui::remote::RemoteTabActions::default();
        let settings = &mut self.settings;
        let service_ui = &mut self.service_ui;
        let remote_view = Arc::clone(&self.remote_view);
        let app_state = &self.app_state;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(10.0);
            // 被控横幅常驻所有 Tab 顶部：无论用户切到哪个页都能看到并随时中断。
            ui::remote::controlled_banner(ui, remote_manager.as_ref());
            match self.selected_tab {
                0 => ui::status::show(ui, &status, app_state.service_owner()),
                1 => ui::mappings::show(ui, &status),
                2 => {
                    remote_actions = ui::remote::show(ui, &remote_view, remote_manager.as_ref(), app_state);
                }
                3 => {
                    ui::settings::show(ui, settings, app_state);
                    // 「无人值守被控端」一节独立于 `show`（那已背着 too_many_lines 豁免）。
                    ui::settings::show_service_section(ui, app_state, service_ui);
                }
                4 => ui::logs::show(ui, &app_state.log_buffer),
                _ => {}
            }
        });
        if let Some(on) = remote_actions.allow_remote {
            self.apply_allow_remote(on);
        }
        if let Some(policy) = remote_actions.approval_policy {
            self.apply_approval_policy(policy);
        }
        // 接入确认弹窗常驻所有 Tab 之上（与 `controlled_banner` 同理）。
        ui::remote::approval_modal(ctx, &mut self.approval_remember, remote_manager.as_ref());

        // 独立会话窗口本体（仅控制器会话且窗口应存在时注册）。
        if let Some(manager) = remote_manager.as_ref() {
            ui::session_window::show(ctx, &self.remote_view, manager, &self.prefs);
        }

        self.sync_display_prefs();

        // 服务操作在途时也要勤刷：结果要靠 `poll` 才被看到（在途期间按钮已禁用）。
        let service_busy = self.service_ui.in_flight().is_some();
        let needs_repaint = !status.connected || status.last_error.is_some() || service_busy;
        if needs_repaint {
            ctx.request_repaint_after(Duration::from_millis(300));
        } else {
            ctx.request_repaint_after(Duration::from_millis(600));
        }
    }
}

/// 构造远程控制引擎的选项。
///
/// GUI 与无头实例（`--instance`）的开关、平台实现与目录来源都不同，故集中在这里传入，
/// 免得 `build_remote_manager` 的形参表越拉越长。
struct ManagerOptions {
    /// 初始是否允许被远程控制。
    enabled: bool,
    /// 接入确认策略。
    approval_policy: ApprovalPolicy,
    /// 平台实现模式（**必须在构造期选定**，见 [`remote::PlatformMode`]）。
    platform_mode: remote::PlatformMode,
    /// 被控端共享目录（`None` = 不接受文件请求）。
    share_dir: Option<PathBuf>,
    /// 控制器下载文件的落盘目录。
    download_dir: PathBuf,
}

/// 构造远程控制引擎（配置就绪后调用一次）。
///
/// `displays` 传 1 只是安装平台实现前的占位——[`remote::install_platform`] 之后
/// 引擎以 `ScreenSource::displays()` 为准。`approval_policy` 取自持久化偏好（GUI）或
/// `host.toml`（实例），决定被控端是直接接受还是每次弹窗确认。
///
/// 返回引擎本身与会话收尾钩子（仅实例模式有；GUI 为 `None`）。
fn build_remote_manager(
    config: &ClientConfig,
    opts: ManagerOptions,
) -> (Arc<RemoteManager>, Option<Arc<dyn remote::SessionRelease>>) {
    let establisher = Arc::new(remote::LazyTransport::new(config.server.clone()));
    let manager = RemoteManager::new(RemoteManagerConfig {
        server_host: config.server.clone(),
        self_client_name: config.name.clone().unwrap_or_default(),
        platform: remote::platform_name().to_owned(),
        displays: 1,
        enabled: opts.enabled,
        approval_policy: opts.approval_policy,
        establisher,
        encode: EncodeOptions::default(),
        share_dir: opts.share_dir,
        download_dir: opts.download_dir,
    });
    // 平台实现在这里**一次性**选定：引擎受理会话时就把 `Arc<dyn ScreenSource>` /
    // `Arc<dyn InputSink>` 克隆走了，之后 `install_platform` 再覆盖对已建立的会话无效。
    let release = remote::install_platform(&manager, opts.platform_mode);
    (manager, release)
}

/// GUI 路径的引擎选项：共享目录固定为「下载」目录下的 `rust-tunnel-share`——可见、易找，
/// 且与用户的个人目录隔离（用户要换目录只需在这里改一处）。
fn gui_manager_options(enabled: bool, approval_policy: ApprovalPolicy) -> ManagerOptions {
    ManagerOptions {
        enabled,
        approval_policy,
        platform_mode: remote::PlatformMode::Gui,
        share_dir: Some(remote::share_dir()),
        download_dir: remote::download_dir(),
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
    initial_allow_remote: bool,
    initial_approval: ApprovalPolicy,
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
        let mut bootstrap =
            config_store::load_from_default_path().or_else(|| ClientConfig::load().ok());
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
            use rust_tunnel_client::log_buffer::LogBuffer as LB;
            use rust_tunnel_common::init_logging_with_layer;
            let lb: Arc<LB> = log_buffer.clone();
            // 必须用进程级共享 layer 注册：run_client_inner 内部复用同一实例做
            // sender 热替换（日志上报服务端）；另建实例会导致其收不到事件、
            // 且每次重连泄漏一个日志转发任务。
            let layer = rust_tunnel_client::control::shared_log_layer();
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

            // 远程控制引擎：配置就绪后构造一次，安装平台实现（截屏/注入），注入 AppState
            // 供 GUI 读取，桥接交给控制通道。构造在此（而非 main）是因为 server_host /
            // 客户端名要等磁盘配置加载完；无配置时上面的循环会先等到配置。
            let (remote_manager, _release) = build_remote_manager(
                &config,
                gui_manager_options(initial_allow_remote, initial_approval),
            );
            app_state.set_remote(Arc::clone(&remote_manager));

            // 身份归属监督：Windows 服务在跑时本进程**不注册**（服务端按客户端名唯一，
            // 同名注册会互相踢，表现为「远程控制每隔几秒掉一次」）。非 Windows 上
            // `start_watch` 恒返回 `None`，下面的循环退化为今天的「一次 run_client_loop」。
            let owner_rx = service_watch::start_watch(Arc::clone(&app_state.service_owner_tx));
            // 「是否已因服务接管而被动」：只在**进入**被动时广播一次状态。UI 的 watch 接收端
            // 会保留最后一次值，一次就够——每 2 秒 tick 都 send 只会白白刷屏。
            let mut passive = false;
            loop {
                let owner = owner_rx.as_ref().map_or(IdentityOwner::Gui, |rx| *rx.borrow());
                if owner.is_service() {
                    if !passive {
                        publish_passive_status(&status_tx);
                        passive = true;
                    }
                    // 被动期间消费并丢弃托盘的「重连」请求：否则服务一停，那个陈旧的请求会
                    // 立刻触发一次注册，与服务实例再撞一次。
                    if reconnect_flag.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        tracing::debug!("服务接管身份期间忽略托盘「重连」请求");
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                // 服务已停（或从没装）：本窗口重新接管，下次真被动时要重新广播。
                passive = false;
                match run_client_loop(
                    config.clone(),
                    Some(Arc::clone(&status_tx)),
                    Some(Arc::clone(&log_buffer)),
                    Some(Arc::clone(&reconnect_flag)),
                    Arc::clone(&remote_manager),
                    owner_rx.clone(),
                )
                .await
                {
                    // 注册被拒等不可重连的失败：就此停下，不再进循环（避免热重试）。
                    LoopExit::Unrecoverable => break,
                    LoopExit::YieldedToService => {
                        // 服务接管：通知一次。本函数只被 GUI 路径调用，通知有人看得到。
                        notify::send(
                            "rust-tunnel",
                            "Windows 服务已接管客户端身份，本窗口不再注册",
                        );
                        tracing::warn!("服务已接管客户端身份，本窗口转为被动");
                    }
                }
            }
        });
    });
}

/// 控制通道循环的退出原因（见 [`run_client_loop`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopExit {
    /// 不可重连的失败（注册被拒等）——调用方应停止，不要再进循环。
    Unrecoverable,
    /// 服务接管了客户端身份——调用方应转被动，等服务停止后再回来。
    YieldedToService,
}

/// 服务接管身份时广播一次「被动」状态：连接置为断开 + 一行解释。
///
/// 状态面板与托盘都读这条 `watch`；`last_error` 的文案要能直接读通（托盘首行会把它截断，
/// 见 `tray::status_menu_text`）。
fn publish_passive_status(status_tx: &tokio::sync::watch::Sender<ClientStatus>) {
    let mut s = status_tx.borrow().clone();
    s.connected = false;
    s.last_error = Some("Windows 服务已接管客户端身份（本窗口未注册）".to_owned());
    let _ = status_tx.send(s);
}

/// 等到身份归属变成 [`IdentityOwner::Service`]（服务接管）。
///
/// 供 [`run_client_loop`] 的 `select!` 用：控制通道是一条长命 future，服务起来时必须能
/// 立刻把它打断，而不是等它自己断线。
///
/// **发送端被丢弃时永不 resolve**：那意味着监督方没了，而不是「服务接管」——若让它 resolve，
/// 一次通道关闭就会被误判成让出，本进程会永远停在被动态。故用 `pending()` 挂住。
async fn wait_for_service_owner(rx: &mut tokio::sync::watch::Receiver<IdentityOwner>) {
    loop {
        // 先 `borrow_and_update` 再 `changed`：watch 的经典用法——否则会立刻被当前值唤醒成
        // 「已变化」，而其实什么都没变。
        if rx.borrow_and_update().is_service() {
            return;
        }
        if rx.changed().await.is_err() {
            // 发送端已丢弃：挂住，永不 resolve（见上）。
            std::future::pending::<()>().await;
        }
    }
}

/// 控制通道重连循环（**GUI 与无头实例共用**）。
///
/// GUI 传 `status_tx` / `log_buffer` / `reconnect_flag` 三者；无头实例（`--instance`）三者
/// 全传 `None`——没有状态面板、没有托盘「重连」按钮，也不需要把日志塞进环形缓冲。
///
/// **`status_tx` 是否为 `Some` 同时充当「GUI 模式」判据**：只有 GUI 才弹系统通知
/// （SYSTEM 会话里的通知没人看得到，且 `notify-rust` 在无桌面上下文下会失败）。
///
/// `service_owner` 是服务状态轮询任务的接收端：`Some` 时本循环在**连接期间**与
/// **退避等待期间**都会盯着它，服务一接管就立刻返回 [`LoopExit::YieldedToService`]，
/// 把身份让给服务实例。无头实例与非 Windows 传 `None`，行为与从前逐字节一致。
#[allow(
    clippy::too_many_lines,
    reason = "重连状态机（成功 / 不可重连 / 可重连 + 可中断退避 + 服务让出）整体可读性优于拆散"
)]
async fn run_client_loop(
    mut config: ClientConfig,
    status_tx: Option<Arc<tokio::sync::watch::Sender<ClientStatus>>>,
    log_buffer: Option<Arc<LogBuffer>>,
    reconnect_flag: Option<Arc<AtomicBool>>,
    remote_manager: Arc<RemoteManager>,
    mut service_owner: Option<tokio::sync::watch::Receiver<IdentityOwner>>,
) -> LoopExit {
    let gui = status_tx.is_some();
    let mut policy = ReconnectPolicy::new();
    let mut first_success = true;

    loop {
        let cfg = config.clone();
        // 控制通道读循环把远程变体交给引擎，上行经桥接回写同一条通道。
        let bridge = Some(remote_manager.bridge());
        // 连接期间与「服务接管」竞速：服务一起来就立刻让出，不等控制通道自己断。
        // 无 `service_owner`（非 Windows / 无头实例）时走原来的纯 await 路径。
        let mut yielded = false;
        let res = match &mut service_owner {
            Some(rx) => {
                tokio::select! {
                    r = rust_tunnel_client::control::run_client_with_status(
                        cfg,
                        status_tx.clone(),
                        log_buffer.clone(),
                        bridge,
                    ) => r,
                    () = wait_for_service_owner(rx) => {
                        yielded = true;
                        Ok(())
                    }
                }
            }
            None => {
                rust_tunnel_client::control::run_client_with_status(
                    cfg,
                    status_tx.clone(),
                    log_buffer.clone(),
                    bridge,
                )
                .await
            }
        };

        if yielded {
            tracing::warn!("服务已接管客户端身份，控制通道让出");
            return LoopExit::YieldedToService;
        }

        match res {
            Ok(()) => {
                policy.reset();
                tracing::warn!("控制通道已关闭，准备重连");
            }
            Err(e) if !ReconnectPolicy::should_reconnect(&e) => {
                tracing::error!("注册被拒（不可重连）：{e}");
                if gui {
                    notify::send("rust-tunnel", &format!("注册被拒：{e}"));
                }
                // 标记错误到 status 供面板展示
                if let Some(tx) = &status_tx {
                    let mut s = tx.borrow().clone();
                    s.last_error = Some(e.to_string());
                    let _ = tx.send(s);
                }
                // 不可重连：就此停下，交由调用方决定（GUI 路径据此不再进循环）。
                break;
            }
            Err(e) => {
                tracing::warn!("连接错误：{e}");
                if let Some(tx) = &status_tx {
                    let mut s = tx.borrow().clone();
                    s.connected = false;
                    s.last_error = Some(e.to_string());
                    let _ = tx.send(s);
                }
                if first_success && gui {
                    notify::send("rust-tunnel", &format!("已断开：{e}"));
                }
            }
        }

        first_success = false;
        let backoff = policy.next_backoff();
        tracing::info!("{backoff}s 后重连…");

        // 可中断的退避等待：托盘「重连」立即打断；无头实例没有这个入口，纯退避。
        let mut waited = 0u64;
        while waited < backoff {
            tokio::time::sleep(Duration::from_secs(1)).await;
            // 服务在这一秒里起来了就立刻让出——绝不空等完 60 秒退避（那时服务早已接管，
            // 等完只会多打一次注定被踢的注册）。
            if service_owner
                .as_ref()
                .is_some_and(|rx| rx.borrow().is_service())
            {
                tracing::warn!("服务已接管客户端身份，中断退避等待");
                return LoopExit::YieldedToService;
            }
            let Some(flag) = &reconnect_flag else {
                waited += 1;
                continue;
            };
            if flag.load(std::sync::atomic::Ordering::SeqCst) {
                flag.store(false, std::sync::atomic::Ordering::SeqCst);
                if let Some(c) = config_store::load_from_default_path()
                    .or_else(|| ClientConfig::load().ok())
                {
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
    LoopExit::Unrecoverable
}

/// 启动模式（由命令行决定）。
#[derive(Debug)]
enum LaunchMode {
    /// 常规 GUI：托盘 + eframe 主窗口。无参数时。
    Gui,
    /// 无头实例：SYSTEM 服务用 `CreateProcessAsUserW` 拉起（见 `--instance`）。
    Instance(instance::InstanceOptions),
}

/// 解析命令行。
///
/// 手写而不用 `clap`：本 crate 是 **GUI-subsystem** 的，`clap` 的 help/报错走 stdout/stderr，
/// 在 GUI-subsystem 下**没有输出**；而这里要的就是「参数写错时能看见一条人话」。
///
/// **未知参数一律报错，不静默忽略**——静默忽略会让「命令行写错」表现成「装上了但行为不对」
/// （服务侧拼错一个 `--share`，实例照跑，只是共享目录落到了别处）。
///
/// # Errors
///
/// 未知参数、缺参数值、或 `--config` 未与 `--instance` 同用时返回错误文本。
fn parse_launch_mode<I: IntoIterator<Item = String>>(args: I) -> Result<LaunchMode, String> {
    const USAGE: &str = "\
用法：
  rust-tunnel-client-gui
      启动桌面托盘客户端（默认）。

  rust-tunnel-client-gui --instance [--config <host.toml>] [--share <dir>] [--download <dir>]
      无头实例：由 Windows 服务拉进控制台会话运行，无窗口、承载客户端身份、
      采集与注入跟随输入桌面。仅 Windows 可用。

    --config <path>    host.toml 路径（含服务端地址 + 客户端 token + 审批策略）。
                       默认 %ProgramData%\\rust-tunnel\\host.toml
    --share <dir>      被控端对外共享的目录。默认 %ProgramData%\\rust-tunnel\\share
    --download <dir>   控制端下载文件的落盘目录。默认 %ProgramData%\\rust-tunnel\\download
                       （三者也可用环境变量 RUST_TUNNEL_HOST_CONFIG / RUST_TUNNEL_SHARE_DIR /
                       RUST_TUNNEL_DOWNLOAD_DIR 覆盖）";

    let mut instance = false;
    let mut config_path: Option<PathBuf> = None;
    let mut share_dir: Option<PathBuf> = None;
    let mut download_dir: Option<PathBuf> = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--instance" => instance = true,
            "--config" | "--share" | "--download" => {
                let Some(value) = iter.next() else {
                    return Err(format!("{arg} 需要一个路径参数\n\n{USAGE}"));
                };
                let slot = match arg.as_str() {
                    "--config" => &mut config_path,
                    "--share" => &mut share_dir,
                    _ => &mut download_dir,
                };
                *slot = Some(PathBuf::from(value));
            }
            other => return Err(format!("未知参数：{other}\n\n{USAGE}")),
        }
    }

    if instance {
        Ok(LaunchMode::Instance(instance::InstanceOptions::resolve(
            config_path,
            share_dir,
            download_dir,
        )))
    } else if config_path.is_some() || share_dir.is_some() || download_dir.is_some() {
        Err(format!(
            "--config / --share / --download 只能与 --instance 一起用\n\n{USAGE}"
        ))
    } else {
        Ok(LaunchMode::Gui)
    }
}

fn main() -> anyhow::Result<()> {
    // argv 分派放在**最前**：早于 DISPLAY 检查、早于托盘与 eframe——`--instance` 是无头的，
    // 走到托盘/eframe 就会起窗口（那是它明确不该做的事）。
    match parse_launch_mode(std::env::args().skip(1)) {
        Ok(LaunchMode::Instance(opts)) => return instance::run(&opts),
        Ok(LaunchMode::Gui) => {}
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }

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

    // 偏好须在构建托盘与运行时之前加载：托盘的「允许被远程控制」勾选项要以持久化值为初态，
    // 且远程引擎的初始开关态取自该偏好（否则重启后菜单显示未勾选、而实际仍在被控状态）。
    let prefs = prefs::load();

    // 后台 tokio 运行时（控制通道/隧道 + 远程控制引擎）
    spawn_runtime(
        app_state.clone(),
        status_tx,
        log_buffer,
        reconnect_flag,
        prefs.allow_remote,
        prefs.remote_approval,
    );

    // 托盘在 eframe 初始化前需有事件循环；tray-icon 内部自行处理跨平台事件循环差异，
    // 此处先构建托盘（句柄随 GuiApp 持有生命周期），再起 eframe。
    let tray: Option<tray::TrayHandle> = match tray::build_tray(prefs.allow_remote) {
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

    let viewport = {
        let mut vp = egui::ViewportBuilder::default()
            .with_inner_size([480.0, 520.0])
            .with_min_inner_size([380.0, 400.0])
            .with_visible(true);
        if let Some(icon) = load_window_icon() {
            vp = vp.with_icon(icon);
        }
        vp
    };

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Rust Tunnel",
        options,
        Box::new(move |cc| {
            fonts::setup_fonts(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, prefs.theme);
            Ok(Box::new(GuiApp::new(app_state, tray, prefs)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    //! argv 分派的纯逻辑单测（无 GUI、无 Windows 依赖，故本机可跑）。
    //!
    //! 只测「参数 → 模式」这一层：`--instance` 的**默认路径**由
    //! [`instance::InstanceOptions::resolve`] 解析，会读环境变量，故这里只对**显式传入**的
    //! 路径做断言（它们优先于环境变量与默认值）。

    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn no_arguments_is_the_gui_path() {
        assert!(matches!(parse_launch_mode(args(&[])), Ok(LaunchMode::Gui)));
    }

    #[test]
    fn instance_flag_alone_is_the_instance_path() {
        let parsed = parse_launch_mode(args(&["--instance"])).unwrap();
        assert!(matches!(parsed, LaunchMode::Instance(_)));
    }

    #[test]
    fn explicit_paths_are_carried_through_in_any_order() {
        let parsed = parse_launch_mode(args(&[
            "--config",
            r"C:\ProgramData\rust-tunnel\host.toml",
            "--instance",
            "--share",
            r"D:\share",
            "--download",
            r"D:\dl",
        ]))
        .unwrap();
        let LaunchMode::Instance(opts) = parsed else {
            panic!("应当是实例模式");
        };
        assert_eq!(
            opts.config_path,
            PathBuf::from(r"C:\ProgramData\rust-tunnel\host.toml")
        );
        assert_eq!(opts.share_dir, PathBuf::from(r"D:\share"));
        assert_eq!(opts.download_dir, PathBuf::from(r"D:\dl"));
    }

    /// 未知参数**必须报错**：静默忽略会让「命令行写错」表现成「装上了但行为不对」。
    #[test]
    fn unknown_arguments_are_rejected() {
        let err = parse_launch_mode(args(&["--instanc"])).unwrap_err();
        assert!(err.contains("未知参数"), "{err}");
        // 参数值本身也可能是拼错的参数（`--config --share` 这种）——它会被当成路径收下，
        // 这不是我们能判定的错误（路径长什么样是服务侧的事）。
        assert!(parse_launch_mode(args(&["--instance", "--oops", "x"])).is_err());
    }

    /// 缺参数值要报错，而不是把下一个参数当成值或直接忽略。
    #[test]
    fn missing_argument_value_is_rejected() {
        for flag in ["--config", "--share", "--download"] {
            let err = parse_launch_mode(args(&["--instance", flag])).unwrap_err();
            assert!(err.contains("需要一个路径参数"), "{flag}: {err}");
        }
    }

    /// 实例专用参数不能单独出现（GUI 路径不读它们，静默忽略同样会掩盖拼写错误）。
    #[test]
    fn instance_only_flags_without_instance_are_rejected() {
        for flag in ["--config", "--share", "--download"] {
            let err = parse_launch_mode(args(&[flag, "x"])).unwrap_err();
            assert!(err.contains("只能与 --instance 一起用"), "{flag}: {err}");
        }
    }
}
