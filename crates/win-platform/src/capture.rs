//! 屏幕采集（GDI `BitBlt`，**仅 Windows 编译**）。
//!
//! # 为什么不用 `xcap`
//!
//! 用户态那条老路径仍在用 `xcap`（一行不改）。但**实例**必须自持采集：
//!
//! 1. `xcap` 在安全桌面上的行为不可控（它按普通窗口站枚举显示器）；
//! 2. 采集必须与注入在**同一条挂在目标桌面上的线程**里做
//!    （见 [`crate::desktop`]），而 `xcap` 不提供「用哪个 DC」这种控制点。
//!
//! # 两个必须写对的地方
//!
//! 1. **`biHeight` 取负**：DIB 的正高度表示**自底向上**存储，采下来的图会上下颠倒。
//!    负高度 = 自顶向下，`bits` 就能直接按行拷贝。这是「画面翻转」类 bug 的根源，
//!    而且它极易被误判成 VP9 编解码的问题。
//! 2. **`CAPTUREBLT`**：不加这个位，分层窗口（layered window）不会被采进来——
//!    在锁屏/登录界面上，LogonUI 的某些元素正好是分层的。
//!
//! # 色彩通道
//!
//! DIB 出来的是 **BGRA**（`rgbBlue` 在最低地址）。转 RGBA 由
//! [`crate::pixels::bgra_to_rgba`] 负责，**不要**在这里顺手交换——探针会把原始
//! BGRA 直接编成 BMP（真值），转换后的 RGBA 只用于 ASCII 预览，两者分开才能
//! 让通道顺序的错误暴露出来而不是互相抵消。

use crate::monitor::Monitor;
use crate::WinResult;

/// 一帧采集结果：**自顶向下**的 BGRA 像素（每像素 4 字节）。
#[derive(Clone, PartialEq, Eq)]
pub struct CapturedFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 自上而下的 BGRA 数据，长度 = `width * height * 4`。
    pub bgra: Vec<u8>,
}

impl std::fmt::Debug for CapturedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印像素：一帧 1080p 是 8 MB，日志里打出来没有意义。
        f.debug_struct("CapturedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bgra.len())
            .finish()
    }
}

impl CapturedFrame {
    /// 转成 RGBA（供 `FrameSink` 那条接口用）。
    #[must_use]
    pub fn rgba(&self) -> Vec<u8> {
        crate::pixels::bgra_to_rgba(&self.bgra)
    }

    /// 平均亮度（0..=255），用于「这一帧是不是全黑」这类粗略判断。
    ///
    /// 锁屏界面**不是**全黑；全黑的帧通常意味着 `BitBlt` 采到了空桌面
    /// （比如线程其实挂在别的桌面上）——探针用它当一条廉价的自检。
    #[must_use]
    pub fn average_luma(&self) -> u8 {
        if self.bgra.is_empty() {
            return 0;
        }
        let chunks = self
            .bgra
            .as_chunks::<{ crate::pixels::BYTES_PER_PIXEL }>()
            .0;
        if chunks.is_empty() {
            return 0;
        }
        let sum: u64 = chunks
            .iter()
            .map(|p| {
                // BT.601 亮度，与 ASCII 缩略图用同一套系数。
                let b = u64::from(p[0]);
                let g = u64::from(p[1]);
                let r = u64::from(p[2]);
                (r * 299 + g * 587 + b * 114) / 1000
            })
            .sum();
        u8::try_from(sum / u64::try_from(chunks.len()).unwrap_or(1)).unwrap_or(u8::MAX)
    }
}

/// 屏幕 DC（`GetDC(NULL)`）的 RAII 包装：`Drop` 时 `ReleaseDC`。
struct ScreenDc(windows_sys::Win32::Graphics::Gdi::HDC);

impl ScreenDc {
    /// 取整个虚拟桌面的 DC。
    ///
    /// # Errors
    ///
    /// `GetDC` 返回空时返回 [`crate::WinError`]。
    fn open() -> WinResult<Self> {
        // SAFETY: hwnd 传 null 表示「屏幕 DC」，这是文档规定的用法。
        let dc = unsafe { windows_sys::Win32::Graphics::Gdi::GetDC(std::ptr::null_mut()) };
        Ok(Self(crate::handle::require_handle("GetDC(NULL)", dc)?))
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        // SAFETY: dc 由本对象独占（open 里已校验非空），且 hwnd 必须与 GetDC 时一致（null）。
        unsafe {
            windows_sys::Win32::Graphics::Gdi::ReleaseDC(std::ptr::null_mut(), self.0);
        }
    }
}

/// 内存 DC 的 RAII 包装：`Drop` 时 `DeleteDC`。
struct MemDc(windows_sys::Win32::Graphics::Gdi::HDC);

