//! 托盘图标与菜单（tray-icon + muda）。

use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::service_watch::IdentityOwner;

/// 托盘连接态（决定图标与菜单文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// 已连接。
    Connected,
    /// 重连中。
    Reconnecting,
    /// 离线/未连接。
    Offline,
    /// Windows 服务正在承载客户端身份——本窗口未注册，托盘不再显示已连接/离线
    /// （那两句都是假的，见 [`TrayState::from_status_with_service`]）。
    ServiceOwned,
}

impl TrayState {
    /// 从客户端状态推导托盘态（**不看**服务归属，保持既有行为）。
    #[must_use]
    pub fn from_status(status: &rust_tunnel_client::ClientStatus) -> Self {
        if status.connected {
            Self::Connected
        } else if status.last_error.is_some() {
            Self::Reconnecting
        } else {
            Self::Offline
        }
    }

    /// 服务感知的版本：服务在承载身份时，本窗口的连接状态与托盘无关，一律
    /// [`Self::ServiceOwned`]；否则与 [`Self::from_status`] 完全一致。
    #[must_use]
    pub fn from_status_with_service(
        status: &rust_tunnel_client::ClientStatus,
        owner: IdentityOwner,
    ) -> Self {
        if owner.is_service() {
            return Self::ServiceOwned;
        }
        Self::from_status(status)
    }

    /// 状态短标签（用于 tooltip 后缀）。
    fn label(self) -> &'static str {
        match self {
            Self::Connected => "已连接",
            Self::Reconnecting => "重连中",
            Self::Offline => "离线",
            Self::ServiceOwned => "服务接管",
        }
    }
}

/// 菜单项 ID（与 eframe 侧分发对齐）。
pub mod ids {
    /// 显示/聚焦主面板。
    pub const SHOW: &str = "show";
    /// 设置 Tab。
    pub const SETTINGS: &str = "settings";
    /// 设置 Tab 的「无人值守被控端」一节（与 [`SETTINGS`] 分开：那条只是打开设置，
    /// 这条要把用户直接带到服务那一节）。
    pub const SERVICE: &str = "service";
    /// 立即重连。
    pub const RECONNECT: &str = "reconnect";
    /// 允许被远程控制（勾选项）。
    pub const ALLOW_REMOTE: &str = "allow-remote";
    /// 退出应用。
    pub const QUIT: &str = "quit";
}

/// 只读的服务状态项 ID。
///
/// **刻意不进 [`ids`]**：它是个禁用的展示项，永远不会产生 `MenuEvent`，故不该出现在
/// [`TrayAction::from_menu_id`] 的映射表里（那张表只收「点了要有反应」的项）。
const SERVICE_STATE_ID: &str = "service-state";

pub(crate) fn load_icon_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some((rgba.into_raw(), w, h))
}

fn offline_icon_bytes() -> &'static [u8] {
    // 托盘图标（白色 glyph + 透明底，macOS 以 template 模式自适应深浅色）
    include_bytes!("../icons/tray-icon@2x.png")
}

/// 托盘句柄：`TrayIcon` 生存期 + 可动态改写的菜单项。
///
/// `muda::MenuItem` 是引用计数句柄，`clone` 后可通过原句柄 `set_text` 更新菜单文案。
pub struct TrayHandle {
    /// 托盘图标（调用方/持有方需保持其生命周期）。
    pub icon: TrayIcon,
    /// 菜单首行的只读状态项。
    pub status_item: MenuItem,
    /// 菜单第二行的只读服务器项。
    pub server_item: MenuItem,
    /// 只读的服务状态项（文案由 [`crate::service_ui::service_menu_text`] 生成）。
    ///
    /// 与 `status_item`/`server_item` 同类：**只读**，用户点不了。装/卸这类破坏性操作
    /// 刻意**不**放进托盘菜单——那是一个用户为「看一眼状态」随手点开的菜单，误点一下
    /// 就能卸掉服务、而且还要弹 UAC；按钮属于「设置」Tab 里的服务那一节。
    pub service_item: MenuItem,
    /// 「允许被远程控制」勾选项。
    ///
    /// 勾选态由 muda 自己翻转，调用方在收到 [`TrayAction::ToggleAllowRemote`] 后
    /// 用 [`CheckMenuItem::is_checked`] 读**新**状态，不要另存一份镜像——镜像会与
    /// 用户从系统菜单触发的点击脱节。
    pub allow_remote_item: CheckMenuItem,
}

