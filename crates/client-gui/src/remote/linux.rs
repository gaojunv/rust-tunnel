//! Linux 平台层。
//!
//! X11 下截屏与输入注入（libxdo）无需授权；Wayland 下 `xcap` 走 portal，可能弹系统授权框，
//! 无法在进程内统一探测，故权限恒返回已授予——真正的失败会由会话错误路径提示。
//!
//! **未在本平台编译验证**：开发机为 macOS，本文件只在 `target_os = "linux"` 下编译。

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

/// Linux 无法统一请求运行期授权，视为已授予（失败由会话错误提示）。
pub fn request_permission(_kind: super::PermissionKind) -> bool {
    true
}

/// 构造平台实现三件套（Linux 上 `--instance` 不可用，`mode` 一律走 GUI 那套）。
#[must_use]
pub fn screen_and_input(_mode: super::PlatformMode) -> super::PlatformHandles {
    super::gui_platform_handles()
}
