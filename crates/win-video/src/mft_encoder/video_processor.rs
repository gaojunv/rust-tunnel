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
//!
//! # AYUV 4:4:4 预转换（色度质量修复）
//!
//! 实测（本机 Intel Graphics，驱动 32.0.101.9033）VideoProcessor 的 BGRA→NV12
//! **色度不是 2×2 盒式平均，而是「横向只取左列、纵向平均两行」**——对三种测试
//! 图案，它与「左列两像素平均」的逐样本误差精确为 0.00，与正确盒式平均的平均差
//! 5.4–47/255。后果是 ClearType 小字的红/蓝彩边像素被原样保留（不被邻像素稀释），
//! 单个彩边变成一块 2×2 的饱和色，复杂汉字整字染色。
//!
//! 修复走**两步 GPU 转换**、完全不回读 CPU：
//!
//! 1. [`AyuvConverter`] 用像素着色器算出 **AYUV 4:4:4** 值：亮度逐像素算，色度按
//!    2×2 块平均后复制给块内四个像素（与 [`super::convert`] 同一套 BT.709 limited
//!    常数）；
//! 2. 现有的 VideoProcessor 再做 BGRA→NV12 的 `Blt`。色度在 2×2 块内是常量，驱动
//!    再怎么「抽取左列」都不影响结果。
//!
//! **中间为什么不是一张 AYUV 纹理**（实测的 D3D11 限制，不是某块卡的偶发问题）：
//! `CheckFormatSupport(AYUV)` 会给出 `RENDER_TARGET`/`SHADER_LOAD`/TypedUAV/VP 输出
//! 等能力位，但那些位是给 **VideoProcessor** 用的；对普通视图创建，D3D11 运行时
//! 一律拒绝——`CreateRenderTargetView` / `CreateShaderResourceView` /
//! `CreateUnorderedAccessView` / `CreateVideoProcessorOutputView` 全部返回
//! `E_INVALIDARG`（NV12 同样，只有 BGRA8 这类普通格式正常；本机 Intel Graphics
//! 32.0.101.9033 与 NVIDIA RTX 5070 Ti 两边结果一致，换 FL 11_1 设备、加
//! `MISC_SHARED`/`KEYEDMUTEX`、按 BGRA/RGBA 建 cast 视图都一样）。跨格式
//! `CopyResource(BGRA→AYUV)` 返回成功却**静默不拷贝**（VP 读到的仍是全 0），BGRA
//! 纹理上的 `FourCC='AYUV'` 输入视图是 `DXGI_ERROR_UNSUPPORTED`。结论：AYUV 纹理
//! 在 GPU 侧**没有任何可写入口**（只有 VP 输入视图可读）。
//!
//! 因此着色器不写 AYUV 字节，而是把想要的 **AYUV 4:4:4 值**用 BT.709 limited 的
//! **逆矩阵**编回 RGB，写进一张普通的 BGRA8 渲染目标；VideoProcessor 的现有
//! BGRA→NV12 `Blt` 再把它正向变换回 YUV。RGB↔YUV 是线性映射，所以「逐像素 Y +
//! 块平均 U/V」在两端严格对应（缩放时也一样：对 RGB 做加权平均等价于对 Y/U/V 做
//! 同样的加权平均），实测两种四色块图案的色度误差为 **0.00**。
//!
//! **已知上限（数学限制，不是实现缺陷）**：RGB 中间表示只能承载 RGB 立方体内的
//! 颜色。若某个像素的真实亮度是黑（Y=16）或白（Y=235），它能携带的色度必然为零
//! ——而 ClearType 彩边恰好是「黑笔画旁边一列饱和彩边」，块平均色度对这些极值像素
//! 而言超出 RGB 色域，逆变换会被 `saturate` 裁掉。因此 `pattern_clear_type` 上色度
//! 只能做到平均差 ~3.1/255（旧的驱动路径 ~5.4/255）、最大 17，无法像纯色块那样归零；
//! 被裁掉的像素亮度也会跟着偏一点（实测平均 1.1、最大 +6/255，黑笔画上表现为轻微
//! 提亮）。做到严格精确必须把 YUV 直接写进一张 YUV 纹理，而上面已证明 D3D11 不
//! 提供这个入口。**取舍**：着色器保留最大色度（宁可让极值像素的亮度偏几级，也不为
//! 了保住亮度把色度退化成中性——那等于退回旧路径）。
//!
//! 着色器用运行期 `LoadLibrary("d3dcompiler_47.dll")` 编译（**禁止静态导入**：
//! 可选 DLL 的静态导入会让进程在 `main` 之前加载失败），`Library` 放在
//! [`D3D_COMPILER`] 里保活。整条预转换是**尽力而为**的：着色器编译、渲染目标/
//! 视图创建、或某帧渲染失败，都静默退回原来的 BGRA→NV12 路径（记一条 `tracing`），
//! 绝不让调用方失败。

use crate::capture::CapturedTexture;
use crate::d3d11::D3d11;
use crate::WinVideoError;
use std::ffi::{c_char, c_void};
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::OnceLock;
use windows::core::{HRESULT, Interface as _, BOOL};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11ClassLinkage, ID3D11DepthStencilView, ID3D11Device, ID3D11DeviceContext,
    ID3D11InputLayout, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VIDEO_ENCODER,
    D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VIEWPORT,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
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
    /// BGRA→AYUV 4:4:4 预转换；`None` = 走原来的 BGRA→NV12 直通路径。
    ayuv: Option<AyuvConverter>,
}

impl std::fmt::Debug for Nv12Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nv12Converter")
            .field("input_key", &self.input_key)
            .field("outputs", &self.output_textures.len())
            .field("ayuv", &self.ayuv.is_some())
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
        Self::new_with_usage(
            device,
            input_width,
            input_height,
            input_format,
            output_width,
            output_height,
            fps,
            D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        )
    }

    /// 同 [`Self::new`]，但显式指定 `D3D11_VIDEO_PROCESSOR_CONTENT_DESC.Usage`。
    ///
    /// 生产路径固定用 `PLAYBACK_NORMAL`；存在的唯一理由是诊断——`OPTIMAL_QUALITY`
    /// 会让驱动把重采样（含色度下采样）往画质一侧偏，用来验证「色度变差是不是
    /// 驱动的重采样滤波导致的」。
    #[expect(
        clippy::too_many_arguments,
        reason = "既有构造签名（诊断测试按 8 个参数直接调用），保持 ABI/API 不变成新增结构体"
    )]
    pub(super) fn new_with_usage(
        device: &D3d11,
        input_width: u32,
        input_height: u32,
        input_format: DXGI_FORMAT,
        output_width: u32,
        output_height: u32,
        fps: u32,
        usage: D3D11_VIDEO_USAGE,
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
            Usage: usage,
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

        // 输入：full-range RGB（桌面，或预转换后的 BGRA 逆编码）；YCbCr 矩阵对
        // RGB 输入无意义，留 0。`blt()` 每帧会连同输出颜色空间一起重设一次。
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

        // AYUV 4:4:4 预转换只在 BGRA 输入上有意义；任何一环失败都只是「退回原路径」，
        // 不能让转换器创建失败（原路径始终可用）。
        let ayuv = if input_format == DXGI_FORMAT_B8G8R8A8_UNORM
            || input_format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        {
            match AyuvConverter::new(
                device,
                &video_device,
                &enumerator,
                input_width,
                input_height,
            ) {
                Ok(converter) => {
                    tracing::debug!(
                        input = %format!("{input_width}x{input_height}"),
                        "AYUV 4:4:4 预转换已启用（色度按 2×2 盒式平均）"
                    );
                    Some(converter)
                }
                Err(err) => {
                    tracing::info!(
                        %err,
                        "AYUV 预转换不可用，回退到 BGRA→NV12（色度会被驱动按左列抽取）"
                    );
                    None
                }
            }
        } else {
            None
        };

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
            ayuv,
        })
    }

    /// 仅测试用：强制关闭 AYUV 预转换，退回原来的 BGRA→NV12 直通路径。
    ///
    /// 诊断测试用它在**同一个**转换器上做新旧路径的 A/B，避免重复建设备。
    #[cfg(test)]
    pub(super) fn disable_ayuv(&mut self) {
        self.ayuv = None;
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
        // 先尝试 AYUV 4:4:4 预转换；任何失败都只是永久退回 BGRA 直通，不向上报错。
        let ayuv_ready = match self.ayuv.as_mut() {
            Some(ayuv) => match ayuv.render(frame) {
                Ok(()) => true,
                Err(err) => {
                    tracing::info!(
                        %err,
                        "AYUV 预转换渲染失败，本次起回退到 BGRA→NV12"
                    );
                    false
                }
            },
            None => false,
        };
        if !ayuv_ready {
            self.ayuv = None;
        }

        let view = if ayuv_ready {
            self.ayuv
                .as_ref()
                .map(|ayuv| ayuv.input_view.clone())
                .ok_or_else(|| WinVideoError::Encode("AYUV 输入视图缺失".to_owned()))?
        } else {
            self.input_view(frame)?
        };
        self.blt(&view)
    }

    /// 一次 `VideoProcessorBlt`：输入视图 → 轮转的 NV12 输出纹理。
    ///
    /// 两条路径的输入都是 **full-range BGRA**（AYUV 路径只是把 AYUV 值逆编码进了
    /// BGRA 通道），所以输入颜色空间固定为 RGB full；输出始终是 BT.709 limited。
    fn blt(
        &mut self,
        view: &ID3D11VideoProcessorInputView,
    ) -> Result<ID3D11Texture2D, WinVideoError> {
        let index = self.next_output % self.output_views.len().max(1);
        let output_view = self
            .output_views
            .get(index)
            .ok_or_else(|| WinVideoError::Encode("VideoProcessor 输出视图缺失".to_owned()))?;

        // 输出始终是 BT.709 limited（16-235）。
        let limited = u32::try_from(D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235.0).unwrap_or(1);
        let output_space = color_space(0, false, true, limited);
        let input_space = color_space(0, false, false, 0);

        // `pInputSurface` 是 ManuallyDrop：Blt 不接管所有权，调用后手动取出来释放。
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            OutputIndex: 0,
            InputFrameOrField: 0,
            ..Default::default()
        };
        stream.pInputSurface = ManuallyDrop::new(Some(view.clone()));

        // SAFETY: `processor`/`output_view` 有效且由同一设备创建，`stream` 指向
        // 本栈帧；Blt 在立即上下文上同步完成。颜色空间设置是纯状态写入。
        let result = unsafe {
            self.video_context
                .VideoProcessorSetStreamColorSpace(&self.processor, 0, &raw const input_space);
            self.video_context
                .VideoProcessorSetOutputColorSpace(&self.processor, &raw const output_space);
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

        let view = create_input_view(&self.video_device, &self.enumerator, &frame.texture)?;

        if self.input_views.len() >= MAX_CACHED_INPUT_VIEWS {
            self.input_views.clear();
        }
        // 同时持有纹理引用：缓存键是裸指针，纹理必须活到视图被移除。
        self.input_views
            .push((raw, frame.texture.clone(), view.clone()));
        Ok(view)
    }
}

