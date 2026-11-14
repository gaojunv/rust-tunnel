//! Media Foundation H.264 **硬件编码器**（Intel Quick Sync / NVIDIA / AMD）。
//!
//! # 为什么需要这个后端
//!
//! Intel HD 630 没有 NVENC，现有路径会回退到 VP9 软件编码（实测 113 ms/帧）。
//! Windows 上 Intel 核显通过 Quick Sync Video 注册了硬件 H.264 编码器 MFT；
//! 本模块用 `MFTEnumEx(MFT_ENUM_FLAG_HARDWARE)` 找到它，把 RGBA 转成 NV12 后
//! 交给 GPU 编码，输出 Annex B 码流。
//!
//! # 输入/输出约定
//!
//! - 输入：`width × height` 的 **RGBA8**（与 remote 的采集侧一致），CPU 转
//!   NV12（BT.709 limited range，与 [`crate::nvenc`] / [`crate::decode`] 同一套常数）。
//! - 输出：[`EncodedPacket`]：`data` = **4 字节大端长度前缀 + Annex B**，与
//!   NVENC 后端逐字节同构；`keyframe` 由码流里的 IDR NAL 判定。MFT 若输出
//!   AVCC，会先转成 Annex B。关键帧会自动确保带 SPS/PPS。
//!
//! # 只选硬件
//!
//! [`MftH264Encoder::new`] **只枚举 `MFT_ENUM_FLAG_HARDWARE`**；一个都没有就
//! 返回错误，让上层回退 VP9。不会静默选择软件 MFT——那会把「硬件编码」变成
//! 「更慢的软件编码」。
//!
//! # 运行期改码率/帧率
//!
//! 优先走 `ICodecAPI`（峰值受限 VBR + mean/max bitrate / frame rate）。不重建 MFT、
//! 不强制 IDR；MFT 不支持时返回明确错误，不假装成功。**码率控制默认用峰值受限 VBR
//! 而不是 CBR**：Intel QSV 的 CBR 会给静止画面插填充把码流灌到目标码率（实测
//! 1080p@30fps、目标 8 Mbps 时静止帧仍是 ~21.6 KB/帧），远程桌面一静止就白占链路。

mod bitstream;
mod convert;
mod device;
mod mfplat;
mod video_processor;

use crate::capture::CapturedTexture;
use crate::d3d11;
use crate::nvenc::EncodedPacket;
use crate::WinVideoError;
use std::cell::Cell;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{Interface as _, GUID, PWSTR};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_PeakConstrainedVBR,
    CODECAPI_AVEncCommonLowLatency, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMaxFrameRate,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoOutputFrameRate, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform, METransformHaveOutput,
    METransformNeedInput, MFMediaType_Video, MFT_FRIENDLY_NAME_Attribute,
    MFT_TRANSFORM_CLSID_Attribute, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
    MF_E_NOTACCEPTING, MF_E_NO_EVENTS_AVAILABLE, MF_E_NO_MORE_TYPES,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_UI4, VT_UI8};

/// Media Foundation 的时间基（100ns/秒）。
const HNS_PER_SECOND: i64 = 10_000_000;
/// 枚举输出类型的安全上限。
const MAX_OUTPUT_TYPES: u32 = 64;
/// 异步 MFT 等待事件的超时（避免驱动异常时永久阻塞编码线程）。
const EVENT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
/// 异步事件轮询间隔。
///
/// 有意取亚毫秒值：进程启动时已用 `timeBeginPeriod(1)` 把定时器粒度降到 1 ms，
/// 但 500 µs 在未生效的机器上仍会按 1 ms 粒度休眠，不会更糟；配合
/// `METransformHaveOutput` 的到达节奏，比固定 1 ms 少一次多余休眠。
const EVENT_POLL_INTERVAL: Duration = Duration::from_micros(500);

/// 峰值受限 VBR 的平均码率占目标码率的百分比。
///
/// 目标码率（`EncodeRates::bitrate_kbps` / 档位上限）当作**峰值**：复杂画面可以打到
/// 它，静止/简单画面则明显低于它。远程桌面绝大多数时间属于后者。
const VBR_MEAN_PERCENT: u32 = 70;

/// 枚举到的一个 H.264 编码器 MFT。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderMftInfo {
    /// `MFT_FRIENDLY_NAME_Attribute`。
    pub name: String,
    /// `MFT_TRANSFORM_CLSID_Attribute` 的可读形式。
    pub clsid: String,
    /// 是否来自硬件枚举（`MFT_ENUM_FLAG_HARDWARE`）。
    pub hardware: bool,
}

/// 一个硬件 H.264 编码器实例。
pub struct MftH264Encoder {
    transform: IMFTransform,
    /// 编码器 MFT 绑定的 D3D11 设备与设备管理器；必须活到 MFT 销毁。
    encode_device: device::EncodeDevice,
    /// 编码器暴露的 `ICodecAPI`；运行期改码率/帧率/强制关键帧都靠它。
    codec: Option<ICodecAPI>,
    /// 异步 MFT 的事件源（Intel QSV 是异步的）。
    events: Option<IMFMediaEventGenerator>,
    /// 上一次事件循环里 MFT 已经请求输入、但当时没有新样本可喂。
    needs_input: bool,
    name: String,
    clsid: String,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    /// 输出流是否由 MFT 自己分配样本。
    provides_samples: bool,
    /// 调用方需要提供的输出缓冲区大小。
    output_buffer_size: u32,
    /// 复用的 NV12 输入缓冲（CPU 路径）。
    nv12: Vec<u8>,
    /// GPU 路径的 BGRA→NV12 转换器；首次 `encode_texture` 时按输入尺寸懒建。
    video: Option<video_processor::Nv12Converter>,
    /// 输出媒体类型里带的 SPS/PPS（Annex B），必要时前置到关键帧。
    sequence_header: Vec<u8>,
    next_pts: i64,
    frame_duration: i64,
    /// 最近一帧 `rgba_to_nv12` 的耗时（毫秒），仅供诊断。
    last_convert_ms: u32,
    /// 最近一帧 MFT 处理（建样本 + ProcessInput/ProcessOutput + Annex B 归一化）
    /// 的耗时（毫秒），仅供诊断。
    last_mft_ms: u32,
}

impl std::fmt::Debug for MftH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MftH264Encoder")
            .field("name", &self.name)
            .field("adapter", &self.encode_device.device.adapter.name)
            .field("size", &(self.width, self.height))
            .field("fps", &self.fps)
            .field("bitrate_kbps", &self.bitrate_kbps)
            .finish_non_exhaustive()
    }
}

