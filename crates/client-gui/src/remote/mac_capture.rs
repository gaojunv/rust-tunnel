//! macOS 采集适配（ScreenCaptureKit → 引擎的 [`ScreenSource`]）。
//!
//! 与 Windows 的 `win_video` 的 `WinVideoScreen` 同构：`rust-tunnel-mac-capture` 负责
//! SCK 流、变化检测与补丁提取，本模块只做类型映射与惰性初始化。
//!
//! # GPU 零拷贝
//!
//! SCK 的帧本来就是 `IOSurface` 背书的 `CVPixelBuffer`：`capture_gpu` 把它原样交给
//! 引擎（[`RawFrame::gpu`]），编码器适配器再直通给 VideoToolbox。采集失败时**回退到
//! CPU `capture()`**（与 Windows 语义一致），会话不会因为一次 GPU 失败而中断。

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use rust_tunnel_mac_capture::{
    CaptureInfo as MacCaptureInfo, CapturedFrame, CapturedGpuFrame, CursorShape as MacCursorShape,
    DisplayInfo as MacDisplay, GpuFrameBuffer, MacCapture,
};
use rust_tunnel_remote::screen::{
    CaptureInfo, CapturedPatches, CursorShape, GpuFrame, ScreenPatch,
};
use rust_tunnel_remote::{DisplayInfo, RawFrame, RemoteError, RemoteResult, ScreenSource};

/// 引擎侧 [`RawFrame::gpu`] 的 macOS 载体：一块 SCK 的 `CVPixelBuffer`。
///
/// SCK 的采集流建流时已按 `max_encode_width` 输出（GPU 内缩放），所以缓冲尺寸通常就是
/// 逻辑编码尺寸；`source_width/source_height` 仍单独保留（显示器比上限小时两者相等，
/// 上限变化会重建采集流）。
pub(crate) struct MacGpuFrame {
    /// 采集帧的像素缓冲（引用计数，不拷贝像素）。
    pub(crate) buffer: GpuFrameBuffer,
    /// 缓冲的实际宽度。
    pub(crate) source_width: u32,
    /// 缓冲的实际高度。
    pub(crate) source_height: u32,
}

impl GpuFrame for MacGpuFrame {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// SAFETY: `CVPixelBuffer` 是不可变、引用计数的 CF 对象，文档契约允许在任意线程 lock
// 读取；SCK 在回调之外不再改写这一帧，我们只持有引用。`source_width/height` 是普通整数。
unsafe impl Send for MacGpuFrame {}
unsafe impl Sync for MacGpuFrame {}

/// 基于 ScreenCaptureKit 的采集源。
///
/// `MacCapture` 在第一次使用时惰性创建：构造发生在 GUI 启动阶段，那时可能还没授予
/// 屏幕录制权限；把失败推迟到真正采集，会话能给出准确错误而不是启动即失败。
/// 显示器物理尺寸的短缓存项：`(显示器 id, 物理像素, 取样时刻)`。
type PhysicalCache = Option<(u32, (u32, u32), Instant)>;

pub struct MacCaptureScreen {
    inner: Mutex<Option<MacCapture>>,
    /// 最近一次采集/操作后的平台读数快照。
    ///
    /// `capture_info()` 每秒被会话循环内联调用一次；它**绝不能**去抢 `inner`
    /// ——采集任务会在那把锁里阻塞最多一个帧等待窗口（`FRAME_WAIT` 20 ms），
    /// 每秒因此冻住输入注入/心跳 20 ms 是白送的卡顿。快照在锁内顺手刷新，读取只抢
    /// 这把几乎不持有的锁。
    telemetry: Mutex<CaptureInfo>,
    /// 显示器 → 物理像素尺寸的短缓存（`encode_pixels` 用）。
    ///
    /// `display_physical_size` 是 CoreGraphics 调用；引擎在补丁模式下每拍都会问一次，
    /// 没必要每帧付一次。显示器几何复查本身就是 2 秒一次，缓存 2 秒足够。
    physical_cache: Mutex<PhysicalCache>,
}

impl MacCaptureScreen {
    /// 新建（尚未打开 SCK）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            telemetry: Mutex::new(CaptureInfo {
                backend: "ScreenCaptureKit".to_owned(),
                ..CaptureInfo::default()
            }),
            physical_cache: Mutex::new(None),
        }
    }

    /// 在锁保护下取采集器；首次调用时创建。
    fn with_capture<R>(
        &self,
        f: impl FnOnce(&mut MacCapture) -> RemoteResult<R>,
    ) -> RemoteResult<R> {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            let capture = MacCapture::new().map_err(|err| {
                RemoteError::Platform(format!("ScreenCaptureKit 初始化失败: {err}"))
            })?;
            *guard = Some(capture);
        }
        let capture = guard
            .as_mut()
            .ok_or_else(|| RemoteError::Platform("ScreenCaptureKit 未初始化".to_owned()))?;
        let result = f(capture);
        // 平台读数只读几个原子/缓存字段，代价可忽略；顺手刷新快照让 `capture_info()`
        // 永远不必抢这把可能被采集阻塞的锁。
        *self
            .telemetry
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = capture_info(capture.info());
        result
    }
}

