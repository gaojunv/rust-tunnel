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
//! 的 NV12（VideoProcessor 内部按盒式平均，一帧内容被压进一万多个格点），再拷到
//! staging 纹理 `Map` 读回，与上一帧逐字节比较。4K 桌面实测 **~1.0 ms/帧**
//! （Blt + 拷贝 + Map + 比较），编码耗时不受影响。
//!
//! # 判据
//!
//! 任一格点的 |Δ| 超过 [`PIXEL_DELTA`] 即判「有变化」。**刻意不做聚合阈值**：终端
//! 字符光标的闪烁只影响 1 个格点，但它是真实内容变化，漏掉会让控制器看起来像卡死。
//! 实测静止画面 124/150 帧完全为 0 差异，光标闪烁类变化是 1–20 个格点，两类分得很开。
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

/// 变化检测使用的网格宽度（像素）。偶数（4:2:0 要求），且足够大以分辨一个字符光标。
const GRID_WIDTH: u32 = 128;

/// 变化检测使用的网格高度（像素）。
const GRID_HEIGHT: u32 = 72;

/// 判定「有变化」的单像素差阈值（8 位）。
///
/// 取 3 而不是 1：VideoProcessor 的缩放可能带来 ±1–2 的确定性噪声；3 以上只可能是内容
/// 差异。实测静止画面在这种判据下是**精确的 0 差异**。
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

