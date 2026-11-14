//! D3D11 VideoProcessor：BGRA 采集纹理 → NV12（BT.709 limited，可缩放）。
//!
//! GPU 纹理路径的中间环节。用同一块 D3D11 设备上的 `ID3D11VideoDevice` /
//! `ID3D11VideoContext` 做一次 `VideoProcessorBlt`，把采集到的 BGRA 纹理转换
//! 并缩放到编码尺寸的 NV12 纹理；随后由 [`super::mfplat::create_dxgi_surface_buffer`]
//! 包成 `IMFSample` 喂给 Intel QSV，全程不落 CPU。
//!
//! # 颜色空间
//!
//! 输入是桌面合成的 **full-range sRGB**（BGRA），输出必须是 **BT.709 limited**
//! （16–235），与 [`super::convert`] 的 CPU 路径、NVENC 与解码侧严格一致。
//! 两组设置缺一不可：
//!
//! - `VideoProcessorSetStreamColorSpace`（输入流）：`Usage=0`、`RGB_Range=0`
//!   （full）、`YCbCr_Matrix=0`、`Nominal_Range=0`；
//! - `VideoProcessorSetOutputColorSpace`（输出）：`Usage=0`、`YCbCr_Matrix=1`
//!   （BT.709）、`Nominal_Range=1`（16–235）。
//!
//! **注意编码方向相反**：D3D11 的 `D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235`
//! 值是 **1**，而 Media Foundation 的 `MFNominalRange_16_235` 是 **2**。这里
//! 用 D3D11 的枚举，别照抄 MF 的数字（否则输出会变成 0–255，解码后整体发灰）。
//!
//! # 输出纹理轮转
//!
//! 异步 MFT（Intel QSV）在 `ProcessInput` 返回后仍可能持有输入样本，因此管道里
//! 会有 1–2 帧在途。NV12 输出纹理用 [`OUTPUT_TEXTURE_COUNT`] 张轮转，避免下一帧
//! 的 `VideoProcessorBlt` 覆盖 MFT 尚未消费的画面。

use crate::capture::CapturedTexture;
use crate::d3d11::D3d11;
use crate::WinVideoError;
use std::mem::ManuallyDrop;
use windows::core::{Interface as _, BOOL};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_VIDEO_ENCODER, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

/// 轮转的 NV12 输出纹理数量（QSV 异步管道通常有 1–2 帧在途）。
const OUTPUT_TEXTURE_COUNT: usize = 3;

/// 输入视图缓存上限。采集池是 4 张纹理，8 个条目足够覆盖池重建前后的过渡期；
/// 超了就整表清空重建，避免无限增长。
const MAX_CACHED_INPUT_VIEWS: usize = 8;

/// 一块输入尺寸/格式对应的 VideoProcessor 与 NV12 输出。
pub(super) struct Nv12Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_textures: Vec<ID3D11Texture2D>,
    output_views: Vec<ID3D11VideoProcessorOutputView>,
    next_output: usize,
    input_key: (u32, u32, DXGI_FORMAT),
    input_views: Vec<(usize, ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}

impl std::fmt::Debug for Nv12Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nv12Converter")
            .field("input_key", &self.input_key)
            .field("outputs", &self.output_textures.len())
            .finish_non_exhaustive()
    }
}

impl Nv12Converter {
    /// 在给定设备上创建转换器。
    ///
    /// `input_format` 是采集纹理的格式（BGRA8 UNORM/SRGB），`output_width` /
    /// `output_height` 是编码尺寸；两者不同时由 VideoProcessor 顺带缩放。
    ///
    /// # Errors
    /// 设备不支持 VideoProcessor、输入/输出格式不被接受、纹理或视图创建失败时
    /// 返回 [`WinVideoError::Encode`]。
    pub(super) fn new(
        device: &D3d11,
        input_width: u32,
        input_height: u32,
        input_format: DXGI_FORMAT,
        output_width: u32,
        output_height: u32,
        fps: u32,
    ) -> Result<Self, WinVideoError> {
        if input_width == 0 || input_height == 0 || output_width == 0 || output_height == 0 {
            return Err(WinVideoError::Encode(format!(
                "VideoProcessor 尺寸不能为 0：输入 {input_width}x{input_height}，输出 {output_width}x{output_height}"
            )));
        }
        let video_device: ID3D11VideoDevice = device
            .device
            .cast()
            .map_err(|err| WinVideoError::Encode(format!("D3D11 设备不支持视频处理: {err}")))?;
        let video_context: ID3D11VideoContext = device
            .context
            .cast()
            .map_err(|err| WinVideoError::Encode(format!("D3D11 上下文不支持视频处理: {err}")))?;

        let rate = DXGI_RATIONAL {
            Numerator: fps.max(1),
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
                WinVideoError::Encode(format!("CreateVideoProcessorEnumerator 失败: {err}"))
            })?;

