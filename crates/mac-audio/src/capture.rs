//! 被控端采集：ScreenCaptureKit **系统声音** → 交织 `f32` → Opus 20 ms 帧。
//!
//! 这一层只负责「系统声音 → 编码包」，不做会话协议。`client-gui` 用一个薄适配器把它
//! 包成 `rust_tunnel_remote::AudioSource`，从而保持本 crate 不依赖引擎。
//!
//! # 数据通路
//!
//! `SCStream` 在 `SCStreamConfiguration.capturesAudio = true` 时把系统混音送进
//! `SCStreamOutput.stream:didOutputSampleBuffer:ofType:` 回调（我们自己的串行
//! dispatch 队列）。回调把 `CMSampleBuffer` 里的 `AudioBufferList` 归一成交织立体声、
//! 攒够 20 ms 就 `try_send` 给 [`LoopbackEncoder::read_packet`]，后者做 Opus 编码。
//!
//! # 延迟口径
//!
//! 回调攒够 20 ms 才交出一帧，所以采集侧引入的额外延迟不超过一帧；`read_packet`
//! 用 5 ms 的轮询粒度等待，`stop` 置位后最迟 5–200 ms 返回 `None`。
//!
//! # 权限
//!
//! ScreenCaptureKit 需要「屏幕录制」TCC 权限。`start` 先 `CGPreflightScreenCaptureAccess`
//! 预检，缺失时调 `CGRequestScreenCaptureAccess` 弹一次系统授权框，仍然拿不到就返回
//! [`MacAudioError::ScreenRecordingPermission`]（中文提示告诉用户去哪里勾选）。

use crate::clock;
use crate::error::{MacAudioError, MacAudioResult};
use crate::format::{CHANNELS, FRAME_FLOATS, OPUS_BITRATE_BPS, SAMPLE_BYTES, SAMPLE_RATE};
use crate::opus::{Encoder, MAX_PACKET_BYTES};
use crate::pcm;

use block2::{DynBlock, RcBlock};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatLinearPCM, AudioBuffer, AudioBufferList,
    AudioStreamBasicDescription,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// 采集帧队列深度（20 ms 帧）。50 帧 ≈ 1 秒，远大于编码消费速度；
/// 满时丢最新一帧，绝不阻塞 SCK 的回调线程。
const FRAME_QUEUE_DEPTH: usize = 50;

/// 等待 SCK 枚举可共享内容的上限（首次可能触发屏幕录制授权对话框）。
const CONTENT_TIMEOUT: Duration = Duration::from_secs(10);
/// 等待 `startCapture` / `stopCapture` 完成的上限。
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// 等 `stopCapture` 完成的上限（停采不需要久等）。
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
/// `read_packet` 单次等待编码帧的轮询粒度（同时也是 `stop` 的响应上限）。
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// `read_packet` 在没有数据时多久返回一次 `None`（让引擎有机会做别的事）。
const IDLE_RETURN: Duration = Duration::from_millis(200);

/// 采集流**不需要**视频：把输出尺寸压到最小、帧间隔放到 1 秒，只留音频。
const NOMINAL_VIDEO_EDGE: usize = 16;

// CoreGraphics 的屏幕录制权限 / 主显示器查询（不引入 `objc2-core-graphics` 依赖）。
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    // 是否已获得「屏幕录制」权限（不弹框）。
    fn CGPreflightScreenCaptureAccess() -> bool;
    // 申请「屏幕录制」权限（首次调用弹系统框）。
    fn CGRequestScreenCaptureAccess() -> bool;
    // 主显示器的 `CGDirectDisplayID`。
    fn CGMainDisplayID() -> u32;
}

/// 一帧编码后的音频。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPacket {
    /// Opus 包。
    pub data: Vec<u8>,
    /// 采集完成时刻（进程单调时钟，微秒）。
    pub capture_ts_us: u64,
}

