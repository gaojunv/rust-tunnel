//! NV12 纹理上传的几何计算（纯逻辑，主机可单测）。
//!
//! 渲染线程把一帧紧包 NV12 拆成两个平面纹理（Y `GL_R8`、交织 UV `GL_RG8`），补丁则用
//! `glTexSubImage2D` 覆写矩形区域。这些偏移/尺寸一旦算错，画面会**错位或颜色错乱**，
//! 而且真机上表现为「看起来像是解码器坏了」——所以把全部几何算术放在这里、逐条单测，
//! GL 那一半（[`super::render`]）只负责把结果喂给驱动。
//!
//! # 平面布局约定
//!
//! - 整帧：`Y(width*height) ‖ UV(width*height/2)`，无行填充（与 `DecodedNv12` 一致）；
//! - 补丁：`Y(rect.w*rect.h) ‖ UV(rect.w*rect.h/2)`，同样无行填充（与 `ScreenPatch` 一致）；
//! - UV 平面每个「像素」是一个 `(U,V)` 字节对，覆盖 2×2 个 luma 像素，因此**目标坐标与
//!   宽高都除以 2**，而**源数据每个补丁行仍有 `rect.w` 字节**（`rect.w/2` 对 × 2 字节）。
//!   GL 的 `UNPACK_ROW_LENGTH` 保持默认 0 时，行宽 = 上传宽度 × 每像素字节 = `rect.w/2 * 2`，
//!   正好等于 `rect.w`，所以不需要设置它。

use rust_tunnel_remote::screen::ScreenPatch;

/// 一个平面的一次 `glTexSubImage2D` 参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlaneUpload {
    /// 目标纹理左上角 X（像素）。
    pub(crate) x: i32,
    /// 目标纹理左上角 Y（像素）。
    pub(crate) y: i32,
    /// 上传宽度（目标纹理像素）。
    pub(crate) width: i32,
    /// 上传高度（目标纹理像素）。
    pub(crate) height: i32,
    /// 源数据在补丁 `nv12` 字节数组里的起始偏移。
    pub(crate) offset: usize,
    /// 本平面使用的字节数（`height` 行 × 每行字节）。
    pub(crate) len: usize,
}

/// 一个补丁的两个平面上传参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PatchUpload {
    /// Y 平面：紧包、每行 `rect.width` 字节。
    pub(crate) y: PlaneUpload,
    /// UV 平面：交织、每行仍是 `rect.width` 字节（`rect.width/2` 对）。
    pub(crate) uv: PlaneUpload,
}

/// 校验一帧紧包 NV12 的尺寸与长度，返回 `(width, height)`。
///
/// 要求宽高**非零且为偶数**（UV 是半分辨率，奇数会让色度块错位），且两个平面长度正好
/// 是 `w*h` 与 `w*h/2`。任何一条不满足都返回 `None`——调用方据此丢弃这一帧，而不是把
/// 错位的内容传进纹理。
pub(crate) fn nv12_frame_dims(
    width: u32,
    height: u32,
    y_len: usize,
    uv_len: usize,
) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return None;
    }
    let pixels = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?;
    if y_len != pixels || uv_len != pixels / 2 {
        return None;
    }
    Some((width, height))
}

