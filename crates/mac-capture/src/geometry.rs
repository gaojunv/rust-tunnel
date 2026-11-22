//! 目标（编码）坐标系的纯逻辑：尺寸换算、区域盒式缩放、补丁矩形校验。
//!
//! # 为什么本模块不依赖引擎
//!
//! `rust-tunnel-mac-capture` 与 `win-video` 一样**不依赖** `rust-tunnel-remote`
//! （否则 `client-gui` 的适配层之外的代码也会被拖进 libvpx）。但目标尺寸的换算规则
//! 必须与引擎 `remote::video::encode_dimensions` **逐位一致**，否则补丁坐标系会和
//! 控制端持久帧缓冲对不上（表现是补丁贴在错位的位置、边缘撕裂）。因此这里复制一份
//! 同语义实现，并用单测把「区域缩放 == 整帧缩放后裁剪」这条等价关系钉死。
//!
//! # 两段缩放
//!
//! - 整帧：`capture()` 用 `mac-video` 的 [`scale_bgra_box`] 把物理像素一步缩到编码尺寸；
//! - 区域：补丁模式**不做整帧缩放再裁剪**（既多搬一份整帧，又会在矩形边缘引入接缝），
//!   而是对每个目标矩形按 [`source_span`] 直接算它在源图上的覆盖块——同一套取整规则，
//!   所以与整帧路径逐像素一致。
//!
//! [`scale_bgra_box`]: rust_tunnel_mac_video::color::scale_bgra_box

/// 每像素字节数（BGRA8 / RGBA8 都是 4）。
pub const BYTES_PER_PIXEL: usize = 4;

/// 目标（编码）坐标系里的一个矩形。
///
/// 与 `mac-video` 变化检测产出的矩形同一口径：`x`/`y`/`width`/`height` 全部**偶数**，
/// 因为 NV12 的 UV 平面是半分辨率，奇数边界会让色度块错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetRect {
    /// 左上角 X（偶数）。
    pub x: u32,
    /// 左上角 Y（偶数）。
    pub y: u32,
    /// 宽度（偶数、非零）。
    pub width: u32,
    /// 高度（偶数、非零）。
    pub height: u32,
}

impl TargetRect {
    /// 右边界（不含）。
    #[must_use]
    pub const fn right(self) -> u32 {
        self.x.saturating_add(self.width)
    }

    /// 下边界（不含）。
    #[must_use]
    pub const fn bottom(self) -> u32 {
        self.y.saturating_add(self.height)
    }
}

/// 把浮点结果安全地转成 `u32`（非有限 / 非正给 `0`）。
fn f64_to_u32(value: f64) -> u32 {
    if !value.is_finite() || value < 1.0 {
        return 0;
    }
    let clamped = value.min(f64::from(u32::MAX));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "已钳到 1..=u32::MAX 且非负，截断不改变有效范围"
    )]
    {
        clamped as u32
    }
}

/// 按比例缩放单个维度并四舍五入，下限 1 像素（与引擎 `scaled_dimension` 同语义）。
///
/// 下限 1 是必要的：0 尺寸会让后续的下采样网格/分配除零。
#[must_use]
pub fn scaled_dimension(original: u32, scale: f32) -> u32 {
    let scaled = (f64::from(original) * f64::from(scale)).round();
    if scaled < 1.0 {
        1
    } else {
        f64_to_u32(scaled).max(1)
    }
}

/// 计算编码尺寸：先乘 `scale`，再把宽度封顶到 `max_width`（等比缩高），最后对齐到偶数。
///
/// **必须与 `remote::video::encode_dimensions` 逐位一致**：引擎用同一函数算控制端持久帧
/// 缓冲的尺寸，采集侧算出的目标尺寸偏一像素就会出现补丁错位。
#[must_use]
pub fn encode_dimensions(width: u32, height: u32, scale: f32, max_width: u32) -> (u32, u32) {
    let mut w = scaled_dimension(width, scale).max(2);
    let mut h = scaled_dimension(height, scale).max(2);
    if w > max_width && max_width >= 2 {
        let ratio = f64::from(max_width) / f64::from(w);
        h = f64_to_u32((f64::from(h) * ratio).round()).max(2);
        w = max_width;
    }
    (w & !1, h & !1)
}

