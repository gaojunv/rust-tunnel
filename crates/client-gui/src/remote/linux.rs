//! Linux 平台层。
//!
//! X11 下截屏与输入注入（libxdo）无需授权；Wayland 下 `xcap` 走 portal，可能弹系统授权框，
//! 无法在进程内统一探测，故权限恒返回已授予——真正的失败会由会话错误路径提示。
//!
//! **未在本平台编译验证**：开发机为 macOS，本文件只在 `target_os = "linux"` 下编译。

use std::sync::Arc;

use rust_tunnel_remote::{InputSink, ScreenSource};

/// 平台标识。
pub const NAME: &str = "linux";

/// Linux 无法统一探测授权，恒返回已授予（失败由会话错误提示）。
#[must_use]
pub fn permissions() -> super::Permissions {
    super::Permissions {
        supported: false,
        screen_capture: true,
        accessibility: true,
    }
}

/// Linux 无统一的隐私面板跳转，空操作。
pub fn open_settings(_kind: super::PermissionKind) {}

/// 构造屏幕采集源。
#[must_use]
pub fn screen_source() -> Arc<dyn ScreenSource> {
    Arc::new(super::XcapScreen)
}

/// 构造输入注入目标。
#[must_use]
pub fn input_sink() -> Arc<dyn InputSink> {
    Arc::new(super::EnigoInput::new())
}
