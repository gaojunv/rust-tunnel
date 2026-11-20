//! rust-tunnel 桌面托盘客户端（winit + tray-icon + eframe）。

#![allow(clippy::missing_docs_in_private_items, reason = "GUI 二进制")]
// 单测豁免（与各 lib crate 的 `lib.rs` 头部一致）：restriction 组在测试里放宽。
// 本 crate 是 bin，没有 lib.rs，故这行放在这里。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
// Release 构建在 Windows 上隐藏控制台窗口（双击启动仅显示 GUI）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod config_store;
mod fonts;
mod instance;
mod notify;
mod prefs;
mod preview;
mod remote;
mod service_ui;
mod service_watch;
mod tray;
mod tray_worker;
mod ui;
mod update;

use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;

use rust_tunnel_client::{ClientConfig, ClientStatus, LogBuffer, ReconnectPolicy};
use rust_tunnel_remote::{
    ApprovalPolicy, EncodeOptions, RemoteManager, RemoteManagerConfig, SessionRole,
};

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
/// 启动时选中的 Tab。`RUST_TUNNEL_GUI_START_TAB=0..4` 仅供截图评审/自动化使用，
/// 正常启动恒为 0（状态页）。
fn initial_tab() -> usize {
    std::env::var("RUST_TUNNEL_GUI_START_TAB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map_or(0, |tab| tab.min(4))
}

/// 映射页初始状态：演示数据下展开第一条规则，截图评审能一次看到「监听 → 路径 → 后端」链路。
fn initial_map_state() -> ui::mappings::MapState {
    let mut state = ui::mappings::MapState::default();
    if preview::enabled() {
        state.expanded.insert("rule_7f3a".to_owned());
    }
    state
}

/// 设置页初始分组；`RUST_TUNNEL_GUI_SETTINGS_PAGE=remote|service|update` 仅供截图评审。
fn initial_settings_page() -> ui::settings::SettingsPage {
    match std::env::var("RUST_TUNNEL_GUI_SETTINGS_PAGE")
        .ok()
        .as_deref()
    {
        Some("remote") => ui::settings::SettingsPage::Remote,
        Some("service") => ui::settings::SettingsPage::Service,
        Some("update") => ui::settings::SettingsPage::Update,
        _ => ui::settings::SettingsPage::default(),
    }
}

/// 「远程」页初始 Tab；`RUST_TUNNEL_GUI_REMOTE_TAB=remote|other` 仅供截图评审。
///
/// 默认落在「本机被控」（安全设置是这一页最常被翻看的内容）；截图/QA 需要「连接到其他设备」
/// 那一屏时用环境变量直接切过去，免去自动化工具模拟点击 Tab。
fn initial_remote_page() -> ui::remote::RemotePage {
    let tab = match std::env::var("RUST_TUNNEL_GUI_REMOTE_TAB").ok().as_deref() {
        Some("remote" | "other") => ui::remote::RemoteTab::OtherDevices,
        _ => ui::remote::RemoteTab::LocalControl,
    };
    ui::remote::RemotePage { tab }
}

struct GuiApp {
    app_state: Arc<app::AppState>,
    settings: ui::settings::SettingsState,
    selected_tab: usize,
    /// 托盘句柄（保持 `TrayIcon` 生命周期，随窗口同生共死）。
    tray: Option<tray::TrayHandle>,
    /// 托盘事件线程与 GUI 之间的桥：托盘线程负责窗口级操作，这里每帧取走它留给
    /// UI 的结果（切 Tab、被控开关镜像）。
    tray_bridge: Arc<tray_worker::TrayBridge>,
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
    /// 上一帧看到的「可用更新版本」。用于检测新版本首次出现时强制显示并聚焦窗口
    /// （窗口可能已隐藏到托盘，更新提示不能只在后台）。
    last_update_version: Option<String>,
    /// 自更新替换任务（后台线程）的结果通道。`Some` = 正在更新（先服务、后客户端）。
    ///
    /// 替换要提权跑一次服务安装器并等它结束（UAC + 复制 + 停服务），放 UI 线程上就是
    /// 「界面无响应」，所以走后台线程、每帧轮询。
    apply_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// 已下载待安装的更新包。替换失败（例如用户取消 UAC）后保留，供「重试」直接再装，
    /// 不必重新下载一遍——两端都不更新是刻意的策略，用户点重试就该接着装同一个包。
    retry_staged: Option<update::StagedUpdate>,
    /// 日志页的本地筛选状态（等级/关键词/暂停自动滚动）。
    logs_state: ui::logs::LogsState,
    /// 映射页的本地状态（搜索词 + 已展开规则）。
    map_state: ui::mappings::MapState,
    /// 状态页的秒级趋势采样（RTT / 活跃隧道）。
    status_history: ui::status::StatusHistory,
    /// 设置页当前选中的二级分组。
    settings_page: ui::settings::SettingsPage,
    /// 「远程」页当前选中的 Tab（本机被控 / 连接到其他设备），跨帧保持。
    remote_page: ui::remote::RemotePage,
}

impl GuiApp {
    fn new(
        app_state: Arc<app::AppState>,
        tray: Option<tray::TrayHandle>,
        tray_bridge: Arc<tray_worker::TrayBridge>,
        prefs: prefs::GuiPrefs,
        wgpu_target_format: Option<egui_wgpu::wgpu::TextureFormat>,
    ) -> Self {
        let settings = app_state
            .config_snapshot()
            .as_ref()
            .map(ui::settings::SettingsState::from_config)
            .unwrap_or_default();
        let remote_view = Arc::new(Mutex::new(remote::RemoteView::from_prefs(
            &prefs,
            wgpu_target_format,
        )));
        Self {
            app_state,
            settings,
            selected_tab: initial_tab(),
            tray,
            tray_bridge,
            last_tray_texts: None,
            prefs,
            remote_view,
            approval_remember: false,
            last_pending_approval: None,
            service_ui: service_ui::ServiceOps::default(),
            last_update_version: None,
            apply_rx: None,
            retry_staged: None,
            logs_state: ui::logs::LogsState::default(),
            map_state: initial_map_state(),
            status_history: ui::status::StatusHistory::default(),
            settings_page: initial_settings_page(),
            remote_page: initial_remote_page(),
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
        self.tray_bridge.set_allow_remote(on);
        if let Some(handle) = self.tray.as_ref() {
            handle.allow_remote_item.set_checked(on);
        }
        if let Err(e) = prefs::save(&self.prefs) {
            tracing::warn!("被控开关保存失败：{e}");
        }
        tracing::info!("允许被远程控制：{}", if on { "开" } else { "关" });
    }

    /// 应用「会话结束后自动锁定本机屏幕」的新状态：同步到引擎并落盘 gui.toml。
    ///
    /// 引擎里写的是原子位，被控端会话收尾守卫**实时读取**——会话进行中打开此设置，
    /// 那一场结束时就会锁屏。
    fn apply_auto_lock_on_disconnect(&mut self, on: bool) {
        self.prefs.remote_auto_lock_on_disconnect = on;
        if let Some(manager) = self.app_state.remote() {
            manager.set_auto_lock_on_disconnect(on);
        }
        if let Err(e) = prefs::save(&self.prefs) {
            tracing::warn!("自动锁屏设置保存失败：{e}");
        }
        tracing::info!("会话结束后自动锁屏：{}", if on { "开" } else { "关" });
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

    /// 执行更新 UI 上的动作（弹窗与设置页共用）。
    fn apply_update_action(&mut self, action: ui::update::UpdateAction, ctx: &egui::Context) {
        match action {
            ui::update::UpdateAction::Download => {
                // 已经下载过（上一次替换失败，例如取消 UAC）：直接重装同一个包，
                // 不再走一遍几十 MB 的下载。
                if let Some(staged) = self.retry_staged.clone() {
                    self.app_state.update.clear_error();
                    self.begin_apply(staged);
                } else if let Err(e) = self.app_state.update.start_download() {
                    self.app_state.update.fail(e);
                }
            }
            ui::update::UpdateAction::Dismiss => self.app_state.update.dismiss(),
            ui::update::UpdateAction::Skip(version) => {
                tracing::info!("跳过客户端版本 {version}");
                self.app_state.update.set_skipped(Some(version.clone()));
                self.prefs.update_skipped_version = Some(version);
                if let Err(e) = prefs::save(&self.prefs) {
                    tracing::warn!("跳过版本偏好保存失败：{e}");
                }
                self.app_state.update.dismiss();
            }
            ui::update::UpdateAction::CancelSkip => {
                self.app_state.update.set_skipped(None);
                self.app_state.update.undismiss();
                self.prefs.update_skipped_version = None;
                if let Err(e) = prefs::save(&self.prefs) {
                    tracing::warn!("取消跳过版本保存失败：{e}");
                }
            }
            ui::update::UpdateAction::CheckNow => {
                if let Err(e) = self.app_state.update.check_now() {
                    self.app_state.update.fail(e);
                }
            }
            ui::update::UpdateAction::ClearError => self.app_state.update.clear_error(),
        }
        ctx.request_repaint();
    }

    /// 起后台任务执行整包替换（Windows 上先提权更新无人值守服务，再替换 GUI）。
    ///
    /// 结果由 [`eframe::App::update`] 每帧轮询：成功就退出本进程（updater 会重启新版本），
    /// 失败则把错误显示在更新弹窗里，并保留 `retry_staged` 供重试。
    fn begin_apply(&mut self, staged: update::StagedUpdate) {
        self.app_state.update.mark_applying();
        self.retry_staged = Some(staged.clone());
        self.apply_rx = Some(update::spawn_apply(staged));
    }
}

/// 应用主题偏好：egui 绘制 + 原生标题栏外观。
///
/// 主题切换后必须**重新落一遍 Fluent 样式**：`set_theme` 只切主题，不会把自定义令牌
/// 带过去；`all_styles_mut` 会把浅色/深色两份都按当前偏好重算。
fn apply_theme(ctx: &egui::Context, pref: prefs::ThemePref) {
    ctx.set_theme(pref.to_egui());
    let native = match pref {
        prefs::ThemePref::System => egui::SystemTheme::SystemDefault,
        prefs::ThemePref::Light => egui::SystemTheme::Light,
        prefs::ThemePref::Dark => egui::SystemTheme::Dark,
    };
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(native));
    ui::theme::apply_global_style(ctx);
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

    /// 取走托盘线程留给 UI 的 Tab 切换请求。
    ///
    /// 托盘事件本身由 [`tray_worker`] 的独立线程消费——**不能**在这里轮询：窗口隐藏后
    /// 没有帧，这份 `update` 根本不会运行（见该模块文档）。
    fn apply_pending_tray_tab(&mut self) {
        if let Some(tab) = self.tray_bridge.take_pending_tab() {
            // 上限保护：桥里的值只由本 crate 写入，越界说明有人加了 Tab 却漏改这里。
            self.selected_tab = tab.min(4);
        }
    }

    /// 把托盘勾选态（**唯一事实来源**，见 [`tray::TrayHandle`]）回写到内存偏好与桥镜像。
    ///
    /// 托盘线程在窗口隐藏时会自己翻转镜像并落盘，这里每帧用真实勾选态校正内存副本，
    /// 否则之后 `sync_display_prefs` 落盘时会拿旧值把 `gui.toml` 覆盖回去。
    fn sync_allow_remote_from_tray(&mut self) {
        let Some(handle) = self.tray.as_ref() else {
            return;
        };
        let on = handle.allow_remote_item.is_checked();
        self.tray_bridge.set_allow_remote(on);
        self.prefs.allow_remote = on;
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

    /// 导航栏底部的主题切换按钮（循环三态，落盘 gui.toml）。
    fn show_theme_button(&mut self, ui: &mut egui::Ui) {
        let cur = self.prefs.theme;
        let effective = match ui.ctx().theme() {
            egui::Theme::Dark => "深色",
            egui::Theme::Light => "浅色",
        };
        let icon = match cur {
            prefs::ThemePref::System => ui::icons::Icon::Monitor,
            prefs::ThemePref::Light => ui::icons::Icon::Sun,
            prefs::ThemePref::Dark => ui::icons::Icon::Moon,
        };
        let btn = ui::widgets::nav_theme_button(
            ui,
            icon,
            cur.label(),
            &format!("切换主题（当前生效：{effective}，偏好：{}）", cur.label()),
        );
        if btn.clicked() {
            let next = cur.next();
            self.prefs.theme = next;
            apply_theme(ui.ctx(), next);
            if let Err(e) = prefs::save(&self.prefs) {
                tracing::warn!("GUI 偏好保存失败：{e}");
            }
        }
    }

    /// 左侧导航栏：品牌区 + 五个导航项 + 底部主题切换。
    ///
    /// 窗口加宽到 760×600 后，左侧导航比顶部 Tab 更符合 Fluent 的桌面应用习惯，也让
    /// 内容区能放下统计块、规则卡片与日志表格。`selected_tab` 的 0..=4 语义保持不变
    /// （托盘菜单与 `apply_pending_tray_tab` 都依赖它）。
    fn show_nav(&mut self, ctx: &egui::Context) {
        let items = [
            (ui::icons::Icon::Pulse, "状态"),
            (ui::icons::Icon::Route, "映射"),
            (ui::icons::Icon::Monitor, "远程"),
            (ui::icons::Icon::Settings, "设置"),
            (ui::icons::Icon::List, "日志"),
        ];
        egui::SidePanel::left("app_nav")
            .resizable(false)
            .exact_width(196.0)
            .frame(ui::theme::nav_frame(ctx.style().as_ref()))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui::icons::app_mark(ui, 26.0);
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("rust-tunnel")
                                .font(ui::icons::heading_font(15.0))
                                .strong(),
                        );
                        ui.label(
                            egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .size(11.0)
                                .weak(),
                        );
                    });
                });
                ui.add_space(16.0);
                for (idx, (icon, label)) in items.iter().enumerate() {
                    if ui::widgets::nav_item(ui, *icon, label, self.selected_tab == idx).clicked() {
                        self.selected_tab = idx;
                    }
                    ui.add_space(2.0);
                }
                // 主题按钮贴底：`bottom_up` 在 SidePanel 里拿到的可用高度会被前面的
                // 水平布局吃掉，改成显式留白更可靠（窗口再矮也只会贴底而不是消失）。
                let remaining = ui.available_height() - 48.0;
                if remaining > 0.0 {
                    ui.add_space(remaining);
                }
                ui::widgets::divider(ui);
                ui.add_space(6.0);
                self.show_theme_button(ui);
            });
    }
    /// 把工具条里改过的展示档位（缩放 / 窗口尺寸 / 角标简洁或详细 / 剪贴板同步 / 工具条自动隐藏 / 帧率上限）回写 `gui.toml`。
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
        if self.prefs.remote_stats_detail != view.stats_detail {
            self.prefs.remote_stats_detail = view.stats_detail;
            changed = true;
        }
        if self.prefs.remote_clipboard_sync != view.clipboard_sync_enabled {
            self.prefs.remote_clipboard_sync = view.clipboard_sync_enabled;
            changed = true;
        }
        if self.prefs.remote_toolbar_autohide != view.toolbar_autohide {
            self.prefs.remote_toolbar_autohide = view.toolbar_autohide;
            changed = true;
        }
        if self.prefs.remote_toolbar_collapsed != view.toolbar_collapsed {
            self.prefs.remote_toolbar_collapsed = view.toolbar_collapsed;
            changed = true;
        }
        if self.prefs.remote_frame_rate != view.frame_rate {
            self.prefs.remote_frame_rate = view.frame_rate;
            changed = true;
        }
        if self.prefs.remote_quality != view.quality {
            self.prefs.remote_quality = view.quality;
            changed = true;
        }
        if self.prefs.remote_audio_enabled != view.audio_enabled {
            self.prefs.remote_audio_enabled = view.audio_enabled;
            changed = true;
        }
        if self.prefs.remote_audio_volume != view.audio_volume {
            self.prefs.remote_audio_volume = view.audio_volume;
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
    #[expect(
        clippy::too_many_lines,
        reason = "每帧编排：托盘、远程会话、自更新、弹窗与重绘节奏都在这里收敛，拆分只会把共享状态再传一遍"
    )]
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.intercept_close(ctx) {
            return;
        }
        self.apply_pending_tray_tab();
        self.sync_allow_remote_from_tray();
        // 抽干服务操作的结果通道：一次 --install 要跑约二十秒，结果只能在后续帧里看到。
        self.service_ui.poll();

        // 自更新：到点自动检查；已校验完成的包在后台线程执行替换（先服务、后客户端）。
        self.app_state
            .update
            .maybe_check(self.prefs.update_auto_check);
        if self.apply_rx.is_none() {
            if let Some(staged) = self.app_state.update.take_staged() {
                self.begin_apply(staged);
            }
        }
        // 先把接收端取出来再 match：否则 `rx` 的可变借用会挡住下面改 `self.apply_rx`。
        if let Some(rx) = self.apply_rx.take() {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    let version = self
                        .retry_staged
                        .as_ref()
                        .map_or("", |staged| staged.version.as_str());
                    tracing::info!(version, "更新已就绪，正在重启应用");
                    self.app_state
                        .should_exit
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                Ok(Err(e)) => {
                    self.app_state.update.clear_applying();
                    tracing::error!("应用更新失败：{e}");
                    self.app_state.update.fail(e);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.apply_rx = Some(rx);
                    // 提权对话框 / 复制 exe 都要时间：保持重绘，别让弹窗停在旧帧上。
                    ctx.request_repaint_after(std::time::Duration::from_millis(200));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.app_state.update.clear_applying();
                    self.app_state
                        .update
                        .fail("更新任务异常结束，请重试".to_owned());
                }
            }
        }

        let status = self.app_state.status_rx.borrow().clone();
        // 趋势线每秒采样一次（页面只读采样结果，不自己计时）。
        self.status_history.sample(&status, Instant::now());

        // 远程引擎就绪后安装帧通道（幂等）；引擎由后台运行时线程注入 AppState。
        let remote_manager = self.app_state.remote();
        // 一帧只取一次远程状态快照：托盘 tooltip、会话生命周期收敛与重连都基于它。
        let remote_state = remote_manager.as_ref().map(|m| m.snapshot());
        let pending_id = remote_state
            .as_ref()
            .and_then(|s| s.pending_approval.as_ref().map(|p| p.session_id.clone()));
        self.refresh_tray(&status, pending_id.is_some());
        self.show_nav(ctx);

        // 待确认请求首次出现：窗口可能已隐藏到托盘（见 `intercept_close`），安全提示绝不能
        // 静默超时，故强制显示并聚焦。仅在 None→Some 跳变时做，避免每帧与用户抢焦点。
        if pending_id.is_some() && self.last_pending_approval != pending_id {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.last_pending_approval = pending_id;

        // 可用更新首次出现：与接入确认同理，窗口可能已隐藏到托盘，更新提示不能被
        // 静默错过。仅在版本变化时抢一次焦点，避免每帧与用户抢。
        let update_snapshot = self.app_state.update.snapshot();
        let offered_version = update_snapshot
            .available
            .as_ref()
            .map(|available| available.version.clone());
        if let Some(version) = offered_version.as_deref() {
            let skipped = self.prefs.update_skipped_version.as_deref() == Some(version);
            let first_time = self.last_update_version.as_deref() != Some(version);
            if first_time && !skipped && !update_snapshot.dismissed {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        self.last_update_version = offered_version;

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
        let mut update_action: Option<ui::update::UpdateAction> = None;
        let settings = &mut self.settings;
        let service_ui = &mut self.service_ui;
        let logs_state = &mut self.logs_state;
        let map_state = &mut self.map_state;
        let settings_page = &mut self.settings_page;
        let remote_page = &mut self.remote_page;
        let prefs_mut = &mut self.prefs;
        let status_history = &self.status_history;
        let remote_view = Arc::clone(&self.remote_view);
        let app_state = &self.app_state;
        egui::CentralPanel::default()
            .frame(ui::theme::page_frame(ctx.style().as_ref()))
            .show(ctx, |ui| {
                // 被控横幅常驻所有 Tab 顶部：无论用户切到哪个页都能看到并随时中断。
                ui::remote::controlled_banner(ui, remote_manager.as_ref());
                match self.selected_tab {
                    0 => ui::status::show(ui, &status, app_state.service_owner(), status_history),
                    1 => ui::mappings::show(ui, &status, map_state),
                    2 => {
                        remote_actions = ui::remote::show(
                            ui,
                            &remote_view,
                            remote_manager.as_ref(),
                            remote_page,
                            app_state,
                        );
                    }
                    3 => {
                        // 设置页自带「页头 + 顶部分组 Tab + 滚动内容」，调用方不再包 ScrollArea。
                        let (prefs_changed, action) = ui::settings::show(
                            ui,
                            settings_page,
                            settings,
                            app_state,
                            prefs_mut,
                            remote_manager.as_ref(),
                            service_ui,
                            app_state.update.as_ref(),
                        );
                        if prefs_changed {
                            if let Err(e) = prefs::save(prefs_mut) {
                                tracing::warn!("GUI 偏好保存失败：{e}");
                            }
                        }
                        if action.is_some() {
                            update_action = action;
                        }
                    }
                    4 => ui::logs::show(ui, &app_state.log_buffer, logs_state),
                    _ => {}
                }
            });
        if let Some(on) = remote_actions.allow_remote {
            self.apply_allow_remote(on);
        }
        if let Some(policy) = remote_actions.approval_policy {
            self.apply_approval_policy(policy);
        }
        if let Some(on) = remote_actions.auto_lock_on_disconnect {
            self.apply_auto_lock_on_disconnect(on);
        }
        // 更新弹窗与接入确认一样常驻所有 Tab 之上；设置页里的按钮优先于弹窗。
        let modal_action = ui::update::modal(
            ctx,
            self.app_state.update.as_ref(),
            self.prefs.update_skipped_version.as_deref(),
        );
        if let Some(action) = update_action.take().or(modal_action) {
            self.apply_update_action(action, ctx);
        }
        // 接入确认弹窗常驻所有 Tab 之上（与 `controlled_banner` 同理）。
        ui::remote::approval_modal(ctx, &mut self.approval_remember, remote_manager.as_ref());

        // 独立会话窗口本体（仅控制器会话且窗口应存在时注册）。
        if let Some(manager) = remote_manager.as_ref() {
            ui::session_window::show(ctx, &self.remote_view, manager);
        }

        self.sync_display_prefs();

        // 服务操作在途时也要勤刷：结果要靠 `poll` 才被看到（在途期间按钮已禁用）。
        let service_busy = self.service_ui.in_flight().is_some();
        let update_snapshot = self.app_state.update.snapshot();
        let update_active = update_snapshot.download.is_some();
        let needs_repaint = !status.connected
            || status.last_error.is_some()
            || service_busy
            || update_active
            || update_snapshot.available.is_some();
        if update_active {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else if needs_repaint {
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "这些开关一一对应 `GuiPrefs` / `host.toml` 的键，拆成状态机只会让调用点更难读"
)]
struct ManagerOptions {
    /// 初始是否允许被远程控制。
    enabled: bool,
    /// 接入确认策略。
    approval_policy: ApprovalPolicy,
    /// 被控端会话结束后是否自动锁定屏幕（GUI 取自 `GuiPrefs`，实例取自 `host.toml`）。
    auto_lock_on_disconnect: bool,
    /// 被控端是否允许原生分辨率（4K）编码（仅硬件 H.264/H.265 生效）。
    native_resolution: bool,
    /// 是否优先协商 H.265/HEVC（双方都支持时）；不可用则自动回退 H.264/VP9。
    prefer_h265: bool,
    /// 是否优先 4:4:4（VP9 profile 1）；优先级高于 `prefer_h265`。
    prefer_444: bool,
    /// 中继会话是否自动尝试升级到 P2P 打洞（协议 v12 会话内路径迁移）。
    ///
    /// GUI 取自偏好；无人值守实例恒 `true`（它不发起会话，只在对端请求时配合）。
    punch_migration: bool,
    /// 平台实现模式（**必须在构造期选定**，见 [`remote::PlatformMode`]）。
    platform_mode: remote::PlatformMode,
    /// 被控端共享目录（`None` = 不接受文件请求）。
    share_dir: Option<PathBuf>,
    /// 控制器下载文件的落盘目录。
    download_dir: PathBuf,
    /// 控制端会话开始时是否请求转发声音（GUI 取自偏好；实例没有控制器，恒 `false`）。
    audio_enabled: bool,
    /// 被控端是否允许转发声音（GUI 取自偏好；实例取自 `host.toml`）。
    audio_allowed: bool,
    /// 控制端本地播放音量（0–100）。
    audio_volume: u8,
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
    // Windows 上探测 NVENC/MFT 的 H.264 与 H.265 编解码器、以及 Media Foundation
    // 解码器；缺失的组合让协商按 H.265 → H.264 → VP9 逐级回退。其他平台四个都是 `None`。
    let remote::HardwareFactories {
        h264_encoder: h264_encoder_factory,
        h264_decoder: h264_decoder_factory,
        h265_encoder: h265_encoder_factory,
        h265_decoder: h265_decoder_factory,
    } = remote::hardware_factories();
    let audio = remote::audio_endpoints();
    let manager = RemoteManager::new(RemoteManagerConfig {
        server_host: config.server.clone(),
        self_client_name: config.name.clone().unwrap_or_default(),
        platform: remote::platform_name().to_owned(),
        displays: 1,
        enabled: opts.enabled,
        approval_policy: opts.approval_policy,
        establisher,
        native_resolution: opts.native_resolution,
        encode: EncodeOptions::default(),
        share_dir: opts.share_dir,
        download_dir: opts.download_dir,
        // Windows 硬件编解码实现由 `remote::win_video` 注入；其他平台保持 VP9。
        h264_encoder_factory,
        h264_decoder_factory,
        h265_encoder_factory,
        h265_decoder_factory,
        // 偏好与工厂就位后即按 H.265 优先协商；任一侧缺能力或环境变量禁用则回退 H.264/VP9。
        prefer_h265: opts.prefer_h265,
        // 文字优先：开启后用 VP9（4:4:4）压过 H.264/H.265（都是 4:2:0）。
        prefer_444: opts.prefer_444,
        // 音频：Windows 上同时注入采集源与播放器；其它平台为空（引擎会向对端上报不可用）。
        // 是否真的开始转发由控制端 `SetAudioEnabled` 驱动，构造这里不打开设备。
        audio_source: audio.source,
        audio_sink: audio.sink,
        audio_enabled: opts.audio_enabled,
        audio_allowed: Arc::new(AtomicBool::new(opts.audio_allowed)),
        // 会话内路径迁移：中继会话后台尝试打洞，成功后无缝换流（见 `crates/remote` 的
        // `migrate` 模块）。`always_use_relay` 时建立器自己会拒绝，不必在这里判断。
        punch_migration: opts.punch_migration,
    });
    // 自动锁屏偏好不在 `RemoteManagerConfig` 里（那个结构是数据面/平台参数），构造后单独设置。
    manager.set_auto_lock_on_disconnect(opts.auto_lock_on_disconnect);
    // 播放音量同理：它是本地增益，直接作用于已注入的 AudioSink。
    manager.set_audio_volume(u32::from(opts.audio_volume));
    // 平台实现在这里**一次性**选定：引擎受理会话时就把 `Arc<dyn ScreenSource>` /
    // `Arc<dyn InputSink>` 克隆走了，之后 `install_platform` 再覆盖对已建立的会话无效。
    let release = remote::install_platform(&manager, opts.platform_mode);
    (manager, release)
}

/// GUI 路径的引擎选项：共享目录固定为「下载」目录下的 `rust-tunnel-share`——可见、易找，
/// 且与用户的个人目录隔离（用户要换目录只需在这里改一处）。
#[expect(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    reason = "参数与 `GuiPrefs` 的开关一一对应，打包成结构体只会让调用点多一层样板"
)]
fn gui_manager_options(
    enabled: bool,
    approval_policy: ApprovalPolicy,
    auto_lock_on_disconnect: bool,
    native_resolution: bool,
    prefer_h265: bool,
    prefer_444: bool,
    punch_migration: bool,
    audio_enabled: bool,
    audio_allowed: bool,
    audio_volume: u8,
) -> ManagerOptions {
    ManagerOptions {
        enabled,
        approval_policy,
        auto_lock_on_disconnect,
        native_resolution,
        prefer_h265,
        prefer_444,
        punch_migration,
        platform_mode: remote::PlatformMode::Gui,
        share_dir: Some(remote::share_dir()),
        download_dir: remote::download_dir(),
        // 偏好接线：控制端是否请求声音、被控端是否允许采集。
        audio_enabled,
        audio_allowed,
        audio_volume,
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    clippy::items_after_statements,
    clippy::map_unwrap_or,
    reason = "GUI 托盘启动编排，一次性初始化链路拆分无益；开关参数与 prefs 一一对应"
)]
fn spawn_runtime(
    app_state: Arc<app::AppState>,
    status_tx: Arc<tokio::sync::watch::Sender<ClientStatus>>,
    log_buffer: Arc<LogBuffer>,
    reconnect_flag: Arc<AtomicBool>,
    initial_allow_remote: bool,
    initial_approval: ApprovalPolicy,
    initial_auto_lock: bool,
    initial_native_resolution: bool,
    initial_prefer_h265: bool,
    initial_prefer_444: bool,
    initial_punch_migration: bool,
    initial_audio_enabled: bool,
    initial_audio_allowed: bool,
    initial_audio_volume: u8,
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
        let mut bootstrap = config_store::load_effective();
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
            use rust_tunnel_common::init_logging_with_layers;
            use tracing_subscriber::filter::filter_fn;
            use tracing_subscriber::Layer as _;
            let lb: Arc<LB> = log_buffer.clone();
            // 必须用进程级共享 layer 注册：run_client_inner 内部复用同一实例做
            // sender 热替换（日志上报服务端）；另建实例会导致其收不到事件、
            // 且每次重连泄漏一个日志转发任务。
            let layer = rust_tunnel_client::control::shared_log_layer();
            layer.set_log_buffer(lb);
            // 用当前 log 级别（若有配置否则 info）初始化；重复 init 内部 try_init 会静默忽略
            let lvl = bootstrap.as_ref().map(|c| c.log.as_str()).unwrap_or("info");
            // 延迟诊断专用文件 sink：普通 GUI 只有 stdout + 500 条环形缓冲（会被正常日志滚掉），
            // 故把 `target == "latency"` 的行另存一份。**首次写入才创建文件**——没开延迟日志
            // 时不会留下一个空文件。
            let latency_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(LatencyFileWriter::new(
                    latency_log_path(),
                )))
                .with_filter(filter_fn(|meta| meta.target() == "latency"));
            init_logging_with_layers(lvl, layer, latency_layer);
            tracing::info!("GUI 已启动");
            tracing::info!("配置路径: {}", config_store::default_path().display());
        }
        rt.block_on(async move {
            // 自更新桥接：控制通道读循环把 `ClientUpdate*` 交给它；GUI 侧从 AppState 读同一份。
            let update_bridge = Arc::clone(&app_state.update);
            // 初始配置：优先 GUI 默认路径，其次 CLI env/TOML
            let mut config = if let Some(c) = config_store::load_from_default_path() {
                c
            } else if let Some(c) = config_store::load_cli_fallback() {
                c
            } else {
                tracing::warn!("no config: waiting for settings");
                // 无配置时保持离线，等待用户在设置 Tab 保存
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if reconnect_flag.load(std::sync::atomic::Ordering::SeqCst) {
                        reconnect_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                        if let Some(c) = config_store::load_from_default_path() {
                            break c;
                        }
                        if let Some(c) = config_store::load_cli_fallback() {
                            break c;
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
                gui_manager_options(
                    initial_allow_remote,
                    initial_approval,
                    initial_auto_lock,
                    initial_native_resolution,
                    initial_prefer_h265,
                    initial_prefer_444,
                    initial_punch_migration,
                    initial_audio_enabled,
                    initial_audio_allowed,
                    initial_audio_volume,
                ),
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
                let owner = owner_rx
                    .as_ref()
                    .map_or(IdentityOwner::Gui, |rx| *rx.borrow());
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
                    Some(Arc::clone(&update_bridge)),
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
    update_bridge: Option<Arc<update::UpdateBridge>>,
    mut service_owner: Option<tokio::sync::watch::Receiver<IdentityOwner>>,
) -> LoopExit {
    let gui = status_tx.is_some();
    let mut policy = ReconnectPolicy::new();
    let mut first_success = true;

    loop {
        let cfg = config.clone();
        // 控制通道读循环把远程变体交给引擎，上行经桥接回写同一条通道。
        let bridge = Some(remote_manager.bridge());
        // 自更新桥接：重连时由控制循环热替换上行发送端；无头实例传 `None`。
        let update: Option<Arc<dyn rust_tunnel_common::ClientUpdateBridge>> = update_bridge
            .as_ref()
            .map(|bridge| Arc::clone(bridge) as Arc<dyn rust_tunnel_common::ClientUpdateBridge>);
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
                        update.clone(),
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
                    update,
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
                if let Some(c) = config_store::load_effective() {
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
    /// 自更新收尾：等待旧进程退出后把当前 exe 复制到目标路径并重启
    /// （Windows 专用；由 `update::apply_and_restart` 以内建参数拉起）。
    ApplyUpdate(PathBuf),
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
                       RUST_TUNNEL_DOWNLOAD_DIR 覆盖）

  rust-tunnel-client-gui --apply-update <target>
      自更新收尾（内部使用）：等待 target 被释放后复制当前 exe 过去并重启。
      由新版客户端在替换 Windows 上正在运行的程序时自动拉起，不要手工调用。";

    let mut instance = false;
    let mut config_path: Option<PathBuf> = None;
    let mut share_dir: Option<PathBuf> = None;
    let mut download_dir: Option<PathBuf> = None;
    let mut apply_update: Option<PathBuf> = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--instance" => instance = true,
            "--config" | "--share" | "--download" | "--apply-update" => {
                let Some(value) = iter.next() else {
                    return Err(format!("{arg} 需要一个路径参数\n\n{USAGE}"));
                };
                let slot = match arg.as_str() {
                    "--config" => &mut config_path,
                    "--share" => &mut share_dir,
                    "--apply-update" => &mut apply_update,
                    _ => &mut download_dir,
                };
                *slot = Some(PathBuf::from(value));
            }
            other => return Err(format!("未知参数：{other}\n\n{USAGE}")),
        }
    }

    if let Some(target) = apply_update {
        if instance || config_path.is_some() || share_dir.is_some() || download_dir.is_some() {
            return Err(format!(
                "--apply-update 是内部自更新模式，不能与其他参数一起用\n\n{USAGE}"
            ));
        }
        return Ok(LaunchMode::ApplyUpdate(target));
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

/// 启动期诊断：把 panic/启动错误写到 `%LOCALAPPDATA%\rust-tunnel\gui-startup.log`。
///
/// GUI 是 windows 子系统二进制，没有控制台；现场「点开即闪退」时这是唯一能拿到的
/// 证据。每次启动截断写一行版本/路径，panic 与启动错误追加。
fn install_startup_diagnostics() -> Option<std::path::PathBuf> {
    let dir = directories::BaseDirs::new().map_or_else(std::env::temp_dir, |base| {
        base.data_local_dir().join("rust-tunnel")
    });
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("gui-startup.log");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let cpus = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let header = format!(
        "\n=== rust-tunnel-client-gui {} starting ===\nexe: {:?}\nargs: {:?}\nprofile: {profile}, debug_assertions: {}, cpus: {cpus}",
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe(),
        std::env::args().collect::<Vec<_>>(),
        cfg!(debug_assertions),
    );
    // 追加而不是截断：Glow 失败后会用 wgpu 重启自身，两次启动的证据都要保留。
    append_startup_log(&path, &header);
    let hook_path = path.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&hook_path)
        {
            let _ = writeln!(
                file,
                "\n=== panic ===\n{info}\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
        show_startup_error_dialog(&format!("panic: {info}"));
        previous(info);
    }));
    Some(path)
}

/// 普通 GUI 的延迟诊断日志路径：`%LOCALAPPDATA%\rust-tunnel\latency.log`。
///
/// 与 [`install_startup_diagnostics`] 用同一个目录（`data_local_dir()/rust-tunnel`）。
fn latency_log_path() -> std::path::PathBuf {
    let dir = directories::BaseDirs::new().map_or_else(std::env::temp_dir, |base| {
        base.data_local_dir().join("rust-tunnel")
    });
    let _ = std::fs::create_dir_all(&dir);
    dir.join("latency.log")
}

/// 只收 `target == "latency"` 的追加文件 sink（**首次写入时才打开文件**）。
///
/// 普通 GUI 没有日志文件：stdout 在窗口程序里丢失，日志面板只有 500 条环形缓冲，而延迟诊断
/// 每秒会写若干行、很容易被正常日志冲掉。这个 writer 让现场可以直接把文件发给开发者。
/// 没开延迟日志时不会有任何事件写进来，也就不会创建空文件。
struct LatencyFileWriter {
    /// 目标路径。
    path: std::path::PathBuf,
    /// 惰性打开的文件句柄（首次 `write` 才 `create + append`）。
    file: Mutex<Option<std::fs::File>>,
}

impl LatencyFileWriter {
    /// 建一个惰性 writer（此刻不碰磁盘）。
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            file: Mutex::new(None),
        }
    }
}

impl std::io::Write for LatencyFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            *guard = Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        if let Some(file) = guard.as_mut() {
            file.write_all(buf)?;
            // 逐行落盘：诊断日志的价值在「进程被杀/断电后还在」，缓冲没有意义。
            file.flush()?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(file) = guard.as_mut() {
            std::io::Write::flush(file)?;
        }
        Ok(())
    }
}

/// 追加一段文本到启动日志（失败静默，诊断本身不能影响启动）。
fn append_startup_log(path: &std::path::Path, text: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{text}");
    }
}

