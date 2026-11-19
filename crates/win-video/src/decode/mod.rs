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
//! [`MfVideoDecoder`] 只实现 `Send`、**不实现 `Sync`**：它独占持有 COM 的
//! `IMFTransform`，调用方按约定把它移进 `spawn_blocking` 后只由一个线程使用。
//! `decode` 是同步阻塞调用。

mod color;
mod dxva;

pub use dxva::{
    MapMode, ReadbackBreakdown, ReadbackOptions, ReadbackPhase, WarmUp,
    DEFAULT_READBACK_BUDGET_US, READBACK_BUDGET_ENV,
};

use crate::codec::CodecKind;
use crate::WinVideoError;
use std::cell::Cell;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::OnceLock;
use std::time::Instant;
use windows::core::{GUID, IUnknown, Interface as _};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CLSID_MSH265DecoderMFT, IMF2DBuffer, IMFActivate, IMFDXGIBuffer,
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFMediaType_Video, MFVideoArea,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_HEVC_ES, MFVideoFormat_IYUV,
    MFVideoFormat_NV12, MFVideoFormat_YV12, MFVideoInterlace_Progressive, MFSTARTUP_FULL,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING,
    MF_E_NO_MORE_TYPES, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_GEOMETRIC_APERTURE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

/// 未指定帧率时使用的名义帧率（解码本身不依赖它，只用于时间戳与媒体类型）。
const DEFAULT_FPS: u32 = 60;
/// 一秒的 100ns 单位数（Media Foundation 的时间基）。
const HNS_PER_SECOND: i64 = 10_000_000;
/// 枚举输出媒体类型的安全上限（防止驱动异常时死循环）。
const MAX_OUTPUT_TYPES: u32 = 64;
/// 调用方未提供输出样本时，缓冲区的额外余量。
const OUTPUT_BUFFER_SLACK: u32 = 4096;
/// `readback_budget_skip` 日志的限频间隔：每秒最多一条。
const SKIP_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

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

/// 一帧紧凑打包的 NV12（Y 平面 + 交织 UV 平面）。
///
/// 两个平面都是**紧包**的：`y.len() == width * height`、`uv.len() == width * height / 2`，
/// 没有行填充。调用方可以直接把它交给 GPU 上传或 CPU 转码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Frame {
    /// Y 平面，长度 `width * height`。
    pub y: Vec<u8>,
    /// 交织 UV 平面（`U0 V0 U1 V1 ...`），长度 `width * height / 2`。
    pub uv: Vec<u8>,
}

/// 解码器的输出模式。
///
/// [`H264OutputMode::Nv12`] 把 MFT 的 4:2:0 输出原样（只做可见区域裁剪）交给调用方，
/// 省掉解码线程上的 NV12→RGBA 转换；[`H264OutputMode::Rgba`] 保持旧行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum H264OutputMode {
    /// 转换为自顶向下 RGBA8（旧行为）。
    #[default]
    Rgba,
    /// 输出紧包 NV12（可见区域为奇数时自动回退 RGBA，见 [`DecodedH264Frame::nv12`]）。
    Nv12,
}

/// 解码后的一帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedH264Frame {
    /// 可见宽度（已按 `MF_MT_MINIMUM_DISPLAY_APERTURE` 裁掉宏块填充）。
    pub width: u32,
    /// 可见高度（1080p 的 1088 行编码高度会裁成 1080）。
    pub height: u32,
    /// 自顶向下 RGBA8，长度恰为 `width * height * 4`；NV12 模式下为空。
    pub rgba: Vec<u8>,
    /// NV12 模式下的紧包 4:2:0 画面；RGBA 模式（或可见区域为奇数而回退）时为 `None`。
    pub nv12: Option<Nv12Frame>,
}

/// 一次 `decode()` 内部四个子阶段的耗时（微秒）。
///
/// 真机上出现过「空闲 600 ms 后的第一帧解码卡 300–500 ms、而同尺寸的后续帧只要
/// 6 ms」的现象，光看总耗时无法判断卡在 MFT 的哪一段。四段分别对应：
///
/// - [`Self::submit_us`]：把包拷进 `IMFSample` 并 `ProcessInput`（含 MFT 内部输入队列满
///   时的那次重试）；
/// - [`Self::output_us`]：`ProcessOutput` 及其等待（MFT 出帧本身）；
/// - [`Self::readback_us`]：DXVA 的 staging 拷贝 + `Map`（GPU 同步，系统内存路径恒为 0）；
/// - [`Self::color_us`]：NV12 打包或 NV12→RGBA 的像素转换。
///
/// 四者之和近似等于调用方测到的总解码耗时，差额是 COM/日志等零碎开销。各段在同一
/// 次 `decode()` 里**累加**（一批多包时是整批之和）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DecodePhaseTimings {
    /// 送包：`ProcessInput` + 缓冲区拷贝。
    pub submit_us: u64,
    /// MFT 出帧：`drain_output` 里除回读/转换之外的部分（`ProcessOutput` 及其等待）。
    pub output_us: u64,
    /// DXVA 回读：staging 拷贝 + `Map`（系统内存路径恒为 0）。
    pub readback_us: u64,
    /// 像素转换/打包（NV12 紧包或 RGBA）。
    pub color_us: u64,
}

/// 硬件解码偏好。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264HardwarePreference {
    /// 优先 DXVA/D3D11 硬件；初始化任一环节失败时自动回退系统内存软件解码。
    Auto,
    /// 强制系统内存软件解码（探针对照组 / 排障用）。
    Software,
    /// 强制 DXVA/D3D11；初始化失败返回错误，不静默回退（探针用于确认硬件真的生效）。
    RequireHardware,
}

/// 解码器实际采用的数据通路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264DecodePath {
    /// DXVA/D3D11：MFT 输出 `IMFDXGIBuffer` 纹理，CPU 只做 staging 拷贝与 NV12→RGBA。
    Dxva,
    /// 系统内存：MFT 把 NV12 写进调用方提供的缓冲区，CPU 解码 + 转换。
    Software,
}

impl H264DecodePath {
    /// 稳定的短名称，供日志与真机探针输出。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dxva => "DXVA/D3D11",
            Self::Software => "系统内存（软件）",
        }
    }
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
//
// 这些 bool 都是解码器**状态机**的不同维度（是否自供样本、是否真的收到过 DXGI
// 样本、告警是否已打过、本帧是否因回读超预算被放弃），彼此独立、语义各不相同；
// 合并成枚举只会让调用点更难读。
#[allow(
    clippy::struct_excessive_bools,
    reason = "四个独立的状态标志，语义不同且各自单独判读"
)]
pub struct MfVideoDecoder {
    transform: IMFTransform,
    output: OutputFormat,
    requested_width: u32,
    requested_height: u32,
    fps: u32,
    /// 本解码器实例的编码标准。
    kind: CodecKind,
    /// MFT 是否自己分配输出样本（`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`）。
    provides_samples: bool,
    /// 调用方必须提供的输出缓冲区最小字节数。
    output_buffer_size: u32,
    /// 下一帧输入样本的时间戳（100ns）。
    next_pts: i64,
    /// 单帧时长（100ns）。
    frame_duration: i64,
    /// 创建 / 重建时使用的硬件偏好。
    preference: H264HardwarePreference,
    /// 输出模式（RGBA 还是紧包 NV12）。
    output_mode: H264OutputMode,
    /// 当前配置的数据通路（DXVA 是否真正生效另见 [`MfVideoDecoder::hardware_buffers_seen`]）。
    path: H264DecodePath,
    /// DXVA 的 D3D11 设备、设备管理器与 staging 纹理；软件路径为 `None`。
    hardware: Option<dxva::HardwareContext>,
    /// 是否真的收到过 `IMFDXGIBuffer`（配置了硬件但 MFT 仍给系统内存时为 false）。
    dxgi_seen: bool,
    /// 「配置了 DXVA 却没有 DXGI 样本」的告警只打一次。
    warned_no_dxgi: bool,
    /// 最近一次 [`MfVideoDecoder::decode`] 的四个子阶段耗时（微秒）。
    ///
    /// 在每次 `decode()` 开头清零、随解码过程累加；`decode()` 返回 `Err` 时仍是
    /// **本次已经发生**的那部分读数（例如送包成功、出帧失败）。
    last_timings: DecodePhaseTimings,
    /// 最近一次 `decode()` 里 DXVA 回读的三段细分（微秒）。
    ///
    /// 与 [`Self::last_timings`] 同寿命、同清零时机；软件路径恒为零。
    last_readback: dxva::ReadbackBreakdown,
    /// 回读诊断选项（默认 = 生产行为；重建解码器时原样保留）。
    readback_options: ReadbackOptions,
    /// 最近一次 `decode()` 是否因为回读超出预算而**放弃了一帧**。
    last_readback_skipped: bool,
    /// 本进程累计被预算放弃的帧数（只增，供日志/探针读数）。
    readback_skip_count: u64,
    /// 上一条 `readback_budget_skip` 日志的时刻（限频用）。
    last_skip_log: Option<Instant>,
    /// 当前正在解码的访问单元是否含关键帧 slice（决定回读要不要强制阻塞）。
    current_access_unit_has_idr: bool,
}

/// 旧名字：H.264 解码器（就是 [`MfVideoDecoder`]，`new` 默认建 H.264）。
pub type H264Decoder = MfVideoDecoder;

impl std::fmt::Debug for MfVideoDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfVideoDecoder")
            .field("codec", &self.kind)
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
            .field("output_mode", &self.output_mode)
            .field("path", &self.path)
            .field("dxgi_seen", &self.dxgi_seen)
            .finish_non_exhaustive()
    }
}

// SAFETY: `MfVideoDecoder` 独占持有一个进程内 MFT（`MSH264DecoderMFT`）。COM 接口
// 只在持有者线程上调用，结构体**不实现 `Sync`**，所以任何时刻只有一个线程能
// 访问它；`decode`/`reconfigure` 都取 `&mut self`。控制器把解码器移进
// `spawn_blocking` 后不会跨线程共享，满足 COM 的单线程使用约定。
unsafe impl Send for MfVideoDecoder {}

/// 编译期断言：解码器必须能移进 `spawn_blocking`（只要求 `Send`）。
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<MfVideoDecoder>();
};

impl MfVideoDecoder {
    /// 创建一个解码 `width × height` 码流的 H.264 解码器。
    ///
    /// 宽高是**第一帧的预期尺寸**，用于配置输入媒体类型；若码流后续因
    /// `ScreenConfig` 变化而换分辨率，调用 [`MfVideoDecoder::reconfigure`] 重建，
    /// 或依赖 MFT 的 `MF_E_TRANSFORM_STREAM_CHANGE` 自动重协商输出类型。
    ///
    /// # Errors
    /// 尺寸为 0、Media Foundation 初始化失败、`MSH264DecoderMFT` 不可用
    /// （非 Windows 或系统组件缺失）或媒体类型被拒绝时返回 [`WinVideoError`]。
    pub fn new(width: u32, height: u32) -> Result<Self, WinVideoError> {
        Self::new_with_preference(width, height, H264HardwarePreference::Auto)
    }

    /// 强制系统内存软件解码（探针对照组）。
    ///
    /// # Errors
    /// 尺寸非法或 Media Foundation 初始化失败时返回 [`WinVideoError`]。
    pub fn new_software(width: u32, height: u32) -> Result<Self, WinVideoError> {
        Self::new_with_preference(width, height, H264HardwarePreference::Software)
    }

