//! 控制端播放：编码包 → 抖动缓冲 → Opus 解码 → WASAPI 默认输出。
//!
//! 播放必须跑在**独立线程**上，原因有两个：
//!
//! 1. WASAPI 的 `Render` 是 COM 公寓绑定的（`!Send`），必须在创建它的线程上使用；
//! 2. 播放节拍是 20 ms 的硬时钟，不能依赖引擎的 async 循环被及时调度。
//!
//! 引擎侧只调用 [`Playback::push`]（非阻塞 `try_send`）。队列满/未起播时丢包由
//! [`crate::jitter`] 的 PLC 抹平。

use crate::error::{AudioError, AudioResult};
use crate::format::{CHANNELS, FRAME_FLOATS, FRAME_MS, SAMPLE_RATE};
use crate::jitter::{JitterBuffer, JitterConfig, Pull};
use crate::opus::Decoder;
use crate::wasapi::Render;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 播放队列深度（编码包）。64 帧 ≈ 1.28 秒，远大于抖动缓冲的目标深度；
/// 满时丢最新包，绝不阻塞引擎的运行时线程。
const QUEUE_DEPTH: usize = 64;

/// 输出设备打开失败后的重试间隔。
const RENDER_RETRY: Duration = Duration::from_secs(2);

/// 播放线程的统计快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackSnapshot {
    /// 收到的包数。
    pub received: u64,
    /// 因播放队列满而丢弃的包数。
    pub dropped: u64,
    /// 抖动缓冲欠载次数。
    pub underruns: u64,
    /// 迟到（序号已过期）而丢弃的包数。
    pub late: u64,
    /// 抖动缓冲溢出时裁掉的包数。
    pub overflow: u64,
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
    buffer_ms: AtomicU32,
}

enum Command {
    Packet { seq: u64, data: Vec<u8> },
    Stop,
}

/// 控制端音频播放器（拥有一个独立线程）。
pub struct Playback {
    tx: SyncSender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
    volume: Arc<AtomicU32>,
    stats: Arc<Stats>,
}

impl Playback {
    /// 启动播放线程。
    ///
    /// 设备打开在播放线程里完成：构造函数不会因为「没有默认输出设备」而失败。
    ///
    /// # Errors
    /// 线程创建失败或 Opus 解码器不可用时返回错误。
    pub fn start() -> AudioResult<Self> {
        let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
        let volume = Arc::new(AtomicU32::new(100));
        let stats = Arc::new(Stats::default());
        let thread = std::thread::Builder::new()
            .name("rt-audio-play".to_owned())
            .spawn({
                let volume = Arc::clone(&volume);
                let stats = Arc::clone(&stats);
                move || playback_thread(rx, volume, stats)
            })
            .map_err(|err| AudioError::Message(format!("创建音频播放线程失败: {err}")))?;
        Ok(Self {
            tx,
            thread: Some(thread),
            volume,
            stats,
        })
    }

