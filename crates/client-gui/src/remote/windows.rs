//! Windows 平台层。
//!
//! Windows 没有「屏幕录制 / 辅助功能」这种运行时授权模型（截屏与 `SendInput` 注入均无需
//! 用户授权），故权限恒为已授予，设置跳转为空操作。
//!
//! **未在本平台编译验证**：开发机为 macOS，本文件只在 `target_os = "windows"` 下编译。

use std::sync::Arc;

use rust_tunnel_remote::{InputSink, ScreenSource};

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
