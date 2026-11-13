//! Media Foundation H.264 4:2:0 硬件解码器（控制器侧）。
//!
//! # 数据通路
//!
//! 输入端与 remote 数据面完全一致：**4 字节大端长度前缀 + Annex B**，允许一次
//! 传入多个拼接的包。每个包是一个完整访问单元（IDR 带内联 SPS/PPS），直接作为
//! 一个 `IMFSample` 喂给 `MSH264DecoderMFT`。输出优先协商 **NV12**，失败时回退
//! `YV12` / `IYUV`，最后在 CPU 上按 BT.709 limited range 转成自顶向下 RGBA8。
//!
//! **可见区域**：H.264 的编码尺寸按 16 像素对齐（1080p 实际是 1920x1088、
//! 360p 是 640x368），解码器在输出媒体类型上用 `MF_MT_MINIMUM_DISPLAY_APERTURE`
//! 标出真正的可见区域。本模块按该光圈裁剪，返回的 `width × height` 是显示尺寸
//! 而不是编码尺寸——否则 remote 的缩放/输入坐标会整体错位。
//!
//! # 为什么是 Media Foundation 而不是 D3D11 Video Processor
//!
//! 本 crate 的 H.264 码流由 NVENC 产生，固定 4:2:0 8-bit。微软自带的
//! `CLSID_MSH264DecoderMFT` 从 Windows 8 起随系统提供，支持 DXVA 加速且无需
//! 额外 SDK；D3D11 Video Processor 只是它的下游缩放/色彩转换器，单独用不了。
//! 输出到系统内存（NV12）而不是 GPU 纹理，是为了直接对接 remote 的
//! `DecodedVideoFrame { rgba }`——控制器本来就要把像素给 UI 或转码。
//!
//! # 低延迟
//!
//! 远程控制不能容忍多帧缓冲：创建后立即在 MFT 属性上设 `MF_LOW_LATENCY = 1`，
//! 并发送 `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING` / `START_OF_STREAM`。编码端已经
//! 关闭 B 帧（`frameIntervalP = 1`），解码端通常一进一出。
//!
//! # 线程
//!
//! [`H264Decoder`] 只实现 `Send`、**不实现 `Sync`**：它独占持有 COM 的
//! `IMFTransform`，调用方按约定把它移进 `spawn_blocking` 后只由一个线程使用。
//! `decode` 是同步阻塞调用。

mod color;

use crate::WinVideoError;
use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::OnceLock;
use windows::core::{IUnknown, Interface as _};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, IMF2DBuffer, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFStartup,
    MFVideoArea, MFVideoFormat_H264, MFVideoFormat_IYUV, MFVideoFormat_NV12, MFVideoFormat_YV12,
    MFVideoInterlace_Progressive, MFSTARTUP_FULL, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MF_E_NOTACCEPTING, MF_E_NO_MORE_TYPES, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_GEOMETRIC_APERTURE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

/// 未指定帧率时使用的名义帧率（解码本身不依赖它，只用于时间戳与媒体类型）。
const DEFAULT_FPS: u32 = 60;
/// 一秒的 100ns 单位数（Media Foundation 的时间基）。
const HNS_PER_SECOND: i64 = 10_000_000;
/// 枚举输出媒体类型的安全上限（防止驱动异常时死循环）。
const MAX_OUTPUT_TYPES: u32 = 64;
/// 调用方未提供输出样本时，缓冲区的额外余量。
const OUTPUT_BUFFER_SLACK: u32 = 4096;

/// 解码器实际协商到的输出色度布局。
///
/// NV12 是主路径；YV12 / IYUV 只在 MFT 拒绝 NV12 时出现。三者的像素内容相同
/// （BT.709 limited range 4:2:0），区别只在内存布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264OutputFormat {
    /// 半平面：Y 平面 + 交织 UV 平面（主路径）。
    Nv12,
    /// 全平面：Y 平面 + V 平面 + U 平面。
    Yv12,
    /// 全平面：Y 平面 + U 平面 + V 平面。
    Iyuv,
}

impl H264OutputFormat {
    /// 稳定的短名称，供日志与真机探针输出。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nv12 => "NV12",
            Self::Yv12 => "YV12",
            Self::Iyuv => "IYUV",
        }
    }
}

/// 解码后的一帧 RGBA8（自顶向下，alpha 恒为 255）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedH264Frame {
    /// 可见宽度（已按 `MF_MT_MINIMUM_DISPLAY_APERTURE` 裁掉宏块填充）。
    pub width: u32,
    /// 可见高度（1080p 的 1088 行编码高度会裁成 1080）。
    pub height: u32,
    /// 自顶向下 RGBA8，长度恰为 `width * height * 4`。
    pub rgba: Vec<u8>,
}

/// 当前协商到的输出格式。
///
/// `coded_width × coded_height` 是 **MFT 缓冲区**的尺寸（H.264 编码尺寸按 16
/// 像素对齐，1080p 实际是 1920x1088）；`display` 是从
/// `MF_MT_MINIMUM_DISPLAY_APERTURE` 读出的**可见区域**（1080p 为 1920x1080）。
/// 两者不同时必须裁掉填充，否则调用方拿到的是多出几行的画面。
#[derive(Debug, Clone, Copy)]
struct OutputFormat {
    coded_width: u32,
    coded_height: u32,
    display: DisplayRect,
    layout: H264OutputFormat,
    /// 连续内存降级路径使用的字节 stride（来自 `MF_MT_DEFAULT_STRIDE`）。
    stride: u32,
}