impl MemDc {
    /// 建一个与 `compatible` 兼容的内存 DC。
    ///
    /// # Errors
    ///
    /// `CreateCompatibleDC` 返回空时返回 [`crate::WinError`]。
    fn new(compatible: windows_sys::Win32::Graphics::Gdi::HDC) -> WinResult<Self> {
        // SAFETY: compatible 是有效的 DC（调用方持有）。
        let dc = unsafe { windows_sys::Win32::Graphics::Gdi::CreateCompatibleDC(compatible) };
        Ok(Self(crate::handle::require_handle(
            "CreateCompatibleDC",
            dc,
        )?))
    }
}

impl Drop for MemDc {
    fn drop(&mut self) {
        // SAFETY: dc 由本对象独占；DeleteDC 只对 CreateCompatibleDC 出来的 DC 合法。
        unsafe {
            windows_sys::Win32::Graphics::Gdi::DeleteDC(self.0);
        }
    }
}

/// DIB 位图的 RAII 包装：`Drop` 时 `DeleteObject`。
struct Dib(windows_sys::Win32::Graphics::Gdi::HBITMAP);

impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: 位图由本对象独占，且**调用方必须在 Drop 之前把它从 DC 里选出去**
        // （见 `capture_rect` 里显式的 SelectObject 还原）——删除仍在 DC 里的位图
        // 是文档明确禁止的。
        unsafe {
            windows_sys::Win32::Graphics::Gdi::DeleteObject(self.0);
        }
    }
}

/// 采集一个矩形区域（虚拟桌面坐标）。
///
/// 调用线程**必须**已经挂在目标桌面上（[`crate::desktop::InputDesktop`]）——
/// 否则采到的是自己所在桌面的画面，而且**不会报错**。
///
/// # Errors
///
/// 任一 GDI 调用失败时返回 [`crate::WinError`]，错误码原样保留。
pub fn capture_rect(x: i32, y: i32, width: u32, height: u32) -> WinResult<CapturedFrame> {
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateDIBSection, GdiFlush, SelectObject, BITMAPINFO, BI_RGB, CAPTUREBLT,
        DIB_RGB_COLORS, SRCCOPY,
    };

    if width == 0 || height == 0 {
        // 0 尺寸的 BitBlt 会因为「源矩形为空」失败，不如直接给出明确的空帧。
        return Ok(CapturedFrame {
            width,
            height,
            bgra: Vec::new(),
        });
    }

    let screen = ScreenDc::open()?;
    let mem = MemDc::new(screen.0)?;

    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = u32::try_from(std::mem::size_of::<
        windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER,
    >())
    .unwrap_or(0);
    info.bmiHeader.biWidth = i32::try_from(width).unwrap_or(i32::MAX);
    // 负高度 = 自顶向下（**关键**，见模块文档）。
    info.bmiHeader.biHeight = -i32::try_from(height).unwrap_or(i32::MAX);
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    info.bmiHeader.biSizeImage = 0;

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: info 是本栈帧上的 BITMAPINFO 且已按 32bpp BI_RGB 填好；
    // hsection 传 null + offset 0 表示「匿名内存位图」；ppvbits 是本栈帧上的指针，
    // 成功后由 GDI 拥有该内存，直到位图被删除。
    let bitmap = unsafe {
        CreateDIBSection(
            screen.0,
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            std::ptr::null_mut(),
            0,
        )
    };
    let bitmap = crate::handle::require_handle("CreateDIBSection", bitmap)?;
    let bitmap = Dib(bitmap);
    if bits.is_null() {
        // 带上**当场**的 last-error 而不是写死的 0：成功返回句柄却给不出像素指针是
        // 异常路径，那个码是唯一的线索。（`require_handle` 成功时不碰 last-error 槽，
        // 所以这里读到的仍是 `CreateDIBSection` 留下的那个。）
        return Err(crate::error::last_error("CreateDIBSection(bits)"));
    }

    // SAFETY: mem 是内存 DC，bitmap 是兼容的 32bpp 位图；返回值是被换出的旧对象，
    // 必须留到后面换回来（DC 里始终要有一张位图）。
    let old = unsafe { SelectObject(mem.0, bitmap.0) };
    // 这个返回值**必须**查。选入失败的话后面的 `BitBlt` 会安安静静地画进 DC 自带的
    // 那张 1×1 位图里，于是报告里出现的是一份「采集成功」的全黑画面，而
    // `WINDOWS-SECURE-DESKTOP.md` 教读者把「平均亮度 ≈ 0」解读成「线程在别的桌面上」
    // ——一次 GDI 调用失败就被误诊成桌面问题。选的是位图（不是 region），
    // 故 MSDN 规定失败返回 NULL。
    if old.is_null() {
        return Err(crate::error::last_error("SelectObject(位图)"));
    }

    // SAFETY: 两侧 DC 都有效；源坐标是虚拟桌面坐标（可为负，桌面 DC 覆盖整个虚拟桌面）；
    // rop 是文档规定的 SRCCOPY | CAPTUREBLT 组合。
    let ok = unsafe {
        BitBlt(
            mem.0,
            0,
            0,
            i32::try_from(width).unwrap_or(i32::MAX),
            i32::try_from(height).unwrap_or(i32::MAX),
            screen.0,
            x,
            y,
            SRCCOPY | CAPTUREBLT,
        )
    };
    // 无论成败都要把旧位图换回去，否则 Dib 的 Drop 会删掉一张仍在 DC 里的位图。
    // SAFETY: old 是上一步 SelectObject 换出的对象，类型是 HGDIOBJ。
    unsafe {
        SelectObject(mem.0, old);
    }
    crate::error::check_bool("BitBlt", ok)?;

    // SAFETY: 无参数；确保 GDI 的批处理已经落到位图内存里，再读 bits。
    let flushed = unsafe { GdiFlush() };
    crate::error::check_bool("GdiFlush", flushed)?;

    let len = usize::try_from(width)
        .unwrap_or(0)
        .saturating_mul(usize::try_from(height).unwrap_or(0))
        .saturating_mul(crate::pixels::BYTES_PER_PIXEL);
    // SAFETY: bits 指向 width*height*4 字节的 DIB 像素区（由 CreateDIBSection 按上述
    // 尺寸分配），在 bitmap 存活期间有效；这里立刻拷走，不持有指针。
    let bgra = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), len) }.to_vec();

    Ok(CapturedFrame {
        width,
        height,
        bgra,
    })
}