// SAFETY: `MftH264Encoder` 独占持有进程内编码器 MFT 与它的 `ICodecAPI`；结构体
// 不实现 `Sync`，`encode_rgba`/`set_*` 都取 `&mut self`，所以同一时刻只有一个
// 线程访问 COM 对象。编码任务把它移进 `spawn_blocking` 后不会跨线程共享。
unsafe impl Send for MftH264Encoder {}

/// 编译期断言：编码器必须能移进独占的编码线程。
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<MftH264Encoder>();
};

/// 枚举 H.264 编码器 MFT。
///
/// `hardware_only == true` 时只用 `MFT_ENUM_FLAG_HARDWARE`（生产路径）；
/// `false` 时用 `MFT_ENUM_FLAG_ALL` 作对照，把硬件/软件 MFT 都列出来。
///
/// # Errors
/// `mfplat.dll` 缺失 / 缺 `MFTEnumEx`、或枚举本身失败时返回 [`WinVideoError::Encode`]。
pub fn enumerate_h264_encoders(hardware_only: bool) -> Result<Vec<EncoderMftInfo>, WinVideoError> {
    ensure_com_initialized()?;
    let activates = enumerate_activates(hardware_only)?;
    Ok(activates
        .iter()
        .map(|activate| EncoderMftInfo {
            name: friendly_name(activate),
            clsid: clsid(activate),
            hardware: hardware_only,
        })
        .collect())
}

/// 按过滤条件枚举编码器激活对象。
fn enumerate_activates(hardware_only: bool) -> Result<Vec<IMFActivate>, WinVideoError> {
    ensure_com_initialized()?;
    mfplat::ensure_started()?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let base = if hardware_only {
        MFT_ENUM_FLAG_HARDWARE.0
    } else {
        MFT_ENUM_FLAG_ALL.0
    };
    let flags = u32::try_from(base | MFT_ENUM_FLAG_SORTANDFILTER.0).unwrap_or(0);
    mfplat::enumerate_encoders(flags, &input, &output)
}

/// 读取激活对象的友好名称（`CoTaskMemAlloc` 分配，读完释放）。
fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = PWSTR::null();
    let mut length = 0u32;
    // SAFETY: `name` / `length` 指向本栈帧；成功时字符串由 CoTaskMemAlloc 分配。
    let result = unsafe {
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &raw mut name, &raw mut length)
    };
    if result.is_err() || name.is_null() {
        return "（无名称）".to_owned();
    }
    // SAFETY: 成功路径保证 `name` 是以 NUL 结尾的 UTF-16 字符串。
    let text = unsafe { name.to_string() }.unwrap_or_default();
    // SAFETY: 与 GetAllocatedString 配对，释放 CoTaskMemAlloc 分配的内存。
    unsafe { CoTaskMemFree(Some(name.0.cast::<c_void>().cast_const())) };
    text
}

/// 读取激活对象的 CLSID。
fn clsid(activate: &IMFActivate) -> String {
    // SAFETY: `activate` 有效；失败时返回占位字符串。
    match unsafe { activate.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) } {
        Ok(guid) => format!("{guid:?}"),
        Err(_) => "（未知 CLSID）".to_owned(),
    }
}

impl MftH264Encoder {
    /// 创建并初始化一个硬件 H.264 编码器。
    ///
    /// 优先选择名称含 `Intel` / `Quick Sync` 的 MFT，其次任意硬件 H.264 编码器。
    ///
    /// # Errors
    /// 尺寸/帧率/码率非法、没有硬件 H.264 编码器 MFT、激活失败、媒体类型被
    /// 拒绝或 Media Foundation 初始化失败时返回 [`WinVideoError::Encode`]。
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self, WinVideoError> {
        validate_config(width, height, fps, bitrate_kbps)?;
        ensure_com_initialized()?;
        let encode_device = device::create()?;
        Self::with_device(encode_device, width, height, fps, bitrate_kbps)
    }

    /// 用调用方提供的 D3D11 设备创建硬件 H.264 编码器。
    ///
    /// GPU 纹理路径（[`MftH264Encoder::encode_texture`]）**必须**走这个构造函数：
    /// 采集纹理与编码器 MFT 的设备管理器必须处在同一个设备上（调用方可用
    /// 采集源的 [`CaptureSource::device`](crate::capture::CaptureSource::device)）。
    /// 传入的 `device` 会被 `Arc` 共享，不会重建或替换。
    ///
    /// # Errors
    /// 参数非法、没有硬件 H.264 编码器 MFT、设备管理器创建/绑定失败、媒体类型
    /// 被拒绝时返回 [`WinVideoError::Encode`]。
    pub fn new_with_device(
        device: Arc<d3d11::D3d11>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self, WinVideoError> {
        validate_config(width, height, fps, bitrate_kbps)?;
        ensure_com_initialized()?;
        let encode_device = device::create_with_device(device)?;
        Self::with_device(encode_device, width, height, fps, bitrate_kbps)
    }

    /// 公共初始化：枚举/激活 MFT、绑定设备管理器、协商媒体类型。
    fn with_device(
        encode_device: device::EncodeDevice,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self, WinVideoError> {
        ensure_com_initialized()?;
        let activates = enumerate_activates(true)?;
        if activates.is_empty() {
            return Err(WinVideoError::Encode(
                "没有可用的硬件 H.264 编码器 MFT（需要 Intel QSV/NVIDIA/AMD 硬件编码器）"
                    .to_owned(),
            ));
        }
        let selected = select_preferred(&activates);

        let name = friendly_name(selected);
        let clsid = clsid(selected);
        // SAFETY: `selected` 是有效的 IMFActivate，激活出 IMFTransform。
        let transform: IMFTransform = unsafe { selected.ActivateObject() }
            .map_err(|err| encode_err("IMFActivate::ActivateObject", &err))?;
        let is_async = is_async_transform(&transform)?;

        // 硬件 MFT 需要 D3D11 设备管理器；必须在媒体类型之前绑定。
        device::bind(&encode_device, &transform)?;

        // Intel QSV 是异步 MFT：必须走 METransformNeedInput/HaveOutput 事件模型。
        let events =
            if is_async {
                Some(transform.cast::<IMFMediaEventGenerator>().map_err(|err| {
                    encode_err("IMFMediaEventGenerator（异步 MFT 缺少事件源）", &err)
                })?)
            } else {
                None
            };

        let codec = transform.cast::<ICodecAPI>().ok();
        if let Some(codec) = &codec {
            configure_codec_defaults(codec, bitrate_kbps.saturating_mul(1000));
        }

        configure_media_types(&transform, width, height, fps, bitrate_kbps)?;

        // SAFETY: 媒体类型已设置；启动消息只影响 MFT 内部状态。
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|err| encode_err("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING", &err))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|err| encode_err("MFT_MESSAGE_NOTIFY_START_OF_STREAM", &err))?;
        }

