//! Windows GUI 路径的硬件采集与 H.264 硬编解码适配。
//!
//! # 分工
//!
//! - **采集**：优先用 `rust-tunnel-win-video` 的 WGC/DXGI（自顶向下 BGRA，
//!   4K 实测约 20 ms/帧），失败时**逐帧回退**到既有的 xcap（GDI）。
//! - **编码**：NVENC H.264 4:2:0。NVENC 对输入只做左上角裁剪、不做缩放，
//!   所以源尺寸与编码尺寸不同（例如 4K 采集、1920 宽编码）时，适配器先用
//!   [`scale_rgba_box`] 在 CPU 上下采样。
//! - **解码**：Media Foundation 的 `MSH264DecoderMFT`（控制器侧）。
//!
//! 平台能力探测在 [`factories`]：只有 NVENC DLL、NVIDIA 适配器与 H.264 MFT
//! 都真的可用时才返回对应的工厂；否则该侧保持 `None`，数据面协商回退 VP9。
//!
//! **实例模式（`--instance`）不走这里**：那条路径由 `win-platform` 用 BitBlt
//! 采集安全桌面，本模块只服务用户会话里的 GUI 进程。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rust_tunnel_remote::codec::{
    VideoDecoderApi, VideoDecoderFactory, VideoEncoderApi, VideoEncoderFactory,
};
use rust_tunnel_remote::{
    encode_dimensions, CodecId, DecodedNv12, DecodedVideoFrame, DisplayInfo, EncodedVideo, GpuFrame,
    RawFrame, RemoteError, RemoteResult, ScreenSource, MAX_ENCODE_WIDTH,
};
use rust_tunnel_win_video::capture::{self, BgraReadback, CaptureSource};
use rust_tunnel_win_video::d3d11;
use rust_tunnel_win_video::decode::{H264Decoder, H264HardwarePreference, H264OutputMode};
use rust_tunnel_win_video::mft_encoder::MftH264Encoder;
use rust_tunnel_win_video::nvenc::{EncoderConfig, InputFormat, NvencApi, NvencEncoder};
use rust_tunnel_win_video::pixels::{bgra_to_rgba, scale_bgra_to_rgba, scale_rgba_box};
use tracing::{debug, info, warn};

use super::XcapScreen;

/// 已缓存采集源的重新校验间隔。
///
/// 缓存有效期内不再调 `xcap::Monitor::all()` 与 `capture::enumerate_monitors()`：
/// 两者每帧各枚举一次显示器，在老 CPU 上是可测开销。2 秒的迟滞对插拔显示器/
/// 改分辨率来说足够短，又足以让绝大多数帧走零枚举的快路径。
const SOURCE_RECHECK_INTERVAL: Duration = Duration::from_secs(2);

/// 一个已打开的硬件采集源及其对应的显示器几何。
struct CachedSource {
    /// WGC 或 DXGI 采集源。
    source: Box<dyn CaptureSource>,
    /// 打开时的虚拟桌面几何；几何变化时丢弃重开。
    geometry: Geometry,
    /// 最近一次完成枚举/几何校验的时刻（缓存有效期从这里算）。
    checked_at: Instant,
}

/// 显示器的虚拟桌面几何（物理像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    /// 左边界。
    left: i32,
    /// 上边界。
    top: i32,
    /// 宽度。
    width: u32,
    /// 高度。
    height: u32,
}

/// 进程级 GPU 采集/编码上下文。
///
/// **为什么是进程级**：采集源（`WinVideoScreen`）与编码器工厂（`MftEncoderFactory`）
/// 由不同的构造路径创建，签名不适合再传一个设备参数。采集成功后把设备记在这里，
/// MFT 工厂创建编码器时复用同一个 `Arc<D3d11>`——`encode_texture` 的
/// `Arc::ptr_eq` 校验要求采集纹理与编码器同设备。
///
/// `disabled` 是**单向开关**：任何一次 GPU 采集/编码失败都会永久禁用本进程的 GPU
/// 路径（回退 CPU），避免每帧都重试一次注定失败的操作。`RUST_TUNNEL_DISABLE_GPU_CAPTURE`
/// 在初始化时就把它置为 `true`。
struct GpuContext {
    /// 最近一次成功 GPU 采集所用的设备。
    device: Mutex<Option<Arc<d3d11::D3d11>>>,
    /// 是否已禁用 GPU 路径（进程级，单向）。
    disabled: AtomicBool,
    /// 是否已经打印过「GPU 路径已启用」日志（只打一次）。
    logged: AtomicBool,
}

impl GpuContext {
    /// 记录（或更新）采集设备。
    fn set_device(&self, device: Arc<d3d11::D3d11>) {
        *self.device.lock().unwrap_or_else(PoisonError::into_inner) = Some(device);
    }

    /// 取当前采集设备（未采过帧时为 `None`）。
    fn device(&self) -> Option<Arc<d3d11::D3d11>> {
        self.device
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 禁用 GPU 路径（幂等；只有第一次调用会 `warn!`）。
    fn disable(&self, reason: &str) {
        if !self.disabled.swap(true, Ordering::SeqCst) {
            warn!("GPU 采集/编码已禁用，回退 CPU 路径：{reason}");
        }
    }

    /// GPU 路径是否已禁用。
    fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::SeqCst)
    }

    /// 首次成功 GPU 采集时打印一条启用日志。
    fn log_enabled_once(&self, adapter: &str) {
        if !self.logged.swap(true, Ordering::SeqCst) {
            // GUI 日志层会丢 tracing 字段，故全部内联进消息文本。
            info!("GPU 零拷贝采集/编码已启用 adapter={adapter}");
        }
    }
}