    /// 强制 DXVA/D3D11 硬件解码；硬件不可用时返回错误而不是静默回退。
    ///
    /// # Errors
    /// 尺寸非法、没有可用硬件适配器、设备管理器创建/绑定失败或 MFT 拒绝
    /// D3D 设备管理器时返回 [`WinVideoError`]。
    pub fn new_hardware(width: u32, height: u32) -> Result<Self, WinVideoError> {
        Self::new_with_preference(width, height, H264HardwarePreference::RequireHardware)
    }

    /// 按指定硬件偏好创建解码器。
    ///
    /// # Errors
    /// 尺寸非法、Media Foundation 初始化失败或（`RequireHardware` 下）DXVA
    /// 初始化失败时返回 [`WinVideoError`]。
    pub fn new_with_preference(
        width: u32,
        height: u32,
        preference: H264HardwarePreference,
    ) -> Result<Self, WinVideoError> {
        Self::new_with_preference_and_output(width, height, preference, H264OutputMode::Rgba)
    }

    /// 创建输出紧包 NV12 的解码器（Auto：DXVA 优先，失败回退系统内存）。
    ///
    /// # Errors
    /// 同 [`MfVideoDecoder::new`]。
    pub fn new_nv12(width: u32, height: u32) -> Result<Self, WinVideoError> {
        Self::new_with_preference_and_output(
            width,
            height,
            H264HardwarePreference::Auto,
            H264OutputMode::Nv12,
        )
    }

    /// 按指定硬件偏好与输出模式创建解码器。
    ///
    /// # Errors
    /// 尺寸非法、Media Foundation 初始化失败或（`RequireHardware` 下）DXVA
    /// 初始化失败时返回 [`WinVideoError`]。
    pub fn new_with_preference_and_output(
        width: u32,
        height: u32,
        preference: H264HardwarePreference,
        output_mode: H264OutputMode,
    ) -> Result<Self, WinVideoError> {
        Self::new_with_codec_preference_and_output(
            width,
            height,
            CodecKind::H264,
            preference,
            output_mode,
        )
    }

    /// 创建指定编码标准的解码器（Auto 硬件偏好、RGBA 输出）。
    ///
    /// # Errors
    /// 同 [`MfVideoDecoder::new`]（H.265 还要求系统有 `MSH265DecoderMFT`）。
    pub fn new_with_codec(
        width: u32,
        height: u32,
        codec: CodecKind,
    ) -> Result<Self, WinVideoError> {
        Self::new_with_codec_preference_and_output(
            width,
            height,
            codec,
            H264HardwarePreference::Auto,
            H264OutputMode::Rgba,
        )
    }

    /// 创建 H.265/HEVC 解码器（Auto 硬件偏好、RGBA 输出）。
    ///
    /// # Errors
    /// 同 [`MfVideoDecoder::new_with_codec`]。
    pub fn new_hevc(width: u32, height: u32) -> Result<Self, WinVideoError> {
        Self::new_with_codec(width, height, CodecKind::H265)
    }

    /// 创建输出紧包 NV12 的 H.265/HEVC 解码器（Auto：DXVA 优先）。
    ///
    /// # Errors
    /// 同 [`MfVideoDecoder::new_with_codec`]。
    pub fn new_hevc_nv12(width: u32, height: u32) -> Result<Self, WinVideoError> {
        Self::new_with_codec_preference_and_output(
            width,
            height,
            CodecKind::H265,
            H264HardwarePreference::Auto,
            H264OutputMode::Nv12,
        )
    }

    /// 按编码标准 + 硬件偏好 + 输出模式创建解码器（内部统一入口）。
    ///
    /// # Errors
    /// 尺寸非法、Media Foundation 初始化失败、对应标准的解码 MFT 不可用或
    /// （`RequireHardware` 下）DXVA 初始化失败时返回 [`WinVideoError`]。
    pub fn new_with_codec_preference_and_output(
        width: u32,
        height: u32,
        codec: CodecKind,
        preference: H264HardwarePreference,
        output_mode: H264OutputMode,
    ) -> Result<Self, WinVideoError> {
        let pipeline = create_pipeline(codec, width, height, DEFAULT_FPS, preference)?;
        let readback_options = ReadbackOptions::production();
        let mut decoder = Self {
            transform: pipeline.transform,
            output: pipeline.output,
            requested_width: width,
            requested_height: height,
            fps: DEFAULT_FPS,
            kind: codec,
            provides_samples: pipeline.provides_samples,
            output_buffer_size: pipeline.output_buffer_size,
            next_pts: 0,
            frame_duration: HNS_PER_SECOND / i64::from(DEFAULT_FPS),
            preference,
            output_mode,
            path: pipeline.path,
            hardware: pipeline.hardware,
            dxgi_seen: false,
            warned_no_dxgi: false,
            last_timings: DecodePhaseTimings::default(),
            last_readback: dxva::ReadbackBreakdown::default(),
            readback_options,
            last_readback_skipped: false,
            readback_skip_count: 0,
            last_skip_log: None,
            current_access_unit_has_idr: false,
        };
        // 把（可能来自环境变量的）回读预算交给硬件上下文；软件路径下是空操作。
        if let Some(hardware) = decoder.hardware.as_mut() {
            hardware.set_options(readback_options);
        }
        Ok(decoder)
    }

    /// 当前配置的输出模式。
    #[must_use]
    pub fn output_mode(&self) -> H264OutputMode {
        self.output_mode
    }

    /// 本解码器的编码标准。
    #[must_use]
    pub fn codec(&self) -> CodecKind {
        self.kind
    }

    /// 当前配置的数据通路。
    ///
    /// `Dxva` 只说明设备管理器已成功交给 MFT；是否真的输出 DXGI 纹理要看
    /// [`MfVideoDecoder::hardware_buffers_seen`]。
    #[must_use]
    pub fn path(&self) -> H264DecodePath {
        self.path
    }

    /// DXVA 路径实际绑定的 D3D11 适配器名；软件路径为 `None`。
    #[must_use]
    pub fn adapter_name(&self) -> Option<&str> {
        self.hardware
            .as_ref()
            .map(dxva::HardwareContext::adapter_name)
    }

    /// 解码过程中是否真的收到过 `IMFDXGIBuffer`（硬件解码生效的证据）。
    #[must_use]
    pub fn hardware_buffers_seen(&self) -> bool {
        self.dxgi_seen
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

    /// 最近一帧 DXGI staging 回读的耗时，单位毫秒（软件路径恒为 0）。
    ///
    /// 覆盖 `IMFDXGIBuffer`→staging 纹理的 `CopySubresourceRegion` 与 `Map`；
    /// 只用于诊断，在 [`MfVideoDecoder::decode`] 返回后读取。由微秒读数向下取整派生
    /// （一次 `decode` 处理多包时是该批之和），需要更细的粒度用
    /// [`MfVideoDecoder::last_phase_timings`]。
    #[must_use]
    pub fn last_readback_ms(&self) -> u32 {
        u32::try_from(self.last_timings.readback_us / 1000).unwrap_or(u32::MAX)
    }

    /// 最近一帧像素转换（RGBA 模式）或 NV12 打包（NV12 模式）的耗时，单位毫秒。
    ///
    /// 只用于诊断，在 [`MfVideoDecoder::decode`] 返回后读取；同样由微秒读数派生。
    #[must_use]
    pub fn last_color_ms(&self) -> u32 {
        u32::try_from(self.last_timings.color_us / 1000).unwrap_or(u32::MAX)
    }

    /// 最近一次 [`MfVideoDecoder::decode`] 的四个子阶段耗时（微秒）。
    ///
    /// 用来回答「空闲后的第一帧到底卡在哪一段」：`output_us` 大说明卡在 MFT 出帧，
    /// `readback_us` 大说明卡在 GPU 同步，`color_us` 大说明是 CPU 像素转换。
    /// `decode()` 返回 `Err` 时读数仍是本次已发生的部分耗时（开头会先清零）。
    #[must_use]
    pub fn last_phase_timings(&self) -> DecodePhaseTimings {
        self.last_timings
    }

    /// 最近一次 [`MfVideoDecoder::decode`] 里 DXVA 回读的**三段细分**（微秒）。
    ///
    /// 只回答一个排障问题：`readback_us` 很大时，究竟是
    /// `IMFDXGIBuffer::GetResource`、`CopySubresourceRegion` 还是 `Map`
    /// （等待 GPU 完成）在等。软件路径恒为全 0。与 [`Self::last_phase_timings`]
    /// 同步清零/累加，两者互不影响（`DecodePhaseTimings` 的字段语义不变）。
    #[must_use]
    pub fn last_readback_breakdown(&self) -> ReadbackBreakdown {
        self.last_readback
    }

    /// 最近一次 [`MfVideoDecoder::decode`] 是否因**回读超出预算**而放弃了一帧。
    ///
    /// 真机上 GPU 空闲后的一次 staging 回读可能卡 400+ ms；解码器按
    /// [`ReadbackOptions::readback_budget_us`]（默认
    /// [`DEFAULT_READBACK_BUDGET_US`]，可由环境变量
    /// [`READBACK_BUDGET_ENV`] 覆盖，`0` = 不限）有界等待，超时就把这一帧丢掉、
    /// 让 `decode()` 返回 `Ok(None)`，而不是把调用方卡满半秒。
    /// 丢弃**不影响** H.264 参考链与解码器实例（与「解码错误」不是一回事）。
    #[must_use]
    pub fn last_readback_skipped(&self) -> bool {
        self.last_readback_skipped
    }

    /// 本进程累计因回读超预算被放弃的帧数（只增）。
    ///
    /// 现场日志里对应 [`MfVideoDecoder::decode`] 打的 `readback_budget_skip`（限频 1 条/秒），
    /// 这里给探针/单测一个精确计数。
    #[must_use]
    pub fn readback_skip_count(&self) -> u64 {
        self.readback_skip_count
    }

    /// 按 [`SKIP_LOG_INTERVAL`] 限频地记一条跳过日志（字段全部内联：GUI 日志层会
    /// 丢弃 tracing 的字段）。
    fn log_skip_if_due(&mut self, map_us: u64) {
        let now = Instant::now();
        if !skip_log_due(self.last_skip_log, now) {
            return;
        }
        self.last_skip_log = Some(now);
        tracing::warn!(
            "readback_budget_skip count={} budget_ms={} map_us={}",
            self.readback_skip_count,
            self.readback_options.readback_budget_us / 1000,
            map_us,
        );
    }

    /// 设置 DXVA 回读的选项（生产默认见 [`ReadbackOptions::production`]）。
    ///
    /// 生产默认 = 原有行为 + 100 ms 回读预算；其余字段（Flush / 显式事件等待 /
    /// 重建 staging / 拷前预热）只给真机探针做 A/B 用，默认全关。软件路径下无效果。
    pub fn set_readback_options(&mut self, options: ReadbackOptions) {
        self.readback_options = options;
        if let Some(hardware) = self.hardware.as_mut() {
            hardware.set_options(options);
        }
    }

    /// 当前 DXVA 回读诊断选项。
    #[must_use]
    pub fn readback_options(&self) -> ReadbackOptions {
        self.readback_options
    }

    /// 重建解码器以适配新的编码尺寸（`ScreenConfig` 变化时调用）。
    ///
    /// 重建会丢弃当前参考帧，下一帧必须是 IDR；这与 remote 的分辨率变更规则
    /// （重建编码器 + 强制 IDR）一致。
    ///
    /// # Errors
    /// 新尺寸非法或 MFT 创建/媒体类型协商失败时返回 [`WinVideoError`]。
    pub fn reconfigure(&mut self, width: u32, height: u32) -> Result<(), WinVideoError> {
        let pipeline = create_pipeline(self.kind, width, height, self.fps, self.preference)?;
        self.transform = pipeline.transform;
        self.output = pipeline.output;
        self.requested_width = width;
        self.requested_height = height;
        self.provides_samples = pipeline.provides_samples;
        self.output_buffer_size = pipeline.output_buffer_size;
        self.path = pipeline.path;
        self.hardware = pipeline.hardware;
        self.next_pts = 0;
        self.dxgi_seen = false;
        self.warned_no_dxgi = false;
        self.last_timings = DecodePhaseTimings::default();
        self.last_readback = dxva::ReadbackBreakdown::default();
        self.last_readback_skipped = false;
        self.current_access_unit_has_idr = false;
        if let Some(hardware) = self.hardware.as_mut() {
            hardware.set_options(self.readback_options);
        }
        Ok(())
    }

    /// 解码一批（4 字节大端长度前缀 + Annex B，可拼接多个包）码流。
    ///
    /// 返回本次调用产生的**最后一帧**；MFT 缓冲了输入、暂时没有输出时返回
    /// `None`。空输入返回 `None`。
    ///
    /// # `Ok(None)` 的两种含义（都不代表解流出错）
    ///
    /// 1. **MFT 还在缓冲输入**：这次送进去的包还没解出画面（硬件编码器/解码器都有
    ///    一帧流水线延迟）。下一批到达时自然会出帧。
    /// 2. **回读超预算，这一帧被主动放弃**（见 [`MfVideoDecoder::last_readback_skipped`]
    ///    与 [`ReadbackOptions::readback_budget_us`]）。
    ///
    /// 两种情况下**解码器实例与 H.264 参考链都完好**：第 2 种是「解码已完成、只是
    /// 没把像素读回 CPU（也没有解映射）」，解码器内部状态与后续 P 帧的参考关系不受
    /// 任何影响，**不需要**重建解码器、**不需要**请求关键帧。调用方（remote 控制器）
    /// 把 `Ok(None)` 当成「本批无输出」是正确语义；用
    /// [`MfVideoDecoder::last_readback_skipped`] / [`MfVideoDecoder::readback_skip_count`]
    /// 才能把两者区分开。
    ///
    /// **跳过是一次性的、量级很小**：本机 250 ms 节奏实测「卡 → 卡 → 2.8 ms 成功」，
    /// 即一次空闲事件通常丢 2 帧；节奏越密丢得越多，上界约等于
    /// 停顿时长（~0.45 s）÷ 到达间隔。含 **IDR** 的访问单元**永不跳过**（补丁模式
    /// 的基准帧必须是这一张），见 `dxva::readback_policy`。
    ///
    /// 子阶段读数（[`MfVideoDecoder::last_phase_timings`]）在**本次调用开头清零**，
    /// 各段在批内累加；早退（空输入）后读数为全 0，出错返回时保留已发生的部分。
    ///
    /// # Errors
    /// 长度前缀不完整/越界、空包、媒体类型或 MFT 处理失败时返回
    /// [`WinVideoError`]。
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedH264Frame>, WinVideoError> {
        // 读数按「本次调用」计：上一批的耗时不能混进来。
        self.last_timings = DecodePhaseTimings::default();
        self.last_readback = dxva::ReadbackBreakdown::default();
        self.last_readback_skipped = false;
        self.current_access_unit_has_idr = false;
        if data.is_empty() {
            return Ok(None);
        }
        // 解码任务可能在线程池线程上运行：COM 初始化是线程局部的，必须每次确认。
        ensure_com_initialized()?;
        ensure_mf_started()?;

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
            let submit_started = Instant::now();
            // 关键帧判定必须在送包之前完成：`submit` 在 `MF_E_NOTACCEPTING` 时会顺手
            // 排空输出，那一次 `convert_sample` 也要看到这个标志。
            self.current_access_unit_has_idr = access_unit_has_idr(payload, self.kind);
            let submitted = self.submit(payload);
            // 失败也要记：读数的语义是「本次已经发生的耗时」，错误路径同样要看得出卡在哪。
            self.last_timings.submit_us = self
                .last_timings
                .submit_us
                .saturating_add(elapsed_us(submit_started));
            submitted?;
            if let Some(frame) = self.drain_output()? {
                last = Some(frame);
            }
            self.current_access_unit_has_idr = false;
            offset = end;
        }
        Ok(last)
    }