        let sequence_header = read_sequence_header(&transform);
        let (provides_samples, output_buffer_size) = output_stream_info(&transform, width, height)?;

        tracing::info!(
            mft = %name,
            clsid = %clsid,
            size = %format!("{width}x{height}"),
            fps,
            bitrate_kbps,
            "Media Foundation 硬件 H.264 编码器已就绪"
        );

        Ok(Self {
            transform,
            encode_device,
            codec,
            events,
            needs_input: false,
            name,
            clsid,
            width,
            height,
            fps,
            bitrate_kbps,
            provides_samples,
            output_buffer_size,
            nv12: Vec::new(),
            video: None,
            sequence_header,
            next_pts: 0,
            frame_duration: HNS_PER_SECOND / i64::from(fps),
            last_convert_ms: 0,
            last_mft_ms: 0,
        })
    }

    /// 编码尺寸 `(width, height)`。
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 当前目标码率（kbps）。
    #[must_use]
    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    /// 当前目标帧率。
    #[must_use]
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// 实际使用的 MFT 友好名称。
    #[must_use]
    pub fn mft_name(&self) -> &str {
        &self.name
    }

    /// 实际使用的 MFT CLSID（可读形式）。
    #[must_use]
    pub fn mft_clsid(&self) -> &str {
        &self.clsid
    }

    /// 实际绑定的 D3D11 适配器名。
    #[must_use]
    pub fn adapter_name(&self) -> &str {
        &self.encode_device.device.adapter.name
    }

    /// 最近一帧 RGBA→NV12 转换的耗时，单位毫秒。
    ///
    /// CPU 路径是 `convert::rgba_to_nv12` 的耗时；GPU 路径
    /// （[`MftH264Encoder::encode_texture`]）是 `VideoProcessorBlt`（含首次
    /// 建转换器的开销）的耗时。只用于诊断，取整到毫秒。
    #[must_use]
    pub fn last_convert_ms(&self) -> u32 {
        self.last_convert_ms
    }

    /// 最近一帧 MFT 处理的耗时，单位毫秒。
    ///
    /// 覆盖建输入样本、`ProcessInput`/`ProcessOutput` 与 Annex B 归一化；同为
    /// 诊断读数，取整到毫秒。
    #[must_use]
    pub fn last_mft_ms(&self) -> u32 {
        self.last_mft_ms
    }

    /// 编码一帧 RGBA8。
    ///
    /// `force_keyframe == true` 时必须产出 IDR + SPS/PPS；若编码器不支持强制
    /// 关键帧或实际输出不是 IDR，返回错误（不静默降级）。
    ///
    /// **可能返回空的 `data`**：部分硬件编码器有一帧流水线延迟，本次调用喂进去
    /// 的帧会在下一次 `encode_rgba` 才吐出来。调用方应跳过空包（它的 `keyframe`
    /// 为 false，且 `data` 为空）。
    ///
    /// # Errors
    /// RGBA 长度不足、码率/帧率非法、`ICodecAPI` 强制关键帧失败、MFT 处理失败
    /// 或码流格式无法识别时返回 [`WinVideoError::Encode`]。
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        force_keyframe: bool,
    ) -> Result<EncodedPacket, WinVideoError> {
        // 编码任务可能在线程池线程上运行：COM 初始化是线程局部的。
        ensure_com_initialized()?;
        let convert_started = Instant::now();
        convert::rgba_to_nv12(rgba, self.width, self.height, &mut self.nv12)?;
        self.last_convert_ms = elapsed_ms(convert_started);

        let sample = self.build_input_sample()?;
        self.process_sample(sample, force_keyframe)
    }

    /// 编码一帧**采集纹理**（GPU 路径）。
    ///
    /// 输入纹理必须是 BGRA8、且与编码器在**同一个 D3D11 设备**上（`Arc::ptr_eq`）。
    /// 内部用 D3D11 VideoProcessor 把它转换/缩放成 NV12（BT.709 limited），再用
    /// `MFCreateDXGISurfaceBuffer` 包成样本喂给 MFT；全程不经过 CPU 内存。
    ///
    /// 与 [`MftH264Encoder::encode_rgba`] 共享关键帧请求、异步事件循环、Annex B
    /// 归一化与空包语义。
    ///
    /// # Errors
    /// 纹理格式/尺寸非法、设备不一致、VideoProcessor 初始化或 Blt 失败、GPU
    /// 表面缓冲创建失败、MFT 处理失败时返回 [`WinVideoError::Encode`]。任何 GPU
    /// 环节失败都**不会**静默回退 CPU——回退由上层决定。
    pub fn encode_texture(
        &mut self,
        frame: &CapturedTexture,
        force_keyframe: bool,
    ) -> Result<EncodedPacket, WinVideoError> {
        ensure_com_initialized()?;
        if !Arc::ptr_eq(&frame.device, &self.encode_device.device) {
            return Err(WinVideoError::Encode(format!(
                "采集纹理与编码器不在同一个 D3D11 设备上（采集 {} vs 编码 {}）",
                frame.device.adapter.name, self.encode_device.device.adapter.name
            )));
        }
        if frame.width == 0 || frame.height == 0 {
            return Err(WinVideoError::Encode(format!(
                "采集纹理尺寸不能为 0：{}x{}",
                frame.width, frame.height
            )));
        }

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `frame.texture` 是有效纹理；`desc` 指向本栈帧。
        unsafe { frame.texture.GetDesc(&raw mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            && desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        {
            return Err(WinVideoError::Encode(format!(
                "GPU 编码只接受 BGRA8 纹理，收到 {:?}",
                desc.Format
            )));
        }
        if desc.Width == 0 || desc.Height == 0 {
            return Err(WinVideoError::Encode(format!(
                "采集纹理描述尺寸非法：{}x{}",
                desc.Width, desc.Height
            )));
        }

        let key = (desc.Width, desc.Height, desc.Format);
        let needs_rebuild = self
            .video
            .as_ref()
            .is_none_or(|converter| converter.input_key() != key);
        let convert_started = Instant::now();
        if needs_rebuild {
            self.video = Some(video_processor::Nv12Converter::new(
                &self.encode_device.device,
                desc.Width,
                desc.Height,
                desc.Format,
                self.width,
                self.height,
                self.fps,
            )?);
        }
        let converter = self
            .video
            .as_mut()
            .ok_or_else(|| WinVideoError::Encode("VideoProcessor 未初始化".to_owned()))?;
        let nv12 = converter.convert(frame)?;
        self.last_convert_ms = elapsed_ms(convert_started);

        let sample = self.build_texture_sample(&nv12)?;
        self.process_sample(sample, force_keyframe)
    }

    /// 公共的「样本 → 码流」处理：关键帧请求、异步/同步事件循环、Annex B 归一化。
    fn process_sample(
        &mut self,
        sample: IMFSample,
        force_keyframe: bool,
    ) -> Result<EncodedPacket, WinVideoError> {
        if force_keyframe {
            self.request_keyframe()?;
        }

        let mft_started = Instant::now();
        let raw = if self.events.is_some() {
            self.encode_async(sample)?
        } else {
            self.encode_sync(&sample)?
        };

        let annex_b = bitstream::normalize_to_annex_b(&raw)?;
        self.last_mft_ms = elapsed_ms(mft_started);
        if annex_b.is_empty() {
            // 编码器还在缓冲；调用方跳过本帧。
            return Ok(EncodedPacket {
                data: Vec::new(),
                keyframe: false,
            });
        }
        if bitstream::has_parameter_sets(&annex_b) {
            self.sequence_header = bitstream::parameter_sets(&annex_b);
        }
        let keyframe = bitstream::has_idr(&annex_b);
        let payload = if keyframe
            && !bitstream::has_parameter_sets(&annex_b)
            && !self.sequence_header.is_empty()
        {
            let mut with_headers =
                Vec::with_capacity(self.sequence_header.len().saturating_add(annex_b.len()));
            with_headers.extend_from_slice(&self.sequence_header);
            with_headers.extend_from_slice(&annex_b);
            with_headers
        } else {
            annex_b
        };
        if force_keyframe && !keyframe {
            return Err(WinVideoError::Encode(
                "请求了关键帧，但编码器输出里没有 IDR NAL".to_owned(),
            ));
        }

        let size = u32::try_from(payload.len())
            .map_err(|_| WinVideoError::Encode("编码包超过 4 GiB".to_owned()))?;
        let mut data = Vec::with_capacity(4 + payload.len());
        data.extend_from_slice(&size.to_be_bytes());
        data.extend_from_slice(&payload);
        Ok(EncodedPacket { data, keyframe })
    }

    /// 运行期修改目标码率，**不重建 MFT、不强制 IDR**。
    ///
    /// 同时更新峰值与均值（峰值受限 VBR；编码器不支持时退回 CBR，见
    /// [`apply_rate_control`]）。
    ///
    /// # Errors
    /// MFT 未暴露 `ICodecAPI`、不支持 `CODECAPI_AVEncCommonMeanBitRate` 或
    /// `SetValue` 失败时返回 [`WinVideoError::Encode`]。
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), WinVideoError> {
        if kbps == 0 {
            return Err(WinVideoError::Encode("码率不能为 0".to_owned()));
        }
        ensure_com_initialized()?;
        let codec = self.codec.as_ref().ok_or_else(|| {
            WinVideoError::Encode("MFT 未暴露 ICodecAPI，不支持运行期改码率".to_owned())
        })?;
        // SAFETY: `codec` 有效；`IsSupported` 只查询能力。
        if let Err(err) = unsafe { codec.IsSupported(&CODECAPI_AVEncCommonMeanBitRate) } {
            return Err(WinVideoError::Encode(format!(
                "编码器不支持运行期改码率（CODECAPI_AVEncCommonMeanBitRate）: {err}"
            )));
        }
        apply_rate_control(codec, kbps.saturating_mul(1000))?;
        self.bitrate_kbps = kbps;
        Ok(())
    }

    /// 运行期修改目标帧率，**不重建 MFT、不强制 IDR**。
    ///
    /// # Errors
    /// MFT 未暴露 `ICodecAPI`、不支持帧率属性或 `SetValue` 失败时返回
    /// [`WinVideoError::Encode`]（调用方据此决定是否重建编码器）。
    pub fn set_fps(&mut self, fps: u32) -> Result<(), WinVideoError> {
        if fps == 0 {
            return Err(WinVideoError::Encode("帧率不能为 0".to_owned()));
        }
        ensure_com_initialized()?;
        let codec = self.codec.as_ref().ok_or_else(|| {
            WinVideoError::Encode("MFT 未暴露 ICodecAPI，不支持运行期改帧率".to_owned())
        })?;
        let rate = (u64::from(fps) << 32) | 1;
        let mut last_error = None;
        let mut supported = false;
        let keys: [&GUID; 2] = [
            &CODECAPI_AVEncVideoOutputFrameRate,
            &CODECAPI_AVEncMaxFrameRate,
        ];
        for key in keys {
            // SAFETY: `codec` 有效；`IsSupported` 只查询能力。
            if unsafe { codec.IsSupported(key) }.is_err() {
                continue;
            }
            supported = true;
            match set_codec_u64(codec, key, rate) {
                Ok(()) => {
                    self.fps = fps;
                    self.frame_duration = HNS_PER_SECOND / i64::from(fps);
                    return Ok(());
                }
                Err(err) => last_error = Some(err),
            }
        }
        let detail =
            last_error.map_or_else(|| "没有可用的帧率属性".to_owned(), |err| err.to_string());
        Err(WinVideoError::Encode(format!(
            "编码器不支持运行期改帧率（supported={supported}）: {detail}"
        )))
    }

    /// 同步 MFT 路径（支持同步 ProcessInput/ProcessOutput 的实现）。
    fn encode_sync(&mut self, sample: &IMFSample) -> Result<Vec<u8>, WinVideoError> {
        let mut raw = Vec::new();
        // SAFETY: `transform` 有效且媒体类型已协商；`sample` 在调用期间存活。
        match unsafe { self.transform.ProcessInput(0, sample, 0) } {
            Ok(()) => {}
            Err(err) if err.code() == MF_E_NOTACCEPTING => {
                // 输入队列满：先排空已有输出，再重试一次。
                raw.extend_from_slice(&self.drain_output()?);
                // SAFETY: 同上次调用；`sample` 仍在本栈帧内。
                unsafe { self.transform.ProcessInput(0, sample, 0) }
                    .map_err(|retry| encode_err("ProcessInput（重试）", &retry))?;
            }
            Err(err) => return Err(encode_err("ProcessInput", &err)),
        }
        raw.extend_from_slice(&self.drain_output()?);
        Ok(raw)
    }

    /// 异步 MFT 路径（Intel QSV）：按 `METransformNeedInput` /
    /// `METransformHaveOutput` 事件驱动喂输入、取输出。
    fn encode_async(&mut self, sample: IMFSample) -> Result<Vec<u8>, WinVideoError> {
        let events = self
            .events
            .clone()
            .ok_or_else(|| WinVideoError::Encode("异步编码器缺少事件源".to_owned()))?;
        let mut pending = Some(sample);
        let mut raw = Vec::new();
        let deadline = Instant::now() + EVENT_WAIT_TIMEOUT;

        // 上一轮事件循环里 MFT 已经请求输入，但当时没有新样本；现在补上。
        if self.needs_input {
            if let Some(sample) = pending.take() {
                self.process_input(&sample)?;
                self.needs_input = false;
            }
        }

        loop {
            // SAFETY: `events` 有效；`GetEvent` 返回的事件对象由返回值持有。
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if !raw.is_empty() || Instant::now() >= deadline {
                        if raw.is_empty() {
                            return Err(WinVideoError::Encode(
                                "等待编码器事件超时（METransformNeedInput/HaveOutput）".to_owned(),
                            ));
                        }
                        break;
                    }
                    std::thread::sleep(EVENT_POLL_INTERVAL);
                    continue;
                }
                Err(err) => return Err(encode_err("IMFMediaEventGenerator::GetEvent", &err)),
            };
            // SAFETY: `event` 有效。
            let kind = unsafe { event.GetType() }
                .map_err(|err| encode_err("IMFMediaEvent::GetType", &err))?;
            let need_input = u32::try_from(METransformNeedInput.0).unwrap_or(0);
            let have_output = u32::try_from(METransformHaveOutput.0).unwrap_or(0);
            if kind == need_input {
                if let Some(sample) = pending.take() {
                    self.process_input(&sample)?;
                } else {
                    // 本次输入已经喂完，编码器还要更多（lookahead）；下次调用再补。
                    self.needs_input = true;
                    break;
                }
            } else if kind == have_output {
                raw.extend_from_slice(&self.process_output_one()?);
            }
            if !raw.is_empty() && pending.is_none() {
                break;
            }
        }
        Ok(raw)
    }

    /// 异步模式下按 `METransformNeedInput` 调用一次 ProcessInput。
    fn process_input(&self, sample: &IMFSample) -> Result<(), WinVideoError> {
        // SAFETY: `transform` 有效且 MFT 已请求输入；`sample` 在调用期间存活。
        unsafe { self.transform.ProcessInput(0, sample, 0) }
            .map_err(|err| encode_err("ProcessInput（异步）", &err))
    }

    /// 异步模式下按 `METransformHaveOutput` 取一次输出。
    fn process_output_one(&mut self) -> Result<Vec<u8>, WinVideoError> {
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
        // 指向本栈帧。pSample/pEvents 是 ManuallyDrop，稍后手动取出释放。
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &raw mut status)
        };
        // SAFETY: 无论成功失败都要取出来释放引用计数。
        let produced = unsafe { ManuallyDrop::take(&mut output.pSample) };
        let events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
        drop(events);
        match result {
            Ok(()) => produced.map_or_else(|| Ok(Vec::new()), |sample| sample_bytes(&sample)),
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Vec::new()),
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.renegotiate_output()?;
                Ok(Vec::new())
            }
            Err(err) => Err(encode_err("ProcessOutput（异步）", &err)),
        }
    }

    /// 通过 `ICodecAPI` 请求下一帧为 IDR。
    fn request_keyframe(&self) -> Result<(), WinVideoError> {
        let codec = self.codec.as_ref().ok_or_else(|| {
            WinVideoError::Encode("MFT 未暴露 ICodecAPI，无法强制关键帧".to_owned())
        })?;
        // SAFETY: `codec` 有效；只查询能力。
        if let Err(err) = unsafe { codec.IsSupported(&CODECAPI_AVEncVideoForceKeyFrame) } {
            return Err(WinVideoError::Encode(format!(
                "编码器不支持强制关键帧（CODECAPI_AVEncVideoForceKeyFrame）: {err}"
            )));
        }
        set_codec_u32(codec, &CODECAPI_AVEncVideoForceKeyFrame, 1)
            .map_err(|err| WinVideoError::Encode(format!("强制关键帧失败: {err}")))
    }

    /// 把当前 NV12 缓冲包装成带时间戳的输入样本。
    fn build_input_sample(&mut self) -> Result<IMFSample, WinVideoError> {
        let length = u32::try_from(self.nv12.len())
            .map_err(|_| WinVideoError::Encode("NV12 缓冲区超过 4 GiB".to_owned()))?;
        let buffer = mfplat::create_memory_buffer(length)?;
        let mut destination: *mut u8 = ptr::null_mut();
        // SAFETY: `destination` 指向本栈帧；`buffer` 在调用期间有效。
        unsafe { buffer.Lock(&raw mut destination, None, None) }
            .map_err(|err| encode_err("IMFMediaBuffer::Lock（输入）", &err))?;
        if destination.is_null() {
            let _ = unsafe { buffer.Unlock() };
            return Err(WinVideoError::Encode("输入缓冲区指针为空".to_owned()));
        }
        // SAFETY: Lock 保证目标至少可写 `length` 字节；源/目标不重叠。
        unsafe {
            ptr::copy_nonoverlapping(self.nv12.as_ptr(), destination, self.nv12.len());
        }
        // SAFETY: 与上面的 Lock 配对。
        unsafe { buffer.Unlock() }
            .map_err(|err| encode_err("IMFMediaBuffer::Unlock（输入）", &err))?;
        // SAFETY: `buffer` 有效；长度不超过最大值。
        unsafe { buffer.SetCurrentLength(length) }
            .map_err(|err| encode_err("IMFMediaBuffer::SetCurrentLength", &err))?;

        let sample = mfplat::create_sample()?;
        // SAFETY: 两个 COM 对象都在本函数内有效；AddBuffer 增加引用计数。
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(|err| encode_err("IMFSample::AddBuffer", &err))?;
        self.stamp_sample(&sample)?;
        Ok(sample)
    }

    /// 把 GPU 表面缓冲包成带时间戳的输入样本（GPU 路径）。
    fn build_texture_sample(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> Result<IMFSample, WinVideoError> {
        // NV12 是线性可寻址的 2D 子资源，`bottom_up_when_linear = false`。
        let buffer = mfplat::create_dxgi_surface_buffer(texture, 0, false)?;
        let sample = mfplat::create_sample()?;
        // SAFETY: 两个 COM 对象都在本函数内有效；AddBuffer 增加引用计数。
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(|err| encode_err("IMFSample::AddBuffer（GPU 输入）", &err))?;
        self.stamp_sample(&sample)?;
        Ok(sample)
    }

    /// 设置样本时间戳并推进时间轴（CPU/GPU 两条路径共用，保证节奏一致）。
    fn stamp_sample(&mut self, sample: &IMFSample) -> Result<(), WinVideoError> {
        // SAFETY: `sample` 有效；时间戳只影响码流时间轴。
        unsafe {
            sample
                .SetSampleTime(self.next_pts)
                .map_err(|err| encode_err("IMFSample::SetSampleTime", &err))?;
            sample
                .SetSampleDuration(self.frame_duration)
                .map_err(|err| encode_err("IMFSample::SetSampleDuration", &err))?;
        }
        self.next_pts = self.next_pts.saturating_add(self.frame_duration);
        Ok(())
    }

    /// 循环取走 MFT 已产生的全部输出样本，返回拼接后的原始码流。
    fn drain_output(&mut self) -> Result<Vec<u8>, WinVideoError> {
        let mut bytes = Vec::new();
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
            // 指向本栈帧。pSample/pEvents 是 ManuallyDrop，稍后手动取出释放。
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &raw mut status)
            };
            // SAFETY: 无论成功失败都要取出来释放引用计数。
            let produced = unsafe { ManuallyDrop::take(&mut output.pSample) };
            let events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
            drop(events);

            match result {
                Ok(()) => {
                    if let Some(sample) = produced {
                        bytes.extend_from_slice(&sample_bytes(&sample)?);
                    }
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(bytes),
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.renegotiate_output()?;
                }
                Err(err) => return Err(encode_err("ProcessOutput", &err)),
            }
        }
    }

    /// 创建一个符合当前输出流要求的样本。
    fn new_output_sample(&self) -> Result<IMFSample, WinVideoError> {
        let buffer = mfplat::create_memory_buffer(self.output_buffer_size.max(1))?;
        let sample = mfplat::create_sample()?;
        // SAFETY: 两个 COM 对象都在本函数内有效；AddBuffer 增加引用计数。
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(|err| encode_err("IMFSample::AddBuffer（输出）", &err))?;
        Ok(sample)
    }

    /// 处理 `MF_E_TRANSFORM_STREAM_CHANGE`：重新选一个 H.264 输出类型并刷新流信息。
    fn renegotiate_output(&mut self) -> Result<(), WinVideoError> {
        let media_type = select_h264_output_type(&self.transform)?;
        // SAFETY: `media_type` 来自本 MFT 的可用类型列表。
        unsafe { self.transform.SetOutputType(0, &media_type, 0) }
            .map_err(|err| encode_err("SetOutputType（流变更重协商）", &err))?;
        if let Some(header) = sequence_header_from_type(&media_type) {
            self.sequence_header = header;
        }
        let (provides_samples, output_buffer_size) =
            output_stream_info(&self.transform, self.width, self.height)?;
        self.provides_samples = provides_samples;
        self.output_buffer_size = output_buffer_size;
        Ok(())
    }
}

