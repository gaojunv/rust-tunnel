//! 与平台无关的画面几何计算（等比 contain 适配）。
//!
//! 渲染线程用它把「远端画面尺寸」映射到「Surface 像素矩形」；触控侧 Kotlin 会自己按
//! `onFrameInfo` 的尺寸与 SurfaceView 的显示矩形算坐标，两边公式一致即可对齐。

/// 一个视口矩形（GL 的 `glViewport` 参数顺序：左下角 x、y、宽、高，单位为 Surface 像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rect {
    /// 左下角 X（GL 坐标系原点在左下角）。
    pub(crate) x: i32,
    /// 左下角 Y。
    pub(crate) y: i32,
    /// 宽（像素）。
    pub(crate) width: i32,
    /// 高（像素）。
    pub(crate) height: i32,
}

/// 计算「等比 contain」：在 `surface` 尺寸里放得下的最大等比矩形，居中。
///
/// 任一尺寸为 0（Surface 尚未就绪 / 远端尚未出帧）时返回 `None`。
/// 结果宽高至少 1 像素，避免某些驱动对 0 尺寸视口的未定义行为。
#[must_use]
pub(crate) fn contain_rect(
    frame_width: u32,
    frame_height: u32,
    surface_width: i32,
    surface_height: i32,
) -> Option<Rect> {
    if frame_width == 0 || frame_height == 0 || surface_width <= 0 || surface_height <= 0 {
        return None;
    }

    // 全部用 u64 计算，避免 4K 尺寸下的整数溢出（宽度上限远大于 u32 的一半）。
    let fw = u64::from(frame_width);
    let fh = u64::from(frame_height);
    let sw = u64::try_from(surface_width).ok()?;
    let sh = u64::try_from(surface_height).ok()?;

    // width_limited = sw/fw <= sh/fh  ⇔  sw*fh <= sh*fw
    let (out_w, out_h) = if sw.saturating_mul(fh) <= sh.saturating_mul(fw) {
        // 宽度受限：横向铺满，纵向按比例。
        (sw, (fh.saturating_mul(sw) / fw).max(1))
    } else {
        ((fw.saturating_mul(sh) / fh).max(1).min(sw), sh)
    };
    let (out_w, out_h) = (out_w.max(1), out_h.max(1).min(sh));

    // `saturating_sub` 已保证非负，垂直居中即「从底部算起的余量一半」（GL 的 Y 轴向上）。
    let x = sw.saturating_sub(out_w) / 2;
    let y = sh.saturating_sub(out_h) / 2;

    Some(Rect {
        x: i32::try_from(x).unwrap_or(0),
        y: i32::try_from(y).unwrap_or(0),
        width: i32::try_from(out_w).unwrap_or(1),
        height: i32::try_from(out_h).unwrap_or(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_aspect_fills_surface() {
        assert_eq!(
            contain_rect(1920, 1080, 1920, 1080),
            Some(Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(
            contain_rect(1920, 1080, 3840, 2160),
            Some(Rect {
                x: 0,
                y: 0,
                width: 3840,
                height: 2160
            })
        );
    }

    #[test]
    fn wider_frame_letterboxes_vertically() {
        // 16:9 画面放进 4:3 的屏：宽度铺满、上下留黑边。
        let rect = contain_rect(1920, 1080, 1024, 768).unwrap();
        assert_eq!(rect.width, 1024);
        assert_eq!(rect.height, 576);
        assert_eq!(rect.x, 0);
        assert_eq!(rect.y, (768 - 576) / 2);
    }

    #[test]
    fn taller_frame_pillarboxes_horizontally() {
        // 竖屏远端画面放进横屏：高度铺满、左右留黑边。
        let rect = contain_rect(1080, 1920, 1920, 1080).unwrap();
        assert_eq!(rect.height, 1080);
        assert_eq!(rect.width, 607); // 1080 * 1080 / 1920 = 607.5，整数除法向下取整
        assert_eq!(rect.y, 0);
        assert_eq!(rect.x, (1920 - rect.width) / 2);
    }

    #[test]
    fn degenerate_inputs_return_none() {
        assert!(contain_rect(0, 1080, 100, 100).is_none());
        assert!(contain_rect(1920, 0, 100, 100).is_none());
        assert!(contain_rect(1920, 1080, 0, 100).is_none());
        assert!(contain_rect(1920, 1080, 100, 0).is_none());
        assert!(contain_rect(1920, 1080, -5, 100).is_none());
    }

    #[test]
    fn huge_frame_does_not_overflow() {
        let rect = contain_rect(u32::MAX, u32::MAX, 4096, 4096).unwrap();
        assert_eq!(rect.width, 4096);
        assert_eq!(rect.height, 4096);
        assert_eq!(rect.x, 0);
        assert_eq!(rect.y, 0);
    }

    #[test]
    fn tiny_surface_still_produces_positive_viewport() {
        let rect = contain_rect(3840, 2160, 1, 1).unwrap();
        assert!(rect.width >= 1);
        assert!(rect.height >= 1);
        assert!(rect.x >= 0);
        assert!(rect.y >= 0);
    }
}
