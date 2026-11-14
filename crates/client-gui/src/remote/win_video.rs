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
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use rust_tunnel_remote::codec::{
    VideoDecoderApi, VideoDecoderFactory, VideoEncoderApi, VideoEncoderFactory,
};
use rust_tunnel_remote::{
    encode_dimensions, CodecId, DecodedVideoFrame, DisplayInfo, EncodedVideo, RawFrame,
    RemoteError, RemoteResult, ScreenSource, MAX_ENCODE_WIDTH,
};
use rust_tunnel_win_video::capture::{self, CaptureSource};
use rust_tunnel_win_video::d3d11;
use rust_tunnel_win_video::decode::H264Decoder;
use rust_tunnel_win_video::mft_encoder::MftH264Encoder;
use rust_tunnel_win_video::nvenc::{EncoderConfig, InputFormat, NvencApi, NvencEncoder};
use rust_tunnel_win_video::pixels::{bgra_to_rgba, scale_bgra_to_rgba, scale_rgba_box};
use tracing::{debug, info, warn};

use super::XcapScreen;

/// 一个已打开的硬件采集源及其对应的显示器几何。
struct CachedSource {
    /// WGC 或 DXGI 采集源。
    source: Box<dyn CaptureSource>,
    /// 打开时的虚拟桌面几何；几何变化时丢弃重开。
    geometry: Geometry,
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

    /// 尝试用硬件后端采集一帧；失败时由调用方回退 xcap。
    ///
    /// 失败会把缓存项移除，下一次调用重新尝试打开（`open_best` 内部按
    /// WGC → DXGI 顺序选择）。
    fn capture_hardware(&self, display_id: u32) -> RemoteResult<RawFrame> {
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

        let mut sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
        let stale = sources
            .get(&display_id)
            .is_none_or(|cached| cached.geometry != geometry);
        if stale {
            sources.remove(&display_id);
            let source = capture::open_best(target)
                .map_err(|err| RemoteError::Platform(format!("打开硬件采集源失败: {err}")))?;
            debug!(display_id, backend = %source.backend(), "硬件采集源已打开");
            sources.insert(display_id, CachedSource { source, geometry });
        }
        let captured = match sources.get_mut(&display_id) {
            Some(cached) => cached.capture_frame(),
            None => Err(RemoteError::Platform("硬件采集源缓存丢失".to_owned())),
        };
        let captured = match captured {
            Ok(frame) => frame,
            Err(err) => {
                sources.remove(&display_id);
                return Err(err);
            }
        };
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
        })
    }
}

impl Default for WinVideoScreen {
    fn default() -> Self {
        Self::new()
    }
}

/// 已缓存采集源的一次采帧（把 `Box<dyn CaptureSource>` 的 `&mut` 封装起来）。
impl CachedSource {
    /// 采一帧并把底层错误转成引擎错误。
    fn capture_frame(&mut self) -> RemoteResult<capture::CapturedFrame> {
        self.source
            .capture()
            .map_err(|err| RemoteError::Platform(format!("硬件采集失败: {err}")))
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
        Ok(Box::new(NvencAdapter { encoder }))
    }
}

/// `NvencEncoder` 的 remote 适配器：尺寸不同时先做 CPU 盒式下采样。
struct NvencAdapter {
    /// 硬件编码器（只借用 `'static` 的函数表）。
    encoder: NvencEncoder<'static>,
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
        let encoder = MftH264Encoder::new(width, height, fps, target_bitrate_kbps)
            .map_err(|err| RemoteError::Encode(format!("硬件 MFT 编码器创建失败: {err}")))?;
        Ok(Box::new(MftAdapter { encoder, fps }))
    }
}

/// `MftH264Encoder` 的 remote 适配器：尺寸不同时先做 CPU 盒式下采样。
struct MftAdapter {
    /// 硬件编码器。
    encoder: MftH264Encoder,
    /// 逻辑帧率（Intel MFT 不支持在线改输出帧率，实际节奏由采集控制）。
    fps: u32,
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
        let packet = if (src_width, src_height) == (dst_width, dst_height) {
            self.encoder.encode_rgba(rgba, force_keyframe)
        } else {
            let scaled = scale_rgba_box(rgba, src_width, src_height, dst_width, dst_height)
                .map_err(|err| RemoteError::Encode(format!("H.264 输入下采样失败: {err}")))?;
            self.encoder.encode_rgba(&scaled, force_keyframe)
        }
        .map_err(|err| RemoteError::Encode(format!("MFT 编码失败: {err}")))?;
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
        }))
    }
}

/// 惰性适配器：`configure(width, height)` 之前不创建 MFT。
struct H264DecoderAdapter {
    /// 当前解码器；`None` 表示尚未 configure 或上次创建失败。
    decoder: Option<H264Decoder>,
    /// 当前解码器适配的编码尺寸。
    configured: Option<(u32, u32)>,
}

impl VideoDecoderApi for H264DecoderAdapter {
    fn configure(&mut self, width: u32, height: u32) -> RemoteResult<()> {
        if self.decoder.is_some() && self.configured == Some((width, height)) {
            return Ok(());
        }
        match H264Decoder::new(width, height) {
            Ok(decoder) => {
                info!(
                    path = ?decoder.path(),
                    adapter = ?decoder.adapter_name(),
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
        let frame = decoder
            .decode(data)
            .map_err(|err| RemoteError::Encode(format!("H.264 解码失败: {err}")))?;
        Ok(frame.map(|frame| DecodedVideoFrame {
            width: frame.width,
            height: frame.height,
            rgba: frame.rgba,
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