/// 采集侧的累计读数（进程内观测，不上线）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// SCK 音频回调次数。
    pub callbacks: u64,
    /// 已交给编码器的 20 ms 帧数。
    pub frames: u64,
    /// 因帧队列满而丢弃的帧数。
    pub dropped: u64,
}

#[derive(Default)]
struct CaptureCounters {
    callbacks: AtomicU64,
    frames: AtomicU64,
    dropped: AtomicU64,
}

/// 回调线程与读取侧共享的采集状态。
struct CaptureShared {
    /// 20 ms 交织 `f32` 帧（满则丢最新一帧）。
    tx: SyncSender<Vec<f32>>,
    /// 还没攒够一帧的残余采样点（只有回调线程访问）。
    pending: Mutex<Vec<f32>>,
    counters: Arc<CaptureCounters>,
    last_error: Arc<Mutex<Option<String>>>,
    /// 只上报一次格式诊断，避免回调每 10 ms 刷一条日志。
    reported: AtomicBool,
}

impl CaptureShared {
    fn handle_sample_buffer(&self, sample_buffer: &CMSampleBuffer) {
        self.counters.callbacks.fetch_add(1, Ordering::Relaxed);
        let mut pending = lock(&self.pending);
        if let Err(err) = sample_buffer_to_pending(sample_buffer, &mut pending) {
            if !self.reported.swap(true, Ordering::Relaxed) {
                let message = err.to_string();
                tracing::warn!(error = %message, "SCK 音频帧无法转换，采集将丢弃这些帧");
                set_error(&self.last_error, message);
            }
            pending.clear();
            return;
        }
        let mut frames = 0_u64;
        while pending.len() >= FRAME_FLOATS {
            let frame: Vec<f32> = pending.drain(..FRAME_FLOATS).collect();
            match self.tx.try_send(frame) {
                Ok(()) => frames = frames.saturating_add(1),
                Err(TrySendError::Full(_)) => {
                    self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        if frames > 0 {
            self.counters.frames.fetch_add(frames, Ordering::Relaxed);
        }
    }
}

/// `SCStreamOutput` / `SCStreamDelegate` 的宿主对象。
struct AudioOutputIvars {
    shared: Arc<CaptureShared>,
}

define_class!(
    // SAFETY: NSObject 没有子类化要求；`AudioOutput` 不实现 `Drop`。
    #[unsafe(super(NSObject))]
    #[ivars = AudioOutputIvars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Audio {
                return;
            }
            self.ivars().shared.handle_sample_buffer(sample_buffer);
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            let message = format!("ScreenCaptureKit 停止采集：{}", ns_error(error));
            tracing::warn!(%message);
            set_error(&self.ivars().shared.last_error, message);
        }
    }
);

impl AudioOutput {
    fn new(shared: Arc<CaptureShared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(AudioOutputIvars { shared });
        // SAFETY: `NSObject` 的指定初始化器；ivars 已在 `set_ivars` 里就位。
        unsafe { msg_send![super(this), init] }
    }
}

/// 一次成功启动的采集会话。
///
/// **只存在于采集线程内**：`SCStream` / `SCContentFilter` 是 ObjC 对象，objc2 没有给
/// 它们 `Send`/`Sync`，所以这些对象绝不能进 `LoopbackEncoder` 那样要求 `Send + Sync`
/// 的共享结构体（引擎的 `AudioSource` 是 `Send + Sync`）。采集线程独占它们、并在
/// 自己退出前完成「移除输出 + 停采」。
struct StreamSession {
    stream: Retained<SCStream>,
    output: Retained<AudioOutput>,
    /// 回调队列必须比 `SCStream` 活得久（`addStreamOutput` 只是弱持有）。
    _queue: DispatchRetained<DispatchQueue>,
}

/// 采集线程的控制句柄。
struct CaptureHandle {
    /// 发一个字节 = 请求采集线程停采并退出；sender 被丢弃同样视为停止。
    stop: SyncSender<()>,
    thread: std::thread::JoinHandle<()>,
}

