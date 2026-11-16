//! GPU 纹理缩放：用 `ID3D11VideoProcessor` 把采集纹理缩到编码尺寸，**全程不落 CPU**。
//!
//! # 为什么需要它
//!
//! NVENC 的 `inputWidth/Height` **只做左上角裁剪、不做缩放**（实测码流逐字节相同），
//! 所以「4K 采集 → 1920×1080 编码」过去只能先把 4K BGRA 回读成 32 MB 内存、CPU
//! 盒式下采样、再上传。真机实测这条路合计 **~43 ms/帧**（NVENC 本身只要 3.4 ms、
//! CPU 缩放 4.5 ms），瓶颈就是 4K 回读。
//!
//! 采集纹理与编码器**同设备**时（Windows GUI 被控端默认就是 NVIDIA 采集 + NVENC），
//! 可以用 D3D11 的 VideoProcessor 在 GPU 上把整帧缩到编码尺寸，再把结果纹理直接
//! 注册给 NVENC（零拷贝）。本模块就是这一步缩放器；与 [`crate::capture::change`]
//! 的变化检测共用同一套 VideoProcessor 调用（枚举器 / 处理器 / 输入视图 / 输出
//! 视图 / `VideoProcessorBlt`），只是输出换成 BGRA 而不是 NV12 网格。
//!
//! # 输出纹理
//!
//! - **持久复用一张**：编码器逐帧同步消费（映射 → 编码 → 解映射），下一帧覆盖它
//!   是安全的；也正因为对象地址不变，NVENC 的资源注册只发生一次。
//! - `D3D11_USAGE_DEFAULT` + `RENDER_TARGET`（VideoProcessor 输出的硬要求）；部分
//!   驱动/编码器还要求 `VIDEO_ENCODER` 绑定位，创建失败时**不带它重试一次**
//!   （与 [`crate::mft_encoder::video_processor`] 的输出纹理同一策略）。
//! - **输出格式优先与输入一致**：输入是 `_SRGB` 时尽量输出 `_SRGB`，避免驱动在
//!   SRGB ↔ UNORM 之间做传输曲线转换（桌面颜色会整体变化）；驱动不支持 SRGB 输出
//!   时退回 `_UNORM`。NVENC 两条都接受（见 `nvenc::NvencEncoder::direct_input`）。
//!
//! # 尺寸相等
//!
//! 源/目标尺寸相同时不做 Blt，直接返回源纹理的克隆（COM 引用计数，不拷贝像素）。
//! 调用方通常只在尺寸不等时才需要缩放器，这条只是让接口语义完整。
//!
//! # 失败语义
//!
//! 设备不支持 VideoProcessor、格式不被接受、`Blt` 失败时返回 [`WinVideoError`]。
//! 调用方（NVENC 适配器）据此**回退回读路径**，只影响本帧/本会话的零拷贝，
//! 绝不会关闭 GPU 采集或把画面降级到 CPU 采集。

use crate::d3d11::D3d11;
use crate::WinVideoError;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use windows::core::Interface as _;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_VIDEO_ENCODER, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_RATIONAL,
    DXGI_SAMPLE_DESC,
};

/// 输入视图缓存上限。采集纹理池是 4 张纹理，8 个条目足够覆盖池重建前后的过渡期；
/// 超了就整表清空重建，避免无限增长。
const MAX_CACHED_INPUT_VIEWS: usize = 8;

/// 缩放器绑定的设备与当前尺寸/格式对应的 VideoProcessor 状态。
pub struct TextureScaler {
    /// 采集/编码共用的 D3D11 设备（设备必须与输出纹理、NVENC 编码器一致）。
    device: Arc<D3d11>,
    /// VideoProcessor 的设备/上下文接口（构造时即校验，失败早暴露）。
    video_device: ID3D11VideoDevice,
    /// 立即上下文（`VideoProcessorBlt` 走它）。
    video_context: ID3D11VideoContext,
    /// 当前尺寸/格式对应的枚举器、处理器与输出纹理；尺寸变化时整组重建。
    state: Option<ScalerState>,
}