    /// 把一个访问单元送进 MFT。
    fn submit(&mut self, payload: &[u8]) -> Result<(), WinVideoError> {
        let length = u32::try_from(payload.len())
            .map_err(|_| WinVideoError::Decode("单个包超过 4 GiB".to_owned()))?;

        let buffer = create_memory_buffer(length)?;

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

        let sample = create_sample()?;
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
    ///
    /// 同时累计出帧（`output_us`）读数：总耗时减去本函数内测到的回读/转换，
    /// 剩下的就是 `ProcessOutput` 及其等待。`submit` 在 `MF_E_NOTACCEPTING`
    /// 时的重试也走这里，所以那段排空同样计入。
    fn drain_output(&mut self) -> Result<Option<DecodedH264Frame>, WinVideoError> {
        let started = Instant::now();
        let readback_before = self.last_timings.readback_us;
        let color_before = self.last_timings.color_us;
        let result = self.drain_output_inner();
        let total_us = elapsed_us(started);
        let measured_us = self
            .last_timings
            .readback_us
            .saturating_sub(readback_before)
            .saturating_add(self.last_timings.color_us.saturating_sub(color_before));
        // 饱和减法：时钟粒度较粗时子段之和可能略超总耗时，宁可记 0 也不回绕。
        self.last_timings.output_us = self
            .last_timings
            .output_us
            .saturating_add(total_us.saturating_sub(measured_us));
        result
    }

    /// [`MfVideoDecoder::drain_output`] 的本体（计量在调用方）。
    fn drain_output_inner(&mut self) -> Result<Option<DecodedH264Frame>, WinVideoError> {
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
                    // `None` = 回读超预算、这一帧被放弃（不是错误，也不影响参考链）。
                    if let Some(frame) = self.convert_sample(&sample)? {
                        last = Some(frame);
                    }
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
        let buffer = create_memory_buffer(self.output_buffer_size.max(1))?;
        let sample = create_sample()?;
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

    /// 把一个输出样本转成配置的输出模式（DXGI 纹理优先，其次系统内存）。
    ///
    /// 回读/转换两段在这里测、**累加**进 [`DecodePhaseTimings`]（一次 `decode()` 可能
    /// 产出多帧）；清零由 [`MfVideoDecoder::decode`] 在开头统一做。
    fn convert_sample(
        &mut self,
        sample: &IMFSample,
    ) -> Result<Option<DecodedH264Frame>, WinVideoError> {
        let mode = self.output_mode;

        // SAFETY: `sample` 有效；返回的缓冲区在作用域内保持引用计数。
        let buffer: IMFMediaBuffer = unsafe { sample.GetBufferByIndex(0) }
            .map_err(|err| decode_err("IMFSample::GetBufferByIndex", &err))?;

        // DXVA 路径：解码器输出 IMFDXGIBuffer（纹理 + subresource），必须拷到
        // staging 纹理再 Map 读取；不能走 IMF2DBuffer（跨设备纹理没有 CPU 指针）。
        if let Ok(dxgi_buffer) = buffer.cast::<IMFDXGIBuffer>() {
            if let Some(hardware) = self.hardware.as_mut() {
                let output = self.output;
                let mut color_us = 0u64;
                let readback_started = Instant::now();
                let mut breakdown = dxva::ReadbackBreakdown::default();
                let force_block = self.current_access_unit_has_idr;
                let converted = hardware.with_mapped(
                    output.coded_width,
                    output.coded_height,
                    &dxgi_buffer,
                    &mut breakdown,
                    force_block,
                    |scanline, pitch| {
                        // 闭包只做像素转换/打包：`with_mapped` 在调用它之前已完成
                        // staging 拷贝与 Map，故“总耗时 - 本段”就是回读耗时。
                        let color_started = Instant::now();
                        let converted = convert_frame(&output, mode, scanline, pitch);
                        color_us = elapsed_us(color_started);
                        converted
                    },
                );
                // 两次独立测量的差值即回读（staging 拷贝 + Map）耗时。
                let readback_us = elapsed_us(readback_started).saturating_sub(color_us);
                self.last_timings.readback_us =
                    self.last_timings.readback_us.saturating_add(readback_us);
                self.last_timings.color_us = self.last_timings.color_us.saturating_add(color_us);
                self.last_readback.accumulate(breakdown);
                self.dxgi_seen = true;
                if matches!(converted, Ok(None)) {
                    // 回读超预算：这一帧被放弃，解码器本身与参考链都不受影响。
                    self.last_readback_skipped = true;
                    self.readback_skip_count = self.readback_skip_count.saturating_add(1);
                    self.log_skip_if_due(breakdown.map_us);
                }
                return converted;
            }
        }
        if self.hardware.is_some() && !self.warned_no_dxgi {
            self.warned_no_dxgi = true;
            tracing::warn!(
                "已配置 DXVA，但 MFT 输出的是系统内存样本；硬件解码未生效（CPU 解码中）"
            );
        }

        // 系统内存路径。
        // SAFETY: `sample` 有效；`ConvertToContiguousBuffer` 失败时退回第一个缓冲区
        // （部分 MFT 只给 2D 缓冲）。
        let contiguous: IMFMediaBuffer = match unsafe { sample.ConvertToContiguousBuffer() } {
            Ok(contiguous) => contiguous,
            Err(_) => buffer,
        };

        if let Ok(buffer_2d) = contiguous.cast::<IMF2DBuffer>() {
            let mut scanline: *mut u8 = ptr::null_mut();
            let mut pitch: i32 = 0;
            // SAFETY: 两个输出指针都指向本栈帧；`buffer_2d` 在解锁前一直有效。
            unsafe { buffer_2d.Lock2D(&raw mut scanline, &raw mut pitch) }
                .map_err(|err| decode_err("IMF2DBuffer::Lock2D", &err))?;
            let color_started = Instant::now();
            let converted = convert_system_memory(&self.output, mode, scanline, pitch);
            self.last_timings.color_us = self
                .last_timings
                .color_us
                .saturating_add(elapsed_us(color_started));
            // SAFETY: 与上面的 Lock2D 配对；无论转换成败都必须解锁。
            let unlocked = unsafe { buffer_2d.Unlock2D() };
            combine_unlock(converted, unlocked, "IMF2DBuffer::Unlock2D").map(Some)
        } else {
            let mut scanline: *mut u8 = ptr::null_mut();
            // SAFETY: `scanline` 指向本栈帧；`contiguous` 在解锁前一直有效。
            unsafe { contiguous.Lock(&raw mut scanline, None, None) }
                .map_err(|err| decode_err("IMFMediaBuffer::Lock（输出）", &err))?;
            let pitch = i32::try_from(self.output.stride).unwrap_or(0);
            let color_started = Instant::now();
            let converted = convert_system_memory(&self.output, mode, scanline, pitch);
            self.last_timings.color_us = self
                .last_timings
                .color_us
                .saturating_add(elapsed_us(color_started));
            // SAFETY: 与上面的 Lock 配对；无论转换成败都必须解锁。
            let unlocked = unsafe { contiguous.Unlock() };
            combine_unlock(converted, unlocked, "IMFMediaBuffer::Unlock（输出）").map(Some)
        }
    }
}

/// 系统内存输出：把 `i32` stride 校验后交给通用转换。
fn convert_system_memory(
    output: &OutputFormat,
    mode: H264OutputMode,
    scanline: *mut u8,
    pitch: i32,
) -> Result<DecodedH264Frame, WinVideoError> {
    let pitch = usize::try_from(pitch).map_err(|_| {
        WinVideoError::Decode(format!("解码输出 stride 非法（{pitch}，负值不支持）"))
    })?;
    convert_frame(output, mode, scanline, pitch)
}

/// 从锁定的缓冲区切出的平面视图（只读，`Unlock2D` / `Unlock` 前有效）。
struct PlaneViews<'a> {
    /// Y 平面。
    y: &'a [u8],
    /// 色度平面（布局随协商到的输出格式而定）。
    chroma: ChromaPlanes<'a>,
}

/// 4:2:0 色度平面的两种内存布局。
#[derive(Debug, Clone, Copy)]
enum ChromaPlanes<'a> {
    /// NV12：UV 交织（`U0 V0 U1 V1 ...`），`pitch` 是**字节** stride。
    Interleaved {
        /// 交织 UV 平面。
        uv: &'a [u8],
        /// 字节 stride。
        pitch: usize,
    },
    /// YV12 / IYUV：U/V 各自独立，`pitch` 是**字节** stride。
    Planar {
        /// U 平面。
        u: &'a [u8],
        /// V 平面。
        v: &'a [u8],
        /// 字节 stride。
        pitch: usize,
    },
}