/// 把 `run_app` 返回的错误追加到启动日志。
fn log_startup_error(path: &std::path::Path, err: &anyhow::Error) {
    append_startup_log(path, &format!("=== startup error ===\n{err:?}"));
}

/// 在 GUI 无法启动时弹一个系统错误框（现场没有控制台）。
#[cfg(target_os = "windows")]
fn show_startup_error_dialog(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let text: Vec<u16> = format!("{message}\n\n日志: %LOCALAPPDATA%\\rust-tunnel\\gui-startup.log")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let title: Vec<u16> = "Rust Tunnel 启动失败"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 两个字符串都以 NUL 结尾；空 HWND 表示无父窗口。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// 非 Windows 平台没有系统错误框；panic 仍会写日志。
#[cfg(not(target_os = "windows"))]
fn show_startup_error_dialog(_message: &str) {}

fn main() {
    // 诊断必须先于一切：即使下面的 COM/窗口初始化失败，也能留下版本与错误。
    let log_path = install_startup_diagnostics();

    // 远程数据面的 MFT/WGC 都用短 sleep 轮询；Windows 默认定时器粒度约 15.6 ms，
    // 会把 0.5–2 ms 的休眠放大一个数量级（表现为每帧凭空多出 15 ms 以上）。
    // 提到 1 ms 是进程级设置，进程退出后自动失效；失败的返回值不影响正确性。
    #[cfg(target_os = "windows")]
    {
        // SAFETY: `timeBeginPeriod` 只影响本进程的定时器分辨率，无指针参数、
        // 无前置条件；与进程生命周期绑定。
        let _ = unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) };
    }

    // 主线程先占住 STA：winit 创建窗口时会 `OleInitialize`，若主线程已被任何依赖
    // 初始化成 MTA，winit 会以 `RPC_E_CHANGED_MODE` **直接 panic**。先初始化 STA
    // 让 winit 那次调用幂等地返回 `S_FALSE`。
    #[cfg(target_os = "windows")]
    {
        // SAFETY: `OleInitialize` 只影响当前线程，且必须在窗口/COM 使用之前调用。
        let hr = unsafe { windows_sys::Win32::System::Ole::OleInitialize(std::ptr::null_mut()) };
        if hr < 0 {
            if let Some(path) = log_path.as_deref() {
                append_startup_log(path, &format!("OleInitialize 失败: HRESULT=0x{hr:08X}"));
            }
        }
    }

    if let Err(err) = run_app(log_path.as_deref()) {
        if let Some(path) = log_path.as_deref() {
            log_startup_error(path, &err);
        }
        show_startup_error_dialog(&format!("启动失败: {err}"));
        std::process::exit(1);
    }
}

