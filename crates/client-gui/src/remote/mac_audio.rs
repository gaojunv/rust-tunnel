//! macOS 音频平台适配：把 `rust-tunnel-mac-audio` 的采集/播放包装成引擎 trait。
//!
//! 与 Windows 的 `audio` 模块同构：采集侧是 ScreenCaptureKit 系统声音 + Opus，
//! 播放侧是抖动缓冲 + Opus 解码 + AudioQueue。播放器**懒启动**（首个音频包到达才建
//! 线程/设备），无人值守实例不会白开一个输出设备。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rust_tunnel_mac_audio::format::{CHANNELS, OPUS_BITRATE_BPS, SAMPLE_RATE};
use rust_tunnel_mac_audio::{EncodedPacket, LoopbackEncoder, Playback, ReceivedPacket};
use rust_tunnel_remote::{
    AudioCodec, AudioPlaybackStats, AudioSink, AudioSource, AudioState, EncodedAudio,
    ReceivedAudio, RemoteError, RemoteResult,
};
use tracing::{info, warn};

use crate::remote::AudioEndpoints;

/// 构造本端音频端点（被控端采集源 + 控制端播放器）。
#[must_use]
pub fn endpoints() -> AudioEndpoints {
    let source = Some(Arc::new(MacAudioSource::new()) as Arc<dyn AudioSource>);
    let sink = Some(Arc::new(MacAudioSink::new()) as Arc<dyn AudioSink>);
    AudioEndpoints { source, sink }
}

/// 被控端采集源：ScreenCaptureKit 系统声音 + Opus。
struct MacAudioSource {
    /// `None` = 编码器创建失败（通常是没有 Opus feature / 初始化失败）。
    encoder: Option<LoopbackEncoder>,
    /// 创建失败的原因（直接进 `AudioStatus.reason`）。
    init_error: Option<String>,
}

impl MacAudioSource {
    fn new() -> Self {
        match LoopbackEncoder::new() {
            Ok(encoder) => Self {
                encoder: Some(encoder),
                init_error: None,
            },
            Err(err) => Self {
                encoder: None,
                init_error: Some(err.to_string()),
            },
        }
    }
}

impl AudioSource for MacAudioSource {
    fn capability(&self) -> AudioState {
        let Some(encoder) = self.encoder.as_ref() else {
            // `unavailable` 已经带 Opus/48k/立体声/96kbps 的口径。
            return AudioState::unavailable(
                self.init_error
                    .clone()
                    .unwrap_or_else(|| "音频编码器不可用".to_owned()),
            );
        };
        AudioState {
            enabled: encoder.is_started(),
            // **乐观上报**：设备/权限在 `start()` 里检查。与 Windows 侧同一理由——
            // 一次启动失败不该让控制端永远没有重试机会。
            available: true,
            reason: encoder.last_error(),
            codec: AudioCodec::Opus,
            sample_rate: SAMPLE_RATE,
            channels: channels(),
            bitrate_kbps: bitrate_kbps(),
        }
    }

    fn start(&self) -> RemoteResult<()> {
        let encoder = self.encoder.as_ref().ok_or_else(|| {
            RemoteError::Platform(
                self.init_error
                    .clone()
                    .unwrap_or_else(|| "音频编码器不可用".to_owned()),
            )
        })?;
        encoder
            .start()
            .map_err(|err| RemoteError::Platform(err.to_string()))
    }

    fn read_packet(&self, stop: &AtomicBool) -> RemoteResult<Option<EncodedAudio>> {
        let Some(encoder) = self.encoder.as_ref() else {
            return Ok(None);
        };
        let packet: Option<EncodedPacket> = encoder.read_packet(stop);
        Ok(packet.map(|packet| EncodedAudio {
            data: packet.data,
            capture_ts_us: packet.capture_ts_us,
        }))
    }

    fn stop(&self) {
        if let Some(encoder) = self.encoder.as_ref() {
            encoder.stop();
        }
    }
}

fn channels() -> u8 {
    u8::try_from(CHANNELS).unwrap_or(2)
}

fn bitrate_kbps() -> u16 {
    u16::try_from(OPUS_BITRATE_BPS / 1000).unwrap_or(96)
}

/// 控制端播放器：抖动缓冲 + Opus 解码 + AudioQueue（独立线程，懒启动）。
struct MacAudioSink {
    state: Mutex<SinkState>,
    /// 播放音量（0–200）。引擎可能在播放器创建前就调用 `set_volume`。
    volume: AtomicU32,
}

#[derive(Default)]
struct SinkState {
    playback: Option<Playback>,
    /// 启动已失败过：只记一次日志，之后丢包，不再每包重试。
    start_failed: bool,
}

impl MacAudioSink {
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
        match Playback::new().and_then(|playback| {
            playback.start()?;
            Ok(playback)
        }) {
            Ok(playback) => {
                playback.set_volume(self.volume.load(Ordering::Relaxed));
                info!("首个音频包到达，AudioQueue 播放器已启动");
                state.playback = Some(playback);
                true
            }
            Err(err) => {
                state.start_failed = true;
                warn!(error = %err, "AudioQueue 播放器启动失败，本进程不再播放声音（音频包丢弃）");
                false
            }
        }
    }
}

impl AudioSink for MacAudioSink {
    fn on_packet(&self, packet: ReceivedAudio) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.ensure_started(&mut state) {
            return;
        }
        if let Some(playback) = state.playback.as_ref() {
            playback.on_packet(ReceivedPacket {
                seq: packet.seq,
                capture_ts_us: packet.capture_ts_us,
                data: packet.data,
            });
        }
    }

    fn on_status(&self, _state: &AudioState) {
        // 播放侧无需动作：对端停发后队列自然排空。
    }

    fn set_volume(&self, percent: u32) {
        self.volume.store(percent.min(200), Ordering::Relaxed);
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(playback) = state.playback.as_ref() {
            playback.set_volume(percent);
        }
    }

    fn snapshot(&self) -> AudioPlaybackStats {
        let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(playback) = guard.playback.as_ref() else {
            return AudioPlaybackStats::default();
        };
        let stats = playback.stats();
        AudioPlaybackStats {
            received: stats.received,
            dropped: stats.dropped,
            underruns: stats.underruns,
            buffer_ms: stats.buffer_ms,
        }
    }
}