/// 可见区域（相对编码帧左上角）。
#[derive(Debug, Clone, Copy)]
struct DisplayRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// 一个可复用的 Media Foundation H.264 解码器。
pub struct H264Decoder {
    transform: IMFTransform,
    output: OutputFormat,
    requested_width: u32,
    requested_height: u32,
    fps: u32,
    /// MFT 是否自己分配输出样本（`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`）。
    provides_samples: bool,
    /// 调用方必须提供的输出缓冲区最小字节数。
    output_buffer_size: u32,
    /// 下一帧输入样本的时间戳（100ns）。
    next_pts: i64,
    /// 单帧时长（100ns）。
    frame_duration: i64,
}

impl std::fmt::Debug for H264Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H264Decoder")
            .field("requested", &(self.requested_width, self.requested_height))
            .field(
                "decoded",
                &(
                    self.output.display.width,
                    self.output.display.height,
                    self.output.coded_width,
                    self.output.coded_height,
                ),
            )
            .field("format", &self.output.layout)
            .finish_non_exhaustive()
    }
}

// SAFETY: `H264Decoder` 独占持有一个进程内 MFT（`MSH264DecoderMFT`）。COM 接口
// 只在持有者线程上调用，结构体**不实现 `Sync`**，所以任何时刻只有一个线程能
// 访问它；`decode`/`reconfigure` 都取 `&mut self`。控制器把解码器移进
// `spawn_blocking` 后不会跨线程共享，满足 COM 的单线程使用约定。
unsafe impl Send for H264Decoder {}

/// 编译期断言：解码器必须能移进 `spawn_blocking`（只要求 `Send`）。
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<H264Decoder>();
};

impl H264Decoder {
    /// 创建一个解码 `width × height` 码流的 H.264 解码器。
    ///
    /// 宽高是**第一帧的预期尺寸**，用于配置输入媒体类型；若码流后续因
    /// `ScreenConfig` 变化而换分辨率，调用 [`H264Decoder::reconfigure`] 重建，
    /// 或依赖 MFT 的 `MF_E_TRANSFORM_STREAM_CHANGE` 自动重协商输出类型。
    ///
    /// # Errors
    /// 尺寸为 0、Media Foundation 初始化失败、`MSH264DecoderMFT` 不可用
    /// （非 Windows 或系统组件缺失）或媒体类型被拒绝时返回 [`WinVideoError`]。
    pub fn new(width: u32, height: u32) -> Result<Self, WinVideoError> {
        let (transform, output, provides_samples, output_buffer_size) =
            create_pipeline(width, height, DEFAULT_FPS)?;
        Ok(Self {
            transform,
            output,
            requested_width: width,
            requested_height: height,
            fps: DEFAULT_FPS,
            provides_samples,
            output_buffer_size,
            next_pts: 0,
            frame_duration: HNS_PER_SECOND / i64::from(DEFAULT_FPS),
        })
    }

    /// 解码器的请求尺寸（`new` / 最近一次 `reconfigure` 的值）。
    #[must_use]
    pub fn requested_dimensions(&self) -> (u32, u32) {
        (self.requested_width, self.requested_height)
    }

    /// 当前协商到的输出尺寸与色度布局。
    #[must_use]
    pub fn negotiated(&self) -> (u32, u32, H264OutputFormat) {
        (
            self.output.display.width,
            self.output.display.height,
            self.output.layout,
        )
    }

    /// 重建解码器以适配新的编码尺寸（`ScreenConfig` 变化时调用）。
    ///
    /// 重建会丢弃当前参考帧，下一帧必须是 IDR；这与 remote 的分辨率变更规则
    /// （重建编码器 + 强制 IDR）一致。
    ///
    /// # Errors
    /// 新尺寸非法或 MFT 创建/媒体类型协商失败时返回 [`WinVideoError`]。
    pub fn reconfigure(&mut self, width: u32, height: u32) -> Result<(), WinVideoError> {
        let (transform, output, provides_samples, output_buffer_size) =
            create_pipeline(width, height, self.fps)?;
        self.transform = transform;
        self.output = output;
        self.requested_width = width;
        self.requested_height = height;
        self.provides_samples = provides_samples;
        self.output_buffer_size = output_buffer_size;
        self.next_pts = 0;
        Ok(())
    }

