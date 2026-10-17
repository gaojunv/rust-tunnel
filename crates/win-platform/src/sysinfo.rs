//! 时间与系统版本信息（可移植的格式化 + Windows 取值）。
//!
//! # 为什么单独一层
//!
//! 探针报告的第一行是**时间戳**（「用户贴回来的日志是哪一次跑的」全靠它），
//! 文件名里也要带时间戳（同一台机器上三态各跑一遍，不能互相覆盖）。
//! 格式化是纯逻辑，故放在可移植层做单测：`{:02}` 少一个零，文件名就会变得
//! 难以排序，而那属于「真机上才发现」的那类低级问题。
//!
//! # 注册表读取（Windows）
//!
//! [`windows_version`] 读 `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`。
//! 这个键在**任何** Windows 上都存在且**所有用户可读**，故不涉及权限。
//! 注意 `ProductName` 在 Windows 11 上仍然是 `Windows 10 Pro`（微软没改），
//! 真正能区分 10/11 的是 `CurrentBuildNumber`（≥ 22000 即 11），
//! 所以两者都记，**报告里不要只信 `ProductName`**。

/// 本地时间（`GetLocalTime` 的字段化结果；不做时区换算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalTime {
    /// 年（如 2026）。
    pub year: u16,
    /// 月（1..=12）。
    pub month: u16,
    /// 日（1..=31）。
    pub day: u16,
    /// 时（0..=23）。
    pub hour: u16,
    /// 分（0..=59）。
    pub minute: u16,
    /// 秒（0..=59）。
    pub second: u16,
    /// 毫秒（0..=999）。
    pub millis: u16,
}

impl LocalTime {
    /// 人读的渲染：`2026-10-17 14:03:22.123`。
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.millis
        )
    }

    /// 文件名用的紧凑渲染：`20261017-140322`（不含毫秒；同秒内不会跑两次探针）。
    #[must_use]
    pub fn file_stamp(&self) -> String {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// 校验一个时间戳是不是 [`LocalTime::file_stamp`] 的产物（`YYYYMMDD-HHMMSS`）。
///
/// # 为什么值得单独一个函数
///
/// 服务侧要把**自己算好的**时间戳通过命令行交给子实例（子实例各算各的「当前时刻」
/// 会跨秒边界对不上，父进程便去找一个不存在的文件名）。于是这个字符串成了
/// **命令行输入**，而它又会直接拼进**文件名**——不校验的话，一个 `..\..\Windows\...`
/// 就能让 SYSTEM 进程把报告写到任意路径去。这里只认「8 位数字 + `-` + 6 位数字」，
/// 其余一律拒（`\`、`/`、`:`、`..` 全部天然被挡在外面）。
#[must_use]
pub fn is_valid_stamp(stamp: &str) -> bool {
    let bytes = stamp.as_bytes();
    bytes.len() == 15
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..].iter().all(u8::is_ascii_digit)
}

/// Windows 版本信息（全部是原始字符串，不做解析）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowsVersion {
    /// `ProductName`，如 `Windows 10 Pro`（**Windows 11 上也是这个名字**）。
    pub product_name: String,
    /// `DisplayVersion`，如 `22H2`（旧系统上叫 `ReleaseId`，读不到就是空）。
    pub display_version: String,
    /// `CurrentBuildNumber`，如 `22631`。
    pub build: String,
    /// `UBR`（修订号），如 `4317`；读不到是 0。
    pub ubr: u32,
}

impl WindowsVersion {
    /// 一行渲染：`Windows 10 Pro 22H2 (22631.4317)`。
    ///
    /// 缺字段就少一段，**不留占位符**——探针报告里 `(22631.0)` 比
    /// `(22631.)` 更容易看出「UBR 没读到」。
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;

