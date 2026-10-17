//! UTF-16 与 Rust 字符串的互转（**可移植**：纯 std，能在 macOS 上单测）。
//!
//! Win32 的 `W` 系列 API 吃 UTF-16、且**要求 NUL 结尾**；`PWSTR` 参数在 Rust 里是
//! `*mut u16`，因此切片必须由调用方持有并在调用期间保持存活。把这件事收在这里，
//! 避免每个调用点各写一份「push 0」。
//!
//! 注意 Windows 上 `OsStr` 的真实编码就是 WTF-16，而 `str` 是合法 UTF-8，
//! 故不存在「非法的 UTF-16」——`from_wide` 用 `from_utf16_lossy` 是安全的兜底
//! （只会发生在对方给的缓冲区没有 NUL 结尾这类畸形输入上）。

/// 把 `&str` 转成 NUL 结尾的 UTF-16 缓冲（可直接当 `PCWSTR` 用）。
#[must_use]
pub fn to_wide(s: &str) -> Vec<u16> {
    let mut buf: Vec<u16> = s.encode_utf16().collect();
    buf.push(0);
    buf
}

/// 把 NUL 结尾的 UTF-16 缓冲转回 `String`（到第一个 NUL 为止，含无 NUL 的兜底）。
#[must_use]
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_wide_is_nul_terminated() {
        assert_eq!(to_wide("abc"), vec![0x61, 0x62, 0x63, 0]);
        assert_eq!(to_wide(""), vec![0]);
    }

    #[test]
    fn round_trip_through_wide() {
        for s in [
            "",
            "rust-tunnel",
            r"C:\Program Files\rust-tunnel\x.exe",
            "中文与 emoji 🚀",
        ] {
            assert_eq!(from_wide(&to_wide(s)), s);
        }
    }

    #[test]
    fn from_wide_stops_at_the_first_nul() {
        let buf = [0x61, 0x00, 0x62, 0x00];
        assert_eq!(from_wide(&buf), "a");
    }

    #[test]
    fn from_wide_tolerates_a_missing_nul() {
        assert_eq!(from_wide(&[0x61, 0x62]), "ab");
    }

    #[test]
    fn from_wide_replaces_unpaired_surrogates_instead_of_panicking() {
        // 半截代理对（畸形输入）必须降级成替换字符，不能 panic。
        assert_eq!(from_wide(&[0xD800]), "\u{FFFD}");
    }
}
