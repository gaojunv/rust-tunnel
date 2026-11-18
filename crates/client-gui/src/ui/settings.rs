//! 设置页：页头 + 顶部分组 Tab + 下方滚动内容（Win11 Fluent 布局）。
//!
//! 分组的意义是把**不同后果的东西分开**：连接（改了就重连）、远程编码（影响下一次会话）、
//! 无人值守（装一个 SYSTEM 服务，后果不可逆）、更新（换掉本机 exe）。这四件事的失败模式
//! 差别极大，混在一列长表单里会让「装服务」和「改服务器地址」看起来同等轻重。

use std::sync::Arc;

use egui::{RichText, Ui};
use rust_tunnel_client::ClientConfig;
use rust_tunnel_remote::{ApprovalPolicy, RemoteManager};

use super::icons::Icon;
use super::theme::palette;
use super::widgets::{self, ButtonKind, Tone};
use crate::prefs::GuiPrefs;
use crate::service_ui::{
    host_config_advisory, host_config_path_display, host_config_presence, owner_label,
    presence_label, start_blocked_reason, HostSeed, ServiceOp, ServiceOps, SERVICE_SUPPORTED,
};
use crate::service_watch::ServicePresence;
use crate::{app::AppState, config_store};

/// 设置页的分组（顶部分组 Tab）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsPage {
    /// 连接：服务器地址 / 客户端名 / token 与传输安全。
    #[default]
    Connection,
    /// 远程编码：被控端的画质与分辨率偏好。
    Remote,
    /// 无人值守被控端（Windows 服务）。
    Service,
    /// 客户端自更新。
    Update,
}

impl SettingsPage {
    /// 全部分组（顺序即 Tab 顺序）。
    pub const ALL: [Self; 4] = [Self::Connection, Self::Remote, Self::Service, Self::Update];

    /// Tab 标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connection => "连接",
            Self::Remote => "远程",
            Self::Service => "无人值守",
            Self::Update => "更新",
        }
    }
}

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

/// 设置页入口：页头 + 顶部分组 Tab + 下方滚动内容。
///
/// 返回 `(偏好是否变化需落盘, 更新动作)`。调用方负责在第一个元素为 `true` 时
/// [`crate::prefs::save`]，并在第二个元素非空时执行更新动作。
#[must_use]
#[expect(
    clippy::too_many_arguments,
    reason = "设置页是各子系统的汇合点（配置 / 偏好 / 远程引擎 / 服务 / 更新）；包一层结构体只是把同一份借用搬个位置"
)]
pub fn show(
    ui: &mut Ui,
    page: &mut SettingsPage,
    state: &mut SettingsState,
    app_state: &Arc<AppState>,
    prefs: &mut GuiPrefs,
    manager: Option<&Arc<RemoteManager>>,
    service: &mut ServiceOps,
    update: &crate::update::UpdateBridge,
) -> (bool, Option<crate::ui::update::UpdateAction>) {
    let mut prefs_changed = false;
    let mut action = None;
    let p = palette(ui.style());

    widgets::page_header(
        ui,
        "设置",
        "客户端、远程编码与无人值守",
        |ui| {
            if let Some(cfg) = app_state.config_snapshot() {
                ui.label(RichText::new(cfg.server).size(12.0).color(p.text_weak));
            }
        },
    );

    // 顶部分组 Tab：与「远程」页同一套分段控件，直接左对齐排布（**不要**用裸 `with_layout`
    // 做右对齐——它会吃掉父级剩余高度，把下面的滚动区压成零高）。标签来自枚举本身，
    // 避免这里和 `ALL` / `label()` 各维护一份顺序。
    let tabs: Vec<_> = SettingsPage::ALL
        .iter()
        .map(|tab| (*tab, tab.label()))
        .collect();
    if let Some(next) = widgets::segmented(ui, *page, &tabs) {
        *page = next;
    }
    ui.add_space(10.0);

    // 每个分组各自记住滚动位置（id 带标签）：从长长的「无人值守」切回「连接」时不会
    // 停在半截，切回去时也能回到原来的位置。
    egui::ScrollArea::vertical()
        .id_salt(("settings_page", page.label()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(2.0);
            match *page {
                SettingsPage::Connection => connection_page(ui, state, app_state),
                SettingsPage::Remote => {
                    prefs_changed |= remote_page(ui, prefs, manager);
                }
                SettingsPage::Service => service_page(ui, prefs, app_state, service),
                SettingsPage::Update => {
                    let (changed, picked) = crate::ui::update::settings_section(ui, update, prefs);
                    prefs_changed |= changed;
                    action = picked;
                }
            }
            ui.add_space(8.0);
        });

    (prefs_changed, action)
}

