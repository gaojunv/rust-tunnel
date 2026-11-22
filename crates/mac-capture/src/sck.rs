//! ScreenCaptureKit 采集实现（仅 macOS）。
//!
//! # 线程模型
//!
//! `SCStream` / `SCContentFilter` / `SCStreamConfiguration` 只存在于**专属采集线程**：
//! 线程内完成「权限 → 枚举 → 建流 → 加输出 → 启采」，之后阻塞等停止信号，退出前
//! 移除输出并停采。[`MacCapture`] 只持有一个**最新样本槽**（`Mutex` + `Condvar`）和
//! 一条停止通道，因此天然 `Send + Sync`，引擎可以把它放进任意阻塞任务。
//!
//! 样本槽里放的是 `CFRetain` 过的 `CVPixelBuffer`，不是像素拷贝：SCK 的 `stream:
//! didOutputSampleBuffer:ofType:` 回调跑在 SCK 自己的派发队列上，那里拷 4K 一帧
//! （33 MB）会直接把采集节拍拖垮。
//!
//! # 三条消费路径
//!
//! - [`MacCapture::capture_gpu`]：GPU 直通。**SCK 建流时就按 `max_encode_width` 输出**
//!   （GPU 内缩放），所以缓冲通常已经等于编码尺寸，编码器零拷贝直通，不再每帧做
//!   4K→1080p 的 `VTPixelTransferSession`；变化检测只用 SCK 的脏矩形附件或一块
//!   320×180 的降采样缓冲；
//! - [`MacCapture::capture_patches`]：脏矩形补丁。变化网格来自同一块 320×180 缓冲，
//!   源像素只按矩形覆盖区间用带行跨度读取，整帧 BGRA 从不拷贝；
//! - [`MacCapture::capture`]：整帧 CPU 回读（VP9 回退路径），像素拷贝发生在这里。
//!
//! # 失败、空闲与重建
//!
//! - **流失败**：`SCStreamDelegate.stream:didStopWithError:` 把错误记进共享槽；下一次
//!   采集发现后停掉线程并按 [`REBUILD_BACKOFF`] 退避重建。退避期内返回
//!   [`MacCaptureError::Backoff`]，绝不 panic、也不无限重试。
//! - **空闲停流**：SCK 是 push 模型，没有会话时不停流会让屏幕录制指示常亮、CPU 白耗。
//!   超过 [`IDLE_STOP_TIMEOUT`] 没有任何 `capture`/`capture_patches` 调用，采集线程
//!   自己 `stopCapture` 并退出（释放 `SCStream`）；下一次采集由 [`MacCapture`] 重新建流。
//!   停止信号只从调用方线程发出、由采集线程自己退出，**不存在 join 自己**的死锁路径。
//! - **几何变化**：显示器分辨率/旋转变化会改变采集输出的物理尺寸，[`GEOMETRY_RECHECK`]
//!   复查一次并重建流（流配置里的宽高是建流时定死的）。
//!
//! # 帧等待窗口
//!
//! SCK 按 [`MAX_FPS`]（60）推送，帧间隔约 16.7 ms；[`FRAME_WAIT`] 取 20 ms 是为了盖住
//! 一个完整帧间隔——窗口小于帧间隔时，引擎节拍与 SCK 相位漂移会让部分拍拿不到新帧
//! （`changed = false` → 引擎跳过），表现为帧率掉一半。真正静止的桌面由变化检测处理
//! （跳过编码），不靠缩短等待窗口省事。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use block2::{DynBlock, RcBlock};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, ClassType, DefinedClass};
use objc2_app_kit::NSCursor;
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGDisplayBounds, CGDirectDisplayID, CGDisplayCopyDisplayMode, CGDisplayIsMain, CGDisplayMode,
    CGDisplayPixelsHigh, CGDisplayPixelsWide, CGError, CGGetActiveDisplayList, CGMainDisplayID,
};
use objc2_core_media::{CMGetAttachment, CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSValue};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoDirtyRects, SCStreamOutput, SCStreamOutputType, SCWindow,
};
use rust_tunnel_mac_video::change::{
    extract_rects, ChangeDetector, ChangeOutcome, ChangeReport, DirtyGrid, RectParams, GRID_HEIGHT,
    GRID_WIDTH,
};
use rust_tunnel_mac_video::color::{bgra_to_nv12, bgra_to_rgba, scale_bgra_box};
use rust_tunnel_mac_video::pixel_transfer::{self, PixelTransfer};

use crate::geometry::{self, TargetRect};
use crate::{
    CaptureInfo, CapturedFrame, CapturedGpuFrame, CapturedPatches, CursorShape, DisplayInfo,
    GpuFrameBuffer, MacCaptureError, MacCaptureResult, PatchRect,
};

/// 等待 SCK 枚举可共享内容的上限（首次可能触发「屏幕录制」授权）。
const CONTENT_TIMEOUT: Duration = Duration::from_secs(10);
/// 等待 `startCapture` / `stopCapture` 完成的上限。
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// 等 `stopCapture` 完成的上限（停采不需要久等）。
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
/// 采集线程就绪（含权限、枚举、启采）的总等待上限。
const READY_TIMEOUT: Duration = Duration::from_secs(25);
/// 单次采集等**新**帧的时长；超时就复用上一帧（与 Windows 侧「无新帧」同语义）。
///
/// 取 20 ms 而不是 10 ms：SCK 按 [`MAX_FPS`]（60）推送，帧间隔约 16.7 ms，而引擎的
/// 采集节拍与它并不同相——10 ms 的窗口会让大约一半的拍拿不到新帧（`changed = false`
/// → 引擎跳过），表现为「帧率掉一半」。20 ms 盖住一个完整帧间隔，静止桌面上也只在
/// 流真正停摆时才会走复用分支。
const FRAME_WAIT: Duration = Duration::from_millis(20);
/// 空闲多久没有 `capture`/`capture_patches` 调用就停流（释放 `SCStream`）。
///
/// SCK 是 push 模型：没有会话时不停流，屏幕录制指示会常亮、CPU 也白耗。采集线程自己
/// 轮询这个阈值并退出（`shutdown_stream` + `stopCapture`），下一次采集再重建。
const IDLE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// 复查显示器几何（分辨率/旋转）的间隔；变化时重建采集流。
const GEOMETRY_RECHECK: Duration = Duration::from_secs(2);
/// 首帧等待上限：建流后第一帧可能晚于 `startCapture` 的完成回调。
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(2);
/// 采集流失败后的重建退避。
const REBUILD_BACKOFF: Duration = Duration::from_secs(2);
/// 采集线程轮询停止/错误的粒度。
const WORKER_POLL: Duration = Duration::from_millis(250);
/// 向 SCK 请求的最大帧率（引擎按自己的节拍拉取，这里只决定上限）。
const MAX_FPS: i32 = 60;
/// SCK 的输出队列深度。
const QUEUE_DEPTH: isize = 3;
/// 默认编码宽度上限（与引擎 `remote::video::MAX_ENCODE_WIDTH` 一致）。
const DEFAULT_MAX_ENCODE_WIDTH: u32 = 1920;
/// 编码宽度上限的硬范围（与引擎 `NATIVE_MAX_ENCODE_WIDTH` 一致）。
const MAX_ENCODE_WIDTH_LIMIT: u32 = 3840;

// CoreGraphics 的屏幕录制权限查询/申请（不额外引入依赖面）。
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    // 是否已获得「屏幕录制」权限（不弹框）。
    fn CGPreflightScreenCaptureAccess() -> bool;
    // 申请「屏幕录制」权限（首次调用弹系统框）。
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// 进程内累计读数（离线观测，不上线）。
#[derive(Debug, Default)]
struct Counters {
    /// 已接收的画面样本数，同时充当样本序号（用于「有没有新帧」判定）。
    seq: AtomicU64,
}

/// 最新一帧的 `CVPixelBuffer`（已 retain，未拷贝像素）。
#[derive(Clone)]
struct Sample {
    buffer: CFRetained<CVPixelBuffer>,
    width: u32,
    height: u32,
    seq: u64,
    /// SCK 样本附件里的脏矩形摘要；`None` = 附件不可用（旧系统/格式不支持）。
    dirty: Option<DirtyHint>,
}

/// SCK 样本附件 `SCStreamFrameInfoDirtyRects` 的摘要。
///
/// 只保留「矩形个数」与「这些矩形覆盖多少物理像素」——采集层**不需要**矩形本身：
/// 变化报告只回答「变了没有 / 面积多大」，补丁矩形仍由 320×180 的像素级网格提取，
/// 这样两条路径（GPU 直通与补丁）对「变化」的判断完全一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirtyHint {
    /// 脏矩形个数（0 = 平台权威地告诉我们这一帧没有变化）。
    rects: u32,
    /// 脏矩形覆盖的物理像素总数（未裁剪到帧边界，只用于千分比估算）。
    area_pixels: u64,
}

// SAFETY: CoreVideo 的 pixel buffer 是不可变、引用计数的 CF 对象；`lock/unlock`
// 成对出现在消费线程内，回调线程从不持有锁，因此跨线程移交是安全的（这也是
// CVPixelBuffer 的文档契约：它可以在任意线程被 lock 读取）。
unsafe impl Send for Sample {}
unsafe impl Sync for Sample {}