    /// 投递一个编码包（非阻塞；满则丢弃并计数）。
    pub fn push(&self, seq: u64, data: Vec<u8>) {
        match self.tx.try_send(Command::Packet { seq, data }) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// 设置播放音量（百分比，0–200）。
    pub fn set_volume(&self, percent: u32) {
        self.volume.store(percent.min(200), Ordering::Relaxed);
    }

    /// 取一份统计快照。
    #[must_use]
    pub fn snapshot(&self) -> PlaybackSnapshot {
        PlaybackSnapshot {
            received: self.stats.received.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
            underruns: self.stats.underruns.load(Ordering::Relaxed),
            late: self.stats.late.load(Ordering::Relaxed),
            overflow: self.stats.overflow.load(Ordering::Relaxed),
            buffer_ms: self.stats.buffer_ms.load(Ordering::Relaxed),
        }
    }

    /// 停止播放线程并等待退出（最坏约一个 WASAPI 缓冲周期）。
    pub fn stop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 播放线程主体：命令泵 + 20 ms 播放节拍。
#[expect(
    clippy::needless_pass_by_value,
    reason = "三个值都移交给 std::thread::spawn 的 'static 闭包，必须按值持有"
)]
fn playback_thread(rx: Receiver<Command>, volume: Arc<AtomicU32>, stats: Arc<Stats>) {
    // 把线程登记到 MMCSS 的 "Audio" 类：播放节拍与 WASAPI 回调节奏更稳。
    let _mmcss = MmcssGuard::new();

    let mut decoder = match Decoder::new(SAMPLE_RATE, CHANNELS) {
        Ok(decoder) => decoder,
        Err(err) => {
            tracing::warn!(error = %err, "音频解码器创建失败，播放线程退出");
            return;
        }
    };
    let mut jitter = JitterBuffer::new(JitterConfig::default());
    let mut render: Option<Render> = None;
    let mut next_open_at = Instant::now();
    let mut pcm = vec![0.0_f32; FRAME_FLOATS];
    let mut next_tick = Instant::now() + Duration::from_millis(u64::from(FRAME_MS));

    loop {
        // 命令泵：最多等 2 ms，保证 20 ms 节拍不被长阻塞。
        let wait = next_tick
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(2));
        match rx.recv_timeout(wait) {
            Ok(Command::Packet { seq, data }) => {
                stats.received.fetch_add(1, Ordering::Relaxed);
                match jitter.push(seq, data, Instant::now()) {
                    crate::jitter::Push::Late => {
                        stats.late.fetch_add(1, Ordering::Relaxed);
                    }
                    crate::jitter::Push::Trimmed => {
                        stats.overflow.fetch_add(1, Ordering::Relaxed);
                    }
                    crate::jitter::Push::Stored => {}
                }
                continue;
            }
            Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        // 设备打开/重建（失败退避，不退出线程）。
        let now = Instant::now();
        if render.is_none() && now >= next_open_at {
            match Render::open_default_output() {
                Ok(opened) => {
                    tracing::info!("音频播放设备已打开");
                    render = Some(opened);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "打开音频播放设备失败，2 秒后重试");
                    next_open_at = now + RENDER_RETRY;
                }
            }
        }

        if Instant::now() < next_tick {
            continue;
        }
        // 一次最多补两帧：线程被长时间挂起后不做「追赶风暴」，剩余落后直接丢弃。
        for _ in 0..2 {
            if Instant::now() < next_tick {
                break;
            }
            next_tick += Duration::from_millis(u64::from(FRAME_MS));
            let Some(device) = render.as_mut() else {
                // 没有设备时不消费抖动缓冲（否则会白白丢光），等设备恢复。
                break;
            };
            match jitter.pull(Instant::now()) {
                Pull::Packet(data) => match decoder.decode(&data, &mut pcm) {
                    Ok(frames) => {
                        apply_volume(&mut pcm[..frames * usize::from(CHANNELS)], &volume);
                        if let Err(err) = device.write_block(&pcm) {
                            tracing::warn!(error = %err, "音频播放设备失效，2 秒后重建");
                            render = None;
                            next_open_at = Instant::now() + RENDER_RETRY;
                            break;
                        }
                    }
                    Err(err) => tracing::warn!(error = %err, "Opus 解码失败"),
                },
                Pull::Missing => {
                    // 丢包隐藏：用解码器内部状态生成一个替代帧，避免爆音/断音。
                    if let Ok(frames) = decoder.decode_plc(&mut pcm) {
                        apply_volume(&mut pcm[..frames * usize::from(CHANNELS)], &volume);
                        if let Err(err) = device.write_block(&pcm) {
                            tracing::warn!(error = %err, "音频播放设备失效，2 秒后重建");
                            render = None;
                            next_open_at = Instant::now() + RENDER_RETRY;
                            break;
                        }
                    }
                }
                Pull::Idle => {}
            }
        }
        if next_tick < Instant::now() {
            // 落后超过一拍：重置节拍，避免下一次醒来连续补帧。
            next_tick = Instant::now() + Duration::from_millis(u64::from(FRAME_MS));
        }
        stats
            .underruns
            .store(jitter.underruns(), Ordering::Relaxed);
        stats.buffer_ms.store(jitter.target_ms(), Ordering::Relaxed);
    }
}

/// 应用播放增益（百分比 → 线性增益），并钳到 `[-1, 1]`。
#[expect(
    clippy::cast_precision_loss,
    reason = "音量百分比只有 0–200，转 f32 无损"
)]
fn apply_volume(pcm: &mut [f32], volume: &AtomicU32) {
    let percent = volume.load(Ordering::Relaxed);
    if percent == 100 {
        return;
    }
    let gain = percent as f32 / 100.0;
    for sample in pcm {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

/// MMCSS（`avrt.dll`）：把播放线程登记为 "Audio" 任务类；失败时静默降级。
struct MmcssGuard {
    handle: Option<windows::Win32::Foundation::HANDLE>,
}

impl MmcssGuard {
    fn new() -> Self {
        let mut index = 0_u32;
        // SAFETY: 任务名是静态 NUL 结尾宽字符串；失败只返回错误码，不影响线程。
        let handle = unsafe {
            windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(
                windows::core::w!("Audio"),
                std::ptr::addr_of_mut!(index),
            )
        }
        .ok();
        Self { handle }
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: 句柄由 `AvSetMmThreadCharacteristicsW` 返回，且只还原一次。
            let _ = unsafe {
                windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(handle)
            };
        }
    }
}
