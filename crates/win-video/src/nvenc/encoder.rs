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
//! **设备由编码器拥有**（`Arc<D3d11>`）：调用方可以把**采集源的同一块设备**
//! 传进来，而不是每次新建一块 NVIDIA 设备；持有 `Arc` 同时保证设备在编码器
//! 存活期间不会被释放。
//!
//! # 两条输入通路
//!
//! - **CPU 上传**（[`NvencEncoder::encode_bgra`] / [`NvencEncoder::encode_rgba`]）：
//!   `UpdateSubresource` 写进本编码器自有的输入纹理。跨设备采集（Intel 核显采集
//!   + NVIDIA 独显编码）或需要 CPU 缩放时走这条。
//! - **GPU 纹理直通**（[`NvencEncoder::encode_texture`]）：调用方的采集纹理经
//!   `nvEncRegisterResource` 注册、逐帧 `nvEncMapInputResource` 后直接交给 NVENC，
//!   **不落 CPU**。前提是纹理与编码器同一设备且尺寸相等（NVENC 只裁剪不缩放）。
//!
//! 直通路径的注册按**纹理对象地址**缓存（采集纹理池反复复用同一批纹理），
//! 因此只有首帧付注册成本。任何注册/映射失败都返回错误，由调用方回退到 CPU
//! 上传——一次失败**不该**关掉整条 GPU 采集路径。
//!
//! # 关键配置
//!
//! - H.264 High profile、**峰值受限 VBR**（均值 = 峰值 × 70%）、`frameIntervalP = 1`
//!   （无 B 帧）、`gopLength = NVENC_INFINITE_GOPLENGTH`（无限 GOP）。
//! - `NV_ENC_TUNING_INFO_LOW_LATENCY` + **四分之一分辨率两遍编码** + **空间 AQ（强度 8）**：
//!   桌面文本是「大片平坦背景 + 少量高频笔画」，单遍无 AQ 会把笔画一起量化糊
//!   （旧的 `ULTRA_LOW_LATENCY` 预设正是这样）。
//! - **QP 上限 `MAX_QP`（26）**：按质量下限编码、码率当预算（RDP RemoteFX/Parsec 的
//!   做法）。只给码率时编码器会把预算摊到整幅画面，文字被平均掉。
//! - **intra refresh（GDR，周期 `INTRA_REFRESH_SECONDS` 秒）**：参考帧持续自愈且没有
//!   关键帧尖峰（驱动不支持时自动退回周期 IDR）。
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
use std::sync::Arc;
use windows::core::Interface as _;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};

/// 允许的最大编码宽度（像素）。
///
/// 3840 = **原生分辨率**（硬件 H.264 4K）的上限。默认会话仍由引擎按 1920 编码，
/// 这条只是**校验边界**、不是默认值；NVENC 的真实上限另由创建会话时的
/// `NV_ENC_CAPS_WIDTH_MAX` 核对。H.264 4:2:0 在消费级 NVENC 上编 4K 是常规能力。
pub const MAX_ENCODE_WIDTH: u32 = 3840;
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

/// VBV 缓冲涵盖的帧数。
///
/// 旧实现是「半秒码率」：10 Mbps 档允许单帧吃掉 625 KB，在 2 Mbps 链路上要走
/// 500 ms，期间后续帧全被堵住（实测帧到达抖动 400–500 ms）。
///
/// **但也不能压到两帧**：VBV 同时是**单帧比特上限**，而关键帧要编码整幅画面。两帧在
/// 60 fps 下只有 33 ms 的预算——10 Mbps 档约 41 KB，内容上限收紧到 1.2 Mbps 时只剩
/// 5 KB，1080p 文本必然糊；而静态画面的 P 帧没有残差，糊掉的参考帧不会自愈（现场表现
/// 就是「静止一会儿字突然完全看不清」）。6 帧在 60 fps 下是 100 ms、30 fps 下 200 ms，
/// 配合发送侧 32 KiB 分块 pacing（单间隙 ≤100 ms）不会再整段堵住后续帧。
const VBV_FRAMES: u32 = 6;

