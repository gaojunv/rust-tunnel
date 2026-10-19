//! Windows 平台层。
//!
//! Windows 没有「屏幕录制 / 辅助功能」这种运行时授权模型（截屏与 `SendInput` 注入均无需
//! 用户授权），故权限恒为已授予，设置跳转为空操作。
//!
//! **未在本平台编译验证**：开发机为 macOS，本文件只在 `target_os = "windows"` 下编译。

/// 平台标识。
pub const NAME: &str = "windows";

/// Windows 无需运行时授权，恒返回已授予。
#[must_use]
pub fn permissions() -> super::Permissions {
    super::Permissions {
        supported: false,
        screen_capture: true,
        accessibility: true,
    }
}

/// Windows 无对应隐私面板，空操作。
pub fn open_settings(_kind: super::PermissionKind) {}

/// 构造平台实现三件套。
///
/// - [`super::PlatformMode::Gui`]：用户会话里的老路径（`xcap` + `enigo` + `arboard`），
///   **一行不改**（发布中的二进制，不承担回归风险）。
/// - [`super::PlatformMode::Instance`]：SYSTEM 服务拉进控制台会话的无头实例，采集/注入/
///   剪贴板都改用 `win-platform` 并跟随输入桌面（见 [`super::win_desktop`]）。
///   **不能**复用 `EnigoInput`：`enigo` 的 `move_mouse(Coordinate::Abs)` 用
///   `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`（仅主显示器）归一化、且没有
///   `MOUSEEVENTF_VIRTUALDESK`，多显示器坐标本来就是错的；`win-platform` 的
///   `VirtualScreen::normalize_absolute` + `MOUSEEVENTF_VIRTUALDESK` 才是对的。
#[must_use]
pub fn screen_and_input(mode: super::PlatformMode) -> super::PlatformHandles {
    match mode {
        super::PlatformMode::Gui => super::gui_platform_handles(),
        super::PlatformMode::Instance => super::win_desktop::screen_and_input(),
    }
}
