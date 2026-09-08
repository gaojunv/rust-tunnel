//! 托盘图标与菜单（tray-icon + muda）。

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// 托盘连接态（决定图标与菜单文案）。
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
    /// 从客户端状态推导托盘态。
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

    /// 状态短标签（用于 tooltip 后缀）。
    fn label(self) -> &'static str {
        match self {
            Self::Connected => "已连接",
            Self::Reconnecting => "重连中",
            Self::Offline => "离线",
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
}

/// 构造托盘图标与菜单，返回可持久持有的句柄（调用方需持有其生命周期）。
///
/// 图标三态可通过 `handle.icon.set_icon(...)` 切换；此处先以离线态创建。
pub fn build_tray() -> anyhow::Result<TrayHandle> {
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
    })
}

/// 错误文案在托盘菜单中展示的最大字符数（超出截断）。
const MAX_ERROR_CHARS: usize = 24;

/// 根据最新状态刷新托盘 tooltip 与状态菜单文案。
pub fn update_tray_for_status(handle: &TrayHandle, status: &rust_tunnel_client::ClientStatus) {
    let state = TrayState::from_status(status);
    let text = match state {
        TrayState::Connected => "● 已连接".to_string(),
        TrayState::Reconnecting => match status.last_error.as_deref() {
            Some(err) => {
                // 取首行、过长截断，避免菜单项被超长错误撑宽。
                let first_line = err.lines().next().unwrap_or(err);
                let brief: String = first_line.chars().take(MAX_ERROR_CHARS).collect();
                if first_line.chars().count() > MAX_ERROR_CHARS {
                    format!("● 重连中：{brief}…")
                } else {
                    format!("● 重连中：{brief}")
                }
            }
            None => "● 重连中".to_string(),
        },
        TrayState::Offline => "● 离线".to_string(),
    };
    handle.status_item.set_text(&text);
    let tooltip = format!("rust-tunnel — {}", state.label());
    if let Err(e) = handle.icon.set_tooltip(Some(tooltip)) {
        tracing::warn!("托盘 tooltip 更新失败：{e}");
    }
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
