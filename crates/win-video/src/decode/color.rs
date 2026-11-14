//! NV12 / YV12 / IYUV → RGBA 转换（BT.709 limited range）。
//!
//! # 色彩常数来源
//!
//! 编码端（[`crate::nvenc`]）在 VUI 里显式打的是 **BT.709 + limited range**
//! （`colourMatrix = BT709`、`videoFullRangeFlag = 0`），因此解码端必须用同一套
//! 矩阵与范围，否则画面会发灰或偏色。本模块的定点常数由 ITU-R BT.709-6 的
//! 亮度方程与 limited range 缩放（Y: 16–235，Cb/Cr: 16–240）推出：
//!
//! - `R' = Y' + 2(1 - Kr)·V`
//! - `G' = Y' - 2(1 - Kb)·Kb/Kg·U - 2(1 - Kr)·Kr/Kg·V`
//! - `B' = Y' + 2(1 - Kb)·U`
//! - 其中 `Kr = 0.2126`、`Kb = 0.0722`、`Kg = 1 - Kr - Kb`；
//! - `Y' = (Y - 16) / 219`、`U = (Cb - 128) / 224`、`V = (Cr - 128) / 224`。
//!
//! 把这些系数乘 255（回到 8-bit）再乘 1024（定点移位），四舍五入后得到下面的
//! 整数常数。误差在 ±0.5 LSB 内，逐像素只需三次乘加，1080p 单核足够快。
//!
//! # 为什么不用 f32
//!
//! 控制器解码是软实时路径（1080p 每秒数十帧）；f32 版本每像素要做浮点乘加
//! 与饱和转换，在低端 CPU 上是可测的开销。定点版本与浮点版本的差异远小于
//! 4:2:0 本身的色度误差。

use crate::WinVideoError;

/// BT.709 limited range 的亮度黑电平。
const Y_OFFSET: i32 = 16;
/// BT.709 limited range 的色度零电平。
const C_OFFSET: i32 = 128;

/// `255 / 219 * 1024`，四舍五入。
const Y_SCALE: i32 = 1192;
/// `1.5748 * 255 / 224 * 1024`（R 的 V 系数）。
const R_V: i32 = 1836;
/// `0.1873 * 255 / 224 * 1024`（G 的 U 系数）。
const G_U: i32 = 218;
/// `0.4681 * 255 / 224 * 1024`（G 的 V 系数）。
const G_V: i32 = 546;
/// `1.8556 * 255 / 224 * 1024`（B 的 U 系数）。
const B_U: i32 = 2163;
/// 定点移位的舍入项（`1 << (SHIFT - 1)`）。
const ROUND: i32 = 512;
/// 定点小数位数。
const SHIFT: u32 = 10;

/// 把定点结果裁剪到 8-bit。
fn clip(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(0)
}

/// 校验 4:2:0 尺寸并转换成 `usize`。
fn dimensions(width: u32, height: u32) -> Result<(usize, usize), WinVideoError> {
    if width == 0 || height == 0 {
        return Err(WinVideoError::Decode(format!(
            "解码尺寸不能为 0：{width}x{height}"
        )));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(WinVideoError::Decode(format!(
            "4:2:0 解码要求宽高为偶数，收到 {width}x{height}"
        )));
    }
    let w = usize::try_from(width)
        .map_err(|_| WinVideoError::Decode("解码宽度无法转为 usize".to_owned()))?;
    let h = usize::try_from(height)
        .map_err(|_| WinVideoError::Decode("解码高度无法转为 usize".to_owned()))?;
    Ok((w, h))
}

/// 校验一个平面的 stride 与长度（避免后续逐像素索引越界）。
fn validate_plane(
    plane: &[u8],
    pitch: usize,
    rows: usize,
    min_row_bytes: usize,
    name: &str,
) -> Result<(), WinVideoError> {
    if pitch < min_row_bytes {
        return Err(WinVideoError::Decode(format!(
            "{name} 平面 stride {pitch} 小于行宽 {min_row_bytes}"
        )));
    }
    let needed = pitch
        .checked_mul(rows)
        .ok_or_else(|| WinVideoError::Decode(format!("{name} 平面大小溢出")))?;
    if plane.len() < needed {
        return Err(WinVideoError::Decode(format!(
            "{name} 平面过小：需要 {needed} 字节，实际 {}",
            plane.len()
        )));
    }
    Ok(())
}