    /// 解码一批（4 字节大端长度前缀 + Annex B，可拼接多个包）码流。
    ///
    /// 返回本次调用产生的**最后一帧**；MFT 缓冲了输入、暂时没有输出时返回
    /// `None`。空输入返回 `None`。
    ///
    /// # Errors
    /// 长度前缀不完整/越界、空包、媒体类型或 MFT 处理失败时返回
    /// [`WinVideoError`]。
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedH264Frame>, WinVideoError> {
        if data.is_empty() {
            return Ok(None);
        }
        // 解码任务可能在线程池线程上运行：COM 初始化是线程局部的，必须每次确认。
        ensure_mf_started()?;
        ensure_com_initialized()?;

        let mut last = None;
        let mut offset = 0usize;
        while offset < data.len() {
            let header = data.get(offset..offset.saturating_add(4)).ok_or_else(|| {
                WinVideoError::Decode(format!(
                    "长度前缀不完整：偏移 {offset}，剩余 {} 字节",
                    data.len().saturating_sub(offset)
                ))
            })?;
            let length = usize::try_from(u32::from_be_bytes([
                header[0], header[1], header[2], header[3],
            ]))
            .map_err(|_| WinVideoError::Decode("包长度无法转为 usize".to_owned()))?;
            if length == 0 {
                return Err(WinVideoError::Decode(format!("偏移 {offset} 处出现空包")));
            }
            let start = offset.saturating_add(4);
            let end = start
                .checked_add(length)
                .ok_or_else(|| WinVideoError::Decode("包长度溢出".to_owned()))?;
            let payload = data.get(start..end).ok_or_else(|| {
                WinVideoError::Decode(format!(
                    "包长度越界：偏移 {offset} 声明 {length} 字节，实际剩余 {}",
                    data.len().saturating_sub(start)
                ))
            })?;
            self.submit(payload)?;
            if let Some(frame) = self.drain_output()? {
                last = Some(frame);
            }
            offset = end;
        }
        Ok(last)
    }