/// 给一张 2D 纹理创建 VideoProcessor 输入视图。
fn create_input_view(
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorInputView, WinVideoError> {
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
    // SAFETY: `texture` 是有效 2D 纹理（尺寸/格式已由调用方校验），`desc` 与
    // 输出指针都指向本栈帧。
    unsafe {
        video_device.CreateVideoProcessorInputView(
            texture,
            enumerator,
            &raw const desc,
            Some(&raw mut view),
        )
    }
    .map_err(|err| WinVideoError::Encode(format!("CreateVideoProcessorInputView 失败: {err}")))?;
    view.ok_or_else(|| WinVideoError::Encode("输入视图返回空接口".to_owned()))
}

// ---- BGRA → AYUV 4:4:4 预转换 ----

/// 内联 HLSL：全屏大三角形 + 逐像素亮度 + 2×2 盒式色度。
///
/// 顶点着色器用 `SV_VertexID` 直接生成覆盖整个渲染目标的三角形，不需要顶点缓冲/
/// 输入布局；像素着色器用 `SV_Position` 的整数像素坐标做 `Texture2D.Load`
/// （不需要 sampler，也就没有滤波带来的额外误差）。
///
/// 色彩常数与 [`super::convert`] 严格一致（BT.709 limited）：`Y = 16 + 219·Y'`、
/// `Cb/Cr = 128 ± 224·(B/R − Y')/(2(1−K))`。色度取整前先对 2×2 块求平均，块内
/// 四个像素写同一组 U/V——VideoProcessor 随后无论抽哪一列/做不做滤波都拿到同一个值。
///
/// **端点越界**：宽/高为奇数时右下角的 `b + 1` 会越界（`Load` 返回 0）。用
/// `GetDimensions` 夹到最后一个像素；偶数尺寸下夹取是恒等变换。
///
/// **输出的是「AYUV 值」的 RGB 逆编码**（驱动限制，理由见模块文档）：先按
/// BT.709 limited 算出 `(Y, Cb, Cr)`（Y 逐像素、Cb/Cr 取 2×2 块平均），再用同一
/// 矩阵的逆变换还原成 R'G'B' 写进 BGRA8 渲染目标。VideoProcessor 随后做
/// BGRA→NV12 时用同一套 BT.709 limited 正向变换，恢复出恰好是我们要的 YUV。
/// 任何一端矩阵/范围写错，`ayuv_chroma_matches_box_average` 都会立刻炸。
///
/// 逆变换（BT.709，R'/G'/B' 与 Y'/Cb/Cr 都是 0..1 归一化）：
/// `R' = Y' + 2(1−KR)·Cr`、`B' = Y' + 2(1−KB)·Cb`、
/// `G' = (Y' − KR·R' − KB·B') / KG`。
const AYUV_SHADER_SOURCE: &str = r"
struct VsOut { float4 pos : SV_Position; };

VsOut vs_main(uint vid : SV_VertexID) {
    float2 uv = float2((vid << 1) & 2, vid & 2);
    VsOut o;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

Texture2D<float4> src : register(t0);

float4 ps_main(VsOut input) : SV_Target {
    uint width;
    uint height;
    src.GetDimensions(width, height);
    int2 last = int2(int2(width, height) - 1);

    int2 p = min(int2(uint2(input.pos.xy)), last);
    int2 b0 = min(int2(uint2(p) & ~1u), last);
    int2 b1 = min(b0 + int2(1, 1), last);

    float3 c00 = src.Load(int3(b0, 0)).rgb;
    float3 c10 = src.Load(int3(b1.x, b0.y, 0)).rgb;
    float3 c01 = src.Load(int3(b0.x, b1.y, 0)).rgb;
    float3 c11 = src.Load(int3(b1, 0)).rgb;
    float3 cur = src.Load(int3(p, 0)).rgb;

    float3 avg = (c00 + c10 + c01 + c11) * 0.25;

    const float KR = 0.2126;
    const float KG = 0.7152;
    const float KB = 0.0722;
    float luma = dot(cur, float3(KR, KG, KB));
    float avg_luma = dot(avg, float3(KR, KG, KB));
    float cb = (avg.b - avg_luma) / (2.0 * (1.0 - KB));
    float cr = (avg.r - avg_luma) / (2.0 * (1.0 - KR));

    float Y = (16.0 + 219.0 * luma) / 255.0;
    float U = (128.0 + 224.0 * cb) / 255.0;
    float V = (128.0 + 224.0 * cr) / 255.0;

    // 反变换回 RGB：VideoProcessor 的 BGRA→NV12 会用同一套 BT.709 limited
    // 正向变换，把这里写下的 RGB 还原成上面的 (Y, U, V)。
    float yn = (Y - 16.0 / 255.0) * (255.0 / 219.0);
    float cbn = (U - 128.0 / 255.0) * (255.0 / 224.0);
    float crn = (V - 128.0 / 255.0) * (255.0 / 224.0);
    float r = yn + 2.0 * (1.0 - KR) * crn;
    float b = yn + 2.0 * (1.0 - KB) * cbn;
    float g = (yn - KR * r - KB * b) / KG;
    return float4(saturate(r), saturate(g), saturate(b), 1.0);
}
";

/// `D3DCompile` 的函数指针类型（`d3dcompiler.h` 的稳定 C ABI）。
///
/// 自己声明而不是用 `windows` crate 的静态绑定：后者会 `#[link(d3dcompiler)]`，
/// 在缺少该 DLL 的精简系统上**进程会在 `main` 之前加载失败**。项目规则是可选
/// DLL 一律运行期加载。
type D3DCompileFn = unsafe extern "system" fn(
    p_src_data: *const c_void,
    src_data_size: usize,
    p_source_name: *const c_char,
    p_defines: *const c_void,
    p_include: *mut c_void,
    p_entrypoint: *const c_char,
    p_target: *const c_char,
    flags1: u32,
    flags2: u32,
    pp_code: *mut Option<ID3DBlob>,
    pp_error_msgs: *mut Option<ID3DBlob>,
) -> HRESULT;

/// `D3DCOMPILE_OPTIMIZATION_LEVEL3`（`d3dcompiler.h`），避免为它引一个 windows 符号。
const D3DCOMPILE_OPTIMIZATION_LEVEL3: u32 = 1 << 15;

/// 运行期加载的 `d3dcompiler_47.dll`。
struct D3dCompiler {
    /// 库句柄必须活到函数指针失效之后；随本结构体在静态变量里活到进程结束。
    _library: libloading::Library,
    compile: D3DCompileFn,
}

/// 进程级编译器句柄：`None` = 本机没有 `d3dcompiler_47.dll`（或缺少导出），
/// 之后所有预转换尝试都直接回退，不再重复加载。
static D3D_COMPILER: OnceLock<Option<D3dCompiler>> = OnceLock::new();

/// 取（或首次加载）`D3DCompile`。
fn d3d_compiler() -> Option<&'static D3dCompiler> {
    D3D_COMPILER
        .get_or_init(|| {
            // SAFETY: 只加载系统目录下的微软编译器 DLL；符号在 `Library` 存活期间
            // 有效，而 `Library` 被移进静态变量、永远不会被卸载。
            unsafe {
                let library = libloading::Library::new("d3dcompiler_47.dll").ok()?;
                let compile = *library.get::<D3DCompileFn>(b"D3DCompile\0").ok()?;
                Some(D3dCompiler {
                    _library: library,
                    compile,
                })
            }
        })
        .as_ref()
}

/// 取 `ID3DBlob` 的字节内容（着色器字节码或编译错误信息）。
fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: `ID3DBlob` 的缓冲区在 blob 存活期间有效（我们只在调用方持有 blob
    // 的作用域内使用返回的切片）。
    unsafe {
        let pointer = blob.GetBufferPointer().cast::<u8>();
        let length = blob.GetBufferSize();
        if pointer.is_null() || length == 0 {
            return &[];
        }
        std::slice::from_raw_parts(pointer, length)
    }
}