/// ScreenCaptureKit 系统声音采集 + Opus 编码。
///
/// 所有方法都是 `&self`（内部互斥）：引擎的 `AudioSource` 是 `&self` 接口，
/// 适配器可以直接持有它而不必再包一层 `Mutex`。
pub struct LoopbackEncoder {
    /// 串行化 `start` / `stop`（引擎可能在两个不同的阻塞任务里调用）。
    lifecycle: Mutex<()>,
    encoder: Mutex<EncoderSlot>,
    capture: Mutex<Option<CaptureHandle>>,
    receiver: Mutex<Option<Receiver<Vec<f32>>>>,
    counters: Arc<CaptureCounters>,
    last_error: Arc<Mutex<Option<String>>>,
}

struct EncoderSlot {
    encoder: Encoder,
    packet: Vec<u8>,
}

impl LoopbackEncoder {
    /// 创建编码器（不打开采集）。
    ///
    /// # Errors
    /// libopus 不可用或参数非法时返回 [`MacAudioError::Opus`] /
    /// [`MacAudioError::Unsupported`]。
    pub fn new() -> MacAudioResult<Self> {
        let encoder = Encoder::new(SAMPLE_RATE, CHANNELS, OPUS_BITRATE_BPS)?;
        Ok(Self {
            lifecycle: Mutex::new(()),
            encoder: Mutex::new(EncoderSlot {
                encoder,
                packet: vec![0_u8; MAX_PACKET_BYTES],
            }),
            capture: Mutex::new(None),
            receiver: Mutex::new(None),
            counters: Arc::new(CaptureCounters::default()),
            last_error: Arc::new(Mutex::new(None)),
        })
    }

