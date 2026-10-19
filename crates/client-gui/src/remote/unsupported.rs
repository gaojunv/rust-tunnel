//! 未支持平台的兜底实现（非 macOS / Windows / Linux）。
//!
//! 仅为让工程在其他目标上仍能编译；远程控制在这些平台不可用。
//!
//! **未在任何平台编译验证**：本模块只在 `not(any(macos, windows, linux))` 下参与编译，
//! 开发机为 macOS，故从未被编译过。

/// 平台标识。
pub const NAME: &str = "unknown";

/// 该平台不支持远程控制。
#[must_use]
pub fn permissions() -> super::Permissions {
    super::Permissions {
        supported: false,
        screen_capture: false,
        accessibility: false,
    }
}

/// 无对应设置面板。
pub fn open_settings(_kind: super::PermissionKind) {}

/// 构造平台实现三件套（实际不可用）。
#[must_use]
pub fn screen_and_input(_mode: super::PlatformMode) -> super::PlatformHandles {
    super::gui_platform_handles()
}