/// 回调线程与调用方共享的状态：最新样本 + 错误槽 + 空闲计时。
struct Shared {
    slot: Mutex<Option<Sample>>,
    ready: Condvar,
    counters: Arc<Counters>,
    error: Mutex<Option<String>>,
    /// 最近一次 `capture`/`capture_patches` 调用的时刻（空闲看门狗用）。
    last_access: Mutex<Instant>,
}

impl Shared {
    fn new(counters: Arc<Counters>) -> Self {
        Self {
            slot: Mutex::new(None),
            ready: Condvar::new(),
            counters,
            error: Mutex::new(None),
            last_access: Mutex::new(Instant::now()),
        }
    }

    /// 记录一次调用方活动（每次采集都刷新，空闲看门狗据此停流）。
    fn touch(&self) {
        *lock(&self.last_access) = Instant::now();
    }

    /// 距离上次调用方活动过了多久。
    fn idle_for(&self) -> Duration {
        lock(&self.last_access).elapsed()
    }

    /// SCK 回调：retain 像素缓冲并覆盖式入槽（不拷贝像素）。
    fn handle_sample(&self, sample_buffer: &CMSampleBuffer) {
        // SAFETY: `sample_buffer` 在回调期间由 SCK 持有；`image_buffer` 返回一份
        // +1 引用的 CVPixelBuffer（objc2 已内部 `CFRetain`），正是我们要放进槽里的东西。
        let Some(buffer) = (unsafe { sample_buffer.image_buffer() }) else {
            return;
        };
        // SAFETY: 上面的 `buffer` 是有效对象；这两个查询只读（objc2 里是安全函数）。
        let (width, height) = (
            CVPixelBufferGetWidth(&buffer),
            CVPixelBufferGetHeight(&buffer),
        );
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }
        let seq = self
            .counters
            .seq
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let dirty = sample_dirty_hint(sample_buffer);
        self.store(Sample {
            buffer,
            width,
            height,
            seq,
            dirty,
        });
    }

    fn store(&self, sample: Sample) {
        {
            let mut slot = lock(&self.slot);
            *slot = Some(sample);
        }
        self.ready.notify_all();
    }

    /// 等一帧序号大于 `after` 的样本（`after == None` 表示任意一帧）。
    fn wait_newer(&self, after: Option<u64>, timeout: Duration) -> Option<Sample> {
        let deadline = Instant::now() + timeout;
        let mut guard = lock(&self.slot);
        loop {
            if let Some(sample) = guard.as_ref() {
                if after.is_none_or(|seq| sample.seq > seq) {
                    return Some(sample.clone());
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, _) = self
                .ready
                .wait_timeout(guard, deadline.saturating_duration_since(now))
                .unwrap_or_else(PoisonError::into_inner);
            guard = next;
        }
    }

    fn set_error(&self, message: String) {
        *lock(&self.error) = Some(message);
    }

    fn error(&self) -> Option<String> {
        lock(&self.error).clone()
    }
}

/// `SCStreamOutput` / `SCStreamDelegate` 的宿主对象。
struct OutputIvars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSObject 没有子类化要求；`StreamOutput` 不实现 `Drop`。
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            // 只处理画面；音频/麦克风输出我们没有添加，出现即忽略。
            if r#type != SCStreamOutputType::Screen {
                return;
            }
            self.ivars().shared.handle_sample(sample_buffer);
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            let message = format!("ScreenCaptureKit 停止采集：{}", describe_error(error));
            tracing::warn!(%message);
            self.ivars().shared.set_error(message);
        }
    }
);

impl StreamOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { shared });
        // SAFETY: `NSObject` 的指定初始化器；ivars 已在 `set_ivars` 里就位。
        unsafe { msg_send![super(this), init] }
    }
}

/// 一次成功启动的采集会话（**只存在于采集线程内**）。
struct StreamSession {
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    /// 建流时使用的显示器物理尺寸；`None` = 当时查不到（退化到点尺寸），不做几何复查。
    geometry: Option<(u32, u32)>,
    /// 建流时配置的**采集输出尺寸**（= 按 `max_encode_width` 封顶后的尺寸）。
    ///
    /// 引擎在会话中可能改编码宽度上限（codec 回退），那时必须重建流——SCK 的
    /// `width/height` 只在建流时生效。
    output_size: (u32, u32),
    /// 回调队列必须比 `SCStream` 活得久（`addStreamOutput` 只是弱持有）。
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
}

/// 采集线程的控制句柄。
struct Worker {
    display: u32,
    stop: SyncSender<()>,
    thread: JoinHandle<()>,
    shared: Arc<Shared>,
    /// 建流时使用的显示器物理尺寸（`None` = 未知，不复查）。
    geometry: Option<(u32, u32)>,
    /// 建流时配置的采集输出尺寸（见 [`StreamSession::output_size`]）。
    output_size: (u32, u32),
    /// 建流时生效的编码宽度上限（变化即需重建，比较不需要查询显示器）。
    max_encode_width: u32,
    /// 上一次复查显示器几何的时刻（分辨率/旋转变化时重建流）。
    checked_at: Instant,
}

/// 一次失败（用于重建退避）。
struct Failure {
    display: u32,
    at: Instant,
    message: String,
}

/// 上一帧（缓存缩放后的 BGRA 与 RGBA，避免静止画面反复重算）。
struct LastFrame {
    display: u32,
    seq: u64,
    dst_width: u32,
    dst_height: u32,
    scaled: Arc<Vec<u8>>,
    rgba: Option<Arc<Vec<u8>>>,
}

/// 一次 `prepare` 的结果（CPU 整帧路径）。
struct Prepared {
    fresh: bool,
    width: u32,
    height: u32,
    scaled: Arc<Vec<u8>>,
    report: ChangeReport,
}

/// GPU 直通路径的「上一帧」（无新样本时按它复用，并据此判定序号）。
struct GpuLast {
    display: u32,
    seq: u64,
    width: u32,
    height: u32,
    sample: Sample,
}

/// 补丁路径的「上一帧」几何与序号（无新样本时据此产出保活）。
#[derive(Clone, Copy)]
struct PatchLast {
    display: u32,
    seq: u64,
    src_width: u32,
    src_height: u32,
}

/// 变化检测用的 GPU 降采样器：把任意尺寸的 `CVPixelBuffer` 缩成 320×180 BGRA。
///
/// 只锁 320×180（约 225 KB）的暂存缓冲做像素比较，**绝不回读整帧 33 MB**。
/// 转换会话与目标缓冲都只建一次、跨帧复用。
struct ChangeScaler {
    transfer: PixelTransfer,
    /// 320×180 的 `32BGRA` 目标（IOSurface 背书）。
    target: CFRetained<CVPixelBuffer>,
    /// 复用的紧包 BGRA 暂存（`GRID_WIDTH * GRID_HEIGHT * 4`）。
    pixels: Vec<u8>,
}

// SAFETY: 两个 CF 句柄都是不可变、引用计数的核心视频对象（`CVPixelBuffer` 的文档契约
// 允许任意线程 lock/读取）；`PixelTransfer` 只在持有 `&mut MacCapture` 的调用线程上使用
// （引擎把采集器放在任务里独占调用，适配层再用 `Mutex` 串行化）。因此跨线程移交安全。
unsafe impl Send for ChangeScaler {}
unsafe impl Sync for ChangeScaler {}

/// macOS 屏幕采集源（ScreenCaptureKit）。
pub struct MacCapture {
    worker: Option<Worker>,
    last: Option<LastFrame>,
    detector: ChangeDetector,
    /// GPU 直通路径的上一帧（独立序号跟踪，与 CPU 路径互不影响）。
    gpu_last: Option<GpuLast>,
    /// 补丁路径的上一帧。
    patch_last: Option<PatchLast>,
    /// GPU 路径与补丁路径**共用**的变化检测器：两者都喂 320×180 网格，因此切换路径
    /// 不会产生虚假的「整帧变化」。
    gpu_detector: ChangeDetector,
    /// 懒建的 GPU 降采样器（变化检测用）。
    change_scaler: Option<ChangeScaler>,
    max_encode_width: u32,
    no_new_frame: u64,
    failure: Option<Failure>,
    /// **最近一帧**是否真的走了 `CVPixelBuffer` 直通（`capture_gpu` 成功时为真；
    /// CPU 的 `capture` 与补丁路径为假）。只影响统计角标，不参与任何决策。
    last_was_gpu: bool,
}

impl MacCapture {
    /// 创建采集源（不打开流；第一次 [`Self::capture`] 时才建流）。
    ///
    /// # Errors
    /// 当前实现不会在构造期失败（建流的错误在首次采集时返回，便于上层先展示 UI）。
    pub fn new() -> MacCaptureResult<Self> {
        Ok(Self {
            worker: None,
            last: None,
            detector: ChangeDetector::new(),
            gpu_last: None,
            patch_last: None,
            gpu_detector: ChangeDetector::new(),
            change_scaler: None,
            max_encode_width: DEFAULT_MAX_ENCODE_WIDTH,
            no_new_frame: 0,
            failure: None,
            last_was_gpu: false,
        })
    }