    /// 把一个访问单元送进 MFT。
    fn submit(&mut self, payload: &[u8]) -> Result<(), WinVideoError> {
        let length = u32::try_from(payload.len())
            .map_err(|_| WinVideoError::Decode("单个包超过 4 GiB".to_owned()))?;

        // SAFETY: `MFCreateMemoryBuffer` 无外部指针参数，返回新的 COM 对象。
        let buffer = unsafe { MFCreateMemoryBuffer(length) }
            .map_err(|err| decode_err("MFCreateMemoryBuffer", &err))?;

        let mut destination: *mut u8 = ptr::null_mut();
        // SAFETY: `destination` 指向本栈帧；`buffer` 在调用期间有效。
        unsafe { buffer.Lock(&raw mut destination, None, None) }
            .map_err(|err| decode_err("IMFMediaBuffer::Lock", &err))?;
        if destination.is_null() {
            // SAFETY: 已成功 Lock，必须配对 Unlock。
            let _ = unsafe { buffer.Unlock() };
            return Err(WinVideoError::Decode(
                "MFCreateMemoryBuffer 返回空缓冲区".to_owned(),
            ));
        }
        // SAFETY: Lock 成功后驱动保证 `destination` 至少可写 `length` 字节；
        // 源切片与目标缓冲区不重叠。
        unsafe {
            ptr::copy_nonoverlapping(payload.as_ptr(), destination, payload.len());
        }
        // SAFETY: 与上面的 Lock 配对。
        unsafe { buffer.Unlock() }.map_err(|err| decode_err("IMFMediaBuffer::Unlock", &err))?;
        // SAFETY: `buffer` 有效；长度已由 Lock 保证不超过最大值。
        unsafe { buffer.SetCurrentLength(length) }
            .map_err(|err| decode_err("IMFMediaBuffer::SetCurrentLength", &err))?;

        // SAFETY: `MFCreateSample` 无外部指针参数，返回新的 COM 对象。
        let sample =
            unsafe { MFCreateSample() }.map_err(|err| decode_err("MFCreateSample", &err))?;
        // SAFETY: `sample` 与 `buffer` 都在本函数内有效；AddBuffer 增加引用计数。
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(|err| decode_err("IMFSample::AddBuffer", &err))?;
        // SAFETY: `sample` 有效；时间戳只影响输出排序，不影响解码正确性。
        unsafe {
            sample
                .SetSampleTime(self.next_pts)
                .map_err(|err| decode_err("IMFSample::SetSampleTime", &err))?;
            sample
                .SetSampleDuration(self.frame_duration)
                .map_err(|err| decode_err("IMFSample::SetSampleDuration", &err))?;
        }
        self.next_pts = self.next_pts.saturating_add(self.frame_duration);

        // SAFETY: `transform` 有效且输入类型已协商；`sample` 在调用期间存活。
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => Ok(()),
            Err(err) if err.code() == MF_E_NOTACCEPTING => {
                // MFT 的内部输入队列满了：先排空输出，再重试一次。
                let _ = self.drain_output()?;
                // SAFETY: 同上次调用；`sample` 仍在本栈帧内。
                unsafe { self.transform.ProcessInput(0, &sample, 0) }
                    .map_err(|retry| decode_err("ProcessInput（重试）", &retry))
            }
            Err(err) => Err(decode_err("ProcessInput", &err)),
        }
    }

    /// 循环取走 MFT 已产生的全部输出帧，返回最后一帧。
    fn drain_output(&mut self) -> Result<Option<DecodedH264Frame>, WinVideoError> {
        let mut last = None;
        loop {
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            if !self.provides_samples {
                output.pSample = ManuallyDrop::new(Some(self.new_output_sample()?));
            }

            let mut status = 0u32;
            // SAFETY: `transform` 有效；`output` 是长度为 1 的输出数组，`status`
            // 指向本栈帧。`pSample`/`pEvents` 是 ManuallyDrop，稍后手动取出。
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &raw mut status)
            };
            // SAFETY: 无论成功失败都必须把 ManuallyDrop 里的 COM 引用取出来释放，
            // 否则每次调用都会泄漏一个引用计数。
            let produced = unsafe { ManuallyDrop::take(&mut output.pSample) };
            let events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
            drop(events);

            match result {
                Ok(()) => {
                    let sample = produced.ok_or_else(|| {
                        WinVideoError::Decode("ProcessOutput 成功但未返回样本".to_owned())
                    })?;
                    last = Some(self.convert_sample(&sample)?);
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(last),
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // 码流参数（分辨率/色度）变了：重新协商输出类型再继续取。
                    self.negotiate_after_stream_change()?;
                }
                Err(err) => return Err(decode_err("ProcessOutput", &err)),
            }
        }
    }

    /// 创建一个符合当前输出流要求的样本。
    fn new_output_sample(&self) -> Result<IMFSample, WinVideoError> {
        // SAFETY: `MFCreateMemoryBuffer` 无外部指针参数。
        let buffer = unsafe { MFCreateMemoryBuffer(self.output_buffer_size.max(1)) }
            .map_err(|err| decode_err("MFCreateMemoryBuffer（输出）", &err))?;
        // SAFETY: `MFCreateSample` 无外部指针参数。
        let sample = unsafe { MFCreateSample() }
            .map_err(|err| decode_err("MFCreateSample（输出）", &err))?;
        // SAFETY: 两个 COM 对象都在本函数内有效；AddBuffer 增加引用计数。
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(|err| decode_err("IMFSample::AddBuffer（输出）", &err))?;
        Ok(sample)
    }

    /// 处理 `MF_E_TRANSFORM_STREAM_CHANGE`：按新的可用类型重设输出并刷新流信息。
    fn negotiate_after_stream_change(&mut self) -> Result<(), WinVideoError> {
        self.output =
            negotiate_output_type(&self.transform, self.requested_width, self.requested_height)?;
        let (provides_samples, output_buffer_size) =
            output_stream_info(&self.transform, &self.output)?;
        self.provides_samples = provides_samples;
        self.output_buffer_size = output_buffer_size;
        Ok(())
    }

    /// 把一个输出样本转换成 RGBA。
    fn convert_sample(&self, sample: &IMFSample) -> Result<DecodedH264Frame, WinVideoError> {
        // SAFETY: `sample` 有效；返回的缓冲区在作用域内保持引用计数。
        // `ConvertToContiguousBuffer` 失败时退回第一个缓冲区（部分 MFT 只给 2D 缓冲）。
        let buffer: IMFMediaBuffer = match unsafe { sample.ConvertToContiguousBuffer() } {
            Ok(buffer) => buffer,
            Err(_) => unsafe { sample.GetBufferByIndex(0) }
                .map_err(|err| decode_err("IMFSample::GetBufferByIndex", &err))?,
        };

        if let Ok(buffer_2d) = buffer.cast::<IMF2DBuffer>() {
            let mut scanline: *mut u8 = ptr::null_mut();
            let mut pitch: i32 = 0;
            // SAFETY: 两个输出指针都指向本栈帧；`buffer_2d` 在解锁前一直有效。
            unsafe { buffer_2d.Lock2D(&raw mut scanline, &raw mut pitch) }
                .map_err(|err| decode_err("IMF2DBuffer::Lock2D", &err))?;
            let converted = Self::convert_2d(&self.output, scanline, pitch);
            // SAFETY: 与上面的 Lock2D 配对；无论转换成败都必须解锁。
            let unlocked = unsafe { buffer_2d.Unlock2D() };
            combine_unlock(converted, unlocked, "IMF2DBuffer::Unlock2D")
        } else {
            let mut scanline: *mut u8 = ptr::null_mut();
            // SAFETY: `scanline` 指向本栈帧；`buffer` 在解锁前一直有效。
            unsafe { buffer.Lock(&raw mut scanline, None, None) }
                .map_err(|err| decode_err("IMFMediaBuffer::Lock（输出）", &err))?;
            let pitch = i32::try_from(self.output.stride).unwrap_or(0);
            let converted = Self::convert_2d(&self.output, scanline, pitch);
            // SAFETY: 与上面的 Lock 配对；无论转换成败都必须解锁。
            let unlocked = unsafe { buffer.Unlock() };
            combine_unlock(converted, unlocked, "IMFMediaBuffer::Unlock（输出）")
        }
    }

    /// 从锁定的 2D 缓冲区转换出一帧。
    fn convert_2d(
        output: &OutputFormat,
        scanline: *mut u8,
        pitch: i32,
    ) -> Result<DecodedH264Frame, WinVideoError> {
        if scanline.is_null() {
            return Err(WinVideoError::Decode("解码输出缓冲区为空指针".to_owned()));
        }
        let pitch = usize::try_from(pitch).map_err(|_| {
            WinVideoError::Decode(format!("解码输出 stride 非法（{pitch}，负值不支持）"))
        })?;
        // 缓冲区按**编码尺寸**（16 像素对齐）解释，最后再裁到可见区域。
        let width = output.coded_width;
        let height = output.coded_height;
        let h = usize::try_from(height)
            .map_err(|_| WinVideoError::Decode("解码高度无法转为 usize".to_owned()))?;
        let luma_len = pitch
            .checked_mul(h)
            .ok_or_else(|| WinVideoError::Decode("Y 平面大小溢出".to_owned()))?;
        let chroma_len = pitch
            .checked_mul(h / 2)
            .ok_or_else(|| WinVideoError::Decode("色度平面大小溢出".to_owned()))?;

        // SAFETY: 对每个平面，MFT 的 `Lock2D` 契约保证从 `scanline` 起至少有
        // `pitch * rows` 字节可读；NV12 的 UV 平面紧跟在 Y 平面之后，YV12/IYUV
        // 的两个色度平面依次跟随。切片只用于只读转换，Unlock 前一直有效。
        let y_plane = unsafe { std::slice::from_raw_parts(scanline, luma_len) };
        let mut rgba = Vec::new();
        match output.layout {
            H264OutputFormat::Nv12 => {
                let uv_plane =
                    unsafe { std::slice::from_raw_parts(scanline.add(luma_len), chroma_len) };
                color::nv12_to_rgba(y_plane, uv_plane, pitch, pitch, width, height, &mut rgba)?;
            }
            H264OutputFormat::Yv12 => {
                let plane_stride = pitch / 2;
                let plane_len = plane_stride
                    .checked_mul(h / 2)
                    .ok_or_else(|| WinVideoError::Decode("色度平面大小溢出".to_owned()))?;
                // YV12：V 平面在前，U 平面在后。
                let v_plane =
                    unsafe { std::slice::from_raw_parts(scanline.add(luma_len), plane_len) };
                let u_plane = unsafe {
                    std::slice::from_raw_parts(scanline.add(luma_len + plane_len), plane_len)
                };
                color::planar_to_rgba(
                    y_plane,
                    u_plane,
                    v_plane,
                    pitch,
                    plane_stride,
                    width,
                    height,
                    &mut rgba,
                )?;
            }
            H264OutputFormat::Iyuv => {
                let plane_stride = pitch / 2;
                let plane_len = plane_stride
                    .checked_mul(h / 2)
                    .ok_or_else(|| WinVideoError::Decode("色度平面大小溢出".to_owned()))?;
                // IYUV：U 平面在前，V 平面在后。
                let u_plane =
                    unsafe { std::slice::from_raw_parts(scanline.add(luma_len), plane_len) };
                let v_plane = unsafe {
                    std::slice::from_raw_parts(scanline.add(luma_len + plane_len), plane_len)
                };
                color::planar_to_rgba(
                    y_plane,
                    u_plane,
                    v_plane,
                    pitch,
                    plane_stride,
                    width,
                    height,
                    &mut rgba,
                )?;
            }
        }
        crop_to_display(output, rgba)
    }
}