/// 把锁定的 MFT 输出缓冲区解释成平面视图。
///
/// # Safety
/// `scanline` 必须由 MFT 的 `Lock2D` / `Lock` 返回且非空；调用方必须保证从它起
/// 按编码尺寸解释至少有 `pitch * coded_height` 字节可读（NV12 的 UV 平面与
/// YV12/IYUV 的两个色度平面紧随 Y 平面），并在 `Unlock2D` / `Unlock` 之前保持
/// 有效。返回的切片只用于只读转换。
fn plane_views<'a>(
    output: &OutputFormat,
    scanline: *const u8,
    pitch: usize,
) -> Result<PlaneViews<'a>, WinVideoError> {
    if scanline.is_null() {
        return Err(WinVideoError::Decode("解码输出缓冲区为空指针".to_owned()));
    }
    // 缓冲区按**编码尺寸**（16 像素对齐）解释，最后再裁到可见区域。
    let h = usize::try_from(output.coded_height)
        .map_err(|_| WinVideoError::Decode("解码高度无法转为 usize".to_owned()))?;
    let luma_len = pitch
        .checked_mul(h)
        .ok_or_else(|| WinVideoError::Decode("Y 平面大小溢出".to_owned()))?;
    let chroma_len = pitch
        .checked_mul(h / 2)
        .ok_or_else(|| WinVideoError::Decode("色度平面大小溢出".to_owned()))?;

    // SAFETY: 由本函数的契约保证：`scanline` 起至少有 `pitch * coded_height`
    // 字节可读，各平面按 MFT 的 4:2:0 布局依次排布。
    let y = unsafe { std::slice::from_raw_parts(scanline, luma_len) };
    let chroma = match output.layout {
        H264OutputFormat::Nv12 => {
            // SAFETY: UV 平面紧跟 Y 平面，长度 `pitch * (coded_height / 2)`。
            let uv = unsafe { std::slice::from_raw_parts(scanline.add(luma_len), chroma_len) };
            ChromaPlanes::Interleaved { uv, pitch }
        }
        H264OutputFormat::Yv12 => {
            let plane_stride = pitch / 2;
            let plane_len = plane_stride
                .checked_mul(h / 2)
                .ok_or_else(|| WinVideoError::Decode("色度平面大小溢出".to_owned()))?;
            // YV12：V 平面在前，U 平面在后。
            // SAFETY: 两个色度平面依次跟在 Y 平面之后。
            let v = unsafe { std::slice::from_raw_parts(scanline.add(luma_len), plane_len) };
            let u = unsafe {
                std::slice::from_raw_parts(scanline.add(luma_len + plane_len), plane_len)
            };
            ChromaPlanes::Planar {
                u,
                v,
                pitch: plane_stride,
            }
        }
        H264OutputFormat::Iyuv => {
            let plane_stride = pitch / 2;
            let plane_len = plane_stride
                .checked_mul(h / 2)
                .ok_or_else(|| WinVideoError::Decode("色度平面大小溢出".to_owned()))?;
            // IYUV：U 平面在前，V 平面在后。
            // SAFETY: 同 YV12。
            let u = unsafe { std::slice::from_raw_parts(scanline.add(luma_len), plane_len) };
            let v = unsafe {
                std::slice::from_raw_parts(scanline.add(luma_len + plane_len), plane_len)
            };
            ChromaPlanes::Planar {
                u,
                v,
                pitch: plane_stride,
            }
        }
    };
    Ok(PlaneViews { y, chroma })
}

/// 判断可见区域能否直接打包成紧包 NV12（4:2:0 要求全偶数且不越界）。
fn nv12_crop_supported(display: DisplayRect, coded_width: u32, coded_height: u32) -> bool {
    let even = |value: u32| value.is_multiple_of(2);
    display.width != 0
        && display.height != 0
        && even(display.x)
        && even(display.y)
        && even(display.width)
        && even(display.height)
        && display
            .x
            .checked_add(display.width)
            .is_some_and(|end| end <= coded_width)
        && display
            .y
            .checked_add(display.height)
            .is_some_and(|end| end <= coded_height)
}

/// 按输出模式转换（或打包）一帧。
fn convert_frame(
    output: &OutputFormat,
    mode: H264OutputMode,
    scanline: *const u8,
    pitch: usize,
) -> Result<DecodedH264Frame, WinVideoError> {
    match mode {
        H264OutputMode::Rgba => convert_planes(output, scanline, pitch),
        H264OutputMode::Nv12 => {
            // 4:2:0 的紧包布局要求可见区域全偶数；奇数光圈（理论上不该出现）
            // 直接回退 RGBA，而不是把整帧丢掉。
            if !nv12_crop_supported(output.display, output.coded_width, output.coded_height) {
                return convert_planes(output, scanline, pitch);
            }
            let views = plane_views(output, scanline, pitch)?;
            let nv12 = pack_nv12(views.y, pitch, views.chroma, output.display)?;
            Ok(DecodedH264Frame {
                width: output.display.width,
                height: output.display.height,
                rgba: Vec::new(),
                nv12: Some(nv12),
            })
        }
    }
}

/// 把三个平面里的可见区域**紧包**成 NV12（纯函数，便于单测）。
///
/// `display` 必须是编码帧内的偶数矩形（由 [`nv12_crop_supported`] 预判）；
/// 不满足时返回 [`WinVideoError::Decode`]，而不是产生半帧数据。
fn pack_nv12(
    y: &[u8],
    y_pitch: usize,
    chroma: ChromaPlanes<'_>,
    display: DisplayRect,
) -> Result<Nv12Frame, WinVideoError> {
    let even = |value: u32| value.is_multiple_of(2);
    if display.width == 0
        || display.height == 0
        || !even(display.x)
        || !even(display.y)
        || !even(display.width)
        || !even(display.height)
    {
        return Err(WinVideoError::Decode(
            "NV12 打包要求可见区域为非零偶数".to_owned(),
        ));
    }
    let x = usize::try_from(display.x)
        .map_err(|_| WinVideoError::Decode("可见区域 x 无法转为 usize".to_owned()))?;
    let top = usize::try_from(display.y)
        .map_err(|_| WinVideoError::Decode("可见区域 y 无法转为 usize".to_owned()))?;
    let width = usize::try_from(display.width)
        .map_err(|_| WinVideoError::Decode("可见宽度无法转为 usize".to_owned()))?;
    let height = usize::try_from(display.height)
        .map_err(|_| WinVideoError::Decode("可见高度无法转为 usize".to_owned()))?;
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| WinVideoError::Decode("可见区域大小溢出".to_owned()))?;

    let mut out_y = Vec::with_capacity(pixels);
    for row in 0..height {
        let start = top
            .checked_add(row)
            .and_then(|absolute| absolute.checked_mul(y_pitch))
            .and_then(|base| base.checked_add(x))
            .ok_or_else(|| WinVideoError::Decode("Y 平面偏移溢出".to_owned()))?;
        let end = start
            .checked_add(width)
            .ok_or_else(|| WinVideoError::Decode("Y 平面偏移溢出".to_owned()))?;
        let Some(line) = y.get(start..end) else {
            return Err(WinVideoError::Decode("可见区域超出 Y 平面".to_owned()));
        };
        out_y.extend_from_slice(line);
    }

    let chroma_height = height / 2;
    let mut out_uv = Vec::with_capacity(pixels / 2);
    match chroma {
        ChromaPlanes::Interleaved { uv, pitch } => {
            // NV12 的 UV 交织布局里每个 luma 像素对应 1 字节，故 x 偏移就是
            // `display.x`（偶数 ⇒ 恰好落在 U 字节上）。
            for row in 0..chroma_height {
                let start = (top / 2)
                    .checked_add(row)
                    .and_then(|absolute| absolute.checked_mul(pitch))
                    .and_then(|base| base.checked_add(x))
                    .ok_or_else(|| WinVideoError::Decode("UV 平面偏移溢出".to_owned()))?;
                let end = start
                    .checked_add(width)
                    .ok_or_else(|| WinVideoError::Decode("UV 平面偏移溢出".to_owned()))?;
                let Some(line) = uv.get(start..end) else {
                    return Err(WinVideoError::Decode("可见区域超出 UV 平面".to_owned()));
                };
                out_uv.extend_from_slice(line);
            }
        }
        ChromaPlanes::Planar { u, v, pitch } => {
            let chroma_x = x / 2;
            let chroma_width = width / 2;
            for row in 0..chroma_height {
                let start = (top / 2)
                    .checked_add(row)
                    .and_then(|absolute| absolute.checked_mul(pitch))
                    .and_then(|base| base.checked_add(chroma_x))
                    .ok_or_else(|| WinVideoError::Decode("色度平面偏移溢出".to_owned()))?;
                let end = start
                    .checked_add(chroma_width)
                    .ok_or_else(|| WinVideoError::Decode("色度平面偏移溢出".to_owned()))?;
                let (Some(u_row), Some(v_row)) = (u.get(start..end), v.get(start..end)) else {
                    return Err(WinVideoError::Decode("可见区域超出色度平面".to_owned()));
                };
                for (&cb, &cr) in u_row.iter().zip(v_row) {
                    out_uv.push(cb);
                    out_uv.push(cr);
                }
            }
        }
    }
    Ok(Nv12Frame {
        y: out_y,
        uv: out_uv,
    })
}