    /// 打开系统声音采集（幂等：已在采集时直接返回 `Ok`）。
    ///
    /// **会阻塞**：SCK 只提供 completion handler 形式的接口，这里用通道把它变成
    /// 同步等待（最长 [`CONTENT_TIMEOUT`] / [`CAPTURE_TIMEOUT`]）。必须在阻塞任务里
    /// 调用，不要在 GUI 主线程上直接调。
    ///
    /// # Errors
    /// 缺少屏幕录制权限时返回 [`MacAudioError::ScreenRecordingPermission`]；
    /// SCK 枚举/建流/启采失败、或等待超时时返回对应错误。错误串是中文，可直接展示。
    pub fn start(&self) -> MacAudioResult<()> {
        let _lifecycle = lock(&self.lifecycle);
        if self.is_started() {
            return Ok(());
        }
        let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE_DEPTH);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let shared = Arc::new(CaptureShared {
            tx,
            pending: Mutex::new(Vec::with_capacity(FRAME_FLOATS * 2)),
            counters: Arc::clone(&self.counters),
            last_error: Arc::clone(&self.last_error),
            reported: AtomicBool::new(false),
        });
        let thread = std::thread::Builder::new()
            .name("rt-mac-audio-capture".to_owned())
            .spawn(move || capture_thread(shared, stop_rx, ready_tx))
            .map_err(|err| MacAudioError::Message(format!("创建音频采集线程失败: {err}")))?;
        let timeout = CONTENT_TIMEOUT + CAPTURE_TIMEOUT + Duration::from_secs(5);
        match ready_rx.recv_timeout(timeout) {
            Ok(Ok(())) => {
                *lock(&self.receiver) = Some(rx);
                *lock(&self.capture) = Some(CaptureHandle {
                    stop: stop_tx,
                    thread,
                });
                Ok(())
            }
            Ok(Err(err)) => {
                let _ = thread.join();
                set_error(&self.last_error, err.to_string());
                Err(err)
            }
            Err(_) => {
                let _ = stop_tx.send(());
                let _ = thread.join();
                Err(MacAudioError::Timeout("等待 ScreenCaptureKit 采集线程就绪"))
            }
        }
    }

    /// 当前是否在采集。
    #[must_use]
    pub fn is_started(&self) -> bool {
        lock(&self.capture).is_some()
    }

    /// 阻塞读取并编码下一帧。
    ///
    /// - `Some(packet)`：产出一帧 20 ms Opus 包；
    /// - `None`：`stop` 已置位、尚未 `start`、或 200 ms 内没有攒够一帧。
    ///
    /// 句柄内部错误（编码失败、格式不支持）只记录到 [`Self::last_error`]，不向上抛：
    /// 会话应当继续（画面/输入不受影响）。
    #[must_use]
    pub fn read_packet(&self, stop: &AtomicBool) -> Option<EncodedPacket> {
        let deadline = Instant::now() + IDLE_RETURN;
        loop {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            // 只在这一小段里持有接收端锁，`stop` 不会被卡住。
            let received = {
                let guard = lock(&self.receiver);
                let receiver = guard.as_ref()?;
                receiver.recv_timeout(POLL_INTERVAL)
            };
            match received {
                Ok(pcm) => return self.encode_frame(&pcm),
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// 停止采集并释放 SCK 资源。
    pub fn stop(&self) {
        let _lifecycle = lock(&self.lifecycle);
        self.stop_locked();
    }

    /// [`Self::stop`] 的本体（调用方已持有 `lifecycle` 锁）。
    fn stop_locked(&self) {
        *lock(&self.receiver) = None;
        if let Some(handle) = lock(&self.capture).take() {
            // 采集线程收到信号后移除输出、停采、释放 ObjC 对象，然后退出。
            let _ = handle.stop.send(());
            let _ = handle.thread.join();
        }
    }

    /// 采集侧的累计读数。
    #[must_use]
    pub fn stats(&self) -> CaptureStats {
        CaptureStats {
            callbacks: self.counters.callbacks.load(Ordering::Relaxed),
            frames: self.counters.frames.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
        }
    }

    /// 最近一次内部错误（中文），供探针/UI 展示。
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }

    /// 把一帧交织 `f32` 编码成 Opus 包。
    fn encode_frame(&self, pcm_frame: &[f32]) -> Option<EncodedPacket> {
        let mut slot = lock(&self.encoder);
        let EncoderSlot { encoder, packet } = &mut *slot;
        match encoder.encode(pcm_frame, packet) {
            Ok(written) => Some(EncodedPacket {
                data: packet[..written].to_vec(),
                capture_ts_us: clock::now_us(),
            }),
            Err(err) => {
                set_error(&self.last_error, err.to_string());
                None
            }
        }
    }
}

impl Drop for LoopbackEncoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 采集线程：建流、启采、等停止信号、清理。
///
/// 所有 ObjC 对象（`SCStream` / `SCContentFilter` / `AudioOutput`）都在本线程创建与
/// 释放——objc2 不给它们 `Send`/`Sync`，而 [`LoopbackEncoder`] 必须是 `Send + Sync`。
#[expect(
    clippy::needless_pass_by_value,
    reason = "三个值都移交给 std::thread::spawn 的 'static 闭包，必须按值持有"
)]
fn capture_thread(
    shared: Arc<CaptureShared>,
    stop: Receiver<()>,
    ready: SyncSender<MacAudioResult<()>>,
) {
    let session = match start_stream(&shared) {
        Ok(session) => session,
        Err(err) => {
            let _ = ready.try_send(Err(err));
            return;
        }
    };
    let _ = ready.try_send(Ok(()));
    tracing::info!(
        sample_rate = SAMPLE_RATE,
        channels = CHANNELS,
        "ScreenCaptureKit 系统声音采集已启动"
    );
    // 阻塞等停止信号；`CaptureHandle` 被丢弃时 sender 断开，也按停止处理。
    let _ = stop.recv();
    shutdown_stream(&session.stream, &session.output);
}

