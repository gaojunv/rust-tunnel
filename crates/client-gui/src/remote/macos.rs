//! macOS 平台层：权限探测与系统设置跳转。
//!
//! 屏幕采集走 ScreenCaptureKit（`mac_capture`），输入注入走 `enigo`
//! （[`super::EnigoInput`]）；本模块只补 macOS 特有的两件事：
//!
//! - **权限探测**：屏幕录制（`CGPreflightScreenCaptureAccess`）与辅助功能
//!   （`AXIsProcessTrusted`）。缺失时不能静默黑屏，UI 必须提示并引导授权。
//! - **设置跳转**：`open x-apple.systempreferences:...` 直达对应隐私面板。

/// 平台标识。
pub const NAME: &str = "macos";

use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};

// CoreGraphics / ApplicationServices 的 C 接口（xcap/enigo 已链接这两个 framework，
// 此处仅为读取权限状态）。`Boolean` 在 C 里是 `unsigned char`，`CGPreflight...` 返回
// C `bool`（1 字节），两者都用 `u8` 承接再判非零，避免 Rust `bool` 的 ABI 假设。
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// 是否已获得屏幕录制权限（macOS 10.15+；不弹授权框）。
    fn CGPreflightScreenCaptureAccess() -> u8;
    /// 请求屏幕录制权限（可能弹一次系统授权框）。
    fn CGRequestScreenCaptureAccess() -> u8;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// 本进程是否受信任使用辅助功能（`Boolean`，非零为真）。
    fn AXIsProcessTrusted() -> u8;
    /// 询问辅助功能授权；`options` 里带 `kAXTrustedCheckOptionPrompt` 时会弹系统框。
    fn AXIsProcessTrustedWithOptions(options: &CFDictionary) -> u8;
    /// 选项字典的键：值为 `true` 时请求系统弹「辅助功能」授权框。
    static kAXTrustedCheckOptionPrompt: Option<&'static CFString>;
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

/// 请求屏幕录制权限；返回调用后是否已授予。
///
/// 只在登录窗口 agent 缺权限时调用：已授权时直接返回 `true`，未授权时可能弹一次框
/// （登录窗口下通常无人能点，长期无人值守依赖 `persistent-content-capture`）。
#[must_use]
pub fn request_screen_capture() -> bool {
    // SAFETY: 无参数、无副作用（只触发系统授权流程并返回当前状态）。
    unsafe { CGRequestScreenCaptureAccess() != 0 }
}

/// 请求辅助功能权限；返回调用后是否已授权（弹窗是异步的）。
#[must_use]
pub fn request_accessibility() -> bool {
    // SAFETY: 常量由 ApplicationServices 导出；字典只在本调用期间存活，调用只读。
    let Some(key) = (unsafe { kAXTrustedCheckOptionPrompt }) else {
        // 框架没有导出常量时不能 panic：退回只查询当前信任状态（不弹窗）。
        tracing::warn!("ApplicationServices 未导出 kAXTrustedCheckOptionPrompt，无法弹出授权框");
        // SAFETY: 无参数、只读的状态查询。
        return unsafe { AXIsProcessTrusted() != 0 };
    };
    let value = CFBoolean::new(true);
    let options = CFDictionary::from_slices(&[key], &[value]);
    let dict: &CFDictionary<CFString, CFBoolean> = &options;
    let base: &CFDictionary = dict.as_ref();
    // SAFETY: `base` 是合法 CFDictionary，调用只读取它。
    unsafe { AXIsProcessTrustedWithOptions(base) != 0 }
}

/// 请求某一类权限（macOS 会弹系统授权框）。
pub fn request_permission(kind: super::PermissionKind) -> bool {
    match kind {
        super::PermissionKind::ScreenCapture => request_screen_capture(),
        super::PermissionKind::Accessibility => request_accessibility(),
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
/// - [`super::PlatformMode::Gui`]：ScreenCaptureKit + `enigo` + `arboard`。
/// - [`super::PlatformMode::LoginWindow`]：ScreenCaptureKit + **空注入器** + 无剪贴板
///   （登录窗口只读 agent）。
/// - [`super::PlatformMode::Instance`] 是 Windows 专用；macOS 上按 GUI 处理（不会走到）。
#[must_use]
pub fn screen_and_input(mode: super::PlatformMode) -> super::PlatformHandles {
    match mode {
        super::PlatformMode::LoginWindow => super::login_window_handles(),
        super::PlatformMode::Gui | super::PlatformMode::Instance => super::gui_platform_handles(),
    }
}