/// 把一帧紧包 NV12 转成自顶向下 RGBA8（CPU 回退路径，供 GUI 使用）。
///
/// `pitch` 按 `width` 处理——这是 [`Nv12Frame`] 的紧包约定。
///
/// # Errors
/// 尺寸为 0/奇数、或平面长度与 `width * height`（UV 为一半）不符时返回
/// [`WinVideoError::Decode`]。
pub fn nv12_to_rgba(
    y: &[u8],
    uv: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, WinVideoError> {
    let pitch = usize::try_from(width)
        .map_err(|_| WinVideoError::Decode("解码宽度无法转为 usize".to_owned()))?;
    let pixels = usize::try_from(height)
        .ok()
        .and_then(|h| pitch.checked_mul(h))
        .ok_or_else(|| WinVideoError::Decode("解码尺寸溢出".to_owned()))?;
    let expected_uv = pixels / 2;
    if y.len() != pixels {
        return Err(WinVideoError::Decode(format!(
            "NV12 Y 平面长度 {} 与 {width}x{height} 不符（应为 {pixels}）",
            y.len()
        )));
    }
    if uv.len() != expected_uv {
        return Err(WinVideoError::Decode(format!(
            "NV12 UV 平面长度 {} 与 {width}x{height} 不符（应为 {expected_uv}）",
            uv.len()
        )));
    }
    let mut rgba = Vec::new();
    color::nv12_to_rgba(y, uv, pitch, pitch, width, height, &mut rgba)?;
    Ok(rgba)
}

/// 从锁定的缓冲区转换出一帧 RGBA。
fn convert_planes(
    output: &OutputFormat,
    scanline: *const u8,
    pitch: usize,
) -> Result<DecodedH264Frame, WinVideoError> {
    let PlaneViews { y: y_plane, chroma } = plane_views(output, scanline, pitch)?;
    let width = output.coded_width;
    let height = output.coded_height;

    let display = output.display;
    let crop = color::CropRect {
        x: display.x,
        y: display.y,
        width: display.width,
        height: display.height,
    };
    let full_frame = display.x == 0
        && display.y == 0
        && display.width == width
        && display.height == height;
    // 编码帧与可见区域一致时走整帧路径（零额外开销）；否则只要 4:2:0 的
    // 偶数对齐与边界都满足，就把裁剪融进颜色转换，省掉整帧再拷一次。
    let fuse_crop = !full_frame && color::crop_is_supported(crop, width, height);

    let mut rgba = Vec::new();
    match chroma {
        ChromaPlanes::Interleaved { uv, pitch: uv_pitch } => {
            if fuse_crop {
                color::nv12_to_rgba_cropped(
                    y_plane, uv, pitch, uv_pitch, width, height, crop, &mut rgba,
                )?;
            } else {
                color::nv12_to_rgba(y_plane, uv, pitch, uv_pitch, width, height, &mut rgba)?;
            }
        }
        ChromaPlanes::Planar {
            u,
            v,
            pitch: chroma_pitch,
        } => {
            if fuse_crop {
                color::planar_to_rgba_cropped(
                    y_plane,
                    u,
                    v,
                    pitch,
                    chroma_pitch,
                    width,
                    height,
                    crop,
                    &mut rgba,
                )?;
            } else {
                color::planar_to_rgba(
                    y_plane,
                    u,
                    v,
                    pitch,
                    chroma_pitch,
                    width,
                    height,
                    &mut rgba,
                )?;
            }
        }
    }
    if fuse_crop {
        // 融合路径的输出已经是可见尺寸，不再走 crop_to_display。
        return Ok(DecodedH264Frame {
            width: display.width,
            height: display.height,
            rgba,
            nv12: None,
        });
    }
    crop_to_display(output, rgba)
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

/// 一条已配置好的解码流水线。
struct Pipeline {
    transform: IMFTransform,
    output: OutputFormat,
    provides_samples: bool,
    output_buffer_size: u32,
    path: H264DecodePath,
    hardware: Option<dxva::HardwareContext>,
}

/// 按偏好创建流水线；`Auto` 在硬件任一步失败时回退软件。
fn create_pipeline(
    codec: CodecKind,
    width: u32,
    height: u32,
    fps: u32,
    preference: H264HardwarePreference,
) -> Result<Pipeline, WinVideoError> {
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

    match preference {
        H264HardwarePreference::Software => create_software_pipeline(codec, width, height, fps),
        H264HardwarePreference::Auto => {
            match create_hardware_pipeline(codec, width, height, fps) {
                Ok(pipeline) => Ok(pipeline),
                Err(err) => {
                    tracing::warn!(
                        codec = codec.label(),
                        error = %err,
                        "DXVA 初始化失败，回退系统内存软件解码"
                    );
                    create_software_pipeline(codec, width, height, fps)
                }
            }
        }
        H264HardwarePreference::RequireHardware => {
            create_hardware_pipeline(codec, width, height, fps)
        }
    }
}

/// 系统内存软件解码流水线。
fn create_software_pipeline(
    codec: CodecKind,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Pipeline, WinVideoError> {
    let transform = create_transform(codec)?;
    let (output, provides_samples, output_buffer_size) =
        configure_types(&transform, codec, width, height, fps)?;
    tracing::info!(
        codec = codec.label(),
        path = H264DecodePath::Software.as_str(),
        "解码流水线已就绪"
    );
    Ok(Pipeline {
        transform,
        output,
        provides_samples,
        output_buffer_size,
        path: H264DecodePath::Software,
        hardware: None,
    })
}

/// DXVA/D3D11 硬件解码流水线。
/// 测试用：强制硬件初始化失败，以验证 `Auto` 的回退分支。
#[cfg(test)]
static FORCE_HARDWARE_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn create_hardware_pipeline(
    codec: CodecKind,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Pipeline, WinVideoError> {
    #[cfg(test)]
    {
        if FORCE_HARDWARE_FAILURE.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(WinVideoError::Decode(
                "测试注入的 DXVA 初始化失败".to_owned(),
            ));
        }
    }
    let device = dxva::create_device()?;
    let transform = create_transform(codec)?;
    // 设备管理器必须在 SetInputType / SetOutputType 之前交给 MFT。
    let hardware = dxva::HardwareContext::new(device, &transform)?;
    let (output, provides_samples, output_buffer_size) =
        configure_types(&transform, codec, width, height, fps)?;
    tracing::info!(
        codec = codec.label(),
        path = H264DecodePath::Dxva.as_str(),
        adapter = hardware.adapter_name(),
        "解码流水线已就绪"
    );
    Ok(Pipeline {
        transform,
        output,
        provides_samples,
        output_buffer_size,
        path: H264DecodePath::Dxva,
        hardware: Some(hardware),
    })
}

/// 创建解码 MFT 并设置低延迟属性。
///
/// H.264 固定用系统自带的 `CLSID_MSH264DecoderMFT`。H.265 先试
/// `CLSID_MSH265DecoderMFT`（HEVC Video Extensions 安装后的稳定注册），失败再
/// `MFTEnumEx` 枚举能接受 HEVC 输入的解码器——不同 Windows 版本/Store 安装方式
/// 的注册点可能不一致。
fn create_transform(codec: CodecKind) -> Result<IMFTransform, WinVideoError> {
    match codec {
        CodecKind::H264 => {
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
            Ok(transform)
        }
        CodecKind::H265 => {
            // SAFETY: 同上；HEVC 解码器由 Microsoft Store 扩展注册。
            let created: windows::core::Result<IMFTransform> = unsafe {
                CoCreateInstance(
                    &CLSID_MSH265DecoderMFT,
                    None::<&IUnknown>,
                    CLSCTX_INPROC_SERVER,
                )
            };
            match created {
                Ok(transform) => {
                    configure_low_latency(&transform);
                    Ok(transform)
                }
                Err(create_err) => {
                    tracing::debug!(
                        error = %create_err,
                        "CLSID_MSH265DecoderMFT 直接创建失败，改按输入 subtype 枚举解码器"
                    );
                    create_hevc_transform_from_enumeration().map_err(|err| {
                        WinVideoError::Decode(format!(
                            "H.265 解码器不可用（CoCreateInstance/CLSID_MSH265DecoderMFT: \
                             {create_err}；枚举: {err}）"
                        ))
                    })
                }
            }
        }
    }
}

/// 枚举能接受 HEVC 输入的解码器 MFT 并激活第一个可用的。
fn create_hevc_transform_from_enumeration() -> Result<IMFTransform, WinVideoError> {
    let mut errors = Vec::new();
    for (subtype, name) in [
        (MFVideoFormat_HEVC_ES, "HEVC_ES"),
        (MFVideoFormat_HEVC, "HEVC"),
    ] {
        match enumerate_decoder_activates(&subtype) {
            Ok(activates) => {
                for activate in &activates {
                    // SAFETY: `activate` 来自 MFTEnumEx，是有效的 IMFActivate。
                    let transform: IMFTransform = match unsafe { activate.ActivateObject() } {
                        Ok(transform) => transform,
                        Err(err) => {
                            errors.push(format!("{name} 激活失败: {err}"));
                            continue;
                        }
                    };
                    configure_low_latency(&transform);
                    return Ok(transform);
                }
                if activates.is_empty() {
                    errors.push(format!("{name}: 没有枚举到解码器"));
                }
            }
            Err(err) => errors.push(format!("{name}: {err}")),
        }
    }
    // 全部失败：把每个 subtype 的错误带出去，工厂探测据此返回 None。
    Err(WinVideoError::Decode(errors.join("；")))
}

/// 按输入 subtype（压缩格式）枚举解码器 MFT，返回激活对象。
fn enumerate_decoder_activates(subtype: &GUID) -> Result<Vec<IMFActivate>, WinVideoError> {
    let api = mf_api()?;
    let enum_ex = api.enum_ex.ok_or_else(|| {
        WinVideoError::Decode("mfplat.dll 未导出 MFTEnumEx（无法枚举 HEVC 解码器）".to_owned())
    })?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *subtype,
    };
    let flags = u32::try_from(MFT_ENUM_FLAG_ALL.0 | MFT_ENUM_FLAG_SORTANDFILTER.0).unwrap_or(0);
    let mut array: *mut *mut c_void = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: `input` 指向本栈帧；输出 subtype 传 NULL 表示不限；成功后
    // `array` 是 CoTaskMemAlloc 分配的 `IMFActivate*` 数组。
    let result = unsafe {
        enum_ex(
            MFT_CATEGORY_VIDEO_DECODER,
            flags,
            &raw const input,
            ptr::null(),
            &raw mut array,
            &raw mut count,
        )
    };
    result
        .ok()
        .map_err(|err| WinVideoError::Decode(format!("MFTEnumEx(解码器) 失败: {err}")))?;

    let mut activates = Vec::new();
    if !array.is_null() {
        let len = usize::try_from(count).unwrap_or(0);
        // SAFETY: `MFTEnumEx` 承诺 `array` 指向 `count` 个接口指针。
        let pointers = unsafe { std::slice::from_raw_parts(array, len) };
        for &pointer in pointers {
            if pointer.is_null() {
                continue;
            }
            // SAFETY: 每个非空指针都是引用计数为 1 的 IMFActivate，所有权移交给 Vec。
            let converted: windows::core::Result<IMFActivate> =
                unsafe { windows::core::Type::from_abi(pointer) };
            match converted {
                Ok(activate) => activates.push(activate),
                Err(err) => tracing::debug!(%err, "枚举到的解码器激活对象无法转换，跳过"),
            }
        }
        // SAFETY: 数组由 MFTEnumEx 用 CoTaskMemAlloc 分配；对象引用已取走。
        unsafe {
            CoTaskMemFree(Some(array.cast::<c_void>().cast_const()));
        }
    }
    Ok(activates)
}

/// 设置输入/输出媒体类型并启动流。
fn configure_types(
    transform: &IMFTransform,
    codec: CodecKind,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(OutputFormat, bool, u32), WinVideoError> {
    let mut errors = Vec::new();
    let mut input_set = false;
    for (subtype, name) in decoder_input_subtypes(codec) {
        let input = build_input_type(subtype, width, height, fps)?;
        // SAFETY: `transform` 与 `input` 都有效；输入流 0 是解码器唯一的输入。
        match unsafe { transform.SetInputType(0, &input, 0) } {
            Ok(()) => {
                input_set = true;
                break;
            }
            Err(err) => errors.push(format!("{name}: {err}")),
        }
    }
    if !input_set {
        return Err(WinVideoError::Decode(format!(
            "SetInputType({}) 失败（{}）",
            codec.label(),
            errors.join("；")
        )));
    }

    let output = negotiate_output_type(transform, width, height)?;

    // SAFETY: 类型已设置；启动消息只影响 MFT 内部状态。
    unsafe {
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|err| decode_err("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING", &err))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|err| decode_err("MFT_MESSAGE_NOTIFY_START_OF_STREAM", &err))?;
    }

    let (provides_samples, output_buffer_size) = output_stream_info(transform, &output)?;
    Ok((output, provides_samples, output_buffer_size))
}

/// 某个标准解码器的输入 subtype 候选与可读名称（按尝试顺序）。
fn decoder_input_subtypes(codec: CodecKind) -> &'static [(GUID, &'static str)] {
    match codec {
        CodecKind::H264 => &[(MFVideoFormat_H264, "H.264")],
        // Annex B 带内参数集走 HEVC_ES；部分 MFT 只注册 MFVideoFormat_HEVC。
        CodecKind::H265 => &[
            (MFVideoFormat_HEVC_ES, "HEVC_ES"),
            (MFVideoFormat_HEVC, "HEVC"),
        ],
    }
}

