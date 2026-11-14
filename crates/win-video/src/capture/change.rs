//! GPU 纹理的**内容变化检测**：远程桌面「静止不编码」的闸门。
//!
//! # 为什么不能只看采集后端「有没有新帧」
//!
//! WGC 按合成器节奏持续产帧——实测 30 fps 轮询下 120 次里 119 次拿到新帧，即使桌面
//! 完全静止（只有一个闪烁的终端）。DXGI 的 `AcquireNextFrame` 超时才是真的没有新
//! 桌面帧。而远程桌面最常见的情形是「有新帧、像素一模一样」：编码器照样出码（Intel
//! QSV 的 CBR 还会为填满目标码率插填充），链路被白白占用。
//!
//! # 做法与代价
//!
//! 用 `ID3D11VideoProcessor` 把采集到的 BGRA 纹理缩到 [`GRID_WIDTH`]×[`GRID_HEIGHT`]
//! 的 NV12，再拷到 staging 纹理 `Map` 读回，与上一帧逐字节比较。4K 桌面实测
//! **0.36 ms/帧**（Blt + 拷贝 + Map + 比较），编码耗时不受影响。
//!
//! **网格密度直接决定检出率，因为 VideoProcessor 在大比例缩小时是点采样**：它默认
//! 不做线性滤波（`D3D11_VIDEO_PROCESSOR_FILTER_*` 里也没有可用的线性项），所以一个
//! 网格点只对应源帧里的少数几个采样位置，而**不是**一小块区域的平均值。本机在 4K
//! 帧上用真实 D3D11 VideoProcessor 实测（同一份代码，只改网格尺寸）：
//!
//! - 128×72（采样密度 1/900）：16×16 的白块只有 5–6/12 次被看到，1×1 只有 2/12；
//! - 320×180：密度提高 4 倍，16×16 → 12/12、8×8 → 9/12，而**单次检测耗时不变**
//!   （0.362 → 0.367 ms）；
//! - 960×540：4×4 → 12/12，但耗时涨到 0.866 ms——再往上就是拿帧时间换密度。
//!
//! 所以取 320×180：显著压低「微小变化漏检」的概率，又不拖慢采集节拍。残余盲区由
//! 引擎侧的**保活补编**兜底（`crates/remote` 的 `UNCHANGED_RESEND_INTERVAL`，静止时
//! 每 250 ms 仍补一帧）。
//!
//! # 判据
//!
//! 任一格点的 |Δ| 超过 [`PIXEL_DELTA`] 即判「有变化」。**刻意不做聚合阈值**：终端
//! 字符光标的闪烁只影响 1 个格点，但它是真实内容变化，漏掉会让控制器看起来像卡死。
//! 静止画面在相同输入下是**精确的 0 差异**（点采样是确定性的），光标闪烁类变化会落在
//! 少数格点上，两类分得很开。
//!
//! # 失败语义
//!
//! 设备不支持 VideoProcessor / 格式不被接受时构造失败，[`TextureChangeTracker`]
//! **永久退回「永远认为有变化」**（每个采集源只会尝试一次，不会每帧重试）。省带宽是
//! 优化，画面正确性是底线。

use super::CapturedTexture;
use crate::d3d11::D3d11;
use crate::WinVideoError;
use std::mem::ManuallyDrop;
use std::time::Instant;
use windows::core::Interface as _;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_VIDEO_ENCODER, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

/// 变化检测使用的网格宽度（像素）。偶数（4:2:0 要求）。
///
/// 320×180 是按实测选的：VideoProcessor 大比例缩小时是**点采样**，网格越稀，
/// 小面积变化漏检的概率越高。128×72 在 4K 屏上只抽 1/900 的像素，16×16 的变化
/// 只有约一半概率被看到；320×180 把采样密度提到 4 倍而**检测耗时不变**（4K 实测
/// 0.36 ms），960×540 才会明显变贵（0.87 ms）。
const GRID_WIDTH: u32 = 320;

/// 变化检测使用的网格高度（像素）。偶数（4:2:0 要求），与 [`GRID_WIDTH`] 保持 16:9。
const GRID_HEIGHT: u32 = 180;

/// 判定「有变化」的单像素差阈值（8 位）。
///
/// 取 3 而不是 1：**静态画面在相同输入下是精确的 0 差异**（点采样是确定性的），
/// 这点抖动余量只留给缩放器/颜色空间转换在驱动间可能引入的 ±1–2；3 以上只可能是
/// 内容差异。
const PIXEL_DELTA: u8 = 3;