/// 在采集线程内完成「权限 → 枚举 → 建流 → 启采」。
fn start_stream(shared: &Arc<CaptureShared>) -> MacAudioResult<StreamSession> {
    ensure_screen_recording()?;
    let content = fetch_shareable_content(CONTENT_TIMEOUT)?;
    let display = pick_display(&content)?;
    let filter = build_filter(&display);
    let config = build_configuration();
    let output = AudioOutput::new(Arc::clone(shared));
    let queue = DispatchQueue::new("rust-tunnel.mac-audio.capture", None);
    // SAFETY: filter/config 都是本函数构造的合法对象；output 实现了
    // `SCStreamDelegate`，且被下面的会话保活。
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
            SCStreamOutputType::Audio,
            Some(&queue),
        )
    };
    if let Err(err) = added {
        // 还没启采：直接释放即可，不需要 stopCapture。
        return Err(ns_error(&err));
    }
    let session = StreamSession {
        stream,
        output,
        _queue: queue,
    };
    if let Err(err) = start_capture(&session.stream, CAPTURE_TIMEOUT) {
        // 已经加了输出：走完整清理（移除输出 + 停采）。
        shutdown_stream(&session.stream, &session.output);
        return Err(err);
    }
    Ok(session)
}

/// 预检 + 申请「屏幕录制」权限。
fn ensure_screen_recording() -> MacAudioResult<()> {
    // SAFETY: 两个函数都没有指针参数，只读写 TCC 状态。
    if unsafe { CGPreflightScreenCaptureAccess() } {
        return Ok(());
    }
    // 首次调用会弹一次系统授权框；同一进程后续调用不再弹。
    // SAFETY: 同上。
    if unsafe { CGRequestScreenCaptureAccess() } {
        return Ok(());
    }
    Err(MacAudioError::ScreenRecordingPermission)
}

/// 同步等待 `SCShareableContent`（SCK 只提供 completion handler 形式的接口）。
fn fetch_shareable_content(timeout: Duration) -> MacAudioResult<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if error.is_null() {
                if content.is_null() {
                    Err(MacAudioError::Platform(
                        "ScreenCaptureKit 返回了空的可共享内容".to_owned(),
                    ))
                } else {
                    // SAFETY: 指针在回调期间有效；`retain` 取一份所有权给读取侧。
                    unsafe { Retained::retain(content) }.ok_or_else(|| {
                        MacAudioError::Platform("无法持有 SCK 可共享内容".to_owned())
                    })
                }
            } else {
                // SAFETY: SCK 在回调期间保证 `error` 指向有效对象。
                Err(ns_error(unsafe { &*error }))
            };
            let _ = tx.try_send(result);
        },
    );
    // SAFETY: 回调只把结果送进本线程的通道，不触碰其它共享状态。
    let block_ref: &DynBlock<dyn Fn(*mut SCShareableContent, *mut NSError)> = &block;
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(block_ref) };
    rx.recv_timeout(timeout).map_err(|_| {
        MacAudioError::Timeout("等待 ScreenCaptureKit 可共享内容（屏幕录制权限未授予？）")
    })?
}

/// 选主显示器（多显示器时按 `CGMainDisplayID` 对齐）；取不到就退回第一个。
fn pick_display(content: &SCShareableContent) -> MacAudioResult<Retained<SCDisplay>> {
    // SAFETY: `content` 由本函数借用，`displays` 只读。
    let displays = unsafe { content.displays() }.to_vec();
    if displays.is_empty() {
        return Err(MacAudioError::Platform(
            "ScreenCaptureKit 没有枚举到任何显示器（屏幕录制权限未授予？）".to_owned(),
        ));
    }
    // SAFETY: 无参数调用，返回主显示器的 `CGDirectDisplayID`。
    let main_id = unsafe { CGMainDisplayID() };
    // SAFETY: `SCDisplay` 由 SCK 持有，`displayID` 只读。
    displays
        .iter()
        .find(|display| unsafe { display.displayID() } == main_id)
        .or_else(|| displays.first())
        .cloned()
        .ok_or_else(|| MacAudioError::Platform("无法取到主显示器".to_owned()))
}