/// 尽力打开低延迟模式；不支持时只记 debug，不影响解码。
fn configure_low_latency(transform: &IMFTransform) {
    // SAFETY: `transform` 有效；属性存储生命周期由 COM 引用计数维护。
    let Ok(attributes) = (unsafe { transform.GetAttributes() }) else {
        return;
    };
    // SAFETY: 属性存储有效；`MF_LOW_LATENCY` 的约定类型是 UINT32。
    if let Err(err) = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) } {
        tracing::debug!(%err, "解码 MFT 拒绝 MF_LOW_LATENCY，按默认延迟解码");
    }
}

/// 构造输入媒体类型（`subtype` 为压缩格式）。
fn build_input_type(
    subtype: &GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<IMFMediaType, WinVideoError> {
    let media_type = create_media_type()?;
    let progressive = u32::try_from(MFVideoInterlace_Progressive.0).unwrap_or_default();
    // SAFETY: `media_type` 有效；所有 GUID 常数都是 `'static`。
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|err| decode_err("输入类型 MF_MT_MAJOR_TYPE", &err))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, subtype)
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
    let media_type = create_media_type()?;
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
            nv12: None,
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
        nv12: None,
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

/// 判断一个访问单元（Annex B）里是否含关键帧 slice。
///
/// `decode()` 拿到的每个包是完整访问单元，头部带内联参数集；这里只扫起始码
/// （`00 00 01`，4 字节起始码 `00 00 00 01` 也含它），再按编码标准解 NAL 头：
///
/// - H.264：`b0 & 0x1F`，IDR = 5；
/// - H.265：`(b0 >> 1) & 0x3F`，IRAP = `16..=23`（BLA/IDR/CRA）。
///
/// 首帧/整屏重绘/关键帧请求的关键帧都是几百 KB，逐字节扫一遍也在微秒级；
/// 而且只对**含片段数据**的包有意义，参数集的 NAL 头不会误命中。
///
/// 为什么要这个判定：补丁模式（协议 v6）的基准帧就是这张关键帧，控制器若把它
/// 按回读预算**跳过**，合成缓冲会停在旧基准上，后续补丁会盖到旧画面上——
/// 于是画面一直错到下一次关键帧。所以含关键帧的访问单元必须阻塞回读。
fn access_unit_has_idr(payload: &[u8], codec: CodecKind) -> bool {
    let mut index = 0usize;
    while index.saturating_add(3) < payload.len() {
        let window = &payload[index..index.saturating_add(3)];
        if window == [0, 0, 1] {
            // 起始码之后就是 NAL 头的第一个字节。
            let first = payload[index.saturating_add(3)];
            let nal_type = if codec.is_hevc() {
                (first >> 1) & 0x3F
            } else {
                first & 0x1F
            };
            let keyframe = if codec.is_hevc() {
                (16..=23).contains(&nal_type)
            } else {
                nal_type == 5
            };
            if keyframe {
                return true;
            }
            index = index.saturating_add(3);
        } else {
            index = index.saturating_add(1);
        }
    }
    false
}

/// 跳过日志的限频判定：从未打过、或距上一条已满 [`SKIP_LOG_INTERVAL`]。
///
/// 抽成纯函数是为了让「每秒最多一条」这条语义能被单测钉住——现场日志洪泛会把
/// 真正有用的行冲掉。
fn skip_log_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= SKIP_LOG_INTERVAL)
}

/// 把从 `started` 到现在的耗时取整为微秒（饱和到 `u64::MAX`）。
///
/// 解码的子阶段（尤其 GPU 回读）常只有几十微秒量级，毫秒取整会把它们全显示成 0，
/// 无法分辨「卡在出帧」还是「卡在 GPU 同步」。
fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// 运行期加载的 Media Foundation 入口表。
///
/// **不在链接期依赖 `mfplat.dll`**：Windows 10 N/LTSC 或精简镜像可能没有它，
/// 静态导入会让进程在 `main` 之前加载失败（表现为「点开即闪退」）。缺失时这里
/// 返回错误，`H264DecoderFactory::new` 探测失败 → 控制器不声明 H.264 → 回退 VP9。
struct MfApi {
    /// 库必须活到函数表失效之后。
    _lib: libloading::Library,
    /// `MFStartup(version, flags)`。
    startup: unsafe extern "system" fn(u32, u32) -> windows::core::HRESULT,
    /// `MFCreateMediaType(&mut ptr)`。
    create_media_type: unsafe extern "system" fn(*mut *mut c_void) -> windows::core::HRESULT,
    /// `MFCreateMemoryBuffer(len, &mut ptr)`。
    create_memory_buffer:
        unsafe extern "system" fn(u32, *mut *mut c_void) -> windows::core::HRESULT,
    /// `MFCreateSample(&mut ptr)`。
    create_sample: unsafe extern "system" fn(*mut *mut c_void) -> windows::core::HRESULT,
    /// `MFCreateDXGIDeviceManager(&mut token, &mut manager)`。
    ///
    /// 设为 `Option`：极旧/精简镜像可能没有这个导出；缺失时只让 DXVA 失败并
    /// 回退软件，不能把软件解码一起拖下水。
    create_dxgi_device_manager:
        Option<unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> windows::core::HRESULT>,
    /// `MFTEnumEx(category, flags, in, out, activates, count)`。
    ///
    /// 设为 `Option`：只有 H.265 在 `CLSID_MSH265DecoderMFT` 直接创建失败时
    /// 才用它枚举可用的 HEVC 解码器；缺失时 H.265 探测失败（回退 H.264），
    /// H.264 路径完全不受影响。
    enum_ex: Option<MftEnumExFn>,
}

/// `MFTEnumEx(GUID, UINT, in, out, IMFActivate***, UINT*)`。
type MftEnumExFn = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut *mut c_void,
    *mut u32,
) -> windows::core::HRESULT;

// SAFETY: 函数表在进程生命周期内只读；`mfplat.dll` 由 `OnceLock` 永久持有，
// 不会卸载；每次调用创建的 COM 对象仍由调用线程独占。
unsafe impl Send for MfApi {}
// SAFETY: 同上——只读函数表可以被多个线程同时读取。
unsafe impl Sync for MfApi {}

impl MfApi {
    fn load() -> Result<Self, String> {
        // SAFETY: 只加载系统目录下的 mfplat.dll。
        let lib = unsafe { libloading::Library::new("mfplat.dll") }
            .map_err(|err| format!("加载 mfplat.dll 失败: {err}"))?;
        // SAFETY: 符号名与下面的函数指针签名必须和 Media Foundation 的 C ABI
        // 一致；`lib` 在 `MfApi` 存活期间不会卸载。
        let startup = unsafe {
            *lib.get::<unsafe extern "system" fn(u32, u32) -> windows::core::HRESULT>(b"MFStartup")
                .map_err(|err| format!("缺少 MFStartup: {err}"))?
        };
        // SAFETY: 同上。
        let create_media_type = unsafe {
            *lib.get::<unsafe extern "system" fn(*mut *mut c_void) -> windows::core::HRESULT>(
                b"MFCreateMediaType",
            )
            .map_err(|err| format!("缺少 MFCreateMediaType: {err}"))?
        };
        // SAFETY: 同上。
        let create_memory_buffer = unsafe {
            *lib.get::<unsafe extern "system" fn(u32, *mut *mut c_void) -> windows::core::HRESULT>(
                b"MFCreateMemoryBuffer",
            )
            .map_err(|err| format!("缺少 MFCreateMemoryBuffer: {err}"))?
        };
        // SAFETY: 同上。
        let create_sample = unsafe {
            *lib.get::<unsafe extern "system" fn(*mut *mut c_void) -> windows::core::HRESULT>(
                b"MFCreateSample",
            )
            .map_err(|err| format!("缺少 MFCreateSample: {err}"))?
        };
        // MXF/DXVA：可选符号。缺失时硬件路径失败并回退软件，软件解码不受影响。
        // SAFETY: 与上面的符号相同；失败时保持 `None`。
        let create_dxgi_device_manager = unsafe {
            lib.get::<unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> windows::core::HRESULT>(
                b"MFCreateDXGIDeviceManager",
            )
            .ok()
            .map(|symbol| *symbol)
        };
        // H.265 的枚举回退入口；可选符号，缺失时只影响 HEVC 探测。
        // SAFETY: 与上面的符号相同；失败时保持 `None`。
        let enum_ex = unsafe {
            lib.get::<MftEnumExFn>(b"MFTEnumEx")
                .ok()
                .map(|symbol| *symbol)
        };
        Ok(Self {
            _lib: lib,
            startup,
            create_media_type,
            create_memory_buffer,
            create_sample,
            create_dxgi_device_manager,
            enum_ex,
        })
    }
}

