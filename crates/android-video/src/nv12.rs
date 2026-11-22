//! NV12 / I420 → **紧包 NV12** 的纯逻辑（去除 `row_stride` / `slice_height` 填充）。
//!
//! MediaCodec 的字节缓冲输出可能带行填充（`stride` 大于可见宽度）与切片填充
//! （`slice-height` 大于可见高度），而引擎要的是 `DecodedNv12` 那种
//! `Y(width*height) ‖ UV(width*height/2)`、**无填充**的紧包布局。本模块负责裁剪与
//! 重排，全部是纯函数，便于在非 Android 宿主上单测。
//!
//! # 布局约定
//!
//! - **NV12（半平面）**：Y 平面在上、交织 UV 平面紧随其后（UV 行步长与 Y 相同，
//!   都是 `width` 字节；这在 Android 解码器上普遍成立）。
//! - **I420（全平面）**：Y、U、V 三个独立平面；色度行步长取 `row_stride / 2`，
//!   色度平面高度取 `slice_height / 2`。
//!
//! 4:2:0 要求宽高为偶数；奇数尺寸一律报错（由控制器重建解码器）。

/// `COLOR_FormatYUV420Planar`（I420）。
pub(crate) const COLOR_FORMAT_I420: i32 = 19;
/// `COLOR_FormatYUV420SemiPlanar`（NV12）。
pub(crate) const COLOR_FORMAT_NV12: i32 = 21;
/// `COLOR_FormatYUV420Flexible`。
pub(crate) const COLOR_FORMAT_FLEXIBLE: i32 = 0x7F42_0888;

/// 解码器输出色度布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorFormat {
    /// 半平面（NV12）。
    SemiPlanar,
    /// 全平面（I420）。
    Planar,
    /// Flexible：按半平面处理（见模块文档的假设）。
    Flexible,
}

impl ColorFormat {
    /// 从 `AMediaFormat` 的 `color-format` 整数映射；未知值返回 `None`。
    pub(crate) fn from_i32(value: i32) -> Option<Self> {
        match value {
            COLOR_FORMAT_NV12 => Some(Self::SemiPlanar),
            COLOR_FORMAT_I420 => Some(Self::Planar),
            COLOR_FORMAT_FLEXIBLE => Some(Self::Flexible),
            _ => None,
        }
    }

    /// 人类可读名称（日志/遥测用）。
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SemiPlanar => "NV12",
            Self::Planar => "I420",
            Self::Flexible => "Flexible",
        }
    }
}

/// 平面几何：可见尺寸 + 缓冲区的行/切片步长。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlaneGeometry {
    /// 可见宽度（像素）。
    pub width: usize,
    /// 可见高度（像素）。
    pub height: usize,
    /// 每行字节数（`>= width`）。
    pub row_stride: usize,
    /// 每个切片（Y 平面）的行数（`>= height`）。
    pub slice_height: usize,
}

impl PlaneGeometry {
    /// 构造几何量；缺失或非法（小于可见尺寸）的步长回退到紧凑值。
    pub(crate) fn new(
        width: usize,
        height: usize,
        row_stride: Option<usize>,
        slice_height: Option<usize>,
    ) -> Self {
        let row_stride = row_stride.filter(|v| *v >= width).unwrap_or(width);
        let slice_height = slice_height.filter(|v| *v >= height).unwrap_or(height);
        Self {
            width,
            height,
            row_stride,
            slice_height,
        }
    }
}

/// 由「可见尺寸 + 缓冲尺寸 + 可选步长」构造平面几何。
///
/// 可见尺寸来自 `configure`（补丁/坐标以它为准），缓冲尺寸来自解码器上报的输出格式
/// （H.264 的 1080 常被对齐成 1088）。缓冲尺寸小于可见尺寸时返回错误，避免读到越界
/// 内容；步长缺失时回退到**缓冲**尺寸（不是可见尺寸，否则 UV 基址会算错）。
pub(crate) fn plane_geometry(
    width: usize,
    height: usize,
    buf_width: usize,
    buf_height: usize,
    row_stride: Option<usize>,
    slice_height: Option<usize>,
) -> Result<PlaneGeometry, &'static str> {
    if buf_width < width || buf_height < height {
        return Err("输出缓冲尺寸小于可见尺寸");
    }
    Ok(PlaneGeometry::new(
        width,
        height,
        Some(row_stride.unwrap_or(buf_width)),
        Some(slice_height.unwrap_or(buf_height)),
    ))
}