/// 单帧比特预算的**每像素下限**（0.5 bit/px，用 `pixels / 2` 表达）。
///
/// 「6 帧码率」这条口径有一个隐蔽的坑：VBV 同时是**单帧比特上限**，而码率会被内容上限
/// 压到很低（4K 的 UI 档 4.8 Mbps → 60 fps 下每帧 10 KB、六帧 60 KB）。于是只要画面
/// 真的变了一块（弹窗、菜单、网页局部刷新），这点预算只够约 **0.2 bit/像素**，编码器
/// 只能把那一块量化成一团——现场表现就是「动不动糊成一块」。QP 地板救不了它：QP 只能
/// 在预算内挑质量，预算本身被 VBV 卡死。
///
/// 因此 VBV 必须按**几何**兜底，而不是跟随时变的（被内容上限压低的）码率：
/// 0.5 bit/px = 1080p ≈ 130 KB、4K ≈ 520 KB 的单帧上限。平均码率仍由内容上限/AIMD
/// 控制（VBR 只在真需要时花），大帧由发送侧 32 KiB 分块 pacing 摊开。
const VBV_BITS_PER_PIXEL_TIMES_2: u32 = 1;

/// `NV_ENC_RC_PARAMS` 的 VBV 缓冲大小：
/// `max(`[`VBV_FRAMES`] 帧的码率, 每像素 0.5 位 × 像素数`)`，至少 1 位。
///
/// `fps == 0` 按 1 处理（避免除零；正常构造路径已拒绝 0 帧率）。
fn vbv_size(bps: u32, fps: u32, width: u32, height: u32) -> u32 {
    let frames = (bps / fps.max(1)).saturating_mul(VBV_FRAMES);
    let pixels = width
        .saturating_mul(height)
        .saturating_mul(VBV_BITS_PER_PIXEL_TIMES_2)
        / 2;
    frames.max(pixels).max(1)
}

/// 一轮 **intra refresh（GDR）** 覆盖的时间（秒）：约 2 秒内把整屏刷成 intra。
///
/// 太短（< 1 秒）每帧的 intra 条带占比高，码率开销明显；太长（> 5 秒）参考帧自愈
/// 变慢，用户看到的糊会持续更久。2 秒在 30/60 fps 下分别是 60/120 帧。
const INTRA_REFRESH_SECONDS: u32 = 2;

/// 一轮 intra refresh 的帧数（`fps × INTRA_REFRESH_SECONDS`，至少 1）。
///
/// NVENC 要求 `intraRefreshCnt <= intraRefreshPeriod`；两者取同一个值 = 每帧均匀刷新
/// `1/period` 的宏块。
fn intra_refresh_frames(fps: u32) -> u32 {
    fps.max(1).saturating_mul(INTRA_REFRESH_SECONDS)
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

/// 有界 LRU 表：容量满时淘汰最久未使用项。
///
/// 专给直通输入注册缓存用。抽成泛型是为了让「命中后变最近使用」「容量淘汰时
/// 返回旧值」这两条顺序不变量能**脱离 GPU 单测**——真机路径无法在本机跑，
/// 而淘汰顺序错了会导致注销一个仍在被映射的注册，或者泄漏注册。
struct LruCache<T> {
    /// 最近使用的在前，最久未使用的在尾。
    entries: Vec<(usize, T)>,
    /// 容量上限（至少 1）。
    capacity: usize,
}

impl<T> LruCache<T> {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// 取值并把它标记为最近使用。
    fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        let index = self
            .entries
            .iter()
            .position(|(candidate, _)| *candidate == key)?;
        let entry = self.entries.remove(index);
        self.entries.insert(0, entry);
        self.entries.first_mut().map(|(_, value)| value)
    }

    /// 插入并标记为最近使用；返回被替换或淘汰的旧值（调用方负责注销）。
    fn insert(&mut self, key: usize, value: T) -> Option<T> {
        let existing = self
            .entries
            .iter()
            .position(|(candidate, _)| *candidate == key);
        let replaced = existing.map(|index| self.entries.remove(index).1);
        self.entries.insert(0, (key, value));
        if replaced.is_some() {
            return replaced;
        }
        if self.entries.len() > self.capacity {
            return self.entries.pop().map(|(_, value)| value);
        }
        None
    }

    /// 删除 `key` 并返回其值。
    fn remove(&mut self, key: usize) -> Option<T> {
        let index = self
            .entries
            .iter()
            .position(|(candidate, _)| *candidate == key)?;
        Some(self.entries.remove(index).1)
    }

    /// 取走全部值（用于 `Drop` 里统一注销）。
    fn drain(&mut self) -> Vec<T> {
        self.entries.drain(..).map(|(_, value)| value).collect()
    }
}

