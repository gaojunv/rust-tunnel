//! 纯像素工具：通道交换与盒式下采样。
//!
//! 这两个函数是采集层（BGRA）与 remote 编码层（RGBA）之间的适配缝，也是
//! NVENC「只裁剪、不缩放」约束的补偿点：4K 采集源必须先在 CPU 上盒式
//! 缩到编码尺寸，再交给硬件编码器。
//!
//! 本模块**不依赖任何 Windows API**，可以单独做单元测试。

use crate::{worker_count, WinVideoError};

/// 保留一个像素的字节数（RGBA8 / BGRA8 都是 4）。
const BYTES_PER_PIXEL: usize = 4;

/// 把自顶向下 BGRA8 转成 RGBA8（交换第 0/2 字节，alpha 原样保留）。
///
/// 输入长度应当是 4 的倍数；末尾不足一个像素的残余字节会被**原样复制**，
/// 这样这个函数永远不会 panic（调用方本就不该传入这种输入）。
#[must_use]
pub fn bgra_to_rgba(src: &[u8]) -> Vec<u8> {
    // 定长预分配后按 4 字节像素直接写入：`extend_from_slice` 每个像素都要做一次
    // 容量检查与一次小拷贝，1080p 每帧两百万次调用是可测开销。切块后目标也是
    // 定长数组，边界检查可被编译器消掉。
    let mut out = vec![0u8; src.len()];
    let (pixels, remainder) = src.as_chunks::<BYTES_PER_PIXEL>();
    let (dst_pixels, dst_remainder) = out.as_chunks_mut::<BYTES_PER_PIXEL>();
    for (src_px, dst_px) in pixels.iter().zip(dst_pixels.iter_mut()) {
        // B、G、R、A → R、G、B、A。
        dst_px[0] = src_px[2];
        dst_px[1] = src_px[1];
        dst_px[2] = src_px[0];
        dst_px[3] = src_px[3];
    }
    dst_remainder.copy_from_slice(remainder);
    out
}

/// 把 RGBA8 画面盒式缩放到目标尺寸。
///
/// 每个目标像素取它在源图上覆盖的矩形区域，对 R/G/B/A 四个通道分别做
/// `u64` 累加后取整平均（与 `crates/remote/src/video.rs` 的 `rgba_to_i444`
/// 同思路，但只输出 RGBA、不做 YUV 转换）。
///
/// 源尺寸恰好是目标尺寸的 2 倍时（4K → 1080p 走这条）使用 2×2 精确快速
/// 路径；其余比例走通用路径。目标尺寸等于源尺寸时直接复制。
///
/// # Errors
/// - `src.len()` 与 `src_width * src_height * 4` 不符；
/// - 源或目标尺寸为 0；
/// - 尺寸相乘溢出 `usize`。
pub fn scale_rgba_box(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
) -> Result<Vec<u8>, WinVideoError> {
    let (sw, sh, dw, dh) = (
        src_width as usize,
        src_height as usize,
        dst_width as usize,
        dst_height as usize,
    );
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return Err(WinVideoError::Config(format!(
            "缩放尺寸不能为 0（源 {src_width}x{src_height}，目标 {dst_width}x{dst_height}）"
        )));
    }
    let expected = sw
        .checked_mul(sh)
        .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| WinVideoError::Config("源尺寸溢出".to_owned()))?;
    if src.len() != expected {
        return Err(WinVideoError::Config(format!(
            "RGBA 长度 {} 与源尺寸 {src_width}x{src_height} 不符（应为 {expected}）",
            src.len()
        )));
    }
    let dst_pixels = dw
        .checked_mul(dh)
        .ok_or_else(|| WinVideoError::Config("目标尺寸溢出".to_owned()))?;
    let dst_len = dst_pixels
        .checked_mul(BYTES_PER_PIXEL)
        .ok_or_else(|| WinVideoError::Config("目标尺寸溢出".to_owned()))?;

    if (sw, sh) == (dw, dh) {
        return Ok(src.to_vec());
    }

    let mut out = vec![0u8; dst_len];
    if sw == dw.saturating_mul(2) && sh == dh.saturating_mul(2) {
        scale_2x2(src, sw, out.as_mut_slice());
    } else {
        scale_generic(src, sw, sh, dw, dh, out.as_mut_slice());
    }
    Ok(out)
}

