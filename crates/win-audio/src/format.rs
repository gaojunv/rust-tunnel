//! 固定音频口径与 WASAPI 格式描述。

#[cfg(windows)]
use windows::Win32::Media::Audio::{
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
#[cfg(windows)]
use windows::core::GUID;

/// 采样率（Hz）。Opus 只支持 8/12/16/24/48 kHz，48 kHz 是桌面音频的标准口径。
pub const SAMPLE_RATE: u32 = 48_000;

/// 声道数（立体声）。
pub const CHANNELS: u16 = 2;

/// 每个采样点的字节数（`f32`）。
pub const SAMPLE_BYTES: usize = 4;

/// 每个采样点的位数（`f32`）。
const SAMPLE_BITS: u16 = 32;

/// 每个采样点的字节数（WASAPI 结构体口径，`u16`）。
const SAMPLE_BYTES_U16: u16 = 4;

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

/// `WAVE_FORMAT_EXTENSIBLE` 的 tag 值。
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`（`{00000003-0000-0010-8000-00AA00389B71}`）。
#[cfg(windows)]
const SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

/// 构造 WASAPI 的 `WAVEFORMATEXTENSIBLE`：48 kHz / 立体声 / 32-bit float。
///
/// 采集与播放都用同一份格式；`AUTOCONVERTPCM` 会把它与设备混音格式之间的采样率、
/// 声道数、位深差异交给音频引擎处理。
#[cfg(windows)]
#[must_use]
pub(crate) fn wave_format() -> WAVEFORMATEXTENSIBLE {
    // nBlockAlign = 2 ch × 4 B = 8；nAvgBytesPerSec = 48000 × 8 = 384000。
    let block_align = CHANNELS * SAMPLE_BYTES_U16;
    let avg_bytes_per_sec = SAMPLE_RATE * u32::from(block_align);
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: avg_bytes_per_sec,
            nBlockAlign: block_align,
            wBitsPerSample: SAMPLE_BITS,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: SAMPLE_BITS,
        },
        // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        dwChannelMask: 0b11,
        SubFormat: SUBTYPE_IEEE_FLOAT,
    }
}