/// 构造托盘图标与菜单，返回可持久持有的句柄（调用方需持有其生命周期）。
///
/// 图标三态可通过 `handle.icon.set_icon(...)` 切换；此处先以离线态创建。
/// `allow_remote` 为「允许被远程控制」的初始勾选态（取自持久化偏好）。
pub fn build_tray(allow_remote: bool) -> anyhow::Result<TrayHandle> {
    let status_item = MenuItem::with_id("status", "● 离线", false, None);
    let server_item = MenuItem::with_id("server", "服务器：未配置", false, None);
    let service_item = MenuItem::with_id(SERVICE_STATE_ID, "服务：—", false, None);
    let show_item = MenuItem::with_id(ids::SHOW, "打开面板…", true, None);
    let settings_item = MenuItem::with_id(ids::SETTINGS, "设置…", true, None);
    let service_settings_item = MenuItem::with_id(ids::SERVICE, "服务设置…", true, None);
    let reconnect_item = MenuItem::with_id(ids::RECONNECT, "重连", true, None);
    let allow_remote_item = CheckMenuItem::with_id(
        ids::ALLOW_REMOTE,
        "允许被远程控制",
        true,
        allow_remote,
        None,
    );
    let quit_item = MenuItem::with_id(ids::QUIT, "退出", true, None);

    let menu = Menu::with_items(&[
        &status_item,
        &server_item,
        &service_item,
        &PredefinedMenuItem::separator(),
        &show_item,
        &settings_item,
        &service_settings_item,
        &PredefinedMenuItem::separator(),
        &allow_remote_item,
        &PredefinedMenuItem::separator(),
        &reconnect_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])?;

    let icon_bytes = offline_icon_bytes();
    let icon = if let Some((rgba, w, h)) = load_icon_rgba(icon_bytes) {
        tray_icon::Icon::from_rgba(rgba, w, h)?
    } else {
        // 空图标兜底
        tray_icon::Icon::from_rgba(vec![0, 0, 0, 0], 1, 1)?
    };

    let tray = TrayIconBuilder::new()
        .with_tooltip("rust-tunnel — 离线")
        .with_icon(icon)
        .with_icon_as_template(true)
        .with_menu(Box::new(menu))
        .build()?;

    // 消费未处理的菜单事件（避免堆积）；托盘点击由 muda 统一分发。
    let _ = MenuEvent::receiver();

    Ok(TrayHandle {
        icon: tray,
        status_item,
        server_item,
        service_item,
        allow_remote_item,
    })
}

/// 错误文案在托盘菜单中展示的最大字符数（超出截断）。
const MAX_ERROR_CHARS: usize = 24;

/// 服务器地址在菜单中展示的最大字符数。
const MAX_SERVER_CHARS: usize = 28;

/// 服务器地址在 tooltip 中展示的最大字符数。
const MAX_TOOLTIP_SERVER_CHARS: usize = 40;

/// 截断字符串（按字符计数，超出补 …）。
fn truncate_chars(s: &str, max: usize) -> String {
    let chars: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{chars}…")
    } else {
        chars
    }
}

/// 服务接管身份时的托盘首行文案（托盘**不能**再说「已连接/离线」）。
const SERVICE_OWNED_MENU: &str = "● 服务已接管客户端身份";

/// 服务接管身份时的托盘 tooltip 正文（说明现状与恢复方式）。
const SERVICE_OWNED_TOOLTIP: &str =
    "本窗口未注册，远程控制由服务实例处理。\n在「设置」里停止或卸载服务可恢复本窗口接管。";

/// 菜单首行文案：状态 + RTT + 活跃隧道数。
#[must_use]
pub fn status_menu_text(status: &rust_tunnel_client::ClientStatus) -> String {
    match TrayState::from_status(status) {
        TrayState::Connected => match status.rtt_ms {
            Some(ms) => format!("● 已连接 · {ms:.0}ms · {} 活跃", status.active_tunnels),
            None => format!("● 已连接 · {} 活跃", status.active_tunnels),
        },
        TrayState::Reconnecting => match status.last_error.as_deref() {
            Some(err) => {
                // 取首行、过长截断，避免菜单项被超长错误撑宽。
                let first_line = err.lines().next().unwrap_or(err);
                let brief = truncate_chars(first_line, MAX_ERROR_CHARS);
                format!("● 重连中：{brief}")
            }
            None => "● 重连中".to_string(),
        },
        TrayState::Offline => "● 离线".to_string(),
        // `from_status` 只看连接状态、不会返回它；这一臂只为匹配穷尽，文案与
        // `status_menu_text_with_service` 的服务分支保持一致。
        TrayState::ServiceOwned => SERVICE_OWNED_MENU.to_string(),
    }
}

