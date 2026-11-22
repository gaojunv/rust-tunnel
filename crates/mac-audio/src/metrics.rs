//! 合成 PCM 的简单度量：供探针与单测报告采集/往返质量。

/// 均方根（0.0–1.0 量级）。
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "PCM 长度远小于 2^53、结果回到 f32 只有报告精度要求"
)]
pub fn rms(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / pcm.len() as f64).sqrt() as f32
}

/// 峰值绝对值。
#[must_use]
pub fn peak(pcm: &[f32]) -> f32 {
    pcm.iter().fold(0.0_f32, |acc, &s| acc.max(s.abs()))
}

/// 两段等长 PCM 的平均绝对差（往返编解码的失真读数）。
///
/// 长度不一致时只比较较短的一段；任一段为空返回 0。
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "PCM 长度远小于 2^53、结果回到 f32 只有报告精度要求"
)]
pub fn mad(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let sum: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| f64::from((x - y).abs()))
        .sum();
    sum as f32 / n as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_sine_is_amplitude_over_sqrt_two() {
        let pcm: Vec<f32> = (0..48_000)
            .map(|i| {
                let phase = i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU;
                phase.sin() * 0.5
            })
            .collect();
        let value = rms(&pcm);
        assert!(
            (value - 0.5 / std::f32::consts::SQRT_2).abs() < 1.0e-3,
            "{value}"
        );
        assert!((peak(&pcm) - 0.5).abs() < 1.0e-3);
    }

    #[test]
    fn mad_counts_average_absolute_difference() {
        assert!((mad(&[0.0, 1.0, -1.0], &[0.0, 0.5, -0.5]) - 1.0 / 3.0).abs() < 1.0e-6);
        assert_eq!(mad(&[], &[1.0]), 0.0);
    }
}
