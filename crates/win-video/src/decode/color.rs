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

use crate::{worker_count, WinVideoError};

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

/// 可见区域（编码帧坐标系）。
///
/// 4:2:0 的融合裁剪要求 `x`/`y`/`width`/`height` 全部为偶数且落在编码帧内；
/// 任一条件不满足时调用方应回退到「整帧转换 + `crop_to_display`」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CropRect {
    /// 可见区域左边界（偶数）。
    pub x: u32,
    /// 可见区域上边界（偶数）。
    pub y: u32,
    /// 可见宽度（偶数）。
    pub width: u32,
    /// 可见高度（偶数）。
    pub height: u32,
}

/// 判断一个可见区域能否走融合裁剪（4:2:0 的偶数对齐 + 不越界）。
pub(crate) fn crop_is_supported(crop: CropRect, coded_width: u32, coded_height: u32) -> bool {
    let even = |value: u32| value.is_multiple_of(2);
    if crop.width == 0
        || crop.height == 0
        || !even(crop.x)
        || !even(crop.y)
        || !even(crop.width)
        || !even(crop.height)
        || !even(coded_width)
        || !even(coded_height)
    {
        return false;
    }
    crop.x
        .checked_add(crop.width)
        .is_some_and(|end| end <= coded_width)
        && crop
            .y
            .checked_add(crop.height)
            .is_some_and(|end| end <= coded_height)
}

/// 内部使用的 `usize` 版本裁剪区域。
#[derive(Debug, Clone, Copy)]
struct Crop {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl CropRect {
    /// 转为内部表示；越界或非偶数时返回错误（调用方通常已用
    /// [`crop_is_supported`] 预判并回退）。
    fn to_internal(self) -> Result<Crop, WinVideoError> {
        let x = usize::try_from(self.x)
            .map_err(|_| WinVideoError::Decode("裁剪 x 无法转为 usize".to_owned()))?;
        let y = usize::try_from(self.y)
            .map_err(|_| WinVideoError::Decode("裁剪 y 无法转为 usize".to_owned()))?;
        let width = usize::try_from(self.width)
            .map_err(|_| WinVideoError::Decode("裁剪宽度无法转为 usize".to_owned()))?;
        let height = usize::try_from(self.height)
            .map_err(|_| WinVideoError::Decode("裁剪高度无法转为 usize".to_owned()))?;
        Ok(Crop {
            x,
            y,
            width,
            height,
        })
    }
}

/// 校验裁剪区域在编码帧内且满足 4:2:0 的偶数约束。
fn validate_crop(crop: Crop, coded_width: usize, coded_height: usize) -> Result<(), WinVideoError> {
    if crop.width == 0
        || crop.height == 0
        || !crop.x.is_multiple_of(2)
        || !crop.y.is_multiple_of(2)
        || !crop.width.is_multiple_of(2)
        || !crop.height.is_multiple_of(2)
    {
        return Err(WinVideoError::Decode(
            "4:2:0 裁剪要求 x/y/宽/高均为非零偶数".to_owned(),
        ));
    }
    if crop.x + crop.width > coded_width || crop.y + crop.height > coded_height {
        return Err(WinVideoError::Decode("裁剪区域超出编码帧".to_owned()));
    }
    Ok(())
}

/// 计算输出长度（`width * height * 4`，带溢出检查）。
fn output_len(crop: Crop) -> Result<usize, WinVideoError> {
    crop.width
        .checked_mul(crop.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| WinVideoError::Decode("裁剪输出大小溢出".to_owned()))
}

/// NV12（Y 平面 + 交织 UV 平面）→ 自顶向下 RGBA（整帧）。
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
    let crop = Crop {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    nv12_to_rgba_impl(
        y,
        uv,
        y_pitch,
        uv_pitch,
        w,
        h,
        crop,
        out,
        worker_count(w.saturating_mul(h), h / 2),
    )
}

/// NV12 → RGBA，并把可见区域裁剪**融进**颜色转换（只写可见行/列）。
///
/// 调用方必须先用 [`crop_is_supported`] 确认偶数对齐与边界；不满足时退回
/// [`nv12_to_rgba`] + 外部裁剪，保证输出逐字节一致。
///
/// # Errors
/// 尺寸/裁剪非法、stride 小于行宽或平面长度不足时返回 [`WinVideoError::Decode`]。
#[expect(
    clippy::too_many_arguments,
    reason = "两个平面 + 两个 stride + 编码尺寸 + 裁剪 + 输出，逐项对应调用点"
)]
pub(crate) fn nv12_to_rgba_cropped(
    y: &[u8],
    uv: &[u8],
    y_pitch: usize,
    uv_pitch: usize,
    width: u32,
    height: u32,
    crop: CropRect,
    out: &mut Vec<u8>,
) -> Result<(), WinVideoError> {
    let (w, h) = dimensions(width, height)?;
    let crop = crop.to_internal()?;
    let workers = worker_count(
        crop.width.saturating_mul(crop.height),
        crop.height / 2,
    );
    nv12_to_rgba_impl(y, uv, y_pitch, uv_pitch, w, h, crop, out, workers)
}