/// 把两个相邻像素写进一个 8 字节块（`[RGBA, RGBA]`）。
///
/// 目标与源都是定长数组，编译器可以消掉逐像素边界检查；这是 1080p 转换
/// 能压进个位数毫秒的关键。
fn write_pair(target: &mut [u8; 8], luma: [u8; 2], chroma: (i32, i32, i32)) {
    let (r_chroma, g_chroma, b_chroma) = chroma;
    let base = luma_term(luma[0]);
    target[0] = clip((base + r_chroma) >> SHIFT);
    target[1] = clip((base + g_chroma) >> SHIFT);
    target[2] = clip((base + b_chroma) >> SHIFT);
    target[3] = 255;
    let base = luma_term(luma[1]);
    target[4] = clip((base + r_chroma) >> SHIFT);
    target[5] = clip((base + g_chroma) >> SHIFT);
    target[6] = clip((base + b_chroma) >> SHIFT);
    target[7] = 255;
}

/// 由 Cb/Cr 计算每像素共享的色度项（2x2 块内四个像素相同）。
fn chroma_terms(cb: u8, cr: u8) -> (i32, i32, i32) {
    let u = i32::from(cb) - C_OFFSET;
    let v = i32::from(cr) - C_OFFSET;
    (R_V * v + ROUND, -G_U * u - G_V * v + ROUND, B_U * u + ROUND)
}

/// 由 Y 计算亮度项。
fn luma_term(y: u8) -> i32 {
    Y_SCALE * (i32::from(y) - Y_OFFSET)
}

/// NV12（Y 平面 + 交织 UV 平面）→ 自顶向下 RGBA。
///
/// `y_pitch` / `uv_pitch` 是**字节** stride，可能大于宽度；不能假设等于宽度。
///
/// # Errors
/// 尺寸非偶数、stride 小于行宽或平面长度不足时返回 [`WinVideoError::Decode`]。
pub(crate) fn nv12_to_rgba(
    y: &[u8],
    uv: &[u8],
    y_pitch: usize,
    uv_pitch: usize,
    width: u32,
    height: u32,
    out: &mut Vec<u8>,
) -> Result<(), WinVideoError> {
    let (w, h) = dimensions(width, height)?;
    validate_plane(y, y_pitch, h, w, "Y")?;
    validate_plane(uv, uv_pitch, h / 2, w, "UV")?;

    out.clear();
    out.resize(w.saturating_mul(h).saturating_mul(4), 255);

    let row_bytes = w.saturating_mul(4);
    for row in (0..h).step_by(2) {
        let y0_start = row.saturating_mul(y_pitch);
        let y1_start = row.saturating_add(1).saturating_mul(y_pitch);
        let uv_start = (row / 2).saturating_mul(uv_pitch);
        let y0 = &y[y0_start..y0_start + w];
        let y1 = &y[y1_start..y1_start + w];
        let uv_row = &uv[uv_start..uv_start + w];
        let (out0, rest) = out[row.saturating_mul(row_bytes)..].split_at_mut(row_bytes);
        let out1 = &mut rest[..row_bytes];

        for (((uv, luma0), luma1), (target0, target1)) in uv_row
            .as_chunks::<2>()
            .0
            .iter()
            .zip(y0.as_chunks::<2>().0)
            .zip(y1.as_chunks::<2>().0)
            .zip(
                out0.as_chunks_mut::<8>()
                    .0
                    .iter_mut()
                    .zip(out1.as_chunks_mut::<8>().0),
            )
        {
            let terms = chroma_terms(uv[0], uv[1]);
            write_pair(target0, *luma0, terms);
            write_pair(target1, *luma1, terms);
        }
    }
    Ok(())
}