/// 紧包 NV12：`y` 长 `width*height`、`uv` 长 `width*height/2`，无填充。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackedNv12 {
    /// Y 平面。
    pub y: Vec<u8>,
    /// 交织 UV 平面（`U0 V0 U1 V1 ...`）。
    pub uv: Vec<u8>,
}

/// 校验 4:2:0 的可见尺寸。
fn check_even(geo: PlaneGeometry, what: &'static str) -> Result<(), &'static str> {
    if geo.width == 0 || geo.height == 0 {
        return Err(what);
    }
    if !geo.width.is_multiple_of(2) || !geo.height.is_multiple_of(2) {
        return Err(what);
    }
    Ok(())
}

/// 从带填充的 NV12 缓冲区裁出紧包 NV12。
///
/// # Errors
/// 尺寸为 0、奇数尺寸，或缓冲区不足以容纳声明的平面时返回静态错误描述。
pub(crate) fn pack_nv12(src: &[u8], geo: PlaneGeometry) -> Result<PackedNv12, &'static str> {
    check_even(geo, "NV12 尺寸必须为非零偶数")?;
    let PlaneGeometry {
        width,
        height,
        row_stride,
        slice_height,
    } = geo;
    let y_len = width.checked_mul(height).ok_or("NV12 Y 平面尺寸溢出")?;
    let uv_len = y_len / 2;
    let uv_base = row_stride
        .checked_mul(slice_height)
        .ok_or("NV12 UV 平面偏移溢出")?;

    let mut y = Vec::with_capacity(y_len);
    for row in 0..height {
        let start = row.checked_mul(row_stride).ok_or("NV12 Y 行偏移溢出")?;
        let end = start.checked_add(width).ok_or("NV12 Y 行长度溢出")?;
        let line = src.get(start..end).ok_or("NV12 缓冲区不足以容纳 Y 平面")?;
        y.extend_from_slice(line);
    }

    let mut uv = Vec::with_capacity(uv_len);
    for row in 0..(height / 2) {
        let row_offset = row
            .checked_mul(row_stride)
            .and_then(|v| uv_base.checked_add(v))
            .ok_or("NV12 UV 行偏移溢出")?;
        let end = row_offset.checked_add(width).ok_or("NV12 UV 行长度溢出")?;
        let line = src
            .get(row_offset..end)
            .ok_or("NV12 缓冲区不足以容纳 UV 平面")?;
        uv.extend_from_slice(line);
    }

    Ok(PackedNv12 { y, uv })
}

/// 从带填充的 I420 缓冲区转出紧包 NV12（交织 U/V）。
///
/// # Errors
/// 尺寸为 0、奇数尺寸，或缓冲区不足以容纳声明的平面时返回静态错误描述。
pub(crate) fn pack_i420(src: &[u8], geo: PlaneGeometry) -> Result<PackedNv12, &'static str> {
    check_even(geo, "I420 尺寸必须为非零偶数")?;
    let PlaneGeometry {
        width,
        height,
        row_stride,
        slice_height,
    } = geo;
    let y_len = width.checked_mul(height).ok_or("I420 Y 平面尺寸溢出")?;
    let cw = width / 2;
    let ch = height / 2;
    let c_stride = row_stride / 2;
    if c_stride < cw {
        return Err("I420 色度行步长小于可见色度宽度");
    }
    let u_base = row_stride
        .checked_mul(slice_height)
        .ok_or("I420 U 平面偏移溢出")?;
    let c_plane_rows = slice_height / 2;
    let c_plane = c_stride
        .checked_mul(c_plane_rows)
        .ok_or("I420 色度平面尺寸溢出")?;
    let v_base = u_base.checked_add(c_plane).ok_or("I420 V 平面偏移溢出")?;

    let mut y = Vec::with_capacity(y_len);
    for row in 0..height {
        let start = row.checked_mul(row_stride).ok_or("I420 Y 行偏移溢出")?;
        let end = start.checked_add(width).ok_or("I420 Y 行长度溢出")?;
        let line = src.get(start..end).ok_or("I420 缓冲区不足以容纳 Y 平面")?;
        y.extend_from_slice(line);
    }

    let mut uv = Vec::with_capacity(y_len / 2);
    for row in 0..ch {
        let row_offset = row.checked_mul(c_stride).ok_or("I420 色度行偏移溢出")?;
        let u_row = u_base.checked_add(row_offset).ok_or("I420 U 行偏移溢出")?;
        let v_row = v_base.checked_add(row_offset).ok_or("I420 V 行偏移溢出")?;
        let u_end = u_row.checked_add(cw).ok_or("I420 U 行长度溢出")?;
        let v_end = v_row.checked_add(cw).ok_or("I420 V 行长度溢出")?;
        let u_line = src
            .get(u_row..u_end)
            .ok_or("I420 缓冲区不足以容纳 U 平面")?;
        let v_line = src
            .get(v_row..v_end)
            .ok_or("I420 缓冲区不足以容纳 V 平面")?;
        for (u, v) in u_line.iter().zip(v_line.iter()) {
            uv.push(*u);
            uv.push(*v);
        }
    }

    Ok(PackedNv12 { y, uv })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::pedantic,
    reason = "测试代码使用断言与 unwrap 更直观"
)]
mod tests {
    use super::*;

