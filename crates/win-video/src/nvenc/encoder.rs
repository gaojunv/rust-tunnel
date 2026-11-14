//! NVENC H.264 4:2:0 编码器：BGRA/RGBA 纹理输入、Annex B 输出。
//!
//! # 数据通路
//!
//! 调用方给的是 **CPU 侧 BGRA 或 RGBA**（通道顺序由 [`InputFormat`] 选择）。
//! 本模块在 NVIDIA 设备上建一块 `B8G8R8A8_UNORM` / `R8G8B8A8_UNORM` 纹理，
//! 每帧 `UpdateSubresource` 上传后用 `NV_ENC_BUFFER_FORMAT_ARGB` /
//! `NV_ENC_BUFFER_FORMAT_ABGR` 注册/映射给 NVENC，由驱动内部完成 RGB→NV12
//! （4:2:0）转换。**不做 CPU 色彩空间或通道转换**。
//!
//! # 输入尺寸与编码尺寸
//!
//! 纹理按 [`EncoderConfig::source_width`] / [`EncoderConfig::source_height`]
//! 创建；编码尺寸由 [`EncoderConfig::width`] / [`EncoderConfig::height`] 决定。
//! 两者不同时把源尺寸填进 `NV_ENC_PIC_PARAMS.inputWidth/Height`。
//!
//! **实测结论（RTX 5070 Ti + 驱动 13.1）**：驱动接受 4K 源 + 1080p 编码，但
//! 输出的是**左上角 1920x1080 裁剪**，不是缩放——两帧仅左上区域之外的内容不同
//! 时，P 帧码流逐字节相同。因此适配层要缩放仍需自行下采样（CPU box 或 GPU）。
//!
//! # 生命周期
//!
//! [`NvencEncoder`] 只借用 [`NvencApi`]（`&'a`），不拥有它；用
//! `OnceLock` + `Box::leak` 拿到 `&'static NvencApi` 即可构造
//! `NvencEncoder<'static>`。
//!
//! # 关键配置
//!
//! - H.264 High profile、**峰值受限 VBR**（均值 = 峰值 × 70%）、`frameIntervalP = 1`
//!   （无 B 帧）、`gopLength = NVENC_INFINITE_GOPLENGTH`（无限 GOP）。
//! - `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`。
//! - 只在调用方要求时用 `NV_ENC_PIC_FLAG_FORCEIDR` 产生 IDR（并同时
//!   `OUTPUT_SPSPPS`，保证任何 IDR 都能独立解码）。
//! - VUI 打 BT.709 + limited range 标签。
//!
//! # 为什么不用重建来实现 `set_bitrate` / `set_fps`
//!
//! 重建编码器会强制一个 IDR。QoS 闭环每秒都可能调整码率，若每次都插 IDR，
//! 就等于每秒制造一次带宽尖峰。因此这里走 `nvEncReconfigureEncoder` 的
//! **在线重配置**路径：`resetEncoder = 0`、`forceIDR = 0`。

use super::{status_err, status_ok, sys, NvencApi, NvencSession};
use crate::d3d11::D3d11;
use crate::WinVideoError;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr;
use windows::core::Interface as _;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};

/// 允许的最大编码宽度（与现有远程控制业务的上限一致）。
pub const MAX_ENCODE_WIDTH: u32 = 1920;
/// 允许的最大编码高度（NVENC H.264 能力上限）。
pub const MAX_ENCODE_HEIGHT: u32 = 4096;
/// 允许的最大输入（源）边长；4K 采集源为 3840x2160。
pub const MAX_SOURCE_DIMENSION: u32 = 4096;

/// 输入像素的通道顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputFormat {
    /// 自顶向下 BGRA（采集侧 WGC/DXGI 原生格式）。
    #[default]
    Bgra,
    /// 自顶向下 RGBA（remote 数据面的 `VideoEncoderApi::encode` 格式）。
    Rgba,
}

impl InputFormat {
    /// 对应的 NVENC 输入缓冲格式。
    #[must_use]
    pub fn buffer_format(self) -> sys::NV_ENC_BUFFER_FORMAT {
        match self {
            Self::Bgra => sys::_NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            Self::Rgba => sys::_NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR,
        }
    }

    /// 对应的 DXGI 纹理格式。
    #[must_use]
    pub fn dxgi_format(self) -> DXGI_FORMAT {
        match self {
            Self::Bgra => DXGI_FORMAT_B8G8R8A8_UNORM,
            Self::Rgba => DXGI_FORMAT_R8G8B8A8_UNORM,
        }
    }

    /// 每个像素的字节数（两种格式都是 4）。
    #[must_use]
    pub const fn bytes_per_pixel(self) -> u32 {
        4
    }
}

/// NVENC 编码器参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// 编码宽度（像素，必须为偶数）。
    pub width: u32,
    /// 编码高度（像素，必须为偶数）。
    pub height: u32,
    /// 目标帧率（帧/秒）。
    pub fps: u32,
    /// 目标码率（kbps）。
    pub bitrate_kbps: u32,
    /// 输入纹理的通道顺序，默认 [`InputFormat::Bgra`]。
    pub input_format: InputFormat,
    /// 输入（源）宽度；默认等于 `width`。用于让 NVENC 尝试输入缩放。
    pub source_width: u32,
    /// 输入（源）高度；默认等于 `height`。
    pub source_height: u32,
}