/// 把 wgpu 枚举到的 adapter 写进启动日志。
///
/// 现场高 CPU 诊断：如果选中的是 WARP/软件适配器（`device_type: Cpu`），
/// 高 CPU 是软件光栅化的必然结果，不是 GUI 逻辑在空转。
#[cfg(target_os = "windows")]
fn log_wgpu_adapters(path: &std::path::Path) {
    let instance = egui_wgpu::wgpu::Instance::default();
    let adapters = instance.enumerate_adapters(egui_wgpu::wgpu::Backends::all());
    if adapters.is_empty() {
        append_startup_log(path, "wgpu: 没有枚举到任何 adapter");
        return;
    }
    for adapter in adapters {
        let info = adapter.get_info();
        append_startup_log(
            path,
            &format!("wgpu adapter: {}", egui_wgpu::adapter_info_summary(&info)),
        );
    }
}

/// 非 Windows 平台没有 wgpu 回退，不需要枚举 adapter。
#[cfg(not(target_os = "windows"))]
fn log_wgpu_adapters(_path: &std::path::Path) {}

/// 用指定 renderer 重新拉起自身（`value` 为 `"wgpu"` 或 `"glow"`）。
///
/// 两个方向各有用处：Glow 依赖 OpenGL 驱动，部分 Windows 10 机器（老驱动、虚拟机、远程
/// 桌面）没有可用的 GL 配置（`Found no glutin configs`）；反过来 wgpu 在个别驱动上也会
/// 失败，此时退回 Glow 至少能让界面起来（NV12 会自动走 CPU 回退）。重启而不是进程内重试，
/// 是因为 `run_native` 会消费掉 `FnOnce` 的 app 构造器。
///
/// 环境变量就是「本次尝试」的标记：子进程读到它后不会再朝同一方向重试，故不会形成循环。
fn relaunch_with_renderer(value: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1));
    command.env("RUST_TUNNEL_RENDERER", value);
    match command.spawn() {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(error = %err, renderer = value, "以指定 renderer 重新启动失败");
            false
        }
    }
}