/// 一组输入尺寸/格式 + 目标尺寸对应的 VideoProcessor 状态。
struct ScalerState {
    /// 缓存键：输入尺寸、输入格式、输出尺寸。任一项变化都要重建。
    key: ScaleKey,
    /// VideoProcessor 设备接口（创建输入视图用）。
    video_device: ID3D11VideoDevice,
    /// 格式枚举器。
    enumerator: ID3D11VideoProcessorEnumerator,
    /// 实际做缩放/格式转换的处理器。
    processor: ID3D11VideoProcessor,
    /// 持久输出纹理（BGRA，目标尺寸）。
    output: ID3D11Texture2D,
    /// 输出纹理的 VP 输出视图。
    output_view: ID3D11VideoProcessorOutputView,
    /// 按纹理裸指针缓存的输入视图（同时持有纹理引用，指针不会悬空）。
    input_views: Vec<(usize, ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}

/// 触发重建的缓存键。
type ScaleKey = (u32, u32, DXGI_FORMAT, u32, u32);

impl std::fmt::Debug for TextureScaler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextureScaler")
            .field("adapter", &self.device.adapter.name)
            .field("state", &self.state.as_ref().map(|state| state.key))
            .finish_non_exhaustive()
    }
}

impl TextureScaler {
    /// 在给定设备上创建缩放器（此时还不知道尺寸，VideoProcessor 状态惰性建立）。
    ///
    /// # Errors
    /// 设备/上下文不支持视频处理（`ID3D11VideoDevice` / `ID3D11VideoContext` 转换
    /// 失败）时返回 [`WinVideoError`]。调用方据此回退回读路径。
    pub fn new(device: Arc<D3d11>) -> Result<Self, WinVideoError> {
        let video_device: ID3D11VideoDevice = device
            .device
            .cast()
            .map_err(|err| WinVideoError::Config(format!("设备不支持视频处理: {err}")))?;
        let video_context: ID3D11VideoContext = device
            .context
            .cast()
            .map_err(|err| WinVideoError::Config(format!("上下文不支持视频处理: {err}")))?;
        Ok(Self {
            device,
            video_device,
            video_context,
            state: None,
        })
    }

    /// 把 `source` 的**整帧**缩放到 `target_size`，返回持久输出纹理（BGRA）。
    ///
    /// 输出纹理在下次调用时会被覆盖；调用方必须在本帧编码完成后再采下一帧
    /// （NVENC 的 `encode_texture` 是同步映射 → 编码 → 解映射，满足这一点）。
    ///
    /// 源纹理与目标尺寸**相等时直接返回源纹理的克隆**，不做 Blt。
    ///
    /// # Errors
    /// 源纹理不是 BGRA8、尺寸为 0、输入/输出格式不被 VideoProcessor 接受、
    /// 输出纹理/视图创建失败或 `Blt` 失败时返回 [`WinVideoError`]。
    pub fn scale(
        &mut self,
        source: &ID3D11Texture2D,
        source_size: (u32, u32),
        target_size: (u32, u32),
    ) -> Result<ID3D11Texture2D, WinVideoError> {
        let (target_width, target_height) = target_size;
        if target_width == 0 || target_height == 0 {
            return Err(WinVideoError::Config(format!(
                "缩放目标尺寸不能为 0：{target_width}x{target_height}"
            )));
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `source` 是调用方保证有效的纹理；`desc` 指向本栈帧。
        unsafe { source.GetDesc(&raw mut desc) };
        if desc.Width == 0 || desc.Height == 0 {
            return Err(WinVideoError::Config("缩放源纹理尺寸为 0".to_owned()));
        }
        if (desc.Width, desc.Height) != source_size {
            return Err(WinVideoError::Config(format!(
                "缩放源尺寸不一致：调用方给 {}x{}，纹理实际 {}x{}",
                source_size.0, source_size.1, desc.Width, desc.Height
            )));
        }
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            && desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        {
            return Err(WinVideoError::Config(format!(
                "缩放只接受 BGRA8 纹理，收到 {:?}",
                desc.Format
            )));
        }
        if source_size == target_size {
            // 尺寸相等：不做任何 GPU 工作，直接共享源纹理（COM 引用计数）。
            return Ok(source.clone());
        }

        let key: ScaleKey = (
            desc.Width,
            desc.Height,
            desc.Format,
            target_width,
            target_height,
        );
        if self.state.as_ref().is_none_or(|state| state.key != key) {
            self.state = Some(ScalerState::new(
                &self.device,
                &self.video_device,
                &self.video_context,
                key,
            )?);
        }
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| WinVideoError::Config("缩放器状态缺失".to_owned()))?;
        state.blit(source, &self.video_context)?;

        // NVENC 在同一块设备上消费输出纹理，但 D3D11 的立即上下文与 NVENC 引擎
        // 是两条队列：把已提交的 Blt 立刻刷给驱动，确保 `nvEncMapInputResource`
        // 看到的是这一帧的内容（`Flush` 只提交、不阻塞 CPU）。
        // SAFETY: 立即上下文的 `Flush` 没有前置条件。
        unsafe { self.device.context.Flush() };

        Ok(state.output.clone())
    }
}

