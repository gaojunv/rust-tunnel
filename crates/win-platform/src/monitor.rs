//! 显示器枚举与虚拟桌面几何（可移植部分 + Win32 实现）。
//!
//! # 为什么显示器 id 由本 crate 定义
//!
//! 远程控制引擎的 `DisplayInfo` 字段是**冻结**的（它嵌在 `HelloAck` 的字节里，见
//! `crates/remote/src/screen.rs`），控制器只按 id 请求画面。用户态那条路径的 id 来自
//! `xcap` 的枚举顺序，而本路径（SYSTEM 实例）**完全自持**：id 就是
//! [`enumerate_monitors`] 的枚举下标。控制器从 `HelloAck` 学到的 id 就是我们给的 id，
//! 因此不存在「与 xcap 对齐」这件事。
//!
//! # 绝对坐标归一化
//!
//! `SendInput` 的 `MOUSEEVENTF_ABSOLUTE` 不吃像素坐标，而吃 **0..=65535 的归一化坐标**；
//! 加上 `MOUSEEVENTF_VIRTUALDESK` 后这个区间映射到**整个虚拟桌面**（多显示器包围盒，
//! 原点可能是负数）。换算公式与边界处理在 [`VirtualScreen::normalize_absolute`] 里，
//! 有单测。

// 注意：这里不 `use crate::WinResult`——`enumerate_monitors` 只在 Windows 上编译，
// 顶层导入会在 macOS 上变成未使用导入。签名里写全路径。

/// 一台显示器在虚拟桌面里的位置与尺寸。`id` 即枚举下标（见模块文档）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    /// 枚举下标，控制器就是拿这个值请求画面的。
    pub id: u32,
    /// 设备名（如 `\\.\DISPLAY1`），只在报告与日志里用。
    pub device: String,
    /// 左上角在虚拟桌面坐标系里的 x（副屏在主屏左边时是负数）。
    pub origin_x: i32,
    /// 左上角在虚拟桌面坐标系里的 y（副屏在主屏上方时是负数）。
    pub origin_y: i32,
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 是否为主显示器。
    pub primary: bool,
}

impl Monitor {
    /// 是否包含虚拟桌面坐标 `(x, y)`。右下边界是**开区间**（像素网格）。
    #[must_use]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        let right = i64::from(self.origin_x) + i64::from(self.width);
        let bottom = i64::from(self.origin_y) + i64::from(self.height);
        i64::from(x) >= i64::from(self.origin_x)
            && i64::from(x) < right
            && i64::from(y) >= i64::from(self.origin_y)
            && i64::from(y) < bottom
    }

    /// 显示器**局部**坐标（控制器看到的画面里的坐标）→ 虚拟桌面全局坐标。
    ///
    /// 局部坐标会先夹到 `[0, width-1] × [0, height-1]`：控制器手里的画面尺寸可能比
    /// 当前实际分辨率旧（改分辨率、画质降档重建编码器），越界的坐标**必须**被夹住，
    /// 否则鼠标会跑到别的显示器上去。
    #[must_use]
    pub fn to_global(&self, local_x: i32, local_y: i32) -> (i32, i32) {
        let max_x = i32::try_from(self.width.saturating_sub(1)).unwrap_or(i32::MAX);
        let max_y = i32::try_from(self.height.saturating_sub(1)).unwrap_or(i32::MAX);
        let lx = local_x.clamp(0, max_x.max(0));
        let ly = local_y.clamp(0, max_y.max(0));
        let gx = i64::from(self.origin_x) + i64::from(lx);
        let gy = i64::from(self.origin_y) + i64::from(ly);
        (
            i32::try_from(gx).unwrap_or(i32::MAX),
            i32::try_from(gy).unwrap_or(i32::MAX),
        )
    }

    /// 本显示器矩形，按 `BitBlt` 需要的顺序给出 `(x, y, cx, cy)`。
    #[must_use]
    pub fn bitblt_rect(&self) -> (i32, i32, i32, i32) {
        (
            self.origin_x,
            self.origin_y,
            i32::try_from(self.width).unwrap_or(0),
            i32::try_from(self.height).unwrap_or(0),
        )
    }
}