/// 进程级 GPU 上下文（见 [`GpuContext`]）。
static GPU_CONTEXT: OnceLock<Arc<GpuContext>> = OnceLock::new();

/// 取（或首次构造）进程级 GPU 上下文。
fn gpu_context() -> &'static Arc<GpuContext> {
    GPU_CONTEXT.get_or_init(|| {
        // 现场诊断开关：置该环境变量即完全禁用 GPU 采集/编码（回退 CPU）。
        let env_disabled = std::env::var_os("RUST_TUNNEL_DISABLE_GPU_CAPTURE").is_some();
        if env_disabled {
            info!("RUST_TUNNEL_DISABLE_GPU_CAPTURE 已设置，GPU 采集/编码保持禁用");
        }
        Arc::new(GpuContext {
            device: Mutex::new(None),
            disabled: AtomicBool::new(env_disabled),
            logged: AtomicBool::new(false),
        })
    })
}

/// 引擎侧 [`RawFrame::gpu`] 的 Windows 载体：一张采集源纹理池里的 BGRA 纹理。
///
/// `frame.width/height`（逻辑编码尺寸）可能小于纹理的物理尺寸（4K 采集先封顶到
/// 1920），故这里同时保留 `source_width/source_height` 供编码器缩放。
struct WinGpuFrame {
    /// 采集纹理（持有池槽位租约，直到整帧被丢弃）。
    texture: capture::CapturedTexture,
    /// 纹理的物理宽度。
    source_width: u32,
    /// 纹理的物理高度。
    source_height: u32,
}

impl GpuFrame for WinGpuFrame {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// 硬件优先、xcap 兜底的屏幕采集源。
///
/// `displays()` 刻意继续用 xcap：`DisplayInfo.id` 必须与 [`super::EnigoInput`]
/// 的坐标查询（同样来自 xcap）保持一致，否则多显示器下输入会整体错位。
pub struct WinVideoScreen {
    /// 每个显示器 id 对应的已打开采集源。
    sources: Mutex<HashMap<u32, CachedSource>>,
}

impl WinVideoScreen {
    /// 构造采集源（不预打开任何设备）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            sources: Mutex::new(HashMap::new()),
        }
    }

    /// 在缓存/重建好的采集源上执行一次操作。
    ///
    /// 采集源缓存命中且校验未过期时**不枚举任何显示器**，直接调用；否则重新
    /// 枚举、比对几何，必要时重建采集源。操作失败会把缓存项移除，下一次调用
    /// 重新尝试打开（`open_best` 内部按 WGC → DXGI 顺序选择）。
    fn with_source<R>(
        &self,
        display_id: u32,
        action: impl FnOnce(&mut dyn CaptureSource) -> RemoteResult<R>,
    ) -> RemoteResult<R> {
        let mut sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
        let cache_fresh = sources
            .get(&display_id)
            .is_some_and(|cached| cached.checked_at.elapsed() < SOURCE_RECHECK_INTERVAL);
        if !cache_fresh {
            refresh_cached_source(&mut sources, display_id)?;
        }
        let result = match sources.get_mut(&display_id) {
            Some(cached) => action(cached.source.as_mut()),
            None => Err(RemoteError::Platform("硬件采集源缓存丢失".to_owned())),
        };
        if result.is_err() {
            sources.remove(&display_id);
        }
        result
    }

    /// 尝试用硬件后端采集一帧 **CPU RGBA**；失败时由调用方回退 xcap。
    fn capture_hardware(&self, display_id: u32) -> RemoteResult<RawFrame> {
        let captured = self.with_source(display_id, |source| {
            source
                .capture()
                .map_err(|err| RemoteError::Platform(format!("硬件采集失败: {err}")))
        })?;
        // **在采集层就封顶到 1920 宽**：4K 源若原样交给引擎，会先生成 33 MB
        // 的 RGBA、再做一次 memcmp 去重与下采样；这里一步 BGRA→目标尺寸 RGBA
        // 把中间缓冲直接压到 1920×1080。`rates.scale` 仍由引擎在这之后生效
        // （例如「流畅」档在半分辨率上再乘 0.5）。
        let (dst_width, dst_height) =
            encode_dimensions(captured.width, captured.height, 1.0, MAX_ENCODE_WIDTH);
        let rgba = if (captured.width, captured.height) == (dst_width, dst_height) {
            bgra_to_rgba(&captured.bgra)
        } else {
            scale_bgra_to_rgba(
                &captured.bgra,
                captured.width,
                captured.height,
                dst_width,
                dst_height,
            )
            .map_err(|err| RemoteError::Platform(format!("采集下采样失败: {err}")))?
        };
        Ok(RawFrame {
            display: display_id,
            width: dst_width,
            height: dst_height,
            rgba,
            gpu: None,
        })
    }

    /// 用硬件后端采集一帧 **GPU 纹理**（不落 CPU）。
    ///
    /// 逻辑尺寸与 CPU 路径一致：先按 [`MAX_ENCODE_WIDTH`] 封顶，`rates.scale` 稍后
    /// 由引擎施加。任何一次失败都会**永久禁用**本进程的 GPU 路径并回退到
    /// [`Self::capture`]（CPU），而不是每帧重试。
    fn capture_gpu_frame(&self, display_id: u32) -> RemoteResult<RawFrame> {
        let captured = match self.with_source(display_id, |source| {
            source
                .capture_texture()
                .map_err(|err| RemoteError::Platform(format!("GPU 采集失败: {err}")))
        }) {
            Ok(captured) => captured,
            Err(err) => {
                gpu_context().disable(&err.to_string());
                // 回退 CPU 路径：本次会话继续有画面，只是不再尝试 GPU。
                return self.capture(display_id);
            }
        };
        let (source_width, source_height) = (captured.width, captured.height);
        gpu_context().set_device(Arc::clone(&captured.device));
        gpu_context().log_enabled_once(&captured.device.adapter.name);
        let (width, height) =
            encode_dimensions(source_width, source_height, 1.0, MAX_ENCODE_WIDTH);
        Ok(RawFrame {
            display: display_id,
            width,
            height,
            rgba: Vec::new(),
            gpu: Some(Arc::new(WinGpuFrame {
                texture: captured,
                source_width,
                source_height,
            })),
        })
    }
}

