//! macOS 平台层：权限探测与系统设置跳转。
//!
//! 截屏（`xcap`）与输入注入（`enigo`）的实体是三平台通用的（见 [`super::XcapScreen`] /
//! [`super::EnigoInput`]），本模块只补 macOS 特有的两件事：
//!
//! - **权限探测**：屏幕录制（`CGPreflightScreenCaptureAccess`）与辅助功能
//!   （`AXIsProcessTrusted`）。缺失时不能静默黑屏，UI 必须提示并引导授权。
//! - **设置跳转**：`open x-apple.systempreferences:...` 直达对应隐私面板。

/// 平台标识。
pub const NAME: &str = "macos";

// CoreGraphics / ApplicationServices 的 C 接口（xcap/enigo 已链接这两个 framework，
// 此处仅为读取权限状态）。`Boolean` 在 C 里是 `unsigned char`，`CGPreflight...` 返回
// C `bool`（1 字节），两者都用 `u8` 承接再判非零，避免 Rust `bool` 的 ABI 假设。
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// 是否已获得屏幕录制权限（macOS 10.15+；不弹授权框）。
    fn CGPreflightScreenCaptureAccess() -> u8;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// 本进程是否受信任使用辅助功能（`Boolean`，非零为真）。
    fn AXIsProcessTrusted() -> u8;
}

/// 探测 macOS 权限。
#[must_use]
pub fn permissions() -> super::Permissions {
    // SAFETY: 两个函数都是无副作用的状态查询，不接收指针、不写内存。
    let screen_capture = unsafe { CGPreflightScreenCaptureAccess() } != 0;
    let accessibility = unsafe { AXIsProcessTrusted() } != 0;
    super::Permissions {
        supported: true,
        screen_capture,
        accessibility,
    }
}

/// 打开对应的「系统设置 → 隐私与安全性」面板。
pub fn open_settings(kind: super::PermissionKind) {
    let url = match kind {
        super::PermissionKind::ScreenCapture => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        super::PermissionKind::Accessibility => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
    };
    if let Err(err) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!("打开系统设置失败：{err}");
    }
}

/// 构造平台实现三件套。
///
/// macOS 上 `--instance` 是无头模式且不可用（Windows 专用），故 `mode` 一律走 GUI 那套
/// （`xcap` + `enigo` + `arboard`）。
#[must_use]
pub fn screen_and_input(_mode: super::PlatformMode) -> super::PlatformHandles {
    super::gui_platform_handles()
}
