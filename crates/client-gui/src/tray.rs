//! 托盘图标与菜单（tray-icon + muda）。

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// 托盘连接态（决定图标与菜单文案）。
#[allow(dead_code, reason = "托盘三态图标将在后续批次切图标时启用")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// 已连接。
    Connected,
    /// 重连中。
    Reconnecting,
    /// 离线/未连接。
    Offline,
}

impl TrayState {
    fn from_status(status: &rust_tunnel_client::ClientStatus) -> Self {
        if status.connected {
            Self::Connected
        } else if status.last_error.is_some() {
            Self::Reconnecting
        } else {
            Self::Offline
        }
    }
}

/// 菜单项 ID（与 eframe 侧分发对齐）。
pub mod ids {
    /// 显示/聚焦主面板。
    pub const SHOW: &str = "show";
    /// 设置 Tab。
    pub const SETTINGS: &str = "settings";
    /// 立即重连。
    pub const RECONNECT: &str = "reconnect";
    /// 退出应用。
    pub const QUIT: &str = "quit";
}

fn load_icon_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some((rgba.into_raw(), w, h))
}

fn offline_icon_bytes() -> &'static [u8] {
    // 优先用导出的图标，回退用通用 icon
    include_bytes!("../icons/icon.png")
}

/// 构造托盘图标与菜单，返回可持久持有的 `TrayIcon`（调用方需持有其生命周期）。
///
/// 图标三态通过 `tray.set_icon(...)` 切换；此处先以离线态创建。
pub fn build_tray() -> anyhow::Result<TrayIcon> {
    let status_item = MenuItem::with_id("status", "● 离线", false, None);
    let show_item = MenuItem::with_id(ids::SHOW, "打开面板…", true, None);
    let settings_item = MenuItem::with_id(ids::SETTINGS, "设置…", true, None);
    let reconnect_item = MenuItem::with_id(ids::RECONNECT, "重连", true, None);
    let quit_item = MenuItem::with_id(ids::QUIT, "退出", true, None);

    let menu = Menu::with_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &show_item,
        &settings_item,
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
        .with_tooltip("rust-tunnel")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()?;

    // 消费未处理的菜单事件（避免堆积）；托盘点击由 muda 统一分发。
    let _ = MenuEvent::receiver();

    Ok(tray)
}

/// 根据最新状态刷新托盘 tooltip 与状态菜单文案。
/// 映射子菜单本期仅在小窗口内展示，托盘保持轻量。
#[allow(dead_code, reason = "托盘映射子菜单在小窗口稳定后接入")]
pub fn update_tray_for_status(_tray: &TrayIcon, status: &rust_tunnel_client::ClientStatus) {
    // TODO: picks different icon per TrayState via set_icon, and rebuilds
    // the mapping submenu from status.mapping_summary when needed.
    // Left minimal for now to keep the tray lightweight and avoid
    // reconstructing muda submenus on every status update.
    let _state = TrayState::from_status(status);
}

/// 轮询 `MenuEvent` 队列，返回待 eframe 侧处理的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    /// 显示主窗口。
    Show,
    /// 聚焦设置 Tab。
    Settings,
    /// 请求重连。
    Reconnect,
    /// 退出进程。
    Quit,
}

impl TrayAction {
    fn from_menu_id(id: &str) -> Option<Self> {
        match id {
            ids::SHOW => Some(Self::Show),
            ids::SETTINGS => Some(Self::Settings),
            ids::RECONNECT => Some(Self::Reconnect),
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