/// 直通缓存的容量：采集纹理池为 4 张，留一倍余量给池重建（改分辨率时新旧池会
/// 短暂并存）与探针自建的临时纹理。
const DIRECT_INPUT_CAPACITY: usize = 8;

/// 直通输入纹理的注册缓存：纹理对象地址 → 注册句柄。
///
/// **必须同时持有纹理引用**：注册期间驱动保存的是裸资源指针，若纹理先被释放、
/// 之后的新对象恰好拿到同一地址，缓存就会把过期句柄发给一张完全不同的纹理。
/// 持引用既稳定了地址，也保证资源在注销前不会被释放。
struct DirectInputCache {
    entries: LruCache<(ID3D11Texture2D, sys::NV_ENC_REGISTERED_PTR)>,
}

impl DirectInputCache {
    fn new() -> Self {
        Self {
            entries: LruCache::new(DIRECT_INPUT_CAPACITY),
        }
    }

    /// 缓存键：纹理对象地址（同一 `ID3D11Texture2D` 在生命周期内不变）。
    fn key(texture: &ID3D11Texture2D) -> usize {
        texture.as_raw() as usize
    }

    /// 取已注册句柄（命中会刷新最近使用顺序）。
    fn get(&mut self, key: usize) -> Option<sys::NV_ENC_REGISTERED_PTR> {
        self.entries.get_mut(key).map(|(_, registered)| *registered)
    }

    /// 登记一条新注册；返回需要由调用方**先注销**的旧句柄（同键替换或淘汰）。
    fn insert(
        &mut self,
        key: usize,
        texture: ID3D11Texture2D,
        registered: sys::NV_ENC_REGISTERED_PTR,
    ) -> Option<sys::NV_ENC_REGISTERED_PTR> {
        self.entries
            .insert(key, (texture, registered))
            .map(|(_, old)| old)
    }

    /// 移除并返回 `key` 的注册句柄（映射失败时用它清掉坏项）。
    fn remove(&mut self, key: usize) -> Option<sys::NV_ENC_REGISTERED_PTR> {
        self.entries.remove(key).map(|(_, registered)| registered)
    }

    /// 取走全部（纹理引用, 注册句柄），供 `Drop` 统一注销。
    fn drain(&mut self) -> Vec<(ID3D11Texture2D, sys::NV_ENC_REGISTERED_PTR)> {
        self.entries.drain()
    }
}