/// 把转换结果与解锁结果合并：转换失败优先报告转换错误。
fn combine_unlock(
    converted: Result<DecodedH264Frame, WinVideoError>,
    unlocked: windows::core::Result<()>,
    context: &str,
) -> Result<DecodedH264Frame, WinVideoError> {
    match (converted, unlocked) {
        (Ok(frame), Ok(())) => Ok(frame),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(decode_err(context, &err)),
    }
}

/// 创建 MFT、配置输入/输出类型并启动流。
fn create_pipeline(
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(IMFTransform, OutputFormat, bool, u32), WinVideoError> {
    if width == 0 || height == 0 {
        return Err(WinVideoError::Config(format!(
            "解码尺寸不能为 0：{width}x{height}"
        )));
    }
    if fps == 0 {
        return Err(WinVideoError::Config("帧率不能为 0".to_owned()));
    }
    ensure_mf_started()?;
    ensure_com_initialized()?;

    // SAFETY: 进程内 COM 工厂，CLSID 由系统注册；`None` 表示不聚合。
    let transform: IMFTransform = unsafe {
        CoCreateInstance(
            &CLSID_MSH264DecoderMFT,
            None::<&IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
    }
    .map_err(|err| decode_err("CoCreateInstance(CLSID_MSH264DecoderMFT)", &err))?;

    configure_low_latency(&transform);

    let input = build_input_type(width, height, fps)?;
    // SAFETY: `transform` 与 `input` 都有效；输入流 0 是解码器唯一的输入。
    unsafe { transform.SetInputType(0, &input, 0) }
        .map_err(|err| decode_err("SetInputType(H.264)", &err))?;

    let output = negotiate_output_type(&transform, width, height)?;

    // SAFETY: 类型已设置；启动消息只影响 MFT 内部状态。
    unsafe {
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|err| decode_err("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING", &err))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|err| decode_err("MFT_MESSAGE_NOTIFY_START_OF_STREAM", &err))?;
    }

    let (provides_samples, output_buffer_size) = output_stream_info(&transform, &output)?;
    Ok((transform, output, provides_samples, output_buffer_size))
}

/// 尽力打开低延迟模式；不支持时只记 debug，不影响解码。
fn configure_low_latency(transform: &IMFTransform) {
    // SAFETY: `transform` 有效；属性存储生命周期由 COM 引用计数维护。
    let Ok(attributes) = (unsafe { transform.GetAttributes() }) else {
        return;
    };
    // SAFETY: 属性存储有效；`MF_LOW_LATENCY` 的约定类型是 UINT32。
    if let Err(err) = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) } {
        tracing::debug!(%err, "MSH264DecoderMFT 拒绝 MF_LOW_LATENCY，按默认延迟解码");
    }
}

