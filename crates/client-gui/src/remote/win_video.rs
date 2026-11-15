//! Windows GUI 路径的硬件采集与 H.264 硬编解码适配。
//!
//! # 分工
//!
//! - **采集**：优先用 `rust-tunnel-win-video` 的 WGC/DXGI（自顶向下 BGRA，
//!   4K 实测约 20 ms/帧），失败时**逐帧回退**到既有的 xcap（GDI）。
//! - **编码**：NVENC H.264 4:2:0。采集纹理与编码器**同设备且尺寸相等**时直接
//!   注册给 NVENC（零拷贝，不落 CPU）；否则回读 BGRA，必要时 CPU 盒式下采样后再
//!   上传（NVENC 对输入只做左上角裁剪、不做缩放，例如 4K 采集 → 1920 宽编码）。
//! - **解码**：Media Foundation 的 `MSH264DecoderMFT`（控制器侧）。
//!
//! 平台能力探测在 [`factories`]：NVENC（DLL + NVIDIA 适配器）可用、或 NVENC
//! 不可用但硬件 MFT 可用时才注入 H.264 编码工厂；否则该侧保持 `None`，数据面
//! 协商回退 VP9。**编码后端在 `create()` 时按采集适配器选路**（见
//! [`choose_backend`]）：混合显卡下采集设备常是 Intel 核显，此时优先 QSV MFT
//! 才能复用同一块设备；无条件优先 NVENC 只会退化成跨设备回读。
//!
//! **实例模式（`--instance`）不走这里**：那条路径由 `win-platform` 用 BitBlt
//! 采集安全桌面，本模块只服务用户会话里的 GUI 进程。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rust_tunnel_remote::codec::{
    VideoDecoderApi, VideoDecoderFactory, VideoEncoderApi, VideoEncoderFactory,
};
use rust_tunnel_remote::screen::{CapturedPatches, ScreenPatch};
use rust_tunnel_remote::video::NATIVE_MAX_ENCODE_WIDTH;
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
/// `frame.width/height`（逻辑编码尺寸）可能小于纹理的物理尺寸（4K 采集先按会话
/// 上限封顶：默认 1920，原生分辨率下就是 3840），故这里同时保留
/// `source_width/source_height` 供编码器缩放。
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
    /// 本会话的编码宽度上限（像素）。引擎在会话开始、codec 协商完成后设定：
    /// 默认 1920，硬件 H.264 + 原生分辨率时 3840（见 [`clamp_encode_width`]）。
    max_encode_width: AtomicU32,
}

