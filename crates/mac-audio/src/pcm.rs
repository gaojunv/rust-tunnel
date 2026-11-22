//! ScreenCaptureKit 音频缓冲的**布局归一**：平面 / 交错、单声道 / 多声道 → 交错立体声。
//!
//! 这层是纯逻辑，不碰任何 Apple API，因此在 Windows 开发机上可以直接单测——
//! `capture` 模块只负责把 `AudioBufferList` 里的裸指针包成 `&[f32]` 再调用这里。
//!
//! ScreenCaptureKit 在 `channelCount = 2` 时通常给**两路单声道平面**（
//! `mNumberBuffers = 2`、每路 `mNumberChannels = 1`），但设备混音被系统做过
//! 归一之后也可能是**一路双声道交错**；两种都要接住。

use crate::error::{MacAudioError, MacAudioResult};

/// 把**平面（非交织）** `f32` PCM 追加成交错立体声。
///
/// - 1 个平面按单声道处理，左右各复制一份；
/// - ≥2 个平面取前两路（L/R），多余声道丢弃（桌面音频只取立体声对）；
/// - 每个平面至少要 `frames` 个采样点。
///
/// # Errors
/// 平面为空、声道数超过 [`MAX_PLANES`]、或某个平面长度不足 `frames` 时返回
/// [`MacAudioError::Platform`]。
pub fn append_planar(out: &mut Vec<f32>, planes: &[&[f32]], frames: usize) -> MacAudioResult<()> {
    if planes.is_empty() {
        return Err(MacAudioError::Platform(
            "SCK 音频帧没有任何声道平面".to_owned(),
        ));
    }
    if planes.len() > MAX_PLANES {
        return Err(MacAudioError::Platform(format!(
            "SCK 音频帧有 {} 个声道平面，超过上限 {MAX_PLANES}",
            planes.len()
        )));
    }
    for (index, plane) in planes.iter().enumerate() {
        if plane.len() < frames {
            return Err(MacAudioError::Platform(format!(
                "SCK 音频平面 {index} 只有 {} 个采样点，少于声明的 {frames} 帧",
                plane.len()
            )));
        }
    }
    out.reserve(frames.saturating_mul(2));
    if planes.len() == 1 {
        let mono = planes[0];
        for &sample in &mono[..frames] {
            out.push(sample);
            out.push(sample);
        }
        return Ok(());
    }
    let left = planes[0];
    let right = planes[1];
    for index in 0..frames {
        out.push(left[index]);
        out.push(right[index]);
    }
    Ok(())
}

/// 把**交错** `f32` PCM 追加成交错立体声。
///
/// - `channels == 1` 按单声道处理，左右各复制一份；
/// - `channels >= 2` 取前两个声道（L/R），多余声道丢弃；
/// - `samples` 至少要 `frames * channels` 个采样点。
///
/// # Errors
/// `channels == 0`、声道数超过 [`MAX_PLANES`]、或采样点不足时返回
/// [`MacAudioError::Platform`]。
pub fn append_interleaved(
    out: &mut Vec<f32>,
    samples: &[f32],
    channels: usize,
    frames: usize,
) -> MacAudioResult<()> {
    if channels == 0 {
        return Err(MacAudioError::Platform("SCK 音频帧的声道数为 0".to_owned()));
    }
    if channels > MAX_PLANES {
        return Err(MacAudioError::Platform(format!(
            "SCK 音频帧有 {channels} 个声道，超过上限 {MAX_PLANES}"
        )));
    }
    let needed = frames
        .checked_mul(channels)
        .ok_or_else(|| MacAudioError::Platform("SCK 音频帧长度溢出".to_owned()))?;
    if samples.len() < needed {
        return Err(MacAudioError::Platform(format!(
            "SCK 音频帧只有 {} 个采样点，少于声明的 {needed} 个（{frames} 帧 × {channels} 声道）",
            samples.len()
        )));
    }
    out.reserve(frames.saturating_mul(2));
    if channels == 1 {
        for &sample in &samples[..frames] {
            out.push(sample);
            out.push(sample);
        }
        return Ok(());
    }
    for index in 0..frames {
        let base = index * channels;
        out.push(samples[base]);
        out.push(samples[base + 1]);
    }
    Ok(())
}

/// 允许的最大声道数 / 平面数（7.1 是桌面音频的上限）。
const MAX_PLANES: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planar_stereo_is_interleaved() {
        let left = [0.1_f32, 0.2, 0.3];
        let right = [-0.1_f32, -0.2, -0.3];
        let mut out = Vec::new();
        append_planar(&mut out, &[&left, &right], 3).expect("平面转交错");
        assert_eq!(out, vec![0.1, -0.1, 0.2, -0.2, 0.3, -0.3]);
    }

    #[test]
    fn planar_mono_is_duplicated() {
        let mono = [0.5_f32, -0.5];
        let mut out = Vec::new();
        append_planar(&mut out, &[&mono], 2).expect("单声道复制");
        assert_eq!(out, vec![0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn interleaved_stereo_is_copied() {
        let samples = [1.0_f32, 2.0, 3.0, 4.0];
        let mut out = Vec::new();
        append_interleaved(&mut out, &samples, 2, 2).expect("交错复制");
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn interleaved_multichannel_takes_front_pair() {
        // 5.1：L R C LFE Ls Rs → 只取 L/R。
        let samples = [1.0_f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mut out = Vec::new();
        append_interleaved(&mut out, &samples, 6, 1).expect("5.1 取前两声道");
        assert_eq!(out, vec![1.0, 2.0]);
    }

    #[test]
    fn short_planes_and_zero_channels_are_errors() {
        let left = [0.1_f32];
        let right = [0.2_f32, 0.3];
        assert!(append_planar(&mut Vec::new(), &[&left, &right], 2).is_err());
        assert!(append_planar(&mut Vec::new(), &[], 1).is_err());
        assert!(append_interleaved(&mut Vec::new(), &[1.0], 0, 1).is_err());
        assert!(append_interleaved(&mut Vec::new(), &[1.0], 2, 1).is_err());
    }
}