/// 输入视图缓存上限（采集纹理池是 4 张，池重建的过渡期再留一倍余量）。
const MAX_CACHED_INPUT_VIEWS: usize = 8;

/// 每个采集源一个的变化检测器（**只尝试构造一次**）。
///
/// 采集源在 `WGC` / `DXGI` 两条路径里各持有一个；构造失败后永久退回「有变化」，
/// 不每帧重试。
pub(crate) struct TextureChangeTracker {
    /// 当前状态。
    state: State,
}

/// [`TextureChangeTracker`] 的内部状态。
enum State {
    /// 还没拿到第一帧（不知道尺寸/格式）。
    Uninitialized,
    /// 检测器就绪。
    Ready(ChangeDetector),
    /// 设备不支持，永久退回「永远认为有变化」。
    Unavailable,
}

impl TextureChangeTracker {
    /// 新建一个尚未初始化的追踪器。
    pub(crate) fn new() -> Self {
        Self {
            state: State::Uninitialized,
        }
    }

    /// 判断这一帧与上一帧是否有可见差异。
    ///
    /// 首帧、检测器尚未初始化、以及任何内部失败都返回 `true`——宁可多编码一帧，
    /// 也不能丢画面。
    pub(crate) fn changed(&mut self, device: &D3d11, frame: &CapturedTexture) -> bool {
        match &mut self.state {
            State::Ready(detector) if detector.matches(frame) => detector.changed(frame),
            State::Unavailable => true,
            // 未初始化或尺寸/格式变了：重建（每个采集源只会走有限次）。
            _ => match ChangeDetector::new(device, frame) {
                Ok(mut detector) => {
                    let changed = detector.changed(frame);
                    self.state = State::Ready(detector);
                    changed
                }
                Err(err) => {
                    tracing::debug!(error = %err, "GPU 变化检测不可用，退回逐帧编码");
                    self.state = State::Unavailable;
                    true
                }
            },
        }
    }
}

/// 一块输入尺寸/格式对应的变化检测器。
struct ChangeDetector {
    /// VideoProcessor 相关的设备接口。
    video_device: ID3D11VideoDevice,
    /// 立即上下文（Blt / CopyResource / Map 都走它）。
    video_context: ID3D11VideoContext,
    /// 枚举器与处理器。
    enumerator: ID3D11VideoProcessorEnumerator,
    /// 实际做缩放/格式转换的处理器。
    processor: ID3D11VideoProcessor,
    /// 缩小后的 NV12 输出纹理。
    output: ID3D11Texture2D,
    /// 输出纹理的 VP 输出视图。
    output_view: ID3D11VideoProcessorOutputView,
    /// CPU 可读的 staging 副本。
    staging: ID3D11Texture2D,
    /// 构造时绑定的输入尺寸与格式。
    input_key: (u32, u32, DXGI_FORMAT),
    /// 按纹理裸指针缓存的输入视图（同时持有纹理引用，指针不会悬空）。
    input_views: Vec<(usize, ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    /// 上一帧的网格像素（Y + UV）。
    previous: Vec<u8>,
}

impl std::fmt::Debug for ChangeDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeDetector")
            .field("input_key", &self.input_key)
            .field("has_previous", &(!self.previous.is_empty()))
            .finish_non_exhaustive()
    }
}