impl Default for MacCaptureScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenSource for MacCaptureScreen {
    fn displays(&self) -> Vec<DisplayInfo> {
        // **不经过 `MacCapture` 实例**：引擎每 5 秒在会话循环里内联调用本方法，
        // `displays()` 因此必须是亚毫秒的 CoreGraphics 同步枚举（SCK 的异步内容枚举
        // 实测 70–80 ms，会周期性地冻住输入注入与采集节拍）。
        rust_tunnel_mac_capture::displays()
            .map(display_list)
            .unwrap_or_default()
    }

    /// 采集/编码**物理像素**尺寸（引擎据此算原生码率倍率与补丁目标帧缓冲）。
    ///
    /// `DisplayInfo` 报的是输入映射用的**逻辑点**（4K 屏 1920×1080），与采集像素
    /// （3840×2160）差一个缩放比。这里缓存 2 秒，避免补丁模式每拍一次 CoreGraphics 调用。
    fn encode_pixels(&self, display: u32) -> Option<(u32, u32)> {
        let mut cache = self
            .physical_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((cached_id, size, at)) = *cache {
            if cached_id == display && at.elapsed() < Duration::from_secs(2) {
                return Some(size);
            }
        }
        let size = rust_tunnel_mac_capture::display_physical_size(display)?;
        *cache = Some((display, size, Instant::now()));
        Some(size)
    }

    fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
        self.with_capture(|capture| {
            capture
                .capture(display)
                .map(raw_frame)
                .map_err(|err| RemoteError::Platform(format!("ScreenCaptureKit 采集失败: {err}")))
        })
    }

    /// `CVPixelBuffer` 直通（零拷贝）在 macOS 上恒可用：SCK 的帧本来就是 GPU 侧缓冲。
    fn supports_gpu_capture(&self) -> bool {
        true
    }

    /// 采集一帧 GPU 直通画面；失败时回退 CPU 采集（与 Windows 语义一致）。
    fn capture_gpu(&self, display: u32) -> RemoteResult<RawFrame> {
        let gpu = self.with_capture(|capture| {
            capture
                .capture_gpu(display)
                .map(gpu_raw_frame)
                .map_err(|err| {
                    RemoteError::Platform(format!("ScreenCaptureKit GPU 采集失败: {err}"))
                })
        });
        match gpu {
            Ok(frame) => Ok(frame),
            Err(err) => {
                // 一次失败的 GPU 采集（权限、流重建、转换会话）不该让会话来一刀切：
                // 下一拍走 CPU 路径，画面继续。
                tracing::warn!(error = %err, "GPU 采集失败，本拍回退 CPU 采集");
                self.capture(display)
            }
        }
    }

    fn set_max_encode_width(&self, width: u32) {
        // 失败只记日志：下一拍采集仍按默认上限工作，会话继续。
        let _ = self.with_capture(|capture| {
            capture.set_max_encode_width(width);
            Ok(())
        });
    }

    fn supports_patches(&self) -> bool {
        true
    }

    fn capture_patches(
        &self,
        display: u32,
        scale: f32,
        max_width: u32,
    ) -> RemoteResult<Option<CapturedPatches>> {
        self.with_capture(|capture| {
            capture
                .capture_patches(display, scale, max_width)
                .map(|patches| patches.map(captured_patches))
                .map_err(|err| {
                    RemoteError::Platform(format!("ScreenCaptureKit 补丁采集失败: {err}"))
                })
        })
    }

    fn capture_info(&self) -> CaptureInfo {
        // 只读快照，**不抢采集锁**（见 `telemetry` 的说明）。
        self.telemetry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn cursor_shape(&self) -> Option<CursorShape> {
        // 形状采样与画面无关，失败不该影响会话。
        self.with_capture(|capture| Ok(capture.cursor_shape().map(cursor_shape)))
            .ok()
            .flatten()
    }
}