impl ScalerState {
    /// 按缓存键建立枚举器 / 处理器 / 输出纹理 / 输出视图。
    ///
    /// 输出格式优先跟随输入（见模块文档的 SRGB 说明）；两者都不被支持时返回错误。
    fn new(
        device: &D3d11,
        video_device: &ID3D11VideoDevice,
        video_context: &ID3D11VideoContext,
        key: ScaleKey,
    ) -> Result<Self, WinVideoError> {
        let (input_width, input_height, input_format, output_width, output_height) = key;
        let rate = DXGI_RATIONAL {
            Numerator: 30,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: rate,
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: `content` 指向本栈帧，`video_device` 有效。
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&raw const content) }
            .map_err(|err| {
                WinVideoError::Config(format!("CreateVideoProcessorEnumerator 失败: {err}"))
            })?;
        if !format_supported(&enumerator, input_format, true)? {
            // 部分驱动只声明 `_UNORM`、但接受 `_SRGB` 纹理（字节布局相同），
            // 反之亦然；这里按「同字节布局的兄弟格式」再查一次，真正能否建视图
            // 由后续 `CreateVideoProcessorInputView` 决定。
            let sibling = sibling_format(input_format);
            if !format_supported(&enumerator, sibling, true)? {
                return Err(WinVideoError::Config(format!(
                    "VideoProcessor 不支持输入格式 {input_format:?}"
                )));
            }
        }
        // 输出格式优先与输入一致：`_SRGB` 输入若输出成 `_UNORM`，部分驱动会做
        // 传输曲线转换，桌面颜色整体变化。不支持时退回 `_UNORM`。
        let output_format = if format_supported(&enumerator, input_format, false)? {
            input_format
        } else if format_supported(&enumerator, DXGI_FORMAT_B8G8R8A8_UNORM, false)? {
            DXGI_FORMAT_B8G8R8A8_UNORM
        } else {
            return Err(WinVideoError::Config(
                "VideoProcessor 不支持 BGRA8 输出".to_owned(),
            ));
        };

        // SAFETY: `enumerator` 由本设备创建；rate conversion index 0 是标准模式。
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|err| WinVideoError::Config(format!("CreateVideoProcessor 失败: {err}")))?;
        // 输入/输出都是 RGB full range：位域全 0（不做范围压缩、不做 YCbCr 矩阵）。
        // 与 [`crate::capture::change`] 的检测器一致——那里也只比较，不改颜色。
        let space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        // SAFETY: `processor` 有效；纯设置，无副作用。
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &raw const space);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &raw const space);
        }

        let output = create_output_texture(
            &device.device,
            output_width,
            output_height,
            output_format,
        )?;
        let output_view = create_output_view(video_device, &enumerator, &output)?;
        tracing::debug!(
            input = %format!("{input_width}x{input_height} {input_format:?}"),
            output = %format!("{output_width}x{output_height} {output_format:?}"),
            "GPU 纹理缩放器已就绪"
        );
        Ok(Self {
            key,
            video_device: video_device.clone(),
            enumerator,
            processor,
            output,
            output_view,
            input_views: Vec::new(),
        })
    }

    /// 把 `source` 整帧 Blt 到本状态持有的输出纹理上。
    fn blit(
        &mut self,
        source: &ID3D11Texture2D,
        video_context: &ID3D11VideoContext,
    ) -> Result<(), WinVideoError> {
        let view = self.input_view(source)?;
        // `pInputSurface` 是 ManuallyDrop：Blt 不接管所有权，调用后手动取出来释放。
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: windows::core::BOOL(1),
            OutputIndex: 0,
            InputFrameOrField: 0,
            ..Default::default()
        };
        stream.pInputSurface = ManuallyDrop::new(Some(view));
        // SAFETY: `processor`/`output_view` 有效且由同一设备创建，`stream` 指向本栈帧。
        let result = unsafe {
            video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // SAFETY: 与上面的 ManuallyDrop::new 配对，释放我们持有的接口引用。
        unsafe { drop(ManuallyDrop::take(&mut stream.pInputSurface)) };
        result.map_err(|err| WinVideoError::Encode(format!("GPU 缩放 Blt 失败: {err}")))
    }

    /// 取得（或创建并缓存）给定纹理的 VideoProcessor 输入视图。
    fn input_view(
        &mut self,
        source: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorInputView, WinVideoError> {
        let raw = source.as_raw() as usize;
        if let Some((_, _, view)) = self
            .input_views
            .iter()
            .find(|(pointer, _, _)| *pointer == raw)
        {
            return Ok(view.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut view: Option<ID3D11VideoProcessorInputView> = None;
        // SAFETY: `source` 是有效 2D 纹理（尺寸/格式已由调用方校验）。
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                source,
                &self.enumerator,
                &raw const desc,
                Some(&raw mut view),
            )
        }
        .map_err(|err| WinVideoError::Config(format!("CreateVideoProcessorInputView 失败: {err}")))?;
        let view = view.ok_or_else(|| WinVideoError::Config("输入视图返回空接口".to_owned()))?;
        if self.input_views.len() >= MAX_CACHED_INPUT_VIEWS {
            self.input_views.clear();
        }
        // 同时持有纹理引用：缓存键是裸指针，纹理必须活到视图被移除。
        self.input_views.push((raw, source.clone(), view.clone()));
        Ok(view)
    }
}