    #[test]
    fn color_format_maps_known_values() {
        assert_eq!(ColorFormat::from_i32(21), Some(ColorFormat::SemiPlanar));
        assert_eq!(ColorFormat::from_i32(19), Some(ColorFormat::Planar));
        assert_eq!(
            ColorFormat::from_i32(0x7F42_0888),
            Some(ColorFormat::Flexible)
        );
        assert_eq!(ColorFormat::from_i32(99), None);
    }

    #[test]
    fn geometry_clamps_invalid_strides_to_tight() {
        let geo = PlaneGeometry::new(4, 4, Some(2), Some(1));
        assert_eq!(geo.row_stride, 4);
        assert_eq!(geo.slice_height, 4);
        let geo = PlaneGeometry::new(4, 4, None, None);
        assert_eq!((geo.row_stride, geo.slice_height), (4, 4));
    }

    #[test]
    fn tight_nv12_is_copied_verbatim() {
        // 4x4 NV12：Y 16 字节 + UV 8 字节。
        let mut src: Vec<u8> = (0..16).collect();
        src.extend_from_slice(&[100, 101, 102, 103, 104, 105, 106, 107]);
        let geo = PlaneGeometry::new(4, 4, None, None);
        let packed = pack_nv12(&src, geo).expect("紧包 NV12 应成功");
        assert_eq!(packed.y, (0..16).collect::<Vec<u8>>());
        assert_eq!(packed.uv, vec![100, 101, 102, 103, 104, 105, 106, 107]);
    }

    #[test]
    fn strided_nv12_crops_padding() {
        // 可见 2x2，但 stride=4、slice=3：Y 占 4*3=12 字节，UV 从偏移 12 起。
        // 布局（每行 4 字节，前 2 字节可见，后 2 字节填充）：
        //   Y 行0: 10 11  X X
        //   Y 行1: 12 13  X X
        //   Y 行2(填充): X X X X
        //   UV 行0: 20 21  X X
        //   UV 行1(填充): X X X X
        let src = [
            10, 11, 0xFF, 0xFF, // Y row 0
            12, 13, 0xFF, 0xFF, // Y row 1
            0xEE, 0xEE, 0xEE, 0xEE, // Y row 2（切片填充）
            20, 21, 0xFF, 0xFF, // UV row 0
            0xEE, 0xEE, 0xEE, 0xEE, // UV row 1（切片填充）
        ];
        let geo = PlaneGeometry::new(2, 2, Some(4), Some(3));
        let packed = pack_nv12(&src, geo).expect("带填充 NV12 应成功");
        assert_eq!(packed.y, vec![10, 11, 12, 13]);
        assert_eq!(packed.uv, vec![20, 21]);
    }

    #[test]
    fn tight_i420_is_interleaved_to_nv12() {
        // 4x4 I420：Y 16 字节、U 4 字节、V 4 字节。
        let mut src: Vec<u8> = (0..16).collect();
        src.extend_from_slice(&[50, 51, 52, 53]); // U
        src.extend_from_slice(&[60, 61, 62, 63]); // V
        let geo = PlaneGeometry::new(4, 4, None, None);
        let packed = pack_i420(&src, geo).expect("I420 转 NV12 应成功");
        assert_eq!(packed.y, (0..16).collect::<Vec<u8>>());
        assert_eq!(packed.uv, vec![50, 60, 51, 61, 52, 62, 53, 63]);
    }

