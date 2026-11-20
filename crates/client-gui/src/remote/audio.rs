//! 音频平台适配：把 `rust-tunnel-win-audio` 的采集/播放包装成引擎 trait。
//!
//! 只在 Windows 编译。macOS/Linux 的 `audio_endpoints()` 在 `mod.rs` 里返回空——引擎会向
//! 对端上报「本机不支持音频」，控制端据此把扬声器按钮置灰。

use crate::remote::AudioEndpoints;
use rust_tunnel_remote::{
    AudioCodec, AudioPlaybackStats, AudioSink, AudioSource, AudioState, EncodedAudio,
    ReceivedAudio, RemoteError, RemoteResult,
};
use rust_tunnel_win_audio::playback::Playback;
use rust_tunnel_win_audio::source::LoopbackEncoder;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// 构造本端音频端点（被控端采集源 + 控制端播放器）。
///
/// **播放器不在这里启动**（见 [`WinAudioSink`]）：无人值守实例没有控制器、永远收不到
/// `AudioPacket`，却会在启动时白开一个 WASAPI 线程与默认输出设备。
#[must_use]
pub fn endpoints() -> AudioEndpoints {
    let source = Some(Arc::new(WinAudioSource::new()) as Arc<dyn AudioSource>);
    let sink = Some(Arc::new(WinAudioSink::new()) as Arc<dyn AudioSink>);
    AudioEndpoints { source, sink }
}

/// 被控端采集源：WASAPI loopback + Opus。
pub struct WinAudioSource {
    /// `None` = 编码器创建失败（通常是 MSVC 构建没有内置 libopus）。
    encoder: Mutex<Option<LoopbackEncoder>>,
    /// 编码器创建失败的原因（直接进 `AudioStatus.reason`，避免只报「不支持音频」）。
    init_error: Option<String>,
}

impl WinAudioSource {
    fn new() -> Self {
        let (encoder, init_error) = match LoopbackEncoder::new() {
            Ok(encoder) => (Some(encoder), None),
            Err(err) => (None, Some(err.to_string())),
        };
        Self {
            encoder: Mutex::new(encoder),
            init_error,
        }
    }
}

impl AudioSource for WinAudioSource {
    fn capability(&self) -> AudioState {
        let encoder_ok = self
            .encoder
            .lock()
            .is_ok_and(|encoder| encoder.is_some());
        // **乐观上报**：不在会话开始前探测设备。无人值守实例经常在音频服务就绪之前
        // 启动；一次探测失败就把 `available` 置 false，会让控制端永远不发
        // `SetAudioEnabled`，连重试的机会都没有。真正的设备检查交给 `start()`——它失败
        // 时上报带原因的状态，并由采集任务每 2 秒重试。
        AudioState {
            enabled: false,
            available: encoder_ok,
            reason: if encoder_ok {
                None
            } else {
                self.init_error
                    .clone()
                    .or_else(|| Some("音频编码器不可用".to_owned()))
            },
            codec: AudioCodec::Opus,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 96,
        }
    }

    fn start(&self) -> RemoteResult<()> {
        let mut guard = self
            .encoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let encoder = guard.as_mut().ok_or_else(|| {
            RemoteError::Platform(
                self.init_error
                    .clone()
                    .unwrap_or_else(|| "音频编码器不可用".to_owned()),
            )
        })?;
        encoder
            .start()
            .map_err(|err| RemoteError::Platform(err.to_string()))?;
        Ok(())
    }

    fn read_packet(&self, stop: &AtomicBool) -> RemoteResult<Option<EncodedAudio>> {
        let mut guard = self
            .encoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(encoder) = guard.as_mut() else {
            return Ok(None);
        };
        match encoder.read(stop) {
            Ok(Some(frame)) => Ok(Some(EncodedAudio {
                data: frame.data,
                capture_ts_us: frame.capture_ts_us,
            })),
            Ok(None) => Ok(None),
            Err(err) => Err(RemoteError::Platform(err.to_string())),
        }
    }

    fn stop(&self) {
        let mut guard = self
            .encoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(encoder) = guard.as_mut() {
            encoder.stop();
        }
    }
}