/// 虚拟桌面包围盒（所有显示器的并集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualScreen {
    /// 包围盒左上角 x（可能为负）。
    pub origin_x: i32,
    /// 包围盒左上角 y（可能为负）。
    pub origin_y: i32,
    /// 包围盒宽（像素）。
    pub width: u32,
    /// 包围盒高（像素）。
    pub height: u32,
}

impl VirtualScreen {
    /// 由显示器列表求并集。空列表返回 `None`（没有显示器就没有虚拟桌面）。
    #[must_use]
    pub fn from_monitors(monitors: &[Monitor]) -> Option<Self> {
        let first = monitors.first()?;
        let mut min_x = i64::from(first.origin_x);
        let mut min_y = i64::from(first.origin_y);
        let mut max_x = min_x + i64::from(first.width);
        let mut max_y = min_y + i64::from(first.height);
        for m in &monitors[1..] {
            min_x = min_x.min(i64::from(m.origin_x));
            min_y = min_y.min(i64::from(m.origin_y));
            max_x = max_x.max(i64::from(m.origin_x) + i64::from(m.width));
            max_y = max_y.max(i64::from(m.origin_y) + i64::from(m.height));
        }
        Some(Self {
            origin_x: i32::try_from(min_x).unwrap_or(0),
            origin_y: i32::try_from(min_y).unwrap_or(0),
            width: u32::try_from(max_x - min_x).unwrap_or(0),
            height: u32::try_from(max_y - min_y).unwrap_or(0),
        })
    }

    /// 是否包含虚拟桌面坐标 `(x, y)`。
    #[must_use]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        let right = i64::from(self.origin_x) + i64::from(self.width);
        let bottom = i64::from(self.origin_y) + i64::from(self.height);
        i64::from(x) >= i64::from(self.origin_x)
            && i64::from(x) < right
            && i64::from(y) >= i64::from(self.origin_y)
            && i64::from(y) < bottom
    }

    /// 虚拟桌面坐标 → `SendInput` 的 `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`
    /// 所需归一化坐标（`0..=65535`）。
    ///
    /// 用的是 MSDN 的 `(x - origin) * 65535 / (extent - 1)` 形式——注意 `-1`：
    /// 最后**一个像素**要落到 65535，否则右下角若干像素永远点不到。另一种常见写法
    /// `* 65536 / extent` 与之最多差 1 像素，哪一种在真机上更准只能实测，
    /// 故这里选了 MSDN 的版本并在阶段 0 的探针里如实记录结果。
    #[must_use]
    pub fn normalize_absolute(&self, x: i32, y: i32) -> (i32, i32) {
        fn norm(v: i32, origin: i32, extent: u32) -> i32 {
            let span = if extent > 1 { i64::from(extent) - 1 } else { 1 };
            let rel = (i64::from(v) - i64::from(origin)).clamp(0, span);
            i32::try_from(rel * 65535 / span).unwrap_or(0)
        }
        (
            norm(x, self.origin_x, self.width),
            norm(y, self.origin_y, self.height),
        )
    }
}

