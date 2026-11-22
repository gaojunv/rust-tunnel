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

use rust_tunnel_mac_capture::{
    CaptureInfo as MacCaptureInfo, CapturedFrame, CapturedGpuFrame, CursorShape as MacCursorShape,
    DisplayInfo as MacDisplay, GpuFrameBuffer, MacCapture,
};
use rust_tunnel_remote::screen::{
    CaptureInfo, CapturedPatches, CursorShape, GpuFrame, ScreenPatch,
};
use rust_tunnel_remote::{DisplayInfo, RawFrame, RemoteError, RemoteResult, ScreenSource};

/// 引擎侧 [`RawFrame::gpu`] 的 macOS 载体：一块 SCK 的 `CVPixelBuffer`（完整物理帧）。
///
/// `frame.width/height`（逻辑编码尺寸）可能小于缓冲的物理尺寸（4K 采集先按会话上限
/// 封顶：默认 1920，原生分辨率下就是 3840），故这里同时保留 `source_width/source_height`
/// 供编码器在 GPU 上缩放。
pub(crate) struct MacGpuFrame {
    /// 采集帧的像素缓冲（引用计数，不拷贝像素）。
    pub(crate) buffer: GpuFrameBuffer,
    /// 缓冲的物理宽度。
    pub(crate) source_width: u32,
    /// 缓冲的物理高度。
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
pub struct MacCaptureScreen {
    inner: Mutex<Option<MacCapture>>,
}

impl MacCaptureScreen {
    /// 新建（尚未打开 SCK）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
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
        f(capture)
    }
}

impl Default for MacCaptureScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenSource for MacCaptureScreen {
    fn displays(&self) -> Vec<DisplayInfo> {
        self.with_capture(|capture| Ok(display_list(capture.displays())))
            .unwrap_or_default()
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
        self.with_capture(|capture| Ok(capture_info(capture.info())))
            .unwrap_or_else(|_| CaptureInfo {
                backend: "ScreenCaptureKit".to_owned(),
                ..CaptureInfo::default()
            })
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
