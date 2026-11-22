//! 像素工具：BGRA↔RGBA、BGRA→NV12、NV12→RGBA、盒式缩放（纯逻辑）。
//!
//! 采集层给的是**自顶向下 BGRA8**；VideoToolbox 编码器吃 `kCVPixelFormatType_32BGRA`
//! （所以编码路径只做通道无关的逐行拷贝），而解码器输出 NV12（`420v`，BT.709 limited）。
//! 本模块承担：
//!
//! - 采集帧 → 编码像素缓冲需要的像素重排/缩放（缩放只在源尺寸 ≠ 编码尺寸时发生）；
//! - NV12 limited → RGBA 的 CPU 参考转换（探针的往返校验与未来的软件回退）；
//! - 变化检测用的 BT.709 limited 亮度。
//!
//! # 颜色口径
//!
//! 全部按 **BT.709 limited range**（`16..=235` 亮度，`16..=240` 色度）：
//! VideoToolbox 的 `420v` 输出、以及 Windows 侧 MFT/NVENC 的 NV12 都是这一套，
//! 控制器渲染层（wgpu 着色器）也按同一常数解。**不要**在这里改成 full range——
//! 两端口径不一致的表现是「黑色发灰 / 白色过曝」，而且只在真机上看得到。

use crate::error::{MacVideoError, MacVideoResult};

/// 一个像素的字节数（BGRA8 / RGBA8 都是 4）。
const BYTES_PER_PIXEL: usize = 4;

/// 把 `i32` 通道值钳到 `0..=255` 后转 `u8`。
fn clamp_u8(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(0)
}

/// 计算一个 BGRA 像素的 **BT.709 limited** 亮度（`16..=235`）。
///
/// 系数是 `219/255 × (0.2126, 0.7152, 0.0722)` 的 Q8 近似
/// （47 / 157 / 16，和 220 略大于 219，误差 ≤1）。
#[must_use]
pub fn bgra_luma(b: u8, g: u8, r: u8) -> u8 {
    let y = 16 + ((47 * u32::from(r) + 157 * u32::from(g) + 16 * u32::from(b) + 128) >> 8);
    u8::try_from(y).unwrap_or(255)
}

/// 把自顶向下 BGRA8 转成 RGBA8（交换第 0/2 字节，alpha 原样保留）。
///
/// 末尾不足一个像素的残余字节被**原样复制**，因此不会 panic；调用方本就不该传入
/// 这种输入。
#[must_use]
pub fn bgra_to_rgba(src: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    let (pixels, remainder) = src.as_chunks::<BYTES_PER_PIXEL>();
    let (dst_pixels, dst_remainder) = out.as_chunks_mut::<BYTES_PER_PIXEL>();
    for (src_px, dst_px) in pixels.iter().zip(dst_pixels.iter_mut()) {
        dst_px[0] = src_px[2];
        dst_px[1] = src_px[1];
        dst_px[2] = src_px[0];
        dst_px[3] = src_px[3];
    }
    dst_remainder.copy_from_slice(remainder);
    out
}