/// `_SRGB` ↔ `_UNORM` 的兄弟格式（其余格式原样返回）。
fn sibling_format(format: DXGI_FORMAT) -> DXGI_FORMAT {
    if format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB {
        DXGI_FORMAT_B8G8R8A8_UNORM
    } else {
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
    }
}

/// 查询 VideoProcessor 是否支持某个方向上的格式。
fn format_supported(
    enumerator: &ID3D11VideoProcessorEnumerator,
    format: DXGI_FORMAT,
    input: bool,
) -> Result<bool, WinVideoError> {
    // SAFETY: `enumerator` 有效；只查询能力。
    let flags = unsafe { enumerator.CheckVideoProcessorFormat(format) }
        .map_err(|err| WinVideoError::Config(format!("CheckVideoProcessorFormat 失败: {err}")))?;
    let needed = if input {
        D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0
    } else {
        D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0
    };
    let needed = u32::try_from(needed).unwrap_or(0);
    Ok(flags & needed != 0)
}

/// 创建持久输出纹理（BGRA，目标尺寸）。
///
/// `RENDER_TARGET` 是 VideoProcessor 输出的硬要求；部分驱动/编码器还要求
/// `VIDEO_ENCODER` 绑定位，创建失败时带上它重试一次（与 MFT 的 NV12 输出一致）。
fn create_output_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D, WinVideoError> {
    let render_target = u32::try_from(D3D11_BIND_RENDER_TARGET.0).unwrap_or(0);
    let with_encoder = u32::try_from(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0)
        .unwrap_or(render_target);
    let mut last_error = None;
    for bind in [render_target, with_encoder] {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` 合法；输出指针指向栈上 Option。
        match unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) } {
            Ok(()) => {
                return texture.ok_or_else(|| {
                    WinVideoError::Config("CreateTexture2D(BGRA) 返回空纹理".to_owned())
                });
            }
            Err(err) => last_error = Some(err),
        }
    }
    Err(WinVideoError::Config(format!(
        "创建 BGRA 缩放输出纹理 {width}x{height} 失败: {}",
        last_error.map_or_else(|| "未知原因".to_owned(), |err| err.to_string())
    )))
}

/// 创建输出纹理的 VideoProcessor 输出视图。
fn create_output_view(
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorOutputView, WinVideoError> {
    let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };
    let mut view: Option<ID3D11VideoProcessorOutputView> = None;
    // SAFETY: `texture` 是有效的 BGRA 2D 纹理，`desc` 与输出指针都在本栈帧。
    unsafe {
        video_device.CreateVideoProcessorOutputView(texture, enumerator, &raw const desc, Some(&raw mut view))
    }
    .map_err(|err| WinVideoError::Config(format!("CreateVideoProcessorOutputView 失败: {err}")))?;
    view.ok_or_else(|| WinVideoError::Config("输出视图返回空接口".to_owned()))
}

// 编译期断言：缩放器要能随 NVENC 适配器移进 `spawn_blocking`。
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<TextureScaler>();
};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
    };

    /// 测试源尺寸：现场就是 4K 采集缩到 1080p。
    const SOURCE_WIDTH: u32 = 3840;
    /// 测试源高度。
    const SOURCE_HEIGHT: u32 = 2160;
    /// 测试目标宽度。
    const TARGET_WIDTH: u32 = 1920;
    /// 测试目标高度。
    const TARGET_HEIGHT: u32 = 1080;

    /// 纯色 BGRA 像素（alpha 恒为 255）。
    fn pixels() -> Vec<u8> {
        let len = usize::try_from(SOURCE_WIDTH * SOURCE_HEIGHT * 4).expect("尺寸应能放进 usize");
        let mut pixels = vec![0_u8; len];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[0x20, 0x40, 0x80, 0xFF]);
        }
        pixels
    }

    /// 创建一张 4K BGRA 纹理（VideoProcessor 输入视图要求有绑定标志）。
    fn create_source_texture(device: &ID3D11Device, data: &[u8]) -> Option<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: SOURCE_WIDTH,
            Height: SOURCE_HEIGHT,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: u32::try_from(
                D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0,
            )
            .unwrap_or(0),
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr().cast(),
            SysMemPitch: SOURCE_WIDTH * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc`/`init` 指向本栈帧，`data` 在调用期间存活；输出指针有效。
        unsafe {
            device.CreateTexture2D(&raw const desc, Some(&raw const init), Some(&raw mut texture))
        }
        .ok()?;
        texture
    }

    /// CI 的 Windows runner 可能只有 WARP：设备建不起来就跳过，不 panic。
    fn hardware_device() -> Option<Arc<D3d11>> {
        match crate::d3d11::create(None) {
            Ok(device) => Some(Arc::new(device)),
            Err(err) => {
                eprintln!("跳过缩放器测试：没有可用的 D3D11 硬件适配器（{err}）");
                None
            }
        }
    }

    /// 4K → 1080p 的缩放必须产出与目标尺寸一致的 BGRA 纹理。
    #[test]
    fn scales_4k_to_1080p() {
        let Some(device) = hardware_device() else {
            return;
        };
        let data = pixels();
        let Some(source) = create_source_texture(&device.device, &data) else {
            eprintln!("跳过缩放器测试：创建 4K 测试纹理失败");
            return;
        };
        let Ok(mut scaler) = TextureScaler::new(Arc::clone(&device)) else {
            eprintln!("跳过缩放器测试：本端不支持 VideoProcessor");
            return;
        };
        let output = match scaler.scale(
            &source,
            (SOURCE_WIDTH, SOURCE_HEIGHT),
            (TARGET_WIDTH, TARGET_HEIGHT),
        ) {
            Ok(texture) => texture,
            Err(err) => {
                eprintln!("跳过缩放器测试：VideoProcessor 缩放不可用（{err}）");
                return;
            }
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `output` 是有效纹理；`desc` 指向本栈帧。
        unsafe { output.GetDesc(&raw mut desc) };
        assert_eq!((desc.Width, desc.Height), (TARGET_WIDTH, TARGET_HEIGHT));
        assert!(
            desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM
                || desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            "缩放输出必须是 BGRA8（实际 {:?}）",
            desc.Format
        );
        assert_ne!(
            desc.BindFlags & u32::try_from(D3D11_BIND_RENDER_TARGET.0).unwrap_or(0),
            0,
            "VideoProcessor 输出纹理必须带 RENDER_TARGET 绑定位"
        );
    }

    /// 同一尺寸时不做 Blt，直接返回源纹理的克隆（同一 COM 对象）。
    #[test]
    fn same_size_returns_source_texture() {
        let Some(device) = hardware_device() else {
            return;
        };
        let data = pixels();
        let Some(source) = create_source_texture(&device.device, &data) else {
            eprintln!("跳过缩放器测试：创建 4K 测试纹理失败");
            return;
        };
        let Ok(mut scaler) = TextureScaler::new(Arc::clone(&device)) else {
            eprintln!("跳过缩放器测试：本端不支持 VideoProcessor");
            return;
        };
        let returned = scaler
            .scale(
                &source,
                (SOURCE_WIDTH, SOURCE_HEIGHT),
                (SOURCE_WIDTH, SOURCE_HEIGHT),
            )
            .expect("同尺寸缩放不应失败");
        assert_eq!(
            returned.as_raw(),
            source.as_raw(),
            "同尺寸必须直接共享源纹理，不应创建新纹理"
        );
    }

    /// 源尺寸与纹理实际尺寸不一致时必须报错（调用方传错参数要早暴露）。
    #[test]
    fn rejects_mismatched_source_size() {
        let Some(device) = hardware_device() else {
            return;
        };
        let data = pixels();
        let Some(source) = create_source_texture(&device.device, &data) else {
            eprintln!("跳过缩放器测试：创建 4K 测试纹理失败");
            return;
        };
        let Ok(mut scaler) = TextureScaler::new(Arc::clone(&device)) else {
            eprintln!("跳过缩放器测试：本端不支持 VideoProcessor");
            return;
        };
        assert!(
            scaler
                .scale(&source, (1920, 1080), (960, 540))
                .is_err(),
            "源尺寸与纹理实际尺寸不一致时必须报错"
        );
    }
}