/// 重新枚举显示器与硬件采集目标；几何未变时只刷新 `checked_at`。
///
/// 只有几何真的变化（或缓存缺失）时才枚举硬件目标并重开采集源——枚举本身
/// 是这次优化的目标，不该在几何未变时白做。
fn refresh_cached_source(
    sources: &mut HashMap<u32, CachedSource>,
    display_id: u32,
) -> RemoteResult<()> {
    let monitors = xcap::Monitor::all()
        .map_err(|err| RemoteError::Platform(format!("枚举显示器失败: {err}")))?;
    let (index, monitor) = monitors
        .iter()
        .enumerate()
        .find(|(_, monitor)| monitor.id().ok() == Some(display_id))
        .or_else(|| monitors.first().map(|monitor| (0, monitor)))
        .ok_or_else(|| RemoteError::Platform("无可用显示器".to_owned()))?;
    let geometry = Geometry {
        left: monitor
            .x()
            .map_err(|err| RemoteError::Platform(format!("读取显示器 X 失败: {err}")))?,
        top: monitor
            .y()
            .map_err(|err| RemoteError::Platform(format!("读取显示器 Y 失败: {err}")))?,
        width: monitor
            .width()
            .map_err(|err| RemoteError::Platform(format!("读取显示器宽度失败: {err}")))?,
        height: monitor
            .height()
            .map_err(|err| RemoteError::Platform(format!("读取显示器高度失败: {err}")))?,
    };

    // 缓存几何未变：采集源继续可用，只把有效期往后推。
    if let Some(cached) = sources.get_mut(&display_id) {
        if cached.geometry == geometry {
            cached.checked_at = Instant::now();
            return Ok(());
        }
    }

    let hardware = capture::enumerate_monitors()
        .map_err(|err| RemoteError::Platform(format!("枚举硬件显示器失败: {err}")))?;
    // 两个枚举都来自 EnumDisplayMonitors，顺序一致；几何匹配优先，
    // 顺序索引兜底（DPI 感知模式不同可能让坐标取整略有差异）。
    let target = hardware
        .iter()
        .find(|candidate| {
            candidate.left == geometry.left
                && candidate.top == geometry.top
                && candidate.width == geometry.width
                && candidate.height == geometry.height
        })
        .or_else(|| hardware.get(index))
        .ok_or_else(|| {
            RemoteError::Platform(format!("未找到与显示器 {display_id} 匹配的硬件采集目标"))
        })?;

    sources.remove(&display_id);
    let source = capture::open_best(target)
        .map_err(|err| RemoteError::Platform(format!("打开硬件采集源失败: {err}")))?;
    debug!(display_id, backend = %source.backend(), "硬件采集源已打开");
    sources.insert(
        display_id,
        CachedSource {
            source,
            geometry,
            checked_at: Instant::now(),
        },
    );
    Ok(())
}

impl Default for WinVideoScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenSource for WinVideoScreen {
    fn displays(&self) -> Vec<DisplayInfo> {
        // 与输入注入共用 xcap 的编号体系，见类型文档。
        XcapScreen.displays()
    }

    fn capture(&self, display_id: u32) -> RemoteResult<RawFrame> {
        match self.capture_hardware(display_id) {
            Ok(frame) => Ok(frame),
            Err(err) => {
                warn!(display_id, error = %err, "硬件采集不可用，回退 xcap");
                XcapScreen.capture(display_id)
            }
        }
    }

    fn supports_gpu_capture(&self) -> bool {
        // 环境变量在 `gpu_context()` 初始化时已折算进 `disabled`。
        !gpu_context().is_disabled()
    }

    fn capture_gpu(&self, display_id: u32) -> RemoteResult<RawFrame> {
        self.capture_gpu_frame(display_id)
    }
}

// 编译期断言：引擎要求 `Arc<dyn ScreenSource>`，它隐含 `Send + Sync + 'static`。
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<WinVideoScreen>();
};

/// 构造硬件优先的 GUI 采集源。
#[must_use]
pub fn screen() -> WinVideoScreen {
    WinVideoScreen::new()
}