    #[test]
    fn strided_i420_crops_chroma_padding() {
        // 可见 4x2，行步长 6（每行末 2 字节是填充）、切片高 2（无额外行）。
        // 色度行步长 = 6/2 = 3，色度宽 = 4/2 = 2（第 3 字节是填充）。
        // 布局：Y 6*2=12 字节；U 从 12 起 3 字节；V 从 15 起 3 字节。
        let mut src = vec![1u8, 2, 3, 4, 0xFF, 0xFF, 5, 6, 7, 8, 0xFF, 0xFF]; // Y 两行
        src.extend_from_slice(&[70, 71, 0xFF]); // U（可见 2 列）
        src.extend_from_slice(&[80, 81, 0xFF]); // V（可见 2 列）
        let geo = PlaneGeometry::new(4, 2, Some(6), Some(2));
        let packed = pack_i420(&src, geo).expect("带填充 I420 应成功");
        assert_eq!(packed.y, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(packed.uv, vec![70, 80, 71, 81]);
    }

    #[test]
    fn odd_dimensions_are_rejected() {
        let src = vec![0u8; 64];
        let geo = PlaneGeometry::new(3, 4, None, None);
        assert!(pack_nv12(&src, geo).is_err());
        assert!(pack_i420(&src, geo).is_err());
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        let src: Vec<u8> = Vec::new();
        assert!(pack_nv12(&src, PlaneGeometry::new(0, 0, None, None)).is_err());
        assert!(pack_i420(&src, PlaneGeometry::new(0, 0, None, None)).is_err());
    }

    #[test]
    fn short_nv12_buffer_is_rejected() {
        // 需要 4x4 NV12 = 16 + 8 = 24 字节，这里只有 20。
        let src = vec![0u8; 20];
        let geo = PlaneGeometry::new(4, 4, None, None);
        assert!(pack_nv12(&src, geo).is_err());
    }

    #[test]
    fn short_i420_buffer_is_rejected() {
        let src = vec![0u8; 20]; // 需要 24
        let geo = PlaneGeometry::new(4, 4, None, None);
        assert!(pack_i420(&src, geo).is_err());
    }

    #[test]
    fn plane_geometry_keeps_visible_size_and_buffer_padding_separate() {
        // H.264 的 1080 常被解码成 1088：输出帧仍是 1080，但平面步长要用缓冲值。
        let geo = plane_geometry(1920, 1080, 1920, 1088, None, None).expect("应合法");
        assert_eq!(geo.width, 1920);
        assert_eq!(geo.height, 1080);
        assert_eq!(geo.row_stride, 1920);
        assert_eq!(geo.slice_height, 1088);
        // 显式步长优先于缓冲尺寸。
        let geo = plane_geometry(1920, 1080, 1920, 1088, Some(2048), Some(1088)).expect("应合法");
        assert_eq!(geo.row_stride, 2048);
    }

    #[test]
    fn plane_geometry_rejects_smaller_buffer() {
        assert!(plane_geometry(1920, 1080, 1920, 1072, None, None).is_err());
        assert!(plane_geometry(1920, 1080, 1912, 1088, None, None).is_err());
    }

    #[test]
    fn nv12_crops_aligned_height_and_places_uv_at_buffer_height() {
        // 可见 4x2，缓冲 4x4（1080→1088 的最小复现）：UV 基址必须是 4*4 而不是 4*2。
        let width = 4;
        let height = 2;
        let buf_height = 4;
        let y_len = width * buf_height;
        let uv_len = width * (buf_height / 2);
        let mut src = vec![0u8; y_len + uv_len];
        for row in 0..height {
            for col in 0..width {
                src[row * width + col] = 0x10;
            }
        }
        for byte in &mut src[y_len..] {
            *byte = 0x80;
        }
        let geo = plane_geometry(width, height, width, buf_height, None, None).expect("应合法");
        let packed = pack_nv12(&src, geo).expect("应成功");
        assert_eq!(packed.y, vec![0x10; width * height]);
        assert_eq!(packed.uv, vec![0x80; width * height / 2]);
    }
}