/// 通用 NV12 转换实现（`crop` 为整帧时即原有行为）。
#[expect(
    clippy::too_many_arguments,
    reason = "平面/stride/编码尺寸/裁剪/输出/worker 数逐项对应调用点，拆结构体反而更难核对"
)]
fn nv12_to_rgba_impl(
    y: &[u8],
    uv: &[u8],
    y_pitch: usize,
    uv_pitch: usize,
    coded_width: usize,
    coded_height: usize,
    crop: Crop,
    out: &mut Vec<u8>,
    workers: usize,
) -> Result<(), WinVideoError> {
    validate_plane(y, y_pitch, coded_height, coded_width, "Y")?;
    validate_plane(uv, uv_pitch, coded_height / 2, coded_width, "UV")?;
    validate_crop(crop, coded_width, coded_height)?;

    // 不要先 `clear()`：长度不变时 `resize` 是 no-op，可省掉每帧 8.3 MB 的
    // memset；输出缓冲随后会被逐像素全部覆写。
    out.resize(output_len(crop)?, 255);

    let row_bytes = crop.width.saturating_mul(4);
    if workers <= 1 {
        nv12_rows(y, uv, y_pitch, uv_pitch, crop, 0, out.as_mut_slice());
        return Ok(());
    }
    let pairs = crop.height / 2;
    let pairs_per_chunk = pairs.div_ceil(workers);
    let chunk_bytes = pairs_per_chunk.saturating_mul(2).saturating_mul(row_bytes);
    std::thread::scope(|scope| {
        for (index, chunk) in out.chunks_mut(chunk_bytes).enumerate() {
            let start_pair = index.saturating_mul(pairs_per_chunk);
            let _ = scope.spawn(move || {
                nv12_rows(y, uv, y_pitch, uv_pitch, crop, start_pair, chunk);
            });
        }
    });
    Ok(())
}

/// 转换从 `start_pair` 开始的一段输出行（`out` 长度决定行数）。
fn nv12_rows(
    y: &[u8],
    uv: &[u8],
    y_pitch: usize,
    uv_pitch: usize,
    crop: Crop,
    start_pair: usize,
    out: &mut [u8],
) {
    let row_bytes = crop.width.saturating_mul(4);
    let pairs = out.len() / row_bytes.saturating_mul(2).max(1);
    for local in 0..pairs {
        let pair = start_pair.saturating_add(local);
        let row = crop.y.saturating_add(pair.saturating_mul(2));
        let y0_start = row.saturating_mul(y_pitch).saturating_add(crop.x);
        let y1_start = row
            .saturating_add(1)
            .saturating_mul(y_pitch)
            .saturating_add(crop.x);
        let uv_start = (row / 2)
            .saturating_mul(uv_pitch)
            .saturating_add(crop.x);
        let y0 = &y[y0_start..y0_start + crop.width];
        let y1 = &y[y1_start..y1_start + crop.width];
        let uv_row = &uv[uv_start..uv_start + crop.width];
        let OutRows { out0, out1 } = split_output_pair(out, local, row_bytes);

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
}

/// 平面 YUV 4:2:0（Y + 独立 U/V 平面）→ 自顶向下 RGBA（整帧）。
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
    planar_to_rgba_with_workers(
        luma,
        u_plane,
        v_plane,
        y_pitch,
        chroma_pitch,
        width,
        height,
        out,
        None,
    )
}