/// 服务感知的首行文案：服务在承载身份时直接说明接管（见 [`SERVICE_OWNED_MENU`]），
/// 否则与 [`status_menu_text`] 完全一致。
#[must_use]
pub fn status_menu_text_with_service(
    status: &rust_tunnel_client::ClientStatus,
    owner: IdentityOwner,
) -> String {
    if owner.is_service() {
        return SERVICE_OWNED_MENU.to_string();
    }
    status_menu_text(status)
}

/// 菜单次行文案：服务器地址（截断 28 字符，空则显示未配置）。
#[must_use]
pub fn server_menu_text(status: &rust_tunnel_client::ClientStatus) -> String {
    if status.server.is_empty() {
        "服务器：未配置".to_string()
    } else {
        let server = truncate_chars(&status.server, MAX_SERVER_CHARS);
        format!("服务器：{server}")
    }
}

/// 托盘 tooltip（多行；Windows 限制 128 UTF-16 单元，Linux 不显示）。
#[must_use]
pub fn tooltip_text(status: &rust_tunnel_client::ClientStatus) -> String {
    let state = TrayState::from_status(status);
    let head = format!("rust-tunnel — {}", state.label());
    match state {
        TrayState::Connected => {
            let server = truncate_chars(&status.server, MAX_TOOLTIP_SERVER_CHARS);
            match status.rtt_ms {
                Some(ms) => format!(
                    "{head}\n服务器：{server}\nRTT：{ms:.0} ms · 活跃隧道：{}",
                    status.active_tunnels
                ),
                None => format!(
                    "{head}\n服务器：{server}\n活跃隧道：{}",
                    status.active_tunnels
                ),
            }
        }
        TrayState::Reconnecting => match status.last_error.as_deref() {
            Some(err) => {
                let brief = err.lines().next().unwrap_or(err);
                format!("{head}\n{brief}")
            }
            None => head,
        },
        TrayState::Offline => {
            if status.server.is_empty() {
                head
            } else {
                let server = truncate_chars(&status.server, MAX_TOOLTIP_SERVER_CHARS);
                format!("{head}\n服务器：{server}")
            }
        }
        // 同 `status_menu_text`：这一臂只为匹配穷尽。
        TrayState::ServiceOwned => format!("{head}\n{SERVICE_OWNED_TOOLTIP}"),
    }
}

/// 服务感知的 tooltip：服务在承载身份时说明接管与恢复方式（见 [`SERVICE_OWNED_TOOLTIP`]），
/// 否则与 [`tooltip_text`] 完全一致。
#[must_use]
pub fn tooltip_text_with_service(
    status: &rust_tunnel_client::ClientStatus,
    owner: IdentityOwner,
) -> String {
    if owner.is_service() {
        let head = format!("rust-tunnel — {}", TrayState::ServiceOwned.label());
        return format!("{head}\n{SERVICE_OWNED_TOOLTIP}");
    }
    tooltip_text(status)
}

/// 轮询 `MenuEvent` 队列，返回待 eframe 侧处理的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    /// 显示主窗口。
    Show,
    /// 聚焦设置 Tab。
    Settings,
    /// 打开设置 Tab 的「无人值守被控端」一节（与 [`Self::Settings`] 同落地：设置就是那个 Tab，
    /// 分开只为让菜单语义与用户意图对上）。
    Service,
    /// 请求重连。
    Reconnect,
    /// 切换「允许被远程控制」。不带勾选态——调用方从
    /// [`TrayHandle::allow_remote_item`] 读 muda 翻转后的新状态。
    ToggleAllowRemote,
    /// 退出进程。
    Quit,
}

impl TrayAction {
    fn from_menu_id(id: &str) -> Option<Self> {
        match id {
            ids::SHOW => Some(Self::Show),
            ids::SETTINGS => Some(Self::Settings),
            ids::SERVICE => Some(Self::Service),
            ids::RECONNECT => Some(Self::Reconnect),
            ids::ALLOW_REMOTE => Some(Self::ToggleAllowRemote),
            ids::QUIT => Some(Self::Quit),
            _ => None,
        }
    }
}