/// 整个显示器、不排除任何窗口（音频与窗口无关，`excludesCurrentProcessAudio` 管回声）。
fn build_filter(display: &SCDisplay) -> Retained<SCContentFilter> {
    let excluded = NSArray::<SCWindow>::from_slice(&[]);
    // SAFETY: `display` 来自本次枚举，`excluded` 是合法的空数组。
    unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &excluded,
        )
    }
}

/// 只采音频的流配置：48 kHz 立体声、排除本进程声音、视频输出压到最小。
fn build_configuration() -> Retained<SCStreamConfiguration> {
    // SAFETY: `new` 是 NSObject 的标准构造；下面的 setter 都是标量参数。
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setCapturesAudio(true);
        config.setExcludesCurrentProcessAudio(true);
        config.setSampleRate(isize::try_from(SAMPLE_RATE).unwrap_or(48_000));
        config.setChannelCount(isize::try_from(CHANNELS).unwrap_or(2));
        // 不添加 Screen 输出，视频帧不会送到我们这里；把尺寸与帧率压到最小，
        // 避免 SCK 在内部为视频白白做缩放/合成。
        config.setWidth(NOMINAL_VIDEO_EDGE);
        config.setHeight(NOMINAL_VIDEO_EDGE);
        config.setShowsCursor(false);
        config.setQueueDepth(3);
        // 一帧每秒：音频链路不依赖它，视频侧越静越好。
        config.setMinimumFrameInterval(CMTime::new(1, 1));
    }
    config
}

/// 同步等待 `startCapture` 完成。
fn start_capture(stream: &SCStream, timeout: Duration) -> MacAudioResult<()> {
    capture_completion(stream, timeout, true)
}

/// 同步等待 `stopCapture` 完成（失败只记日志，不影响资源释放）。
fn shutdown_stream(stream: &SCStream, output: &AudioOutput) {
    // SAFETY: `output` 确实以 Audio 类型添加过，`stream` 由调用方持有。
    let removed = unsafe {
        stream.removeStreamOutput_type_error(
            ProtocolObject::from_ref(output),
            SCStreamOutputType::Audio,
        )
    };
    if let Err(err) = removed {
        tracing::debug!(error = %ns_error(&err), "移除 SCK 音频输出失败（采集可能已停止）");
    }
    let _ = capture_completion(stream, STOP_TIMEOUT, false);
}