        let mut out = if self.product_name.is_empty() {
            String::from("(未知产品名)")
        } else {
            self.product_name.clone()
        };
        if !self.display_version.is_empty() {
            out.push(' ');
            out.push_str(&self.display_version);
        }
        if !self.build.is_empty() {
            // 用 write! 而不是 push_str(&format!(..))：后者多分配一次。
            let _ = write!(out, " ({}.{})", self.build, self.ubr);
        }
        out
    }
}

/// 读本地时间（`GetLocalTime`）。
///
/// 该 API **不会失败**（无返回值），故这里也不返回 `Result`。
#[cfg(windows)]
#[must_use]
pub fn local_now() -> LocalTime {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let mut st = SYSTEMTIME::default();
    // SAFETY: st 是本栈帧上按 Win32 布局定义的结构，API 只写入它、不持有。
    unsafe { GetLocalTime(&raw mut st) };
    LocalTime {
        year: st.wYear,
        month: st.wMonth,
        day: st.wDay,
        hour: st.wHour,
        minute: st.wMinute,
        second: st.wSecond,
        millis: st.wMilliseconds,
    }
}

/// 读 `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` 下的版本信息。
///
/// 任何一步失败都只让对应字段为空/0：探针报告不能因为读不到版本号就整体失败。
#[cfg(windows)]
#[must_use]
pub fn windows_version() -> WindowsVersion {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    const SUBKEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

    let subkey = crate::wide::to_wide(SUBKEY);
    let mut hkey = std::ptr::null_mut();
    // SAFETY: subkey 是 NUL 结尾的 UTF-16（存活到本语句结束之后仍在作用域内），
    // ulOptions 必须为 0，phkresult 是本栈帧上的 HKEY；成功时由本函数负责关闭。
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            0,
            KEY_READ,
            &raw mut hkey,
        )
    };
    if status != 0 || hkey.is_null() {
        return WindowsVersion::default();
    }

    let read = |name: &str| reg_string(hkey, name);
    let version = WindowsVersion {
        product_name: read("ProductName").unwrap_or_default(),
        display_version: read("DisplayVersion")
            .or_else(|| read("ReleaseId"))
            .unwrap_or_default(),
        build: read("CurrentBuildNumber").unwrap_or_default(),
        ubr: reg_dword(hkey, "UBR").unwrap_or(0),
    };
    // SAFETY: hkey 由上面成功的 RegOpenKeyExW 返回，且未被传递给别处。
    unsafe { RegCloseKey(hkey) };
    version
}

/// 读一个 `REG_SZ` 值（失败返回 `None`）。
#[cfg(windows)]
fn reg_string(hkey: windows_sys::Win32::System::Registry::HKEY, name: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegQueryValueExW, REG_SZ};

    let name = crate::wide::to_wide(name);
    let mut ty: u32 = 0;
    let mut bytes: u32 = 0;
    // 第一趟：问需要多少字节。缓冲区为 null 时按文档会返回 ERROR_SUCCESS 并给出长度。
    // SAFETY: lpreserved 必须为 null；lpdata 为 null 表示「只要长度」；
    // lptype/lpcbdata 都是本栈帧上的可写局部变量。
    let status = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null(),
            &raw mut ty,
            std::ptr::null_mut(),
            &raw mut bytes,
        )
    };
    if status != 0 || bytes == 0 {
        return None;
    }
    let units = usize::try_from(bytes.div_ceil(2)).unwrap_or(0);
    let mut buf: Vec<u16> = vec![0; units];
    let mut got = bytes;
    // SAFETY: buf 是本栈帧上的可写缓冲，长度按字节数如实告知（got 传入初始容量）。
    let status = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null(),
            &raw mut ty,
            buf.as_mut_ptr().cast(),
            &raw mut got,
        )
    };
    if status != 0 {
        return None;
    }
    if ty != REG_SZ {
        return None;
    }
    Some(crate::wide::from_wide(&buf))
}