/// 从驱动取 H.264 的 preset 默认配置，再覆盖成本模块要求的固定参数。
fn build_encode_config(
    session: &NvencSession<'_>,
    bitrate_bps: u32,
    fps: u32,
    width: u32,
    height: u32,
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
    // **LOW_LATENCY 而不是 ULTRA_LOW_LATENCY**：后者为了省下最后一点延迟会关掉 RDO 与
    // 自适应量化，桌面文本（高频细节）最先被量化掉。LOW_LATENCY 仍是零 B 帧、无 lookahead
    // 的实时路径，编码耗时差别在毫秒级，画质差距却肉眼可见。
    let status = unsafe {
        get_preset(
            session.handle(),
            sys::NV_ENC_CODEC_H264_GUID,
            sys::NV_ENC_PRESET_P4_GUID,
            sys::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY,
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
    config.rcParams.vbvBufferSize = vbv_size(bitrate_bps, fps, width, height);
    config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
    // **两遍编码（四分之一分辨率）**：第一遍在低分辨率上预分析场景复杂度，第二遍据此
    // 分配比特。桌面文本的比特分配极不均匀（大片纯色 + 少量高频笔画），单遍 VBR 会把
    // 笔画一起量化掉。四分之一分辨率是延迟/画质的最佳折衷（增益接近全分辨率两遍，
    // 开销小一个量级）。
    config.rcParams.multiPass =
        sys::_NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION;
    // **空间自适应量化**：把比特从平坦背景挪到文字/控件边缘。旧实现完全依赖预设，
    // 而 LOW_LATENCY 预设默认不开 AQ——这正是「同码率下字发糊」的编码器侧主因之一。
    // 强度取 8（0..15 的中上档）：再高会在纯色背景上产生可见的块状纹理。
    config.rcParams.set_enableAQ(1);
    config.rcParams.set_aqStrength(8);
    // **QP 上限（画质地板）**：只给「最多多少 Mbps」是不够的——码率控制会把预算摊到
    // 整幅画面上，文字被平均掉。给 QP 设上限等于告诉编码器「质量不许低于这条线，
    // 需要的比特你自己拿」，这是 RDP RemoteFX/Parsec 这类实现的共同做法（按质量目标
    // 编码，码率当预算）。需要更多比特时仍受上面的 VBV 约束，不会无限放大。
    // 关键帧（I）再低 2 档：它是整幅参考，后续 P 帧都靠它。
    let max_qp = crate::max_qp();
    config.rcParams.set_enableMaxQP(1);
    config.rcParams.maxQP = sys::NV_ENC_QP {
        qpInterP: max_qp,
        qpInterB: max_qp,
        qpIntra: max_qp.saturating_sub(2).max(1),
    };

    // SAFETY: `encodeCodecConfig` 是 union；本会话固定走 H.264 分支，且 union
    // 已由 preset 配置按 H.264 初始化。
    let h264 = unsafe { &mut config.encodeCodecConfig.h264Config };
    // **渐进式刷新（intra refresh / GDR）**：每帧把一小条宏块编成 intra，约
    // [`INTRA_REFRESH_SECONDS`] 秒刷完一整屏。替代「周期性 IDR」的刷新职责——参考帧
    // 持续自我修复，且**没有关键帧尖峰**（Moonlight/Sunshine 与 RDP 都这么做）。
    // 显式 FORCEIDR（新控制端接入、序号断层、几何变化）仍然生效，语义不变。
    //
    // 只在驱动声明支持时开：能力查询失败/为 0 就保持关闭，退回到引擎侧的周期 IDR
    // （见 `controlled.rs` 的 `PERIODIC_KEYFRAME_INTERVAL`），绝不因为一个可选特性
    // 让整个编码会话初始化失败。
    let intra_refresh_supported = session
        .api()
        .cap(
            session,
            sys::NV_ENC_CODEC_H264_GUID,
            sys::_NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_INTRA_REFRESH,
        )
        .is_ok_and(|value| value > 0);
    if intra_refresh_supported {
        h264.set_enableIntraRefresh(1);
        h264.intraRefreshPeriod = intra_refresh_frames(fps);
        h264.intraRefreshCnt = intra_refresh_frames(fps);
    } else {
        tracing::debug!("NVENC 未声明支持 intra refresh，参考帧刷新退回周期 IDR");
    }
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

    // 真机排障要能确认这两个「画质旋钮」真的生效（每次编码器创建只打一行）。
    tracing::info!(
        max_qp,
        intra_refresh = intra_refresh_supported,
        intra_refresh_frames = intra_refresh_frames(fps),
        fps,
        bitrate_bps,
        "NVENC 编码配置就绪（QP 地板 / 渐进刷新）"
    );

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
    /// 设备所有权（纹理与上下文都源自它）。
    ///
    /// 复用采集设备时这里与采集源共享同一个 `Arc<D3d11>`；独立设备（探针路径）
    /// 则独占一块新建的设备。直通路径要求调用方的纹理来自**同一块**设备。
    d3d: Arc<D3d11>,
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    /// 外部 GPU 纹理的注册缓存（[`Self::encode_texture`] 用）。
    direct: DirectInputCache,
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
    /// 在 `d3d` 指定的 D3D11 设备上打开一个 H.264 4:2:0 编码会话并准备好输入纹理。
    ///
    /// `d3d` 按值传入（`Arc`）：调用方可以复用采集源的设备，避免多建一块
    /// NVIDIA 设备。
    ///
    /// # Errors
    /// 参数越界、驱动拒绝会话/初始化、D3D11 纹理创建或资源注册失败时返回
    /// [`WinVideoError`]。
    pub fn new(
        api: &'a NvencApi,
        d3d: Arc<D3d11>,
        config: EncoderConfig,
    ) -> Result<Self, WinVideoError> {
        config.validate()?;

        let session = api.open_session(&d3d)?;
        let bitrate_bps = kbps_to_bps(config.bitrate_kbps);
        let encode_config = build_encode_config(
            &session,
            bitrate_bps,
            config.fps,
            config.width,
            config.height,
        )?;

        let context = d3d.context.clone();
        let texture = create_texture(
            &d3d.device,
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
        init.tuningInfo = sys::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY;
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
            d3d,
            context,
            texture,
            direct: DirectInputCache::new(),
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

    /// 编码器所在的 D3D11 设备。
    ///
    /// 直通路径（[`Self::encode_texture`]）要求纹理与它**是同一块设备**，
    /// 调用方用 `Arc::ptr_eq` 校验即可。
    #[must_use]
    pub fn device(&self) -> &Arc<D3d11> {
        &self.d3d
    }

    /// `device` 是否就是本编码器所在的那块 D3D11 设备。
    #[must_use]
    pub fn is_same_device(&self, device: &Arc<D3d11>) -> bool {
        Arc::ptr_eq(&self.d3d, device)
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
        self.encode_mapped(self.mapped, self.mapped_format, force_idr)
    }

    /// 编码一帧**调用方的 GPU 纹理**（零拷贝直通，`nvEncRegisterResource`）。
    ///
    /// 与 [`Self::encode`] 相比只换了输入来源：把纹理注册给 NVENC 后逐帧
    /// 映射 → 编码 → 解映射，**不经过本编码器自有的输入纹理、不做任何 CPU 拷贝**。
    ///
    /// 调用方必须保证：
    /// - 纹理与编码器在同一块 D3D11 设备上（跨设备注册会被驱动拒绝）；
    /// - 纹理尺寸 == [`Self::source_size`]（NVENC 只按 `inputWidth/Height` 裁剪、
    ///   不缩放），格式与 [`Self::input_format`] 一致。
    ///
    /// 注册按纹理对象地址缓存（采集纹理池会反复复用同一批纹理），首帧之后每帧
    /// 只多一次 `nvEncMapInputResource`。
    ///
    /// **失败不是致命的**：约束不满足、驱动拒绝注册/映射时返回错误，调用方应回退
    /// 到 CPU 上传路径（[`Self::encode_bgra`] / [`Self::encode_rgba`]），而**不要**
    /// 因此关掉整条 GPU 采集路径。
    ///
    /// # Errors
    /// 纹理格式/尺寸不符（[`WinVideoError::Config`]）、驱动未导出注册/映射入口、
    /// 注册或映射失败、编码/锁码流失败（[`WinVideoError::Nvenc`]）。
    pub fn encode_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        force_idr: bool,
    ) -> Result<EncodedPacket, WinVideoError> {
        let key = DirectInputCache::key(texture);
        let registered = self.direct_input(key, texture)?;
        let (mapped, mapped_format) = match map_input(&self.session, registered) {
            Ok(mapped) => mapped,
            Err(err) => {
                // 映射失败说明这条注册不可用，别把坏句柄留在缓存里（否则每帧都会
                // 在同一处失败，且 Drop 时还会去注销一个可能已失效的句柄）。
                if let Some(dead) = self.direct.remove(key) {
                    unregister_input(&self.session, dead);
                }
                return Err(err);
            }
        };
        let result = self.encode_mapped(mapped, mapped_format, force_idr);
        // 无论编码成功与否都必须解映射，否则下一帧无法再映射这张纹理。
        unmap_input(&self.session, mapped);
        result
    }

    /// 取（必要时注册）外部纹理对应的 NVENC 注册句柄。
    fn direct_input(
        &mut self,
        key: usize,
        texture: &ID3D11Texture2D,
    ) -> Result<sys::NV_ENC_REGISTERED_PTR, WinVideoError> {
        if let Some(registered) = self.direct.get(key) {
            return Ok(registered);
        }
        // 首次见到这张纹理才校验：同一对象的描述不会变，缓存命中时无需重复查询。
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` 是调用方保证有效的纹理；`desc` 指向本栈帧。
        unsafe { texture.GetDesc(&raw mut desc) };
        // 采集纹理可能是 `_SRGB` 变体（WGC/DXGI 的帧格式），字节布局与 `_UNORM`
        // 完全相同；NVENC 注册只看排列不看传输曲线，故一并接受。
        let srgb_bgra = self.input_format == InputFormat::Bgra
            && desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB;
        if desc.Format != self.input_format.dxgi_format() && !srgb_bgra {
            return Err(WinVideoError::Config(format!(
                "直通纹理格式 {:?} 与编码器输入格式 {:?} 不一致",
                desc.Format, self.input_format
            )));
        }
        if (desc.Width, desc.Height) != (self.source_width, self.source_height) {
            return Err(WinVideoError::Config(format!(
                "直通纹理尺寸 {}x{} 与编码器输入尺寸 {}x{} 不一致（缩放必须先回读）",
                desc.Width, desc.Height, self.source_width, self.source_height
            )));
        }
        let registered = register_input(
            &self.session,
            texture,
            self.source_width,
            self.source_height,
            self.input_format,
        )?;
        if let Some(old) = self.direct.insert(key, texture.clone(), registered) {
            // 同键替换（不该发生：命中时不会走到这里）或容量淘汰。
            unregister_input(&self.session, old);
        }
        Ok(registered)
    }

    /// 用**已映射**的输入资源编码一帧；CPU 上传与纹理直通两条路径共用。
    fn encode_mapped(
        &mut self,
        mapped: sys::NV_ENC_INPUT_PTR,
        mapped_format: sys::NV_ENC_BUFFER_FORMAT,
        force_idr: bool,
    ) -> Result<EncodedPacket, WinVideoError> {
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
            (*pic_ptr).inputBuffer = mapped;
            (*pic_ptr).outputBitstream = self.bitstream;
            (*pic_ptr).bufferFmt = mapped_format;
            (*pic_ptr).pictureStruct = sys::_NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
        }

        // SAFETY: 会话句柄有效；输入缓冲来自本编码器映射的资源，输出缓冲由本
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
        self.config.rcParams.vbvBufferSize = vbv_size(bps, self.fps, self.width, self.height);
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
        // VBV 额度按帧数计算，帧率一变额度也跟着变，故必须重算后一起重配置
        // （NVENC 的 `nvEncReconfigureEncoder` 同时接受帧率与 VBV，仍不重建/不插 IDR）。
        self.config.rcParams.vbvBufferSize =
            vbv_size(self.bitrate_bps, fps, self.width, self.height);
        self.config.rcParams.vbvInitialDelay = self.config.rcParams.vbvBufferSize;
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
        // 直通缓存：**先注销再放掉纹理引用**。顺序不能反——纹理一旦先释放，
        // 注销的就不再是一块有效资源。每帧的映射都在编码后立即解除，这里不会
        // 有仍处于映射状态的注册。
        for (_, registered) in self.direct.drain() {
            unregister_input(&self.session, registered);
        }
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
    use super::{
        intra_refresh_frames, vbr_average, vbv_size, LruCache, INTRA_REFRESH_SECONDS,
        VBV_BITS_PER_PIXEL_TIMES_2, VBV_FRAMES,
    };

    /// Intra refresh 的周期按「秒 × fps」算；刷新跨度的帧数必须 ≥1 且不除零。
    #[test]
    fn intra_refresh_period_covers_seconds_at_any_fps() {
        assert_eq!(intra_refresh_frames(60), 60 * INTRA_REFRESH_SECONDS);
        assert_eq!(intra_refresh_frames(30), 30 * INTRA_REFRESH_SECONDS);
        assert_eq!(intra_refresh_frames(0), INTRA_REFRESH_SECONDS, "fps=0 按 1 处理");
        // 极大 fps 走饱和乘法，不溢出。
        assert!(intra_refresh_frames(u32::MAX) > 0);
    }

    /// VBV 取「六帧码率」与「每像素 0.5 位」的较大者。
    #[test]
    fn vbv_size_gives_keyframes_headroom() {
        // 10 Mbps @ 30fps、640×360：六帧 1_999_998 位 > 每像素下限 115_200 位。
        assert_eq!(vbv_size(10_000_000, 30, 640, 360), 1_999_998);
        // 2 Mbps @ 15fps、640×360：六帧 799_998 位 > 115_200 位。
        assert_eq!(vbv_size(2_000_000, 15, 640, 360), 799_998);
        // 1080p 的每像素下限 = 1920×1080/2 = 1_036_800 位（≈130 KB）。
        assert_eq!(vbv_size(2_000_000, 15, 1920, 1080), 1_036_800);
        // 4K = 3840×2160/2 = 4_147_200 位（≈520 KB）。
        assert_eq!(vbv_size(40_000_000, 60, 3840, 2160), 4_147_200);
        assert_eq!(VBV_FRAMES, 6);
        assert_eq!(VBV_BITS_PER_PIXEL_TIMES_2, 1);
    }

    /// 关键回归：内容上限把码率压到 UI 档（4K 4.8 Mbps）时，单帧预算不能跟着塌到几十 KB。
    ///
    /// 旧口径 `6 × bps/fps` 在这个组合下只有 60 KB（≈0.06 bit/px），任何一块画面变化都会被
    /// 量化成一团——现场就是「动不动糊成一块」。geometry 兜底后是 520 KB（≈0.5 bit/px）。
    #[test]
    fn vbv_size_survives_the_content_cap_at_four_k() {
        let capped = vbv_size(4_800_000, 60, 3840, 2160);
        assert_eq!(capped, 4_147_200, "应按几何兜底，而不是按被压低的码率");
        // 换算成字节：≈506 KB / 帧。
        assert!(capped / 8 > 500_000);
        // 1080p 的 UI 档（1.2 Mbps）同理。
        assert_eq!(vbv_size(1_200_000, 60, 1920, 1080), 1_036_800);
    }

    /// 帧率为 0 时按 1 处理（不除零），极小码率也不返回 0。
    #[test]
    fn vbv_size_guards_zero() {
        assert_eq!(vbv_size(1, 0, 0, 0), 6);
        assert_eq!(vbv_size(0, 30, 0, 0), 1);
        // 极大值走饱和乘法，不 panic。
        assert!(vbv_size(u32::MAX, 1, u32::MAX, u32::MAX) > 0);
    }

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

    /// 直通注册缓存的**淘汰顺序**：命中刷新最近使用、满容量淘汰最久未使用。
    ///
    /// 顺序错了会注销一张仍在使用中的注册（画面直接坏掉）或泄漏注册，
    /// 而这条路径在本机无法真机验证，只能靠这里的机械锁。
    #[test]
    fn lru_cache_evicts_least_recently_used() {
        let mut cache = LruCache::new(2);
        assert_eq!(cache.insert(1, "a"), None);
        assert_eq!(cache.insert(2, "b"), None);
        // 命中 1：顺序变成 1（最近）→ 2（最久）。
        assert_eq!(cache.get_mut(1), Some(&mut "a"));
        // 插入 3 必须淘汰 2，而不是 1。
        assert_eq!(cache.insert(3, "c"), Some("b"));
        assert!(cache.get_mut(2).is_none(), "被淘汰的键不能再命中");
        assert_eq!(cache.get_mut(1), Some(&mut "a"));
        assert_eq!(cache.get_mut(3), Some(&mut "c"));
    }

    /// 同键重复插入必须把旧值交回调用方注销（不能静默丢弃句柄）。
    #[test]
    fn lru_cache_same_key_returns_old_value() {
        let mut cache = LruCache::new(4);
        assert_eq!(cache.insert(7, 10_u32), None);
        assert_eq!(cache.insert(7, 20_u32), Some(10));
        assert_eq!(cache.get_mut(7), Some(&mut 20));
    }

    /// 容量为 0 会被抬到 1（一个键都不缓存等于每帧重新注册）。
    #[test]
    fn lru_cache_capacity_is_at_least_one() {
        let mut cache = LruCache::new(0);
        assert_eq!(cache.insert(1, "a"), None);
        assert_eq!(cache.insert(2, "b"), Some("a"));
        assert_eq!(cache.get_mut(2), Some(&mut "b"));
    }

    /// `remove` / `drain` 必须真的把值交出来（`Drop` 靠 `drain` 逐个注销）。
    #[test]
    fn lru_cache_remove_and_drain_return_values() {
        let mut cache = LruCache::new(4);
        cache.insert(1, "a");
        cache.insert(2, "b");
        cache.insert(3, "c");
        assert_eq!(cache.remove(2), Some("b"));
        assert_eq!(cache.remove(2), None);
        let mut drained = cache.drain();
        drained.sort_unstable();
        assert_eq!(drained, vec!["a", "c"]);
        assert!(cache.get_mut(1).is_none(), "drain 之后表必须为空");
    }
}