/// 构造 H.264 输入媒体类型。
fn build_input_type(width: u32, height: u32, fps: u32) -> Result<IMFMediaType, WinVideoError> {
    // SAFETY: `MFCreateMediaType` 无外部指针参数。
    let media_type =
        unsafe { MFCreateMediaType() }.map_err(|err| decode_err("MFCreateMediaType", &err))?;
    let progressive = u32::try_from(MFVideoInterlace_Progressive.0).unwrap_or_default();
    // SAFETY: `media_type` 有效；所有 GUID 常数都是 `'static`。
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|err| decode_err("输入类型 MF_MT_MAJOR_TYPE", &err))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|err| decode_err("输入类型 MF_MT_SUBTYPE", &err))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))
            .map_err(|err| decode_err("输入类型 MF_MT_FRAME_SIZE", &err))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, progressive)
            .map_err(|err| decode_err("输入类型 MF_MT_INTERLACE_MODE", &err))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(fps, 1))
            .map_err(|err| decode_err("输入类型 MF_MT_FRAME_RATE", &err))?;
        media_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))
            .map_err(|err| decode_err("输入类型 MF_MT_PIXEL_ASPECT_RATIO", &err))?;
    }
    Ok(media_type)
}

/// 枚举并设置输出类型：优先 NV12，其次 YV12 / IYUV，最后自建 NV12 再试。
fn negotiate_output_type(
    transform: &IMFTransform,
    width: u32,
    height: u32,
) -> Result<OutputFormat, WinVideoError> {
    let mut fallback: Option<(IMFMediaType, H264OutputFormat)> = None;
    for index in 0..MAX_OUTPUT_TYPES {
        // SAFETY: `transform` 有效；索引从 0 递增直到 `MF_E_NO_MORE_TYPES`。
        let candidate = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(candidate) => candidate,
            Err(err) if err.code() == MF_E_NO_MORE_TYPES => break,
            Err(err) => return Err(decode_err("GetOutputAvailableType", &err)),
        };
        let Some(layout) = output_layout(&candidate)? else {
            continue;
        };
        if layout == H264OutputFormat::Nv12 {
            // SAFETY: `candidate` 来自本 MFT 的可用类型列表。
            unsafe { transform.SetOutputType(0, &candidate, 0) }
                .map_err(|err| decode_err("SetOutputType(NV12)", &err))?;
            return Ok(read_output_format(&candidate, width, height, layout));
        }
        if fallback.is_none() {
            fallback = Some((candidate, layout));
        }
    }
    if let Some((candidate, layout)) = fallback {
        // SAFETY: `candidate` 来自本 MFT 的可用类型列表。
        unsafe { transform.SetOutputType(0, &candidate, 0) }
            .map_err(|err| decode_err(&format!("SetOutputType({})", layout.as_str()), &err))?;
        return Ok(read_output_format(&candidate, width, height, layout));
    }

    // 部分驱动在流启动前不枚举输出类型；按请求尺寸自建 NV12 再试一次。
    let media_type = build_output_type(width, height, &MFVideoFormat_NV12)?;
    // SAFETY: `media_type` 有效且与输入尺寸一致。
    unsafe { transform.SetOutputType(0, &media_type, 0) }
        .map_err(|err| decode_err("SetOutputType(自建 NV12)", &err))?;
    Ok(read_output_format(
        &media_type,
        width,
        height,
        H264OutputFormat::Nv12,
    ))
}

/// 读取媒体类型的子类型并映射到支持的输出布局。
fn output_layout(media_type: &IMFMediaType) -> Result<Option<H264OutputFormat>, WinVideoError> {
    // SAFETY: `media_type` 有效。
    let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }
        .map_err(|err| decode_err("输出类型 MF_MT_SUBTYPE", &err))?;
    let layout = if subtype == MFVideoFormat_NV12 {
        Some(H264OutputFormat::Nv12)
    } else if subtype == MFVideoFormat_YV12 {
        Some(H264OutputFormat::Yv12)
    } else if subtype == MFVideoFormat_IYUV {
        Some(H264OutputFormat::Iyuv)
    } else {
        None
    };
    Ok(layout)
}

/// 构造指定子类型的输出媒体类型。
fn build_output_type(
    width: u32,
    height: u32,
    subtype: &windows::core::GUID,
) -> Result<IMFMediaType, WinVideoError> {
    // SAFETY: `MFCreateMediaType` 无外部指针参数。
    let media_type = unsafe { MFCreateMediaType() }
        .map_err(|err| decode_err("MFCreateMediaType（输出）", &err))?;
    let progressive = u32::try_from(MFVideoInterlace_Progressive.0).unwrap_or_default();
    // SAFETY: `media_type` 有效；GUID 常数都是 `'static`。
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|err| decode_err("输出类型 MF_MT_MAJOR_TYPE", &err))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, subtype)
            .map_err(|err| decode_err("输出类型 MF_MT_SUBTYPE", &err))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))
            .map_err(|err| decode_err("输出类型 MF_MT_FRAME_SIZE", &err))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, progressive)
            .map_err(|err| decode_err("输出类型 MF_MT_INTERLACE_MODE", &err))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(DEFAULT_FPS, 1))
            .map_err(|err| decode_err("输出类型 MF_MT_FRAME_RATE", &err))?;
        media_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))
            .map_err(|err| decode_err("输出类型 MF_MT_PIXEL_ASPECT_RATIO", &err))?;
    }
    Ok(media_type)
}