/// 取（或首次加载）Media Foundation 入口表。
fn mf_api() -> Result<&'static MfApi, WinVideoError> {
    static API: OnceLock<Result<MfApi, String>> = OnceLock::new();
    match API.get_or_init(MfApi::load) {
        Ok(api) => Ok(api),
        Err(message) => Err(WinVideoError::Decode(message.clone())),
    }
}

/// `MFCreateMediaType` 的运行期版本。
fn create_media_type() -> Result<IMFMediaType, WinVideoError> {
    let api = mf_api()?;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: `raw` 指向本栈帧；成功后返回一个由 `from_abi` 接管的接口指针。
    let hr = unsafe { (api.create_media_type)(&raw mut raw) };
    hr.ok()
        .map_err(|err| decode_err("MFCreateMediaType", &err))?;
    // SAFETY: 成功路径的 `raw` 非空且引用计数为 1，所有权交给返回的接口。
    unsafe { windows::core::Type::from_abi(raw) }
        .map_err(|err| decode_err("MFCreateMediaType", &err))
}

/// `MFCreateMemoryBuffer` 的运行期版本。
fn create_memory_buffer(length: u32) -> Result<IMFMediaBuffer, WinVideoError> {
    let api = mf_api()?;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: `raw` 指向本栈帧；成功后返回一个由 `from_abi` 接管的接口指针。
    let hr = unsafe { (api.create_memory_buffer)(length, &raw mut raw) };
    hr.ok()
        .map_err(|err| decode_err("MFCreateMemoryBuffer", &err))?;
    // SAFETY: 成功路径的 `raw` 非空且引用计数为 1，所有权交给返回的接口。
    unsafe { windows::core::Type::from_abi(raw) }
        .map_err(|err| decode_err("MFCreateMemoryBuffer", &err))
}

/// `MFCreateSample` 的运行期版本。
fn create_sample() -> Result<IMFSample, WinVideoError> {
    let api = mf_api()?;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: `raw` 指向本栈帧；成功后返回一个由 `from_abi` 接管的接口指针。
    let hr = unsafe { (api.create_sample)(&raw mut raw) };
    hr.ok().map_err(|err| decode_err("MFCreateSample", &err))?;
    // SAFETY: 成功路径的 `raw` 非空且引用计数为 1，所有权交给返回的接口。
    unsafe { windows::core::Type::from_abi(raw) }.map_err(|err| decode_err("MFCreateSample", &err))
}