/// 「连接」组：服务器身份与传输安全。
#[expect(
    clippy::too_many_lines,
    reason = "连接表单：三段输入 + 五个开关 + 逐字保留的保存流程，拆开会让「保存做了什么」散成几处"
)]
fn connection_page(ui: &mut Ui, state: &mut SettingsState, app_state: &Arc<AppState>) {
    let p = palette(ui.style());
    widgets::card_titled(
        ui,
        "服务器连接",
        Some("服务器地址、客户端身份与传输安全"),
        |ui| {
            widgets::labeled_row(ui, "服务器地址", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.server)
                        .desired_width(ui.available_width().min(380.0)),
                );
            });
            ui.add_space(2.0);
            widgets::labeled_row(ui, "客户端名", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.name)
                        .desired_width(ui.available_width().min(380.0)),
                );
            });
            ui.add_space(2.0);
            widgets::labeled_row(ui, "密码 / Token", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.password)
                        .password(!state.remember_password)
                        .desired_width(ui.available_width().min(380.0)),
                );
            });

            ui.add_space(8.0);
            widgets::switch_row(
                ui,
                "显示密码",
                Some("在界面上明文显示密码 / Token"),
                &mut state.remember_password,
            );
            widgets::switch_row(
                ui,
                "存入系统钥匙串",
                Some("密码交给系统钥匙串保管，配置文件里不再留明文"),
                &mut state.save_to_keyring,
            );
            widgets::switch_row(
                ui,
                "启用 TLS",
                Some("控制通道使用 TLS 加密（服务端为自签证书）"),
                &mut state.tls,
            );
            widgets::switch_row(
                ui,
                "跳过证书校验 (TOFU)",
                Some("首次连接即信任服务端证书；仅在服务端证书自签且无固定指纹时开启"),
                &mut state.tls_insecure,
            );
            widgets::switch_row(
                ui,
                "启用 Agent 执行器（远程命令/文件操作）",
                None,
                &mut state.enable_agent,
            );

            ui.add_space(8.0);
            let can_save = !state.server.trim().is_empty() && !state.password.trim().is_empty();
            // 禁用态用透明度弱化：`primary_button` 在 disabled 的 `Ui` 里不会响应点击，
            // 但强调色填充仍然抢眼，看不出「现在不能点」。
            let save = ui
                .add_enabled_ui(can_save, |ui| {
                    if !can_save {
                        ui.set_opacity(0.45);
                    }
                    widgets::primary_button(ui, "保存并重连")
                })
                .inner;
            if save.clicked() {
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
                                warn_msg = Some(format!("钥匙串写入失败（密码已明文落盘）: {e}"));
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
            if !can_save {
                ui.label(
                    RichText::new("请填写服务器与密码后保存")
                        .size(12.0)
                        .color(p.text_weak),
                );
            }
            if let Some(msg) = state.status_msg.clone() {
                ui.add_space(4.0);
                widgets::info_bar(ui, status_tone(&msg), None, &msg);
            }
            ui.add_space(2.0);
            ui.label(
                RichText::new(format!(
                    "配置路径: {}",
                    config_store::default_path().display()
                ))
                .monospace()
                .size(12.0)
                .color(p.text_weak),
            );
        },
    );
}

/// 保存结果的语义色调：成功绿、失败红、钥匙串降级这类「成功但有代价」用警告色。
fn status_tone(message: &str) -> Tone {
    if message.contains("失败") {
        if message.contains("正在重连") {
            Tone::Warn
        } else {
            Tone::Err
        }
    } else if message.contains("必填") {
        Tone::Err
    } else {
        Tone::Ok
    }
}