impl Drop for MftH264Encoder {
    fn drop(&mut self) {
        // SAFETY: 只发送停止消息，失败无副作用。
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// 校验编码参数。
fn validate_config(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<(), WinVideoError> {
    if width == 0 || height == 0 {
        return Err(WinVideoError::Encode(format!(
            "编码尺寸不能为 0：{width}x{height}"
        )));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(WinVideoError::Encode(format!(
            "4:2:0 编码要求宽高为偶数，收到 {width}x{height}"
        )));
    }
    if fps == 0 {
        return Err(WinVideoError::Encode("帧率不能为 0".to_owned()));
    }
    if bitrate_kbps == 0 {
        return Err(WinVideoError::Encode("码率不能为 0".to_owned()));
    }
    Ok(())
}

/// 优先 Intel / Quick Sync，其次列表里的第一个。
fn select_preferred(activates: &[IMFActivate]) -> &IMFActivate {
    activates
        .iter()
        .find(|activate| {
            let name = friendly_name(activate).to_lowercase();
            name.contains("intel") || name.contains("quick sync")
        })
        .unwrap_or(&activates[0])
}

/// 判断 MFT 是否是异步的（Intel QSV 是；异步必须走事件模型）。
fn is_async_transform(transform: &IMFTransform) -> Result<bool, WinVideoError> {
    // SAFETY: `transform` 有效；属性存储生命周期由 COM 引用计数维护。
    let attributes = unsafe { transform.GetAttributes() }
        .map_err(|err| encode_err("IMFTransform::GetAttributes", &err))?;
    // SAFETY: `attributes` 有效；`MF_TRANSFORM_ASYNC` 的约定类型是 UINT32。
    let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
    if is_async {
        // 异步 MFT 默认是「锁定」的：不先解锁，连 MFT_MESSAGE_SET_D3D_MANAGER
        // 都会返回 MF_E_TRANSFORM_ASYNC_LOCKED。解锁不等于改成同步调用，
        // 之后仍然要按 METransformNeedInput/HaveOutput 事件模型驱动。
        // SAFETY: `attributes` 有效；必须在任何 ProcessMessage / 设置类型之前。
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .map_err(|err| encode_err("MF_TRANSFORM_ASYNC_UNLOCK", &err))?;
    }
    // 低延迟只是尽力而为：不支持时按默认延迟编码。
    // SAFETY: `attributes` 有效。
    let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };
    Ok(is_async)
}

/// 设置码率控制：优先**峰值受限 VBR**，编码器不支持时回退 CBR。
///
/// # 为什么不是 CBR
///
/// 实测 Intel QSV（1080p@30fps、目标 8 Mbps）在**输入完全静止**时仍输出 21666 字节/帧
/// （≈5.2 Mbps）——CBR 会插入填充数据把码流灌到目标码率。远程桌面一静止，上行就被白占；
/// 改成峰值受限 VBR（均值 = 目标 × [`VBR_MEAN_PERCENT`]，峰值 = 目标）后，静止画面
/// 回落到 ~3 KB/帧，复杂画面仍可打到目标峰值。
///
/// # Errors
/// 连 CBR 的均值码率都设不进去时返回 [`WinVideoError::Encode`]（调用方按「编码器不可信、
/// 重建」处理）。
fn apply_rate_control(codec: &ICodecAPI, bitrate_bps: u32) -> Result<(), WinVideoError> {
    let vbr = u32::try_from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0).unwrap_or(1);
    let mean = bitrate_bps.saturating_mul(VBR_MEAN_PERCENT) / 100;
    // 逐项确认：有些 MFT 只认模式、不认峰值属性；任何一步失败都退回 CBR。
    // SAFETY: `codec` 有效；`IsSupported` 只查询能力。
    let peak_supported = unsafe { codec.IsSupported(&CODECAPI_AVEncCommonMaxBitRate) }.is_ok();
    let vbr_applied = peak_supported
        && set_codec_u32(codec, &CODECAPI_AVEncCommonRateControlMode, vbr).is_ok()
        && set_codec_u32(codec, &CODECAPI_AVEncCommonMaxBitRate, bitrate_bps).is_ok()
        && set_codec_u32(codec, &CODECAPI_AVEncCommonMeanBitRate, mean).is_ok();
    if vbr_applied {
        tracing::debug!(bitrate_bps, mean, "码率控制：峰值受限 VBR");
        return Ok(());
    }

    tracing::debug!("编码器不支持峰值受限 VBR，回退 CBR");
    let cbr = u32::try_from(eAVEncCommonRateControlMode_CBR.0).unwrap_or(0);
    set_codec_u32(codec, &CODECAPI_AVEncCommonRateControlMode, cbr)
        .map_err(|err| WinVideoError::Encode(format!("设置 CBR 失败: {err}")))?;
    set_codec_u32(codec, &CODECAPI_AVEncCommonMeanBitRate, bitrate_bps)
        .map_err(|err| WinVideoError::Encode(format!("设置目标码率失败: {err}")))?;
    Ok(())
}

/// 设置编码器的默认参数（码率控制、低延迟、无 B 帧）；失败只记 debug。
fn configure_codec_defaults(codec: &ICodecAPI, bitrate_bps: u32) {
    if let Err(err) = apply_rate_control(codec, bitrate_bps) {
        tracing::debug!(%err, "编码器拒绝码率控制参数，按 MFT 默认值继续");
    }
    let settings = [
        (&CODECAPI_AVEncCommonLowLatency, 1),
        (&CODECAPI_AVEncMPVDefaultBPictureCount, 0),
    ];
    for (key, value) in settings {
        if let Err(err) = set_codec_u32(codec, key, value) {
            tracing::debug!(%err, "编码器拒绝默认参数，按 MFT 默认值继续");
        }
    }
}

/// 设置输入 NV12 / 输出 H.264 媒体类型。
///
/// 编码器通常要求先设输出再设输入；少数 MFT 相反，失败时交换顺序重试一次。
fn configure_media_types(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<(), WinVideoError> {
    let output = build_output_type(width, height, fps, bitrate_kbps.saturating_mul(1000))?;
    let input = build_input_type(width, height, fps)?;
    // SAFETY: 两个媒体类型与 transform 都有效。
    let output_first = unsafe {
        transform
            .SetOutputType(0, &output, 0)
            .and_then(|()| transform.SetInputType(0, &input, 0))
    };
    if output_first.is_ok() {
        return Ok(());
    }
    // SAFETY: 同上；改用输入优先的顺序重试。
    unsafe {
        transform
            .SetInputType(0, &input, 0)
            .map_err(|err| encode_err("SetInputType(NV12)", &err))?;
        transform
            .SetOutputType(0, &output, 0)
            .map_err(|err| encode_err("SetOutputType(H.264)", &err))?;
    }
    Ok(())
}

/// 构造 H.264 输出媒体类型。
fn build_output_type(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
) -> Result<IMFMediaType, WinVideoError> {
    let media_type = mfplat::create_media_type()?;
    let progressive = u32::try_from(MFVideoInterlace_Progressive.0).unwrap_or_default();
    // SAFETY: `media_type` 有效；GUID 常数都是 `'static`。
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|err| encode_err("输出类型 MF_MT_MAJOR_TYPE", &err))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|err| encode_err("输出类型 MF_MT_SUBTYPE", &err))?;
        media_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)
            .map_err(|err| encode_err("输出类型 MF_MT_AVG_BITRATE", &err))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))
            .map_err(|err| encode_err("输出类型 MF_MT_FRAME_SIZE", &err))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(fps, 1))
            .map_err(|err| encode_err("输出类型 MF_MT_FRAME_RATE", &err))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, progressive)
            .map_err(|err| encode_err("输出类型 MF_MT_INTERLACE_MODE", &err))?;
        media_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))
            .map_err(|err| encode_err("输出类型 MF_MT_PIXEL_ASPECT_RATIO", &err))?;
    }
    Ok(media_type)
}