/// 采集一台显示器的全部像素。
///
/// # Errors
///
/// 同 [`capture_rect`]。
pub fn capture_monitor(monitor: &Monitor) -> WinResult<CapturedFrame> {
    let (x, y, cx, cy) = monitor.bitblt_rect();
    capture_rect(
        x,
        y,
        u32::try_from(cx).unwrap_or(0),
        u32::try_from(cy).unwrap_or(0),
    )
}

/// 采集主显示器（探针用：锁屏界面上通常只有主屏有内容）。
///
/// # Errors
///
/// 没有显示器时返回 [`crate::WinError`]；其余同 [`capture_rect`]。
pub fn capture_primary() -> WinResult<CapturedFrame> {
    let monitors = crate::monitor::enumerate_monitors()?;
    let primary = monitors
        .iter()
        .find(|m| m.primary)
        .or_else(|| monitors.first())
        .ok_or_else(|| crate::WinError::new("EnumDisplayMonitors", 0))?;
    capture_monitor(primary)
}

/// 采集整个虚拟桌面（多显示器包围盒）。
///
/// # Errors
///
/// 没有显示器时返回 [`crate::WinError`]；其余同 [`capture_rect`]。
pub fn capture_virtual_screen() -> WinResult<CapturedFrame> {
    let monitors = crate::monitor::enumerate_monitors()?;
    let vs = crate::monitor::VirtualScreen::from_monitors(&monitors)
        .ok_or_else(|| crate::WinError::new("EnumDisplayMonitors", 0))?;
    capture_rect(vs.origin_x, vs.origin_y, vs.width, vs.height)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 真机上跑：能采到一帧像素，且尺寸与显示器一致。
    ///
    /// 需要有一个**交互式**会话（CI 的 windows runner 是交互式登录的会话 1，
    /// 但 `BitBlt` 在无窗口站的服务会话里会失败——所以这条测试只在
    /// 交互式会话下有意义，失败信息里带上错误码便于分辨）。
    #[test]
    fn captures_the_primary_monitor_on_a_real_machine() {
        let monitors = crate::monitor::enumerate_monitors().expect("枚举显示器");
        assert!(!monitors.is_empty(), "真机上应当至少有一台显示器");
        let frame = capture_primary().expect("采集主显示器");
        assert!(frame.width > 0 && frame.height > 0, "{frame:?}");
        assert_eq!(
            frame.bgra.len(),
            usize::try_from(frame.width).unwrap() * usize::try_from(frame.height).unwrap() * 4,
            "像素缓冲长度必须与尺寸自洽"
        );
    }

    #[test]
    fn zero_sized_capture_returns_an_empty_frame() {
        let frame = capture_rect(0, 0, 0, 0).expect("0 尺寸不该报错");
        assert!(frame.bgra.is_empty());
    }
}
