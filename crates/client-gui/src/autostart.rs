//! 开机自启封装（auto-launch，失败以 Result 透出供设置面板展示）。

use auto_launch::AutoLaunch;

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
    Ok(AutoLaunch::new("RustTunnel", &exe, &[] as &[&str]))
}