/// 目标轴上的索引 `index`（`0..dst_len`）在源轴上覆盖的半开区间 `[start, end)`。
///
/// 取整规则与 `mac-video` 的 `scale_bgra_box` 完全相同：起点向下取整、终点向上取整并
/// 至少覆盖 1 个源像素（放大时一个目标像素也能取到真实数据）。
#[must_use]
pub fn source_span(index: u32, src_len: u32, dst_len: u32) -> (u32, u32) {
    if src_len == 0 || dst_len == 0 {
        return (0, 0);
    }
    let (src, dst, at) = (u64::from(src_len), u64::from(dst_len), u64::from(index));
    let start = at.saturating_mul(src) / dst;
    let end = (at.saturating_add(1))
        .saturating_mul(src)
        .checked_div(dst)
        .unwrap_or(src)
        .max(start.saturating_add(1))
        .min(src);
    (
        u32::try_from(start).unwrap_or(0),
        u32::try_from(end).unwrap_or(src_len),
    )
}

/// 校验一个补丁矩形是否可以直接用于 `bgra_to_nv12`：坐标偶数、宽高为**正偶数**、
/// 完全落在目标尺寸内。
///
/// `mac-video` 的 `extract_rects` 已经保证这些条件；这里是**纵深防御**——补丁矩形要
/// 送进 NV12 转换与线上协议，任何一个奇数/越界值都会在控制端解出错误几何。
#[must_use]
pub fn is_valid_patch_rect(rect: TargetRect, dst_width: u32, dst_height: u32) -> bool {
    rect.width > 0
        && rect.height > 0
        && rect.x.is_multiple_of(2)
        && rect.y.is_multiple_of(2)
        && rect.width.is_multiple_of(2)
        && rect.height.is_multiple_of(2)
        && rect.right() <= dst_width
        && rect.bottom() <= dst_height
}