        check_format(&enumerator, input_format, true)?;
        check_format(&enumerator, DXGI_FORMAT_NV12, false)?;

        // SAFETY: `enumerator` 由本设备创建；rate conversion index 0 是标准模式。
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|err| WinVideoError::Encode(format!("CreateVideoProcessor 失败: {err}")))?;

        // 输入：full-range RGB（桌面）；YCbCr 矩阵对 RGB 输入无意义，留 0。
        let input_space = color_space(0, false, false, 0);
        // 输出：BT.709 limited（16-235），与 CPU 转换/NVENC VUI/解码侧一致。
        let limited = u32::try_from(D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235.0).unwrap_or(1);
        let output_space = color_space(0, false, true, limited);
        // SAFETY: `processor` 有效；颜色空间与帧格式都是纯设置，失败无副作用。
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &raw const input_space);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &raw const output_space);
        }

        let output_textures = create_output_textures(&device.device, output_width, output_height)?;
        let mut output_views = Vec::with_capacity(OUTPUT_TEXTURE_COUNT);
        for texture in &output_textures {
            output_views.push(create_output_view(&video_device, &enumerator, texture)?);
        }

        tracing::debug!(
            input = %format!("{input_width}x{input_height} {input_format:?}"),
            output = %format!("{output_width}x{output_height}"),
            "D3D11 VideoProcessor 已就绪（BGRA → NV12 BT.709 limited）"
        );
        Ok(Self {
            video_device,
            video_context,
            enumerator,
            processor,
            output_textures,
            output_views,
            next_output: 0,
            input_key: (input_width, input_height, input_format),
            input_views: Vec::new(),
        })
    }

    /// 当前缓存对应的输入尺寸与格式。
    pub(super) fn input_key(&self) -> (u32, u32, DXGI_FORMAT) {
        self.input_key
    }

    /// 把一帧 BGRA 纹理转换/缩放到 NV12，返回本帧使用的输出纹理。
    ///
    /// 返回的纹理在接下来 [`OUTPUT_TEXTURE_COUNT`]−1 次调用内不会被覆盖；调用方
    /// 应在此之前完成 `ProcessInput`（编码器会持有自己的引用）。
    ///
    /// # Errors
    /// 输入视图创建或 `VideoProcessorBlt` 失败时返回 [`WinVideoError::Encode`]。
    pub(super) fn convert(
        &mut self,
        frame: &CapturedTexture,
    ) -> Result<ID3D11Texture2D, WinVideoError> {
        let view = self.input_view(frame)?;
        let index = self.next_output % self.output_views.len().max(1);
        let output_view = self
            .output_views
            .get(index)
            .ok_or_else(|| WinVideoError::Encode("VideoProcessor 输出视图缺失".to_owned()))?;

        // `pInputSurface` 是 ManuallyDrop：Blt 不接管所有权，调用后手动取出来释放。
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            OutputIndex: 0,
            InputFrameOrField: 0,
            ..Default::default()
        };
        stream.pInputSurface = ManuallyDrop::new(Some(view));

        // SAFETY: `processor`/`output_view` 有效且由同一设备创建，`stream` 指向
        // 本栈帧；Blt 在立即上下文上同步完成。
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // SAFETY: 与上面的 ManuallyDrop::new 配对，释放我们持有的接口引用。
        unsafe { drop(ManuallyDrop::take(&mut stream.pInputSurface)) };
        result.map_err(|err| WinVideoError::Encode(format!("VideoProcessorBlt 失败: {err}")))?;

        let texture = self
            .output_textures
            .get(index)
            .cloned()
            .ok_or_else(|| WinVideoError::Encode("VideoProcessor 输出纹理缺失".to_owned()))?;
        self.next_output = (index + 1) % self.output_textures.len().max(1);
        Ok(texture)
    }

    /// 取得（或创建并缓存）给定纹理的输入视图。
    fn input_view(
        &mut self,
        frame: &CapturedTexture,
    ) -> Result<ID3D11VideoProcessorInputView, WinVideoError> {
        let raw = frame.texture.as_raw() as usize;
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
        // SAFETY: `frame.texture` 是有效 2D 纹理（尺寸/格式已由调用方校验），
        // `desc` 与输出指针都指向本栈帧。
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                &frame.texture,
                &self.enumerator,
                &raw const desc,
                Some(&raw mut view),
            )
        }
        .map_err(|err| {
            WinVideoError::Encode(format!("CreateVideoProcessorInputView 失败: {err}"))
        })?;
        let view = view.ok_or_else(|| WinVideoError::Encode("输入视图返回空接口".to_owned()))?;

        if self.input_views.len() >= MAX_CACHED_INPUT_VIEWS {
            self.input_views.clear();
        }
        // 同时持有纹理引用：缓存键是裸指针，纹理必须活到视图被移除。
        self.input_views
            .push((raw, frame.texture.clone(), view.clone()));
        Ok(view)
    }
}