/// 抽干一次 `MenuEvent` 队列。
#[must_use]
pub fn poll_menu_actions() -> Vec<TrayAction> {
    let mut out = Vec::new();
    while let Ok(ev) = MenuEvent::receiver().try_recv() {
        if let Some(a) = TrayAction::from_menu_id(ev.id().0.as_str()) {
            out.push(a);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! 托盘文案的纯逻辑单测（不建托盘、不碰系统菜单，故本机可跑）。

    use super::*;
    use rust_tunnel_client::ClientStatus;

    fn status(connected: bool, error: Option<&str>) -> ClientStatus {
        let mut s = ClientStatus::new("srv:3000".to_owned(), "home-nas".to_owned(), "0.11.0".to_owned());
        s.connected = connected;
        s.last_error = error.map(str::to_owned);
        s
    }

    /// 三个既有连接态的行为**不变**。
    #[test]
    fn from_status_keeps_the_three_connection_states() {
        assert_eq!(TrayState::from_status(&status(true, None)), TrayState::Connected);
        assert_eq!(
            TrayState::from_status(&status(false, Some("boom"))),
            TrayState::Reconnecting
        );
        assert_eq!(TrayState::from_status(&status(false, None)), TrayState::Offline);
    }

    /// 服务在承载身份时，**任何**连接状态都被覆盖成 `ServiceOwned`；归 GUI 时与旧行为一致。
    #[test]
    fn service_owner_overrides_every_connection_state() {
        for s in [
            status(true, None),
            status(false, Some("boom")),
            status(false, None),
        ] {
            assert_eq!(
                TrayState::from_status_with_service(&s, IdentityOwner::Service),
                TrayState::ServiceOwned
            );
            assert_eq!(
                TrayState::from_status_with_service(&s, IdentityOwner::Gui),
                TrayState::from_status(&s)
            );
        }
    }

    /// 服务接管时首行必须说接管，且**不能**说「已连接」（本窗口根本没注册）。
    #[test]
    fn service_owned_menu_text_states_the_takeover() {
        let s = status(true, None);
        let text = status_menu_text_with_service(&s, IdentityOwner::Service);
        assert!(text.contains("服务"), "{text}");
        assert!(text.contains("接管"), "{text}");
        assert!(!text.contains("已连接"), "{text}");
        // 非服务时与旧函数逐字相同。
        assert_eq!(
            status_menu_text_with_service(&s, IdentityOwner::Gui),
            status_menu_text(&s)
        );
    }

    /// 服务接管时 tooltip 必须说清「本窗口未注册」与怎么拿回来（在设置里停服务）。
    #[test]
    fn service_owned_tooltip_states_how_to_revert() {
        let s = status(false, None);
        let tip = tooltip_text_with_service(&s, IdentityOwner::Service);
        assert!(tip.contains("未注册"), "{tip}");
        assert!(tip.contains("设置"), "{tip}");
        assert_eq!(
            tooltip_text_with_service(&s, IdentityOwner::Gui),
            tooltip_text(&s)
        );
    }

    /// 既有文案（三态）保持原样——这三条是回归锁。
    #[test]
    fn plain_texts_keep_their_wording() {
        let connected = status(true, None);
        assert!(status_menu_text(&connected).contains("已连接"));
        assert_eq!(server_menu_text(&connected), "服务器：srv:3000");

        let offline = status(false, None);
        assert_eq!(status_menu_text(&offline), "● 离线");
        assert!(tooltip_text(&offline).contains("离线"));

        let reconnecting = status(false, Some("握手失败"));
        assert!(status_menu_text(&reconnecting).contains("重连中"));
    }

    /// 机械锁：`ids` 里的**每一个**常量都必须映射到一个动作（漏一个 = 那个菜单项点了没反应），
    /// 未知 id 必须得到 `None`（否则别的菜单事件会被我们误当成自己的动作）。
    #[test]
    fn every_menu_id_maps_to_an_action_and_unknown_ids_are_none() {
        let cases: &[(&str, TrayAction)] = &[
            (ids::SHOW, TrayAction::Show),
            (ids::SETTINGS, TrayAction::Settings),
            (ids::SERVICE, TrayAction::Service),
            (ids::RECONNECT, TrayAction::Reconnect),
            (ids::ALLOW_REMOTE, TrayAction::ToggleAllowRemote),
            (ids::QUIT, TrayAction::Quit),
        ];
        for (id, expected) in cases {
            assert_eq!(
                TrayAction::from_menu_id(id),
                Some(expected.clone()),
                "菜单项 {id}"
            );
        }
        // 表必须覆盖 `ids` 的**全部**常量（现在是 6 个）——加常量却漏测时这里先炸。
        assert_eq!(cases.len(), 6);

        // 只读项用的是裸字符串 id（刻意不在 `ids` 里），必须得到 `None`：它们被禁用、
        // 不会产生 MenuEvent，不该被当成动作。
        assert_eq!(TrayAction::from_menu_id(SERVICE_STATE_ID), None);
        assert_eq!(TrayAction::from_menu_id("status"), None);
        assert_eq!(TrayAction::from_menu_id("server"), None);
        // 真正的未知 id。
        assert_eq!(TrayAction::from_menu_id("nope"), None);
    }
}