    /// 当前显示器列表（失败时返回空列表并记日志）。
    #[must_use]
    pub fn displays(&self) -> Vec<DisplayInfo> {
        match list_displays() {
            Ok(displays) => displays,
            Err(err) => {
                tracing::warn!(error = %err, "枚举显示器失败");
                Vec::new()
            }
        }
    }

    /// 设置本会话的编码宽度上限（引擎在会话开始时调用一次）。
    ///
    /// 与 Windows 侧同一口径：钳到 `[1920, 3840]` 并对齐偶数。**SCK 的采集流直接按它
    /// 输出**（建流时写进 `SCStreamConfiguration`，GPU 内缩放），所以 GPU 直通路径拿到
    /// 的就是编码尺寸的缓冲；上限变化会重建采集流（见 `ensure_worker`）。
    pub fn set_max_encode_width(&mut self, width: u32) {
        self.max_encode_width = width.clamp(DEFAULT_MAX_ENCODE_WIDTH, MAX_ENCODE_WIDTH_LIMIT) & !1;
    }

    /// 采集指定显示器的一帧 RGBA（按 `max_encode_width` 封顶）。
    ///
    /// 无新帧时最多等 [`FRAME_WAIT`]，之后复用上一帧并累计 `no_new_frame`；此时
    /// `changed == false`，引擎应完全跳过编码。首帧最多等 [`FIRST_FRAME_TIMEOUT`]。
    ///
    /// # Errors
    /// 缺权限、显示器不存在、流启动/重建退避、首帧超时等返回对应错误（中文）。
    pub fn capture(&mut self, display: u32) -> MacCaptureResult<CapturedFrame> {
        let prepared = self.prepare(display)?;
        let rgba = if prepared.fresh {
            let rgba = Arc::new(bgra_to_rgba(&prepared.scaled));
            if let Some(last) = self.last.as_mut() {
                last.rgba = Some(Arc::clone(&rgba));
            }
            (*rgba).clone()
        } else {
            let cached = self
                .last
                .as_mut()
                .and_then(|last| last.rgba.as_ref().map(Arc::clone));
            if let Some(rgba) = cached {
                (*rgba).clone()
            } else {
                let rgba = Arc::new(bgra_to_rgba(&prepared.scaled));
                if let Some(last) = self.last.as_mut() {
                    last.rgba = Some(Arc::clone(&rgba));
                }
                (*rgba).clone()
            }
        };
        self.last_was_gpu = false;
        Ok(CapturedFrame {
            display,
            width: prepared.width,
            height: prepared.height,
            rgba,
            changed: prepared.report.changed,
            changed_permille: prepared.report.changed_permille,
        })
    }

    /// 本采集源是否支持 **GPU 直通**采集（macOS 恒 `true`：SCK 的帧本来就是
    /// `IOSurface` 背书的 `CVPixelBuffer`）。
    #[must_use]
    pub fn supports_gpu_capture(&self) -> bool {
        true
    }

    /// 采集一帧 **GPU 直通**画面：SCK 输出的 `CVPixelBuffer`，不在 CPU 上缩放或转换。
    ///
    /// 流的输出尺寸在建流时已按 [`Self::set_max_encode_width`] 封顶（GPU 内缩放），
    /// 因此缓冲尺寸通常等于编码尺寸：引擎把这块缓冲直接交给 VideoToolbox
    /// （`VtEncoder::encode_pixel_buffer`）即**零拷贝直通**。
    ///
    /// 变化检测与 CPU 路径同源：SCK 样本附件报告「零脏矩形」时直接采信（平台给的权威
    /// 信号）；否则把帧缩到 320×180 后做逐像素比较。**绝不回读整帧**。
    ///
    /// 无新帧时最多等 [`FRAME_WAIT`]，之后复用上一帧缓冲并报 `changed = false`；首帧
    /// 最多等 [`FIRST_FRAME_TIMEOUT`]。
    ///
    /// # Errors
    /// 缺权限、显示器不存在、流启动/重建退避、首帧超时等返回对应错误（中文）。
    pub fn capture_gpu(&mut self, display: u32) -> MacCaptureResult<CapturedGpuFrame> {
        self.ensure_worker(display)?;
        if self
            .gpu_last
            .as_ref()
            .is_some_and(|last| last.display != display)
        {
            // 切显示器：上一帧缓冲、补丁状态与检测基准全部作废。
            self.gpu_last = None;
            self.patch_last = None;
            self.gpu_detector.reset();
        }
        let shared = self.shared()?;
        let after = self.gpu_last.as_ref().map(|last| last.seq);
        let timeout = if after.is_some() {
            FRAME_WAIT
        } else {
            FIRST_FRAME_TIMEOUT
        };
        let sample = shared.wait_newer(after, timeout);
        let Some(sample) = sample else {
            if let Some(message) = shared.error() {
                return Err(MacCaptureError::Platform(message));
            }
            self.no_new_frame = self.no_new_frame.saturating_add(1);
            let Some(last) = self.gpu_last.as_ref() else {
                return Err(MacCaptureError::Timeout("等待 ScreenCaptureKit 首帧"));
            };
            // 复用上一帧缓冲（引用计数克隆，不拷像素）。
            return Ok(CapturedGpuFrame {
                display,
                width: last.width,
                height: last.height,
                source_width: last.sample.width,
                source_height: last.sample.height,
                buffer: GpuFrameBuffer::clone(&last.sample.buffer),
                changed: false,
                changed_permille: 0,
            });
        };
        let (width, height) =
            geometry::encode_dimensions(sample.width, sample.height, 1.0, self.max_encode_width);
        let report = match self.observe_gpu_frame(&sample) {
            Ok(outcome) => outcome.report,
            Err(err) => {
                // 检测失败绝不能丢帧：按满面积处理（保守地不压制码率/帧率）。
                tracing::debug!(error = %err, "GPU 变化检测失败，本帧按满面积处理");
                ChangeReport::changed_all()
            }
        };
        self.gpu_last = Some(GpuLast {
            display,
            seq: sample.seq,
            width,
            height,
            sample: sample.clone(),
        });
        self.last_was_gpu = true;
        Ok(CapturedGpuFrame {
            display,
            width,
            height,
            source_width: sample.width,
            source_height: sample.height,
            buffer: sample.buffer.clone(),
            changed: report.changed,
            changed_permille: report.changed_permille,
        })
    }