/// 枚举当前所有显示器（`EnumDisplayMonitors` + `GetMonitorInfoW`）。
///
/// # 实现契约
///
/// - `EnumDisplayMonitors(NULL, NULL, Some(cb), lparam)`，`lparam` 传一个
///   `*mut Vec<Monitor>` 的 provenance；回调里：
///   1. `MONITORINFOEXW` 零值，`monitorInfo.cbSize = size_of::<MONITORINFOEXW>()`；
///   2. **`GetMonitorInfoW` 的形参类型是 `*mut MONITORINFO`**，故必须把
///      `*mut MONITORINFOEXW` 转型传进去（`MONITORINFOEXW` 的首成员就是
///      `MONITORINFO`，这是 Win32 的既定用法）；
///   3. `rcMonitor` 给出全局矩形，`szDevice` 给出设备名（取到第一个 NUL 为止），
///      `dwFlags & MONITORINFOF_PRIMARY` 给出主屏标志；
///   4. `id` = 压栈顺序的下标；回调返回 `TRUE`(1) 继续枚举。
/// - 回调**不得 panic、不得解引用空指针**（`lparam` 为 0 时直接返回 0）：
///   panic 跨 `extern "system"` 边界是未定义行为。
/// - 返回顺序即 id 顺序，调用方（实例）依赖这一点。
///
/// # Errors
///
/// `EnumDisplayMonitors` 返回 0 时返回 [`crate::WinError`]。回调内部的
/// `GetMonitorInfoW` 失败只跳过该显示器（不使整体失败）——真机上更常见的是
/// 枚举到正在被拔掉的显示器。
#[cfg(windows)]
pub fn enumerate_monitors() -> crate::WinResult<Vec<Monitor>> {
    use std::cell::RefCell;
    use windows_sys::Win32::Foundation::{RECT, TRUE};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

    // 枚举回调的输出缓冲。
    //
    // 用 thread-local 而不是把 `Vec` 的指针塞进 `lparam`：后者要写
    // 「指针 → 整数 → 指针」的双向 `as` 转换（pedantic 下要逐个 `#[expect]`），
    // 且把一个别名指针的裸生命周期跨过 FFI 边界。回调是**同步**调用的，
    // 缓冲区在调用期间独占本线程，故 thread-local 更简单也更难写错。
    // （注意这里只能用 `//`：macro 调用挂 `///` 会被 rustc 判为 unused doc comment。）
    thread_local! {
        static SINK: RefCell<Vec<Monitor>> = const { RefCell::new(Vec::new()) };
    }

    /// `EnumDisplayMonitors` 的枚举回调：把每台显示器推进 [`SINK`]。
    ///
    /// 声明为**安全**的 `extern "system" fn`（安全函数可强转成
    /// `Option<unsafe extern "system" fn>`）：这样函数体里每个 FFI 调用都必须显式
    /// `unsafe`，不会被「`unsafe fn` 全函数免责」掩盖。
    extern "system" fn enum_proc(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        _lparam: isize,
    ) -> windows_sys::core::BOOL {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = u32::try_from(std::mem::size_of::<MONITORINFOEXW>()).unwrap_or(0);
        // SAFETY: EnumDisplayMonitors 给的 HMONITOR 在本回调内有效；目标结构是
        // 本栈帧上的 MONITORINFOEXW，首成员即 MONITORINFO 且 cbSize 已按 EXW 的大小
        // 告知，符合 Win32 对 GetMonitorInfoW 的既定用法；指针独占且可写。
        let ok = unsafe {
            GetMonitorInfoW(
                monitor,
                std::ptr::addr_of_mut!(info.monitorInfo).cast::<MONITORINFO>(),
            )
        };
        if ok != 0 {
            let rect = info.monitorInfo.rcMonitor;
            let name_len = info
                .szDevice
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(info.szDevice.len());
            SINK.with_borrow_mut(|sink| {
                sink.push(Monitor {
                    id: u32::try_from(sink.len()).unwrap_or(u32::MAX),
                    device: String::from_utf16_lossy(&info.szDevice[..name_len]),
                    origin_x: rect.left,
                    origin_y: rect.top,
                    width: u32::try_from(rect.right.saturating_sub(rect.left)).unwrap_or(0),
                    height: u32::try_from(rect.bottom.saturating_sub(rect.top)).unwrap_or(0),
                    primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                });
            });
        }
        TRUE
    }

    // 上一次调用若在半途失败可能留下残渣，先清空。
    SINK.with_borrow_mut(Vec::clear);
    // SAFETY: hdc/clip 传 null 表示「整个虚拟桌面」；lparam 未用（输出走 thread-local）；
    // 回调不 panic、不捕获环境、不跨线程。
    let ok =
        unsafe { EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(enum_proc), 0) };
    crate::error::check_bool("EnumDisplayMonitors", ok)?;
    Ok(SINK.with_borrow_mut(std::mem::take))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(id: u32, x: i32, y: i32, w: u32, h: u32, primary: bool) -> Monitor {
        Monitor {
            id,
            device: format!("\\\\.\\DISPLAY{}", id + 1),
            origin_x: x,
            origin_y: y,
            width: w,
            height: h,
            primary,
        }
    }

    #[test]
    fn contains_uses_half_open_bounds() {
        let m = mon(0, 0, 0, 1920, 1080, true);
        assert!(m.contains(0, 0));
        assert!(m.contains(1919, 1079));
        assert!(!m.contains(1920, 0), "右边界是开区间");
        assert!(!m.contains(0, 1080), "下边界是开区间");
        assert!(!m.contains(-1, 0));
    }

    #[test]
    fn to_global_offsets_by_monitor_origin() {
        let second = mon(1, 1920, 0, 1280, 1024, false);
        assert_eq!(second.to_global(0, 0), (1920, 0));
        assert_eq!(second.to_global(10, 20), (1930, 20));
        // 左/上为负的副屏
        let left = mon(1, -1280, -200, 1280, 1024, false);
        assert_eq!(left.to_global(0, 0), (-1280, -200));
    }

    #[test]
    fn to_global_clamps_out_of_range_local_coordinates() {
        // 控制器手里的画面尺寸比当前分辨率旧时会送来越界坐标：必须夹住，
        // 否则鼠标会跑到另一台显示器上。
        let m = mon(0, 100, 50, 800, 600, false);
        assert_eq!(m.to_global(5000, 5000), (899, 649));
        assert_eq!(m.to_global(-5, -5), (100, 50));
    }

    #[test]
    fn to_global_survives_degenerate_monitor() {
        let m = mon(0, 0, 0, 0, 0, false);
        assert_eq!(m.to_global(10, 10), (0, 0));
        assert!(!m.contains(0, 0));
    }

    #[test]
    fn virtual_screen_is_the_union_including_negative_origins() {
        let monitors = [
            mon(0, 0, 0, 1920, 1080, true),
            mon(1, -1280, -200, 1280, 1024, false),
        ];
        let vs = VirtualScreen::from_monitors(&monitors).unwrap();
        assert_eq!(vs.origin_x, -1280);
        assert_eq!(vs.origin_y, -200);
        assert_eq!(vs.width, 3200);
        assert_eq!(vs.height, 1280);
        assert!(vs.contains(-1280, -200));
        assert!(!vs.contains(-1281, -200));
    }

    #[test]
    fn virtual_screen_of_nothing_is_none() {
        assert!(VirtualScreen::from_monitors(&[]).is_none());
    }

    #[test]
    fn normalize_maps_corners_to_the_full_16_bit_range() {
        let vs = VirtualScreen {
            origin_x: 0,
            origin_y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(vs.normalize_absolute(0, 0), (0, 0));
        assert_eq!(vs.normalize_absolute(1919, 1079), (65535, 65535));
        // 画面正中：1920 宽的像素网格里，连续中点 959.5 落在下标 959 与 960 之间，
        // 故归一化后的量程中点 32767 必须被这两个下标**夹住**（这也是公式用
        // `extent - 1` 而不是 `extent` 的原因）。
        let (lo, _) = vs.normalize_absolute(959, 539);
        let (hi, _) = vs.normalize_absolute(960, 540);
        assert!(lo <= 32767 && 32767 <= hi, "lo={lo} hi={hi}");
        // 单调不减
        let (a, _) = vs.normalize_absolute(100, 0);
        let (b, _) = vs.normalize_absolute(101, 0);
        assert!(a < b);
    }

    #[test]
    fn normalize_is_relative_to_the_virtual_desktop_origin() {
        let vs = VirtualScreen {
            origin_x: -1280,
            origin_y: -200,
            width: 3200,
            height: 1280,
        };
        assert_eq!(vs.normalize_absolute(-1280, -200), (0, 0));
        assert_eq!(vs.normalize_absolute(1919, 1079), (65535, 65535));
    }

    #[test]
    fn normalize_clamps_coordinates_outside_the_virtual_desktop() {
        let vs = VirtualScreen {
            origin_x: 0,
            origin_y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(vs.normalize_absolute(-9999, -9999), (0, 0));
        assert_eq!(vs.normalize_absolute(9999, 9999), (65535, 65535));
    }

    #[test]
    fn normalize_survives_a_one_pixel_screen() {
        let vs = VirtualScreen {
            origin_x: 0,
            origin_y: 0,
            width: 1,
            height: 1,
        };
        assert_eq!(vs.normalize_absolute(0, 0), (0, 0));
        // 1 像素宽的屏幕上，归一化分母退化成 1，不能除零。
        assert_eq!(vs.normalize_absolute(5, 5), (65535, 65535));
    }

    #[test]
    fn bitblt_rect_matches_the_monitor_geometry() {
        let m = mon(1, -1280, -200, 1280, 1024, false);
        assert_eq!(m.bitblt_rect(), (-1280, -200, 1280, 1024));
    }
}