/// 选择 eframe 渲染器。
///
/// - Windows 上**默认 wgpu**（DX12）：NV12 着色器上屏需要它，且它比 Glow 更能覆盖老驱动 /
///   虚拟机 / 远程桌面（后者没有可用 GL 配置）。
/// - 非 Windows 仍默认 Glow（现状不变）。
/// - `RUST_TUNNEL_RENDERER=glow|wgpu` 显式覆盖；wgpu 只在 Windows 上被采纳。
fn select_renderer() -> eframe::Renderer {
    let requested = std::env::var("RUST_TUNNEL_RENDERER").ok();
    if cfg!(target_os = "windows") {
        match requested.as_deref() {
            Some("glow") => eframe::Renderer::Glow,
            _ => eframe::Renderer::Wgpu,
        }
    } else {
        eframe::Renderer::Glow
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "GUI 启动路径：参数分派、托盘、远程引擎、renderer 选择与回退是一条不可分割的启动链"
)]
fn run_app(log_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    // DPI 感知必须在**任何窗口/采集初始化之前**设置：否则 150% 缩放的 4K 屏会被
    // Win32 报成 2560×1440，采集坐标与输入坐标整体错位（见 win_video 模块文档）。
    #[cfg(target_os = "windows")]
    remote::win_video::enable_per_monitor_dpi_awareness();

    // argv 分派放在**最前**：早于 DISPLAY 检查、早于托盘与 eframe——`--instance` 是无头的，
    // 走到托盘/eframe 就会起窗口（那是它明确不该做的事）。
    match parse_launch_mode(std::env::args().skip(1)) {
        Ok(LaunchMode::Instance(opts)) => return instance::run(&opts),
        // 自更新收尾：无窗口、无桌面要求，必须在托盘/eframe 之前分派。
        Ok(LaunchMode::ApplyUpdate(target)) => {
            return update::run_apply_update(&target);
        }
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
    let mut prefs = prefs::load();
    // 延迟诊断日志：环境变量只作默认值；偏好在启动时显式打开则覆盖它（运行中可在
    // 设置页随时切换，见 `rust_tunnel_remote::latency::set_mode`）。
    if prefs.remote_latency_log {
        rust_tunnel_remote::latency::set_mode(if prefs.remote_latency_log_frames {
            rust_tunnel_remote::latency::Mode::Frames
        } else {
            rust_tunnel_remote::latency::Mode::Summary
        });
    }
    // 截图评审/自动化：RUST_TUNNEL_GUI_THEME=system|light|dark 覆盖本次运行的主题偏好
    // （不落盘；正常启动不设置该变量）。
    if let Ok(value) = std::env::var("RUST_TUNNEL_GUI_THEME") {
        prefs.theme = match value.to_ascii_lowercase().as_str() {
            "light" => prefs::ThemePref::Light,
            "dark" => prefs::ThemePref::Dark,
            _ => prefs::ThemePref::System,
        };
    }
    // 「跳过此版本」镜像必须在**控制通道启动前**就位：托盘线程 50 ms 一轮，
    // 可能在 `GuiApp` 构造前就看到服务端推送的可用更新。
    app_state
        .update
        .set_skipped(prefs.update_skipped_version.clone());

    // 后台 tokio 运行时（控制通道/隧道 + 远程控制引擎）。
    // 截图评审模式（`RUST_TUNNEL_GUI_DEMO=1`）**不启动运行时**：不连服务端、不注册身份，
    // 只把一份合成状态塞进 `AppState`，避免与用户真实在跑的客户端抢同一个客户端名。
    if preview::enabled() {
        preview::seed(&app_state);
    } else {
        spawn_runtime(
            app_state.clone(),
            status_tx,
            log_buffer,
            reconnect_flag,
            prefs.allow_remote,
            prefs.remote_approval,
            prefs.remote_auto_lock_on_disconnect,
            prefs.remote_native_resolution,
            // H.265 偏好与环境变量禁用合成生效值：环境变量优先（现场排障开关）。
            remote::effective_prefer_h265(prefs.remote_prefer_h265, remote::h265_disabled_by_env()),
            // 文字优先（4:4:4）：开启后协商优先选 VP9（4:4:4），压过 H.264/H.265。
            prefs.remote_prefer_444,
            prefs.remote_p2p_upgrade,
            prefs.remote_audio_enabled,
            prefs.remote_audio_allowed,
            prefs.remote_audio_volume,
        );
    }

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

    // 托盘事件由独立线程消费：关窗后原生窗口被隐藏，Windows 不再投递 WM_PAINT，
    // eframe 的 `update` 随之停摆——在那之前，托盘完全依赖 `update` 轮询菜单事件，
    // 于是「点托盘没反应、『退出』也没反应」（见 `tray_worker` 模块文档）。
    let tray_bridge = Arc::new(tray_worker::TrayBridge::new(prefs.allow_remote));
    tray_worker::spawn(Arc::clone(&tray_bridge), Arc::clone(&app_state));

    let viewport = {
        let mut vp = egui::ViewportBuilder::default()
            .with_inner_size([760.0, 600.0])
            .with_min_inner_size([480.0, 430.0])
            .with_visible(true);
        if let Some(icon) = load_window_icon() {
            vp = vp.with_icon(icon);
        }
        vp
    };

    // Windows 默认 wgpu（NV12 着色器上屏需要它），可用 RUST_TUNNEL_RENDERER 覆盖。
    let renderer = select_renderer();
    let renderer_env = std::env::var("RUST_TUNNEL_RENDERER").ok();
    if let Some(path) = log_path {
        append_startup_log(path, &format!("renderer: {renderer:?}"));
        if matches!(renderer, eframe::Renderer::Wgpu) {
            log_wgpu_adapters(path);
        }
    }

    // 上屏交换链只保留 1 帧在排队（wgpu 默认 2）。远程画面是**实时**内容：多排一帧就等于
    // 把每一帧的上屏时间再推迟一个刷新周期（60 Hz 下约 16 ms），而平滑度收益几乎为零
    // （画面到达本就被采集节拍打散）。egui-wgpu 的 `AutoVsync` 保持不变——窗口模式下由
    // DWM 合成，降延迟不需要关 vsync。
    let wgpu_options = egui_wgpu::WgpuConfiguration {
        desired_maximum_frame_latency: Some(1),
        ..Default::default()
    };
    let options = eframe::NativeOptions {
        viewport,
        renderer,
        wgpu_options,
        ..Default::default()
    };

    // 闭包要 `'static`，故把日志路径拷一份带进去（`log_path` 后面错误分支还要用）。
    let startup_log = log_path.map(std::path::Path::to_path_buf);
    let result = eframe::run_native(
        "Rust Tunnel",
        options,
        Box::new(move |cc| {
            fonts::setup_fonts(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, prefs.theme);
            // 把 HWND 与 egui 上下文交给托盘线程：隐藏窗口的还原必须走原生调用。
            tray_bridge.attach(cc);
            // `wgpu_render_state` 是「本次运行真的在 wgpu 上」的唯一判据：有它才能用 GPU
            // 着色器上屏，否则 NV12 走 CPU 回退。把目标格式记进启动日志，便于现场确认
            // sRGB/非 sRGB 选对了片元入口。
            let wgpu_target_format = cc
                .wgpu_render_state
                .as_ref()
                .map(|state| state.target_format);
            if let Some(path) = startup_log.as_deref() {
                if let Some(format) = wgpu_target_format {
                    append_startup_log(path, &format!("wgpu target format: {format:?}"));
                    append_startup_log(path, "NV12 着色器上屏已启用");
                } else {
                    append_startup_log(path, "NV12 走 CPU 回退（无 wgpu render state）");
                }
            }
            Ok(Box::new(GuiApp::new(
                app_state,
                tray,
                tray_bridge,
                prefs,
                wgpu_target_format,
            )))
        }),
    );

    if let Err(err) = result {
        let message = err.to_string();
        if cfg!(target_os = "windows") {
            // 默认 wgpu 失败（个别老驱动/虚拟机）→ 用 Glow 重试一次。仅当用户没显式指定
            // renderer 时这么做，避免覆盖用户的选择。
            if matches!(renderer, eframe::Renderer::Wgpu) && renderer_env.is_none() {
                if let Some(path) = log_path {
                    append_startup_log(
                        path,
                        &format!("eframe(wgpu) 失败，改用 glow 重新启动:\n{message}"),
                    );
                }
                if relaunch_with_renderer("glow") {
                    std::process::exit(0);
                }
            }
            // Glow/OpenGL 配置不可用时自动切 wgpu（保留旧回退；已显式选 wgpu 时不再回头）。
            if matches!(renderer, eframe::Renderer::Glow)
                && renderer_env.as_deref() != Some("wgpu")
                && message.contains("glutin")
            {
                if let Some(path) = log_path {
                    append_startup_log(
                        path,
                        &format!("eframe(glow) 失败，改用 wgpu 重新启动:\n{message}"),
                    );
                }
                if relaunch_with_renderer("wgpu") {
                    std::process::exit(0);
                }
            }
        }
        return Err(anyhow::anyhow!("{message}"));
    }

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

    /// 自更新收尾模式：只接受一个目标路径，且不能与其他模式混用。
    #[test]
    fn apply_update_mode_is_internal_and_exclusive() {
        let target = r"C:\Program Files\rust-tunnel\rust-tunnel-client-gui.exe";
        let parsed = parse_launch_mode(args(&["--apply-update", target])).unwrap();
        let LaunchMode::ApplyUpdate(path) = parsed else {
            panic!("应当是自更新模式");
        };
        assert_eq!(path, PathBuf::from(target));

        // 缺目标路径
        assert!(parse_launch_mode(args(&["--apply-update"])).is_err());
        // 不能与实例模式混用
        let err = parse_launch_mode(args(&["--instance", "--apply-update", target])).unwrap_err();
        assert!(err.contains("不能与其他参数一起用"), "{err}");
    }
}
