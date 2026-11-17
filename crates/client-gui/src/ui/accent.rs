//! 系统强调色：Windows 读注册表，其他平台用默认 Fluent 蓝。
//!
//! Win11 的强调色存放在 `HKCU\Software\Microsoft\Windows\DWM\AccentColor`，格式是
//! **ABGR** 的 DWORD（低字节是 R）。这里只取 RGB，alpha 统一忽略——窗口材质是实色，
//! 强调色不能带透明度。
//!
//! 另外提供 `RUST_TUNNEL_ACCENT=RRGGBB` 覆盖：截图评审时要能稳定复现某一种强调色，
//! 而改注册表既打扰用户、也不该由应用去做。

use std::sync::OnceLock;

/// 系统强调色（RGB）。整个进程只探一次：注册表值在运行期变化极少，
/// 而 `palette()` 每帧都会被调用。
pub fn system_accent_rgb() -> Option<[u8; 3]> {
    static CACHE: OnceLock<Option<[u8; 3]>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("RUST_TUNNEL_ACCENT")
            .ok()
            .and_then(|v| parse_hex(&v))
            .or_else(read_os_accent)
    })
}

/// 解析 `RRGGBB`（允许带 `#`）。
fn parse_hex(text: &str) -> Option<[u8; 3]> {
    let text = text.trim().trim_start_matches('#');
    if text.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&text[0..2], 16).ok()?;
    let g = u8::from_str_radix(&text[2..4], 16).ok()?;
    let b = u8::from_str_radix(&text[4..6], 16).ok()?;
    Some([r, g, b])
}

#[cfg(target_os = "windows")]
fn read_os_accent() -> Option<[u8; 3]> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let subkey = wide("Software\\Microsoft\\Windows\\DWM");
    let value = wide("AccentColor");
    let mut data: u32 = 0;
    let mut size = u32::try_from(std::mem::size_of::<u32>()).ok()?;
    let mut kind: u32 = 0;
    // SAFETY: 所有指针都指向本函数栈上的有效存储；`size` 以字节为单位且与 `data` 匹配。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::addr_of_mut!(kind),
            std::ptr::addr_of_mut!(data).cast(),
            std::ptr::addr_of_mut!(size),
        )
    };
    if status != 0 {
        return None;
    }
    // ABGR：低字节 R、其次 G、其次 B、高字节 A。
    Some([
        (data & 0xFF) as u8,
        ((data >> 8) & 0xFF) as u8,
        ((data >> 16) & 0xFF) as u8,
    ])
}

#[cfg(not(target_os = "windows"))]
fn read_os_accent() -> Option<[u8; 3]> {
    // macOS 的 controlAccentColor 需要 Objective-C 运行时；Linux 各桌面环境协议不一。
    // 两处都回退到 Fluent 蓝（见 `theme::accent_for`）。
    None
}

#[cfg(test)]
mod tests {
    use super::parse_hex;

    #[test]
    fn parses_rrggbb_with_or_without_hash() {
        assert_eq!(parse_hex("0067C0"), Some([0x00, 0x67, 0xC0]));
        assert_eq!(parse_hex("#60cdff"), Some([0x60, 0xCD, 0xFF]));
    }

    #[test]
    fn rejects_wrong_length_or_non_hex() {
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("12345"), None);
        assert_eq!(parse_hex("GGGGGG"), None);
    }
}