/// 「远程」组：被控端编码选项。返回偏好是否变化（调用方负责落盘）。
///
/// 原生分辨率的语义有意保守：**只对硬件 H.264 生效**（VP9 软件解码 4K 在控制器侧
/// 37–58 ms/帧），且只影响**下一次**会话（运行中的会话不重建编码器、不插 IDR）。
fn remote_page(ui: &mut Ui, prefs: &mut GuiPrefs, manager: Option<&Arc<RemoteManager>>) -> bool {
    let p = palette(ui.style());
    let mut changed = false;
    widgets::card_titled(
        ui,
        "远程控制 · 编码与诊断",
        Some("本机作为被控端时生效；运行中修改在下次会话生效"),
        |ui| {
            let mut on = prefs.remote_native_resolution;
            if widgets::switch_row(
                ui,
                "原生分辨率（不缩放到 1080p，仅硬件 H.264）",
                None,
                &mut on,
            ) {
                prefs.remote_native_resolution = on;
                if let Some(manager) = manager {
                    manager.set_native_resolution(on);
                }
                tracing::info!("原生分辨率（4K）：{}", if on { "开" } else { "关" });
                changed = true;
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "本机作为被控端时生效：显示宽度 >1920 时按原生像素编码（≤1920 无变化），\
                     码率上限翻倍（封顶 30 Mbps）。关掉会把 4K/2K 桌面缩到 1920——文字细节在\
                     编码前就丢了，之后再高的码率也补不回来。对端只支持 VP9 时仍按 1080p。\
                     运行中修改在下次会话生效。",
                )
                .size(12.0)
                .color(p.text_weak),
            );

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            // 延迟诊断日志：进程内可变，勾选立即生效（见 `rust_tunnel_remote::latency`）。
            let mut latency_on = prefs.remote_latency_log;
            let mut latency_frames = prefs.remote_latency_log_frames;
            let mut latency_changed = widgets::switch_row(
                ui,
                "延迟诊断日志（分阶段耗时）",
                Some("在两台机器上分别打开；运行中切换立即生效，无需重启。"),
                &mut latency_on,
            );
            ui.add_enabled_ui(latency_on, |ui| {
                latency_changed |= widgets::switch_row(
                    ui,
                    "详细：每帧一行（量大）",
                    None,
                    &mut latency_frames,
                );
            });
            ui.label(
                RichText::new(
                    "日志级别需 ≥ info。GUI 模式写入 %LOCALAPPDATA%\\rust-tunnel\\latency.log，\
                     无人值守实例写入 %ProgramData%\\rust-tunnel\\instance.log。",
                )
                .size(12.0)
                .color(p.text_weak),
            );
            if latency_changed {
                prefs.remote_latency_log = latency_on;
                prefs.remote_latency_log_frames = latency_frames;
                let mode = if !latency_on {
                    rust_tunnel_remote::latency::Mode::Off
                } else if latency_frames {
                    rust_tunnel_remote::latency::Mode::Frames
                } else {
                    rust_tunnel_remote::latency::Mode::Summary
                };
                rust_tunnel_remote::latency::set_mode(mode);
                tracing::info!("延迟诊断日志：{mode:?}");
                changed = true;
            }
        },
    );
    changed
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

/// [`RISK_NOTES`] 的首行标题（信息条把它画成 Fluent 的警告标题）。
const RISK_TITLE: &str = "⚠ 启用前必读";

/// 风险正文：去掉 [`RISK_NOTES`] 首行标题后的原文。
///
/// **常量本身一字未改**（单测的机械锁盯着它），这里只是把首行拆出来给 Fluent 信息条当标题，
/// 避免同一个标题在卡片里显示两遍。
fn risk_body() -> &'static str {
    RISK_NOTES
        .split_once("\n\n")
        .map_or(RISK_NOTES, |(_head, body)| body)
}