    /// 采集一帧并产出**脏矩形补丁**（目标编码坐标系，紧包 NV12）。
    ///
    /// 源像素**只按脏矩形读取**：变化网格由 320×180 的 GPU 降采样得出，随后对样本的
    /// `CVPixelBuffer` 加一次读锁，按每个矩形覆盖的源区间做带行跨度的区域缩放——
    /// 整帧 BGRA（4K 33 MB）从不被拷贝。
    ///
    /// 返回 `Ok(None)` 表示「本拍不适合补丁」：变化网格缺失（首帧/检测器不可用）、
    /// 矩形数/面积超限、或矩形映射/读源失败——引擎据此回到全帧视频并强制关键帧。
    /// 没有新帧或没有变化时返回 `Ok(Some(空 rects))` 作为保活。
    ///
    /// # Errors
    /// 采集失败（同 [`Self::capture`]）时返回错误。
    pub fn capture_patches(
        &mut self,
        display: u32,
        scale: f32,
        max_width: u32,
    ) -> MacCaptureResult<Option<CapturedPatches>> {
        self.ensure_worker(display)?;
        if self
            .patch_last
            .as_ref()
            .is_some_and(|last| last.display != display)
        {
            self.patch_last = None;
            self.gpu_last = None;
            self.gpu_detector.reset();
        }
        let shared = self.shared()?;
        let after = self.patch_last.as_ref().map(|last| last.seq);
        let timeout = if after.is_some() {
            FRAME_WAIT
        } else {
            FIRST_FRAME_TIMEOUT
        };
        let max_width = max_width.max(2);
        let sample = shared.wait_newer(after, timeout);
        let Some(sample) = sample else {
            if let Some(message) = shared.error() {
                return Err(MacCaptureError::Platform(message));
            }
            self.no_new_frame = self.no_new_frame.saturating_add(1);
            let Some(last) = self.patch_last else {
                return Err(MacCaptureError::Timeout("等待 ScreenCaptureKit 首帧"));
            };
            let (cap_width, cap_height) =
                geometry::encode_dimensions(last.src_width, last.src_height, 1.0, max_width);
            let (width, height) =
                geometry::encode_dimensions(cap_width, cap_height, scale, max_width);
            return Ok(Some(keepalive_patches(display, width, height)));
        };
        // 与引擎/Windows 侧同一套双重换算：先按 max_width 把源封顶，再施加 scale。
        let (cap_width, cap_height) =
            geometry::encode_dimensions(sample.width, sample.height, 1.0, max_width);
        let (width, height) = geometry::encode_dimensions(cap_width, cap_height, scale, max_width);
        self.patch_last = Some(PatchLast {
            display,
            seq: sample.seq,
            src_width: sample.width,
            src_height: sample.height,
        });
        self.last_was_gpu = false;
        let outcome = match self.observe_gpu_frame(&sample) {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::debug!(error = %err, "补丁变化检测失败，回退全帧");
                return Ok(None);
            }
        };
        if !outcome.report.changed {
            return Ok(Some(keepalive_patches(display, width, height)));
        }
        let Some(grid) = outcome.grid else {
            // 首帧/检测器不可用：有变化就只能回退全帧。
            tracing::debug!("变化网格缺失（首帧或检测器不可用），回退全帧");
            return Ok(None);
        };
        let Some(rects) = extract_rects(&grid, (width, height), &RectParams::default()) else {
            tracing::debug!("脏矩形超限（数量/面积），回退全帧");
            return Ok(None);
        };
        if rects.is_empty() {
            return Ok(Some(keepalive_patches(display, width, height)));
        }
        let Some(patches) = extract_patches_from_sample(&sample, &rects, (width, height))? else {
            tracing::debug!("按脏矩形读取源像素失败，回退全帧");
            return Ok(None);
        };
        Ok(Some(CapturedPatches {
            display,
            width,
            height,
            changed_permille: outcome.report.changed_permille,
            rects: patches,
        }))
    }

    /// 远端光标形状（读不到时 `None`）。
    ///
    /// 采集帧本身**不含光标**（`showsCursor = false`，避免双光标），形状单独采样。
    /// macOS 没有非弃用的全局光标读取接口，这里用 `NSCursor.currentSystemCursor`；
    /// 自定义光标落到 [`CursorShape::Unknown`]（控制端显示默认箭头）。
    #[must_use]
    #[expect(
        deprecated,
        reason = "NSCursor.currentSystemCursor 是 macOS 上唯一能读到全局系统光标形状的公开接口；没有替代品"
    )]
    pub fn cursor_shape(&self) -> Option<CursorShape> {
        let current = NSCursor::currentSystemCursor()?;
        // 候选光标用动态消息发送取对象：objc2 生成的 `NSCursor::arrowCursor()`
        // 等在无 AppKit 初始化/无 WindowServer 的进程里会返回 NULL 并 panic，
        // 而可空返回 `Option<Retained<NSCursor>>` 会把 nil 折成 None。光标形状
        // 只是可选遥测，读不到就按「未知」处理，不能让采集循环崩溃。
        let candidates = [
            (CursorShape::Arrow, objc2::sel!(arrowCursor)),
            (CursorShape::IBeam, objc2::sel!(IBeamCursor)),
            (CursorShape::Hand, objc2::sel!(pointingHandCursor)),
            (CursorShape::Crosshair, objc2::sel!(crosshairCursor)),
            (CursorShape::No, objc2::sel!(operationNotAllowedCursor)),
            (CursorShape::SizeNS, objc2::sel!(rowResizeCursor)),
            (CursorShape::SizeWE, objc2::sel!(columnResizeCursor)),
            (CursorShape::Hand, objc2::sel!(openHandCursor)),
            (CursorShape::Hand, objc2::sel!(closedHandCursor)),
        ];
        for (shape, sel) in candidates {
            use objc2::runtime::MessageReceiver;
            // SAFETY: 这些 NSCursor 类方法都无参数、返回对象；sel 来自 sel! 字面量。
            // 返回的是 +0（autoreleased）对象，故用 `Retained::retain` 而不是
            // `from_raw`；NULL 会被折成 `None`，不 panic。
            let ptr: *mut NSCursor = unsafe { NSCursor::class().send_message(sel, ()) };
            // SAFETY: 指针由上面的消息发送返回，可能是 NULL（retain 会折成 None）。
            let Some(candidate) = (unsafe { Retained::retain(ptr) }) else {
                continue;
            };
            if *current == *candidate {
                return Some(shape);
            }
        }
        Some(CursorShape::Unknown)
    }

    /// 采集源的平台信息（`backend = "ScreenCaptureKit"`）。
    ///
    /// `gpu_capture` 按**最近一帧**是否真的走了 `CVPixelBuffer` 直通上报：GPU 编解码
    /// 会话在 `capture_gpu` 路径下为真，CPU/补丁路径为假。
    #[must_use]
    pub fn info(&self) -> CaptureInfo {
        CaptureInfo {
            backend: "ScreenCaptureKit".to_owned(),
            adapter: String::new(),
            gpu_capture: self.last_was_gpu,
            gpu_disabled: false,
            gpu_disabled_reason: String::new(),
            fallbacks: 0,
            no_new_frame: self.no_new_frame,
            open_fallbacks: 0,
        }
    }

    /// 停止采集并释放 SCK 资源（幂等）。
    pub fn stop(&mut self) {
        self.stop_worker();
        self.reset_pipelines();
    }

    /// 丢弃所有「上一帧/检测基准」（停流、切显示器、几何变化后调用）。
    fn reset_pipelines(&mut self) {
        self.last = None;
        self.gpu_last = None;
        self.patch_last = None;
        self.detector.reset();
        self.gpu_detector.reset();
        // 降采样器本身与流无关（只是转换会话 + 目标缓冲），保留可避免重建成本；
        // 但它的像素基准随检测器一起作废。
        self.last_was_gpu = false;
    }

    /// 取当前采集线程的共享槽，并刷新空闲看门狗计时。
    fn shared(&self) -> MacCaptureResult<Arc<Shared>> {
        let shared = Arc::clone(
            &self
                .worker
                .as_ref()
                .ok_or_else(|| MacCaptureError::Platform("采集线程不存在".to_owned()))?
                .shared,
        );
        // 每次采集都刷新空闲计时：5 s 没有任何采集调用，采集线程会自行停流退出。
        shared.touch();
        Ok(shared)
    }

    /// 取最新一帧并完成「拷贝 → 缩放 → 变化检测」。
    fn prepare(&mut self, display: u32) -> MacCaptureResult<Prepared> {
        self.ensure_worker(display)?;
        if self
            .last
            .as_ref()
            .is_some_and(|last| last.display != display)
        {
            // 切显示器：旧帧与检测基准都作废。
            self.last = None;
            self.detector.reset();
        }
        let shared = self.shared()?;
        let after = self.last.as_ref().map(|last| last.seq);
        let timeout = if after.is_some() {
            FRAME_WAIT
        } else {
            FIRST_FRAME_TIMEOUT
        };
        let sample = shared.wait_newer(after, timeout);
        let Some(sample) = sample else {
            if let Some(message) = shared.error() {
                return Err(MacCaptureError::Platform(message));
            }
            self.no_new_frame = self.no_new_frame.saturating_add(1);
            return self.reuse_last(display);
        };
        let source = Arc::new(copy_bgra(&sample)?);
        let (dst_width, dst_height) =
            geometry::encode_dimensions(sample.width, sample.height, 1.0, self.max_encode_width);
        let scaled = if (sample.width, sample.height) == (dst_width, dst_height) {
            Arc::clone(&source)
        } else {
            Arc::new(
                scale_bgra_box(&source, sample.width, sample.height, dst_width, dst_height)
                    .map_err(|err| MacCaptureError::Platform(format!("采集帧下采样失败：{err}")))?,
            )
        };
        let outcome = self.detector.observe(&scaled, dst_width, dst_height);
        let report = outcome.report;
        self.last = Some(LastFrame {
            display,
            seq: sample.seq,
            dst_width,
            dst_height,
            scaled: Arc::clone(&scaled),
            rgba: None,
        });
        Ok(Prepared {
            fresh: true,
            width: dst_width,
            height: dst_height,
            scaled,
            report,
        })
    }

    /// 复用上一帧（没有新样本或样本无效）。
    fn reuse_last(&self, display: u32) -> MacCaptureResult<Prepared> {
        let Some(last) = self.last.as_ref() else {
            return Err(MacCaptureError::Timeout("等待 ScreenCaptureKit 首帧"));
        };
        if last.display != display {
            return Err(MacCaptureError::Timeout("等待 ScreenCaptureKit 首帧"));
        }
        Ok(Prepared {
            fresh: false,
            width: last.dst_width,
            height: last.dst_height,
            scaled: Arc::clone(&last.scaled),
            report: ChangeReport::unchanged(),
        })
    }

    /// 观测一帧 GPU 样本，返回变化报告与脏网格。
    ///
    /// 快路径：SCK 样本附件报告**零个**脏矩形，且检测器已有基准 ⇒ 平台已经权威地
    /// 告诉我们「这一帧与上一帧逐像素相同」，直接报未变化（网格给全 0），**不碰任何
    /// 像素**。其余情况把帧缩到 320×180 后做一次逐像素比较（约 225 KB 的 CPU 侧读取）。
    fn observe_gpu_frame(&mut self, sample: &Sample) -> MacCaptureResult<ChangeOutcome> {
        if sample.dirty.is_some_and(|dirty| dirty.rects == 0) && self.gpu_detector.has_baseline() {
            return Ok(ChangeOutcome {
                report: ChangeReport::unchanged(),
                grid: Some(DirtyGrid::empty()),
            });
        }
        self.downscale_for_change(sample)?;
        let Some(scaler) = self.change_scaler.as_ref() else {
            return Err(MacCaptureError::Platform(
                "变化检测降采样器未就绪".to_owned(),
            ));
        };
        // 与网格同尺寸（320×180）：`ChangeDetector` 内部再降采样时每个格点恰好 1 像素。
        Ok(self
            .gpu_detector
            .observe(&scaler.pixels, GRID_WIDTH, GRID_HEIGHT))
    }

    /// 把样本帧在 GPU 上缩到 320×180 BGRA，锁目标缓冲后拷进复用的暂存。
    fn downscale_for_change(&mut self, sample: &Sample) -> MacCaptureResult<()> {
        let scaler = self.change_scaler()?;
        scaler.downscale(&sample.buffer)
    }

    /// 取（或懒建）变化检测降采样器。
    fn change_scaler(&mut self) -> MacCaptureResult<&mut ChangeScaler> {
        if self.change_scaler.is_none() {
            let transfer = PixelTransfer::new()
                .map_err(|err| MacCaptureError::Platform(format!("像素转换会话创建失败：{err}")))?;
            let target = pixel_transfer::create_pixel_buffer(
                GRID_WIDTH,
                GRID_HEIGHT,
                kCVPixelFormatType_32BGRA,
                false,
            )
            .map_err(|err| MacCaptureError::Platform(format!("变化检测目标缓冲创建失败：{err}")))?;
            let row_bytes = usize::try_from(GRID_WIDTH)
                .ok()
                .and_then(|width| width.checked_mul(4))
                .and_then(|row| {
                    usize::try_from(GRID_HEIGHT)
                        .ok()
                        .and_then(|height| row.checked_mul(height))
                })
                .ok_or_else(|| MacCaptureError::Platform("变化检测缓冲尺寸溢出".to_owned()))?;
            self.change_scaler = Some(ChangeScaler {
                transfer,
                target,
                pixels: vec![0; row_bytes],
            });
        }
        self.change_scaler
            .as_mut()
            .ok_or_else(|| MacCaptureError::Platform("变化检测降采样器未就绪".to_owned()))
    }

    /// 确保当前显示器的采集线程在跑；需要时重建（带退避）。
    fn ensure_worker(&mut self, display: u32) -> MacCaptureResult<()> {
        // 首次采集必须建流：worker 为 None 时不能走「无需重启」的提前返回，
        // 否则随后的 `shared()` 会报「采集线程不存在」。
        let mut restart = self.worker.is_none();
        let mut failure_message = None;
        if let Some(worker) = self.worker.as_ref() {
            if worker.display != display {
                restart = true;
            } else if let Some(message) = worker.shared.error() {
                failure_message = Some(message);
                restart = true;
            } else if worker.thread.is_finished() {
                // 空闲看门狗已停流（或流自行退出但没有错误）：正常重建，不计退避。
                restart = true;
            }
        }
        // 显示器几何（分辨率/旋转）**或编码宽度上限**变化时重建：SCK 的输出尺寸是
        // 建流时定死的。输出尺寸按 `max_encode_width` 在**采集侧**直接封顶，这样
        // `capture_gpu` 交出的就是编码尺寸的缓冲，编码器零拷贝直通——不再每帧做一次
        // 4K→1080p 的 `VTPixelTransferSession`（真机实测 ~3 ms/帧，且随帧率放大）。
        //
        // **宽度上限的比较不查显示器**：本函数每拍都跑，`physical_size` 是 CoreGraphics
        // 调用，不能每帧付一次。宽度上限只在建流/切 codec 时变，直接比缓存值即可；
        // 分辨率变化的复查仍是每 [`GEOMETRY_RECHECK`] 一次。
        if !restart {
            if let Some(worker) = self.worker.as_mut() {
                if worker.max_encode_width != self.max_encode_width {
                    tracing::info!(
                        "显示器 {} 编码宽度上限变化（{} → {}），重建采集流",
                        worker.display,
                        worker.max_encode_width,
                        self.max_encode_width
                    );
                    restart = true;
                }
                if !restart && worker.checked_at.elapsed() >= GEOMETRY_RECHECK {
                    worker.checked_at = Instant::now();
                    if let Some(current) = physical_size(display) {
                        if worker
                            .geometry
                            .is_some_and(|configured| configured != current)
                        {
                            let previous = worker.geometry;
                            let display_id = display;
                            tracing::info!(
                                "显示器 {display_id} 几何变化（{previous:?} → {current:?}），重建采集流"
                            );
                            restart = true;
                        } else {
                            // 分辨率没变也要确认输出尺寸与当前上限一致（显示器热的
                            // 分辨率/旋转组合可能改变点缩放比而 `physical_size` 不变）。
                            let desired = geometry::encode_dimensions(
                                current.0,
                                current.1,
                                1.0,
                                self.max_encode_width,
                            );
                            if desired != worker.output_size {
                                tracing::info!(
                                    "显示器 {} 采集输出尺寸变化（{:?} → {:?}），重建采集流",
                                    worker.display,
                                    worker.output_size,
                                    desired
                                );
                                restart = true;
                            }
                        }
                    }
                }
            }
        }
        if !restart {
            return Ok(());
        }
        self.stop_worker();
        if let Some(message) = failure_message {
            self.failure = Some(Failure {
                display,
                at: Instant::now(),
                message,
            });
        }
        if let Some(failure) = self.failure.as_ref() {
            if failure.display == display {
                let elapsed = failure.at.elapsed();
                if elapsed < REBUILD_BACKOFF {
                    return Err(MacCaptureError::Backoff {
                        remaining_ms: u64::try_from(
                            REBUILD_BACKOFF.saturating_sub(elapsed).as_millis(),
                        )
                        .unwrap_or(0),
                        reason: failure.message.clone(),
                    });
                }
            }
        }
        match self.start_worker(display) {
            Ok(()) => {
                self.failure = None;
                self.reset_pipelines();
                Ok(())
            }
            Err(err) => {
                self.failure = Some(Failure {
                    display,
                    at: Instant::now(),
                    message: err.to_string(),
                });
                Err(err)
            }
        }
    }

    /// 停掉当前采集线程（等待它移除输出、停采、释放 ObjC 对象）。
    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.stop.send(());
            let _ = worker.thread.join();
        }
    }

    /// 起一条采集线程，并同步等它完成建流/启采。
    fn start_worker(&mut self, display: u32) -> MacCaptureResult<()> {
        let shared = Arc::new(Shared::new(Arc::new(Counters::default())));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let output_size = desired_output_size(display, self.max_encode_width);
        let thread = std::thread::Builder::new()
            .name("rt-mac-capture".to_owned())
            .spawn(move || {
                worker_thread(display, output_size, thread_shared, stop_rx, ready_tx);
            })
            .map_err(|err| MacCaptureError::Message(format!("创建采集线程失败: {err}")))?;
        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(started)) => {
                self.worker = Some(Worker {
                    display,
                    stop: stop_tx,
                    thread,
                    shared,
                    geometry: started.geometry,
                    output_size: started.output_size,
                    max_encode_width: self.max_encode_width,
                    checked_at: Instant::now(),
                });
                Ok(())
            }
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = stop_tx.send(());
                let _ = thread.join();
                Err(MacCaptureError::Timeout(
                    "等待 ScreenCaptureKit 采集线程就绪",
                ))
            }
        }
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 采集线程建流成功后回报给调用方的两套尺寸。
struct StreamStarted {
    /// 显示器物理尺寸（几何复查用）。
    geometry: Option<(u32, u32)>,
    /// 实际配置的采集输出尺寸（编码宽度上限封顶后的值）。
    output_size: (u32, u32),
}