/// 构造 NV12 输入媒体类型。
fn build_input_type(width: u32, height: u32, fps: u32) -> Result<IMFMediaType, WinVideoError> {
    let media_type = mfplat::create_media_type()?;
    let progressive = u32::try_from(MFVideoInterlace_Progressive.0).unwrap_or_default();
    // SAFETY: `media_type` 有效；GUID 常数都是 `'static`。
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|err| encode_err("输入类型 MF_MT_MAJOR_TYPE", &err))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|err| encode_err("输入类型 MF_MT_SUBTYPE", &err))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))
            .map_err(|err| encode_err("输入类型 MF_MT_FRAME_SIZE", &err))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(fps, 1))
            .map_err(|err| encode_err("输入类型 MF_MT_FRAME_RATE", &err))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, progressive)
            .map_err(|err| encode_err("输入类型 MF_MT_INTERLACE_MODE", &err))?;
        media_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))
            .map_err(|err| encode_err("输入类型 MF_MT_PIXEL_ASPECT_RATIO", &err))?;
    }
    Ok(media_type)
}

/// 从当前输出类型读取 (或枚举) SPS/PPS。
fn read_sequence_header(transform: &IMFTransform) -> Vec<u8> {
    // SAFETY: `transform` 有效；当前输出类型可能不可用，失败时回退 None。
    if let Ok(current) = unsafe { transform.GetOutputCurrentType(0) } {
        if let Some(header) = sequence_header_from_type(&current) {
            return header;
        }
    }
    Vec::new()
}

