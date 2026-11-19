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
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// 构造本端音频端点（被控端采集源 + 控制端播放器）。
#[must_use]
pub fn endpoints() -> AudioEndpoints {
    let source = Some(Arc::new(WinAudioSource::new()) as Arc<dyn AudioSource>);
    let sink = WinAudioSink::start().map(|sink| Arc::new(sink) as Arc<dyn AudioSink>);
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