/// 把从 `started` 到现在的耗时取整为毫秒（饱和到 `u32::MAX`）。
fn elapsed_ms(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}

/// 进程级 NVENC API 只加载一次；`Box::leak` 让它活到进程结束，从而能给出
/// `&'static NvencApi`（编码器实例只借用函数表，不借用每次新建的设备）。
static NVENC_API: OnceLock<Option<SharedNvencApi>> = OnceLock::new();

/// 取（或首次加载）NVENC 函数表；没有 NVIDIA 驱动时返回 `None`。
fn nvenc_api() -> Option<SharedNvencApi> {
    *NVENC_API.get_or_init(|| match NvencApi::load() {
        Ok(api) => {
            info!(driver_api = ?api.driver_api_version(), "NVENC API 已加载");
            Some(SharedNvencApi(Box::leak(Box::new(api))))
        }
        Err(err) => {
            info!(error = %err, "NVENC 不可用，尝试 Intel/AMD 硬件 MFT");
            None
        }
    })
}

/// 进程级 NVENC 函数表的共享包装。
///
/// `NvencApi` 内部的函数表结构含驱动使用的裸指针，因此没有自动实现
/// `Sync`；但它创建后只读，具体会话状态都在每次 `create` 新建的
/// [`NvencEncoder`] 里，NVENC 本来就支持多线程各自持有会话。
#[derive(Debug, Clone, Copy)]
struct SharedNvencApi(&'static NvencApi);

// SAFETY: `SharedNvencApi` 只暴露 `&'static NvencApi` 的只读访问；函数表
// 在进程生命周期内不变，且每次编码会话都是独立的，不存在跨线程共享的可变
// 状态。NVENC 驱动接口本身允许多个线程同时持有各自的编码会话。
unsafe impl Send for SharedNvencApi {}
// SAFETY: 同上——只读函数表可以被多个线程同时读取。
unsafe impl Sync for SharedNvencApi {}

/// NVENC H.264 4:2:0 编码器工厂。
#[derive(Debug)]
pub struct NvencEncoderFactory {
    /// 进程级函数表。
    api: SharedNvencApi,
}

impl VideoEncoderFactory for NvencEncoderFactory {
    fn codec(&self) -> CodecId {
        CodecId::H264
    }

    fn create(
        &self,
        width: u32,
        height: u32,
        target_bitrate_kbps: u32,
        max_fps: u8,
    ) -> RemoteResult<Box<dyn VideoEncoderApi>> {
        let d3d = d3d11::create_nvidia()
            .map_err(|err| RemoteError::Encode(format!("创建 NVIDIA D3D11 设备失败: {err}")))?;
        let config = EncoderConfig::new(width, height, u32::from(max_fps), target_bitrate_kbps)
            .map_err(|err| RemoteError::Encode(format!("H.264 编码参数非法: {err}")))?
            .with_input_format(InputFormat::Rgba);
        let encoder = NvencEncoder::new(self.api.0, &d3d, config)
            .map_err(|err| RemoteError::Encode(format!("NVENC 编码器创建失败: {err}")))?;
        Ok(Box::new(NvencAdapter {
            encoder,
            readback: BgraReadback::new(),
        }))
    }
}

/// `NvencEncoder` 的 remote 适配器：尺寸不同时先做 CPU 盒式下采样。
///
/// NVENC 的输入纹理必须由它自己的设备创建，不能直接吃采集纹理（与 MFT 不同），
/// 所以收到 GPU 帧时先回读 BGRA 再走原有的 RGBA 上传路径。**这条回读没有 GPU
/// 收益**，只是为了让「被控端有 NVIDIA 独显」时不至于因 GPU 帧而失败。
struct NvencAdapter {
    /// 硬件编码器（只借用 `'static` 的函数表）。
    encoder: NvencEncoder<'static>,
    /// 可复用的 GPU→CPU 回读器（staging 纹理按尺寸缓存）。
    readback: BgraReadback,
}

impl NvencAdapter {
    /// 把采集纹理回读成编码器尺寸的 RGBA。
    ///
    /// 4K 采集封顶到 1920 时，`source_width/height` 仍是纹理物理尺寸，故这里用
    /// [`scale_bgra_to_rgba`] 一步完成下采样 + 通道交换（不是先转 RGBA 再缩）。
    fn readback_to_rgba(&mut self, gpu: &WinGpuFrame) -> RemoteResult<Vec<u8>> {
        let (src_width, src_height, bgra) =
            self.readback.read(&gpu.texture).map_err(|err| {
                gpu_context().disable(&format!("NVENC GPU 帧回读失败: {err}"));
                RemoteError::Encode(format!("NVENC GPU 帧回读失败: {err}"))
            })?;
        let (dst_width, dst_height) = self.encoder.size();
        let rgba = if (src_width, src_height) == (dst_width, dst_height) {
            bgra_to_rgba(&bgra)
        } else {
            scale_bgra_to_rgba(&bgra, src_width, src_height, dst_width, dst_height).map_err(
                |err| {
                    gpu_context().disable(&format!("NVENC GPU 帧下采样失败: {err}"));
                    RemoteError::Encode(format!("NVENC GPU 帧下采样失败: {err}"))
                },
            )?
        };
        Ok(rgba)
    }
}

impl VideoEncoderApi for NvencAdapter {
    fn dimensions(&self) -> (u32, u32) {
        self.encoder.size()
    }

    fn bitrate_kbps(&self) -> u32 {
        self.encoder.bitrate_bps() / 1000
    }

    fn fps(&self) -> u32 {
        self.encoder.fps()
    }

    fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()> {
        self.encoder
            .set_bitrate(kbps)
            .map_err(|err| RemoteError::Encode(format!("NVENC 在线改码率失败: {err}")))
    }

    fn set_fps(&mut self, fps: u8) -> RemoteResult<()> {
        self.encoder
            .set_fps(u32::from(fps))
            .map_err(|err| RemoteError::Encode(format!("NVENC 在线改帧率失败: {err}")))
    }

    fn encode(
        &mut self,
        rgba: &[u8],
        src_width: u32,
        src_height: u32,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        let expected = (src_width as usize)
            .checked_mul(src_height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| RemoteError::Encode("帧尺寸溢出".to_owned()))?;
        if rgba.len() != expected {
            return Err(RemoteError::Encode(format!(
                "RGBA 长度 {} 与源尺寸 {src_width}x{src_height} 不符（应为 {expected}）",
                rgba.len()
            )));
        }
        let (dst_width, dst_height) = self.encoder.size();
        // NVENC 只按 inputWidth/Height 做左上角裁剪，绝不缩放，所以 4K 源必须
        // 在 CPU 上缩到编码尺寸（4K→1080p 命中 2×2 快速路径）。
        let packet = if (src_width, src_height) == (dst_width, dst_height) {
            self.encoder.encode_rgba(rgba, force_keyframe)
        } else {
            let scaled = scale_rgba_box(rgba, src_width, src_height, dst_width, dst_height)
                .map_err(|err| RemoteError::Encode(format!("H.264 输入下采样失败: {err}")))?;
            self.encoder.encode_rgba(&scaled, force_keyframe)
        }
        .map_err(|err| RemoteError::Encode(format!("NVENC 编码失败: {err}")))?;
        Ok(EncodedVideo {
            data: packet.data,
            keyframe: packet.keyframe,
        })
    }

    fn encode_frame(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        let Some(gpu) = frame.gpu.as_deref() else {
            // CPU 帧：与默认实现等价。
            return self.encode(&frame.rgba, frame.width, frame.height, force_keyframe);
        };
        // NVENC 不能直接吃 WGC/DXGI 的采集纹理（跨设备），只能回读。代码路径统一
        // 走 `WinGpuFrame`，未知载体说明上游接错了，直接报错（不静默编码空帧）。
        let gpu_frame = gpu.as_any().downcast_ref::<WinGpuFrame>().ok_or_else(|| {
            RemoteError::Encode("NVENC 收到无法识别的 GPU 帧".to_owned())
        })?;
        let rgba = self.readback_to_rgba(gpu_frame)?;
        let (dst_width, dst_height) = self.encoder.size();
        self.encode(&rgba, dst_width, dst_height, force_keyframe)
    }
}

/// Media Foundation 硬件 H.264 编码器工厂（Intel QSV / AMD VCE 等）。
///
/// 只在 NVENC 不可用时才探测；实际是否可用由 `MftH264Encoder::new` 的硬件
/// MFT 枚举决定。Intel MFT 不支持运行期改帧率，适配器会保持内部帧率，只让
/// 采集节奏随 QoS 变化，避免重建编码器/插 IDR。
#[derive(Debug)]
pub struct MftEncoderFactory;

impl MftEncoderFactory {
    /// 探测是否有可用的硬件 H.264 编码 MFT（专用线程，避免 COM 套间污染）。
    #[must_use]
    pub fn probe() -> bool {
        let probe = std::thread::Builder::new()
            .name("mft-encoder-probe".to_owned())
            .spawn(|| match MftH264Encoder::new(1280, 720, 30, 2000) {
                Ok(_) => true,
                Err(err) => {
                    info!(error = %err, "没有可用的硬件 H.264 编码 MFT");
                    false
                }
            })
            .ok();
        match probe.map(std::thread::JoinHandle::join) {
            Some(Ok(ok)) => ok,
            Some(Err(_)) => {
                info!("MFT 编码器探测线程 panic");
                false
            }
            None => false,
        }
    }
}

impl VideoEncoderFactory for MftEncoderFactory {
    fn codec(&self) -> CodecId {
        CodecId::H264
    }

    fn create(
        &self,
        width: u32,
        height: u32,
        target_bitrate_kbps: u32,
        max_fps: u8,
    ) -> RemoteResult<Box<dyn VideoEncoderApi>> {
        let fps = u32::from(max_fps.max(1));
        // GPU 零拷贝只在 **Intel** 采集设备上启用：VideoProcessor + QSV MFT 的同设备
        // 路径是本机验证过的那一条（见 `MftH264Encoder::encode_texture`）。其它厂商
        // （AMD 等）继续走 CPU RGBA 输入，避免未验证的组合。
        let gpu_device = if gpu_context().is_disabled() {
            None
        } else {
            gpu_context()
                .device()
                .filter(|device| device.adapter.vendor_id == d3d11::VENDOR_INTEL)
        };
        let (encoder, device) = if let Some(device) = gpu_device {
            let adapter = device.adapter.name.clone();
            match MftH264Encoder::new_with_device(
                Arc::clone(&device),
                width,
                height,
                fps,
                target_bitrate_kbps,
            ) {
                Ok(encoder) => {
                    // GUI 日志层会丢 tracing 字段，全部内联。
                    info!("MFT 编码器复用采集 D3D11 设备（GPU 零拷贝） adapter={adapter}");
                    (encoder, Some(device))
                }
                Err(err) => {
                    info!("MFT 复用采集设备失败，回退 CPU RGBA 输入：{err}");
                    let encoder =
                        MftH264Encoder::new(width, height, fps, target_bitrate_kbps).map_err(
                            |err| {
                                RemoteError::Encode(format!("硬件 MFT 编码器创建失败: {err}"))
                            },
                        )?;
                    (encoder, None)
                }
            }
        } else {
            let encoder = MftH264Encoder::new(width, height, fps, target_bitrate_kbps).map_err(
                |err| RemoteError::Encode(format!("硬件 MFT 编码器创建失败: {err}")),
            )?;
            (encoder, None)
        };
        Ok(Box::new(MftAdapter {
            encoder,
            fps,
            frames: 0,
            gpu: device.is_some(),
            device,
        }))
    }
}

/// `MftH264Encoder` 的 remote 适配器：尺寸不同时先做 CPU 盒式下采样。
struct MftAdapter {
    /// 硬件编码器。
    encoder: MftH264Encoder,
    /// 逻辑帧率（Intel MFT 不支持在线改输出帧率，实际节奏由采集控制）。
    fps: u32,
    /// 已编码帧数，用于限制细分耗时日志的频率（前 3 帧 + 每 120 帧）。
    frames: u64,
    /// 编码器是否复用了采集 D3D11 设备（`encode_texture` 可用）。
    gpu: bool,
    /// GPU 路径下编码器绑定的采集设备；用于检测采集源重建导致的设备漂移。
    device: Option<Arc<d3d11::D3d11>>,
}

impl VideoEncoderApi for MftAdapter {
    fn dimensions(&self) -> (u32, u32) {
        self.encoder.dimensions()
    }

    fn bitrate_kbps(&self) -> u32 {
        self.encoder.bitrate_kbps()
    }

    fn fps(&self) -> u32 {
        self.fps
    }

    fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()> {
        self.encoder
            .set_bitrate(kbps)
            .map_err(|err| RemoteError::Encode(format!("MFT 在线改码率失败: {err}")))
    }

    fn set_fps(&mut self, fps: u8) -> RemoteResult<()> {
        let fps = u32::from(fps.max(1));
        self.fps = fps;
        if let Err(err) = self.encoder.set_fps(fps) {
            debug!(
                error = %err,
                fps,
                "MFT 不支持在线改帧率，保持编码器内部帧率（采集节奏仍按 QoS）"
            );
        }
        Ok(())
    }

    fn encode(
        &mut self,
        rgba: &[u8],
        src_width: u32,
        src_height: u32,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        let expected = (src_width as usize)
            .checked_mul(src_height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| RemoteError::Encode("帧尺寸溢出".to_owned()))?;
        if rgba.len() != expected {
            return Err(RemoteError::Encode(format!(
                "RGBA 长度 {} 与源尺寸 {src_width}x{src_height} 不符（应为 {expected}）",
                rgba.len()
            )));
        }
        let (dst_width, dst_height) = self.encoder.dimensions();
        let total_started = Instant::now();
        let mut scale_ms = 0u32;
        let packet = if (src_width, src_height) == (dst_width, dst_height) {
            self.encoder.encode_rgba(rgba, force_keyframe)
        } else {
            let scale_started = Instant::now();
            let scaled = scale_rgba_box(rgba, src_width, src_height, dst_width, dst_height)
                .map_err(|err| RemoteError::Encode(format!("H.264 输入下采样失败: {err}")))?;
            scale_ms = elapsed_ms(scale_started);
            self.encoder.encode_rgba(&scaled, force_keyframe)
        }
        .map_err(|err| RemoteError::Encode(format!("MFT 编码失败: {err}")))?;
        let total_ms = elapsed_ms(total_started);
        self.frames = self.frames.saturating_add(1);
        // 只在前 3 帧与每 120 帧打一条：GUI 日志层会丢弃 tracing 字段，故全部
        // 内联进消息文本。字段 = 适配器总耗时（缩放 + 转换 + MFT）。
        if self.frames <= 3 || self.frames.is_multiple_of(120) {
            info!(
                "MFT H.264 编码细分 总计={total_ms}ms 转换={}ms MFT={}ms 尺寸={dst_width}x{dst_height} 缩放={scale_ms}ms",
                self.encoder.last_convert_ms(),
                self.encoder.last_mft_ms(),
            );
        }
        Ok(EncodedVideo {
            data: packet.data,
            keyframe: packet.keyframe,
        })
    }

    fn encode_frame(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        let Some(gpu) = frame.gpu.as_deref() else {
            // CPU 帧：原路径（RGBA + 盒式下采样）。
            return self.encode(&frame.rgba, frame.width, frame.height, force_keyframe);
        };
        if !self.gpu {
            // 工厂没能复用采集设备（例如采集源换了适配器）：不能用空 `rgba` 编码，
            // 禁用 GPU 让下一帧回退 CPU 采集。
            gpu_context().disable("MFT 编码器未复用采集 D3D11 设备");
            return Err(RemoteError::Encode(
                "MFT 编码器未启用 GPU 纹理输入".to_owned(),
            ));
        }
        let Some(gpu_frame) = gpu.as_any().downcast_ref::<WinGpuFrame>() else {
            gpu_context().disable("收到无法识别的 GPU 帧类型");
            return Err(RemoteError::Encode("无法识别的 GPU 帧".to_owned()));
        };
        // 采集源重建（缓存失效/显示器变更）会换一个新 `Arc<D3d11>`；此时**不永久
        // 禁用 GPU**——返回错误让引擎重建编码器，工厂会从 GPU 上下文取新设备。
        if let Some(device) = &self.device {
            if !Arc::ptr_eq(device, &gpu_frame.texture.device) {
                return Err(RemoteError::Encode(
                    "采集设备已变化，重建编码器".to_owned(),
                ));
            }
        }
        let started = Instant::now();
        let packet = self
            .encoder
            .encode_texture(&gpu_frame.texture, force_keyframe)
            .map_err(|err| {
                // 任何真实的 GPU 编码失败都视为该路径不可用：本次调用返回错误，下一帧
                // `capture_gpu` 看到 disabled 后回退 CPU（会丢一帧，可接受）。
                gpu_context().disable(&format!("MFT GPU 编码失败: {err}"));
                RemoteError::Encode(format!("MFT GPU 编码失败: {err}"))
            })?;
        let total_ms = elapsed_ms(started);
        self.frames = self.frames.saturating_add(1);
        if self.frames <= 3 || self.frames.is_multiple_of(120) {
            let (width, height) = self.encoder.dimensions();
            // GUI 日志层会丢弃 tracing 字段，全部内联。
            info!(
                "MFT GPU H.264 编码 总计={total_ms}ms 转换={}ms MFT={}ms 尺寸={width}x{height} 源={}x{}",
                self.encoder.last_convert_ms(),
                self.encoder.last_mft_ms(),
                gpu_frame.source_width,
                gpu_frame.source_height,
            );
        }
        Ok(EncodedVideo {
            data: packet.data,
            keyframe: packet.keyframe,
        })
    }
}

/// Media Foundation H.264 解码器工厂（控制器侧）。
#[derive(Debug)]
pub struct H264DecoderFactory;

impl H264DecoderFactory {
    /// 探测 `MSH264DecoderMFT` 是否可用；不可用时返回 `None`（不声明 H.264 能力）。
    ///
    /// **探测放在专用线程**：`H264Decoder::new` 会 `CoInitializeEx(MTA)` 并
    /// `MFStartup`。虽然本函数跑在远程引擎的后台线程、不是 winit 主线程，但把
    /// COM 套间初始化限制在一条一次性线程上更干净，也不会与后续任何 STA 依赖
    /// 抢主线程的套间。
    #[must_use]
    pub fn new() -> Option<Self> {
        let probe = std::thread::Builder::new()
            .name("mft-probe".to_owned())
            .spawn(|| match H264Decoder::new(64, 64) {
                Ok(_) => true,
                Err(err) => {
                    info!(error = %err, "Media Foundation H.264 解码器不可用，控制器不声明 H.264 能力");
                    false
                }
            })
            .ok()?;
        match probe.join() {
            Ok(true) => Some(Self),
            Ok(false) => None,
            Err(_) => {
                info!("MFT 探测线程 panic，控制器不声明 H.264 能力");
                None
            }
        }
    }
}

impl VideoDecoderFactory for H264DecoderFactory {
    fn codec(&self) -> CodecId {
        CodecId::H264
    }

    fn create(&self) -> RemoteResult<Box<dyn VideoDecoderApi>> {
        Ok(Box::new(H264DecoderAdapter {
            decoder: None,
            configured: None,
            frames: 0,
        }))
    }
}

/// 惰性适配器：`configure(width, height)` 之前不创建 MFT。
struct H264DecoderAdapter {
    /// 当前解码器；`None` 表示尚未 configure 或上次创建失败。
    decoder: Option<H264Decoder>,
    /// 当前解码器适配的编码尺寸。
    configured: Option<(u32, u32)>,
    /// 已成功解出的帧数，用于限制细分耗时日志的频率（前 3 帧 + 每 120 帧）。
    frames: u64,
}

impl VideoDecoderApi for H264DecoderAdapter {
    fn configure(&mut self, width: u32, height: u32) -> RemoteResult<()> {
        if self.decoder.is_some() && self.configured == Some((width, height)) {
            return Ok(());
        }
        // 现场 A/B 开关：若解码细分日志显示 DXVA 回读（staging Map）比颜色转换
        // 还慢，置 `RUST_TUNNEL_DECODE_SOFTWARE=1` 可强制系统内存软件解码对照。
        // 默认行为不变（Auto → DXVA 优先）。
        let software = std::env::var_os("RUST_TUNNEL_DECODE_SOFTWARE")
            .is_some_and(|value| !value.is_empty());
        // 输出 **紧包 NV12**：CPU 侧的 NV12→RGBA 转换移到 GUI 线程（阶段 1b 会换成
        // GPU 着色器），解码线程只做 MFT 解码 + 可见区域拷贝。
        let created = if software {
            H264Decoder::new_with_preference_and_output(
                width,
                height,
                H264HardwarePreference::Software,
                H264OutputMode::Nv12,
            )
        } else {
            H264Decoder::new_nv12(width, height)
        };
        match created {
            Ok(decoder) => {
                info!(
                    path = ?decoder.path(),
                    adapter = ?decoder.adapter_name(),
                    software,
                    output = "Nv12",
                    width,
                    height,
                    "H.264 解码器已创建"
                );
                self.decoder = Some(decoder);
                self.configured = Some((width, height));
                Ok(())
            }
            Err(err) => {
                self.decoder = None;
                self.configured = None;
                Err(RemoteError::Encode(format!(
                    "H.264 解码器初始化失败: {err}"
                )))
            }
        }
    }

    fn decode(&mut self, data: &[u8]) -> RemoteResult<Option<DecodedVideoFrame>> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| RemoteError::Encode("H.264 解码器尚未 configure".to_owned()))?;
        let total_started = Instant::now();
        let frame = decoder
            .decode(data)
            .map_err(|err| RemoteError::Encode(format!("H.264 解码失败: {err}")))?;
        let total_ms = elapsed_ms(total_started);
        // 在结束 `decoder` 的可变借用前读出诊断读数与通路名。
        let readback_ms = decoder.last_readback_ms();
        let color_ms = decoder.last_color_ms();
        let path = decoder.path().as_str();
        let Some(frame) = frame else {
            // MFT 还在缓冲输入，本帧没有输出；不计数、不打日志。
            return Ok(None);
        };
        self.frames = self.frames.saturating_add(1);
        // 只在前 3 帧与每 120 帧打一条（GUI 日志层会丢弃 tracing 字段，全部内联）。
        if self.frames <= 3 || self.frames.is_multiple_of(120) {
            info!(
                "H.264 解码细分 总计={total_ms}ms 回读={readback_ms}ms 打包={color_ms}ms 尺寸={}x{} path={path}",
                frame.width, frame.height,
            );
        }
        // 正常路径是 `nv12: Some(..)`、`rgba` 为空；奇数可见区域会被解码器回退成
        // RGBA（`nv12: None`），两者都原样交给引擎。
        let nv12 = frame.nv12.map(|nv12| DecodedNv12 {
            y: nv12.y,
            uv: nv12.uv,
        });
        Ok(Some(DecodedVideoFrame {
            width: frame.width,
            height: frame.height,
            rgba: frame.rgba,
            nv12,
        }))
    }
}