/// 把 BGRA8 画面**一步**盒式缩放到目标尺寸并输出 RGBA8。
///
/// 这是采集路径的合并优化：4K 采集源直接写到 1920×1080 的 RGBA 缓冲，
/// 避免先生成 33 MB 的中间 RGBA 再缩一遍。通道交换在写目标像素时完成。
///
/// # Errors
/// 同 [`scale_rgba_box`]：源长度不符、源或目标尺寸为 0、尺寸溢出。
pub fn scale_bgra_to_rgba(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
) -> Result<Vec<u8>, WinVideoError> {
    let (sw, sh, dw, dh) = (
        src_width as usize,
        src_height as usize,
        dst_width as usize,
        dst_height as usize,
    );
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return Err(WinVideoError::Config(format!(
            "缩放尺寸不能为 0（源 {src_width}x{src_height}，目标 {dst_width}x{dst_height}）"
        )));
    }
    let expected = sw
        .checked_mul(sh)
        .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| WinVideoError::Config("源尺寸溢出".to_owned()))?;
    if src.len() != expected {
        return Err(WinVideoError::Config(format!(
            "BGRA 长度 {} 与源尺寸 {src_width}x{src_height} 不符（应为 {expected}）",
            src.len()
        )));
    }
    let dst_len = dw
        .checked_mul(dh)
        .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| WinVideoError::Config("目标尺寸溢出".to_owned()))?;

    if (sw, sh) == (dw, dh) {
        return Ok(bgra_to_rgba(src));
    }

    let mut out = vec![0u8; dst_len];
    if sw == dw.saturating_mul(2) && sh == dh.saturating_mul(2) {
        scale_bgra_2x2(src, sw, out.as_mut_slice());
    } else {
        scale_bgra_generic(src, sw, sh, dw, dh, out.as_mut_slice());
    }
    Ok(out)
}

/// 精确 2×2 快速路径：每个目标像素是源图 2×2 块的均值。
///
/// `src_width` 必须等于目标宽度的 2 倍（调用方已校验），因此这里用
/// `src_width / 2` 重建目标宽度，避免再传一个参数。
fn scale_2x2(src: &[u8], src_width: usize, dst: &mut [u8]) {
    let dst_width = src_width / 2;
    let rows = dst.len() / (dst_width * BYTES_PER_PIXEL).max(1);
    let workers = worker_count(rows.saturating_mul(dst_width), rows);
    scale_2x2_with_workers(src, src_width, dst, workers);
}

/// 指定 worker 数的 2×2 路径（测试用它对比单线程/多线程输出）。
fn scale_2x2_with_workers(src: &[u8], src_width: usize, dst: &mut [u8], workers: usize) {
    let dst_width = src_width / 2;
    let row_bytes = dst_width * BYTES_PER_PIXEL;
    if workers <= 1 {
        scale_2x2_rows(src, src_width, 0, dst);
        return;
    }
    let rows = dst.len() / row_bytes.max(1);
    let rows_per_chunk = rows.div_ceil(workers);
    let chunk_bytes = rows_per_chunk.saturating_mul(row_bytes);
    std::thread::scope(|scope| {
        for (index, chunk) in dst.chunks_mut(chunk_bytes).enumerate() {
            let start_row = index.saturating_mul(rows_per_chunk);
            let _ = scope.spawn(move || scale_2x2_rows(src, src_width, start_row, chunk));
        }
    });
}

/// 处理输出行 `[start_row, start_row + dst 行数)` 的 2×2 盒式下采样。
fn scale_2x2_rows(src: &[u8], src_width: usize, start_row: usize, dst: &mut [u8]) {
    let dst_width = src_width / 2;
    let row_bytes = dst_width * BYTES_PER_PIXEL;
    let src_row_bytes = src_width * BYTES_PER_PIXEL;
    for (local, dst_row) in dst.chunks_exact_mut(row_bytes).enumerate() {
        let dy = start_row.saturating_add(local);
        let row0 = dy.saturating_mul(2).saturating_mul(src_row_bytes);
        let row1 = row0.saturating_add(src_row_bytes);
        let (dst_pixels, _) = dst_row.as_chunks_mut::<BYTES_PER_PIXEL>();
        for (dx, px) in dst_pixels.iter_mut().enumerate() {
            let col0 = dx * 2 * BYTES_PER_PIXEL;
            let col1 = col0 + BYTES_PER_PIXEL;
            for channel in 0..BYTES_PER_PIXEL {
                let sum = u64::from(src[row0 + col0 + channel])
                    + u64::from(src[row0 + col1 + channel])
                    + u64::from(src[row1 + col0 + channel])
                    + u64::from(src[row1 + col1 + channel]);
                px[channel] = mean4(sum);
            }
        }
    }
}