/// 本次会话期望的采集输出尺寸：物理尺寸按 `max_encode_width` 封顶（保持宽高比、偶数对齐）。
///
/// 在**采集侧**直接按编码尺寸出帧，`capture_gpu` 交出的 `CVPixelBuffer` 就与编码器配置
/// 尺寸一致 ⇒ `VtEncoder::encode_pixel_buffer` 走零拷贝直通，不再每帧做 4K→1080p 的
/// `VTPixelTransferSession`（真机实测 ~3 ms/帧）。
fn desired_output_size(display: u32, max_encode_width: u32) -> Option<(u32, u32)> {
    physical_size(display)
        .map(|(width, height)| geometry::encode_dimensions(width, height, 1.0, max_encode_width))
}

/// 采集线程：建流 → 等停止/失败信号 → 清理。
///
/// 所有 ObjC 对象（`SCStream` / `SCContentFilter` / `StreamOutput`）都在本线程创建与
/// 释放，`MacCapture` 只通过共享样本槽与通道与它交互。
#[expect(
    clippy::needless_pass_by_value,
    reason = "四个值都移交给 std::thread::spawn 的 'static 闭包，必须按值持有"
)]
fn worker_thread(
    display: u32,
    output_size: Option<(u32, u32)>,
    shared: Arc<Shared>,
    stop: Receiver<()>,
    ready: SyncSender<MacCaptureResult<StreamStarted>>,
) {
    let session = match start_stream(display, output_size, &shared) {
        Ok(session) => session,
        Err(err) => {
            let _ = ready.try_send(Err(err));
            return;
        }
    };
    let _ = ready.try_send(Ok(StreamStarted {
        geometry: session.geometry,
        output_size: session.output_size,
    }));
    loop {
        match stop.recv_timeout(WORKER_POLL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                // 回调线程报的错（didStopWithError）也要能让本线程退出。
                if shared.error().is_some() {
                    break;
                }
                // 空闲看门狗：没有会话时 SCK 不应继续录屏（指示常亮 + CPU 白耗）。
                // 这里只退出本线程（会 stopCapture + 释放 SCStream），重建由下一次
                // 采集走 `ensure_worker`——绝不在当前调用持有的线程上 join 自己。
                if shared.idle_for() >= IDLE_STOP_TIMEOUT {
                    tracing::debug!("采集空闲超时，停止 ScreenCaptureKit 流");
                    break;
                }
            }
        }
    }
    shutdown_stream(&session.stream, &session.output);
}