/// 编译一段内联 HLSL。
fn compile_shader(source: &str, entry: &str, target: &str) -> Result<ID3DBlob, WinVideoError> {
    let compiler = d3d_compiler().ok_or_else(|| {
        WinVideoError::Encode("d3dcompiler_47.dll 不可用（无法编译 AYUV 着色器）".to_owned())
    })?;
    let entry = std::ffi::CString::new(entry)
        .map_err(|err| WinVideoError::Encode(format!("着色器入口名非法: {err}")))?;
    let target = std::ffi::CString::new(target)
        .map_err(|err| WinVideoError::Encode(format!("着色器目标名非法: {err}")))?;

    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: 所有指针都在调用期间有效；`D3DCompile` 只读源码并写两个 out 参数。
    // `pDefines`/`pInclude` 传空表示没有宏定义、不用 #include。
    let result = unsafe {
        (compiler.compile)(
            source.as_ptr().cast(),
            source.len(),
            c"ayuv.hlsl".as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            entry.as_ptr().cast(),
            target.as_ptr().cast(),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &raw mut code,
            &raw mut errors,
        )
    };
    if result.is_err() {
        let message = errors.as_ref().map_or_else(
            || format!("HRESULT {result:?}"),
            |blob| String::from_utf8_lossy(blob_bytes(blob)).into_owned(),
        );
        return Err(WinVideoError::Encode(format!(
            "AYUV 着色器编译失败（{entry:?} / {target:?}）: {message}"
        )));
    }
    code.ok_or_else(|| WinVideoError::Encode("D3DCompile 未返回字节码".to_owned()))
}

/// BGRA 采集纹理 → **AYUV 4:4:4 值**的 GPU 预转换器。
///
/// 只持有一张持久渲染目标（BGRA8，承载 AYUV 值的逆编码，见模块文档）：渲染后
/// 立即被同一条立即上下文上的 `VideoProcessorBlt` 消费，不需要轮转（NV12 输出
/// 纹理仍由 [`Nv12Converter`] 轮转）。
struct AyuvConverter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    /// 像素着色器写入的 BGRA8 纹理（承载 AYUV 值的逆编码），同时是 VideoProcessor
    /// 的输入。持久一张：渲染后同步被同一上下文的 `Blt` 消费。
    ///
    /// 生产代码只经 `input_view` 间接使用它；保留字段是为了明确所有权，诊断测试
    /// 还会直接读回它的字节。
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "所有权字段；诊断测试（cfg(test)）会读取")
    )]
    texture: ID3D11Texture2D,
    render_target: ID3D11RenderTargetView,
    /// 纹理的 VideoProcessor 输入视图（创建一次，长期复用）。
    input_view: ID3D11VideoProcessorInputView,
    /// BGRA 源纹理的着色器资源视图，按纹理裸指针缓存（采集池是 4 张纹理）。
    input_srvs: Vec<(usize, ID3D11Texture2D, ID3D11ShaderResourceView)>,
}

impl AyuvConverter {
    /// 建渲染目标、编译着色器、建立全部视图。
    ///
    /// # Errors
    /// D3D 编译器不可用、着色器编译失败、纹理/视图创建失败时返回
    /// [`WinVideoError::Encode`]。调用方捕获后只是回退老路径。
    fn new(
        device: &D3d11,
        video_device: &ID3D11VideoDevice,
        enumerator: &ID3D11VideoProcessorEnumerator,
        width: u32,
        height: u32,
    ) -> Result<Self, WinVideoError> {
        // 渲染目标是 **BGRA8**，不是 AYUV：本机 Intel 驱动（32.0.101.9033）对
        // AYUV 纹理的任何可写视图都返回 `E_INVALIDARG`（见模块文档）。BGRA8 是
        // 唯一能写、又能在 VideoProcessor 里当输入的选择——着色器因此把 AYUV 值
        // 逆编码成 RGB（见 `AYUV_SHADER_SOURCE`）。
        let (texture, render_target) = create_bgra_render_target(&device.device, width, height)?;
        let input_view = create_input_view(video_device, enumerator, &texture)?;

        let vertex_blob = compile_shader(AYUV_SHADER_SOURCE, "vs_main", "vs_5_0")?;
        let pixel_blob = compile_shader(AYUV_SHADER_SOURCE, "ps_main", "ps_5_0")?;

        let mut vertex_shader: Option<ID3D11VertexShader> = None;
        // SAFETY: 字节码来自本机 D3DCompile；输出指针指向栈上 Option。
        unsafe {
            device.device.CreateVertexShader(
                blob_bytes(&vertex_blob),
                None::<&ID3D11ClassLinkage>,
                Some(&raw mut vertex_shader),
            )
        }
        .map_err(|err| WinVideoError::Encode(format!("CreateVertexShader 失败: {err}")))?;
        let vertex_shader = vertex_shader
            .ok_or_else(|| WinVideoError::Encode("CreateVertexShader 返回空接口".to_owned()))?;

        let mut pixel_shader: Option<ID3D11PixelShader> = None;
        // SAFETY: 同上。
        unsafe {
            device.device.CreatePixelShader(
                blob_bytes(&pixel_blob),
                None::<&ID3D11ClassLinkage>,
                Some(&raw mut pixel_shader),
            )
        }
        .map_err(|err| WinVideoError::Encode(format!("CreatePixelShader 失败: {err}")))?;
        let pixel_shader = pixel_shader
            .ok_or_else(|| WinVideoError::Encode("CreatePixelShader 返回空接口".to_owned()))?;

        Ok(Self {
            device: device.device.clone(),
            context: device.context.clone(),
            width,
            height,
            vertex_shader,
            pixel_shader,
            texture,
            render_target,
            input_view,
            input_srvs: Vec::new(),
        })
    }