/// 通用盒式路径：按比例把源矩形映射到目标像素。
fn scale_generic(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, dst: &mut [u8]) {
    let workers = worker_count(dw.saturating_mul(dh), dh);
    scale_generic_with_workers(src, sw, sh, dw, dh, dst, workers);
}

/// 指定 worker 数的通用路径（测试用它对比单线程/多线程输出）。
fn scale_generic_with_workers(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    dst: &mut [u8],
    workers: usize,
) {
    let row_bytes = dw * BYTES_PER_PIXEL;
    if workers <= 1 {
        scale_generic_rows(src, sw, sh, dw, dh, 0, dst);
        return;
    }
    let rows_per_chunk = dh.div_ceil(workers);
    let chunk_bytes = rows_per_chunk.saturating_mul(row_bytes);
    std::thread::scope(|scope| {
        for (index, chunk) in dst.chunks_mut(chunk_bytes).enumerate() {
            let start_row = index.saturating_mul(rows_per_chunk);
            let _ = scope.spawn(move || {
                scale_generic_rows(src, sw, sh, dw, dh, start_row, chunk);
            });
        }
    });
}

/// 处理输出行 `[start_row, start_row + dst 行数)` 的通用盒式下采样。
fn scale_generic_rows(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    start_row: usize,
    dst: &mut [u8],
) {
    let row_bytes = dw * BYTES_PER_PIXEL;
    for (local, dst_row) in dst.chunks_exact_mut(row_bytes).enumerate() {
        let dy = start_row.saturating_add(local);
        // 与 `rgba_to_i444` 相同：目标行 `dy` 覆盖源行区间 `[sy0, sy1)`。
        let sy0 = dy * sh / dh;
        let sy1 = (((dy + 1) * sh) / dh).max(sy0 + 1);
        for dx in 0..dw {
            let sx0 = dx * sw / dw;
            let sx1 = (((dx + 1) * sw) / dw).max(sx0 + 1);
            let mut sums = [0u64; BYTES_PER_PIXEL];
            let mut count = 0u64;
            for sy in sy0..sy1 {
                let row = sy * sw;
                for sx in sx0..sx1 {
                    let off = (row + sx) * BYTES_PER_PIXEL;
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum += u64::from(src[off + channel]);
                    }
                    count += 1;
                }
            }
            let off = dx * BYTES_PER_PIXEL;
            for (channel, sum) in sums.iter().copied().enumerate() {
                dst_row[off + channel] = mean(sum, count);
            }
        }
    }
}

/// BGRA 的 2×2 快速路径：源 4 个像素的 B/G/R/A 分别取均值，输出 R/G/B/A。
fn scale_bgra_2x2(src: &[u8], src_width: usize, dst: &mut [u8]) {
    let dst_width = src_width / 2;
    let rows = dst.len() / (dst_width * BYTES_PER_PIXEL).max(1);
    let workers = worker_count(rows.saturating_mul(dst_width), rows);
    scale_bgra_2x2_with_workers(src, src_width, dst, workers);
}

/// 指定 worker 数的 BGRA 2×2 路径。
fn scale_bgra_2x2_with_workers(src: &[u8], src_width: usize, dst: &mut [u8], workers: usize) {
    let dst_width = src_width / 2;
    let row_bytes = dst_width * BYTES_PER_PIXEL;
    if workers <= 1 {
        scale_bgra_2x2_rows(src, src_width, 0, dst);
        return;
    }
    let rows = dst.len() / row_bytes.max(1);
    let rows_per_chunk = rows.div_ceil(workers);
    let chunk_bytes = rows_per_chunk.saturating_mul(row_bytes);
    std::thread::scope(|scope| {
        for (index, chunk) in dst.chunks_mut(chunk_bytes).enumerate() {
            let start_row = index.saturating_mul(rows_per_chunk);
            let _ = scope.spawn(move || scale_bgra_2x2_rows(src, src_width, start_row, chunk));
        }
    });
}