impl EncoderConfig {
    /// 构造参数并做基本校验（BGRA 输入、源尺寸 = 编码尺寸）。
    ///
    /// # Errors
    /// 尺寸越界/为奇数、帧率为 0 或码率为 0 时返回 [`WinVideoError::Config`]。
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self, WinVideoError> {
        let config = Self {
            width,
            height,
            fps,
            bitrate_kbps,
            input_format: InputFormat::Bgra,
            source_width: width,
            source_height: height,
        };
        config.validate()?;
        Ok(config)
    }

    /// 指定输入通道顺序。
    #[must_use]
    pub fn with_input_format(mut self, format: InputFormat) -> Self {
        self.input_format = format;
        self
    }

    /// 指定输入（源）尺寸，与编码尺寸不同时交给 NVENC 尝试缩放。
    #[must_use]
    pub fn with_source_size(mut self, width: u32, height: u32) -> Self {
        self.source_width = width;
        self.source_height = height;
        self
    }

    /// 校验尺寸与码率参数。
    fn validate(self) -> Result<(), WinVideoError> {
        if self.width == 0 || self.height == 0 {
            return Err(WinVideoError::Config("编码尺寸不能为 0".to_owned()));
        }
        if self.width > MAX_ENCODE_WIDTH {
            return Err(WinVideoError::Config(format!(
                "编码宽度 {} 超过上限 {MAX_ENCODE_WIDTH}",
                self.width
            )));
        }
        if self.height > MAX_ENCODE_HEIGHT {
            return Err(WinVideoError::Config(format!(
                "编码高度 {} 超过上限 {MAX_ENCODE_HEIGHT}",
                self.height
            )));
        }
        // 4:2:0 输出要求宽高都是偶数。
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(WinVideoError::Config(format!(
                "4:2:0 编码要求宽高为偶数，收到 {}x{}",
                self.width, self.height
            )));
        }
        if self.source_width == 0 || self.source_height == 0 {
            return Err(WinVideoError::Config("输入尺寸不能为 0".to_owned()));
        }
        if self.source_width > MAX_SOURCE_DIMENSION || self.source_height > MAX_SOURCE_DIMENSION {
            return Err(WinVideoError::Config(format!(
                "输入尺寸 {}x{} 超过上限 {MAX_SOURCE_DIMENSION}",
                self.source_width, self.source_height
            )));
        }
        if !self.source_width.is_multiple_of(2) || !self.source_height.is_multiple_of(2) {
            return Err(WinVideoError::Config(format!(
                "输入尺寸必须为偶数，收到 {}x{}",
                self.source_width, self.source_height
            )));
        }
        if self.fps == 0 {
            return Err(WinVideoError::Config("帧率不能为 0".to_owned()));
        }
        if self.bitrate_kbps == 0 {
            return Err(WinVideoError::Config("码率不能为 0".to_owned()));
        }
        Ok(())
    }
}

/// 一帧编码输出：4 字节大端长度前缀 + Annex B 字节流。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPacket {
    /// 已带 4 字节大端长度前缀的 Annex B 码流。
    pub data: Vec<u8>,
    /// 本帧是否是关键帧（IDR / I / intra refresh）。
    pub keyframe: bool,
}

impl EncodedPacket {
    /// 码流字节数（不含 4 字节长度前缀）。
    #[must_use]
    pub fn payload_len(&self) -> usize {
        self.data.len().saturating_sub(4)
    }
}

fn check(status: sys::NVENCSTATUS, context: &str) -> Result<(), WinVideoError> {
    if status_ok(status) {
        Ok(())
    } else {
        Err(status_err(context, status))
    }
}

/// 把 kbps 换算成 NVENC 需要的 bps（饱和，避免溢出）。
fn kbps_to_bps(kbps: u32) -> u32 {
    kbps.saturating_mul(1000)
}

/// `NV_ENC_RC_PARAMS` 的 VBV 缓冲大小：半秒码率，至少 1 位。
fn vbv_size(bps: u32) -> u32 {
    (bps / 2).max(1)
}

/// 峰值受限 VBR 的均值码率（峰值的 70%，与 MFT 路径保持一致）。
fn vbr_average(peak_bps: u32) -> u32 {
    (peak_bps.saturating_mul(70) / 100).max(1)
}

fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: InputFormat,
) -> Result<ID3D11Texture2D, WinVideoError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format.dxgi_format(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // 与 NVIDIA 示例一致：输入纹理作为 render target 创建（仅表示用途，
        // 我们不真的渲染）。`UpdateSubresource` 对 DEFAULT 纹理有效。
        BindFlags: u32::try_from(D3D11_BIND_RENDER_TARGET.0).unwrap_or_default(),
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` 与输出槽都在本栈帧内有效；设备由调用方持有且存活。
    unsafe {
        device.CreateTexture2D(
            &raw const desc,
            None::<*const D3D11_SUBRESOURCE_DATA>,
            Some(&raw mut texture),
        )
    }?;
    texture.ok_or_else(|| WinVideoError::Nvenc("D3D11 返回空输入纹理".to_owned()))
}

fn register_input(
    session: &NvencSession<'_>,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    format: InputFormat,
) -> Result<sys::NV_ENC_REGISTERED_PTR, WinVideoError> {
    let register = session
        .api()
        .fns
        .nvEncRegisterResource
        .ok_or_else(|| WinVideoError::Nvenc("驱动未导出 nvEncRegisterResource".to_owned()))?;

    // SAFETY: POD；`texture` 在会话与注册资源存续期间一直存活（由本结构体持有）。
    let mut params: sys::NV_ENC_REGISTER_RESOURCE = unsafe { std::mem::zeroed() };
    params.version = sys::NV_ENC_REGISTER_RESOURCE_VER;
    params.resourceType = sys::_NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    params.width = width;
    params.height = height;
    params.pitch = 0;
    params.subResourceIndex = 0;
    params.resourceToRegister = texture.as_raw();
    params.bufferFormat = format.buffer_format();
    params.bufferUsage = sys::_NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;

    // SAFETY: 会话句柄有效；params 指向本栈帧，D3D11 纹理指针由 `texture` 持有。
    let status = unsafe { register(session.handle(), &raw mut params) };
    check(status, "nvEncRegisterResource")?;
    if params.registeredResource.is_null() {
        return Err(WinVideoError::Nvenc(
            "nvEncRegisterResource 返回空注册句柄".to_owned(),
        ));
    }
    Ok(params.registeredResource)
}

fn map_input(
    session: &NvencSession<'_>,
    registered: sys::NV_ENC_REGISTERED_PTR,
) -> Result<(sys::NV_ENC_INPUT_PTR, sys::NV_ENC_BUFFER_FORMAT), WinVideoError> {
    let map = session
        .api()
        .fns
        .nvEncMapInputResource
        .ok_or_else(|| WinVideoError::Nvenc("驱动未导出 nvEncMapInputResource".to_owned()))?;

    // SAFETY: POD；registered 由 nvEncRegisterResource 产生且尚未注销。
    let mut params: sys::NV_ENC_MAP_INPUT_RESOURCE = unsafe { std::mem::zeroed() };
    params.version = sys::NV_ENC_MAP_INPUT_RESOURCE_VER;
    params.registeredResource = registered;

    // SAFETY: 会话句柄有效；params 指向本栈帧。
    let status = unsafe { map(session.handle(), &raw mut params) };
    check(status, "nvEncMapInputResource")?;
    if params.mappedResource.is_null() {
        return Err(WinVideoError::Nvenc(
            "nvEncMapInputResource 返回空映射句柄".to_owned(),
        ));
    }
    Ok((params.mappedResource, params.mappedBufferFmt))
}

fn create_bitstream(session: &NvencSession<'_>) -> Result<sys::NV_ENC_OUTPUT_PTR, WinVideoError> {
    let create = session
        .api()
        .fns
        .nvEncCreateBitstreamBuffer
        .ok_or_else(|| WinVideoError::Nvenc("驱动未导出 nvEncCreateBitstreamBuffer".to_owned()))?;

    // SAFETY: POD；输出句柄写入本栈帧。
    let mut params: sys::NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { std::mem::zeroed() };
    params.version = sys::NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    params.size = 0;
    params.memoryHeap = sys::_NV_ENC_MEMORY_HEAP::NV_ENC_MEMORY_HEAP_AUTOSELECT;

    // SAFETY: 会话句柄有效；params 指向本栈帧。
    let status = unsafe { create(session.handle(), &raw mut params) };
    check(status, "nvEncCreateBitstreamBuffer")?;
    if params.bitstreamBuffer.is_null() {
        return Err(WinVideoError::Nvenc(
            "nvEncCreateBitstreamBuffer 返回空 bitstream".to_owned(),
        ));
    }
    Ok(params.bitstreamBuffer)
}

fn destroy_bitstream(session: &NvencSession<'_>, bitstream: sys::NV_ENC_OUTPUT_PTR) {
    if let Some(destroy) = session.api().fns.nvEncDestroyBitstreamBuffer {
        // SAFETY: bitstream 由本模块创建且只销毁一次；会话仍存活。
        unsafe { destroy(session.handle(), bitstream) };
    }
}

fn unmap_input(session: &NvencSession<'_>, mapped: sys::NV_ENC_INPUT_PTR) {
    if let Some(unmap) = session.api().fns.nvEncUnmapInputResource {
        // SAFETY: mapped 由本模块映射且只注销一次；会话仍存活。
        unsafe { unmap(session.handle(), mapped) };
    }
}

fn unregister_input(session: &NvencSession<'_>, registered: sys::NV_ENC_REGISTERED_PTR) {
    if let Some(unregister) = session.api().fns.nvEncUnregisterResource {
        // SAFETY: registered 由本模块注册且只注销一次；会话仍存活。
        unsafe { unregister(session.handle(), registered) };
    }
}

/// 从驱动取 H.264 的 preset 默认配置，再覆盖成本模块要求的固定参数。
fn build_encode_config(
    session: &NvencSession<'_>,
    bitrate_bps: u32,
) -> Result<sys::NV_ENC_CONFIG, WinVideoError> {
    let get_preset = session
        .api()
        .fns
        .nvEncGetEncodePresetConfigEx
        .ok_or_else(|| {
            WinVideoError::Nvenc("驱动未导出 nvEncGetEncodePresetConfigEx".to_owned())
        })?;

    // 不能在 Rust 里直接 `zeroed::<NV_ENC_PRESET_CONFIG>()`：其内嵌
    // `NV_ENC_CONFIG.frameFieldMode` 是 `NV_ENC_PARAMS_FRAME_FIELD_MODE`，该枚举
    // 从 1 开始，0 不是合法判别值（`mem::zeroed` 会做有效性检查并 panic）。
    // 做法：在 `MaybeUninit` 里清零，先用裸指针把该字段写成合法值，再
    // `assume_init`——此时所有字节已初始化、所有枚举都是合法值。
    let mut preset = MaybeUninit::<sys::NV_ENC_PRESET_CONFIG>::zeroed();
    let preset_ptr = preset.as_mut_ptr();
    // SAFETY: 只通过裸指针写字段，不读取尚未完成初始化的值。
    unsafe {
        (*preset_ptr).presetCfg.frameFieldMode =
            sys::_NV_ENC_PARAMS_FRAME_FIELD_MODE::NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
    }
    // SAFETY: 见上：零值覆盖了全部字节，唯一的非法枚举字段已修正。
    let mut preset = unsafe { preset.assume_init() };
    preset.version = sys::NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = sys::NV_ENC_CONFIG_VER;

    // SAFETY: 会话句柄有效；preset 指向本栈帧，调用期间不会被移动。
    let status = unsafe {
        get_preset(
            session.handle(),
            sys::NV_ENC_CODEC_H264_GUID,
            sys::NV_ENC_PRESET_P4_GUID,
            sys::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            &raw mut preset,
        )
    };
    check(status, "nvEncGetEncodePresetConfigEx")?;

    let mut config = preset.presetCfg;
    config.version = sys::NV_ENC_CONFIG_VER;
    config.profileGUID = sys::NV_ENC_H264_PROFILE_HIGH_GUID;
    // 无限 GOP：只有显式 FORCEIDR 才产生关键帧。
    config.gopLength = sys::NVENC_INFINITE_GOPLENGTH;
    // 1 = 只有 P 帧（无 B 帧）。
    config.frameIntervalP = 1;
    // 峰值受限 VBR：`averageBitRate` 是均值、`maxBitRate` 是峰值。远程桌面大部分时间
    // 是静止/低变化画面，CBR 会把码率灌满目标值（白白占用上行）；VBR 让简单画面只花
    // 需要的码率，复杂画面仍可打到峰值。取值与 MFT 路径的 `VBR_MEAN_PERCENT` 一致。
    config.rcParams.rateControlMode = sys::_NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
    config.rcParams.averageBitRate = vbr_average(bitrate_bps);
    config.rcParams.maxBitRate = bitrate_bps;
    config.rcParams.vbvBufferSize = vbv_size(bitrate_bps);
    config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
    config.rcParams.multiPass = sys::_NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;

    // SAFETY: `encodeCodecConfig` 是 union；本会话固定走 H.264 分支，且 union
    // 已由 preset 配置按 H.264 初始化。
    let h264 = unsafe { &mut config.encodeCodecConfig.h264Config };
    // 0 == NV_ENC_LEVEL_AUTOSELECT（该字段在绑定里是裸 u32）。
    h264.level = 0;
    h264.idrPeriod = sys::NVENC_INFINITE_GOPLENGTH;
    // 1 = 4:2:0。
    h264.chromaFormatIDC = 1;
    h264.entropyCodingMode =
        sys::_NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
    h264.sliceMode = 0;
    h264.sliceModeData = 0;
    // VUI：BT.709 + limited range。调用方（remote）据此按同一套矩阵解码。
    h264.h264VUIParameters.videoSignalTypePresentFlag = 1;
    h264.h264VUIParameters.videoFormat =
        sys::_NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    h264.h264VUIParameters.videoFullRangeFlag = 0;
    h264.h264VUIParameters.colourDescriptionPresentFlag = 1;
    h264.h264VUIParameters.colourPrimaries =
        sys::_NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    h264.h264VUIParameters.transferCharacteristics =
        sys::_NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    h264.h264VUIParameters.colourMatrix =
        sys::_NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;

    Ok(config)
}

/// 一个可复用的 NVENC H.264 编码器。
///
/// 生命周期绑定到 [`NvencApi`]：函数表必须比会话活得久。
pub struct NvencEncoder<'a> {
    session: NvencSession<'a>,
    /// 当前生效配置（pre-set 默认值 + 本模块覆盖）。重配置时驱动会读它。
    config: sys::NV_ENC_CONFIG,
    /// 上一次 `nvEncInitializeEncoder` 的参数副本，`set_*` 时作为重配置基底。
    init: sys::NV_ENC_INITIALIZE_PARAMS,
    bitstream: sys::NV_ENC_OUTPUT_PTR,
    registered: sys::NV_ENC_REGISTERED_PTR,
    mapped: sys::NV_ENC_INPUT_PTR,
    mapped_format: sys::NV_ENC_BUFFER_FORMAT,
    /// 只为保持设备存活（纹理与上下文都源自它）；本模块不直接读它。
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    input_format: InputFormat,
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
    fps: u32,
    bitrate_bps: u32,
    frame_idx: u32,
}

// SAFETY: `NvencEncoder` 独占持有以下资源，且**不实现 `Sync`**，因此同一时刻只有
// 一个线程能访问：
// - NVENC 会话/输入/输出句柄没有线程亲和性；驱动允许在同一会话上串行调用，
//   这里的 API 都取 `&mut self`（查询类取 `&self` 但只读）。
// - D3D11 立即上下文（`ID3D11DeviceContext`）不是线程安全的，但本类型只会在
//   持有独占引用的那个线程上调用它；移动所有权后旧线程不再访问。
// - `&NvencApi` 指向只读的函数表，且 `libloading::Library` 的句柄是进程级的。
// 结论：把整个编码器（含设备/上下文/纹理/会话）整体移交给另一个线程是安全的。
unsafe impl Send for NvencEncoder<'_> {}

impl std::fmt::Debug for NvencEncoder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvencEncoder")
            .field("size", &(self.width, self.height))
            .field("source_size", &(self.source_width, self.source_height))
            .field("input_format", &self.input_format)
            .field("fps", &self.fps)
            .field("bitrate_bps", &self.bitrate_bps)
            .field("frame_idx", &self.frame_idx)
            .finish_non_exhaustive()
    }
}

impl<'a> NvencEncoder<'a> {
    /// 在 NVIDIA D3D11 设备上打开一个 H.264 4:2:0 编码会话并准备好输入纹理。
    ///
    /// # Errors
    /// 参数越界、驱动拒绝会话/初始化、D3D11 纹理创建或资源注册失败时返回
    /// [`WinVideoError`]。
    pub fn new(
        api: &'a NvencApi,
        d3d: &D3d11,
        config: EncoderConfig,
    ) -> Result<Self, WinVideoError> {
        config.validate()?;

        let session = api.open_session(d3d)?;
        let bitrate_bps = kbps_to_bps(config.bitrate_kbps);
        let encode_config = build_encode_config(&session, bitrate_bps)?;

        let device = d3d.device.clone();
        let context = d3d.context.clone();
        let texture = create_texture(
            &device,
            config.source_width,
            config.source_height,
            config.input_format,
        )?;

        // 初始化参数放在配置之后构造，保证 encodeConfig 指针指向本地副本。
        let mut stored_config = encode_config;
        // SAFETY: `NV_ENC_INITIALIZE_PARAMS` 全零是合法状态（其枚举字段都有 0 值），
        // 随后逐字段赋值。
        let mut init: sys::NV_ENC_INITIALIZE_PARAMS = unsafe { std::mem::zeroed() };
        init.version = sys::NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = sys::NV_ENC_CODEC_H264_GUID;
        init.presetGUID = sys::NV_ENC_PRESET_P4_GUID;
        init.encodeWidth = config.width;
        init.encodeHeight = config.height;
        init.darWidth = config.width;
        init.darHeight = config.height;
        init.frameRateNum = config.fps;
        init.frameRateDen = 1;
        init.enableEncodeAsync = 0;
        init.enablePTD = 1;
        init.encodeConfig = &raw mut stored_config;
        init.maxEncodeWidth = config.width;
        init.maxEncodeHeight = config.height;
        init.tuningInfo = sys::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        init.bufferFormat = config.input_format.buffer_format();

        // 顺序与 NVIDIA 示例一致：先初始化编码器，再注册/映射输入、创建码流缓冲。
        // （bitstream buffer 在初始化之前创建会得到 NV_ENC_ERR_DEVICE_NOT_EXIST。）
        let initialize =
            session.api().fns.nvEncInitializeEncoder.ok_or_else(|| {
                WinVideoError::Nvenc("驱动未导出 nvEncInitializeEncoder".to_owned())
            })?;
        // SAFETY: 会话句柄有效；init 指向本栈帧，encodeConfig 指向本地 config。
        let status = unsafe { initialize(session.handle(), &raw mut init) };
        check(status, "nvEncInitializeEncoder")?;
        // `stored_config` 即将移入结构体，先清掉指向栈上旧地址的指针；
        // 每次重配置都会重新绑定到 `self.config`。
        init.encodeConfig = ptr::null_mut();

        let registered = register_input(
            &session,
            &texture,
            config.source_width,
            config.source_height,
            config.input_format,
        )?;
        let (mapped, mapped_format) = match map_input(&session, registered) {
            Ok(mapped) => mapped,
            Err(err) => {
                unregister_input(&session, registered);
                return Err(err);
            }
        };
        let bitstream = match create_bitstream(&session) {
            Ok(bitstream) => bitstream,
            Err(err) => {
                unmap_input(&session, mapped);
                unregister_input(&session, registered);
                return Err(err);
            }
        };

        Ok(Self {
            session,
            config: stored_config,
            init,
            bitstream,
            registered,
            mapped,
            mapped_format,
            _device: device,
            context,
            texture,
            input_format: config.input_format,
            width: config.width,
            height: config.height,
            source_width: config.source_width,
            source_height: config.source_height,
            fps: config.fps,
            bitrate_bps,
            frame_idx: 0,
        })
    }

    /// 编码尺寸 `(width, height)`。
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 输入（源）尺寸 `(width, height)`。
    #[must_use]
    pub fn source_size(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    /// 输入像素的通道顺序。
    #[must_use]
    pub fn input_format(&self) -> InputFormat {
        self.input_format
    }

    /// 当前目标帧率。
    #[must_use]
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// 当前目标码率（bps）。
    #[must_use]
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    fn check_input_format(&self, expected: InputFormat) -> Result<(), WinVideoError> {
        if self.input_format == expected {
            Ok(())
        } else {
            Err(WinVideoError::Config(format!(
                "编码器配置为 {:?} 输入，无法上传 {:?} 数据",
                self.input_format, expected
            )))
        }
    }

    /// 把一帧 **自顶向下像素** 上传到输入纹理。
    ///
    /// 字节顺序必须与 [`EncoderConfig::input_format`] 一致（`Bgra` 或 `Rgba`）。
    /// `pixels.len()` 必须至少为 `source_width * source_height * 4`。
    ///
    /// # Errors
    /// 数据长度不足或 D3D11 上传失败时返回 [`WinVideoError`]。
    pub fn upload_pixels(&self, pixels: &[u8]) -> Result<(), WinVideoError> {
        let row_pitch = self
            .source_width
            .checked_mul(self.input_format.bytes_per_pixel())
            .ok_or_else(|| WinVideoError::Config("行字节数溢出".to_owned()))?;
        let height = usize::try_from(self.source_height)
            .map_err(|_| WinVideoError::Config("输入高度无法转为 usize".to_owned()))?;
        let needed = usize::try_from(row_pitch)
            .ok()
            .and_then(|pitch| pitch.checked_mul(height))
            .ok_or_else(|| WinVideoError::Config("帧字节数溢出".to_owned()))?;
        if pixels.len() < needed {
            return Err(WinVideoError::Config(format!(
                "{:?} 数据不足：需要 {needed} 字节，收到 {}",
                self.input_format,
                pixels.len()
            )));
        }
        // SAFETY: 源缓冲区由调用方保证至少 needed 字节；目标纹理是本结构体持有
        // 的同设备 DEFAULT 纹理。`D3D11` 的立即上下文保证上一条命令完成后才读源。
        unsafe {
            self.context.UpdateSubresource(
                &self.texture,
                0,
                None,
                pixels.as_ptr().cast::<c_void>(),
                row_pitch,
                0,
            );
        }
        Ok(())
    }

    /// 把一帧 **自顶向下 BGRA** 上传到输入纹理（要求编码器配置为 [`InputFormat::Bgra`]）。
    ///
    /// # Errors
    /// 输入格式不匹配、数据长度不足或 D3D11 上传失败时返回 [`WinVideoError`]。
    pub fn upload_bgra(&self, bgra: &[u8]) -> Result<(), WinVideoError> {
        self.check_input_format(InputFormat::Bgra)?;
        self.upload_pixels(bgra)
    }

    /// 把一帧 **自顶向下 RGBA** 上传到输入纹理（要求编码器配置为 [`InputFormat::Rgba`]）。
    ///
    /// # Errors
    /// 输入格式不匹配、数据长度不足或 D3D11 上传失败时返回 [`WinVideoError`]。
    pub fn upload_rgba(&self, rgba: &[u8]) -> Result<(), WinVideoError> {
        self.check_input_format(InputFormat::Rgba)?;
        self.upload_pixels(rgba)
    }

    /// 编码当前输入纹理中的一帧。
    ///
    /// `force_idr == true` 时请求一个 IDR（并带 SPS/PPS）；否则由编码器按
    /// 无限 GOP 继续出 P 帧。
    ///
    /// # Errors
    /// NVENC 编码/锁码流失败时返回 [`WinVideoError`]。
    pub fn encode(&mut self, force_idr: bool) -> Result<EncodedPacket, WinVideoError> {
        let encode_picture = self
            .session
            .api()
            .fns
            .nvEncEncodePicture
            .ok_or_else(|| WinVideoError::Nvenc("驱动未导出 nvEncEncodePicture".to_owned()))?;

        // `NV_ENC_PIC_PARAMS.pictureStruct` 的枚举从 1 开始（0 非法），故不能
        // 直接 `zeroed()`；留在 `MaybeUninit` 里用裸指针逐字段写入，并在 FFI
        // 调用前把 `pictureStruct` 设成合法值。编码后不再读该结构体。
        let mut pic = MaybeUninit::<sys::NV_ENC_PIC_PARAMS>::zeroed();
        let pic_ptr = pic.as_mut_ptr();
        let flags = if force_idr {
            pic_flag(sys::_NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR)
                | pic_flag(sys::_NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS)
        } else {
            0
        };
        // SAFETY: pic_ptr 指向本栈帧内已清零且大小正确的存储；逐字段写入不会
        // 读取未初始化值，`pictureStruct` 在调用前已是合法判别值。
        unsafe {
            (*pic_ptr).version = sys::NV_ENC_PIC_PARAMS_VER;
            // 输入尺寸可以大于编码尺寸：NVENC 会尝试自行缩放（是否支持见探针）。
            (*pic_ptr).inputWidth = self.source_width;
            (*pic_ptr).inputHeight = self.source_height;
            (*pic_ptr).inputPitch = 0;
            (*pic_ptr).encodePicFlags = flags;
            (*pic_ptr).frameIdx = self.frame_idx;
            (*pic_ptr).inputTimeStamp = u64::from(self.frame_idx);
            (*pic_ptr).inputDuration = 1;
            (*pic_ptr).inputBuffer = self.mapped;
            (*pic_ptr).outputBitstream = self.bitstream;
            (*pic_ptr).bufferFmt = self.mapped_format;
            (*pic_ptr).pictureStruct = sys::_NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
        }

        // SAFETY: 会话句柄有效；输入缓冲来自本编码器的映射资源，输出缓冲由本
        // 编码器创建；参数指向本栈帧且字段已完整初始化。
        let status = unsafe { encode_picture(self.session.handle(), pic_ptr) };
        check(status, "nvEncEncodePicture")?;
        self.frame_idx = self.frame_idx.wrapping_add(1);

        self.lock_bitstream()
    }

    /// 上传像素并编码一帧（便捷组合，字节顺序由 `input_format` 决定）。
    ///
    /// # Errors
    /// 上传或编码失败时返回 [`WinVideoError`]。
    pub fn encode_pixels(
        &mut self,
        pixels: &[u8],
        force_idr: bool,
    ) -> Result<EncodedPacket, WinVideoError> {
        self.upload_pixels(pixels)?;
        self.encode(force_idr)
    }

    /// 上传 BGRA 并编码一帧（便捷组合）。
    ///
    /// # Errors
    /// 输入格式不匹配、上传或编码失败时返回 [`WinVideoError`]。
    pub fn encode_bgra(
        &mut self,
        bgra: &[u8],
        force_idr: bool,
    ) -> Result<EncodedPacket, WinVideoError> {
        self.check_input_format(InputFormat::Bgra)?;
        self.encode_pixels(bgra, force_idr)
    }

    /// 上传 RGBA 并编码一帧（便捷组合）。
    ///
    /// # Errors
    /// 输入格式不匹配、上传或编码失败时返回 [`WinVideoError`]。
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        force_idr: bool,
    ) -> Result<EncodedPacket, WinVideoError> {
        self.check_input_format(InputFormat::Rgba)?;
        self.encode_pixels(rgba, force_idr)
    }

    fn lock_bitstream(&self) -> Result<EncodedPacket, WinVideoError> {
        let lock = self
            .session
            .api()
            .fns
            .nvEncLockBitstream
            .ok_or_else(|| WinVideoError::Nvenc("驱动未导出 nvEncLockBitstream".to_owned()))?;
        let unlock =
            self.session.api().fns.nvEncUnlockBitstream.ok_or_else(|| {
                WinVideoError::Nvenc("驱动未导出 nvEncUnlockBitstream".to_owned())
            })?;

        // 同 `NV_ENC_PIC_PARAMS`：`pictureStruct` 零值非法，先在 `MaybeUninit`
        // 里清零并修正该字段；驱动随后会覆盖它。
        let mut params = MaybeUninit::<sys::NV_ENC_LOCK_BITSTREAM>::zeroed();
        let params_ptr = params.as_mut_ptr();
        // SAFETY: 只通过裸指针写字段；`pictureStruct` 在调用前写成合法值。
        unsafe {
            (*params_ptr).version = sys::NV_ENC_LOCK_BITSTREAM_VER;
            (*params_ptr).outputBitstream = self.bitstream;
            (*params_ptr).pictureStruct = sys::_NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
        }

        // SAFETY: 会话句柄有效；params 指向本栈帧，字段已完整初始化。
        let status = unsafe { lock(self.session.handle(), params_ptr) };
        check(status, "nvEncLockBitstream")?;

        // SAFETY: 锁成功意味着驱动已填充结构体；`zeroed()` 也保证了所有字节
        // 在构造时就已初始化。
        let params = unsafe { params.assume_init() };
        let result = Self::copy_locked(&params);
        // 无论拷贝是否成功都必须解锁，否则下一帧无法锁码流。
        // SAFETY: 刚刚成功锁定的同一 bitstream，且 params 未再被使用。
        unsafe { unlock(self.session.handle(), self.bitstream) };
        result
    }

    fn copy_locked(params: &sys::NV_ENC_LOCK_BITSTREAM) -> Result<EncodedPacket, WinVideoError> {
        let size = params.bitstreamSizeInBytes;
        if size == 0 {
            return Err(WinVideoError::Nvenc("NVENC 返回空码流".to_owned()));
        }
        if params.bitstreamBufferPtr.is_null() {
            return Err(WinVideoError::Nvenc("NVENC 返回空码流指针".to_owned()));
        }
        let len = usize::try_from(size)
            .map_err(|_| WinVideoError::Nvenc("码流长度无法转为 usize".to_owned()))?;
        // SAFETY: 驱动承诺 `bitstreamBufferPtr` 指向 `bitstreamSizeInBytes` 字节
        // 的有效内存，且解锁前保持有效。
        let bytes =
            unsafe { std::slice::from_raw_parts(params.bitstreamBufferPtr.cast::<u8>(), len) };
        let keyframe = matches!(
            params.pictureType,
            sys::_NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR
                | sys::_NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
                | sys::_NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_INTRA_REFRESH
        );
        let mut data = Vec::with_capacity(4 + len);
        data.extend_from_slice(&size.to_be_bytes());
        data.extend_from_slice(bytes);
        Ok(EncodedPacket { data, keyframe })
    }

    /// 在线修改目标码率，**不重建编码器、不插 IDR**。
    ///
    /// `kbps` 是**峰值**；均值按 [`vbr_average`] 取峰值的 70%（峰值受限 VBR）。
    ///
    /// # Errors
    /// `kbps == 0` 或驱动拒绝重配置时返回 [`WinVideoError`]。
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), WinVideoError> {
        if kbps == 0 {
            return Err(WinVideoError::Config("码率不能为 0".to_owned()));
        }
        let bps = kbps_to_bps(kbps);
        self.config.rcParams.averageBitRate = vbr_average(bps);
        self.config.rcParams.maxBitRate = bps;
        self.config.rcParams.vbvBufferSize = vbv_size(bps);
        self.config.rcParams.vbvInitialDelay = self.config.rcParams.vbvBufferSize;
        self.reconfigure()?;
        self.bitrate_bps = bps;
        Ok(())
    }

    /// 在线修改目标帧率，**不重建编码器、不插 IDR**。
    ///
    /// # Errors
    /// `fps == 0` 或驱动拒绝重配置时返回 [`WinVideoError`]。
    pub fn set_fps(&mut self, fps: u32) -> Result<(), WinVideoError> {
        if fps == 0 {
            return Err(WinVideoError::Config("帧率不能为 0".to_owned()));
        }
        self.init.frameRateNum = fps;
        self.reconfigure()?;
        self.fps = fps;
        Ok(())
    }

    fn reconfigure(&mut self) -> Result<(), WinVideoError> {
        let reconfigure = self
            .session
            .api()
            .fns
            .nvEncReconfigureEncoder
            .ok_or_else(|| WinVideoError::Nvenc("驱动未导出 nvEncReconfigureEncoder".to_owned()))?;

        let mut params: sys::NV_ENC_RECONFIGURE_PARAMS = unsafe { std::mem::zeroed() };
        params.version = sys::NV_ENC_RECONFIGURE_PARAMS_VER;
        params.reInitEncodeParams = self.init;
        params.reInitEncodeParams.encodeConfig = &raw mut self.config;
        // 关键：不复位、不强制 IDR——QoS 闭环每秒改一次，插 IDR 会制造带宽尖峰。
        params.set_resetEncoder(0);
        params.set_forceIDR(0);

        // SAFETY: 会话句柄有效；params 指向本栈帧，encodeConfig 指向结构体内部
        // 的稳定字段。
        let status = unsafe { reconfigure(self.session.handle(), &raw mut params) };
        check(status, "nvEncReconfigureEncoder")
    }
}

impl Drop for NvencEncoder<'_> {
    fn drop(&mut self) {
        unmap_input(&self.session, self.mapped);
        self.mapped = ptr::null_mut();
        unregister_input(&self.session, self.registered);
        self.registered = ptr::null_mut();
        destroy_bitstream(&self.session, self.bitstream);
        self.bitstream = ptr::null_mut();
        // 纹理 / 设备 / 上下文随字段析构；编码器句柄由 `NvencSession::drop` 销毁。
    }
}

/// 把 NVENC 的 pic flag 枚举转成 `u32` 位掩码。
#[expect(
    clippy::cast_sign_loss,
    reason = "NVENC pic flag 枚举值均为非负的位掩码"
)]
fn pic_flag(flag: sys::_NV_ENC_PIC_FLAGS) -> u32 {
    flag as i32 as u32
}

#[cfg(test)]
mod tests {
    use super::vbr_average;

    #[test]
    fn vbr_average_is_seventy_percent_of_peak() {
        assert_eq!(vbr_average(10_000_000), 7_000_000);
        assert_eq!(vbr_average(2_000_000), 1_400_000);
        // 均值必须严格小于峰值，否则 VBR 退化成 CBR（远程桌面静止时仍灌满上行）。
        for peak in [2_000_000, 6_000_000, 10_000_000] {
            assert!(vbr_average(peak) < peak);
        }
        // 极小码率也不能算出 0：NVENC 会拒绝 0 码率。
        assert_eq!(vbr_average(1), 1);
        assert_eq!(vbr_average(0), 1);
        // 极大值走饱和乘法，不 panic。
        assert!(vbr_average(u32::MAX) > 0);
    }
}