    /// 把一帧 BGRA 渲染成 AYUV 4:4:4 值的逆编码（写入 `self.texture`）。
    ///
    /// 渲染后解绑 SRV 与 RTV：这张纹理随后要当 VideoProcessor 的输入，句柄还
    /// 挂着会构成读写 hazard（D3D11 会告警，部分驱动读到未定义内容）。
    ///
    /// # Errors
    /// SRV 创建失败或 D3D 调用失败时返回 [`WinVideoError::Encode`]。
    #[expect(
        clippy::cast_precision_loss,
        reason = "视口尺寸来自纹理描述（≤ 限定的采集尺寸），f32 可精确表示"
    )]
    fn render(&mut self, frame: &CapturedTexture) -> Result<(), WinVideoError> {
        let srv = self.shader_resource_view(&frame.texture)?;
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.width as f32,
            Height: self.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        // SAFETY: 全部对象都由本转换器持有的有效接口；立即上下文上的调用顺序即
        // GPU 执行顺序，随后同一上下文上的 VideoProcessorBlt 能看到渲染结果。
        unsafe {
            self.context.OMSetRenderTargets(
                Some(&[Some(self.render_target.clone())]),
                None::<&ID3D11DepthStencilView>,
            );
            self.context.RSSetViewports(Some(&[viewport]));
            self.context.IASetInputLayout(None::<&ID3D11InputLayout>);
            self.context
                .IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex_shader, None);
            self.context.PSSetShader(&self.pixel_shader, None);
            self.context
                .PSSetShaderResources(0, Some(&[Some(srv)]));
            self.context.Draw(3, 0);
            // 解绑 SRV/RTV：这张纹理随后要当 VideoProcessor 的输入，句柄还挂着会
            // 构成读写 hazard（D3D11 会告警，部分驱动读到未定义内容）。
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context
                .OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
        }
        Ok(())
    }

    /// 取得（或创建并缓存）BGRA 源纹理的着色器资源视图。
    fn shader_resource_view(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11ShaderResourceView, WinVideoError> {
        let raw = texture.as_raw() as usize;
        if let Some((_, _, srv)) = self
            .input_srvs
            .iter()
            .find(|(pointer, _, _)| *pointer == raw)
        {
            return Ok(srv.clone());
        }

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` 有效；`desc` 指向本栈帧。
        unsafe { texture.GetDesc(&raw mut desc) };
        // `_SRGB` 纹理的默认 SRV 会做 sRGB→线性解码，而 VideoProcessor 的 BGRA
        // 输入是按**原始字节**解释的。显式用 UNORM 视图保持两条路径数值一致。
        let format = if desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB {
            DXGI_FORMAT_B8G8R8A8_UNORM
        } else {
            desc.Format
        };
        let view_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        // SAFETY: `texture` 带 `SHADER_RESOURCE` 绑定位（采集纹理必然有）；`desc`
        // 与输出指针都在本栈帧。
        let explicit = unsafe {
            self.device.CreateShaderResourceView(
                texture,
                Some(&raw const view_desc),
                Some(&raw mut srv),
            )
        };
        if explicit.is_err() {
            // 少数老驱动不接受 `_SRGB` 纹理上的 UNORM cast 视图；退回默认描述
            // （会做 sRGB 解码，色彩略偏，但比整条预转换失效好）。这种纹理在本项目
            // 的采集路径里本来就不常见（WGC/DXGI 给的是 UNORM）。
            // SAFETY: 同上，`texture` 有效且拥有 `SHADER_RESOURCE` 绑定位。
            unsafe { self.device.CreateShaderResourceView(texture, None, Some(&raw mut srv)) }
                .map_err(|err| {
                    WinVideoError::Encode(format!("CreateShaderResourceView 失败: {err}"))
                })?;
        }
        let srv = srv.ok_or_else(|| WinVideoError::Encode("SRV 返回空接口".to_owned()))?;

        if self.input_srvs.len() >= MAX_CACHED_INPUT_VIEWS {
            self.input_srvs.clear();
        }
        // 同时持有纹理引用：缓存键是裸指针，纹理必须活到视图被移除。
        self.input_srvs
            .push((raw, texture.clone(), srv.clone()));
        Ok(srv)
    }
}

/// 创建 BGRA8 渲染目标纹理与视图（像素着色器写 AYUV 逆编码的地方）。
fn create_bgra_render_target(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView), WinVideoError> {
    let bind = D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: u32::try_from(bind)
            .map_err(|_| WinVideoError::Encode("BGRA 渲染目标绑定标志溢出".to_owned()))?,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` 合法；输出指针指向栈上 Option。
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }.map_err(
        |err| WinVideoError::Encode(format!("创建 BGRA 渲染目标 {width}x{height} 失败: {err}")),
    )?;
    let texture = texture
        .ok_or_else(|| WinVideoError::Encode("CreateTexture2D(BGRA) 返回空纹理".to_owned()))?;

    let mut view: Option<ID3D11RenderTargetView> = None;
    // SAFETY: `texture` 带 `RENDER_TARGET` 绑定位；输出指针指向栈上 Option。
    unsafe { device.CreateRenderTargetView(&texture, None, Some(&raw mut view)) }
        .map_err(|err| WinVideoError::Encode(format!("CreateRenderTargetView(BGRA) 失败: {err}")))?;
    let view = view.ok_or_else(|| WinVideoError::Encode("渲染目标返回空接口".to_owned()))?;
    Ok((texture, view))
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
    //! 纯逻辑单测 + **离线**驱动行为诊断（不需要真实桌面/编码会话）。

    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::print_stdout,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]

    use super::*;
    use crate::capture::CapturedTexture;
    use crate::mft_encoder::convert::rgba_to_nv12;
    use crate::pixels::bgra_to_rgba;
    use std::sync::Arc;
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Device, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_MAP_READ, D3D11_SUBRESOURCE_DATA, D3D11_USAGE, D3D11_USAGE_STAGING,
        D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

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

    // ---- 诊断：VideoProcessor 的 BGRA→NV12 色度到底怎么采样 ----

    /// 诊断图尺寸。
    const W: u32 = 64;
    /// 诊断图尺寸。
    const H: u32 = 64;

    /// 优先 Intel 适配器（与现场被控端一致），否则任意硬件适配器。
    fn intel_or_default_device() -> Option<Arc<D3d11>> {
        let adapters = crate::d3d11::adapters().ok()?;
        let chosen = adapters
            .iter()
            .find(|adapter| !adapter.software && adapter.vendor_id == crate::d3d11::VENDOR_INTEL)
            .or_else(|| adapters.iter().find(|adapter| !adapter.software))?;
        eprintln!("诊断设备：{}（vendor {:#x}）", chosen.name, chosen.vendor_id);
        crate::d3d11::create(Some(chosen.index))
            .ok()
            .map(Arc::new)
    }

    /// 模拟 ClearType 小字：白底 + 每 8 列一根 1px 黑竖笔，左缘偏红、右缘偏蓝。
    ///
    /// 这正是 4:2:0 会失真的那类内容：**同一个 2×2 块里同时有红边、蓝边和黑笔画**。
    fn pattern_clear_type(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..height {
            for x in 0..width {
                let (r, g, b) = match x % 8 {
                    0 => (0, 0, 0),
                    1 => (255, 170, 170),
                    7 => (170, 170, 255),
                    _ => (255, 255, 255),
                };
                out.extend_from_slice(&[b, g, r, 255]);
            }
        }
        out
    }

    /// 每个 2×2 块四角分别是红/绿/蓝/白：盒式平均与「点采样」的差异被放到最大，
    /// 用来区分驱动到底做的是平均还是挑一个像素。
    fn pattern_blocks(width: u32, height: u32) -> Vec<u8> {
        let colors = [(255u8, 0u8, 0u8), (0, 255, 0), (0, 0, 255), (255, 255, 255)];
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = colors[((y % 2) * 2 + (x % 2)) as usize];
                out.extend_from_slice(&[b, g, r, 255]);
            }
        }
        out
    }

    /// 建一张 BGRA 源纹理。
    fn create_bgra_texture(
        device: &ID3D11Device,
        data: &[u8],
        width: u32,
        height: u32,
    ) -> Option<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: u32::try_from(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0)
                .ok()?,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr().cast(),
            SysMemPitch: width * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc`/`init` 指向本栈帧，`data` 在调用期间存活。
        unsafe { device.CreateTexture2D(&raw const desc, Some(&raw const init), Some(&raw mut texture)) }
            .ok()?;
        texture
    }

    /// 把 NV12 纹理拷到 staging 并读回紧凑的 `(Y, UV)`。
    fn read_nv12(
        device: &D3d11,
        texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
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
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).ok()?,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` 合法，输出指针指向栈上 Option。
        unsafe { device.device.CreateTexture2D(&raw const desc, None, Some(&raw mut staging)) }
            .ok()?;
        let staging = staging?;
        // SAFETY: 两张纹理尺寸/格式一致。
        unsafe { device.context.CopyResource(&staging, texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging 带 CPU_ACCESS_READ；`mapped` 指向本栈帧。
        unsafe { device.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }.ok()?;
        let pitch = usize::try_from(mapped.RowPitch).ok()?;
        let (wu, hu) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
        // SAFETY: NV12 是 Y 平面（H 行）+ UV 平面（H/2 行），每行至少 pitch 字节。
        let data = unsafe {
            std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                pitch.checked_mul(hu + hu / 2)?,
            )
        };
        let mut y = Vec::with_capacity(wu * hu);
        for row in 0..hu {
            y.extend_from_slice(&data[row * pitch..row * pitch + wu]);
        }
        let uv_base = pitch * hu;
        let mut uv = Vec::with_capacity(wu * hu / 2);
        for row in 0..(hu / 2) {
            let start = uv_base + row * pitch;
            uv.extend_from_slice(&data[start..start + wu]);
        }
        // SAFETY: 与上面的 Map 严格配对。
        unsafe { device.context.Unmap(&staging, 0) };
        Some((y, uv))
    }

    /// 把紧凑 NV12 拆成 `(Y, UV)`。
    fn split_nv12(nv12: &[u8], width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
        let pixels = usize::try_from(width * height).unwrap_or(0);
        (nv12[..pixels].to_vec(), nv12[pixels..].to_vec())
    }

    /// 两个色度平面的 `(平均绝对差, 最大绝对差)`。
    fn uv_stats(actual: &[u8], expected: &[u8]) -> (f64, u32) {
        let n = actual.len().min(expected.len()).max(1);
        let mut sum = 0u64;
        let mut max = 0u32;
        for (a, b) in actual.iter().zip(expected).take(n) {
            let d = u32::from(a.abs_diff(*b));
            sum += u64::from(d);
            max = max.max(d);
        }
        (sum as f64 / n as f64, max)
    }

    /// 用「每个 2×2 块都填成该块某个角的颜色」的办法得到一个**点采样**色度参考。
    ///
    /// `corner`：0 = 左上、1 = 右上、2 = 左下、3 = 右下。
    fn point_sampled_uv(bgra: &[u8], width: u32, height: u32, corner: usize) -> Vec<u8> {
        let (wu, hu) = (usize::try_from(width).unwrap_or(0), usize::try_from(height).unwrap_or(0));
        let (cx, cy) = (corner % 2, corner / 2);
        let mut flat = vec![0u8; wu * hu * 4];
        for y in 0..hu {
            for x in 0..wu {
                let sx = (x / 2) * 2 + cx;
                let sy = (y / 2) * 2 + cy;
                let src = (sy * wu + sx) * 4;
                let dst = (y * wu + x) * 4;
                flat[dst..dst + 4].copy_from_slice(&bgra[src..src + 4]);
            }
        }
        let mut nv12 = Vec::new();
        rgba_to_nv12(&bgra_to_rgba(&flat), width, height, &mut nv12).unwrap_or_default();
        split_nv12(&nv12, width, height).1
    }

    /// 每个 2×2 块四角分别是红/绿/蓝/灰：盒式平均、左列、右列、上行、下行四种
    /// 「错误采样」彼此都不同，用来精确判定驱动到底取了哪几个像素。
    fn pattern_quad(width: u32, height: u32) -> Vec<u8> {
        let colors = [
            (255u8, 0u8, 0u8),
            (0, 255, 0),
            (0, 0, 255),
            (128, 128, 128),
        ];
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = colors[((y % 2) * 2 + (x % 2)) as usize];
                out.extend_from_slice(&[b, g, r, 255]);
            }
        }
        out
    }

    /// 每个 2×2 块只对 `mask` 选中的像素求平均，再把整块填成该颜色后转 NV12。
    ///
    /// mask 顺序：`[左上, 右上, 左下, 右下]`。产生的色度就是「只对这几个像素做平均」
    /// 的参考值，用来反推 VideoProcessor 实际用了哪几个像素。
    fn region_average_uv(bgra: &[u8], width: u32, height: u32, mask: [bool; 4]) -> Vec<u8> {
        let (wu, hu) = (
            usize::try_from(width).unwrap_or(0),
            usize::try_from(height).unwrap_or(0),
        );
        let mut flat = vec![0u8; wu * hu * 4];
        for by in 0..(hu / 2) {
            for bx in 0..(wu / 2) {
                let mut sum = [0u32; 3];
                let mut count = 0u32;
                for (index, (dx, dy)) in [(0usize, 0usize), (1, 0), (0, 1), (1, 1)]
                    .iter()
                    .enumerate()
                {
                    if !mask[index] {
                        continue;
                    }
                    let src = ((by * 2 + dy) * wu + bx * 2 + dx) * 4;
                    // BGRA → RGB 累加。
                    sum[0] += u32::from(bgra[src + 2]);
                    sum[1] += u32::from(bgra[src + 1]);
                    sum[2] += u32::from(bgra[src]);
                    count += 1;
                }
                let count = count.max(1);
                let rgb = [sum[0] / count, sum[1] / count, sum[2] / count];
                for (dx, dy) in [(0usize, 0usize), (1, 0), (0, 1), (1, 1)] {
                    let dst = ((by * 2 + dy) * wu + bx * 2 + dx) * 4;
                    flat[dst] = u8::try_from(rgb[2]).unwrap_or(0);
                    flat[dst + 1] = u8::try_from(rgb[1]).unwrap_or(0);
                    flat[dst + 2] = u8::try_from(rgb[0]).unwrap_or(0);
                    flat[dst + 3] = 255;
                }
            }
        }
        let mut nv12 = Vec::new();
        rgba_to_nv12(&bgra_to_rgba(&flat), width, height, &mut nv12).unwrap_or_default();
        split_nv12(&nv12, width, height).1
    }

    /// 把色度平面水平平移 `dx` 个色度样本（用于检测共址相位）。
    fn shift_uv(uv: &[u8], width: u32, height: u32, dx: isize) -> Vec<u8> {
        let (wu, hu) = (usize::try_from(width).unwrap_or(0), usize::try_from(height).unwrap_or(0));
        let (uv_w, uv_h) = (wu / 2, hu / 2);
        let mut out = vec![128u8; uv.len()];
        for y in 0..uv_h {
            for x in 0..uv_w {
                let sx = x.cast_signed() + dx;
                if sx < 0 || sx >= uv_w.cast_signed() {
                    continue;
                }
                let src = (y * uv_w + usize::try_from(sx).unwrap_or(0)) * 2;
                let dst = (y * uv_w + x) * 2;
                out[dst] = uv[src];
                out[dst + 1] = uv[src + 1];
            }
        }
        out
    }

    /// 逐块判断 VP 的色度最像「盒式平均」还是某个角的「点采样」，并统计占比。
    fn classify_blocks(vp_uv: &[u8], refs: &[&[u8]]) -> Vec<usize> {
        let mut counts = vec![0usize; refs.len()];
        for block in 0..(vp_uv.len() / 2) {
            let mut best = 0usize;
            let mut best_err = u32::MAX;
            for (index, reference) in refs.iter().enumerate() {
                let err = u32::from(vp_uv[block * 2].abs_diff(reference[block * 2]))
                    + u32::from(vp_uv[block * 2 + 1].abs_diff(reference[block * 2 + 1]));
                if err < best_err {
                    best_err = err;
                    best = index;
                }
            }
            counts[best] += 1;
        }
        counts
    }

    /// **探路**：如果要自己在 GPU 上做 BGRA→NV12，哪些格式可以作为
    /// 渲染目标 / UAV 使用（决定走「像素着色器 → AYUV → VideoProcessor」还是
    /// 「compute → 双平面」）。
    #[test]
    fn probe_gpu_conversion_format_support() {
        // D3D11_FORMAT_SUPPORT_* 位（公开常量，避免为诊断再引一批 windows 符号）。
        const TEXTURE2D: u32 = 0x0000_0020;
        const SHADER_SAMPLE: u32 = 0x0000_0200;
        const RENDER_TARGET: u32 = 0x0000_4000;
        const UNORDERED_ACCESS: u32 = 0x0200_0000;
        const VIDEO_PROCESSOR_INPUT: u32 = 0x2000_0000;
        const VIDEO_PROCESSOR_OUTPUT: u32 = 0x1000_0000;
        let Some(device) = intel_or_default_device() else {
            eprintln!("跳过探路：没有可用的 D3D11 硬件适配器");
            return;
        };
        let formats: [(&str, DXGI_FORMAT); 6] = [
            ("NV12", DXGI_FORMAT_NV12),
            ("AYUV", windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_AYUV),
            (
                "R8_UNORM",
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8_UNORM,
            ),
            (
                "R8G8_UNORM",
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8_UNORM,
            ),
            ("BGRA8", DXGI_FORMAT_B8G8R8A8_UNORM),
            (
                "RGBA8",
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            ),
        ];
        for (name, format) in formats {
            // SAFETY: 纯能力查询，无副作用。
            let flags = unsafe { device.device.CheckFormatSupport(format) }.unwrap_or(0);
            let mark = |bit: u32| if flags & bit != 0 { "✓" } else { "✗" };
            println!(
                "{name}: Texture2D {}  RenderTarget {}  ShaderSample {}  UAV {}  VP输入 {}  VP输出 {}  (flags {flags:#x})",
                mark(TEXTURE2D),
                mark(RENDER_TARGET),
                mark(SHADER_SAMPLE),
                mark(UNORDERED_ACCESS),
                mark(VIDEO_PROCESSOR_INPUT),
                mark(VIDEO_PROCESSOR_OUTPUT)
            );
        }
    }

    /// **测成本**：1080p 的「GPU→CPU 回读 + 转 NV12」到底多少毫秒。
    ///
    /// 用来回答「把 BGRA→NV12 搬回 CPU 会不会明显加延迟」。回读路径与生产一致：
    /// `CopyResource` 到 staging → `Map` → 按 `RowPitch` 拷行；转换用的是生产同一份
    /// `rgba_to_nv12`（BT.709 limited + 2×2 盒式色度）。
    #[test]
    fn measure_1080p_readback_and_cpu_nv12_cost() {
        let Some(device) = intel_or_default_device() else {
            eprintln!("跳过测量：没有可用的 D3D11 硬件适配器");
            return;
        };
        let (w, h) = (1920_u32, 1080_u32);
        // 渐变内容：避免全同色被驱动做旁路优化。
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                bgra.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]);
            }
        }
        let Some(texture) = create_bgra_texture(&device.device, &bgra, w, h) else {
            eprintln!("跳过测量：建源纹理失败");
            return;
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0),
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` 合法；输出指针指向栈上 Option。
        if unsafe { device.device.CreateTexture2D(&raw const desc, None, Some(&raw mut staging)) }
            .is_err()
        {
            eprintln!("跳过测量：建 staging 纹理失败");
            return;
        }
        let Some(staging) = staging else { return };

        let (wu, hu) = (usize::try_from(w).unwrap_or(0), usize::try_from(h).unwrap_or(0));
        let row_bytes = wu * 4;
        let mut best_readback = f64::MAX;
        let mut copy_out = vec![0u8; row_bytes * hu];
        for _ in 0..10 {
            let started = std::time::Instant::now();
            // SAFETY: 两张纹理尺寸/格式一致；Map 与 Unmap 严格配对。
            unsafe { device.context.CopyResource(&staging, &texture) };
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if unsafe { device.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }
                .is_err()
            {
                return;
            }
            let pitch = usize::try_from(mapped.RowPitch).unwrap_or(row_bytes);
            // SAFETY: Map 保证 RowPitch × H 可读。
            let source =
                unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * hu) };
            for row in 0..hu {
                copy_out[row * row_bytes..(row + 1) * row_bytes]
                    .copy_from_slice(&source[row * pitch..row * pitch + row_bytes]);
            }
            // SAFETY: 与上面的 Map 配对。
            unsafe { device.context.Unmap(&staging, 0) };
            best_readback = best_readback.min(started.elapsed().as_secs_f64() * 1000.0);
        }

        let mut best_convert = f64::MAX;
        let mut best_swap = f64::MAX;
        let mut nv12 = Vec::new();
        for _ in 0..10 {
            let started = std::time::Instant::now();
            let rgba = bgra_to_rgba(&copy_out);
            best_swap = best_swap.min(started.elapsed().as_secs_f64() * 1000.0);
            let started = std::time::Instant::now();
            rgba_to_nv12(&rgba, w, h, &mut nv12).expect("CPU NV12 转换");
            best_convert = best_convert.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        // 复用缓冲的理想情况：只算算法本身，不含每帧 8.3 MB 的分配与清零。
        let mut reused_rgba = vec![0u8; row_bytes * hu];
        let mut best_reused = f64::MAX;
        for _ in 0..10 {
            let started = std::time::Instant::now();
            for (src, dst) in copy_out
                .as_chunks::<4>()
                .0
                .iter()
                .zip(reused_rgba.as_chunks_mut::<4>().0)
            {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = src[3];
            }
            rgba_to_nv12(&reused_rgba, w, h, &mut nv12).expect("CPU NV12 转换");
            best_reused = best_reused.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "1080p 回读（CopyResource+Map+拷行）：{best_readback:.2} ms；BGRA→RGBA：{best_swap:.2} ms；RGBA→NV12：{best_convert:.2} ms；合计 {:.2} ms；复用缓冲合计 {best_reused:.2} ms",
            best_readback + best_swap + best_convert
        );
        assert!(!nv12.is_empty());
    }

    /// **诊断主测**：VP 的色度下采样 vs 我们自己的 2×2 盒式平均 vs 点采样。
    ///
    /// 输出的是数值，不做强断言：驱动行为随厂商/版本变化，这里只负责把「VP 到底
    /// 做了什么」量化出来，供我们决定要不要把 BGRA→NV12 搬回自己手里。
    #[test]
    fn diagnose_video_processor_chroma_downsampling() {
        let Some(device) = intel_or_default_device() else {
            eprintln!("跳过诊断：没有可用的 D3D11 硬件适配器");
            return;
        };
        let patterns = [
            ("ClearType 竖笔（白底/红边/黑笔画/蓝边）", pattern_clear_type(W, H)),
            ("2x2 四色块（红/绿/蓝/白）", pattern_blocks(W, H)),
            ("2x2 四色块（红/绿/蓝/灰，区分左右列与上下行）", pattern_quad(W, H)),
        ];
        let usages = [
            ("PLAYBACK_NORMAL", D3D11_VIDEO_USAGE_PLAYBACK_NORMAL),
            ("OPTIMAL_QUALITY", D3D11_VIDEO_USAGE_OPTIMAL_QUALITY),
        ];

        for (pattern_name, bgra) in &patterns {
            // 期望值：我们自己的 CPU 实现（2×2 盒式平均，BT.709 limited）。
            let rgba = bgra_to_rgba(bgra);
            let mut cpu_nv12 = Vec::new();
            rgba_to_nv12(&rgba, W, H, &mut cpu_nv12).expect("CPU NV12 转换");
            let (_, cpu_uv) = split_nv12(&cpu_nv12, W, H);
            // 四角点采样参考。
            let points: [Vec<u8>; 4] = [
                point_sampled_uv(bgra, W, H, 0),
                point_sampled_uv(bgra, W, H, 1),
                point_sampled_uv(bgra, W, H, 2),
                point_sampled_uv(bgra, W, H, 3),
            ];

            println!("\n=== {pattern_name} ===");
            for (usage_name, usage) in usages {
                let Some(texture) = create_bgra_texture(&device.device, bgra, W, H) else {
                    println!("{usage_name}: 建源纹理失败，跳过");
                    continue;
                };
                let captured =
                    CapturedTexture::from_texture(Arc::clone(&device), texture, 0, W, H);
                let Ok(mut converter) = Nv12Converter::new_with_usage(
                    &device,
                    W,
                    H,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    W,
                    H,
                    30,
                    usage,
                ) else {
                    println!("{usage_name}: VideoProcessor 不可用，跳过");
                    continue;
                };
                // 本诊断要量化的是**驱动原生 BGRA→NV12** 的色度采样。AYUV 预转换
                // 会把色度换成我们自己的 2×2 盒式平均，必须显式关掉。
                converter.disable_ayuv();
                let Ok(output) = converter.convert(&captured) else {
                    println!("{usage_name}: Blt 失败，跳过");
                    continue;
                };
                let Some((vp_y, vp_uv)) = read_nv12(&device, &output, W, H) else {
                    println!("{usage_name}: 读回失败，跳过");
                    continue;
                };

                let (box_mean, box_max) = uv_stats(&vp_uv, &cpu_uv);
                let (luma_mean, luma_max) = uv_stats(&vp_y, &split_nv12(&cpu_nv12, W, H).0);
                println!(
                    "{usage_name}: Y 平均差 {luma_mean:.2} 最大 {luma_max}；UV vs 盒式平均 平均差 {box_mean:.2} 最大 {box_max}"
                );
                // 反推驱动到底对哪几个像素求平均：左列 / 右列 / 上行 / 下行。
                let regions = [
                    ("左列(左上+左下)", [true, false, true, false]),
                    ("右列(右上+右下)", [false, true, false, true]),
                    ("上行(左上+右上)", [true, true, false, false]),
                    ("下行(左下+右下)", [false, false, true, true]),
                ];
                for (name, mask) in regions {
                    let reference = region_average_uv(bgra, W, H, mask);
                    let (mean, max) = uv_stats(&vp_uv, &reference);
                    println!("    vs {name}：平均差 {mean:.2} 最大 {max}");
                }
                for (index, point) in points.iter().enumerate() {
                    let (mean, max) = uv_stats(&vp_uv, point);
                    println!("    点采样(角{index})：平均差 {mean:.2} 最大 {max}");
                }
                let refs: [&[u8]; 5] = [
                    cpu_uv.as_slice(),
                    points[0].as_slice(),
                    points[1].as_slice(),
                    points[2].as_slice(),
                    points[3].as_slice(),
                ];
                let counts = classify_blocks(&vp_uv, &refs);
                println!(
                    "    逐块最像：盒式 {} / 左上 {} / 右上 {} / 左下 {} / 右下 {}（共 {} 块）",
                    counts[0],
                    counts[1],
                    counts[2],
                    counts[3],
                    counts[4],
                    vp_uv.len() / 2
                );
                // 相位：把盒式参考整体平移 ±1 个色度样本再比一次。
                for dx in [-1isize, 1] {
                    let shifted = shift_uv(&cpu_uv, W, H, dx);
                    let (mean, max) = uv_stats(&vp_uv, &shifted);
                    println!("    盒式参考平移 {dx} 个色度样本：平均差 {mean:.2} 最大 {max}");
                }
            }
        }
    }

    // ---- AYUV 4:4:4 预转换 ----

    /// 把 32 位/像素的纹理（BGRA8、AYUV……）拷到 staging 并读回紧凑字节。
    fn read_32bpp(
        device: &D3d11,
        texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Option<Vec<u8>> {
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` 有效；`source_desc` 指向本栈帧。
        unsafe { texture.GetDesc(&raw mut source_desc) };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: source_desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).ok()?,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` 合法；输出指针指向栈上 Option。
        unsafe { device.device.CreateTexture2D(&raw const desc, None, Some(&raw mut staging)) }
            .ok()?;
        let staging = staging?;
        // SAFETY: 两张纹理尺寸/格式一致。
        unsafe { device.context.CopyResource(&staging, texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging 带 CPU_ACCESS_READ；`mapped` 指向本栈帧。
        unsafe { device.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }.ok()?;
        let pitch = usize::try_from(mapped.RowPitch).ok()?;
        let (wu, hu) = (
            usize::try_from(width).ok()?,
            usize::try_from(height).ok()?,
        );
        // SAFETY: AYUV 每像素 4 字节，staging 有 `hu` 行、每行至少 `pitch` 字节。
        let data =
            unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch.checked_mul(hu)?) };
        let mut out = Vec::with_capacity(wu * hu * 4);
        for row in 0..hu {
            out.extend_from_slice(&data[row * pitch..row * pitch + wu * 4]);
        }
        // SAFETY: 与上面的 Map 严格配对。
        unsafe { device.context.Unmap(&staging, 0) };
        Some(out)
    }

    /// GPU 完成栅栏：1×1 纹理的 `CopyResource` + `Map` 会等到此前提交的 GPU 工作
    /// 全部完成（Blt/渲染都是异步提交的，纯 CPU 计时会低估真实耗时）。
    struct GpuFence {
        source: ID3D11Texture2D,
        staging: ID3D11Texture2D,
    }

    impl GpuFence {
        fn new(device: &D3d11) -> Option<Self> {
            let make = |usage: D3D11_USAGE, cpu: u32| -> Option<ID3D11Texture2D> {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: 1,
                    Height: 1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: usage,
                    BindFlags: 0,
                    CPUAccessFlags: cpu,
                    MiscFlags: 0,
                };
                let mut texture: Option<ID3D11Texture2D> = None;
                // SAFETY: `desc` 合法；输出指针指向栈上 Option。
                unsafe { device.device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }
                    .ok()?;
                texture
            };
            Some(Self {
                source: make(D3D11_USAGE_DEFAULT, 0)?,
                staging: make(
                    D3D11_USAGE_STAGING,
                    u32::try_from(D3D11_CPU_ACCESS_READ.0).ok()?,
                )?,
            })
        }

        fn wait(&self, device: &D3d11) -> Option<()> {
            // SAFETY: 两张纹理都是 1×1 同格式；Map 等到 GPU 完成并严格配对 Unmap。
            unsafe {
                device.context.CopyResource(&self.staging, &self.source);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                device
                    .context
                    .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
                    .ok()?;
                device.context.Unmap(&self.staging, 0);
            }
            Some(())
        }
    }

    /// **诊断**：纯色端到端验证 AYUV 4:4:4 预转换（逆编码 → VideoProcessor →
    /// NV12）。
    ///
    /// 纯红在 BT.709 limited 下是 `Y=63, Cb=102, Cr=240`——三个值互不相同，任何
    /// 矩阵/通道错误都会立刻暴露。中间渲染目标的首像素也打印出来，便于区分
    /// 「逆编码算错」和「VideoProcessor 的正向变换与我们的常数不一致」。
    ///
    /// 本机实测（Intel Graphics，驱动 32.0.101.9033）：`DXGI_FORMAT_AYUV` 纹理
    /// 无法创建任何可写视图（RTV/SRV/UAV/VP 输出全部 `E_INVALIDARG`，跨格式
    /// `CopyResource` 静默不拷贝），所以中间必须是 BGRA8；这也是本测试存在的原因
    /// ——它锁住「逆编码 + 驱动正向变换 = 正确的 BT.709 limited」整条链路。
    #[test]
    fn probe_ayuv_channel_order() {
        let Some(device) = intel_or_default_device() else {
            eprintln!("跳过探针：没有可用的 D3D11 硬件适配器");
            return;
        };
        let (w, h) = (16_u32, 16_u32);
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        // BGRA 纯红：(B, G, R, A) = (0, 0, 255, 255)。
        for _ in 0..(w * h) {
            bgra.extend_from_slice(&[0, 0, 255, 255]);
        }
        let Some(texture) = create_bgra_texture(&device.device, &bgra, w, h) else {
            eprintln!("跳过探针：建源纹理失败");
            return;
        };
        let captured = CapturedTexture::from_texture(Arc::clone(&device), texture, 0, w, h);
        let Ok(mut converter) = Nv12Converter::new(
            &device,
            w,
            h,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            w,
            h,
            30,
        ) else {
            eprintln!("跳过探针：VideoProcessor 不可用");
            return;
        };
        let Some(ayuv) = converter.ayuv.as_mut() else {
            panic!("本机应能用 AYUV 预转换（若驱动不支持 d3dcompiler，这条会暴露）");
        };
        ayuv.render(&captured).expect("AYUV 渲染");
        let render_bytes = read_32bpp(&device, &ayuv.texture, w, h).expect("中间纹理读回");
        println!("中间 BGRA 首像素（B,G,R,A）：{:?}", &render_bytes[..4]);

        // 端到端：NV12 应与 BT.709 limited 纯红一致。
        let output = converter.convert(&captured).expect("端到端转换");
        let (y, uv) = read_nv12(&device, &output, w, h).expect("NV12 读回");
        println!("NV12: Y={} U={} V={}", y[0], uv[0], uv[1]);
        assert!(y[0].abs_diff(63) <= 1, "Y 应为 63，实际 {}", y[0]);
        assert!(uv[0].abs_diff(102) <= 1, "U 应为 102，实际 {}", uv[0]);
        assert!(uv[1].abs_diff(240) <= 1, "V 应为 240，实际 {}", uv[1]);
    }

    /// **验收**：新路径的色度必须等于 2×2 盒式平均；强制关掉它之后，驱动原生
    /// BGRA→NV12 仍然**远离**盒式平均（这就是本次修复针对的缺陷）。
    ///
    /// 阈值分两档，原因是数学上绕不开的色域上限（见模块文档）：
    /// - 两种四色块图案的块平均色度对块内每个像素都落在 RGB 立方体内，逆编码是
    ///   精确的 → 沿用 **≤1.5 / ≤3**；
    /// - ClearType 图案里「黑笔画 / 白底」的像素亮度是 16/235，能携带的色度必然为
    ///   零，而相邻彩边让块平均色度对这些像素超出 RGB 色域，`saturate` 会裁掉一部分
    ///   → 只能做到 **≤3.5 / ≤18**（旧的驱动路径是 5.4/19）。要做成严格精确必须把
    ///   YUV 直接写进 YUV 纹理，而 D3D11 运行时不给这个入口（见模块文档）。
    #[test]
    fn ayuv_chroma_matches_box_average() {
        let Some(device) = intel_or_default_device() else {
            eprintln!("跳过验收：没有可用的 D3D11 硬件适配器");
            return;
        };
        // (名称, 图案, 色度平均差上限, 色度最大差上限)
        let patterns = [
            (
                "ClearType 竖笔（白底/红边/黑笔画/蓝边）",
                pattern_clear_type(W, H),
                3.5,
                18,
            ),
            ("2x2 四色块（红/绿/蓝/白）", pattern_blocks(W, H), 1.5, 3),
            ("2x2 四色块（红/绿/蓝/灰）", pattern_quad(W, H), 1.5, 3),
        ];
        let mut results = Vec::new();

        for (name, bgra, mean_limit, max_limit) in &patterns {
            // 参考值：CPU 实现（2×2 盒式平均，BT.709 limited）。
            let rgba = bgra_to_rgba(bgra);
            let mut cpu_nv12 = Vec::new();
            rgba_to_nv12(&rgba, W, H, &mut cpu_nv12).expect("CPU NV12 转换");
            let (cpu_y, cpu_uv) = split_nv12(&cpu_nv12, W, H);

            let Some(texture) = create_bgra_texture(&device.device, bgra, W, H) else {
                eprintln!("{name}: 建源纹理失败，跳过");
                continue;
            };
            let captured =
                CapturedTexture::from_texture(Arc::clone(&device), texture, 0, W, H);
            let Ok(mut converter) = Nv12Converter::new(
                &device,
                W,
                H,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                W,
                H,
                30,
            ) else {
                eprintln!("{name}: VideoProcessor 不可用，跳过");
                continue;
            };
            assert!(
                converter.ayuv.is_some(),
                "{name}: 本机应能用 AYUV 预转换（若驱动缺 d3dcompiler/AYUV 支持，这条会暴露）"
            );

            let output = converter.convert(&captured).expect("AYUV 路径转换");
            let (new_y, new_uv) = read_nv12(&device, &output, W, H).expect("新路径读回");
            let (new_mean, new_max) = uv_stats(&new_uv, &cpu_uv);
            // 亮度必须保持逐像素精确（逆编码只为色度服务）。
            let (luma_mean, luma_max) = uv_stats(&new_y, &cpu_y);

            // 同一转换器上关掉 AYUV，量一遍驱动原生行为作对照。
            converter.disable_ayuv();
            let output = converter.convert(&captured).expect("回退路径转换");
            let (_, old_uv) = read_nv12(&device, &output, W, H).expect("旧路径读回");
            let (old_mean, old_max) = uv_stats(&old_uv, &cpu_uv);

            println!(
                "{name}\n  新路径（AYUV 4:4:4）：Y 平均差 {luma_mean:.2} 最大 {luma_max}；UV 平均差 {new_mean:.2} 最大 {new_max}\n  旧路径（驱动 BGRA→NV12）：UV 平均差 {old_mean:.2} 最大 {old_max}"
            );
            results.push((*name, new_mean, new_max, old_mean, luma_mean, luma_max, *mean_limit, *max_limit));
        }

        for (name, new_mean, new_max, old_mean, luma_mean, luma_max, mean_limit, max_limit) in results {
            assert!(
                new_mean <= mean_limit,
                "{name}: 新路径色度平均差 {new_mean:.2} 超过 {mean_limit}"
            );
            assert!(
                new_max <= max_limit,
                "{name}: 新路径色度最大差 {new_max} 超过 {max_limit}"
            );
            // 亮度在可表示的块里逐像素精确；只有色域被裁的极值像素会偏一点（见模块
            // 文档里的取舍说明），因此上限按 ClearType 实测值（1.12 / 6）留余量。
            assert!(
                luma_mean <= 1.2 && luma_max <= 7,
                "{name}: 亮度被改动了（平均差 {luma_mean:.2} 最大 {luma_max}）"
            );
            assert!(
                old_mean > 2.0,
                "{name}: 旧路径本应远离盒式平均（平均差 {old_mean:.2}），驱动行为可能已改变"
            );
            assert!(
                new_mean < old_mean,
                "{name}: 新路径（{new_mean:.2}）应严格优于旧路径（{old_mean:.2}）"
            );
        }
    }

    /// **测成本**：1080p 下 AYUV 预转换（render）与整条新路径（render + Blt）。
    ///
    /// 只打印实测值，断言仅用作「没有掉进软件模拟」的宽松兜底（GPU 路径应在
    /// 毫秒级；100 ms 是极端慢机的上限，不是性能目标）。
    #[test]
    fn ayuv_path_1080p_cost() {
        let Some(device) = intel_or_default_device() else {
            eprintln!("跳过测量：没有可用的 D3D11 硬件适配器");
            return;
        };
        let (w, h) = (1920_u32, 1080_u32);
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                bgra.extend_from_slice(&[
                    (x % 256) as u8,
                    (y % 256) as u8,
                    ((x + y) % 256) as u8,
                    255,
                ]);
            }
        }
        let Some(texture) = create_bgra_texture(&device.device, &bgra, w, h) else {
            eprintln!("跳过测量：建源纹理失败");
            return;
        };
        let captured = CapturedTexture::from_texture(Arc::clone(&device), texture, 0, w, h);
        let Ok(mut converter) = Nv12Converter::new(
            &device,
            w,
            h,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            w,
            h,
            30,
        ) else {
            eprintln!("跳过测量：VideoProcessor 不可用");
            return;
        };
        assert!(converter.ayuv.is_some(), "本机应能用 AYUV 预转换");
        let Some(fence) = GpuFence::new(&device) else {
            eprintln!("跳过测量：建栅栏纹理失败");
            return;
        };
        // 预热：首帧包含着色器/纹理的惰性初始化。
        converter.convert(&captured).expect("预热转换");
        fence.wait(&device).expect("预热栅栏");

        // Blt 单独计时需要一个输入视图；预转换路径用的就是这个（渲染目标本身）。
        let blt_view = converter
            .ayuv
            .as_ref()
            .expect("AYUV")
            .input_view
            .clone();
        let mut best_render = f64::MAX;
        let mut best_blt = f64::MAX;
        let mut best_total = f64::MAX;
        for _ in 0..20 {
            let started = std::time::Instant::now();
            converter
                .ayuv
                .as_mut()
                .expect("AYUV")
                .render(&captured)
                .expect("render");
            fence.wait(&device).expect("栅栏");
            best_render = best_render.min(started.elapsed().as_secs_f64() * 1000.0);

            let started = std::time::Instant::now();
            converter.blt(&blt_view).expect("blt");
            fence.wait(&device).expect("栅栏");
            best_blt = best_blt.min(started.elapsed().as_secs_f64() * 1000.0);

            let started = std::time::Instant::now();
            converter.convert(&captured).expect("convert");
            fence.wait(&device).expect("栅栏");
            best_total = best_total.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "1080p AYUV 预转换（每次含 GPU 完成等待）：render {best_render:.2} ms；Blt {best_blt:.2} ms；render+Blt {best_total:.2} ms（含两次提交之间的固定开销）"
        );
        assert!(
            best_total < 100.0,
            "1080p 新路径耗时 {best_total:.2} ms 超出宽松上限（疑似退化成软件模拟）"
        );
    }
}