/// 读一个 `REG_DWORD` 值（失败返回 `None`）。
#[cfg(windows)]
fn reg_dword(hkey: windows_sys::Win32::System::Registry::HKEY, name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RegQueryValueExW, REG_DWORD};

    let name = crate::wide::to_wide(name);
    let mut ty: u32 = 0;
    let mut data: u32 = 0;
    let mut bytes = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4);
    // SAFETY: data 是本栈帧上的 u32，bytes 如实告知其大小；lpreserved 必须为 null。
    let status = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null(),
            &raw mut ty,
            std::ptr::addr_of_mut!(data).cast(),
            &raw mut bytes,
        )
    };
    if status != 0 || ty != REG_DWORD {
        return None;
    }
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_padded_timestamp() {
        let t = LocalTime {
            year: 2026,
            month: 10,
            day: 17,
            hour: 14,
            minute: 3,
            second: 2,
            millis: 7,
        };
        assert_eq!(t.render(), "2026-10-17 14:03:02.007");
    }

    #[test]
    fn file_stamp_is_compact_and_sortable() {
        let t = LocalTime {
            year: 2026,
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
            millis: 999,
        };
        assert_eq!(t.file_stamp(), "20260102-030405");
        assert!(!t.file_stamp().contains(':'), "文件名里不能有冒号");
    }

    #[test]
    fn a_generated_stamp_always_passes_validation() {
        // 这两件事必须一致：生成的那个格式就是校验接受的那个格式。
        let t = LocalTime {
            year: 2026,
            month: 10,
            day: 17,
            hour: 9,
            minute: 5,
            second: 3,
            millis: 0,
        };
        assert!(is_valid_stamp(&t.file_stamp()), "{}", t.file_stamp());
        assert!(is_valid_stamp("20261017-090503"));
    }

    #[test]
    fn validation_rejects_anything_that_could_escape_the_output_directory() {
        // 这个字符串会从命令行进来、又直接拼进文件名，所以 `..`/分隔符必须一律拒。
        for bad in [
            r"..\..\Windows\System32\x",
            "20261017/090503",
            "20261017-09050",
            "20261017-0905033",
            "2026a017-090503",
            "20261017_090503",
            "",
            "..",
            r"C:\Windows\Temp\pwn",
        ] {
            assert!(!is_valid_stamp(bad), "不该放行：{bad}");
        }
    }

    #[test]
    fn version_render_includes_every_present_field() {
        let v = WindowsVersion {
            product_name: String::from("Windows 10 Pro"),
            display_version: String::from("22H2"),
            build: String::from("22631"),
            ubr: 4317,
        };
        assert_eq!(v.render(), "Windows 10 Pro 22H2 (22631.4317)");
    }

    #[test]
    fn version_render_omits_missing_fields_without_stray_punctuation() {
        let v = WindowsVersion {
            build: String::from("22631"),
            ..WindowsVersion::default()
        };
        assert_eq!(v.render(), "(未知产品名) (22631.0)");
        assert_eq!(WindowsVersion::default().render(), "(未知产品名)");
    }

    /// 真机上跑：时间必须落在合理范围内——`SYSTEMTIME` 字段顺序抄错的话
    /// 这里立刻就能看出来（比如月日互换在 10 月 17 日会变成 17 月）。
    #[cfg(all(test, windows))]
    #[test]
    fn local_now_is_plausible_on_a_real_machine() {
        let t = local_now();
        assert!(t.year >= 2020, "年份不合理：{t:?}");
        assert!((1..=12).contains(&t.month), "月份越界：{t:?}");
        assert!((1..=31).contains(&t.day), "日期越界：{t:?}");
        assert!(t.hour < 24 && t.minute < 60 && t.second < 60, "{t:?}");
        assert!(t.millis < 1000, "{t:?}");
    }

    #[cfg(all(test, windows))]
    #[test]
    fn windows_version_is_readable_from_a_real_registry() {
        let v = windows_version();
        assert!(
            !v.build.is_empty(),
            "CurrentBuildNumber 应当总能读到：{v:?}"
        );
    }
}