/// 读取媒体类型上的 `MF_MT_MPEG_SEQUENCE_HEADER`。
fn sequence_header_from_type(media_type: &IMFMediaType) -> Option<Vec<u8>> {
    // SAFETY: `media_type` 有效；blob 大小查询失败表示没有该属性。
    let size = unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.ok()?;
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u8; usize::try_from(size).ok()?];
    let mut actual = 0u32;
    // SAFETY: `buffer` 指向本栈帧且长度与查询值一致。
    unsafe {
        media_type
            .GetBlob(
                &MF_MT_MPEG_SEQUENCE_HEADER,
                &mut buffer,
                Some(&raw mut actual),
            )
            .ok()?;
    }
    buffer.truncate(usize::try_from(actual).ok()?);
    if buffer.is_empty() {
        return None;
    }
    bitstream::normalize_to_annex_b(&buffer).ok()
}

/// 读取输出流信息：是否自供样本 + 最小缓冲区大小。
fn output_stream_info(
    transform: &IMFTransform,
    width: u32,
    height: u32,
) -> Result<(bool, u32), WinVideoError> {
    // SAFETY: `transform` 有效且输出流 0 存在。
    let info = unsafe { transform.GetOutputStreamInfo(0) }
        .map_err(|err| encode_err("GetOutputStreamInfo", &err))?;
    let provides_flag = u32::try_from(MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0).unwrap_or(0);
    let provides_samples = info.dwFlags & provides_flag != 0;
    let raw_frame = width
        .checked_mul(height)
        .ok_or_else(|| WinVideoError::Encode("输出尺寸溢出".to_owned()))?;
    Ok((provides_samples, info.cbSize.max(raw_frame).max(1)))
}