/// 校验输入/输出格式是否被这块 VideoProcessor 接受。
fn check_format(
    enumerator: &ID3D11VideoProcessorEnumerator,
    format: DXGI_FORMAT,
    input: bool,
) -> Result<(), WinVideoError> {
    // SAFETY: `enumerator` 有效；只查询能力。
    let flags = unsafe { enumerator.CheckVideoProcessorFormat(format) }
        .map_err(|err| WinVideoError::Encode(format!("CheckVideoProcessorFormat 失败: {err}")))?;
    let needed = if input {
        D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0
    } else {
        D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0
    };
    let needed = u32::try_from(needed).unwrap_or(0);
    if flags & needed == 0 {
        return Err(WinVideoError::Encode(format!(
            "VideoProcessor 不支持该{}格式 {format:?}（support flags = {flags:#x}）",
            if input { "输入" } else { "输出" }
        )));
    }
    Ok(())
}

/// 创建 NV12 输出纹理。
///
/// `RENDER_TARGET` 是 VideoProcessor 输出的硬要求；部分驱动/编码器还要求
/// `VIDEO_ENCODER` 绑定位，创建失败时带上它重试一次。
fn create_output_textures(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    width: u32,
    height: u32,
) -> Result<Vec<ID3D11Texture2D>, WinVideoError> {
    let mut textures = Vec::with_capacity(OUTPUT_TEXTURE_COUNT);
    for _ in 0..OUTPUT_TEXTURE_COUNT {
        textures.push(create_output_texture(device, width, height)?);
    }
    Ok(textures)
}

/// 创建单张 NV12 输出纹理。
fn create_output_texture(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    width: u32,
    height: u32,
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
            Format: DXGI_FORMAT_NV12,
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
                    WinVideoError::Encode("CreateTexture2D(NV12) 返回空纹理".to_owned())
                });
            }
            Err(err) => last_error = Some(err),
        }
    }
    Err(WinVideoError::Encode(format!(
        "创建 NV12 输出纹理 {width}x{height} 失败: {}",
        last_error.map_or_else(|| "未知原因".to_owned(), |err| err.to_string())
    )))
}

/// 创建 NV12 输出纹理的 VideoProcessor 输出视图。
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
    // SAFETY: `texture` 是有效的 NV12 2D 纹理，`desc` 与输出指针都在本栈帧。
    unsafe {
        video_device.CreateVideoProcessorOutputView(
            texture,
            enumerator,
            &raw const desc,
            Some(&raw mut view),
        )
    }
    .map_err(|err| WinVideoError::Encode(format!("CreateVideoProcessorOutputView 失败: {err}")))?;
    view.ok_or_else(|| WinVideoError::Encode("输出视图返回空接口".to_owned()))
}

/// 组装 `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` 位域。
///
/// 布局：bit0 `Usage`、bit1 `RGB_Range`（1 = limited）、bit2 `YCbCr_Matrix`
/// （1 = BT.709）、bit3 `YCbCr_xvYCC`、bit4–5 `Nominal_Range`。
fn color_space(
    usage: u32,
    rgb_limited: bool,
    bt709: bool,
    nominal_range: u32,
) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
        _bitfield: (usage & 1)
            | (u32::from(rgb_limited) << 1)
            | (u32::from(bt709) << 2)
            | ((nominal_range & 0b11) << 4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_space_bitfield_matches_d3d11_layout() {
        // 输出：BT.709 + 16-235 → bit2 与 bit4 置位。
        let limited = u32::try_from(D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235.0).unwrap_or(1);
        let output = color_space(0, false, true, limited);
        assert_eq!(output._bitfield, 0b1_0100);
        // 输入：full-range RGB → 全 0。
        let input = color_space(0, false, false, 0);
        assert_eq!(input._bitfield, 0);
    }
}