/// 进程级 `MFStartup`（只执行一次；失败结果也缓存，避免反复尝试）。
fn ensure_mf_started() -> Result<(), WinVideoError> {
    static STARTUP: OnceLock<Result<(), String>> = OnceLock::new();
    match STARTUP.get_or_init(|| {
        let api = mf_api().map_err(|err| err.to_string())?;
        // SAFETY: `MFStartup` 是进程级初始化，`MF_VERSION` 是编译期常数。
        unsafe { (api.startup)(MF_VERSION, MFSTARTUP_FULL) }
            .ok()
            .map_err(|err| err.to_string())
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

    /// 回读三段细分的判读：`total_us` 求和、`dominant` 归因（并列时优先 `Map`）。
    ///
    /// 真机排障就是靠这条语义把 445 ms 归到 `Map`（GPU 同步）而不是 `Copy`。
    #[test]
    fn readback_breakdown_sums_and_classifies_dominant_segment() {
        let zeros = ReadbackBreakdown::default();
        assert_eq!(zeros.total_us(), 0);
        assert_eq!(zeros.dominant(), ReadbackPhase::None);

        let map_heavy = ReadbackBreakdown {
            get_resource_us: 40,
            copy_us: 120,
            map_us: 445_000,
        };
        assert_eq!(map_heavy.total_us(), 445_160);
        assert_eq!(map_heavy.dominant(), ReadbackPhase::Map);

        let copy_heavy = ReadbackBreakdown {
            get_resource_us: 10,
            copy_us: 9_000,
            map_us: 300,
        };
        assert_eq!(copy_heavy.dominant(), ReadbackPhase::Copy);

        let get_heavy = ReadbackBreakdown {
            get_resource_us: 7_000,
            copy_us: 10,
            map_us: 20,
        };
        assert_eq!(get_heavy.dominant(), ReadbackPhase::GetResource);

        // 并列时优先 Map：它是唯一的同步点，宁可把锅判给它。
        let tie = ReadbackBreakdown {
            get_resource_us: 500,
            copy_us: 500,
            map_us: 500,
        };
        assert_eq!(tie.dominant(), ReadbackPhase::Map);
    }

    /// 测试回读预算的解析：缺省/非法回落默认，`0` = 不限，其余按毫秒 → 微秒。
    #[test]
    fn readback_budget_parsing_is_bounded() {
        use dxva::parse_readback_budget_us;
        assert_eq!(parse_readback_budget_us(None), DEFAULT_READBACK_BUDGET_US);
        assert_eq!(parse_readback_budget_us(Some("")), DEFAULT_READBACK_BUDGET_US);
        assert_eq!(parse_readback_budget_us(Some("abc")), DEFAULT_READBACK_BUDGET_US);
        assert_eq!(parse_readback_budget_us(Some("-1")), DEFAULT_READBACK_BUDGET_US);
        assert_eq!(parse_readback_budget_us(Some("0")), 0, "0 = 关闭上限");
        assert_eq!(parse_readback_budget_us(Some(" 150 ")), 150_000);
        assert_eq!(
            parse_readback_budget_us(Some(&u64::MAX.to_string())),
            dxva::MAX_READBACK_BUDGET_US,
            "荒谬值必须夹住，否则 Instant + Duration 会溢出"
        );
        assert_eq!(ReadbackOptions::default().readback_budget_us, DEFAULT_READBACK_BUDGET_US);
        assert_eq!(ReadbackOptions::unbounded().readback_budget_us, 0);
    }

    /// IDR 判定：H.264 只有 `nal_unit_type == 5` 的 slice 才算关键帧。
    ///
    /// 这条判定决定「补丁模式基准帧不允许被回读预算跳过」——判错（漏判）会让
    /// 控制端的合成缓冲停在旧基准上，画面一直错到下一次 IDR。
    #[test]
    fn access_unit_idr_detection_covers_annex_b_shapes() {
        let h264 = CodecKind::H264;
        // 空/无起始码。
        assert!(!access_unit_has_idr(&[], h264));
        assert!(!access_unit_has_idr(&[0x00, 0x00, 0x02, 0x65], h264));

        // 3 字节起始码 + nal_unit_type=5（IDR slice）。
        assert!(access_unit_has_idr(&[0x00, 0x00, 0x01, 0x65, 0xAA], h264));
        // 4 字节起始码（`00 00 00 01`）同样命中。
        assert!(access_unit_has_idr(
            &[0x00, 0x00, 0x00, 0x01, 0x65, 0xAA],
            h264
        ));
        // 带 nal_ref_idc 位的 IDR（0x65 已是；再试 0xE5）。
        assert!(access_unit_has_idr(&[0x00, 0x00, 0x01, 0xE5], h264));

        // SPS(7) / PPS(8) / 非 IDR slice(1) / SEI(6) 都不算。
        assert!(!access_unit_has_idr(&[0x00, 0x00, 0x01, 0x67], h264));
        assert!(!access_unit_has_idr(&[0x00, 0x00, 0x01, 0x68], h264));
        assert!(!access_unit_has_idr(&[0x00, 0x00, 0x01, 0x41], h264));
        assert!(!access_unit_has_idr(&[0x00, 0x00, 0x01, 0x06], h264));

        // 典型 IDR 访问单元：SPS + PPS + SEI + IDR。
        let mut access_unit = Vec::new();
        access_unit.extend_from_slice(&[0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1F]);
        access_unit.extend_from_slice(&[0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C]);
        access_unit.extend_from_slice(&[0x00, 0x00, 0x01, 0x06, 0x05]);
        access_unit.extend_from_slice(&[0x00, 0x00, 0x01, 0x65, 0x88, 0x84]);
        assert!(access_unit_has_idr(&access_unit, h264));

        // 只有 SPS/PPS 的配置包看起来像参数集时仍不是 IDR。
        access_unit.truncate(14);
        assert!(!access_unit_has_idr(&access_unit, h264));

        // 起始码在末尾但没有 NAL 头字节：不越界、判 false。
        assert!(!access_unit_has_idr(&[0x00, 0x00, 0x01], h264));
        assert!(!access_unit_has_idr(&[0x00, 0x00, 0x01, 0x00], h264));
    }

    /// HEVC 关键帧判定：IRAP `16..=23` 全算，参数集（VPS 32 / SPS 33 / PPS 34）
    /// 与普通 P 帧都不算；且不会把 H.264 的 IDR 字节误判成 HEVC 关键帧。
    #[test]
    fn access_unit_hevc_irap_detection() {
        let hevc = CodecKind::H265;
        for nal_type in 16u8..=23 {
            let unit = [0x00, 0x00, 0x00, 0x01, (nal_type << 1) | 1, 0x01];
            assert!(
                access_unit_has_idr(&unit, hevc),
                "IRAP {nal_type} 必须被识别为关键帧"
            );
        }
        for nal_type in [0u8, 1, 2, 15, 24, 32, 33, 34, 39] {
            let unit = [0x00, 0x00, 0x00, 0x01, (nal_type << 1) | 1, 0x01];
            assert!(
                !access_unit_has_idr(&unit, hevc),
                "{nal_type} 不是 IRAP，不应算关键帧"
            );
        }
        // 交叉判定：H.264 的 IDR（0x65）按 HEVC 解是 type 50，不是关键帧。
        assert!(!access_unit_has_idr(&[0x00, 0x00, 0x01, 0x65, 0xAA], hevc));
        // 典型 HEVC IDR 访问单元：VPS + SPS + PPS + IDR_W_RADL(19)。
        let mut unit = Vec::new();
        for nal_type in [32u8, 33, 34, 19] {
            unit.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, (nal_type << 1) | 1, 0x01]);
        }
        assert!(access_unit_has_idr(&unit, hevc));
    }

    /// 跳过日志限频：一秒内只放第一条，满一秒后再放。
    #[test]
    fn skip_log_is_rate_limited_to_one_per_second() {
        let base = Instant::now();
        assert!(skip_log_due(None, base), "首帧必须打出来");
        assert!(!skip_log_due(Some(base), base), "同一时刻的连续跳过不应重复打");
        assert!(!skip_log_due(
            Some(base),
            base + std::time::Duration::from_millis(999)
        ));
        assert!(skip_log_due(
            Some(base),
            base + std::time::Duration::from_secs(1)
        ));
        assert!(skip_log_due(
            Some(base),
            base + std::time::Duration::from_secs(5)
        ));
    }

    /// 回读策略：IDR 永远阻塞（绝对优先）；预算 0 阻塞；否则按空闲时间判。
    #[test]
    fn readback_policy_never_skips_idr() {
        use dxva::{readback_policy, ReadbackPolicy};

        // IDR + 预算开启（哪怕空闲很久）→ 阻塞。
        assert_eq!(
            readback_policy(100_000, true, Some(5_000_000)),
            ReadbackPolicy::Blocking,
            "IDR 绝不能被预算跳过（补丁模式基准帧）"
        );
        // IDR + 预算关闭 → 也是阻塞（语义一致，不因为配置改变而不同）。
        assert_eq!(
            readback_policy(0, true, Some(5_000_000)),
            ReadbackPolicy::Blocking
        );

        // 非 IDR + 预算关闭 → 阻塞（旧行为）。
        assert_eq!(
            readback_policy(0, false, Some(5_000_000)),
            ReadbackPolicy::Blocking
        );
        // 非 IDR + 空闲未知（会话第一帧）→ 有界。
        assert_eq!(readback_policy(100_000, false, None), ReadbackPolicy::Bounded);
        // 健康节奏（间隔 ≤ 预算一半）→ 阻塞，零额外开销。
        assert_eq!(
            readback_policy(100_000, false, Some(30_000)),
            ReadbackPolicy::Blocking
        );
        assert_eq!(
            readback_policy(100_000, false, Some(50_000)),
            ReadbackPolicy::Blocking
        );
        // 空闲超过预算一半 → 有界。
        assert_eq!(
            readback_policy(100_000, false, Some(50_001)),
            ReadbackPolicy::Bounded
        );
        assert_eq!(
            readback_policy(100_000, false, Some(250_000)),
            ReadbackPolicy::Bounded
        );
    }

    /// 累加语义：一次 `decode()` 多帧时逐段相加，溢出饱和而不是回绕。
    #[test]
    fn readback_breakdown_accumulates_and_saturates() {
        let one = ReadbackBreakdown {
            get_resource_us: 1,
            copy_us: 2,
            map_us: 3,
        };
        let mut total = ReadbackBreakdown::default();
        total.accumulate(one);
        total.accumulate(one);
        assert_eq!(
            total,
            ReadbackBreakdown {
                get_resource_us: 2,
                copy_us: 4,
                map_us: 6,
            }
        );

        let mut big = ReadbackBreakdown {
            get_resource_us: u64::MAX,
            copy_us: u64::MAX,
            map_us: u64::MAX,
        };
        big.accumulate(one);
        assert_eq!(big.get_resource_us, u64::MAX);
        assert_eq!(big.copy_us, u64::MAX);
        assert_eq!(big.map_us, u64::MAX);
        assert_eq!(big.total_us(), u64::MAX);
    }

    #[test]
    fn unpack_size_rejects_zero() {
        assert_eq!(unpack_size(0), (0, 0));
    }

    /// `Auto` 在 DXVA 初始化失败时必须静默回退到系统内存软件解码。
    #[test]
    fn auto_falls_back_to_software_when_dxva_init_fails() {
        FORCE_HARDWARE_FAILURE.store(true, std::sync::atomic::Ordering::SeqCst);
        let result = MfVideoDecoder::new(640, 360);
        FORCE_HARDWARE_FAILURE.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(result.is_ok(), "Auto 应在 DXVA 失败后回退软件: {result:?}");
        if let Ok(decoder) = result {
            assert_eq!(decoder.path(), H264DecodePath::Software);
            assert!(decoder.adapter_name().is_none());
            assert!(!decoder.hardware_buffers_seen());
        }
    }

    /// 1080p（coded 1920×1088、display 1920×1080）的融合裁剪必须与
    /// 「整帧转换后再裁」逐字节一致。
    #[test]
    fn fused_crop_decode_matches_full_then_crop() {
        let (width, height) = (1920_u32, 1088_u32);
        let w = usize::try_from(width).unwrap_or(0);
        let h = usize::try_from(height).unwrap_or(0);
        let pixels = w * h;
        let mut data = vec![0u8; pixels + pixels / 2];
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = u8::try_from(index % 256).unwrap_or(0);
        }

        let coded = DisplayRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let display = DisplayRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let full_output = OutputFormat {
            coded_width: width,
            coded_height: height,
            display: coded,
            layout: H264OutputFormat::Nv12,
            stride: width,
        };
        let fused_output = OutputFormat {
            display,
            ..full_output
        };

        let full_result = convert_planes(&full_output, data.as_ptr(), w);
        let fused_result = convert_planes(&fused_output, data.as_ptr(), w);
        assert!(full_result.is_ok());
        assert!(fused_result.is_ok());
        let (Ok(full), Ok(fused)) = (full_result, fused_result) else {
            return;
        };
        assert_eq!(fused.width, 1920);
        assert_eq!(fused.height, 1080);

        let row_bytes = w * 4;
        let display_row_bytes = 1920 * 4;
        let mut expected = Vec::with_capacity(fused.rgba.len());
        for row in 0..1080 {
            let start = row * row_bytes;
            expected.extend_from_slice(&full.rgba[start..start + display_row_bytes]);
        }
        assert_eq!(fused.rgba, expected);
    }

    /// NV12 半平面：紧包必须只拷可见区域，且 UV 的 x 偏移就是 `display.x`。
    #[test]
    fn pack_nv12_packs_semipolar_visible_region() {
        let (w, h) = (8usize, 8usize);
        let y: Vec<u8> = (0..w * h)
            .map(|index| u8::try_from(index % 251).unwrap_or(0))
            .collect();
        let uv: Vec<u8> = (0..w * h / 2)
            .map(|index| u8::try_from((index * 3) % 251).unwrap_or(0))
            .collect();
        let chroma = ChromaPlanes::Interleaved {
            uv: &uv,
            pitch: w,
        };

        // 整帧：逐字节相同。
        let full = pack_nv12(
            &y,
            w,
            chroma,
            DisplayRect {
                x: 0,
                y: 0,
                width: 8,
                height: 8,
            },
        );
        assert!(full.is_ok(), "整帧打包应成功");
        let Ok(full) = full else {
            return;
        };
        assert_eq!(full.y, y);
        assert_eq!(full.uv, uv);

        // 非零偶数偏移：Y 按行取 x..x+w，UV 按行取 x..x+w。
        let cropped = pack_nv12(
            &y,
            w,
            chroma,
            DisplayRect {
                x: 2,
                y: 2,
                width: 4,
                height: 4,
            },
        );
        assert!(cropped.is_ok(), "偶数裁剪打包应成功");
        let Ok(cropped) = cropped else {
            return;
        };
        let mut expected_y = Vec::new();
        for row in 2..6usize {
            expected_y.extend_from_slice(&y[row * w + 2..row * w + 6]);
        }
        let mut expected_uv = Vec::new();
        for row in 1..3usize {
            expected_uv.extend_from_slice(&uv[row * w + 2..row * w + 6]);
        }
        assert_eq!(cropped.y, expected_y);
        assert_eq!(cropped.uv, expected_uv);
    }

    /// YV12 / IYUV 的全平面布局必须按 2×2 交织成 NV12 的 UV。
    #[test]
    fn pack_nv12_interleaves_planar_layouts() {
        let (width, height) = (8usize, 8usize);
        let plane_stride = width / 2;
        let plane_len = plane_stride * (height / 2);
        let y_plane: Vec<u8> = (0..width * height)
            .map(|index| u8::try_from(index % 251).unwrap_or(0))
            .collect();
        let u_plane: Vec<u8> = (0..plane_len)
            .map(|index| u8::try_from(100 + index).unwrap_or(0))
            .collect();
        let v_plane: Vec<u8> = (0..plane_len)
            .map(|index| u8::try_from(200 + index).unwrap_or(0))
            .collect();
        let display = DisplayRect {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        };
        let planar = ChromaPlanes::Planar {
            u: &u_plane,
            v: &v_plane,
            pitch: plane_stride,
        };
        let packed = pack_nv12(&y_plane, width, planar, display);
        assert!(packed.is_ok(), "平面布局打包应成功");
        let Ok(packed) = packed else {
            return;
        };
        let mut expected_uv = Vec::with_capacity(plane_len * 2);
        for (&cb, &cr) in u_plane.iter().zip(&v_plane) {
            expected_uv.push(cb);
            expected_uv.push(cr);
        }
        assert_eq!(packed.y, y_plane);
        assert_eq!(packed.uv, expected_uv);

        // 走完整入口，锁定 YV12（V 在前）与 IYUV（U 在前）的平面顺序。
        let output = |layout| OutputFormat {
            coded_width: 8,
            coded_height: 8,
            display,
            layout,
            stride: 8,
        };
        for layout in [H264OutputFormat::Yv12, H264OutputFormat::Iyuv] {
            let mut raw = y_plane.clone();
            if layout == H264OutputFormat::Yv12 {
                raw.extend_from_slice(&v_plane);
                raw.extend_from_slice(&u_plane);
            } else {
                raw.extend_from_slice(&u_plane);
                raw.extend_from_slice(&v_plane);
            }
            let frame = convert_frame(
                &output(layout),
                H264OutputMode::Nv12,
                raw.as_ptr(),
                width,
            );
            assert!(frame.is_ok(), "{layout:?} 应能打包");
            if let Ok(frame) = frame {
                assert!(frame.nv12.is_some(), "{layout:?} 应输出 NV12");
                if let Some(nv12) = frame.nv12 {
                    assert_eq!(nv12.uv, expected_uv, "{layout:?} 的 U/V 顺序错误");
                }
            }
        }
    }

    /// 奇数可见区域必须回退 RGBA（不报错、不丢帧）。
    #[test]
    fn odd_visible_region_falls_back_to_rgba() {
        let output = OutputFormat {
            coded_width: 8,
            coded_height: 8,
            display: DisplayRect {
                x: 1,
                y: 0,
                width: 6,
                height: 8,
            },
            layout: H264OutputFormat::Nv12,
            stride: 8,
        };
        let data: Vec<u8> = (0..8 * 8 * 3 / 2)
            .map(|index| u8::try_from(index % 251).unwrap_or(0))
            .collect();
        let frame = convert_frame(&output, H264OutputMode::Nv12, data.as_ptr(), 8);
        assert!(frame.is_ok(), "奇数光圈应回退而不是报错");
        if let Ok(frame) = frame {
            assert!(frame.nv12.is_none(), "奇数光圈不得输出 NV12");
            assert_eq!((frame.width, frame.height), (6, 8));
            assert_eq!(frame.rgba.len(), 6 * 8 * 4);
        }
    }

    /// 公开的 CPU 回退转换必须与内部颜色转换逐字节一致。
    #[test]
    fn public_nv12_to_rgba_matches_internal_conversion() {
        let (w, h) = (8u32, 8u32);
        let pitch = usize::try_from(w).unwrap_or(0);
        let pixels = pitch * usize::try_from(h).unwrap_or(0);
        let y: Vec<u8> = (0..pixels)
            .map(|index| u8::try_from(index % 220).unwrap_or(0).saturating_add(16))
            .collect();
        let uv: Vec<u8> = (0..pixels / 2)
            .map(|index| u8::try_from((index * 7) % 200).unwrap_or(0).saturating_add(16))
            .collect();

        let mut expected = Vec::new();
        let internal = color::nv12_to_rgba(&y, &uv, pitch, pitch, w, h, &mut expected);
        assert!(internal.is_ok(), "内部转换应成功");
        let public = nv12_to_rgba(&y, &uv, w, h);
        assert_eq!(public.ok().as_deref(), Some(expected.as_slice()));

        // 长度不符时给出错误而不是越界。
        assert!(nv12_to_rgba(&y[..y.len() - 1], &uv, w, h).is_err());
        assert!(nv12_to_rgba(&y, &uv[..uv.len() - 1], w, h).is_err());
    }
}