/// 平面 YUV → RGBA，融合可见区域裁剪。
///
/// 调用方必须先用 [`crop_is_supported`] 确认偶数对齐与边界。
///
/// # Errors
/// 同 [`planar_to_rgba`]；裁剪非法时也返回 [`WinVideoError::Decode`]。
#[expect(
    clippy::too_many_arguments,
    reason = "三个平面 + 两个 stride + 尺寸 + 裁剪 + 输出，逐项对应调用点"
)]
pub(crate) fn planar_to_rgba_cropped(
    luma: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_pitch: usize,
    chroma_pitch: usize,
    width: u32,
    height: u32,
    crop: CropRect,
    out: &mut Vec<u8>,
) -> Result<(), WinVideoError> {
    let (w, h) = dimensions(width, height)?;
    let crop = crop.to_internal()?;
    planar_to_rgba_impl(
        luma,
        u_plane,
        v_plane,
        y_pitch,
        chroma_pitch,
        w,
        h,
        crop,
        out,
        worker_count(
            crop.width.saturating_mul(crop.height),
            crop.height / 2,
        ),
    )
}

/// 平面 YUV 转换的公共实现；`workers == None` 表示用整帧尺寸计算 worker 数。
#[expect(
    clippy::too_many_arguments,
    reason = "平面/stride/尺寸/输出/worker 数逐项对应调用点"
)]
fn planar_to_rgba_with_workers(
    luma: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_pitch: usize,
    chroma_pitch: usize,
    width: u32,
    height: u32,
    out: &mut Vec<u8>,
    workers: Option<usize>,
) -> Result<(), WinVideoError> {
    let (w, h) = dimensions(width, height)?;
    let crop = Crop {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let workers = workers.unwrap_or_else(|| worker_count(w.saturating_mul(h), h / 2));
    planar_to_rgba_impl(
        luma,
        u_plane,
        v_plane,
        y_pitch,
        chroma_pitch,
        w,
        h,
        crop,
        out,
        workers,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "三个平面 + 两个 stride + 编码尺寸 + 裁剪 + 输出 + worker 数"
)]
fn planar_to_rgba_impl(
    luma: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_pitch: usize,
    chroma_pitch: usize,
    coded_width: usize,
    coded_height: usize,
    crop: Crop,
    out: &mut Vec<u8>,
    workers: usize,
) -> Result<(), WinVideoError> {
    let chroma_width = coded_width / 2;
    let chroma_rows = coded_height / 2;
    validate_plane(luma, y_pitch, coded_height, coded_width, "Y")?;
    validate_plane(u_plane, chroma_pitch, chroma_rows, chroma_width, "U")?;
    validate_plane(v_plane, chroma_pitch, chroma_rows, chroma_width, "V")?;
    validate_crop(crop, coded_width, coded_height)?;

    // 同 `nv12_to_rgba_impl`：不要先 `clear()`，让定长复用缓冲省掉 memset。
    out.resize(output_len(crop)?, 255);

    let row_bytes = crop.width.saturating_mul(4);
    if workers <= 1 {
        planar_rows(
            luma, u_plane, v_plane, y_pitch, chroma_pitch, crop, 0, out.as_mut_slice(),
        );
        return Ok(());
    }
    let pairs = crop.height / 2;
    let pairs_per_chunk = pairs.div_ceil(workers);
    let chunk_bytes = pairs_per_chunk.saturating_mul(2).saturating_mul(row_bytes);
    std::thread::scope(|scope| {
        for (index, chunk) in out.chunks_mut(chunk_bytes).enumerate() {
            let start_pair = index.saturating_mul(pairs_per_chunk);
            let _ = scope.spawn(move || {
                planar_rows(
                    luma, u_plane, v_plane, y_pitch, chroma_pitch, crop, start_pair, chunk,
                );
            });
        }
    });
    Ok(())
}

/// 转换从 `start_pair` 开始的一段平面 YUV 输出行。
#[expect(
    clippy::too_many_arguments,
    reason = "三个平面 + 两个 stride + 裁剪 + 起始行对 + 输出"
)]
fn planar_rows(
    luma: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_pitch: usize,
    chroma_pitch: usize,
    crop: Crop,
    start_pair: usize,
    out: &mut [u8],
) {
    let row_bytes = crop.width.saturating_mul(4);
    let pairs = out.len() / row_bytes.saturating_mul(2).max(1);
    let chroma_width = crop.width / 2;
    let chroma_x = crop.x / 2;
    for local in 0..pairs {
        let pair = start_pair.saturating_add(local);
        let row = crop.y.saturating_add(pair.saturating_mul(2));
        let y0_start = row.saturating_mul(y_pitch).saturating_add(crop.x);
        let y1_start = row
            .saturating_add(1)
            .saturating_mul(y_pitch)
            .saturating_add(crop.x);
        let chroma_start = (row / 2)
            .saturating_mul(chroma_pitch)
            .saturating_add(chroma_x);
        let y0 = &luma[y0_start..y0_start + crop.width];
        let y1 = &luma[y1_start..y1_start + crop.width];
        let u_row = &u_plane[chroma_start..chroma_start + chroma_width];
        let v_row = &v_plane[chroma_start..chroma_start + chroma_width];
        let OutRows { out0, out1 } = split_output_pair(out, local, row_bytes);

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
}

/// 一段输出块里的一对相邻输出行。
struct OutRows<'a> {
    /// 偶数行。
    out0: &'a mut [u8],
    /// 奇数行。
    out1: &'a mut [u8],
}