/// 处理输出行 `[start_row, start_row + dst 行数)` 的 BGRA 2×2 下采样。
fn scale_bgra_2x2_rows(src: &[u8], src_width: usize, start_row: usize, dst: &mut [u8]) {
    let dst_width = src_width / 2;
    let row_bytes = dst_width * BYTES_PER_PIXEL;
    let src_row_bytes = src_width * BYTES_PER_PIXEL;
    for (local, dst_row) in dst.chunks_exact_mut(row_bytes).enumerate() {
        let dy = start_row.saturating_add(local);
        let row0 = dy.saturating_mul(2).saturating_mul(src_row_bytes);
        let row1 = row0.saturating_add(src_row_bytes);
        let (dst_pixels, _) = dst_row.as_chunks_mut::<BYTES_PER_PIXEL>();
        for (dx, px) in dst_pixels.iter_mut().enumerate() {
            let col0 = dx * 2 * BYTES_PER_PIXEL;
            let col1 = col0 + BYTES_PER_PIXEL;
            let mut sums = [0u64; BYTES_PER_PIXEL];
            for offset in [row0 + col0, row0 + col1, row1 + col0, row1 + col1] {
                for (channel, sum) in sums.iter_mut().enumerate() {
                    *sum += u64::from(src[offset + channel]);
                }
            }
            // 源是 B、G、R、A；输出是 R、G、B、A。
            px[0] = mean4(sums[2]);
            px[1] = mean4(sums[1]);
            px[2] = mean4(sums[0]);
            px[3] = mean4(sums[3]);
        }
    }
}

/// BGRA 的通用盒式路径：按比例取源矩形均值并在输出时交换 R/B。
fn scale_bgra_generic(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, dst: &mut [u8]) {
    let workers = worker_count(dw.saturating_mul(dh), dh);
    scale_bgra_generic_with_workers(src, sw, sh, dw, dh, dst, workers);
}

/// 指定 worker 数的 BGRA 通用路径。
fn scale_bgra_generic_with_workers(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    dst: &mut [u8],
    workers: usize,
) {
    let row_bytes = dw * BYTES_PER_PIXEL;
    if workers <= 1 {
        scale_bgra_generic_rows(src, sw, sh, dw, dh, 0, dst);
        return;
    }
    let rows_per_chunk = dh.div_ceil(workers);
    let chunk_bytes = rows_per_chunk.saturating_mul(row_bytes);
    std::thread::scope(|scope| {
        for (index, chunk) in dst.chunks_mut(chunk_bytes).enumerate() {
            let start_row = index.saturating_mul(rows_per_chunk);
            let _ = scope.spawn(move || {
                scale_bgra_generic_rows(src, sw, sh, dw, dh, start_row, chunk);
            });
        }
    });
}

/// 处理输出行 `[start_row, start_row + dst 行数)` 的 BGRA 通用下采样。
fn scale_bgra_generic_rows(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    start_row: usize,
    dst: &mut [u8],
) {
    let row_bytes = dw * BYTES_PER_PIXEL;
    for (local, dst_row) in dst.chunks_exact_mut(row_bytes).enumerate() {
        let dy = start_row.saturating_add(local);
        let sy0 = dy * sh / dh;
        let sy1 = (((dy + 1) * sh) / dh).max(sy0 + 1);
        for dx in 0..dw {
            let sx0 = dx * sw / dw;
            let sx1 = (((dx + 1) * sw) / dw).max(sx0 + 1);
            let mut sums = [0u64; BYTES_PER_PIXEL];
            let mut count = 0u64;
            for sy in sy0..sy1 {
                let row = sy * sw;
                for sx in sx0..sx1 {
                    let off = (row + sx) * BYTES_PER_PIXEL;
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum += u64::from(src[off + channel]);
                    }
                    count += 1;
                }
            }
            let off = dx * BYTES_PER_PIXEL;
            // 源是 B、G、R、A；输出是 R、G、B、A。
            dst_row[off] = mean(sums[2], count);
            dst_row[off + 1] = mean(sums[1], count);
            dst_row[off + 2] = mean(sums[0], count);
            dst_row[off + 3] = mean(sums[3], count);
        }
    }
}