/// 从可用输出类型里挑 H.264。
fn select_h264_output_type(transform: &IMFTransform) -> Result<IMFMediaType, WinVideoError> {
    for index in 0..MAX_OUTPUT_TYPES {
        // SAFETY: `transform` 有效；索引递增直到 `MF_E_NO_MORE_TYPES`。
        let candidate = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(candidate) => candidate,
            Err(err) if err.code() == MF_E_NO_MORE_TYPES => break,
            Err(err) => return Err(encode_err("GetOutputAvailableType", &err)),
        };
        // SAFETY: `candidate` 有效。
        let subtype = unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.ok();
        if subtype == Some(MFVideoFormat_H264) {
            return Ok(candidate);
        }
    }
    // SAFETY: 回退到当前输出类型。
    unsafe { transform.GetOutputCurrentType(0) }
        .map_err(|err| encode_err("GetOutputCurrentType", &err))
}

/// 读取样本的连续字节（编码后的 H.264 码流）。
fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, WinVideoError> {
    // SAFETY: `sample` 有效；ConvertToContiguousBuffer 失败时退回第一个缓冲区。
    let buffer = match unsafe { sample.ConvertToContiguousBuffer() } {
        Ok(buffer) => buffer,
        Err(_) => unsafe { sample.GetBufferByIndex(0) }
            .map_err(|err| encode_err("IMFSample::GetBufferByIndex", &err))?,
    };
    let mut data: *mut u8 = ptr::null_mut();
    let mut length = 0u32;
    // SAFETY: `data` / `length` 指向本栈帧；`buffer` 在解锁前一直有效。
    unsafe { buffer.Lock(&raw mut data, None, Some(&raw mut length)) }
        .map_err(|err| encode_err("IMFMediaBuffer::Lock（输出）", &err))?;
    if data.is_null() {
        let _ = unsafe { buffer.Unlock() };
        return Err(WinVideoError::Encode("编码输出缓冲区为空指针".to_owned()));
    }
    let len = usize::try_from(length).unwrap_or(0);
    // SAFETY: Lock 保证从 `data` 起至少有 `length` 字节可读。
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    // SAFETY: 与上面的 Lock 配对。
    unsafe { buffer.Unlock() }.map_err(|err| encode_err("IMFMediaBuffer::Unlock（输出）", &err))?;
    Ok(bytes)
}