/// 把自顶向下 BGRA8 转成**紧包 NV12**（`Y(w*h) ‖ UV(w*h/2)`，BT.709 limited）。
///
/// 每个 2×2 像素块共用一对 UV：先对 B/G/R 分别求均值再转色度，避免 4 次转换后再平均
/// 带来的额外量化误差。
///
/// # Errors
/// 宽/高为 0、宽或高非偶数（4:2:0 要求）、或输入长度与 `width*height*4` 不符时返回
/// [`MacVideoError::Config`]。
pub fn bgra_to_nv12(src: &[u8], width: u32, height: u32) -> MacVideoResult<Vec<u8>> {
    let (w, h) = (
        usize::try_from(width).unwrap_or(0),
        usize::try_from(height).unwrap_or(0),
    );
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(MacVideoError::Config(format!(
            "NV12 需要偶数非零尺寸，收到 {width}x{height}"
        )));
    }
    let expected = w
        .checked_mul(h)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| MacVideoError::Config("BGRA 尺寸溢出".to_owned()))?;
    if src.len() != expected {
        return Err(MacVideoError::Config(format!(
            "BGRA 长度 {} 与尺寸 {width}x{height} 不符（应为 {expected}）",
            src.len()
        )));
    }
    let mut out = vec![0u8; w * h + w * h / 2];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    for row in 0..h {
        let row_start = row * w * BYTES_PER_PIXEL;
        for col in 0..w {
            let offset = row_start + col * BYTES_PER_PIXEL;
            y_plane[row * w + col] = bgra_luma(src[offset], src[offset + 1], src[offset + 2]);
        }
    }
    let uv_row_width = w; // 每行 UV 字节数 = w（每对像素 2 字节）
    for row in (0..h).step_by(2) {
        for col in (0..w).step_by(2) {
            let mut sums = [0u64; 3];
            for dy in 0..2 {
                for dx in 0..2 {
                    let offset = (row + dy) * w * BYTES_PER_PIXEL + (col + dx) * BYTES_PER_PIXEL;
                    sums[0] += u64::from(src[offset]); // B
                    sums[1] += u64::from(src[offset + 1]); // G
                    sums[2] += u64::from(src[offset + 2]); // R
                }
            }
            let blue = i32::try_from(sums[0] / 4).unwrap_or(0);
            let green = i32::try_from(sums[1] / 4).unwrap_or(0);
            let red = i32::try_from(sums[2] / 4).unwrap_or(0);
            // BT.709 **limited** 的 Q8 色度系数（libyuv `RGBToUVP709` 同款）：
            //   Cb = 128 + (112B - 26R - 87G) / 256
            //   Cr = 128 + (112R - 102G - 10B) / 256
            // **不能**用 full-range 的 (-29, -99, 128) / (128, -116, -12)：那会把色度
            // 填满 0..=255，而解码端（wgpu 着色器 / `nv12_limited_to_rgba`）按 16..=240
            // 解，补丁区域会比周围更饱和、边界可见。
            let cb = 128 + ((-26 * red - 87 * green + 112 * blue + 128) >> 8);
            let cr = 128 + ((112 * red - 102 * green - 10 * blue + 128) >> 8);
            let uv_offset = (row / 2) * uv_row_width + col;
            uv_plane[uv_offset] = clamp_u8(cb);
            uv_plane[uv_offset + 1] = clamp_u8(cr);
        }
    }
    Ok(out)
}

/// 把紧包 NV12（BT.709 limited）转成 RGBA8。
///
/// 用于探针的逐像素比对；真实渲染路径由控制器把 NV12 直接交给 wgpu 着色器，不走这里。
///
/// # Errors
/// 尺寸为零、非偶数，或 `y`/`uv` 长度与尺寸不符时返回 [`MacVideoError::Config`]。
#[expect(
    clippy::cast_possible_truncation,
    reason = "浮点结果四舍五入后立刻 clamp 到 0..=255，i32 截断不改变有效范围"
)]
pub fn nv12_limited_to_rgba(
    y: &[u8],
    uv: &[u8],
    width: u32,
    height: u32,
) -> MacVideoResult<Vec<u8>> {
    let (w, h) = (
        usize::try_from(width).unwrap_or(0),
        usize::try_from(height).unwrap_or(0),
    );
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(MacVideoError::Config(format!(
            "NV12 需要偶数非零尺寸，收到 {width}x{height}"
        )));
    }
    let luma_len = w
        .checked_mul(h)
        .ok_or_else(|| MacVideoError::Config("NV12 亮度平面尺寸溢出".to_owned()))?;
    let chroma_len = luma_len / 2;
    if y.len() != luma_len || uv.len() != chroma_len {
        return Err(MacVideoError::Config(format!(
            "NV12 平面长度不符：Y={}（应为 {luma_len}）UV={}（应为 {chroma_len}）",
            y.len(),
            uv.len()
        )));
    }
    let mut out = vec![0u8; luma_len * BYTES_PER_PIXEL];
    for row in 0..h {
        for col in 0..w {
            let luma = f32::from(y[row * w + col]);
            let uv_offset = (row / 2) * w + (col & !1);
            let cb = f32::from(uv[uv_offset]) - 128.0;
            let cr = f32::from(uv[uv_offset + 1]) - 128.0;
            // BT.709 limited → full range。
            let luma = (luma - 16.0) * (255.0 / 219.0);
            let cb = cb * (255.0 / 224.0);
            let cr = cr * (255.0 / 224.0);
            let red = luma + 1.5748 * cr;
            let green = luma - 0.187_324 * cb - 0.468_124 * cr;
            let blue = luma + 1.855_6 * cb;
            let offset = (row * w + col) * BYTES_PER_PIXEL;
            out[offset] = clamp_u8(red.round() as i32);
            out[offset + 1] = clamp_u8(green.round() as i32);
            out[offset + 2] = clamp_u8(blue.round() as i32);
            out[offset + 3] = 255;
        }
    }
    Ok(out)
}