/// 「无人值守」组：服务状态、装/卸/启/停与**必读风险**。
#[allow(
    clippy::too_many_lines,
    reason = "服务一节：状态 + 按钮 + 必读风险，拆散会割裂「看懂后果」这条主线"
)]
fn service_page(ui: &mut Ui, prefs: &GuiPrefs, app_state: &Arc<AppState>, service: &mut ServiceOps) {
    let p = palette(ui.style());
    if !SERVICE_SUPPORTED {
        // 非 Windows 上这一整套都不成立：没有 Windows 服务、没有安全桌面、没有 UAC。
        widgets::card_titled(ui, "无人值守被控端（Windows 服务）", None, |ui| {
            ui.label(
                RichText::new(
                    "此功能仅在 Windows 上可用：它靠一个以 LocalSystem 运行的 Windows 服务，\
                         把被控实例拉进控制台会话，从而覆盖登录界面与提权窗口（如任务管理器）。",
                )
                .size(13.0)
                .color(p.text_weak),
            );
        });
        return;
    }

    let presence = service.presence();
    widgets::card_titled(
        ui,
        "无人值守被控端（Windows 服务）",
        Some("以 LocalSystem 运行，覆盖登录界面与提权窗口"),
        |ui| {
            widgets::key_value(ui, "服务状态", presence_label(presence), false);
            widgets::key_value(
                ui,
                "客户端身份",
                owner_label(app_state.service_owner()),
                false,
            );
            ui.horizontal(|ui| {
                ui.add_sized(
                    [widgets::LABEL_WIDTH, 20.0],
                    egui::Label::new(RichText::new("配置文件").size(12.0).color(p.text_weak)),
                );
                ui.label(
                    RichText::new(host_config_path_display())
                        .monospace()
                        .size(12.0)
                        .color(p.text_weak),
                );
            });

            ui.add_space(10.0);
            let busy = service.in_flight();
            // 「安装并启用」需要的当前客户端配置快照。拿不到时退化成老流程（只安装，
            // host.toml 由用户手工写）——不要因为配置没读出来就把安装按钮整个藏掉。
            let seed = app_state
                .config_snapshot()
                // 空配置（用户还没填完/没保存）不能生成 host.toml：否则服务起来后
                // 注册必然失败，而用户看到的是「服务在跑」。
                .filter(|config| !config.server.trim().is_empty() && !config.password.is_empty())
                .map(|config| {
                    let approval = app_state
                        .remote()
                        .map_or(ApprovalPolicy::Confirm, |manager| manager.approval_policy());
                    let auto_lock = app_state
                        .remote()
                        .is_some_and(|manager| manager.auto_lock_on_disconnect());
                    // 原生分辨率是**被控端本机**偏好：装服务时原样带进 host.toml，否则
                    // 用户在设置里关掉它、装完的实例却仍然编 4K（或反过来）。
                    let native = app_state
                        .remote()
                        .is_some_and(|manager| manager.native_resolution());
                    HostSeed::from_config(
                        &config,
                        approval,
                        auto_lock,
                        native,
                        prefs.remote_latency_log,
                        prefs.remote_latency_log_frames,
                    )
                });
            // `Absent` = 确定没装；其余（含 `Unknown`）按「已装」处理，多给几个按钮总比
            // 在查询偶发失败时把「卸载」藏起来强。
            let installed = presence != ServicePresence::Absent;
            // 服务已停止时允许重新安装：它是修「host.toml 写错了 / token 换了 / 审批策略
            // 要改」的唯一 GUI 路径。**Running 时不放开**——`--install` 会先停服务，等于
            // 掐断正在进行的远程会话。
            let can_reinstall = presence == ServicePresence::Stopped;
            let install_enabled = busy.is_none() && (!installed || can_reinstall);
            // 三态：只有**确定**缺失才拦（`Unknown` 放行——本窗口多半读不到那个目录，
            // 见 `HostConfigPresence`）。
            let config_presence = host_config_presence();
            let blocked = start_blocked_reason(config_presence);
            let advisory = host_config_advisory(config_presence);
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(install_enabled, |ui| {
                    if !install_enabled {
                        ui.set_opacity(0.45);
                    }
                    // 有配置快照时是「安装 + 写 host.toml + 启动」三合一，只弹一次 UAC。
                    let label = if installed {
                        "重新安装并启用"
                    } else if seed.is_some() {
                        "安装并启用（推荐）"
                    } else {
                        "安装服务"
                    };
                    if widgets::primary_button(ui, label).clicked() {
                        service.spawn_install(seed.clone());
                    }
                });
                // 「启动」在 host.toml **确定**缺失时禁用：服务一起来本窗口就转被动，
                // 而实例读不到配置也注册不了——整机就此失去身份（见 start_blocked_reason）。
                // `Unknown` 放行：读不到目录不等于文件不存在（见 HostConfigPresence）。
                let start_enabled = busy.is_none() && installed && blocked.is_none();
                ui.add_enabled_ui(start_enabled, |ui| {
                    if !start_enabled {
                        ui.set_opacity(0.45);
                    }
                    if widgets::button(ui, "启动服务", Some(Icon::Play), ButtonKind::Secondary)
                        .clicked()
                    {
                        service.spawn_op(ServiceOp::Start);
                    }
                });
                let manage_enabled = busy.is_none() && installed;
                ui.add_enabled_ui(manage_enabled, |ui| {
                    if !manage_enabled {
                        ui.set_opacity(0.45);
                    }
                    if widgets::button(ui, "停止服务", Some(Icon::Stop), ButtonKind::Secondary)
                        .clicked()
                    {
                        service.spawn_op(ServiceOp::Stop);
                    }
                    if widgets::button(ui, "卸载服务", Some(Icon::Trash), ButtonKind::Danger)
                        .clicked()
                    {
                        service.spawn_op(ServiceOp::Uninstall);
                    }
                });
            });
            if !installed || can_reinstall {
                ui.add_space(2.0);
                match seed.as_ref() {
                    Some(seed) => {
                        ui.label(
                            RichText::new(
                                "「安装并启用」会用当前客户端配置（服务器 / token / 客户端名）\
                                 自动生成 host.toml，并在安装成功后立即启动服务。",
                            )
                            .size(12.0)
                            .color(p.text_weak),
                        );
                        if seed.approval() == ApprovalPolicy::Confirm {
                            widgets::info_bar(
                                ui,
                                Tone::Warn,
                                None,
                                "当前接入确认为「需要确认」：无头实例没有人能点确认弹窗，\
                                 接入请求会在 45 秒后超时被拒。请先在「远程」Tab 切到\
                                 「自动接受」再启用。",
                            );
                        }
                    }
                    None => {
                        ui.label(
                            RichText::new(
                                "还没有可用的客户端配置，无法自动生成 host.toml：请先填写服务器\
                                 与 token，或安装后手工写 host.toml。",
                            )
                            .size(12.0)
                            .color(p.text_weak),
                        );
                    }
                }
            }
            if let Some(reason) = blocked {
                ui.add_space(2.0);
                widgets::info_bar(ui, Tone::Warn, None, reason);
            }
            // `Unknown` 的补充说明是**信息**不是警告：它是「本窗口读不到那个目录」这件事
            // 的解释，不是「你做错了什么」。故用弱色而非警告色。
            if let Some(note) = advisory {
                ui.add_space(2.0);
                ui.label(RichText::new(note).size(12.0).color(p.text_weak));
            }
            if let Some(op) = busy {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "正在{}…（界面保持可用，完成后结果显示在这里）",
                        op.label()
                    ));
                });
            }
            if let Some(message) = service.last_message() {
                ui.add_space(4.0);
                ui.label(RichText::new(message).size(12.0));
            }

            ui.add_space(8.0);
            widgets::divider(ui);
            ui.add_space(6.0);
            // 风险说明放在卡片**内部**、按钮下方：用户点「安装并启用」前不可能漏看。
            widgets::info_bar(ui, Tone::Warn, Some(RISK_TITLE), risk_body());
        },
    );
}