impl ChangeDetector {
    /// 在给定设备上构造检测器；输入尺寸/格式取自 `frame` 的描述。
    ///
    /// # Errors
    /// 设备不支持视频处理、输入/输出格式不被接受、纹理/视图创建失败时返回错误。
    /// 调用方据此退回「永远认为有变化」。
    #[expect(
        clippy::too_many_lines,
        reason = "枚举器/处理器/输出纹理/输出视图/颜色空间是一组必须按顺序建立且共享同一个错误路径的初始化步骤"
    )]
    fn new(device: &D3d11, frame: &CapturedTexture) -> Result<Self, WinVideoError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `frame.texture` 是有效纹理；`desc` 指向本栈帧。
        unsafe { frame.texture.GetDesc(&raw mut desc) };
        let (input_width, input_height, input_format) = (desc.Width, desc.Height, desc.Format);
        if input_width == 0 || input_height == 0 {
            return Err(WinVideoError::Config(
                "变化检测的输入尺寸不能为 0".to_owned(),
            ));
        }

        let video_device: ID3D11VideoDevice = device
            .device
            .cast()
            .map_err(|err| WinVideoError::Config(format!("设备不支持视频处理: {err}")))?;
        let video_context: ID3D11VideoContext = device
            .context
            .cast()
            .map_err(|err| WinVideoError::Config(format!("上下文不支持视频处理: {err}")))?;

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
            OutputWidth: GRID_WIDTH,
            OutputHeight: GRID_HEIGHT,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: `content` 指向本栈帧，`video_device` 有效。
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&raw const content) }
            .map_err(|err| {
                WinVideoError::Config(format!("CreateVideoProcessorEnumerator 失败: {err}"))
            })?;
        // SAFETY: `enumerator` 有效；只查询能力。
        let in_flags = unsafe { enumerator.CheckVideoProcessorFormat(input_format) }.map_err(|err| {
            WinVideoError::Config(format!("CheckVideoProcessorFormat 失败: {err}"))
        })?;
        if in_flags & u32::try_from(D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0).unwrap_or(0) == 0 {
            return Err(WinVideoError::Config(format!(
                "变化检测不支持输入格式 {input_format:?}"
            )));
        }
        // SAFETY: 同上。
        let out_flags = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }.map_err(
            |err| WinVideoError::Config(format!("CheckVideoProcessorFormat 失败: {err}")),
        )?;
        if out_flags & u32::try_from(D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0).unwrap_or(0) == 0 {
            return Err(WinVideoError::Config(
                "变化检测不支持 NV12 输出".to_owned(),
            ));
        }
        // SAFETY: `enumerator` 由本设备创建；rate conversion index 0 是标准模式。
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|err| WinVideoError::Config(format!("CreateVideoProcessor 失败: {err}")))?;
        // 颜色空间不是重点（只做比较），但两条路径都显式设成同一套，保证确定性。
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

        let output = create_texture(
            &device.device,
            D3D11_USAGE_DEFAULT,
            u32::try_from(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0).unwrap_or(0),
            0,
        )?;
        let staging = create_texture(
            &device.device,
            D3D11_USAGE_STAGING,
            0,
            u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0),
        )?;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view: Option<ID3D11VideoProcessorOutputView> = None;
        // SAFETY: 三个参数都是本设备创建的有效对象；输出指针指向栈上 Option。
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &output,
                &enumerator,
                &raw const desc,
                Some(&raw mut view),
            )
        }
        .map_err(|err| WinVideoError::Config(format!("CreateVideoProcessorOutputView 失败: {err}")))?;
        let output_view =
            view.ok_or_else(|| WinVideoError::Config("输出视图返回空接口".to_owned()))?;

        tracing::debug!(
            input = %format!("{input_width}x{input_height} {input_format:?}"),
            grid = %format!("{GRID_WIDTH}x{GRID_HEIGHT}"),
            "GPU 变化检测器已就绪"
        );
        Ok(Self {
            video_device,
            video_context,
            enumerator,
            processor,
            output,
            output_view,
            staging,
            input_key: (input_width, input_height, input_format),
            input_views: Vec::new(),
            previous: Vec::new(),
        })
    }

    /// 当前检测器绑定的输入尺寸与格式是否与 `frame` 一致。
    fn matches(&self, frame: &CapturedTexture) -> bool {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `frame.texture` 是有效纹理；`desc` 指向本栈帧。
        unsafe { frame.texture.GetDesc(&raw mut desc) };
        self.input_key == (desc.Width, desc.Height, desc.Format)
    }

    /// 判断这一帧与上一帧是否有可见差异；首帧恒为 `true`。
    ///
    /// 比较失败（Blt/Map 出错）时也返回 `true`：宁可多编码一帧，也不能丢画面。
    fn changed(&mut self, frame: &CapturedTexture) -> bool {
        let started = Instant::now();
        let Ok(current) = self.read_grid(frame) else {
            return true;
        };
        let changed = self.previous.len() != current.len()
            || self
                .previous
                .iter()
                .zip(current.iter())
                .any(|(before, now)| before.abs_diff(*now) > PIXEL_DELTA);
        self.previous = current;
        tracing::trace!(
            changed,
            micros = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX),
            "变化检测"
        );
        changed
    }

    /// 把采集纹理缩到网格并读回（Y + UV）。
    fn read_grid(&mut self, frame: &CapturedTexture) -> Result<Vec<u8>, WinVideoError> {
        let view = self.input_view(frame)?;
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
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // SAFETY: 与上面的 ManuallyDrop::new 配对，释放我们持有的接口引用。
        unsafe { drop(ManuallyDrop::take(&mut stream.pInputSurface)) };
        result.map_err(|err| WinVideoError::Encode(format!("变化检测 Blt 失败: {err}")))?;

        // SAFETY: 两张纹理同设备、同尺寸/格式；拷贝在立即上下文上同步完成。
        unsafe {
            frame
                .device
                .context
                .CopyResource(&self.staging, &self.output);
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `staging` 是 STAGING + CPU_ACCESS_READ；`mapped` 指向本栈帧。
        unsafe {
            frame
                .device
                .context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
        }
        .map_err(|err| WinVideoError::Encode(format!("变化检测 Map 失败: {err}")))?;
        let pitch = usize::try_from(mapped.RowPitch).unwrap_or(0);
        let rows = usize::try_from(GRID_HEIGHT).unwrap_or(0);
        // NV12：Y 平面 `pitch × H`，UV 平面紧随其后 `pitch × H / 2`。
        let len = pitch.saturating_mul(rows).saturating_mul(3) / 2;
        let grid = if mapped.pData.is_null() || pitch == 0 {
            Vec::new()
        } else {
            // SAFETY: Map 成功后驱动保证至少 `pitch × H × 1.5` 字节可读；长度已按此计算。
            unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) }.to_vec()
        };
        // SAFETY: 与上面的 Map 配对。
        unsafe { frame.device.context.Unmap(&self.staging, 0) };
        Ok(grid)
    }

    /// 取得（或创建并缓存）给定纹理的 VideoProcessor 输入视图。
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
        // SAFETY: `frame.texture` 是有效 2D 纹理（尺寸/格式已由调用方校验）。
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                &frame.texture,
                &self.enumerator,
                &raw const desc,
                Some(&raw mut view),
            )
        }
        .map_err(|err| {
            WinVideoError::Config(format!("CreateVideoProcessorInputView 失败: {err}"))
        })?;
        let view = view.ok_or_else(|| WinVideoError::Config("输入视图返回空接口".to_owned()))?;
        if self.input_views.len() >= MAX_CACHED_INPUT_VIEWS {
            self.input_views.clear();
        }
        self.input_views
            .push((raw, frame.texture.clone(), view.clone()));
        Ok(view)
    }
}