/// 4 个字节和的均值（2×2 快速路径）。
#[expect(
    clippy::cast_possible_truncation,
    reason = "4 个 u8 之和最大 1020，除以 4 后落在 0..=255，取整后必然落在 u8 内"
)]
fn mean4(sum: u64) -> u8 {
    (sum / 4) as u8
}

/// `sum / count` 的均值；`count` 由调用方保证非零。
#[expect(
    clippy::cast_possible_truncation,
    reason = "均值不超过单个通道的最大值 255，取整后必然落在 u8 内"
)]
fn mean(sum: u64, count: u64) -> u8 {
    (sum / count) as u8
}

#[cfg(test)]
mod tests {
    // 测试代码豁免 panic 风险 lint：断言失败即 panic 是测试的预期行为。
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 生成 `width × height` 的渐变 RGBA（每像素值只取决于下标，便于手算）。
    fn gradient(width: u32, height: u32) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| {
                let i = u8::try_from(i).unwrap_or(0);
                [i, i.wrapping_mul(2), i.wrapping_mul(3), 255]
            })
            .collect()
    }

    #[test]
    fn bgra_to_rgba_swaps_red_and_blue_and_keeps_alpha() {
        let bgra = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(bgra_to_rgba(&bgra), vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn bgra_to_rgba_copies_trailing_partial_pixel() {
        let bgra = [1, 2, 3, 4, 9, 8];
        assert_eq!(bgra_to_rgba(&bgra), vec![3, 2, 1, 4, 9, 8]);
    }

    #[test]
    fn scale_4x4_to_2x2_averages_each_block() {
        let src = gradient(4, 4);
        let out = scale_rgba_box(&src, 4, 4, 2, 2).expect("4x4→2x2 应成功");
        assert_eq!(out.len(), 2 * 2 * 4);
        // 左上 2×2 块：下标 0、1、4、5。
        assert_eq!(&out[0..4], &[2, 5, 7, 255]);
        // 右上 2×2 块：下标 2、3、6、7。
        assert_eq!(&out[4..8], &[4, 9, 13, 255]);
        // 左下 2×2 块：下标 8、9、12、13。
        assert_eq!(&out[8..12], &[10, 21, 31, 255]);
        // 右下 2×2 块：下标 10、11、14、15。
        assert_eq!(&out[12..16], &[12, 25, 37, 255]);
    }

    #[test]
    fn scale_generic_path_averages_whole_image() {
        let src = gradient(4, 4);
        let out = scale_rgba_box(&src, 4, 4, 1, 1).expect("4x4→1x1 应成功");
        // 16 个像素下标 0..16：R 均值 7，G 均值 15，B 均值 22，A 恒 255。
        assert_eq!(out, vec![7, 15, 22, 255]);
    }

    #[test]
    fn scale_same_size_is_a_passthrough() {
        let src = gradient(3, 2);
        let out = scale_rgba_box(&src, 3, 2, 3, 2).expect("同尺寸应成功");
        assert_eq!(out, src);
    }

    #[test]
    fn scale_bgra_2x2_to_rgba_averages_and_swaps_channels() {
        // 2×2 BGRA：[B,G,R,A]，输出应是对应通道均值并按 R,G,B,A 排列。
        let src = [
            10, 20, 30, 255, // (0,0)
            40, 50, 60, 255, // (1,0)
            70, 80, 90, 255, // (0,1)
            100, 110, 120, 255, // (1,1)
        ];
        let out = scale_bgra_to_rgba(&src, 2, 2, 1, 1).expect("2x2→1x1 应成功");
        assert_eq!(out, vec![75, 65, 55, 255]);
    }

    #[test]
    fn scale_bgra_same_size_is_a_channel_swap() {
        let bgra = [1, 2, 3, 4, 5, 6, 7, 8];
        let out = scale_bgra_to_rgba(&bgra, 2, 1, 2, 1).expect("同尺寸应成功");
        assert_eq!(out, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn scale_bgra_generic_path_swaps_channels() {
        // 4×4 全图同一颜色：B=10、G=20、R=30、A=255。
        let src: Vec<u8> = (0..16).flat_map(|_| [10u8, 20, 30, 255]).collect();
        let out = scale_bgra_to_rgba(&src, 4, 4, 1, 1).expect("4x4→1x1 应成功");
        assert_eq!(out, vec![30, 20, 10, 255]);
    }

    #[test]
    fn scale_bgra_rejects_bad_length() {
        let src = vec![0u8; 63];
        assert!(scale_bgra_to_rgba(&src, 4, 4, 2, 2).is_err());
    }

    #[test]
    fn scale_rejects_bad_length() {
        let src = vec![0u8; 63];
        assert!(scale_rgba_box(&src, 4, 4, 2, 2).is_err());
    }

    #[test]
    fn scale_rejects_zero_dimensions() {
        let src = vec![0u8; 16];
        assert!(scale_rgba_box(&src, 4, 4, 0, 2).is_err());
        assert!(scale_rgba_box(&[], 0, 0, 2, 2).is_err());
    }

    /// 2×2 路径并行与单线程必须逐字节一致。
    #[test]
    fn parallel_2x2_matches_single_thread() {
        let src = gradient(1024, 1024);
        let dst_len = 512 * 512 * 4;
        let mut single = vec![0u8; dst_len];
        scale_2x2_with_workers(&src, 1024, &mut single, 1);
        let mut multi = vec![0u8; dst_len];
        scale_2x2_with_workers(&src, 1024, &mut multi, 4);
        assert_eq!(single, multi);

        let mut bgra_single = vec![0u8; dst_len];
        scale_bgra_2x2_with_workers(&src, 1024, &mut bgra_single, 1);
        let mut bgra_multi = vec![0u8; dst_len];
        scale_bgra_2x2_with_workers(&src, 1024, &mut bgra_multi, 4);
        assert_eq!(bgra_single, bgra_multi);
    }

    /// 通用路径并行与单线程必须逐字节一致。
    #[test]
    fn parallel_generic_matches_single_thread() {
        let (sw, sh, dw, dh) = (1000, 1000, 333, 333);
        let src = gradient(1000, 1000);
        let dst_len = 333 * 333 * 4;
        let mut single = vec![0u8; dst_len];
        scale_generic_with_workers(&src, sw, sh, dw, dh, &mut single, 1);
        let mut multi = vec![0u8; dst_len];
        scale_generic_with_workers(&src, sw, sh, dw, dh, &mut multi, 4);
        assert_eq!(single, multi);

        let mut bgra_single = vec![0u8; dst_len];
        scale_bgra_generic_with_workers(&src, sw, sh, dw, dh, &mut bgra_single, 1);
        let mut bgra_multi = vec![0u8; dst_len];
        scale_bgra_generic_with_workers(&src, sw, sh, dw, dh, &mut bgra_multi, 4);
        assert_eq!(bgra_single, bgra_multi);
    }

    /// 4K → 1080p 的耗时打印（release 下跑：`cargo test -p rust-tunnel-win-video --lib --release -- --ignored`）。
    #[test]
    #[ignore = "性能观察用，默认不跑"]
    fn bench_4k_to_1080p() {
        let src = vec![128u8; 3840 * 2160 * 4];
        let started = std::time::Instant::now();
        let out = scale_rgba_box(&src, 3840, 2160, 1920, 1080).expect("4K→1080p 应成功");
        println!(
            "4K→1080p 盒式下采样耗时 {:?}（输出 {} 字节）",
            started.elapsed(),
            out.len()
        );
        assert_eq!(out.len(), 1920 * 1080 * 4);

        let dst_len = 1920 * 1080 * 4;
        let mut single = vec![0u8; dst_len];
        let started = std::time::Instant::now();
        scale_2x2_with_workers(&src, 3840, &mut single, 1);
        println!("4K→1080p 2×2 workers=1: {:?}", started.elapsed());
        let mut multi = vec![0u8; dst_len];
        let started = std::time::Instant::now();
        scale_2x2_with_workers(&src, 3840, &mut multi, 4);
        println!("4K→1080p 2×2 workers=4: {:?}", started.elapsed());
        assert_eq!(single, multi);
    }
}