/// 校验一个补丁能否叠进 `frame_width × frame_height` 的整帧，并算出两个平面的上传参数。
///
/// 判据与 `DecodedNv12::patch_geometry` **一字不差**（偶数坐标/宽高、非零、不越界、NV12
/// 长度正好 `w*h*3/2`）：引擎侧与渲染侧对「什么补丁合法」必须只有一种答案，否则会出现
/// 「引擎认为拼得上、控制端却丢帧」的静默降级。
pub(crate) fn patch_upload(
    frame_width: u32,
    frame_height: u32,
    rect: &ScreenPatch,
) -> Option<PatchUpload> {
    if frame_width == 0 || frame_height == 0 || !frame_width.is_multiple_of(2) {
        return None;
    }
    if !frame_height.is_multiple_of(2) {
        return None;
    }
    let (w, h) = (rect.width, rect.height);
    if w == 0
        || h == 0
        || !w.is_multiple_of(2)
        || !h.is_multiple_of(2)
        || !rect.x.is_multiple_of(2)
        || !rect.y.is_multiple_of(2)
    {
        return None;
    }
    let right = rect.x.checked_add(w)?;
    let bottom = rect.y.checked_add(h)?;
    if right > frame_width || bottom > frame_height {
        return None;
    }

    let y_len = usize::try_from(w)
        .ok()?
        .checked_mul(usize::try_from(h).ok()?)?;
    let uv_len = y_len / 2;
    if rect.nv12.len() != y_len.checked_add(uv_len)? {
        return None;
    }

    let half_w = w / 2;
    let half_h = h / 2;
    Some(PatchUpload {
        y: PlaneUpload {
            x: i32::try_from(rect.x).ok()?,
            y: i32::try_from(rect.y).ok()?,
            width: i32::try_from(w).ok()?,
            height: i32::try_from(h).ok()?,
            offset: 0,
            len: y_len,
        },
        uv: PlaneUpload {
            // 色度块的左上角 = luma 坐标的一半；宽高同理。
            x: i32::try_from(rect.x / 2).ok()?,
            y: i32::try_from(rect.y / 2).ok()?,
            width: i32::try_from(half_w).ok()?,
            height: i32::try_from(half_h).ok()?,
            offset: y_len,
            len: uv_len,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个合法补丁（`w*h*3/2` 字节，内容不重要）。
    fn patch(x: u32, y: u32, w: u32, h: u32) -> ScreenPatch {
        let len = (w as usize) * (h as usize) * 3 / 2;
        ScreenPatch {
            x,
            y,
            width: w,
            height: h,
            nv12: vec![0x5A; len],
        }
    }

    #[test]
    fn frame_dims_validate_lengths_and_evenness() {
        // 8×6：Y 48 字节、UV 24 字节。
        assert_eq!(nv12_frame_dims(8, 6, 48, 24), Some((8, 6)));
        // 奇数宽/高、零、长度不符都拒绝。
        for (w, h, y_len, uv_len) in [
            (7, 6, 42, 21),
            (8, 5, 40, 20),
            (0, 6, 0, 0),
            (8, 0, 0, 0),
            (8, 6, 47, 24),
            (8, 6, 48, 23),
            (8, 6, 48, 25),
        ] {
            assert_eq!(
                nv12_frame_dims(w, h, y_len, uv_len),
                None,
                "{w}x{h} 应被拒绝"
            );
        }
    }

    #[test]
    fn patch_uploads_y_at_rect_and_uv_at_half_coordinates() {
        // 整帧 16×8，补丁在 (4, 2) 处 4×2（Y 8 字节、UV 4 字节）。
        let upload = patch_upload(16, 8, &patch(4, 2, 4, 2)).expect("合法补丁");
        assert_eq!(
            upload.y,
            PlaneUpload {
                x: 4,
                y: 2,
                width: 4,
                height: 2,
                offset: 0,
                len: 8,
            }
        );
        // UV：目标 (2, 1)、宽 2 像素、高 1 行；源数据从 Y 之后开始。
        assert_eq!(
            upload.uv,
            PlaneUpload {
                x: 2,
                y: 1,
                width: 2,
                height: 1,
                offset: 8,
                len: 4,
            }
        );
    }

    /// UV 的偏移/宽高必须是 luma 的一半（色度块坐标），且源行宽仍是 `rect.width` 字节：
    /// `uv.width * 2 == rect.width`、`uv.len == uv.height * rect.width`。
    #[test]
    fn uv_geometry_is_half_resolution_with_luma_row_stride() {
        let rect = patch(0, 0, 6, 4);
        let upload = patch_upload(64, 32, &rect).expect("合法补丁");
        assert_eq!(upload.uv.width, 3);
        assert_eq!(upload.uv.height, 2);
        assert_eq!(upload.uv.x, 0);
        assert_eq!(upload.uv.y, 0);
        // 2 行 × 每行 6 字节 = 12 字节（= w*h/2）。
        assert_eq!(upload.uv.len, 12);
        assert_eq!(upload.uv.offset, 24);
        // 每行字节数 = 上传宽度 * 2（RG8）= 源行宽 rect.width。
        assert_eq!(
            usize::try_from(upload.uv.width).unwrap_or(0) * 2,
            usize::try_from(rect.width).unwrap_or(0)
        );
        assert_eq!(
            upload.uv.len,
            usize::try_from(upload.uv.height).unwrap_or(0)
                * usize::try_from(rect.width).unwrap_or(0)
        );
        // 覆盖的字节范围正好是补丁的后半段。
        assert_eq!(upload.uv.offset, 6 * 4);
        assert_eq!(upload.uv.offset + upload.uv.len, rect.nv12.len());
    }

    #[test]
    fn patch_rejects_odd_geometry_out_of_bounds_and_bad_lengths() {
        let valid = patch(4, 2, 4, 2);
        let cases = [
            (
                ScreenPatch {
                    width: 3,
                    ..valid.clone()
                },
                "奇数宽度",
            ),
            (
                ScreenPatch {
                    height: 3,
                    ..valid.clone()
                },
                "奇数高度",
            ),
            (
                ScreenPatch {
                    x: 3,
                    ..valid.clone()
                },
                "奇数 X",
            ),
            (
                ScreenPatch {
                    y: 3,
                    ..valid.clone()
                },
                "奇数 Y",
            ),
            (
                ScreenPatch {
                    x: 14,
                    ..valid.clone()
                },
                "右越界",
            ),
            (
                ScreenPatch {
                    y: 7,
                    ..valid.clone()
                },
                "下越界",
            ),
            (
                ScreenPatch {
                    nv12: vec![0; 11],
                    ..valid.clone()
                },
                "长度不足",
            ),
            (
                ScreenPatch {
                    nv12: vec![0; 13],
                    ..valid.clone()
                },
                "长度超出",
            ),
            (
                ScreenPatch {
                    width: 0,
                    ..valid.clone()
                },
                "零宽",
            ),
            (
                ScreenPatch {
                    height: 0,
                    ..valid.clone()
                },
                "零高",
            ),
        ];
        for (rect, reason) in cases {
            assert_eq!(patch_upload(16, 8, &rect), None, "{reason} 应被拒绝");
        }
        // 整帧尺寸非法（0 / 奇数）同样拒绝。
        assert_eq!(patch_upload(0, 8, &valid), None);
        assert_eq!(patch_upload(15, 8, &valid), None);
        assert_eq!(patch_upload(16, 7, &valid), None);
        // 合法补丁贴着右下角也必须通过。
        assert!(patch_upload(16, 8, &patch(12, 6, 4, 2)).is_some());
    }

    /// 尺寸不符的补丁由调用方（渲染线程）在比较纹理尺寸时拒绝；这里锁住「几何本身合法」，
    /// 避免把「尺寸比较」和「几何校验」混成一条规则。
    #[test]
    fn patch_geometry_is_independent_of_target_buffer_size() {
        let rect = patch(0, 0, 4, 2);
        // 2×2 的补丁放在 1920×1080 与 3840×2160 上都合法（目标尺寸由调用方另行比较）。
        assert!(patch_upload(1920, 1080, &rect).is_some());
        assert!(patch_upload(3840, 2160, &rect).is_some());
        // 超出小缓冲时几何层就拒绝。
        assert!(patch_upload(3, 2, &rect).is_none());
    }
}
