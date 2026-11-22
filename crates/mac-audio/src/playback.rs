//! 控制端播放：编码包 → 抖动缓冲 → Opus 解码 → AudioQueue 默认输出。
//!
//! # 为什么用 AudioQueue 而不是 AudioUnit
//!
//! AudioQueue 的**缓冲区完成回调就是播放时钟**：每播完一个 20 ms 缓冲，系统在内部
//! 线程上回调一次，我们在这个回调里从抖动缓冲取下一帧、解码、填回同一个缓冲并
//! `AudioQueueEnqueueBuffer`。这样既不需要自己维护 20 ms 定时器，也不会和设备时钟
//! 漂移；`AudioQueueNewOutput` 传 NULL run loop 就得到这个内部回调线程。
//!
//! # 队列深度与延迟
//!
//! 三个 20 ms 缓冲（60 ms）在队列里循环，加上抖动缓冲默认 80 ms，播放侧总延迟
//! ≈ 140 ms + 设备硬件延迟。缓冲数少于 2 会在回调稍慢时断音，多于 4 只会加大延迟。
//!
//! # 线程与阻塞
//!
//! 引擎的 `AudioSink::on_packet` 在 Tokio 运行时线程上调用，[`Playback::on_packet`]
//! 只做 `try_send`（满则丢弃并计数，绝不阻塞）。抖动缓冲、解码与设备写入全部发生在
//! 播放线程与 AudioQueue 的回调线程上。

use crate::error::{MacAudioError, MacAudioResult};
use crate::format::{CHANNELS, FRAME_BYTES, FRAME_FLOATS, SAMPLE_BYTES, SAMPLE_RATE};
use crate::jitter::{JitterBuffer, JitterConfig, Pull, Push};
use crate::opus::Decoder;

use objc2_audio_toolbox::{
    AudioQueueAllocateBuffer, AudioQueueBuffer, AudioQueueBufferRef, AudioQueueDispose,
    AudioQueueEnqueueBuffer, AudioQueueNewOutput, AudioQueueRef, AudioQueueStart, AudioQueueStop,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
    AudioStreamBasicDescription,
};

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// 播放队列深度（编码包）。64 帧 ≈ 1.28 秒，远大于抖动缓冲的目标深度；
/// 满时丢最新包，绝不阻塞引擎的运行时线程。
const QUEUE_DEPTH: usize = 64;

/// AudioQueue 的循环缓冲数：3 × 20 ms = 60 ms 的设备侧余量。
const QUEUE_BUFFERS: usize = 3;

/// 输出设备打开失败后的重试间隔。
const RENDER_RETRY: Duration = Duration::from_secs(2);

/// 播放线程命令泵的等待粒度（必须是 20 ms 的整数分频，保证补帧节拍不被拖慢）。
const PUMP_INTERVAL: Duration = Duration::from_millis(5);

/// 播放线程启动握手的最长等待。
const START_TIMEOUT: Duration = Duration::from_secs(2);

/// 一帧 PCM 的字节数（AudioQueue buffer 的口径）。
#[expect(
    clippy::cast_possible_truncation,
    reason = "FRAME_BYTES = 7680，远小于 u32 上限，且是编译期常量"
)]
const FRAME_BYTES_U32: u32 = FRAME_BYTES as u32;

/// 控制端收到的一段编码音频（对应引擎的 `ReceivedAudio`，本 crate 不依赖它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedPacket {
    /// 线上序号（被控端递增；可能不连续——发送侧队列满会丢新包）。
    pub seq: u64,
    /// 采集完成时刻（被控端单调时钟，微秒；仅观测用）。
    pub capture_ts_us: u64,
    /// Opus 包。
    pub data: Vec<u8>,
}