/// 把 `u32` 写进 `ICodecAPI`（VT_UI4）。
fn set_codec_u32(codec: &ICodecAPI, key: &GUID, value: u32) -> windows::core::Result<()> {
    let mut variant = VARIANT::default();
    // SAFETY: VARIANT 已零初始化；先写 vt 再写对应 union 成员，不读取未初始化值。
    // 显式 `*` 解开 ManuallyDrop，避免它自动调用旧值析构。
    unsafe {
        (*variant.Anonymous.Anonymous).vt = VT_UI4;
        (*variant.Anonymous.Anonymous).Anonymous.ulVal = value;
    }
    // SAFETY: `codec` 有效；`variant` 指向本栈帧且类型与 key 的约定一致。
    unsafe { codec.SetValue(key, &raw const variant) }
}

/// 把 `u64` 写进 `ICodecAPI`（VT_UI8）。
fn set_codec_u64(codec: &ICodecAPI, key: &GUID, value: u64) -> windows::core::Result<()> {
    let mut variant = VARIANT::default();
    // SAFETY: VARIANT 已零初始化；先写 vt 再写对应 union 成员。
    // 显式 `*` 解开 ManuallyDrop，避免它自动调用旧值析构。
    unsafe {
        (*variant.Anonymous.Anonymous).vt = VT_UI8;
        (*variant.Anonymous.Anonymous).Anonymous.ullVal = value;
    }
    // SAFETY: `codec` 有效；`variant` 指向本栈帧且类型与 key 的约定一致。
    unsafe { codec.SetValue(key, &raw const variant) }
}

/// `MF_MT_FRAME_SIZE` 的打包：高 32 位宽、低 32 位高。
fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

/// `MF_MT_FRAME_RATE` / `MF_MT_PIXEL_ASPECT_RATIO` 的打包。
fn pack_ratio(numerator: u32, denominator: u32) -> u64 {
    (u64::from(numerator) << 32) | u64::from(denominator)
}

/// 把从 `started` 到现在的耗时取整为毫秒（饱和到 `u32::MAX`）。
fn elapsed_ms(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}

/// 把 HRESULT 包成带上下文的编码错误。
fn encode_err(context: &str, error: &windows::core::Error) -> WinVideoError {
    WinVideoError::Encode(format!("{context} 失败: {error}"))
}

thread_local! {
    /// 当前线程是否已完成 COM 初始化。
    ///
    /// `MFTEnumEx` / `ActivateObject` / `ICodecAPI` 都是 COM 调用，而
    /// client-gui 的工厂探测与编码任务跑在不同线程上，两者都可能未初始化
    /// COM（`CO_E_NOTINITIALIZED`）。每个入口先确保 MTA。
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
            Err(WinVideoError::Encode(format!(
                "CoInitializeEx 失败: {result:?}"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_and_ratio_pack() {
        assert_eq!(pack_size(1920, 1080), 0x0000_0780_0000_0438);
        assert_eq!(pack_ratio(60, 1), 0x0000_003C_0000_0001);
    }

    #[test]
    fn invalid_config_is_rejected() {
        assert!(validate_config(0, 720, 60, 4000).is_err());
        assert!(validate_config(1279, 720, 60, 4000).is_err());
        assert!(validate_config(1280, 720, 0, 4000).is_err());
        assert!(validate_config(1280, 720, 60, 0).is_err());
        assert!(validate_config(1280, 720, 60, 4000).is_ok());
    }
}