#[cfg(test)]
mod tests {
    //! 纯文案的机械锁：这五条风险是**要求的内容**，删掉或改淡都是回归。

    use super::{risk_body, SettingsPage, RISK_NOTES, RISK_TITLE};

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

    /// 风险正文必须来自常量本身（标题只拆首行，不得另写一份会漂移的副本）。
    #[test]
    fn risk_body_is_the_constant_without_its_title_line() {
        assert_eq!(risky_head(), RISK_TITLE);
        assert!(
            risk_body().starts_with("1)"),
            "正文应保留编号列表：{}",
            risk_body()
        );
        assert!(RISK_NOTES.contains(risk_body()), "正文必须是常量的一部分");
    }

    fn risky_head() -> &'static str {
        RISK_NOTES
            .split_once("\n\n")
            .map_or(RISK_NOTES, |(head, _body)| head)
    }

    /// 四个分组齐全、标签互不相同、默认落在「连接」。
    #[test]
    fn settings_pages_cover_every_group() {
        assert_eq!(SettingsPage::default(), SettingsPage::Connection);
        assert_eq!(SettingsPage::ALL.len(), 4);
        let mut labels = [
            SettingsPage::Connection.label(),
            SettingsPage::Remote.label(),
            SettingsPage::Service.label(),
            SettingsPage::Update.label(),
        ];
        labels.sort_unstable();
        for pair in labels.windows(2) {
            assert_ne!(pair[0], pair[1], "分组标签不得重复");
        }
    }
}