/// 播放线程的统计快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackSnapshot {
    /// 收到的包数（进入抖动缓冲的调用次数）。
    pub received: u64,
    /// 因播放队列满而丢弃的包数。
    pub dropped: u64,
    /// 抖动缓冲欠载次数。
    pub underruns: u64,
    /// 迟到（序号已过期）而丢弃的包数。
    pub late: u64,
    /// 抖动缓冲溢出时裁掉的包数。
    pub overflow: u64,
    /// 用 Opus PLC 顶替的帧数（丢包隐藏次数）。
    pub concealed: u64,
    /// 当前目标缓冲（毫秒）。
    pub buffer_ms: u32,
}

#[derive(Default)]
struct Stats {
    received: AtomicU64,
    dropped: AtomicU64,
    underruns: AtomicU64,
    late: AtomicU64,
    overflow: AtomicU64,
    concealed: AtomicU64,
    buffer_ms: AtomicU32,
}

enum Command {
    Packet { seq: u64, data: Vec<u8> },
    Stop,
}

/// 抖动缓冲 + 解码器（只由播放线程与 AudioQueue 回调线程访问，互斥串行化）。
struct PlayerCore {
    decoder: Decoder,
    jitter: JitterBuffer,
    /// 解码输出（一帧交错 `f32`）。
    pcm: Vec<f32>,
}

/// 一次填充的结果（只用于统计）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FillOutcome {
    Packet,
    Concealed,
    Idle,
    /// 缓冲容量不够 / 解码失败，写了静音。
    Degraded,
}

impl PlayerCore {
    /// 从抖动缓冲取一帧写进 `out`（长度必须是 [`FRAME_FLOATS`]）。
    fn pull_into(&mut self, out: &mut [f32]) -> FillOutcome {
        match self.jitter.pull(Instant::now()) {
            Pull::Packet(data) => match self.decoder.decode(&data, &mut self.pcm, false) {
                Ok(frames) => {
                    copy_decoded(out, &self.pcm, frames);
                    FillOutcome::Packet
                }
                Err(err) => {
                    tracing::debug!(error = %err, "Opus 解码失败，用静音顶替这一帧");
                    out.fill(0.0);
                    FillOutcome::Degraded
                }
            },
            Pull::Missing => {
                // 丢包隐藏：用解码器内部状态生成一个替代帧，避免爆音/断音。
                let concealed = match self.decoder.decode_plc(&mut self.pcm) {
                    Ok(frames) => {
                        copy_decoded(out, &self.pcm, frames);
                        true
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "Opus PLC 失败，用静音顶替这一帧");
                        out.fill(0.0);
                        false
                    }
                };
                if concealed {
                    FillOutcome::Concealed
                } else {
                    FillOutcome::Degraded
                }
            }
            Pull::Idle => {
                // 没有可播的数据：写静音（AudioQueue 必须拿到完整的一帧才能还回来）。
                out.fill(0.0);
                FillOutcome::Idle
            }
        }
    }
}

/// 把解码结果拷进输出缓冲，不足一帧的部分补零（Opus 正常总是给满 20 ms）。
fn copy_decoded(out: &mut [f32], pcm: &[f32], frames: usize) {
    let samples = frames
        .saturating_mul(usize::from(CHANNELS))
        .min(out.len())
        .min(pcm.len());
    out[..samples].copy_from_slice(&pcm[..samples]);
    out[samples..].fill(0.0);
}

/// 播放线程与 AudioQueue 回调共享的状态。
struct PlaybackShared {
    core: Mutex<PlayerCore>,
    volume: Arc<AtomicU32>,
    stats: Arc<Stats>,
    /// 回调里发现队列异常（enqueue 失败）时置位，由播放线程重建队列。
    needs_reopen: AtomicBool,
}

/// 一个已启动的 AudioQueue 及其循环缓冲。
struct QueueSession {
    queue: AudioQueueRef,
    /// 持有 `PlaybackShared` 的 `Arc`，保证传给回调的裸指针在队列生命周期内有效。
    _shared: Arc<PlaybackShared>,
}

