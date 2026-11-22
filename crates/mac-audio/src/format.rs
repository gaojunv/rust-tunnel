//! 固定音频口径（与 `rust-tunnel-win-audio::format` 一致）。

/// 采样率（Hz）。Opus 只支持 8/12/16/24/48 kHz，48 kHz 是桌面音频的标准口径。
pub const SAMPLE_RATE: u32 = 48_000;

/// 声道数（立体声）。
pub const CHANNELS: u16 = 2;

/// 每个采样点的字节数（`f32`）。
pub const SAMPLE_BYTES: usize = 4;

/// 一帧的时长（毫秒）。Opus 允许 2.5/5/10/20/40/60 ms；20 ms 是延迟与开销的折中。
pub const FRAME_MS: u32 = 20;

/// 一帧的采样点数（每声道）。
pub const FRAME_FRAMES: usize = (SAMPLE_RATE / 1000 * FRAME_MS) as usize;

/// 一帧的采样点总数（交错后，含两个声道）。
pub const FRAME_FLOATS: usize = FRAME_FRAMES * CHANNELS as usize;

/// 一帧的 PCM 字节数（`f32` 交错）。
pub const FRAME_BYTES: usize = FRAME_FLOATS * SAMPLE_BYTES;

/// Opus 目标码率（bit/s）。固定 96 kbps 立体声，不随视频档位变化。
pub const OPUS_BITRATE_BPS: i32 = 96_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_geometry_matches_opus_20ms_stereo() {
        assert_eq!(FRAME_FRAMES, 960);
        assert_eq!(FRAME_FLOATS, 1920);
        assert_eq!(FRAME_BYTES, 7680);
    }
}
