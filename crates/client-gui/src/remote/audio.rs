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
use rust_tunnel_win_audio::source::{has_output_device, LoopbackEncoder};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 构造本端音频端点（被控端采集源 + 控制端播放器）。
#[must_use]
pub fn endpoints() -> AudioEndpoints {
    let source = WinAudioSource::new().map(|source| Arc::new(source) as Arc<dyn AudioSource>);
    let sink = WinAudioSink::start().map(|sink| Arc::new(sink) as Arc<dyn AudioSink>);
    AudioEndpoints { source, sink }
}

/// 被控端采集源：WASAPI loopback + Opus。
pub struct WinAudioSource {
    /// `None` = 编码器创建失败（libopus 不可用）。
    encoder: Mutex<Option<LoopbackEncoder>>,
    /// 最近一次探测到的设备可用性。
    available: AtomicBool,
}

impl WinAudioSource {
    fn new() -> Option<Self> {
        let encoder = LoopbackEncoder::new().ok()?;
        Some(Self {
            encoder: Mutex::new(Some(encoder)),
            available: AtomicBool::new(has_output_device()),
        })
    }
}

impl AudioSource for WinAudioSource {
    fn capability(&self) -> AudioState {
        let available = self.available.load(Ordering::Relaxed);
        AudioState {
            enabled: false,
            available,
            reason: (!available).then(|| "没有可用的音频回放设备".to_owned()),
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
        let encoder = guard
            .as_mut()
            .ok_or_else(|| RemoteError::Platform("音频编码器不可用".to_owned()))?;
        encoder
            .start()
            .map_err(|err| RemoteError::Platform(err.to_string()))?;
        self.available.store(true, Ordering::Relaxed);
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
pub struct WinAudioSink {
    playback: Playback,
}

impl WinAudioSink {
    fn start() -> Option<Self> {
        Playback::start().ok().map(|playback| Self { playback })
    }
}

impl AudioSink for WinAudioSink {
    fn on_packet(&self, packet: ReceivedAudio) {
        self.playback.push(packet.seq, packet.data);
    }

    fn on_status(&self, _state: &AudioState) {
        // 播放侧不需要对状态做额外动作：对端停发后队列自然排空。
        // 本地静音走工具条的 `set_volume(0)` / `SetAudioEnabled{false}`。
    }

    fn set_volume(&self, percent: u32) {
        self.playback.set_volume(percent);
    }

    fn snapshot(&self) -> AudioPlaybackStats {
        let stats = self.playback.snapshot();
        AudioPlaybackStats {
            received: stats.received,
            dropped: stats.dropped,
            underruns: stats.underruns,
            buffer_ms: stats.buffer_ms,
        }
    }
}