/// 从已设置的输出媒体类型读取实际尺寸/stride。
fn read_output_format(
    media_type: &IMFMediaType,
    fallback_width: u32,
    fallback_height: u32,
    layout: H264OutputFormat,
) -> OutputFormat {
    // SAFETY: `media_type` 有效；`MF_MT_FRAME_SIZE` 的高 32 位是宽、低 32 位是高。
    let (coded_width, coded_height) = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .ok()
        .map(unpack_size)
        .filter(|(width, height)| *width != 0 && *height != 0)
        .unwrap_or((fallback_width, fallback_height));
    // SAFETY: 同上；stride 是可选属性，缺失时退回宽度。
    let hint = unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) }.unwrap_or(0);
    let stride = if hint == 0 { coded_width } else { hint };
    OutputFormat {
        coded_width,
        coded_height,
        display: read_display_rect(media_type, coded_width, coded_height),
        layout,
        stride,
    }
}

/// 读取可见区域（`MF_MT_MINIMUM_DISPLAY_APERTURE`，回退 `MF_MT_GEOMETRIC_APERTURE`）。
///
/// H.264 的编码尺寸按宏块对齐（1080p → 1920x1088、360p → 640x368），解码器
/// 在输出媒体类型上用显示光圈标出真正的可见区域。缺失时退回整个编码帧。
fn read_display_rect(
    media_type: &IMFMediaType,
    coded_width: u32,
    coded_height: u32,
) -> DisplayRect {
    for key in [&MF_MT_MINIMUM_DISPLAY_APERTURE, &MF_MT_GEOMETRIC_APERTURE] {
        if let Some(rect) = read_aperture(media_type, key, coded_width, coded_height) {
            return rect;
        }
    }
    DisplayRect {
        x: 0,
        y: 0,
        width: coded_width,
        height: coded_height,
    }
}

/// 读取并解析一个 `MFVideoArea` 光圈属性。
fn read_aperture(
    media_type: &IMFMediaType,
    key: &windows::core::GUID,
    coded_width: u32,
    coded_height: u32,
) -> Option<DisplayRect> {
    let mut blob = [0u8; std::mem::size_of::<MFVideoArea>()];
    let mut size = 0u32;
    // SAFETY: `media_type` 有效；`blob` 指向本栈帧，`size` 指向本栈帧。
    unsafe { media_type.GetBlob(key, &mut blob, Some(&raw mut size)) }.ok()?;
    if usize::try_from(size).ok()? < blob.len() {
        return None;
    }
    let mut area = MFVideoArea::default();
    // SAFETY: `area` 与 `blob` 都是 POD；长度已校验。u8 数组可能未按 4 字节
    // 对齐，故按字节拷贝到 `MFVideoArea`。
    unsafe {
        ptr::copy_nonoverlapping(blob.as_ptr(), (&raw mut area).cast::<u8>(), blob.len());
    }
    let width = u32::try_from(area.Area.cx).ok()?;
    let height = u32::try_from(area.Area.cy).ok()?;
    if width == 0 || height == 0 || width > coded_width || height > coded_height {
        return None;
    }
    let x = u32::try_from(area.OffsetX.value)
        .unwrap_or(0)
        .min(coded_width.saturating_sub(width));
    let y = u32::try_from(area.OffsetY.value)
        .unwrap_or(0)
        .min(coded_height.saturating_sub(height));
    Some(DisplayRect {
        x,
        y,
        width,
        height,
    })
}

/// 把编码帧（含宏块填充）裁成可见区域。
fn crop_to_display(
    output: &OutputFormat,
    rgba: Vec<u8>,
) -> Result<DecodedH264Frame, WinVideoError> {
    let display = output.display;
    if display.x == 0
        && display.y == 0
        && display.width == output.coded_width
        && display.height == output.coded_height
    {
        return Ok(DecodedH264Frame {
            width: display.width,
            height: display.height,
            rgba,
        });
    }

    let source_row_bytes = usize::try_from(output.coded_width)
        .map_err(|_| WinVideoError::Decode("编码宽度无法转为 usize".to_owned()))?
        .checked_mul(4)
        .ok_or_else(|| WinVideoError::Decode("编码行字节数溢出".to_owned()))?;
    let visible_row_bytes = usize::try_from(display.width)
        .map_err(|_| WinVideoError::Decode("可见宽度无法转为 usize".to_owned()))?
        .checked_mul(4)
        .ok_or_else(|| WinVideoError::Decode("可见行字节数溢出".to_owned()))?;
    let rows = usize::try_from(display.height)
        .map_err(|_| WinVideoError::Decode("可见高度无法转为 usize".to_owned()))?;
    let capacity = visible_row_bytes
        .checked_mul(rows)
        .ok_or_else(|| WinVideoError::Decode("可见区域大小溢出".to_owned()))?;
    let x_offset = usize::try_from(display.x)
        .map_err(|_| WinVideoError::Decode("可见区域 x 偏移无法转为 usize".to_owned()))?
        .checked_mul(4)
        .ok_or_else(|| WinVideoError::Decode("可见区域 x 偏移溢出".to_owned()))?;
    let y_offset = usize::try_from(display.y)
        .map_err(|_| WinVideoError::Decode("可见区域 y 偏移无法转为 usize".to_owned()))?;

    let mut cropped = Vec::with_capacity(capacity);
    for row in 0..rows {
        let start = y_offset
            .checked_add(row)
            .and_then(|absolute| absolute.checked_mul(source_row_bytes))
            .and_then(|base| base.checked_add(x_offset))
            .ok_or_else(|| WinVideoError::Decode("可见区域偏移溢出".to_owned()))?;
        let end = start
            .checked_add(visible_row_bytes)
            .ok_or_else(|| WinVideoError::Decode("可见区域偏移溢出".to_owned()))?;
        let Some(slice) = rgba.get(start..end) else {
            return Err(WinVideoError::Decode("可见区域超出解码缓冲区".to_owned()));
        };
        cropped.extend_from_slice(slice);
    }
    Ok(DecodedH264Frame {
        width: display.width,
        height: display.height,
        rgba: cropped,
    })
}