/// 平面 YUV 4:2:0（Y + 独立 U/V 平面）→ 自顶向下 RGBA。
///
/// `u` / `v` 由调用方按布局（YV12 为 V 在前，IYUV 为 U 在前）排好。
///
/// # Errors
/// 尺寸非偶数、stride 小于行宽或平面长度不足时返回 [`WinVideoError::Decode`]。
#[expect(
    clippy::too_many_arguments,
    reason = "三个平面 + 两个 stride + 尺寸 + 输出，逐项对应调用点，拆结构体反而更难核对"
)]
pub(crate) fn planar_to_rgba(
    luma: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_pitch: usize,
    chroma_pitch: usize,
    width: u32,
    height: u32,
    out: &mut Vec<u8>,
) -> Result<(), WinVideoError> {
    let (pixel_width, pixel_height) = dimensions(width, height)?;
    let chroma_width = pixel_width / 2;
    let chroma_rows = pixel_height / 2;
    validate_plane(luma, y_pitch, pixel_height, pixel_width, "Y")?;
    validate_plane(u_plane, chroma_pitch, chroma_rows, chroma_width, "U")?;
    validate_plane(v_plane, chroma_pitch, chroma_rows, chroma_width, "V")?;

    out.clear();
    out.resize(
        pixel_width.saturating_mul(pixel_height).saturating_mul(4),
        255,
    );

    let row_bytes = pixel_width.saturating_mul(4);
    for row in (0..pixel_height).step_by(2) {
        let y0_start = row.saturating_mul(y_pitch);
        let y1_start = row.saturating_add(1).saturating_mul(y_pitch);
        let chroma_start = (row / 2).saturating_mul(chroma_pitch);
        let y0 = &luma[y0_start..y0_start + pixel_width];
        let y1 = &luma[y1_start..y1_start + pixel_width];
        let u_row = &u_plane[chroma_start..chroma_start + chroma_width];
        let v_row = &v_plane[chroma_start..chroma_start + chroma_width];
        let (out0, rest) = out[row.saturating_mul(row_bytes)..].split_at_mut(row_bytes);
        let out1 = &mut rest[..row_bytes];

        for ((((&cb, &cr), luma0), luma1), (target0, target1)) in u_row
            .iter()
            .zip(v_row)
            .zip(y0.as_chunks::<2>().0)
            .zip(y1.as_chunks::<2>().0)
            .zip(
                out0.as_chunks_mut::<8>()
                    .0
                    .iter_mut()
                    .zip(out1.as_chunks_mut::<8>().0),
            )
        {
            let terms = chroma_terms(cb, cr);
            write_pair(target0, *luma0, terms);
            write_pair(target1, *luma1, terms);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_and_white_are_exact_limited_range_endpoints() {
        // 2x2 全黑：Y=16、Cb=Cr=128。
        let mut rgba = Vec::new();
        nv12_to_rgba(&[16, 16, 16, 16], &[128, 128], 2, 2, 2, 2, &mut rgba).unwrap_or_default();
        assert_eq!(
            rgba,
            vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]
        );
        // 2x2 全白：Y=235、Cb=Cr=128。
        nv12_to_rgba(&[235, 235, 235, 235], &[128, 128], 2, 2, 2, 2, &mut rgba).unwrap_or_default();
        assert_eq!(
            rgba,
            vec![255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn pitch_larger_than_width_is_honoured() {
        // 4 像素宽但 stride=8：每行后 4 字节是填充，不能读进来。
        let y = [
            16, 16, 16, 16, 0, 0, 0, 0, // 第 0 行：黑
            235, 235, 235, 235, 0, 0, 0, 0, // 第 1 行：白
        ];
        let uv = [128u8; 8];
        let mut rgba = Vec::new();
        nv12_to_rgba(&y, &uv, 8, 8, 4, 2, &mut rgba).unwrap_or_default();
        assert_eq!(&rgba[0..4], &[0, 0, 0, 255]);
        assert_eq!(&rgba[16..20], &[255, 255, 255, 255]);
    }

    #[test]
    fn odd_dimensions_are_rejected() {
        let mut rgba = Vec::new();
        assert!(nv12_to_rgba(&[16; 16], &[128; 8], 4, 4, 3, 2, &mut rgba).is_err());
    }
}