/// 把 SCK 的 completion handler 变成同步等待。
fn capture_completion(stream: &SCStream, timeout: Duration, start: bool) -> MacAudioResult<()> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(move |error: *mut NSError| {
        let result = if error.is_null() {
            Ok(())
        } else {
            // SAFETY: SCK 在回调期间保证 `error` 指向有效对象。
            Err(ns_error(unsafe { &*error }))
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
        .unwrap_or(Err(MacAudioError::Timeout(fallback)))
}

/// `NSError` → 本项目错误（中文可读）。
fn ns_error(error: &NSError) -> MacAudioError {
    let (code, message) = (error.code(), error.localizedDescription());
    MacAudioError::ScreenCaptureKit {
        code: i64::try_from(code).unwrap_or(i64::MAX),
        message: message.to_string(),
    }
}

/// 把 `CMSampleBuffer` 里的 `AudioBufferList` 归一成交织立体声，追加到 `out`。
fn sample_buffer_to_pending(
    sample_buffer: &CMSampleBuffer,
    out: &mut Vec<f32>,
) -> MacAudioResult<()> {
    // SAFETY: `sample_buffer` 由 SCK 在回调期间持有；下面的查询都是只读的。
    let frames = usize::try_from(unsafe { sample_buffer.num_samples() })
        .map_err(|_| MacAudioError::Platform("SCK 音频帧数为负".to_owned()))?;
    if frames == 0 {
        return Ok(());
    }
    let Some(format) = (unsafe { sample_buffer.format_description() }) else {
        return Err(MacAudioError::Platform("SCK 音频帧没有格式描述".to_owned()));
    };
    // SAFETY: 返回指向格式描述内部 ASBD 的只读指针；`format` 在本函数内有效。
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&format) };
    if asbd.is_null() {
        return Err(MacAudioError::Platform(
            "SCK 音频帧的格式描述里没有 ASBD".to_owned(),
        ));
    }
    // SAFETY: 上一行已确认非空，且指针在 `format` 生命周期内有效。
    let asbd = unsafe { &*asbd };
    validate_asbd(asbd)?;
    let channels = usize::try_from(asbd.mChannelsPerFrame).unwrap_or(0);
    if channels == 0 {
        return Err(MacAudioError::Platform("SCK 音频帧的声道数为 0".to_owned()));
    }

    let mut size_needed = 0_usize;
    // SAFETY: 传 null 的 `buffer_list_out` 是查询所需大小的合法用法。
    let _ = unsafe {
        sample_buffer.audio_buffer_list_with_retained_block_buffer(
            &raw mut size_needed,
            std::ptr::null_mut(),
            0,
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            std::ptr::null_mut(),
        )
    };
    if size_needed == 0 {
        return Err(MacAudioError::Platform(
            "SCK 音频帧没有 AudioBufferList".to_owned(),
        ));
    }
    // 用 u64 保证 8 字节对齐（`AudioBufferList` 里的指针字段要求）。
    let mut storage = vec![0_u64; size_needed.div_ceil(size_of::<u64>())];
    let storage_bytes = storage.len().saturating_mul(size_of::<u64>());
    let list = storage.as_mut_ptr().cast::<AudioBufferList>();
    // `block_buffer_out` 必须给非空指针：macOS 27 上传 NULL 会直接返回
    // `kCMSampleBufferError_RequiredParameterMissing (-12731)`，哪怕 bufferList 与
    // size 都正确。返回的 block buffer 是 +1 引用，且 AudioBufferList 的 `mData`
    // 指向它的内存——必须在拷贝完成前保活，否则会悬垂。
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    // SAFETY: `storage` 至少有 `size_needed` 字节且 8 字节对齐；`block` 由调用方接管。
    let status = unsafe {
        sample_buffer.audio_buffer_list_with_retained_block_buffer(
            &raw mut size_needed,
            list,
            storage_bytes,
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &raw mut block,
        )
    };
    if status != 0 {
        return Err(MacAudioError::Platform(format!(
            "取 SCK 音频缓冲失败：OSStatus {status}"
        )));
    }
    // SAFETY: 成功时 `block` 是 +1 引用；`CFRetained` 在函数结束时释放。
    let _block = std::ptr::NonNull::new(block).map(|ptr| unsafe { CFRetained::from_raw(ptr) });
    // SAFETY: 上面的调用已按 `size_needed` 填好 AudioBufferList。
    let buffer_count = usize::try_from(unsafe { (*list).mNumberBuffers }).unwrap_or(0);
    if buffer_count == 0 {
        return Err(MacAudioError::Platform(
            "SCK 音频帧的 AudioBufferList 为空".to_owned(),
        ));
    }

    if buffer_count == 1 {
        // SAFETY: `mBuffers` 至少有一个元素（`buffer_count == 1`）。
        let buffer = unsafe { &(*list).mBuffers[0] };
        let buffer_channels = usize::try_from(buffer.mNumberChannels).unwrap_or(0);
        if buffer_channels == 0 {
            return Err(MacAudioError::Platform(
                "SCK 音频缓冲的声道数为 0".to_owned(),
            ));
        }
        // SAFETY: 指针与长度由 SCK 按 `frames × buffer_channels` 填好。
        let samples = unsafe { audio_buffer_samples(buffer, frames, buffer_channels)? };
        return pcm::append_interleaved(out, samples, buffer_channels, frames);
    }

    // 非交织：每个 AudioBuffer 一路单声道平面。
    let mut planes: Vec<&[f32]> = Vec::with_capacity(buffer_count);
    for index in 0..buffer_count {
        // SAFETY: `mBuffers` 是长度为 `mNumberBuffers` 的变长数组。
        let buffer = unsafe { &*((*list).mBuffers.as_ptr().add(index)) };
        let buffer_channels = usize::try_from(buffer.mNumberChannels).unwrap_or(0);
        if buffer_channels != 1 {
            return Err(MacAudioError::Platform(format!(
                "SCK 给了 {buffer_count} 个音频缓冲，但第 {index} 个有 {buffer_channels} 个声道"
            )));
        }
        // SAFETY: 同上，每个平面是 `frames` 个单声道采样点。
        planes.push(unsafe { audio_buffer_samples(buffer, frames, 1)? });
    }
    pcm::append_planar(out, &planes, frames)
}