/// 探测并构造 Windows 硬件编解码工厂。
///
/// 返回的每一侧都是独立的：NVENC 不可用不影响 MFT 解码声明，反之亦然。
/// 任一侧为 `None` 时，协商会回退到 VP9。
#[must_use]
pub fn factories() -> super::HardwareFactories {
    // 现场诊断开关：怀疑 NVENC/Media Foundation 探测导致启动问题时置 1，
    // 跳过所有硬件探测，数据面完全回退 VP9（GUI 仍应正常启动）。
    if std::env::var_os("RUST_TUNNEL_DISABLE_HWCODEC").is_some() {
        info!("RUST_TUNNEL_DISABLE_HWCODEC 已设置，跳过 NVENC/MFT 探测");
        return (None, None);
    }
    let nvenc = match nvenc_api() {
        Some(api) => match d3d11::create_nvidia() {
            Ok(_) => {
                info!("NVENC H.264 硬件编码已注入");
                let factory: Arc<dyn VideoEncoderFactory> = Arc::new(NvencEncoderFactory { api });
                Some(factory)
            }
            Err(err) => {
                info!(error = %err, "没有可用的 NVIDIA 适配器，尝试 Intel/AMD 硬件 MFT");
                None
            }
        },
        None => None,
    };
    // NVENC 优先；没有 NVIDIA 时再探测 Intel QSV / AMD VCE 的硬件编码 MFT。
    let encoder = match nvenc {
        Some(factory) => Some(factory),
        None => {
            if MftEncoderFactory::probe() {
                info!("Intel/AMD 硬件 MFT H.264 编码已注入");
                let factory: Arc<dyn VideoEncoderFactory> = Arc::new(MftEncoderFactory);
                Some(factory)
            } else {
                info!("没有可用的硬件 H.264 编码器（NVENC/MFT），回退 VP9");
                None
            }
        }
    };
    let decoder = H264DecoderFactory::new().map(|factory| {
        let factory: Arc<dyn VideoDecoderFactory> = Arc::new(factory);
        factory
    });
    (encoder, decoder)
}

/// 让进程在 eframe/winit 创建窗口**之前**切到「每显示器 DPI 感知 v2」。
///
/// 只有这样才能让 xcap 的 `DEVMODE` 尺寸、win-video 的 `GetMonitorInfoW`
/// 尺寸与 WGC 的物理像素帧三者一致；否则 150% 缩放的 4K 屏会被报成
/// 2560×1440，采集坐标与输入坐标整体错位。
///
/// 失败（例如 manifest 已设置过、或系统不支持）只记 debug——DPI 感知只能
/// 在进程启动早期设置一次，重复设置返回失败是正常情况。
pub fn enable_per_monitor_dpi_awareness() {
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };

    // SAFETY: 传入的是文档定义的进程级 DPI 感知上下文常量；该调用没有
    // 指针参数、也没有额外的前置条件。
    let ok = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if ok == 0 {
        debug!("SetProcessDpiAwarenessContext(PMv2) 失败（可能已设置过），沿用当前感知级别");
    }
}
