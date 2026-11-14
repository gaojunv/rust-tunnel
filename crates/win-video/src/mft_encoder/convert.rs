//! RGBA → NV12（BT.709 limited range）CPU 转换。
//!
//! # 色彩常数来源
//!
//! 与 [`crate::nvenc`] 的 VUI 和解码侧 [`crate::decode::color`] 严格一致：
//! **BT.709 + limited range**（Y 16–235，Cb/Cr 16–240）。归一化公式
//! `R'G'B' → Y'CbCr` 乘 255 再乘 1024（定点）后取整：
//!
//! - `Y  = 16 + (187R + 629G + 63B + 512) >> 10`
//! - `Cb = 128 + (-103R - 347G + 450B + 512) >> 10`
//! - `Cr = 128 + (450R - 409G - 41B + 512) >> 10`
//!
//! 三组系数之和分别为 879 / 0 / 0：色度在纯灰度上严格落在 128，不引入 ±1
//! 色偏；1024 的定点精度也比 256 更接近真实系数（纯红 Cb 的误差 < 0.5 LSB）。
//!
//! # 4:2:0 下采样
//!
//! 每个 2x2 块共享一组 Cb/Cr。先对 4 个像素的 R/G/B 求平均再套公式，等价于
//! 先逐像素算 U/V 再平均（公式对 R/G/B 线性），但省掉三次转换。

use crate::WinVideoError;

/// 由单个像素的 R/G/B 计算 limited-range 亮度。
fn luma(red: u8, green: u8, blue: u8) -> u8 {
    let value =
        16 + ((187 * i32::from(red) + 629 * i32::from(green) + 63 * i32::from(blue) + 512) >> 10);
    u8::try_from(value.clamp(16, 235)).unwrap_or(16)
}

/// 色度裁剪到 limited range。
fn clip_chroma(value: i32) -> u8 {
    u8::try_from(value.clamp(16, 240)).unwrap_or(128)
}

/// 由 2x2 块的 RGB 平均值计算 Cb/Cr。
fn chroma(red: u32, green: u32, blue: u32) -> (u8, u8) {
    let red = i32::try_from(red).unwrap_or(0);
    let green = i32::try_from(green).unwrap_or(0);
    let blue = i32::try_from(blue).unwrap_or(0);
    let cb = 128 + ((-103 * red - 347 * green + 450 * blue + 512) >> 10);
    let cr = 128 + ((450 * red - 409 * green - 41 * blue + 512) >> 10);
    (clip_chroma(cb), clip_chroma(cr))
}