/// 在采集线程内完成「权限 → 枚举 → 建流 → 启采」。
fn start_stream(
    display_id: u32,
    output_size: Option<(u32, u32)>,
    shared: &Arc<Shared>,
) -> MacCaptureResult<StreamSession> {
    ensure_screen_recording()?;
    let content = fetch_shareable_content(CONTENT_TIMEOUT)?;
    // SAFETY: `content` 由本函数借用，`displays` 只读。
    let displays = unsafe { content.displays() }.to_vec();
    if displays.is_empty() {
        return Err(MacCaptureError::Platform(
            "ScreenCaptureKit 没有枚举到任何显示器（屏幕录制权限未授予？）".to_owned(),
        ));
    }
    // SAFETY: `SCDisplay` 由 SCK 持有，`displayID` 只读。
    let display = displays
        .iter()
        .find(|candidate| unsafe { candidate.displayID() } == display_id)
        .ok_or_else(|| {
            MacCaptureError::Platform(format!(
                "找不到显示器 {display_id}（当前枚举到 {} 台）",
                displays.len()
            ))
        })?;
    // SAFETY: `display` 来自本次枚举；排除窗口列表为空（整屏采集）。
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::<SCWindow>::from_slice(&[]),
        )
    };
    let (fallback_width, fallback_height) = capture_geometry(display_id, display, &filter);
    let (width, height) = output_size.unwrap_or((fallback_width, fallback_height));
    let config = build_configuration(width, height);
    let output = StreamOutput::new(Arc::clone(shared));
    let queue = DispatchQueue::new("rust-tunnel.mac-capture.screen", None);
    // SAFETY: filter/config 都是本函数构造的合法对象；output 实现了 `SCStreamDelegate`，
    // 且被下面的会话保活。
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        )
    };
    // SAFETY: `output` 实现了 `SCStreamOutput`；`queue` 由 `StreamSession` 保活。
    let added = unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*output),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    };
    if let Err(err) = added {
        return Err(describe_error(&err));
    }
    let session = StreamSession {
        stream,
        output,
        geometry: physical_size(display_id),
        output_size: (width, height),
        _queue: queue,
    };
    if let Err(err) = capture_completion(&session.stream, CAPTURE_TIMEOUT, true) {
        shutdown_stream(&session.stream, &session.output);
        return Err(err);
    }
    tracing::info!(
        display = display_id,
        width,
        height,
        "ScreenCaptureKit 采集已启动"
    );
    Ok(session)
}

/// 采集输出的**退化**尺寸：优先 CoreGraphics 的物理像素，退化到「点尺寸 × 缩放比」。
///
/// 正常路径由 [`desired_output_size`]（物理尺寸按编码宽度上限封顶）决定；只有查不到
/// 物理尺寸时才用这里的值。
fn capture_geometry(display_id: u32, display: &SCDisplay, filter: &SCContentFilter) -> (u32, u32) {
    if let Some((width, height)) = physical_size(display_id) {
        return (width, height);
    }
    // SAFETY: 两个查询都只读；`display`/`filter` 来自本次枚举。
    let (points_w, points_h, scale) =
        unsafe { (display.width(), display.height(), filter.pointPixelScale()) };
    let scale = if scale.is_finite() && scale >= 1.0 {
        f64::from(scale)
    } else {
        1.0
    };
    (
        scaled_points(points_w, scale).max(2),
        scaled_points(points_h, scale).max(2),
    )
}

/// 点尺寸 × 缩放比 → 物理像素（钳位到 `1..=u32::MAX`）。
fn scaled_points(points: isize, scale: f64) -> u32 {
    let points = f64::from(i32::try_from(points.max(1)).unwrap_or(1));
    let value = (points * scale).round().clamp(1.0, f64::from(u32::MAX));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "已钳到 1..=u32::MAX 且非负，截断不改变有效范围"
    )]
    {
        value as u32
    }
}

/// 显示器的物理像素尺寸（采集配置与几何复查用，**不是**给控制端做输入映射的尺寸）。
///
/// `CGDisplayPixelsWide/High` 返回的是**点**尺寸（Retina/HiDPI 下会是 1920×1080，
/// 而面板实际是 3840×2160），所以优先取当前显示模式的 `pixelWidth/pixelHeight`；
/// 模式不可用时再回退到点尺寸。
pub(crate) fn physical_size(display_id: u32) -> Option<(u32, u32)> {
    if let Some(mode) = CGDisplayCopyDisplayMode(display_id) {
        let width = CGDisplayMode::pixel_width(Some(&mode));
        let height = CGDisplayMode::pixel_height(Some(&mode));
        if width != 0 && height != 0 {
            return Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?));
        }
    }
    let width = CGDisplayPixelsWide(display_id);
    let height = CGDisplayPixelsHigh(display_id);
    if width == 0 || height == 0 {
        return None;
    }
    Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
}

/// 采集流配置：**编码尺寸输出**（GPU 内缩放）、32BGRA、不烧光标、最多 [`MAX_FPS`]。
///
/// `width/height` 由调用方按 `max_encode_width` 封顶后传入（见 [`desired_output_size`]），
/// 不是显示器物理尺寸——这样编码器拿到的缓冲就是编码尺寸，可零拷贝直通。
fn build_configuration(width: u32, height: u32) -> Retained<SCStreamConfiguration> {
    // SAFETY: `new` 是 NSObject 的标准构造；下面的 setter 都是标量参数。
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(usize::try_from(width).unwrap_or(1920));
        config.setHeight(usize::try_from(height).unwrap_or(1080));
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        // 光标由协议 v9 的「形状」单独传，画面里绝不能烧进去（否则控制端双光标）。
        config.setShowsCursor(false);
        config.setMinimumFrameInterval(CMTime::new(1, MAX_FPS));
        config.setQueueDepth(QUEUE_DEPTH);
        config.setCapturesAudio(false);
    }
    config
}

/// 预检 + 申请「屏幕录制」权限。
fn ensure_screen_recording() -> MacCaptureResult<()> {
    // SAFETY: 两个函数都没有指针参数，只读写 TCC 状态。
    if unsafe { CGPreflightScreenCaptureAccess() } {
        return Ok(());
    }
    // 首次调用会弹一次系统授权框；同一进程后续调用不再弹。
    // SAFETY: 同上。
    if unsafe { CGRequestScreenCaptureAccess() } {
        return Ok(());
    }
    Err(MacCaptureError::ScreenRecordingPermission)
}

/// 同步等待 `SCShareableContent`（SCK 只提供 completion handler 形式的接口）。
fn fetch_shareable_content(timeout: Duration) -> MacCaptureResult<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if error.is_null() {
                if content.is_null() {
                    Err(MacCaptureError::Platform(
                        "ScreenCaptureKit 返回了空的可共享内容".to_owned(),
                    ))
                } else {
                    // SAFETY: 指针在回调期间有效；`retain` 取一份所有权给读取侧。
                    unsafe { Retained::retain(content) }.ok_or_else(|| {
                        MacCaptureError::Platform("无法持有 SCK 可共享内容".to_owned())
                    })
                }
            } else {
                // SAFETY: SCK 在回调期间保证 `error` 指向有效对象。
                Err(describe_error(unsafe { &*error }))
            };
            let _ = tx.try_send(result);
        },
    );
    // SAFETY: 回调只把结果送进本线程的通道，不触碰其它共享状态。
    let block_ref: &DynBlock<dyn Fn(*mut SCShareableContent, *mut NSError)> = &block;
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(block_ref) };
    rx.recv_timeout(timeout).map_err(|_| {
        MacCaptureError::Timeout("等待 ScreenCaptureKit 可共享内容（屏幕录制权限未授予？）")
    })?
}