impl QueueSession {
    /// 建队列、分配并预热 3 个静音缓冲、启动播放。
    fn open(shared: Arc<PlaybackShared>) -> MacAudioResult<Self> {
        let format = pcm_format();
        // SAFETY: `shared` 的 `Arc` 会被本结构持有；传给回调的指针在队列释放前一直有效。
        let user = Arc::as_ptr(&shared).cast::<c_void>().cast_mut();
        let mut queue: AudioQueueRef = std::ptr::null_mut();
        // SAFETY: 格式结构在调用期间有效（AudioQueue 会复制它），out 参数指向栈上变量。
        let status = unsafe {
            AudioQueueNewOutput(
                NonNull::from(&format),
                Some(output_callback),
                user,
                None,
                None,
                0,
                NonNull::from(&mut queue),
            )
        };
        if status != 0 || queue.is_null() {
            return Err(MacAudioError::audio_queue(status, "AudioQueueNewOutput"));
        }
        // 先建会话：下面任何一步失败都由 `Drop` 负责释放队列。
        let session = Self {
            queue,
            _shared: shared,
        };
        for _ in 0..QUEUE_BUFFERS {
            let mut buffer: AudioQueueBufferRef = std::ptr::null_mut();
            // SAFETY: `queue` 有效，`buffer` 指向栈上变量。
            let status = unsafe {
                AudioQueueAllocateBuffer(queue, FRAME_BYTES_U32, NonNull::from(&mut buffer))
            };
            if status != 0 || buffer.is_null() {
                return Err(MacAudioError::audio_queue(
                    status,
                    "AudioQueueAllocateBuffer",
                ));
            }
            // SAFETY: 缓冲由上面的调用分配，容量至少是 `FRAME_BYTES_U32`。
            unsafe {
                std::ptr::write_bytes((*buffer).mAudioData.as_ptr().cast::<u8>(), 0, FRAME_BYTES);
                (*buffer).mAudioDataByteSize = FRAME_BYTES_U32;
            }
            // SAFETY: `buffer` 属于 `queue`，参数与绑定签名一致。
            let status = unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null()) };
            if status != 0 {
                return Err(MacAudioError::audio_queue(
                    status,
                    "AudioQueueEnqueueBuffer（预热）",
                ));
            }
        }
        // SAFETY: `queue` 有效；起点传 NULL 表示立即开始。
        let status = unsafe { AudioQueueStart(queue, std::ptr::null()) };
        if status != 0 {
            return Err(MacAudioError::audio_queue(status, "AudioQueueStart"));
        }
        Ok(session)
    }
}

impl Drop for QueueSession {
    fn drop(&mut self) {
        // SAFETY: 队列由本结构独占。immediate = true 保证返回后不再有回调，
        // 因此随后释放 `PlaybackShared` 的最后一个 `Arc` 是安全的。
        unsafe {
            let _ = AudioQueueStop(self.queue, true);
            let _ = AudioQueueDispose(self.queue, true);
        }
    }
}

/// 播放格式：48 kHz / 立体声 / 32-bit float 交错 linear PCM。
fn pcm_format() -> AudioStreamBasicDescription {
    let sample_bytes = u32::try_from(SAMPLE_BYTES).unwrap_or(4);
    let bytes_per_frame = u32::from(CHANNELS).saturating_mul(sample_bytes);
    AudioStreamBasicDescription {
        mSampleRate: f64::from(SAMPLE_RATE),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_frame,
        mChannelsPerFrame: u32::from(CHANNELS),
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// AudioQueue 的缓冲完成回调：取一帧、解码、填回缓冲并重新入队。
///
/// # Safety
/// `user` 必须是 [`QueueSession::open`] 传进去的 `PlaybackShared` 裸指针，且该对象在
/// 回调期间存活；`queue` / `buffer` 由 AudioQueue 保证有效。
unsafe extern "C-unwind" fn output_callback(
    user: *mut c_void,
    queue: AudioQueueRef,
    buffer: AudioQueueBufferRef,
) {
    if user.is_null() || buffer.is_null() {
        return;
    }
    // SAFETY: 见函数文档；`Arc` 由 `QueueSession` 与播放线程共同持有。
    let shared = unsafe { &*user.cast::<PlaybackShared>() };
    {
        let mut core = lock(&shared.core);
        let volume = shared.volume.load(Ordering::Relaxed);
        // SAFETY: `buffer` 由 AudioQueue 传入且有效。
        let outcome = unsafe { fill_buffer(&mut core, volume, &mut *buffer) };
        if outcome == FillOutcome::Concealed {
            shared.stats.concealed.fetch_add(1, Ordering::Relaxed);
        }
        shared
            .stats
            .underruns
            .store(core.jitter.underruns(), Ordering::Relaxed);
        shared
            .stats
            .buffer_ms
            .store(core.jitter.target_ms(), Ordering::Relaxed);
    }
    // SAFETY: 同一个缓冲刚播完，重新入队是 AudioQueue 的标准用法。
    let status = unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null()) };
    if status != 0 {
        shared.needs_reopen.store(true, Ordering::Relaxed);
    }
}