/// 把 `width × height` 的 RGBA8 转成 NV12（Y 平面 + 交织 UV 平面）。
///
/// # Errors
/// 尺寸为 0/奇数、或 `rgba` 长度不足时返回 [`WinVideoError::Encode`]。
pub(super) fn rgba_to_nv12(
    rgba: &[u8],
    width: u32,
    height: u32,
    out: &mut Vec<u8>,
) -> Result<(), WinVideoError> {
    if width == 0 || height == 0 {
        return Err(WinVideoError::Encode(format!(
            "RGBA→NV12 尺寸不能为 0：{width}x{height}"
        )));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(WinVideoError::Encode(format!(
            "4:2:0 编码要求宽高为偶数，收到 {width}x{height}"
        )));
    }
    let pixel_width = usize::try_from(width)
        .map_err(|_| WinVideoError::Encode("编码宽度无法转为 usize".to_owned()))?;
    let pixel_height = usize::try_from(height)
        .map_err(|_| WinVideoError::Encode("编码高度无法转为 usize".to_owned()))?;
    let pixels = pixel_width
        .checked_mul(pixel_height)
        .ok_or_else(|| WinVideoError::Encode("编码尺寸溢出".to_owned()))?;
    let needed = pixels
        .checked_mul(4)
        .ok_or_else(|| WinVideoError::Encode("RGBA 大小溢出".to_owned()))?;
    if rgba.len() < needed {
        return Err(WinVideoError::Encode(format!(
            "RGBA 数据不足：需要 {needed} 字节，收到 {}",
            rgba.len()
        )));
    }
    let nv12_len = pixels
        .checked_mul(3)
        .and_then(|value| value.checked_div(2))
        .ok_or_else(|| WinVideoError::Encode("NV12 大小溢出".to_owned()))?;
    out.clear();
    out.resize(nv12_len, 0);

    let row_bytes = pixel_width.saturating_mul(4);
    let (luma_plane, uv_plane) = out.split_at_mut(pixels);
    for row in (0..pixel_height).step_by(2) {
        let rgba0 = &rgba[row.saturating_mul(row_bytes)..(row + 1).saturating_mul(row_bytes)];
        let rgba1 = &rgba[(row + 1).saturating_mul(row_bytes)..(row + 2).saturating_mul(row_bytes)];
        let (luma0, luma1) = luma_plane
            [row.saturating_mul(pixel_width)..(row + 2).saturating_mul(pixel_width)]
            .split_at_mut(pixel_width);
        let uv = &mut uv_plane[(row / 2).saturating_mul(pixel_width)..(row / 2 + 1) * pixel_width];

        for (((chunk0, chunk1), luma_pair0), (luma_pair1, uv_pair)) in rgba0
            .as_chunks::<8>()
            .0
            .iter()
            .zip(rgba1.as_chunks::<8>().0)
            .zip(luma0.as_chunks_mut::<2>().0)
            .zip(
                luma1
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(uv.as_chunks_mut::<2>().0),
            )
        {
            // 每个 8 字节块是两个相邻的 RGBA 像素。
            let (r00, g00, b00) = (chunk0[0], chunk0[1], chunk0[2]);
            let (r01, g01, b01) = (chunk0[4], chunk0[5], chunk0[6]);
            let (r10, g10, b10) = (chunk1[0], chunk1[1], chunk1[2]);
            let (r11, g11, b11) = (chunk1[4], chunk1[5], chunk1[6]);

            luma_pair0[0] = luma(r00, g00, b00);
            luma_pair0[1] = luma(r01, g01, b01);
            luma_pair1[0] = luma(r10, g10, b10);
            luma_pair1[1] = luma(r11, g11, b11);

            let red = (u32::from(r00) + u32::from(r01) + u32::from(r10) + u32::from(r11) + 2) >> 2;
            let green =
                (u32::from(g00) + u32::from(g01) + u32::from(g10) + u32::from(g11) + 2) >> 2;
            let blue = (u32::from(b00) + u32::from(b01) + u32::from(b10) + u32::from(b11) + 2) >> 2;
            let (cb, cr) = chroma(red, green, blue);
            uv_pair[0] = cb;
            uv_pair[1] = cr;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_and_white_map_to_limited_range_endpoints() {
        let mut nv12 = Vec::new();
        // 2x2 黑。
        rgba_to_nv12(
            &[0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255],
            2,
            2,
            &mut nv12,
        )
        .unwrap_or_default();
        assert_eq!(&nv12[..4], &[16, 16, 16, 16]);
        assert_eq!(&nv12[4..6], &[128, 128]);
        // 2x2 白。
        rgba_to_nv12(
            &[
                255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            ],
            2,
            2,
            &mut nv12,
        )
        .unwrap_or_default();
        assert_eq!(&nv12[..4], &[235, 235, 235, 235]);
        assert_eq!(&nv12[4..6], &[128, 128]);
    }

    #[test]
    fn pure_red_uses_bt709_limited_range() {
        let mut nv12 = Vec::new();
        rgba_to_nv12(
            &[
                255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
            ],
            2,
            2,
            &mut nv12,
        )
        .unwrap_or_default();
        assert_eq!(nv12[0], 63);
        assert_eq!(nv12[4], 102); // Cb
        assert_eq!(nv12[5], 240); // Cr
    }

    #[test]
    fn odd_dimensions_are_rejected() {
        let mut nv12 = Vec::new();
        assert!(rgba_to_nv12(&[0; 64], 3, 2, &mut nv12).is_err());
        assert!(rgba_to_nv12(&[0; 8], 2, 2, &mut nv12).is_err());
    }
}