/// 从输出块中切出第 `pair` 对相邻行。
fn split_output_pair(out: &mut [u8], pair: usize, row_bytes: usize) -> OutRows<'_> {
    let start = pair.saturating_mul(2).saturating_mul(row_bytes);
    let (out0, rest) = out[start..].split_at_mut(row_bytes);
    let out1 = &mut rest[..row_bytes];
    OutRows { out0, out1 }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成 NV12 渐变图（Y 取值 16..=235，UV 取值 16..=215）。
    fn gradient_nv12(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
        let w = usize::try_from(width).unwrap_or(0);
        let h = usize::try_from(height).unwrap_or(0);
        let y = (0..w * h)
            .map(|i| u8::try_from(i % 220).unwrap_or(0).saturating_add(16))
            .collect();
        let uv = (0..(w * h) / 2)
            .map(|i| u8::try_from((i * 7) % 200).unwrap_or(0).saturating_add(16))
            .collect();
        (y, uv)
    }

    /// 从整帧 RGBA 里手工裁出可见区域（旧路径的 `crop_to_display` 语义）。
    fn crop_rgba(
        full: &[u8],
        coded_width: usize,
        crop: CropRect,
    ) -> Vec<u8> {
        let coded_row_bytes = coded_width * 4;
        let crop_row_bytes = usize::try_from(crop.width).unwrap_or(0) * 4;
        let x_offset = usize::try_from(crop.x).unwrap_or(0) * 4;
        let y_offset = usize::try_from(crop.y).unwrap_or(0);
        let rows = usize::try_from(crop.height).unwrap_or(0);
        let mut out = Vec::with_capacity(crop_row_bytes * rows);
        for row in 0..rows {
            let start = (y_offset + row) * coded_row_bytes + x_offset;
            out.extend_from_slice(&full[start..start + crop_row_bytes]);
        }
        out
    }

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

    /// 融合裁剪（1920×1088 coded → 1920×1080 display）必须与「整帧转换后再裁」
    /// 逐字节一致。
    #[test]
    fn fused_crop_matches_full_then_crop_for_1080p() {
        let (width, height) = (1920_u32, 1088_u32);
        let (y, uv) = gradient_nv12(width, height);
        let pitch = usize::try_from(width).unwrap_or(0);
        let crop = CropRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };

        let mut full = Vec::new();
        nv12_to_rgba(&y, &uv, pitch, pitch, width, height, &mut full).unwrap_or_default();
        let mut fused = Vec::new();
        nv12_to_rgba_cropped(&y, &uv, pitch, pitch, width, height, crop, &mut fused)
            .unwrap_or_default();

        assert_eq!(fused, crop_rgba(&full, pitch, crop));
    }

    /// x/y 非零的偶数偏移也必须逐字节一致。
    #[test]
    fn fused_crop_with_even_offset_matches_full_then_crop() {
        let (width, height) = (1920_u32, 1088_u32);
        let (y, uv) = gradient_nv12(width, height);
        let pitch = usize::try_from(width).unwrap_or(0);
        let crop = CropRect {
            x: 64,
            y: 32,
            width: 1280,
            height: 720,
        };

        let mut full = Vec::new();
        nv12_to_rgba(&y, &uv, pitch, pitch, width, height, &mut full).unwrap_or_default();
        let mut fused = Vec::new();
        nv12_to_rgba_cropped(&y, &uv, pitch, pitch, width, height, crop, &mut fused)
            .unwrap_or_default();

        assert_eq!(fused, crop_rgba(&full, pitch, crop));
    }

    /// 奇数偏移/越界必须被判定为不支持，由调用方回退旧路径。
    #[test]
    fn odd_or_out_of_bounds_crop_is_rejected() {
        assert!(!crop_is_supported(
            CropRect { x: 1, y: 0, width: 64, height: 64 },
            128,
            128
        ));
        assert!(!crop_is_supported(
            CropRect { x: 0, y: 3, width: 64, height: 64 },
            128,
            128
        ));
        assert!(!crop_is_supported(
            CropRect { x: 0, y: 0, width: 63, height: 64 },
            128,
            128
        ));
        assert!(!crop_is_supported(
            CropRect { x: 0, y: 0, width: 128, height: 130 },
            128,
            128
        ));
        assert!(crop_is_supported(
            CropRect { x: 0, y: 0, width: 128, height: 128 },
            128,
            128
        ));
    }

    /// 并行与单线程必须逐字节一致。
    #[test]
    fn parallel_nv12_matches_single_thread() {
        let (width, height) = (1920_u32, 1088_u32);
        let (y, uv) = gradient_nv12(width, height);
        let w = usize::try_from(width).unwrap_or(0);
        let h = usize::try_from(height).unwrap_or(0);
        let crop = Crop {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let mut single = Vec::new();
        nv12_to_rgba_impl(y.as_slice(), &uv, w, w, w, h, crop, &mut single, 1)
            .unwrap_or_default();
        let mut multi = Vec::new();
        nv12_to_rgba_impl(y.as_slice(), &uv, w, w, w, h, crop, &mut multi, 4)
            .unwrap_or_default();
        assert_eq!(single, multi);
    }

    /// 平面 YUV 的融合裁剪与并行也必须一致。
    #[test]
    fn parallel_planar_crop_matches_full_then_crop() {
        let (width, height) = (640_u32, 368_u32);
        let w = usize::try_from(width).unwrap_or(0);
        let h = usize::try_from(height).unwrap_or(0);
        let luma: Vec<u8> = (0..w * h)
            .map(|i| u8::try_from(i % 220).unwrap_or(0).saturating_add(16))
            .collect();
        let chroma: Vec<u8> = (0..(w / 2) * (h / 2))
            .map(|i| u8::try_from((i * 5) % 200).unwrap_or(0).saturating_add(16))
            .collect();
        let crop = CropRect {
            x: 4,
            y: 2,
            width: 320,
            height: 180,
        };

        let mut full = Vec::new();
        planar_to_rgba(&luma, &chroma, &chroma, w, w / 2, width, height, &mut full)
            .unwrap_or_default();
        let mut fused = Vec::new();
        planar_to_rgba_cropped(
            &luma,
            &chroma,
            &chroma,
            w,
            w / 2,
            width,
            height,
            crop,
            &mut fused,
        )
        .unwrap_or_default();
        assert_eq!(fused, crop_rgba(&full, w, crop));

        // 单线程与 4 worker 的输出一致。
        let full_crop = Crop {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let mut single = Vec::new();
        planar_to_rgba_impl(
            &luma,
            &chroma,
            &chroma,
            w,
            w / 2,
            w,
            h,
            full_crop,
            &mut single,
            1,
        )
        .unwrap_or_default();
        let mut multi = Vec::new();
        planar_to_rgba_impl(
            &luma,
            &chroma,
            &chroma,
            w,
            w / 2,
            w,
            h,
            full_crop,
            &mut multi,
            4,
        )
        .unwrap_or_default();
        assert_eq!(single, multi);
    }

    /// 1080p NV12 单线程/多线程耗时对照（release 下 `--ignored --nocapture`）。
    #[test]
    #[ignore = "性能观察用，默认不跑"]
    fn bench_nv12_1080p_parallel() {
        let (width, height) = (1920_u32, 1088_u32);
        let (y, uv) = gradient_nv12(width, height);
        let w = usize::try_from(width).unwrap_or(0);
        let h = usize::try_from(height).unwrap_or(0);
        let crop = Crop {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let mut out = Vec::new();
        for workers in [1, 4] {
            // 取 5 次最小值：共享机器上的调度抖动会让单次读数失真。
            let mut best = std::time::Duration::MAX;
            for _ in 0..5 {
                let started = std::time::Instant::now();
                nv12_to_rgba_impl(y.as_slice(), &uv, w, w, w, h, crop, &mut out, workers)
                    .unwrap_or_default();
                best = best.min(started.elapsed());
            }
            println!("nv12_to_rgba 1920x1088 workers={workers}: {best:?}");
        }
    }
}