/// 从抖动缓冲取一帧写进 AudioQueue 缓冲。
///
/// # Safety
/// `buffer` 必须指向一个由 `AudioQueueAllocateBuffer` 分配、且当前归我们所有的缓冲。
unsafe fn fill_buffer(
    core: &mut PlayerCore,
    volume_percent: u32,
    buffer: &mut AudioQueueBuffer,
) -> FillOutcome {
    let capacity = usize::try_from(buffer.mAudioDataBytesCapacity).unwrap_or(0);
    if capacity < FRAME_BYTES {
        core.jitter.pull(Instant::now());
        return FillOutcome::Degraded;
    }
    let data = buffer.mAudioData.as_ptr().cast::<f32>();
    if !data.is_aligned() {
        return FillOutcome::Degraded;
    }
    // SAFETY: 容量已校验，`FRAME_FLOATS` 个 f32 正好落在缓冲内。
    let out = unsafe { std::slice::from_raw_parts_mut(data, FRAME_FLOATS) };
    let outcome = core.pull_into(out);
    apply_volume(out, volume_percent);
    // SAFETY: 缓冲区长度已校验，`FRAME_BYTES` 不超过容量。
    buffer.mAudioDataByteSize = FRAME_BYTES_U32;
    outcome
}

/// 应用播放增益（百分比 → 线性增益），并钳到 `[-1, 1]`。
#[expect(
    clippy::cast_precision_loss,
    reason = "音量百分比只有 0–200，转 f32 无损"
)]
fn apply_volume(pcm: &mut [f32], percent: u32) {
    if percent == 100 {
        return;
    }
    let gain = percent as f32 / 100.0;
    for sample in pcm {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

/// 控制端音频播放器（拥有一个独立播放线程）。
///
/// [`Self::new`] 只做准备（不启动），[`Self::start`] 才拉起线程与 AudioQueue。
/// 所有方法都是 `&self`，可以直接塞进 `AudioSink` 的适配器。
pub struct Playback {
    inner: Mutex<PlaybackInner>,
    volume: Arc<AtomicU32>,
    stats: Arc<Stats>,
}

struct PlaybackInner {
    tx: Option<SyncSender<Command>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Playback {
    /// 创建播放器（**不启动**线程，也不会因为「没有默认输出设备」而失败）。
    ///
    /// # Errors
    /// 目前不会失败；返回 `Result` 是为了与 [`Self::start`] 的调用形状一致。
    pub fn new() -> MacAudioResult<Self> {
        let stats = Arc::new(Stats::default());
        // 未起播时给 UI 一个合理读数（抖动缓冲的默认目标），而不是 0。
        stats
            .buffer_ms
            .store(JitterConfig::default().target_ms, Ordering::Relaxed);
        Ok(Self {
            inner: Mutex::new(PlaybackInner {
                tx: None,
                thread: None,
            }),
            volume: Arc::new(AtomicU32::new(100)),
            stats,
        })
    }

    /// 启动播放线程（幂等）。
    ///
    /// 设备打开在播放线程里完成：失败只记日志并每 2 秒重试，不会让这里返回错误。
    ///
    /// # Errors
    /// 线程创建失败、或 Opus 解码器不可用时返回错误。
    pub fn start(&self) -> MacAudioResult<()> {
        let mut inner = lock(&self.inner);
        if inner.thread.is_some() {
            return Ok(());
        }
        let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let volume = Arc::clone(&self.volume);
        let stats = Arc::clone(&self.stats);
        let thread = std::thread::Builder::new()
            .name("rt-mac-audio-play".to_owned())
            .spawn(move || playback_thread(rx, volume, stats, ready_tx))
            .map_err(|err| MacAudioError::Message(format!("创建音频播放线程失败: {err}")))?;
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => {
                inner.tx = Some(tx);
                inner.thread = Some(thread);
                Ok(())
            }
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(MacAudioError::Message(message))
            }
            Err(RecvTimeoutError::Disconnected) => {
                // 线程在发握手消息前就退出了（只可能是构造解码器之外的意外）。
                let _ = thread.join();
                Err(MacAudioError::Message(
                    "音频播放线程在启动前意外退出".to_owned(),
                ))
            }
            Err(RecvTimeoutError::Timeout) => {
                // 握手超时：线程仍在跑（只是还没走到发消息那一步），按已启动处理。
                inner.tx = Some(tx);
                inner.thread = Some(thread);
                Ok(())
            }
        }
    }

    /// 投递一个编码包（非阻塞；队列满则丢弃并计数）。
    pub fn on_packet(&self, packet: ReceivedPacket) {
        let inner = lock(&self.inner);
        let Some(tx) = inner.tx.as_ref() else {
            return;
        };
        match tx.try_send(Command::Packet {
            seq: packet.seq,
            data: packet.data,
        }) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// 投递一个编码包（只要序号与数据；等价于 [`Self::on_packet`]）。
    pub fn push(&self, seq: u64, data: Vec<u8>) {
        self.on_packet(ReceivedPacket {
            seq,
            capture_ts_us: 0,
            data,
        });
    }

    /// 设置播放音量（百分比，0–200）。这是本地增益，不上线。
    pub fn set_volume(&self, percent: u32) {
        self.volume.store(percent.min(200), Ordering::Relaxed);
    }

    /// 取一份统计快照。
    #[must_use]
    pub fn stats(&self) -> PlaybackSnapshot {
        PlaybackSnapshot {
            received: self.stats.received.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
            underruns: self.stats.underruns.load(Ordering::Relaxed),
            late: self.stats.late.load(Ordering::Relaxed),
            overflow: self.stats.overflow.load(Ordering::Relaxed),
            concealed: self.stats.concealed.load(Ordering::Relaxed),
            buffer_ms: self.stats.buffer_ms.load(Ordering::Relaxed),
        }
    }

    /// 当前是否已启动播放线程。
    #[must_use]
    pub fn is_running(&self) -> bool {
        lock(&self.inner).thread.is_some()
    }

    /// 停止播放线程并等待退出（最坏约一个设备缓冲周期）。
    pub fn stop(&self) {
        let mut inner = lock(&self.inner);
        let Some(tx) = inner.tx.take() else {
            return;
        };
        let _ = tx.send(Command::Stop);
        drop(tx);
        if let Some(thread) = inner.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 播放线程主体：命令泵 + 队列维护。
#[expect(
    clippy::needless_pass_by_value,
    reason = "两个值都移交给 std::thread::spawn 的 'static 闭包，必须按值持有"
)]
fn playback_thread(
    rx: Receiver<Command>,
    volume: Arc<AtomicU32>,
    stats: Arc<Stats>,
    ready: SyncSender<Result<(), String>>,
) {
    let decoder = match Decoder::new(SAMPLE_RATE, CHANNELS) {
        Ok(decoder) => decoder,
        Err(err) => {
            let _ = ready.try_send(Err(format!("Opus 解码器创建失败：{err}")));
            return;
        }
    };
    let core = PlayerCore {
        decoder,
        jitter: JitterBuffer::new(JitterConfig::default()),
        pcm: vec![0.0_f32; FRAME_FLOATS],
    };
    let shared = Arc::new(PlaybackShared {
        core: Mutex::new(core),
        volume,
        stats,
        needs_reopen: AtomicBool::new(false),
    });
    let _ = ready.try_send(Ok(()));

    let mut session: Option<QueueSession> = None;
    let mut next_open = Instant::now();
    loop {
        if session.is_none() && Instant::now() >= next_open {
            match QueueSession::open(Arc::clone(&shared)) {
                Ok(opened) => {
                    tracing::info!(buffers = QUEUE_BUFFERS, "AudioQueue 播放已启动");
                    session = Some(opened);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "打开 AudioQueue 播放失败，2 秒后重试");
                    next_open = Instant::now() + RENDER_RETRY;
                }
            }
        }

        match rx.recv_timeout(PUMP_INTERVAL) {
            Ok(Command::Packet { seq, data }) => {
                shared.stats.received.fetch_add(1, Ordering::Relaxed);
                let mut core = lock(&shared.core);
                match core.jitter.push(seq, data, Instant::now()) {
                    Push::Stored => {}
                    Push::Late => {
                        shared.stats.late.fetch_add(1, Ordering::Relaxed);
                    }
                    Push::Trimmed => {
                        shared.stats.overflow.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        if shared.needs_reopen.swap(false, Ordering::Relaxed) {
            tracing::warn!("AudioQueue 回调报告异常，重建播放队列");
            session = None;
            next_open = Instant::now();
        }
    }
    // 先释放会话（触发 `AudioQueueDispose`，回调随之停止），再释放 `shared`。
    drop(session);
}

/// 取锁；中毒时取回内部值（音频路径不能因为一次 panic 永久停摆）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_format_is_48k_stereo_f32() {
        let format = pcm_format();
        assert!((format.mSampleRate - 48_000.0).abs() < f64::EPSILON);
        assert_eq!(format.mFormatID, kAudioFormatLinearPCM);
        assert_eq!(format.mChannelsPerFrame, 2);
        assert_eq!(format.mBitsPerChannel, 32);
        assert_eq!(format.mBytesPerFrame, 8);
        assert_eq!(format.mBytesPerPacket, 8);
        assert_eq!(format.mFramesPerPacket, 1);
        assert_ne!(format.mFormatFlags & kAudioFormatFlagIsFloat, 0);
    }

    #[test]
    fn volume_scales_samples_and_clamps() {
        let mut pcm = [0.5_f32, -0.5, 0.9];
        apply_volume(&mut pcm, 200);
        assert!((pcm[0] - 1.0).abs() < 1.0e-6);
        assert!((pcm[1] + 1.0).abs() < 1.0e-6);
        assert_eq!(pcm[2], 1.0);
        // 100% 是原样。
        let mut pcm = [0.25_f32];
        apply_volume(&mut pcm, 100);
        assert_eq!(pcm[0], 0.25);
    }

    #[test]
    fn copy_decoded_pads_the_tail_with_silence() {
        let pcm = [0.5_f32, -0.5];
        let mut out = [1.0_f32; 4];
        copy_decoded(&mut out, &pcm, 1);
        assert_eq!(out, [0.5, -0.5, 0.0, 0.0]);
    }

    /// 引擎的 `AudioSink` 要求 `Send + Sync + 'static`；这里用编译期断言钉住。
    #[test]
    fn playback_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Playback>();
    }

    /// 新播放器的初始读数：音量 100、目标缓冲 80 ms、其余为 0。
    #[test]
    fn fresh_playback_reports_default_buffer() {
        let playback = Playback::new().expect("创建播放器");
        assert!(!playback.is_running());
        let stats = playback.stats();
        assert_eq!(stats.buffer_ms, 80);
        assert_eq!(stats.received, 0);
        assert_eq!(stats.dropped, 0);
    }
}