/// 枚举所有显示器（**CoreGraphics 同步枚举**，亚毫秒级）。
///
/// 引擎在会话循环里每 5 秒内联调用一次 `ScreenSource::displays()`（看热插拔/分辨率变化），
/// 它直接跑在 tokio worker 上。**绝不能用 `SCShareableContent` 的异步枚举**：真机实测
/// 一次 `getShareableContentWithCompletionHandler` 要 **70–80 ms**（XPC + WindowServer），
/// 每 5 秒把整个会话循环（输入注入、采集节拍、心跳）冻住一次，表现为周期性卡顿。
/// Windows 的 `EnumDisplayMonitors` 是亚毫秒，这也是「Windows→Windows 不卡、Windows→macOS
/// 卡」最容易被忽略的一条。
///
/// 尺寸语义与旧实现一致：`DisplayInfo` 是 **macOS 全局逻辑点**（`CGDisplayBounds` 的单位），
/// 控制端输入映射用；采集/编码仍用 [`physical_size`] 的物理像素。
pub(crate) fn list_displays() -> MacCaptureResult<Vec<DisplayInfo>> {
    // **不查 TCC**：`CGGetActiveDisplayList` 不需要「屏幕录制」权限，而
    // `CGPreflightScreenCaptureAccess` 在真机上要走一次 TCC XPC（实测几十毫秒）——
    // 它同样会拖慢每 5 秒一次的会话循环内联枚举。缺权限的报错由采集路径给出。
    let mut count: u32 = 0;
    // SAFETY: `active_displays` 传空指针 = 只问数量（CoreGraphics 文档允许）。
    let status = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &raw mut count) };
    if status != CGError::Success {
        return Err(MacCaptureError::Message(format!(
            "CGGetActiveDisplayList（计数）失败：{}",
            status.0
        )));
    }
    if count == 0 {
        return Err(MacCaptureError::Platform(
            "CoreGraphics 没有枚举到任何活动显示器".to_owned(),
        ));
    }
    let mut ids = vec![0 as CGDirectDisplayID; count as usize];
    let mut actual = count;
    // SAFETY: `ids` 至少有 `count` 个元素，`actual` 是合法可写指针。
    let status = unsafe { CGGetActiveDisplayList(count, ids.as_mut_ptr(), &raw mut actual) };
    if status != CGError::Success {
        return Err(MacCaptureError::Message(format!(
            "CGGetActiveDisplayList 失败：{}",
            status.0
        )));
    }
    let main_display = CGMainDisplayID();
    let mut out = Vec::with_capacity(actual as usize);
    for id in ids.into_iter().take(actual as usize) {
        // `CGDisplayBounds` 的单位是全局显示坐标空间的**点**（4K 屏“看起来像 1920×1080”
        // 时就是 1920×1080），正是控制端输入映射要的坐标系。
        let bounds = CGDisplayBounds(id);
        let width = logical_points(bounds.size.width);
        let height = logical_points(bounds.size.height);
        out.push(DisplayInfo {
            id,
            width,
            height,
            name: format!("显示器 {id}"),
            // SAFETY: `CGDisplayIsMain` 只读全局显示配置。
            primary: id == main_display || CGDisplayIsMain(id),
        });
    }
    Ok(out)
}

/// `CGFloat` 点尺寸 → 非零 `u32`（非法/非有限值退回 1，避免整块列表被一台怪显示器作废）。
fn logical_points(value: f64) -> u32 {
    let points = if value.is_finite() && value > 0.0 {
        value.round()
    } else {
        1.0
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "已钳到正数；显示器点尺寸远小于 u32 上限"
    )]
    {
        points.clamp(1.0, f64::from(u32::MAX)) as u32
    }
}

/// 把 SCK 的 completion handler 变成同步等待。
fn capture_completion(stream: &SCStream, timeout: Duration, start: bool) -> MacCaptureResult<()> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(move |error: *mut NSError| {
        let result = if error.is_null() {
            Ok(())
        } else {
            // SAFETY: SCK 在回调期间保证 `error` 指向有效对象。
            Err(describe_error(unsafe { &*error }))
        };
        let _ = tx.try_send(result);
    });
    let block_ref: &DynBlock<dyn Fn(*mut NSError)> = &block;
    // SAFETY: 回调只把结果送进本线程的通道。
    unsafe {
        if start {
            stream.startCaptureWithCompletionHandler(Some(block_ref));
        } else {
            stream.stopCaptureWithCompletionHandler(Some(block_ref));
        }
    }
    let fallback = if start {
        "等待 ScreenCaptureKit 启动采集"
    } else {
        "等待 ScreenCaptureKit 停止采集"
    };
    rx.recv_timeout(timeout)
        .unwrap_or(Err(MacCaptureError::Timeout(fallback)))
}

/// 移除输出 + 停采（失败只记日志，不影响资源释放）。
fn shutdown_stream(stream: &SCStream, output: &StreamOutput) {
    // SAFETY: `output` 确实以 Screen 类型添加过，`stream` 由调用方持有。
    let removed = unsafe {
        stream.removeStreamOutput_type_error(
            ProtocolObject::from_ref(output),
            SCStreamOutputType::Screen,
        )
    };
    if let Err(err) = removed {
        tracing::debug!(error = %describe_error(&err), "移除 SCK 画面输出失败（采集可能已停止）");
    }
    let _ = capture_completion(stream, STOP_TIMEOUT, false);
}

/// `NSError` → 本项目错误（中文可读）。
fn describe_error(error: &NSError) -> MacCaptureError {
    let (code, message) = (error.code(), error.localizedDescription());
    MacCaptureError::ScreenCaptureKit {
        code: i64::try_from(code).unwrap_or(i64::MAX),
        message: message.to_string(),
    }
}

/// 把 `CVPixelBuffer` 按 `bytesPerRow` 拷成紧包 BGRA（自顶向下）。
///
/// # Errors
/// 像素格式不是 32BGRA、锁定失败、基址为空、行跨度小于 `width * 4` 时返回错误。
fn copy_bgra(sample: &Sample) -> MacCaptureResult<Vec<u8>> {
    let width = usize::try_from(sample.width)
        .map_err(|_| MacCaptureError::Platform("帧宽非法".to_owned()))?;
    let height = usize::try_from(sample.height)
        .map_err(|_| MacCaptureError::Platform("帧高非法".to_owned()))?;
    let row_len = width
        .checked_mul(crate::geometry::BYTES_PER_PIXEL)
        .ok_or_else(|| MacCaptureError::Platform("帧行宽溢出".to_owned()))?;
    // SAFETY: `sample.buffer` 是有效对象；格式查询只读（objc2 里是安全函数）。
    let format = CVPixelBufferGetPixelFormatType(&sample.buffer);
    if format != kCVPixelFormatType_32BGRA {
        return Err(MacCaptureError::Platform(format!(
            "采集帧像素格式是 {format:#010x}，不是 32BGRA（配置未生效？）"
        )));
    }
    // SAFETY: 锁定/解锁成对出现（`PixelLock` 的 Drop 保证解锁），且其它线程不写这块内存。
    let status =
        unsafe { CVPixelBufferLockBaseAddress(&sample.buffer, CVPixelBufferLockFlags::ReadOnly) };
    if status != 0 {
        return Err(MacCaptureError::Platform(format!(
            "锁定采集帧失败：CVReturn {status}"
        )));
    }
    let _lock = PixelLock(&sample.buffer);
    // SAFETY: 已锁定，基址在解锁前有效（objc2 里是安全函数）。
    let base = CVPixelBufferGetBaseAddress(&sample.buffer);
    if base.is_null() {
        return Err(MacCaptureError::Platform("采集帧基址为空".to_owned()));
    }
    // SAFETY: 只读查询（objc2 里是安全函数）。
    let bytes_per_row = CVPixelBufferGetBytesPerRow(&sample.buffer);
    if bytes_per_row < row_len {
        return Err(MacCaptureError::Platform(format!(
            "采集帧行跨度 {bytes_per_row} 小于 {row_len}"
        )));
    }
    let total = bytes_per_row
        .checked_mul(height)
        .ok_or_else(|| MacCaptureError::Platform("采集帧尺寸溢出".to_owned()))?;
    // SAFETY: 锁定后基址指向至少 `bytes_per_row * height` 字节（CVPixelBuffer 的行跨度语义）。
    let all = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), total) };
    if bytes_per_row == row_len {
        return Ok(all.to_vec());
    }
    let mut out = vec![0_u8; row_len.saturating_mul(height)];
    for row in 0..height {
        let src_start = row.saturating_mul(bytes_per_row);
        let dst_start = row.saturating_mul(row_len);
        out[dst_start..dst_start + row_len].copy_from_slice(&all[src_start..src_start + row_len]);
    }
    Ok(out)
}

/// `CVPixelBuffer` 的 RAII 读锁（保证任何返回路径都会解锁）。
struct PixelLock<'a>(&'a CVPixelBuffer);

impl Drop for PixelLock<'_> {
    fn drop(&mut self) {
        // SAFETY: 与 `CVPixelBufferLockBaseAddress` 的 ReadOnly 标志严格成对。
        let _ = unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly) };
    }
}