/// 校验 ASBD：必须是 48 kHz、32-bit float、PCM。
fn validate_asbd(asbd: &AudioStreamBasicDescription) -> MacAudioResult<()> {
    let rate = asbd.mSampleRate;
    if asbd.mFormatID != kAudioFormatLinearPCM
        || asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0
        || asbd.mBitsPerChannel != 32
    {
        return Err(MacAudioError::Platform(format!(
            "SCK 音频格式不是 32-bit float PCM（format={:#x} flags={:#x} bits={}）",
            asbd.mFormatID, asbd.mFormatFlags, asbd.mBitsPerChannel
        )));
    }
    if (rate - f64::from(SAMPLE_RATE)).abs() > 1.0 {
        return Err(MacAudioError::Platform(format!(
            "SCK 音频采样率是 {rate} Hz，不是 {SAMPLE_RATE} Hz（Opus 只接受 48 kHz）"
        )));
    }
    Ok(())
}

/// 把一个 `AudioBuffer` 的裸指针包成 `&[f32]`。
///
/// # Safety
/// `buffer.mData` 必须指向至少 `frames × channels` 个 `f32`，且在返回的切片使用期间有效。
unsafe fn audio_buffer_samples<'a>(
    buffer: &AudioBuffer,
    frames: usize,
    channels: usize,
) -> MacAudioResult<&'a [f32]> {
    if buffer.mData.is_null() {
        return Err(MacAudioError::Platform(
            "SCK 音频缓冲的数据指针为空".to_owned(),
        ));
    }
    let samples = frames
        .checked_mul(channels)
        .ok_or_else(|| MacAudioError::Platform("SCK 音频帧长度溢出".to_owned()))?;
    let bytes = samples
        .checked_mul(SAMPLE_BYTES)
        .ok_or_else(|| MacAudioError::Platform("SCK 音频帧字节数溢出".to_owned()))?;
    let capacity = usize::try_from(buffer.mDataByteSize).unwrap_or(0);
    if capacity < bytes {
        return Err(MacAudioError::Platform(format!(
            "SCK 音频缓冲只有 {capacity} 字节，少于 {frames} 帧 × {channels} 声道所需的 {bytes} 字节"
        )));
    }
    let data = buffer.mData.cast::<f32>();
    if !data.is_aligned() {
        return Err(MacAudioError::Platform(
            "SCK 音频缓冲不是 4 字节对齐的 f32".to_owned(),
        ));
    }
    Ok(std::slice::from_raw_parts(data, samples))
}

/// 取锁；中毒时取回内部值（音频路径不能因为一次 panic 永久停摆）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 记录最近一次内部错误。
fn set_error(slot: &Mutex<Option<String>>, message: String) {
    *lock(slot) = Some(message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_start_at_zero() {
        let encoder = LoopbackEncoder::new().expect("创建编码器");
        assert_eq!(encoder.stats(), CaptureStats::default());
        assert!(!encoder.is_started());
        assert!(encoder.last_error().is_none());
    }

    /// 引擎的 `AudioSource` 要求 `Send + Sync + 'static`；这里用编译期断言钉住。
    /// （`Playback` 对应 `AudioSink`，在同 crate 的 `playback` 里同样断言。）
    #[test]
    fn encoder_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<LoopbackEncoder>();
    }
}