/// 从整帧源图里取出 `rect`（目标坐标）覆盖的区域，**按目标坐标盒式缩放**成
/// `rect.width × rect.height` 的紧包 BGRA。
///
/// 与「先把整帧缩到目标尺寸再裁剪」逐像素一致（单测 `region_scale_matches_full_frame_crop`
/// 钉住），但只搬矩形范围内的像素——补丁模式下其余像素一个字节都不碰。
///
/// # Errors
///
/// 源尺寸/长度不符、矩形非法（奇数/越界）、尺寸溢出时返回 `None`。调用方按
/// 「本拍不适合补丁」处理即可，不要 panic。
#[must_use]
pub fn scale_bgra_region(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    rect: TargetRect,
    dst_width: u32,
    dst_height: u32,
) -> Option<Vec<u8>> {
    if !is_valid_patch_rect(rect, dst_width, dst_height) || src_width == 0 || src_height == 0 {
        return None;
    }
    let expected = usize::try_from(src_width)
        .ok()?
        .checked_mul(usize::try_from(src_height).ok()?)?
        .checked_mul(BYTES_PER_PIXEL)?;
    if src.len() != expected {
        return None;
    }
    let out_len = usize::try_from(rect.width)
        .ok()?
        .checked_mul(usize::try_from(rect.height).ok()?)?
        .checked_mul(BYTES_PER_PIXEL)?;
    let mut out = vec![0_u8; out_len];
    for row in 0..rect.height {
        let (sy0, sy1) = source_span(rect.y.saturating_add(row), src_height, dst_height);
        for col in 0..rect.width {
            let (sx0, sx1) = source_span(rect.x.saturating_add(col), src_width, dst_width);
            let mut sums = [0_u64; BYTES_PER_PIXEL];
            let mut count = 0_u64;
            for sy in sy0..sy1 {
                let row_start = usize::try_from(sy)
                    .ok()?
                    .saturating_mul(usize::try_from(src_width).ok()?)
                    .saturating_mul(BYTES_PER_PIXEL);
                for sx in sx0..sx1 {
                    let offset = row_start
                        .saturating_add(usize::try_from(sx).ok()?.saturating_mul(BYTES_PER_PIXEL));
                    let pixel = src.get(offset..offset.saturating_add(BYTES_PER_PIXEL))?;
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum = sum.saturating_add(u64::from(*pixel.get(channel)?));
                    }
                    count = count.saturating_add(1);
                }
            }
            let divisor = count.max(1);
            let out_offset = usize::try_from(row)
                .ok()?
                .saturating_mul(usize::try_from(rect.width).ok()?)
                .saturating_add(usize::try_from(col).ok()?)
                .saturating_mul(BYTES_PER_PIXEL);
            for (channel, sum) in sums.iter().enumerate() {
                *out.get_mut(out_offset.checked_add(channel)?)? =
                    u8::try_from(sum / divisor).unwrap_or(0);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use rust_tunnel_mac_video::color::scale_bgra_box;

    /// 与引擎 `encode_dimensions` 的既有用例逐条对齐。
    #[test]
    fn encode_dimensions_matches_engine() {
        assert_eq!(encode_dimensions(3840, 2160, 1.0, 1920), (1920, 1080));
        assert_eq!(encode_dimensions(5120, 2880, 1.0, 1920), (1920, 1080));
        assert_eq!(encode_dimensions(1920, 1080, 1.0, 1920), (1920, 1080));
        assert_eq!(encode_dimensions(3840, 2160, 0.5, 1920), (1920, 1080));
        assert_eq!(encode_dimensions(1280, 800, 0.5, 1920), (640, 400));
        assert_eq!(encode_dimensions(101, 77, 1.0, 1920), (100, 76));
        assert_eq!(encode_dimensions(1, 1, 1.0, 1920), (2, 2));
        assert_eq!(encode_dimensions(3840, 2160, 1.0, 3840), (3840, 2160));
        assert_eq!(encode_dimensions(7680, 4320, 1.0, 3840), (3840, 2160));
        assert_eq!(encode_dimensions(3840, 2160, 0.5, 3840), (1920, 1080));
    }

    /// 非有限/零缩放不会塌到 0 或产生非法尺寸。
    #[test]
    fn encode_dimensions_is_defensive() {
        assert_eq!(scaled_dimension(1, 0.01), 1);
        assert_eq!(encode_dimensions(1920, 1080, 0.0, 1920), (2, 2));
        // NaN 与 0 同路：与引擎一致退到 2 像素下限，而不是产生非法尺寸。
        assert_eq!(encode_dimensions(1920, 1080, f32::NAN, 1920), (2, 2));
        assert_eq!(encode_dimensions(1920, 1080, -1.0, 1920), (2, 2));
        // 输出永远是偶数。
        let (w, h) = encode_dimensions(1919, 1079, 1.0, 1920);
        assert_eq!((w % 2, h % 2), (0, 0));
    }

    /// 下采样时（采集侧的唯一方向）源区间**恰好划分**整个源轴：不重叠、不越界、不漏像素。
    #[test]
    fn source_span_partitions_source_when_downscaling() {
        for (src, dst) in [
            (3840u32, 1920u32),
            (1080, 1080),
            (2560, 400),
            (7, 3),
            (2, 1),
        ] {
            let mut covered = 0;
            let mut previous_end = 0;
            for index in 0..dst {
                let (start, end) = source_span(index, src, dst);
                assert!(start <= end && end <= src, "{src}->{dst} 第 {index} 段越界");
                assert!(end > start, "{src}->{dst} 第 {index} 段为空");
                assert!(start >= previous_end, "{src}->{dst} 第 {index} 段重叠");
                covered += end - start;
                previous_end = end;
            }
            assert_eq!(covered, src, "{src}->{dst} 未覆盖整个源轴");
        }
        // 边界：零尺寸不 panic。
        assert_eq!(source_span(0, 0, 10), (0, 0));
        assert_eq!(source_span(0, 10, 0), (0, 0));
    }

    /// 关键不变量：区域缩放与「整帧缩放后裁剪」逐字节一致。
    #[test]
    fn region_scale_matches_full_frame_crop() {
        let (sw, sh) = (64u32, 48u32);
        let (dw, dh) = (32u32, 24u32);
        let mut src = Vec::with_capacity((sw * sh) as usize * 4);
        for y in 0..sh {
            for x in 0..sw {
                src.extend_from_slice(&[
                    u8::try_from((x * 3 + y) % 256).unwrap(),
                    u8::try_from((x + y * 5) % 256).unwrap(),
                    u8::try_from((x * 7 + y * 11) % 256).unwrap(),
                    255,
                ]);
            }
        }
        let full = scale_bgra_box(&src, sw, sh, dw, dh).unwrap();
        for rect in [
            TargetRect {
                x: 0,
                y: 0,
                width: dw,
                height: dh,
            },
            TargetRect {
                x: 4,
                y: 6,
                width: 8,
                height: 10,
            },
            TargetRect {
                x: 20,
                y: 0,
                width: 12,
                height: 24,
            },
        ] {
            let region = scale_bgra_region(&src, sw, sh, rect, dw, dh).expect("区域缩放");
            let row_len = usize::try_from(rect.width).unwrap() * 4;
            for row in 0..rect.height {
                let src_offset = (usize::try_from(rect.y + row).unwrap()
                    * usize::try_from(dw).unwrap()
                    + usize::try_from(rect.x).unwrap())
                    * 4;
                let dst_offset = usize::try_from(row).unwrap() * row_len;
                assert_eq!(
                    &region[dst_offset..dst_offset + row_len],
                    &full[src_offset..src_offset + row_len],
                    "矩形 {rect:?} 第 {row} 行与整帧缩放不一致"
                );
            }
        }
    }

    /// 奇数 / 越界 / 零面积的矩形一律判非法（防御，不 panic）。
    #[test]
    fn patch_rect_validation() {
        let (dw, dh) = (1920u32, 1080u32);
        assert!(is_valid_patch_rect(
            TargetRect {
                x: 0,
                y: 0,
                width: 64,
                height: 32
            },
            dw,
            dh
        ));
        assert!(!is_valid_patch_rect(
            TargetRect {
                x: 1,
                y: 0,
                width: 64,
                height: 32
            },
            dw,
            dh
        ));
        assert!(!is_valid_patch_rect(
            TargetRect {
                x: 0,
                y: 1,
                width: 64,
                height: 32
            },
            dw,
            dh
        ));
        assert!(!is_valid_patch_rect(
            TargetRect {
                x: 0,
                y: 0,
                width: 63,
                height: 32
            },
            dw,
            dh
        ));
        assert!(!is_valid_patch_rect(
            TargetRect {
                x: 0,
                y: 0,
                width: 64,
                height: 31
            },
            dw,
            dh
        ));
        assert!(!is_valid_patch_rect(
            TargetRect {
                x: 0,
                y: 0,
                width: 0,
                height: 32
            },
            dw,
            dh
        ));
        assert!(!is_valid_patch_rect(
            TargetRect {
                x: 1900,
                y: 0,
                width: 64,
                height: 32
            },
            dw,
            dh
        ));
        assert!(!is_valid_patch_rect(
            TargetRect {
                x: 0,
                y: 1060,
                width: 64,
                height: 32
            },
            dw,
            dh
        ));
        // 源长度不符 → None。
        assert!(scale_bgra_region(
            &[0; 4],
            64,
            48,
            TargetRect {
                x: 0,
                y: 0,
                width: 2,
                height: 2
            },
            dw,
            dh
        )
        .is_none());
    }
}
