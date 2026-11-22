//! 极简 WAV 落盘（16-bit PCM），只给探针与真机验收用。

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// 把交错 `f32` PCM 写成 16-bit PCM 的 `.wav`。
///
/// # Errors
/// 文件创建/写入失败，或数据长度超过 WAV 头的 32 位上限时返回 [`io::Error`]。
#[expect(
    clippy::cast_possible_truncation,
    reason = "采样值先 clamp 到 [-1, 1] 再乘 i16::MAX，转换范围有界"
)]
pub fn write_i16(
    path: impl AsRef<Path>,
    pcm: &[f32],
    sample_rate: u32,
    channels: u16,
) -> io::Result<()> {
    let data_len = u32::try_from(pcm.len().saturating_mul(2)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "PCM 数据超过 WAV 的 32 位长度上限",
        )
    })?;
    let block_align = channels.saturating_mul(2);
    let byte_rate = sample_rate.saturating_mul(u32::from(block_align));

    let mut file = File::create(path)?;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data_len).to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16_u32.to_le_bytes()); // fmt 块长度
    header.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&16_u16.to_le_bytes()); // 位深
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_len.to_le_bytes());
    file.write_all(&header)?;

    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for &sample in pcm {
        let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    file.write_all(&bytes)?;
    file.flush()
}
