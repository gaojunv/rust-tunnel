//! 未支持平台的兜底实现（非 macOS / Windows / Linux）。
//!
//! 仅为让工程在其他目标上仍能编译；远程控制在这些平台不可用。
//!
//! **未在任何平台编译验证**：本模块只在 `not(any(macos, windows, linux))` 下参与编译，
//! 开发机为 macOS，故从未被编译过。

use std::sync::Arc;

use rust_tunnel_remote::{InputSink, ScreenSource};

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

/// 构造屏幕采集源（实际不可用）。
#[must_use]
pub fn screen_source() -> Arc<dyn ScreenSource> {
    Arc::new(super::XcapScreen)
}

/// 构造输入注入目标（实际不可用）。
#[must_use]
pub fn input_sink() -> Arc<dyn InputSink> {
    Arc::new(super::EnigoInput::new())
}