/// 创建检测用的网格纹理（NV12，`GRID_WIDTH`×`GRID_HEIGHT`）。
fn create_texture(
    device: &ID3D11Device,
    usage: D3D11_USAGE,
    bind: u32,
    cpu_access: u32,
) -> Result<ID3D11Texture2D, WinVideoError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: GRID_WIDTH,
        Height: GRID_HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` 合法；输出指针指向栈上 Option。
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }
        .map_err(|err| WinVideoError::Config(format!("创建变化检测纹理失败: {err}")))?;
    texture.ok_or_else(|| WinVideoError::Config("CreateTexture2D 返回空纹理".to_owned()))
}

// 编译期把几条判据钉住：
// - 4:2:0 要求宽高为偶数（网格尺寸是常量，防止日后改成奇数）；
// - 阈值必须严格大于缩放/抖动的 ±1–2 噪声。
const _: () = assert!(GRID_WIDTH.is_multiple_of(2));
const _: () = assert!(GRID_HEIGHT.is_multiple_of(2));
const _: () = assert!(PIXEL_DELTA > 2);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::Arc;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

    /// 测试帧尺寸：4K 是采样密度最稀的一端，也是现场反馈的来源。
    const FRAME_WIDTH: u32 = 3840;
    /// 测试帧高度。
    const FRAME_HEIGHT: u32 = 2160;
    /// 现场「看不到」的最小变化量级（微信/QQ 消息闪动、细小选区）。
    const BLOCK_SIZE: u32 = 16;

    /// 纯色底（BGRA，alpha 恒为 255）。
    fn base_pixels() -> Vec<u8> {
        let len = usize::try_from(FRAME_WIDTH * FRAME_HEIGHT * 4)
            .expect("3840×2160×4 应能放进 usize");
        let mut pixels = vec![0_u8; len];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[0x20, 0x40, 0x80, 0xFF]);
        }
        pixels
    }

    /// 在纯色底上画一个 16×16 白块。
    fn block_pixels(left: u32, top: u32) -> Vec<u8> {
        let mut pixels = base_pixels();
        for y in top..top + BLOCK_SIZE {
            for x in left..left + BLOCK_SIZE {
                let offset = usize::try_from((y * FRAME_WIDTH + x) * 4)
                    .expect("像素偏移应能放进 usize");
                pixels[offset..offset + 4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
            }
        }
        pixels
    }

    /// 用初始数据创建一张 4K BGRA 纹理。
    ///
    /// 必须带 `RENDER_TARGET | SHADER_RESOURCE`：`CreateVideoProcessorInputView`
    /// 不接受没有任何绑定标志的纹理。
    fn create_texture(device: &ID3D11Device, pixels: &[u8]) -> Option<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: FRAME_WIDTH,
            Height: FRAME_HEIGHT,
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
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: FRAME_WIDTH * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc`/`init` 指向本栈帧，`pixels` 在调用期间存活；输出指针有效。
        unsafe {
            device.CreateTexture2D(&raw const desc, Some(&raw const init), Some(&raw mut texture))
        }
        .ok()?;
        texture
    }

    /// 把自建纹理包成采集层的 [`CapturedTexture`]。
    fn captured(device: &Arc<D3d11>, texture: ID3D11Texture2D) -> CapturedTexture {
        CapturedTexture::from_texture(Arc::clone(device), texture, 0, FRAME_WIDTH, FRAME_HEIGHT)
    }

    /// CI 的 Windows runner 可能只有 WARP：设备建不起来就跳过，不 panic。
    fn hardware_device() -> Option<Arc<D3d11>> {
        match crate::d3d11::create(None) {
            Ok(device) => Some(Arc::new(device)),
            Err(err) => {
                eprintln!("跳过变化检测测试：没有可用的 D3D11 硬件适配器（{err}）");
                None
            }
        }
    }

    /// 同一张纹理连续两次：第一次没有基准 → `true`，第二次必须 `false`。
    ///
    /// 后半句是「静止闸门」的定义：若这里为 `true`，静止桌面会退回每拍编码。
    #[test]
    fn unchanged_frame_is_not_reported_as_changed() {
        let Some(device) = hardware_device() else {
            return;
        };
        let Some(texture) = create_texture(&device.device, &base_pixels()) else {
            eprintln!("跳过变化检测测试：创建 4K 测试纹理失败");
            return;
        };
        let frame = captured(&device, texture);
        let Ok(mut detector) = ChangeDetector::new(&device, &frame) else {
            eprintln!("跳过变化检测测试：本端不支持 VideoProcessor 变化检测");
            return;
        };
        assert!(detector.changed(&frame), "首帧没有基准，必须判为有变化");
        assert!(
            !detector.changed(&frame),
            "同一张纹理连续两次必须是「无变化」（否则静止闸门失效）"
        );
    }

    /// 4K 帧上贴着边缘的 16×16 变化必须被检出（四角 + 中心）。
    ///
    /// 这条同时锁住「缩放的 source/dest rect 必须覆盖整帧」——角上的块最容易被
    /// 裁剪/采样边界漏掉。128×72 的旧网格在 4K 上只会随机看到其中一部分。
    #[test]
    fn tiny_blocks_near_frame_edges_are_detected() {
        let Some(device) = hardware_device() else {
            return;
        };
        let Some(base_texture) = create_texture(&device.device, &base_pixels()) else {
            eprintln!("跳过变化检测测试：创建 4K 测试纹理失败");
            return;
        };
        let base = captured(&device, base_texture);

        let positions = [
            (0, 0),
            (FRAME_WIDTH - BLOCK_SIZE, 0),
            (0, FRAME_HEIGHT - BLOCK_SIZE),
            (FRAME_WIDTH - BLOCK_SIZE, FRAME_HEIGHT - BLOCK_SIZE),
            (FRAME_WIDTH / 2, FRAME_HEIGHT / 2),
        ];
        for (left, top) in positions {
            let Some(texture) = create_texture(&device.device, &block_pixels(left, top)) else {
                eprintln!("跳过变化检测测试：创建 4K 测试纹理失败");
                return;
            };
            let changed_frame = captured(&device, texture);
            let Ok(mut detector) = ChangeDetector::new(&device, &base) else {
                eprintln!("跳过变化检测测试：本端不支持 VideoProcessor 变化检测");
                return;
            };
            // 第一帧建立基准（首帧恒为 `true`），第二帧才是真正的比较。
            assert!(detector.changed(&base));
            assert!(
                detector.changed(&changed_frame),
                "16×16 的变化在 ({left}, {top}) 漏检（检查网格密度与缩放矩形是否覆盖整帧）"
            );
        }
    }
}