/// 构造 GUI 路径的 macOS 采集源。
#[must_use]
pub fn screen() -> MacCaptureScreen {
    MacCaptureScreen::new()
}

fn display_list(displays: Vec<MacDisplay>) -> Vec<DisplayInfo> {
    displays
        .into_iter()
        .map(|display| DisplayInfo {
            id: display.id,
            width: display.width,
            height: display.height,
            name: display.name,
            primary: display.primary,
        })
        .collect()
}

fn raw_frame(frame: CapturedFrame) -> RawFrame {
    RawFrame {
        display: frame.display,
        width: frame.width,
        height: frame.height,
        rgba: frame.rgba,
        gpu: None,
        changed: frame.changed,
        changed_permille: frame.changed_permille,
    }
}

/// GPU 直通帧：`width`/`height` 是**逻辑编码尺寸**，缓冲保持原始物理分辨率。
fn gpu_raw_frame(frame: CapturedGpuFrame) -> RawFrame {
    RawFrame {
        display: frame.display,
        width: frame.width,
        height: frame.height,
        rgba: Vec::new(),
        gpu: Some(std::sync::Arc::new(MacGpuFrame {
            buffer: frame.buffer,
            source_width: frame.source_width,
            source_height: frame.source_height,
        })),
        changed: frame.changed,
        changed_permille: frame.changed_permille,
    }
}

fn captured_patches(patches: rust_tunnel_mac_capture::CapturedPatches) -> CapturedPatches {
    CapturedPatches {
        display: patches.display,
        width: patches.width,
        height: patches.height,
        changed_permille: patches.changed_permille,
        rects: patches
            .rects
            .into_iter()
            .map(|rect| ScreenPatch {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                nv12: rect.nv12,
            })
            .collect(),
    }
}

fn capture_info(info: MacCaptureInfo) -> CaptureInfo {
    CaptureInfo {
        backend: info.backend,
        adapter: info.adapter,
        gpu_capture: info.gpu_capture,
        gpu_disabled: info.gpu_disabled,
        gpu_disabled_reason: info.gpu_disabled_reason,
        fallbacks: info.fallbacks,
        no_new_frame: info.no_new_frame,
        open_fallbacks: info.open_fallbacks,
    }
}

fn cursor_shape(shape: MacCursorShape) -> CursorShape {
    // 两个枚举的变体一一对应（线上判别式由 remote 侧冻结，这里只做名字映射）。
    match shape {
        MacCursorShape::Arrow => CursorShape::Arrow,
        MacCursorShape::IBeam => CursorShape::IBeam,
        MacCursorShape::Hand => CursorShape::Hand,
        MacCursorShape::Crosshair => CursorShape::Crosshair,
        MacCursorShape::Wait => CursorShape::Wait,
        MacCursorShape::AppStarting => CursorShape::AppStarting,
        MacCursorShape::Help => CursorShape::Help,
        MacCursorShape::SizeNS => CursorShape::SizeNS,
        MacCursorShape::SizeWE => CursorShape::SizeWE,
        MacCursorShape::SizeNWSE => CursorShape::SizeNWSE,
        MacCursorShape::SizeNESW => CursorShape::SizeNESW,
        MacCursorShape::SizeAll => CursorShape::SizeAll,
        MacCursorShape::No => CursorShape::No,
        MacCursorShape::UpArrow => CursorShape::UpArrow,
        MacCursorShape::Unknown => CursorShape::Unknown,
        MacCursorShape::Hidden => CursorShape::Hidden,
    }
}