impl WinVideoScreen {
    /// 构造采集源（不预打开任何设备）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            sources: Mutex::new(HashMap::new()),
            max_encode_width: AtomicU32::new(MAX_ENCODE_WIDTH),
        }
    }

    /// 当前会话的编码宽度上限。
    fn max_encode_width(&self) -> u32 {
        self.max_encode_width.load(Ordering::Relaxed)
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
        // **在采集层就按会话上限封顶**：4K 源若原样交给引擎，会先生成 33 MB
        // 的 RGBA、再做一次 memcmp 去重与下采样；这里一步 BGRA→目标尺寸 RGBA
        // 把中间缓冲直接压到上限尺寸（默认 1920×1080，原生分辨率下 3840）。
        // `rates.scale` 仍由引擎在这之后生效（例如「流畅」档在半分辨率上再乘 0.5）。
        let max_width = self.max_encode_width();
        let (dst_width, dst_height) =
            encode_dimensions(captured.width, captured.height, 1.0, max_width);
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
            // 这是 WGC 的 CPU 读回路径，没有便宜的变化信号，交由引擎按 memcmp 去重。
            changed: true,
            // 面积未知 = 满帧：CPU 路径不参与按面积收紧码率。
            changed_permille: 1000,
        })
    }

    /// 用硬件后端采集一帧 **GPU 纹理**（不落 CPU）。
    ///
    /// 逻辑尺寸与 CPU 路径一致：先按**会话上限**（默认
    /// [`MAX_ENCODE_WIDTH`]，原生分辨率下 [`NATIVE_MAX_ENCODE_WIDTH`]）封顶，
    /// `rates.scale` 稍后由引擎施加。任何一次失败都会**永久禁用**本进程的 GPU
    /// 路径并回退到 [`Self::capture`]（CPU），而不是每帧重试。
    ///
    /// 同时把采集层的**像素级**变化判断填进 [`RawFrame::changed`] 与变化面积
    /// [`RawFrame::changed_permille`]：前者决定是否跳过这一拍，后者供被控端按内容活跃度
    /// 收紧码率/帧率（CPU 路径仍靠 memcmp 去重，面积未知按满帧处理）。采集后端会先过滤
    /// 「没有新帧」的超时拍，再做 320×180 下采样 + 回读比较——只看「有没有新合成帧」不够，
    /// WGC 在静止桌面上仍按刷新节奏产帧。
    fn capture_gpu_frame(&self, display_id: u32) -> RemoteResult<RawFrame> {
        let (captured, change) = match self.with_source(display_id, |source| {
            source
                .capture_texture_with_change()
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
        let (width, height) = encode_dimensions(
            source_width,
            source_height,
            1.0,
            self.max_encode_width(),
        );
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
            changed: change.changed,
            changed_permille: change.changed_permille,
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

/// 把引擎给的编码宽度上限钳到 `[MAX_ENCODE_WIDTH, NATIVE_MAX_ENCODE_WIDTH]` 并对齐偶数。
///
/// 引擎目前只会传 1920 / 3840，这里仍做钳制：平台侧是**防御边界**。越界值（未来引擎
/// 传 8K、或某个调用方直接调 trait 方法）不该让采集层产出远超编码能力的纹理；
/// 下界 1920 则保证「传 0 / 垃圾值」不会把画面缩成 2 像素。两个边界都是偶数，`& !1`
/// 只影响中间值（保持 NV12 的 4:2:0 偶数约束）。
fn clamp_encode_width(width: u32) -> u32 {
    width.clamp(MAX_ENCODE_WIDTH, NATIVE_MAX_ENCODE_WIDTH) & !1
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

    /// 引擎在会话开始、codec 协商完成后设定本会话的编码宽度上限。
    ///
    /// 采集源据此把源帧一步缩到目标尺寸：默认 1920（4K 源缩 1080p），原生分辨率
    /// 硬件 H.264 时 3840（4K 源保持原尺寸 → NVENC 纹理直通有机会命中）。
    fn set_max_encode_width(&self, width: u32) {
        self.max_encode_width
            .store(clamp_encode_width(width), Ordering::Relaxed);
    }

    fn supports_gpu_capture(&self) -> bool {
        // 环境变量在 `gpu_context()` 初始化时已折算进 `disabled`。
        !gpu_context().is_disabled()
    }

    fn capture_gpu(&self, display_id: u32) -> RemoteResult<RawFrame> {
        self.capture_gpu_frame(display_id)
    }

    fn supports_patches(&self) -> bool {
        // 补丁模式建立在 WGC/DXGI 的 GPU 纹理与变化网格上；GPU 路径被禁用（环境变量或
        // 一次真实的采集/编码失败）时它也不可用。
        !gpu_context().is_disabled()
    }

    /// 采集一帧并产出**脏矩形补丁**（编码尺寸坐标系，紧包 NV12）。
    ///
    /// 只有 H.264 且画面活跃度低时引擎才会调用。返回 `Ok(None)` 表示「本拍不适合补丁」：
    /// 采集失败、变化网格缺失（首帧/检测器不可用/超时复用了上一帧）、或矩形数/面积超限。
    /// **不在这里禁用 GPU 路径**——补丁提取只是优化，失败应当退回全帧视频，而不是把整个
    /// 硬件链路永久关掉。
    fn capture_patches(
        &self,
        display_id: u32,
        scale: f32,
        max_width: u32,
    ) -> RemoteResult<Option<CapturedPatches>> {
        let collected = self.with_source(display_id, |source| {
            source
                .capture_texture_with_dirty()
                .map_err(|err| RemoteError::Platform(format!("GPU 补丁采集失败: {err}")))
        });
        let (captured, report, grid) = match collected {
            Ok(result) => result,
            Err(err) => {
                // GUI 日志层会丢 tracing 字段，原因全部内联。
                debug!("脏矩形补丁采集失败，回退全帧：{err}");
                return Ok(None);
            }
        };
        // 用**实际纹理尺寸**算目标编码尺寸：会话中途改分辨率也不会沿用陈旧尺寸。
        //
        // **必须与视频路径同一套双重换算**：`capture_gpu_frame` 先按 `max_width` 把源封顶
        // （`encode_dimensions(.., 1.0, ..)`），引擎随后再施加 `rates.scale`。若这里直接用
        // 原始纹理 × scale，4K + 非 1.0 缩放会得到 1920×1080，而视频路径得到 960×540 ——
        // 补丁尺寸就与控制端合成帧对不上（见 `controlled.rs` 的同名换算）。
        let (cap_width, cap_height) = encode_dimensions(captured.width, captured.height, 1.0, max_width);
        let (width, height) = encode_dimensions(cap_width, cap_height, scale, max_width);
        let Some(grid) = grid else {
            // 两种「没有网格」必须分开：
            // - `changed == false`：采集后端超时后复用了上一帧（DXGI 静止桌面很常见）。
            //   这不是检测器不可用，只是本拍没有新帧——给一个空补丁当保活，别把引擎
            //   踢回全帧模式。
            // - `changed == true`：首帧或检测器不可用，无法判断脏区，只能回退全帧。
            if !report.changed {
                return Ok(Some(CapturedPatches {
                    display: display_id,
                    width,
                    height,
                    changed_permille: 0,
                    rects: Vec::new(),
                }));
            }
            debug!("脏矩形补丁不可用（变化网格缺失），回退全帧");
            return Ok(None);
        };
        gpu_context().set_device(Arc::clone(&captured.device));
        gpu_context().log_enabled_once(&captured.device.adapter.name);
        let patches = match capture::extract_patches(
            &captured,
            &grid,
            width,
            height,
            &capture::PatchParams::default(),
        ) {
            Ok(Some(patches)) => patches,
            Ok(None) => {
                debug!("脏矩形补丁不适用（矩形数/面积超限或图集过高），回退全帧");
                return Ok(None);
            }
            Err(err) => {
                debug!("脏矩形补丁提取失败，回退全帧：{err}");
                return Ok(None);
            }
        };
        let rects = patches
            .into_iter()
            .map(|patch| ScreenPatch {
                x: patch.x,
                y: patch.y,
                width: patch.width,
                height: patch.height,
                nv12: patch.nv12,
            })
            .collect();
        Ok(Some(CapturedPatches {
            display: display_id,
            width,
            height,
            changed_permille: report.changed_permille,
            rects,
        }))
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

/// 一种硬件 H.264 编码后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// NVIDIA NVENC（H.264 4:2:0，输入可以复用 NVIDIA 采集设备）。
    Nvenc,
    /// Media Foundation 硬件编码 MFT（Intel QSV / AMD VCE 等）。
    Mft,
}

/// 按**采集适配器**决定候选编码后端（按优先级排列）。
///
/// # 为什么不能无条件优先 NVENC
///
/// 混合显卡（Intel 核显负责输出 + NVIDIA 独显）下，采集设备是显示器所接的
/// **Intel** 那块，而 NVENC 会话必须在 NVIDIA 设备上开。跨设备只能回读，
/// 恰好放弃了 Intel QSV「采集纹理 → VideoProcessor → MFT」同一设备的零拷贝
/// 路径。所以采集设备是 Intel/AMD 时先试 MFT；NVIDIA 采集设备、或纯 CPU
/// 路径（`None`，例如 xcap 回退）才先试 NVENC。
///
/// 返回值是**候选序列**而不是单一结论：`create()` 依次尝试，首选失败还能
/// 落到另一个后端，不至于把已经协商好的 H.264 会话直接打断。NVENC 可用但
/// 采集在 Intel、且 MFT 探测为假时，序列里仍有 NVENC（跨设备回读，画面正常）。
fn choose_backend(capture_vendor: Option<u32>, nvenc: bool, mft: bool) -> Vec<Backend> {
    let mut order = Vec::with_capacity(2);
    // Intel/AMD 采集设备：MFT 有机会复用采集设备（Intel 零拷贝，AMD 走 CPU 输入）。
    let prefer_mft = matches!(
        capture_vendor,
        Some(d3d11::VENDOR_INTEL | d3d11::VENDOR_AMD)
    );
    if prefer_mft && mft {
        order.push(Backend::Mft);
    }
    if nvenc {
        order.push(Backend::Nvenc);
    }
    if mft && !prefer_mft {
        order.push(Backend::Mft);
    }
    order
}

/// 进程级 MFT 编码器可用性（**懒探测**）。
///
/// `MftH264Encoder::new` 要枚举硬件 MFT、初始化 COM/Media Foundation，启动成本
/// 明显高于加载 NVENC 函数表。只有真正可能用到它——NVENC 不可用，或采集设备是
/// Intel/AMD——时才付这笔钱。探测结果在进程内缓存，失败不重试。
static MFT_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// 取（或首次探测）硬件 H.264 编码 MFT 是否可用。
fn mft_available() -> bool {
    *MFT_AVAILABLE.get_or_init(|| {
        let available = MftEncoderFactory::probe();
        if available {
            info!("Intel/AMD 硬件 MFT H.264 编码可用");
        }
        available
    })
}

/// 进程启动时探测一次 NVENC：驱动 DLL 可加载 **且** 存在非软件 NVIDIA 适配器。
fn nvenc_supported() -> Option<SharedNvencApi> {
    let api = nvenc_api()?;
    match d3d11::adapters() {
        Ok(adapters) if adapters.iter().any(d3d11::AdapterInfo::is_nvidia) => Some(api),
        Ok(_) => {
            info!("没有可用的 NVIDIA 适配器，NVENC 不参与 H.264 编码");
            None
        }
        Err(err) => {
            info!("枚举 DXGI 适配器失败，NVENC 不参与 H.264 编码：{err}");
            None
        }
    }
}

/// 统一的硬件 H.264 编码器工厂：在 NVENC 与 MFT 之间按采集设备选路。
///
/// 取代原先「只要 NVENC 可用就无条件选 NVENC、否则才探测 MFT」的二选一，
/// 让混合显卡能落到与采集设备同设备的编码器。`api` 为 `None` 表示本进程
/// 没有 NVENC（此时只有 MFT 可作为后端）。
#[derive(Debug)]
pub struct HardwareEncoderFactory {
    /// 进程级 NVENC 函数表。
    api: Option<SharedNvencApi>,
}

impl HardwareEncoderFactory {
    /// 按指定后端创建编码器适配器。
    fn create_backend(
        &self,
        backend: Backend,
        capture: Option<&Arc<d3d11::D3d11>>,
        width: u32,
        height: u32,
        target_bitrate_kbps: u32,
        max_fps: u8,
    ) -> RemoteResult<Box<dyn VideoEncoderApi>> {
        match backend {
            Backend::Nvenc => {
                let api = self
                    .api
                    .ok_or_else(|| RemoteError::Encode("NVENC 不可用".to_owned()))?;
                // 采集设备是 NVIDIA 时**直接复用它**：回读变成设备内 CopyResource，
                // 也避免了第二块设备的显存/上下文开销。其它情况（Intel/AMD 采集、
                // 纯 CPU 路径）才新建一块 NVIDIA 设备，由回读器做跨适配器复制。
                let device = match capture {
                    Some(device) if device.adapter.is_nvidia() => Arc::clone(device),
                    _ => Arc::new(d3d11::create_nvidia().map_err(|err| {
                        RemoteError::Encode(format!("创建 NVIDIA D3D11 设备失败: {err}"))
                    })?),
                };
                let config =
                    EncoderConfig::new(width, height, u32::from(max_fps), target_bitrate_kbps)
                        .map_err(|err| RemoteError::Encode(format!("H.264 编码参数非法: {err}")))?
                        // 回读出来的是 BGRA，而 `NV_ENC_BUFFER_FORMAT_ARGB` 在内存里
                        // 就是 BGRA 字节序：直接上传，省掉一次整帧通道交换。
                        .with_input_format(InputFormat::Bgra);
                let encoder = NvencEncoder::new(api.0, device, config)
                    .map_err(|err| RemoteError::Encode(format!("NVENC 编码器创建失败: {err}")))?;
                Ok(Box::new(NvencAdapter {
                    encoder,
                    readback: BgraReadback::new(),
                    zero_copy_disabled: false,
                    zero_copy_logged: false,
                }))
            }
            Backend::Mft => <MftEncoderFactory as VideoEncoderFactory>::create(
                &MftEncoderFactory,
                width,
                height,
                target_bitrate_kbps,
                max_fps,
            ),
        }
    }
}

impl VideoEncoderFactory for HardwareEncoderFactory {
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
        // 创建编码器时首帧采集通常已经发生，`device()` 就是「显示器所接的适配器」。
        let capture = if gpu_context().is_disabled() {
            None
        } else {
            gpu_context().device()
        };
        let capture_vendor = capture.as_ref().map(|device| device.adapter.vendor_id);
        let nvenc = self.api.is_some();
        // 懒探测：只有 NVENC 不可用、或采集在 Intel/AMD 时 MFT 才可能入选。
        let mft_needed = !nvenc
            || matches!(
                capture_vendor,
                Some(d3d11::VENDOR_INTEL | d3d11::VENDOR_AMD)
            );
        let mft = mft_needed && mft_available();
        let mut last_error = None;
        for backend in choose_backend(capture_vendor, nvenc, mft) {
            match self.create_backend(
                backend,
                capture.as_ref(),
                width,
                height,
                target_bitrate_kbps,
                max_fps,
            ) {
                Ok(encoder) => {
                    // GUI 日志层会丢 tracing 字段，原因全部内联。
                    info!(
                        "H.264 硬件编码后端已选定 backend={backend:?} 采集厂商={capture_vendor:?} 尺寸={width}x{height}"
                    );
                    return Ok(encoder);
                }
                Err(err) => {
                    info!("H.264 编码后端 {backend:?} 创建失败，尝试下一个：{err}");
                    last_error = Some(err);
                }
            }
        }
        // 候选里不含 MFT（懒探测未触发）时最后兜底一次：NVENC 创建失败不该
        // 直接打断已经协商好的 H.264 会话。
        if !mft && mft_available() {
            match self.create_backend(
                Backend::Mft,
                capture.as_ref(),
                width,
                height,
                target_bitrate_kbps,
                max_fps,
            ) {
                Ok(encoder) => {
                    info!("H.264 编码后端回退到 MFT（采集厂商={capture_vendor:?}）");
                    return Ok(encoder);
                }
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| RemoteError::Encode("没有可用的 H.264 硬件编码后端".to_owned())))
    }
}

/// 是否可以用零拷贝直通（把采集纹理直接交给 NVENC，`nvEncRegisterResource`）。
///
/// 三个条件缺一不可：
/// - **同设备**：注册给 NVENC 的资源必须与编码会话在同一块 D3D11 设备上；跨设备
///   注册会被驱动直接拒绝（混合显卡下采集常常落在 Intel 核显）。
/// - **尺寸相等**：NVENC 的 `inputWidth/Height` 只做左上角裁剪、**不缩放**，4K 采集
///   按默认上限缩到 1920 时尺寸不同，必须先回读 + CPU 盒式下采样；原生分辨率（3840）
///   下两者相等，直通才有机会命中。
/// - **本会话未禁用直通**：注册/映射失败过一次就永久回退（单向开关），避免每帧
///   重试一次注定失败的驱动调用。
fn can_use_direct_texture(
    same_device: bool,
    source: (u32, u32),
    encode: (u32, u32),
    disabled: bool,
) -> bool {
    same_device && source == encode && !disabled
}

/// `NvencEncoder` 的 remote 适配器。
///
/// **两条输入路径**（见 [`can_use_direct_texture`]）：
/// - 采集纹理与编码器同设备且尺寸相等时，纹理**直接注册给 NVENC**，全程不落 CPU；
/// - 否则（混合显卡跨设备、4K → 1920 缩放、或直通已在本会话失败过）回读 BGRA，
///   必要时做 CPU 盒式下采样，再走 `UpdateSubresource` 上传。
///
/// 直通失败只关掉直通本身，**不关 GPU 采集**：回读路径仍然可用，当帧照常编码。
struct NvencAdapter {
    /// 硬件编码器（只借用 `'static` 的函数表，设备由编码器持有）。
    encoder: NvencEncoder<'static>,
    /// 可复用的 GPU→CPU 回读器（staging 纹理按尺寸缓存）。
    readback: BgraReadback,
    /// 本会话是否已禁用纹理直通（单向；只影响直通，不影响 GPU 采集）。
    zero_copy_disabled: bool,
    /// 是否已打印过「直通已启用」日志（只打一次，便于真机确认走的是零拷贝）。
    zero_copy_logged: bool,
}

impl NvencAdapter {
    /// 把采集纹理回读成编码器尺寸的 **BGRA**。
    ///
    /// 同尺寸时直接返回回读结果（零转换）；4K 采集封顶到 1920 时用
    /// [`scale_bgra_to_rgba`] 一步完成下采样，再**原地**把 R/B 换回来（通道交换
    /// 自反，省掉一次分配）。编码器配成 [`InputFormat::Bgra`]，所以上传前
    /// **不再**做整帧通道交换。
    fn readback_to_bgra(&mut self, gpu: &WinGpuFrame) -> RemoteResult<Vec<u8>> {
        let (src_width, src_height, bgra) = self.readback.read(&gpu.texture).map_err(|err| {
            gpu_context().disable(&format!("NVENC GPU 帧回读失败: {err}"));
            RemoteError::Encode(format!("NVENC GPU 帧回读失败: {err}"))
        })?;
        let (dst_width, dst_height) = self.encoder.size();
        if (src_width, src_height) == (dst_width, dst_height) {
            return Ok(bgra);
        }
        let mut scaled = scale_bgra_to_rgba(&bgra, src_width, src_height, dst_width, dst_height)
            .map_err(|err| {
                gpu_context().disable(&format!("NVENC GPU 帧下采样失败: {err}"));
                RemoteError::Encode(format!("NVENC GPU 帧下采样失败: {err}"))
            })?;
        for pixel in scaled.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Ok(scaled)
    }
}

/// RGBA → BGRA。
///
/// 通道交换是**自反的**（只交换第 0/2 字节），所以直接复用 [`bgra_to_rgba`]
/// 的优化实现；名字只是为了让调用点的方向一目了然。
fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    bgra_to_rgba(rgba)
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
        //
        // **CPU 输入是 RGBA，而编码器配的是 BGRA**：这里要换一次通道（GPU 回读
        // 路径则天然是 BGRA，不必换）。两条输入路径的格式不能混。
        let bgra = if (src_width, src_height) == (dst_width, dst_height) {
            rgba_to_bgra(rgba)
        } else {
            let scaled = scale_rgba_box(rgba, src_width, src_height, dst_width, dst_height)
                .map_err(|err| RemoteError::Encode(format!("H.264 输入下采样失败: {err}")))?;
            rgba_to_bgra(&scaled)
        };
        let packet = self
            .encoder
            .encode_bgra(&bgra, force_keyframe)
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
        // 代码路径统一走 `WinGpuFrame`；未知载体说明上游接错了，直接报错
        // （不静默编码空帧）。
        let gpu_frame = gpu
            .as_any()
            .downcast_ref::<WinGpuFrame>()
            .ok_or_else(|| RemoteError::Encode("NVENC 收到无法识别的 GPU 帧".to_owned()))?;
        // 直通选路。**同设备**由 `Arc::ptr_eq` 判断：工厂在采集设备是 NVIDIA 时
        // 直接复用它，采集源重建换设备后这里会自动落回回读。
        let source = (gpu_frame.source_width, gpu_frame.source_height);
        if can_use_direct_texture(
            self.encoder.is_same_device(&gpu_frame.texture.device),
            source,
            self.encoder.size(),
            self.zero_copy_disabled,
        ) {
            match self
                .encoder
                .encode_texture(&gpu_frame.texture.texture, force_keyframe)
            {
                Ok(packet) => {
                    if !self.zero_copy_logged {
                        self.zero_copy_logged = true;
                        // GUI 日志层会丢 tracing 字段，全部内联。
                        info!(
                            "NVENC 纹理直通已启用（零拷贝） adapter={} 尺寸={}x{}",
                            gpu_frame.texture.device.adapter.name,
                            gpu_frame.source_width,
                            gpu_frame.source_height,
                        );
                    }
                    return Ok(EncodedVideo {
                        data: packet.data,
                        keyframe: packet.keyframe,
                    });
                }
                Err(err) => {
                    // 直通失败**只关直通**：回读路径照常可用，本帧继续编码。
                    // 关掉整个 GPU 采集会让一次驱动拒绝把画面整体降级到 xcap。
                    self.zero_copy_disabled = true;
                    // GUI 日志层会丢 tracing 字段，原因全部内联。
                    warn!("NVENC 纹理直通不可用，本会话回退回读上传：{err}");
                }
            }
        }
        let bgra = self.readback_to_bgra(gpu_frame)?;
        // 回读结果已经是编码器尺寸的 BGRA，直接上传。
        let packet = self
            .encoder
            .encode_bgra(&bgra, force_keyframe)
            .map_err(|err| RemoteError::Encode(format!("NVENC 编码失败: {err}")))?;
        Ok(EncodedVideo {
            data: packet.data,
            keyframe: packet.keyframe,
        })
    }
}

/// Media Foundation 硬件 H.264 编码器工厂（Intel QSV / AMD VCE 等）。
///
/// 探测是**懒加载**的（见 [`mft_available`]）：NVENC 可用的机器只有在采集设备
/// 是 Intel/AMD 时才会付 `MftEncoderFactory::probe()` 的启动成本。实际是否可用
/// 由 `MftH264Encoder::new` 的硬件 MFT 枚举决定。Intel MFT 不支持运行期改帧率，
/// 适配器会保持内部帧率，只让采集节奏随 QoS 变化，避免重建编码器/插 IDR。
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
/// 返回的每一侧都是独立的：NVENC/MFT 不可用不影响 MFT 解码声明，反之亦然。
/// 任一侧为 `None` 时，协商会回退到 VP9。
///
/// **编码后端在这里不锁定**：NVENC 可用时只代表「H.264 值得声明」，具体用
/// NVENC 还是 MFT 由 [`HardwareEncoderFactory::create`] 在会话建立时按采集
/// 适配器决定（见 [`choose_backend`]）。MFT 探测对 NVENC 可用的机器保持懒加载。
#[must_use]
pub fn factories() -> super::HardwareFactories {
    // 现场诊断开关：怀疑 NVENC/Media Foundation 探测导致启动问题时置 1，
    // 跳过所有硬件探测，数据面完全回退 VP9（GUI 仍应正常启动）。
    if std::env::var_os("RUST_TUNNEL_DISABLE_HWCODEC").is_some() {
        info!("RUST_TUNNEL_DISABLE_HWCODEC 已设置，跳过 NVENC/MFT 探测");
        return (None, None);
    }
    let api = nvenc_supported();
    let encoder = if api.is_some() {
        info!("NVENC H.264 硬件编码已注入（编码后端在会话建立时按采集设备选路）");
        let factory: Arc<dyn VideoEncoderFactory> = Arc::new(HardwareEncoderFactory { api });
        Some(factory)
    } else if mft_available() {
        info!("Intel/AMD 硬件 MFT H.264 编码已注入");
        let factory: Arc<dyn VideoEncoderFactory> = Arc::new(HardwareEncoderFactory { api: None });
        Some(factory)
    } else {
        info!("没有可用的硬件 H.264 编码器（NVENC/MFT），回退 VP9");
        None
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

#[cfg(test)]
mod tests {
    // 测试代码豁免 panic 风险 lint：断言失败即 panic 是测试的预期行为。
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{can_use_direct_texture, choose_backend, clamp_encode_width, Backend};
    use rust_tunnel_win_video::d3d11::{VENDOR_AMD, VENDOR_INTEL, VENDOR_NVIDIA};

    /// 采集侧上限的钳制表：只接受 `[1920, 3840]`、结果恒为偶数。
    ///
    /// 钉住两条：① 引擎传的 1920/3840 原样落地（原生选项不被悄悄降级）；
    /// ② 越界/垃圾值被钳到边界且不产生奇数（奇数会让 NV12 色度块错位）。
    #[test]
    fn clamp_encode_width_table() {
        assert_eq!(clamp_encode_width(1920), 1920);
        assert_eq!(clamp_encode_width(3840), 3840);
        assert_eq!(clamp_encode_width(2560), 2560);
        // 低于下界：钳到 1920（0 也不会把画面缩没）。
        assert_eq!(clamp_encode_width(0), 1920);
        assert_eq!(clamp_encode_width(1280), 1920);
        // 高于上界：钳到 3840。
        assert_eq!(clamp_encode_width(7680), 3840);
        assert_eq!(clamp_encode_width(u32::MAX), 3840);
        // 奇数对齐（中间值）。
        assert_eq!(clamp_encode_width(2559), 2558);
        assert_eq!(clamp_encode_width(1921), 1920);
    }

    /// 厂商 × 后端可用性 → 候选序列的完整表格。
    ///
    /// 钉住的**不变量**：
    /// - Intel/AMD 采集设备上，MFT 可用时**绝不能**把 NVENC 排在前面
    ///   （否则混合显卡会退化成跨设备回读，白丢 QSV 零拷贝）；
    /// - NVIDIA 采集设备 / 未知厂商 / 无采集设备（CPU 路径）优先 NVENC；
    /// - NVENC 不可用时必须回退 MFT（且绝不能返回空序列而漏掉 MFT）；
    /// - 两个后端都不可用时返回空序列（`create()` 才不会误报成功）。
    #[test]
    fn choose_backend_table() {
        const NVIDIA: Option<u32> = Some(VENDOR_NVIDIA);
        const INTEL: Option<u32> = Some(VENDOR_INTEL);
        const AMD: Option<u32> = Some(VENDOR_AMD);
        const OTHER: Option<u32> = Some(0x9999);
        const NONE: Option<u32> = None;
        let cases: [(Option<u32>, bool, bool, Vec<Backend>); 14] = [
            // NVIDIA 采集设备：同设备 NVENC 优先，MFT 作兜底。
            (NVIDIA, true, true, vec![Backend::Nvenc, Backend::Mft]),
            (NVIDIA, true, false, vec![Backend::Nvenc]),
            (NVIDIA, false, true, vec![Backend::Mft]),
            // Intel 采集设备：MFT 永远排在 NVENC 前面。
            (INTEL, true, true, vec![Backend::Mft, Backend::Nvenc]),
            (INTEL, true, false, vec![Backend::Nvenc]),
            (INTEL, false, true, vec![Backend::Mft]),
            // AMD 采集设备：MFT 优先（当前是 CPU RGBA 输入，未验证零拷贝）。
            (AMD, true, true, vec![Backend::Mft, Backend::Nvenc]),
            (AMD, false, true, vec![Backend::Mft]),
            // 其它厂商：与无采集设备同规则（NVENC 优先）。
            (OTHER, true, true, vec![Backend::Nvenc, Backend::Mft]),
            (OTHER, false, true, vec![Backend::Mft]),
            // 无采集设备（CPU 路径）：NVENC 优先。
            (NONE, true, true, vec![Backend::Nvenc, Backend::Mft]),
            (NONE, true, false, vec![Backend::Nvenc]),
            (NONE, false, true, vec![Backend::Mft]),
            (NONE, false, false, vec![]),
        ];
        for (vendor, nvenc, mft, expected) in cases {
            assert_eq!(
                choose_backend(vendor, nvenc, mft),
                expected,
                "vendor={vendor:?} nvenc={nvenc} mft={mft}"
            );
        }
    }

    /// 两条最容易被「顺手优化」推翻的不变量，单独钉一遍。
    #[test]
    fn intel_capture_never_prefers_nvenc() {
        for nvenc in [true, false] {
            let order = choose_backend(Some(VENDOR_INTEL), nvenc, true);
            assert_eq!(order.first(), Some(&Backend::Mft), "nvenc={nvenc}");
        }
    }

    /// NVENC 不可用时，只要 MFT 可用就必须有候选（不能因为采集设备是
    /// NVIDIA 就返回空序列）。
    #[test]
    fn mft_is_used_when_nvenc_is_unavailable() {
        for vendor in [
            None,
            Some(VENDOR_NVIDIA),
            Some(VENDOR_INTEL),
            Some(VENDOR_AMD),
            Some(0x9999),
        ] {
            assert_eq!(
                choose_backend(vendor, false, true),
                vec![Backend::Mft],
                "vendor={vendor:?}"
            );
        }
    }

    /// 直通选路的完整表格。
    ///
    /// 钉住的**不变量**：
    /// - 跨设备（混合显卡：采集在 Intel、编码在 NVIDIA）**永远不能**直通——
    ///   NVENC 注册会失败，而失败一次就会把直通永久关掉；
    /// - 尺寸不等（4K 采集封顶到 1920）**永远不能**直通——NVENC 只裁剪不缩放，
    ///   否则画面会被裁成左上角而不是缩放；
    /// - 会话级 `disabled` 一旦置位，即使前面两个条件恢复也不能再直通。
    #[test]
    fn can_use_direct_texture_table() {
        const HD: (u32, u32) = (1920, 1080);
        const UHD: (u32, u32) = (3840, 2160);
        let cases: [(bool, (u32, u32), (u32, u32), bool, bool); 8] = [
            // 同设备 + 同尺寸 + 未禁用 → 直通。
            (true, HD, HD, false, true),
            // 4K 纹理 / 1080p 编码：要缩放，只能回读。
            (true, UHD, HD, false, false),
            // 跨设备：注册必失败。
            (false, HD, HD, false, false),
            // 会话级禁用：无论其它条件如何都回读。
            (true, HD, HD, true, false),
            (true, UHD, HD, true, false),
            (false, HD, HD, true, false),
            // 编码器比采集纹理大（不该出现，但不能因此误判成可直通）。
            (true, HD, UHD, false, false),
            // 尺寸与设备同时满足、但禁用标志为真。
            (true, HD, HD, true, false),
        ];
        for (same_device, source, encode, disabled, expected) in cases {
            assert_eq!(
                can_use_direct_texture(same_device, source, encode, disabled),
                expected,
                "same_device={same_device} source={source:?} encode={encode:?} disabled={disabled}"
            );
        }
    }

    /// 禁用标志是单向的：一旦置位，健康条件下也不能重新启用（避免每帧重试）。
    #[test]
    fn direct_texture_stays_disabled_once_flagged() {
        assert!(can_use_direct_texture(
            true,
            (1920, 1080),
            (1920, 1080),
            false
        ));
        assert!(!can_use_direct_texture(
            true,
            (1920, 1080),
            (1920, 1080),
            true
        ));
    }
}