/// 控制端播放器：抖动缓冲 + Opus 解码 + WASAPI 输出（独立线程）。
///
/// # 为什么懒启动
///
/// 构造本端音频端点发生在**进程启动**（`build_remote_manager`），而在无人值守实例里
/// 它是纯浪费：`Playback::start()` 会立刻起一个 `rt-audio-play` 线程，那个线程第一拍就
/// 去 `Render::open_default_output()`——实例没有控制器、永远收不到 `AudioPacket`，这个
/// 线程与 WASAPI 客户端会一直空转到进程退出。控制端则相反：只有真的收到第一段音频包
/// 才有播放的意义（`SetAudioEnabled{true}` 之后才有包）。
///
/// # 取舍：为什么在线程里同步启动而不是丢给后台线程
///
/// `Playback::start()` 本身**不打开设备**（设备在播放线程的首拍里打开），它只建一个
/// 通道并 `thread::spawn`，耗时是几十微秒量级的一次性开销；再套一层后台线程反而要多
/// 一次线程创建、还要处理「启动完成前的包怎么办」。真正的设备枚举/打开仍在播放线程上，
/// 不会占用引擎的 async 线程（`AudioSink::on_packet` 的非阻塞要求仍然成立）。
pub struct WinAudioSink {
    /// 惰性创建的播放器。`None` = 尚未收到第一个音频包。
    state: Mutex<SinkState>,
    /// 播放音量（0–200）。
    ///
    /// 引擎在**构造时**就会调一次 `set_audio_volume`（见 `build_remote_manager`），那时
    /// 播放器还不存在，所以音量必须独立保存、在真正启动时回填。
    volume: AtomicU32,
}

/// [`WinAudioSink`] 的惰性状态。
#[derive(Default)]
struct SinkState {
    /// 播放器（第一个包到达时创建）。
    playback: Option<Playback>,
    /// 启动已失败过：只记一次日志，之后丢包，不再每包重试。
    ///
    /// `Playback::start()` 失败只可能是线程创建失败（解码器创建失败发生在播放线程内部）
    /// ——那通常是资源耗尽，按包重试只会变成日志/线程创建风暴。
    start_failed: bool,
}

impl WinAudioSink {
    fn new() -> Self {
        Self {
            state: Mutex::new(SinkState::default()),
            volume: AtomicU32::new(100),
        }
    }

    /// 确保播放器已启动；返回它是否可用。
    fn ensure_started(&self, state: &mut SinkState) -> bool {
        if state.playback.is_some() {
            return true;
        }
        if state.start_failed {
            return false;
        }
        match Playback::start() {
            Ok(playback) => {
                // 回填构造期设置过的音量：`set_volume` 可能早于本函数很多。
                playback.set_volume(self.volume.load(Ordering::Relaxed));
                tracing::info!("首个音频包到达，音频播放器已启动");
                state.playback = Some(playback);
                true
            }
            Err(err) => {
                state.start_failed = true;
                tracing::warn!(error = %err, "音频播放器启动失败，本进程不再播放声音（音频包丢弃）");
                false
            }
        }
    }
}

impl AudioSink for WinAudioSink {
    fn on_packet(&self, packet: ReceivedAudio) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !self.ensure_started(&mut state) {
            return;
        }
        if let Some(playback) = state.playback.as_ref() {
            playback.push(packet.seq, packet.data);
        }
    }

    fn on_status(&self, _state: &AudioState) {
        // 播放侧不需要对状态做额外动作：对端停发后队列自然排空。
        // 本地静音走工具条的 `set_volume(0)` / `SetAudioEnabled{false}`。
    }

    fn set_volume(&self, percent: u32) {
        // 先记下来（启动前调用是常态），再在播放器存在时就地应用。
        self.volume.store(percent.min(200), Ordering::Relaxed);
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(playback) = state.playback.as_ref() {
            playback.set_volume(percent);
        }
    }

    fn snapshot(&self) -> AudioPlaybackStats {
        let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(playback) = guard.playback.as_ref() else {
            // 尚未播放：与「无播放能力」同口径的全 0（控制端 UI 不需要特判）。
            return AudioPlaybackStats::default();
        };
        let stats = playback.snapshot();
        AudioPlaybackStats {
            received: stats.received,
            dropped: stats.dropped,
            underruns: stats.underruns,
            buffer_ms: stats.buffer_ms,
        }
    }
}