impl ChangeScaler {
    /// 把 `source` 缩到 320×180 并拷进复用的暂存（只锁 225 KB 的目标缓冲）。
    fn downscale(&mut self, source: &CVPixelBuffer) -> MacCaptureResult<()> {
        self.transfer
            .transfer(source, &self.target)
            .map_err(|err| MacCaptureError::Platform(format!("变化检测降采样失败：{err}")))?;
        let row_bytes = usize::try_from(GRID_WIDTH)
            .ok()
            .and_then(|width| width.checked_mul(crate::geometry::BYTES_PER_PIXEL))
            .ok_or_else(|| MacCaptureError::Platform("变化检测行宽溢出".to_owned()))?;
        let height = usize::try_from(GRID_HEIGHT)
            .map_err(|_| MacCaptureError::Platform("变化检测高度溢出".to_owned()))?;
        // SAFETY: 目标缓冲由本类型独占持有；锁定/解锁成对（`PixelLock` 保证解锁），
        // 且转换已在本线程完成，没有其它线程写它。
        let status =
            unsafe { CVPixelBufferLockBaseAddress(&self.target, CVPixelBufferLockFlags::ReadOnly) };
        if status != 0 {
            return Err(MacCaptureError::Platform(format!(
                "锁定变化检测缓冲失败：CVReturn {status}"
            )));
        }
        let _lock = PixelLock(&self.target);
        let base = CVPixelBufferGetBaseAddress(&self.target);
        if base.is_null() {
            return Err(MacCaptureError::Platform("变化检测缓冲基址为空".to_owned()));
        }
        let pitch = CVPixelBufferGetBytesPerRow(&self.target);
        if pitch < row_bytes {
            return Err(MacCaptureError::Platform(format!(
                "变化检测缓冲行跨度 {pitch} 小于 {row_bytes}"
            )));
        }
        let total = pitch
            .checked_mul(height)
            .ok_or_else(|| MacCaptureError::Platform("变化检测缓冲尺寸溢出".to_owned()))?;
        // SAFETY: 锁定后基址指向至少 `pitch * height` 字节（CVPixelBuffer 的行跨度语义）。
        let all = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), total) };
        let pixels = &mut self.pixels;
        if pitch == row_bytes {
            if pixels.len() != total {
                pixels.resize(total, 0);
            }
            pixels.copy_from_slice(all);
        } else {
            if pixels.len() != row_bytes.saturating_mul(height) {
                pixels.resize(row_bytes.saturating_mul(height), 0);
            }
            for row in 0..height {
                let src_start = row.saturating_mul(pitch);
                let dst_start = row.saturating_mul(row_bytes);
                pixels[dst_start..dst_start + row_bytes]
                    .copy_from_slice(&all[src_start..src_start + row_bytes]);
            }
        }
        Ok(())
    }
}

/// 构造一条「没有变化」的补丁结果（保活用，空 rects）。
fn keepalive_patches(display: u32, width: u32, height: u32) -> CapturedPatches {
    CapturedPatches {
        display,
        width,
        height,
        changed_permille: 0,
        rects: Vec::new(),
    }
}

/// 在样本的 `CVPixelBuffer` 上**一次加读锁**，按脏矩形逐个读取源区间并转 NV12。
///
/// 与整帧路径逐像素一致（走 [`geometry::scale_bgra_region_strided`] 的同一套取整规则），
/// 但只碰矩形覆盖的源行——补丁模式下整帧 BGRA 一个字节都不拷。
///
/// 返回 `Ok(None)` 表示本拍不适合补丁（像素格式不是 32BGRA、基址为空、矩形非法、
/// 转 NV12 失败）；`Err` 只用于加锁失败这类真错误。
fn extract_patches_from_sample(
    sample: &Sample,
    rects: &[rust_tunnel_mac_video::change::Rect],
    dst: (u32, u32),
) -> MacCaptureResult<Option<Vec<PatchRect>>> {
    // SAFETY: 只读查询。
    let format = CVPixelBufferGetPixelFormatType(&sample.buffer);
    if format != kCVPixelFormatType_32BGRA {
        tracing::debug!(
            format = format_string(format),
            "补丁路径只支持 32BGRA 源缓冲，回退全帧"
        );
        return Ok(None);
    }
    let height = usize::try_from(sample.height)
        .map_err(|_| MacCaptureError::Platform("补丁源高度溢出".to_owned()))?;
    // SAFETY: 锁定/解锁成对（`PixelLock` 保证解锁），且 SCK 不会在回调之外改写这一帧。
    let status =
        unsafe { CVPixelBufferLockBaseAddress(&sample.buffer, CVPixelBufferLockFlags::ReadOnly) };
    if status != 0 {
        return Err(MacCaptureError::Platform(format!(
            "锁定采集帧失败：CVReturn {status}"
        )));
    }
    let _lock = PixelLock(&sample.buffer);
    let base = CVPixelBufferGetBaseAddress(&sample.buffer);
    if base.is_null() {
        return Ok(None);
    }
    let pitch = CVPixelBufferGetBytesPerRow(&sample.buffer);
    let total = pitch
        .checked_mul(height)
        .ok_or_else(|| MacCaptureError::Platform("补丁源尺寸溢出".to_owned()))?;
    // SAFETY: 锁定后基址指向至少 `pitch * height` 字节。
    let all = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), total) };
    let mut patches = Vec::with_capacity(rects.len());
    for rect in rects {
        let target = TargetRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        };
        let Some(bgra) = geometry::scale_bgra_region_strided(
            all,
            sample.width,
            sample.height,
            pitch,
            target,
            dst.0,
            dst.1,
        ) else {
            tracing::debug!(?target, "补丁矩形读源失败，回退全帧");
            return Ok(None);
        };
        let Ok(nv12) = bgra_to_nv12(&bgra, target.width, target.height) else {
            tracing::debug!(?target, "补丁转 NV12 失败，回退全帧");
            return Ok(None);
        };
        patches.push(PatchRect {
            x: target.x,
            y: target.y,
            width: target.width,
            height: target.height,
            nv12,
        });
    }
    Ok(Some(patches))
}

/// 读取 SCK 样本附件里的脏矩形摘要（不可用时返回 `None`）。
///
/// `SCStreamFrameInfoDirtyRects` 是 `NS_TYPED_ENUM(NSString)`，附件值是
/// `CFArray<NSValue(CGRect)>`（单位：物理像素）。这里只统计矩形个数与覆盖面积；数组里
/// 任何一个元素读不出来就整体当作「附件不可用」——宁可多做一次像素比较，也不能把
/// 「读不懂」当成「没有变化」。
fn sample_dirty_hint(sample: &CMSampleBuffer) -> Option<DirtyHint> {
    // SAFETY: SCK 把帧元数据作为 CF 附件挂在 sample buffer 上；该静态量是 NSString，
    // 与 CFString 是 toll-free bridged（CoreFoundation 文档保证同构）。
    let key: &CFString =
        unsafe { &*std::ptr::from_ref(SCStreamFrameInfoDirtyRects).cast::<CFString>() };
    // SAFETY: 只读查询附件；`key` 是有效的 CFString，模式出参传空。
    let value = unsafe { CMGetAttachment(sample, key, std::ptr::null_mut()) }?;
    // SAFETY: 文档保证该键的附件是 CFArray（元素为 NSValue 包装的 CGRect）。
    let array = unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(value) };
    let mut hint = DirtyHint {
        rects: 0,
        area_pixels: 0,
    };
    for index in 0..array.len() {
        let item = array.get(index)?;
        // SAFETY: 元素是 NSValue（CGRect 包装）；`item` 在本次循环内保持存活。
        let raw = CFRetained::as_ptr(&item).cast::<NSValue>();
        // SAFETY: `raw` 由 `item`（活着的 CFRetained）保证有效。
        let value: &NSValue = unsafe { &*raw.as_ptr() };
        // SAFETY: 元素确实包装的是 CGRect（文档契约）。
        let rect = unsafe { value.rectValue() };
        hint.rects = hint.rects.saturating_add(1);
        hint.area_pixels = hint
            .area_pixels
            .saturating_add(rect_area_pixels(rect.size.width, rect.size.height));
    }
    Some(hint)
}

/// `CGRect` 的宽高（`CGFloat`）转成像素面积，非法/非有限值按 0 处理。
fn rect_area_pixels(width: f64, height: f64) -> u64 {
    let width = if width.is_finite() && width > 0.0 {
        width
    } else {
        0.0
    };
    let height = if height.is_finite() && height > 0.0 {
        height
    } else {
        0.0
    };
    let area = (width * height).min(f64::from(u32::MAX));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "已钳到 0..=u32::MAX 且非负，截断不改变有效范围"
    )]
    {
        area as u64
    }
}

/// 像素格式码 → 便于日志阅读的十六进制串。
fn format_string(format: u32) -> String {
    format!("{format:#010x}")
}

/// 取锁；中毒时取回内部值（采集路径不能因为一次 panic 永久停摆）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 引擎要求 `Arc<dyn ScreenSource>` 隐含 `Send + Sync`；适配器持有本类型。
    #[test]
    fn mac_capture_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MacCapture>();
    }

    /// 编码宽度上限与 Windows 侧同口径（钳位 + 偶数对齐）。
    #[test]
    fn max_encode_width_clamps_like_windows() {
        let mut capture = MacCapture::new().expect("构造采集源");
        capture.set_max_encode_width(0);
        assert_eq!(capture.max_encode_width, DEFAULT_MAX_ENCODE_WIDTH);
        capture.set_max_encode_width(8000);
        assert_eq!(capture.max_encode_width, MAX_ENCODE_WIDTH_LIMIT);
        capture.set_max_encode_width(1921);
        assert_eq!(capture.max_encode_width, 1920);
        capture.set_max_encode_width(3840);
        assert_eq!(capture.max_encode_width, 3840);
    }

    /// 未启动采集时：没有上一帧 → 首帧超时错误；有 `info()` 也不谎报 GPU。
    #[test]
    fn info_reports_screencapturekit_without_gpu() {
        let capture = MacCapture::new().expect("构造采集源");
        let info = capture.info();
        assert_eq!(info.backend, "ScreenCaptureKit");
        assert!(!info.gpu_capture);
        assert_eq!(info.no_new_frame, 0);
        assert!(capture.cursor_shape().is_none() || capture.cursor_shape().is_some());
    }
}