/// 读取输出流信息，算出「MFT 是否自供样本」与最小缓冲区大小。
fn output_stream_info(
    transform: &IMFTransform,
    output: &OutputFormat,
) -> Result<(bool, u32), WinVideoError> {
    // SAFETY: `transform` 有效且输出流 0 存在。
    let info = unsafe { transform.GetOutputStreamInfo(0) }
        .map_err(|err| decode_err("GetOutputStreamInfo", &err))?;
    let provides_flag = u32::try_from(MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0).unwrap_or_default();
    let provides_samples = info.dwFlags & provides_flag != 0;
    let fallback = minimum_output_size(output)?;
    Ok((provides_samples, info.cbSize.max(fallback)))
}

/// 输出缓冲区的保守下限：4:2:0 一帧 = `width * height * 3 / 2`，再加余量。
fn minimum_output_size(output: &OutputFormat) -> Result<u32, WinVideoError> {
    let pixels = output
        .coded_width
        .checked_mul(output.coded_height)
        .ok_or_else(|| WinVideoError::Decode("输出尺寸溢出".to_owned()))?;
    pixels
        .checked_mul(3)
        .and_then(|value| value.checked_div(2))
        .and_then(|value| value.checked_add(OUTPUT_BUFFER_SLACK))
        .ok_or_else(|| WinVideoError::Decode("输出缓冲区大小溢出".to_owned()))
}

/// `MF_MT_FRAME_SIZE` 的打包方式：高 32 位宽、低 32 位高。
fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

/// `MF_MT_FRAME_SIZE` 的解包。
fn unpack_size(value: u64) -> (u32, u32) {
    (
        u32::try_from(value >> 32).unwrap_or(0),
        u32::try_from(value & 0xFFFF_FFFF).unwrap_or(0),
    )
}

/// `MF_MT_FRAME_RATE` / `MF_MT_PIXEL_ASPECT_RATIO` 的打包：高 32 位分子、低 32 位分母。
fn pack_ratio(numerator: u32, denominator: u32) -> u64 {
    (u64::from(numerator) << 32) | u64::from(denominator)
}

/// 把 HRESULT 包装成带上下文的解码错误。
fn decode_err(context: &str, error: &windows::core::Error) -> WinVideoError {
    WinVideoError::Decode(format!("{context} 失败: {error}"))
}

/// 进程级 `MFStartup`（只执行一次；失败结果也缓存，避免反复尝试）。
fn ensure_mf_started() -> Result<(), WinVideoError> {
    static STARTUP: OnceLock<Result<(), String>> = OnceLock::new();
    match STARTUP.get_or_init(|| {
        // SAFETY: `MFStartup` 是进程级初始化，`MF_VERSION` 是编译期常数。
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|err| err.to_string())
    }) {
        Ok(()) => Ok(()),
        Err(message) => Err(WinVideoError::Decode(format!("MFStartup 失败: {message}"))),
    }
}

thread_local! {
    /// 当前线程是否已完成 COM 初始化（`decode` 可能在线程池线程上运行）。
    static COM_INITIALIZED: Cell<bool> = const { Cell::new(false) };
}

/// 线程级 COM 初始化（MTA）。已初始化或已有其他套间时视为成功。
fn ensure_com_initialized() -> Result<(), WinVideoError> {
    COM_INITIALIZED.with(|initialized| {
        if initialized.get() {
            return Ok(());
        }
        // SAFETY: `CoInitializeEx` 只影响当前线程；`None` 表示使用默认分配器。
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_ok() || result == RPC_E_CHANGED_MODE {
            initialized.set(true);
            Ok(())
        } else {
            Err(WinVideoError::Decode(format!(
                "CoInitializeEx 失败: {result:?}"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_and_ratio_pack_roundtrip() {
        assert_eq!(pack_size(1280, 720), 0x0000_0500_0000_02D0);
        assert_eq!(unpack_size(pack_size(1280, 720)), (1280, 720));
        assert_eq!(pack_ratio(60, 1), 0x0000_003C_0000_0001);
    }

    #[test]
    fn unpack_size_rejects_zero() {
        assert_eq!(unpack_size(0), (0, 0));
    }
}
