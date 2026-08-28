//! 开机自启封装（auto-launch，失败以 Result 透出供设置面板展示）。

use auto_launch::{AutoLaunch, AutoLaunchBuilder};

/// 是否已启用自启。
#[allow(dead_code, reason = "设置面板开关将接入")]
pub fn is_enabled() -> Result<bool, String> {
    let al = build()?;
    al.is_enabled().map_err(|e| e.to_string())
}

/// 启用自启。
///
/// # Errors
/// 当底层写入失败时返回 `Err`。
#[allow(dead_code, reason = "设置面板开关将接入")]
pub fn enable() -> Result<(), String> {
    let al = build()?;
    al.enable().map_err(|e| e.to_string())
}

/// 禁用自启。
///
/// # Errors
/// 当底层写入失败时返回 `Err`。
#[allow(dead_code, reason = "设置面板开关将接入")]
pub fn disable() -> Result<(), String> {
    let al = build()?;
    al.disable().map_err(|e| e.to_string())
}

fn build() -> Result<AutoLaunch, String> {
    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .to_string();
    // app_name 决定 LaunchAgent/plist/注册表键名，取固定值避免随版本抖动
    // 用 builder 而非 AutoLaunch::new：后者签名平台相关（macOS 多一个 use_launch_agent 参数）
    // macOS 走 LaunchAgent plist 而非 AppleScript 登录项——我们是裸二进制，不是 .app bundle
    AutoLaunchBuilder::new()
        .set_app_name("RustTunnel")
        .set_app_path(&exe)
        .set_use_launch_agent(true)
        .set_args(&[] as &[&str])
        .build()
        .map_err(|e| e.to_string())
}