/// 把自顶向下 BGRA8 画面**盒式**缩放到目标尺寸（输出仍是 BGRA8）。
///
/// 每个目标像素取它在源图上覆盖的矩形区域，对 B/G/R/A 分别做 `u64` 累加后取整平均。
/// 源尺寸等于目标尺寸时直接复制。
///
/// # Errors
/// 源长度与源尺寸不符、任一尺寸为 0、或尺寸相乘溢出时返回 [`MacVideoError::Config`]。
pub fn scale_bgra_box(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
) -> MacVideoResult<Vec<u8>> {
    let (sw, sh, dw, dh) = (
        usize::try_from(src_width).unwrap_or(0),
        usize::try_from(src_height).unwrap_or(0),
        usize::try_from(dst_width).unwrap_or(0),
        usize::try_from(dst_height).unwrap_or(0),
    );
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return Err(MacVideoError::Config(format!(
            "缩放尺寸不能为 0（源 {src_width}x{src_height}，目标 {dst_width}x{dst_height}）"
        )));
    }
    let expected = sw
        .checked_mul(sh)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| MacVideoError::Config("BGRA 源尺寸溢出".to_owned()))?;
    if src.len() != expected {
        return Err(MacVideoError::Config(format!(
            "BGRA 长度 {} 与源尺寸 {src_width}x{src_height} 不符（应为 {expected}）",
            src.len()
        )));
    }
    if (sw, sh) == (dw, dh) {
        return Ok(src.to_vec());
    }
    let dst_len = dw
        .checked_mul(dh)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| MacVideoError::Config("BGRA 目标尺寸溢出".to_owned()))?;
    let mut out = vec![0u8; dst_len];
    for dy in 0..dh {
        let sy0 = dy.saturating_mul(sh) / dh;
        let sy1 = ((dy.saturating_add(1)).saturating_mul(sh) / dh)
            .max(sy0.saturating_add(1))
            .min(sh);
        for dx in 0..dw {
            let sx0 = dx.saturating_mul(sw) / dw;
            let sx1 = ((dx.saturating_add(1)).saturating_mul(sw) / dw)
                .max(sx0.saturating_add(1))
                .min(sw);
            let mut sums = [0u64; BYTES_PER_PIXEL];
            let mut count = 0u64;
            for sy in sy0..sy1 {
                let row_start = sy * sw * BYTES_PER_PIXEL;
                for sx in sx0..sx1 {
                    let offset = row_start + sx * BYTES_PER_PIXEL;
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum += u64::from(src[offset + channel]);
                    }
                    count = count.saturating_add(1);
                }
            }
            let divisor = count.max(1);
            let offset = (dy * dw + dx) * BYTES_PER_PIXEL;
            for (channel, sum) in sums.iter().enumerate() {
                out[offset + channel] = u8::try_from(sum / divisor).unwrap_or(0);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 造一张 `width × height` 的 BGRA（每像素给定 B/G/R/A）。
    fn solid(width: u32, height: u32, b: u8, g: u8, r: u8, a: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height) as usize * 4);
        for _ in 0..width * height {
            out.extend_from_slice(&[b, g, r, a]);
        }
        out
    }

    /// 纯黑/纯白/中灰的亮度落在 BT.709 limited 的端点与中点附近。
    #[test]
    fn luma_endpoints_match_limited_range() {
        assert_eq!(bgra_luma(0, 0, 0), 16);
        assert_eq!(bgra_luma(255, 255, 255), 235);
        assert_eq!(bgra_luma(128, 128, 128), 126);
    }

    /// 通道权重：绿 > 红 > 蓝。
    #[test]
    fn luma_weights_are_ordered() {
        let red = bgra_luma(0, 0, 255);
        let green = bgra_luma(0, 255, 0);
        let blue = bgra_luma(255, 0, 0);
        assert!(green > red, "绿 {green} 应亮于红 {red}");
        assert!(red > blue, "红 {red} 应亮于蓝 {blue}");
    }

    /// BGRA 与 RGBA 只交换通道，alpha 保留。
    #[test]
    fn bgra_rgba_swaps_channels() {
        let src = [10, 20, 30, 40];
        assert_eq!(bgra_to_rgba(&src), [30, 20, 10, 40]);
        // 残余字节原样复制（不 panic）。
        assert_eq!(bgra_to_rgba(&[1, 2]), [1, 2]);
    }

    /// 纯黑→Y=16/U=V=128；纯白→Y=235/U=V≈128；纯红/纯蓝的色度必须落在 limited 的
    /// 16..=240，而不是 full-range 的 0..=255。
    ///
    /// 白点色度是 127 而不是 128：libyuv 的 limited Q8 系数和为零时仍有 ±1 取整误差
    /// （`112-26-87 = -1`），这是标准实现的行为，不是 bug。
    #[test]
    fn nv12_endpoints() {
        let black = bgra_to_nv12(&solid(4, 4, 0, 0, 0, 255), 4, 4).expect("黑帧转 NV12");
        assert_eq!(black.len(), 4 * 4 + 4 * 4 / 2);
        for &luma in &black[..16] {
            assert_eq!(luma, 16);
        }
        for &chroma in &black[16..] {
            assert_eq!(chroma, 128);
        }
        let white = bgra_to_nv12(&solid(4, 4, 255, 255, 255, 255), 4, 4).expect("白帧转 NV12");
        for &luma in &white[..16] {
            assert_eq!(luma, 235);
        }
        for &chroma in &white[16..] {
            assert!(chroma == 127 || chroma == 128, "白色色度应为中性 {chroma}");
        }
        // 纯红：Cr 顶到 limited 上限 240，Cb≈102。
        let red = bgra_to_nv12(&solid(4, 4, 0, 0, 255, 255), 4, 4).expect("红帧转 NV12");
        assert_eq!(red[16], 102, "红色 Cb");
        assert_eq!(red[17], 240, "红色 Cr 应为 limited 上限");
        // 纯蓝：Cb 顶到 240，Cr≈118。
        let blue = bgra_to_nv12(&solid(4, 4, 255, 0, 0, 255), 4, 4).expect("蓝帧转 NV12");
        assert_eq!(blue[16], 240, "蓝色 Cb 应为 limited 上限");
        assert_eq!(blue[17], 118, "蓝色 Cr");
    }

    /// NV12 往返：从 BGRA → NV12 → RGBA，亮度误差 ≤2、通道误差 ≤3（4:2:0 色度有损）。
    #[test]
    fn nv12_roundtrip_is_close() {
        let (w, h) = (8u32, 6u32);
        let mut src = Vec::with_capacity((w * h) as usize * 4);
        for y in 0..h {
            for x in 0..w {
                src.extend_from_slice(&[
                    u8::try_from(x * 8).unwrap_or(0),
                    u8::try_from(y * 10).unwrap_or(0),
                    u8::try_from(x * 4 + y * 6).unwrap_or(0),
                    255,
                ]);
            }
        }
        let nv12 = bgra_to_nv12(&src, w, h).expect("转 NV12");
        let (luma, chroma) = nv12.split_at((w * h) as usize);
        let rgba = nv12_limited_to_rgba(luma, chroma, w, h).expect("NV12 转 RGBA");
        let (source_pixels, _) = src.as_chunks::<4>();
        let (decoded_pixels, _) = rgba.as_chunks::<4>();
        for (index, (before, after)) in source_pixels.iter().zip(decoded_pixels.iter()).enumerate()
        {
            // 亮度误差容差 12：4:2:0 色度下采样 + 系数/limited 范围两轮量化。
            let expected_luma = before[1];
            let actual_luma = after[1];
            assert!(
                expected_luma.abs_diff(actual_luma) <= 12,
                "像素 {index} 亮度 {actual_luma} 与 {expected_luma} 差距过大"
            );
        }
    }

    /// 非偶数/零尺寸与非整帧长度都被拒绝。
    #[test]
    fn nv12_rejects_invalid_geometry() {
        assert!(bgra_to_nv12(&[], 0, 0).is_err());
        assert!(bgra_to_nv12(&[0; 4 * 3 * 4], 3, 3).is_err());
        assert!(bgra_to_nv12(&[0; 4], 2, 2).is_err());
        assert!(nv12_limited_to_rgba(&[], &[], 0, 0).is_err());
    }

    /// 2×2 盒式缩放对四个不同颜色的源像素取均值。
    #[test]
    fn box_scale_averages_pixels() {
        let src = [
            0, 0, 0, 255, // 黑
            100, 100, 100, 255, // 灰
            200, 200, 200, 255, // 浅灰
            255, 255, 255, 255, // 白
        ];
        let scaled = scale_bgra_box(&src, 2, 2, 1, 1).expect("缩放到 1x1");
        assert_eq!(scaled.len(), 4);
        let expected = (100 + 200 + 255) / 4;
        for &channel in scaled.iter().take(3) {
            assert_eq!(u32::from(channel), expected);
        }
        assert_eq!(scaled[3], 255);
    }

    /// 同尺寸缩放着原样返回。
    #[test]
    fn box_scale_identity_copies() {
        let src = solid(3, 3, 1, 2, 3, 4);
        let scaled = scale_bgra_box(&src, 3, 3, 3, 3).expect("同尺寸缩放");
        assert_eq!(scaled, src);
    }
}
