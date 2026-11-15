//! 脏矩形**补丁模式**的采集侧：从脏格点网格到编码尺寸的紧包 NV12 矩形。
//!
//! # 为什么要补丁
//!
//! H.264 的整帧编码在「小面积变化」场景下仍然要为整帧付码率：一个闪烁的终端光标、
//! 一行滚动日志，都值得只发那块矩形。采集层已经有 [`DirtyGrid`]（320×180 的变化格点），
//! 本模块把它变成若干**编码尺寸坐标系**的矩形，并且只把这些矩形的像素从 GPU 读回、
//! 缩放/转成 NV12 交给编码器——其余像素一个字节都不搬。
//!
//! # 两段式设计
//!
//! 1. [`extract_rects`] 是**纯逻辑**（不碰 GPU，可单测）：膨胀 → 8 连通分量 → 合并 →
//!    映射到目标坐标 → 外扩/偶数对齐 → 再合并。返回 `None` 表示这一帧「不适合补丁」
//!    （矩形过多/面积超预算/网格非法），调用方退回全帧视频。
//! 2. [`extract_patches`] 做 GPU 读回：把每个目标矩形对应的**源矩形**用
//!    `CopySubresourceRegion` 拷进**一张**纵向堆叠的 staging 图集，只 `Map` 一次，
//!    再逐矩形做**绝对坐标**盒式缩放（与整帧缩放逐像素一致，不会在矩形边缘出现接缝）。
//!
//! # 坐标与偶数约束
//!
//! 网格是 320×180，目标编码尺寸通常更大（例如 1920×1080），所以一个格点对应目标上的
//! 一块区域。所有输出矩形的 `x`/`y`/`width`/`height` **都是偶数**：NV12 的 UV 平面是
//! 半分辨率，奇数边界会让色度块错位。对齐是**向外**取偶（`x &= !1`、右/下边界向上取偶），
//! 保证不裁掉任何被标记的像素。
//!
//! # 读回成本
//!
//! staging 纹理按 `(宽, 高, 格式)` 缓存在线程局部（采集通常固定在同一个 worker 线程）。
//! 图集高度超过 [`MAX_ATLAS_HEIGHT`] 时直接返回 `None`：与其为一张巨大的 staging 纹理
//! 付显存和拷贝代价，不如让引擎走全帧编码。

use super::{CapturedTexture, DirtyGrid};
use crate::mft_encoder::convert::rgba_to_nv12;
use crate::WinVideoError;
use std::cell::RefCell;
use std::sync::Arc;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

/// 相邻脏格点合并的最大间隔（格点数）。
///
/// 膨胀已经把 3×3 邻域连成一块，这里再把相隔 1–2 个空格点的两块并进同一个包围盒——
/// 既避免为同一段文字里的字间空隙各发一个矩形，又不会把整屏都并成一块。
const MERGE_GAP: u32 = 2;

/// 堆叠图集的高度上限（像素）。超过就放弃补丁，退回全帧。
const MAX_ATLAS_HEIGHT: u32 = 4096;

/// 补丁提取参数（矩形数上限、面积预算、外扩像素）。
///
/// 默认值面向「桌面 UI 小面积变化」：最多 16 块、总覆盖面积不超过目标的 20%、每块向外
/// 扩 4 像素（盖住抗锯齿/阴影边缘，避免补丁之间留下可见接缝）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchParams {
    /// 允许的最大矩形数。超过即返回 `None`（矩形太多说明画面已经不「局部」了）。
    pub max_rects: usize,
    /// 允许的最大总覆盖面积（千分比，`0..=1000`）。
    pub max_area_permille: u16,
    /// 每个矩形向四周外扩的目标像素数（再按偶数对齐）。
    pub expand_px: u32,
}

impl Default for PatchParams {
    fn default() -> Self {
        Self {
            max_rects: 16,
            max_area_permille: 200,
            expand_px: 4,
        }
    }
}

/// 目标（编码尺寸）坐标系里的一个矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// 左上角 X（偶数）。
    pub x: u32,
    /// 左上角 Y（偶数）。
    pub y: u32,
    /// 宽度（偶数，且非零）。
    pub width: u32,
    /// 高度（偶数，且非零）。
    pub height: u32,
}

impl Rect {
    /// 右边界（不含）。
    fn right(self) -> u32 {
        self.x.saturating_add(self.width)
    }

    /// 下边界（不含）。
    fn bottom(self) -> u32 {
        self.y.saturating_add(self.height)
    }

    /// 两个矩形的包围盒。
    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self {
            x,
            y,
            width: right.saturating_sub(x),
            height: bottom.saturating_sub(y),
        }
    }

    /// 是否重叠或相邻（含对角相接）。
    fn touches(self, other: Self) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }
}

/// 一个脏矩形补丁：目标坐标 + 紧包 NV12（`Y` 平面 + 交织 `UV` 平面）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchRect {
    /// 目标帧缓冲内的左上角 X（偶数）。
    pub x: u32,
    /// 目标帧缓冲内的左上角 Y（偶数）。
    pub y: u32,
    /// 宽度（偶数）。
    pub width: u32,
    /// 高度（偶数）。
    pub height: u32,
    /// 紧包 NV12，长度 = `width * height * 3 / 2`（无行填充）。
    pub nv12: Vec<u8>,
}

/// 格点坐标系的闭区间包围盒（`x1`/`y1` 含）。
#[derive(Debug, Clone, Copy)]
struct CellBox {
    /// 最小 X 格点。
    x0: u32,
    /// 最小 Y 格点。
    y0: u32,
    /// 最大 X 格点（含）。
    x1: u32,
    /// 最大 Y 格点（含）。
    y1: u32,
}

impl CellBox {
    /// 两个格点包围盒的并。
    fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
}

/// 把脏格点网格转成目标坐标系的矩形列表。
///
/// `src` 是源画面尺寸（只用于调用方自洽性；映射只依赖网格与 `dst`）。
///
/// 返回 `None` 表示这一帧不适合补丁：网格非法（长度与宽高不符）、目标尺寸太小（无法
/// 构成偶数的 NV12 矩形）、矩形数超过 [`PatchParams::max_rects`]、或覆盖面积超过
/// [`PatchParams::max_area_permille`]。空网格（没有变化）返回 `Some(vec![])`——那是
/// 「本拍只有保活、没有补丁」的合法结果，不是失败。
///
/// 纯逻辑、无 GPU 依赖，可单测。
#[must_use]
pub fn extract_rects(
    grid: &DirtyGrid,
    _src: (u32, u32),
    dst: (u32, u32),
    params: &PatchParams,
) -> Option<Vec<Rect>> {
    let grid_width = usize::try_from(grid.width).ok()?;
    let grid_height = usize::try_from(grid.height).ok()?;
    let dst_width = usize::try_from(dst.0).ok()?;
    let dst_height = usize::try_from(dst.1).ok()?;
    // 宽度/高度 < 2 连一个合法的 NV12 块都放不下（UV 平面是半分辨率）。
    if grid_width == 0 || grid_height == 0 || dst_width < 2 || dst_height < 2 {
        return None;
    }
    if grid.cells.len() != grid_width.checked_mul(grid_height)? {
        return None;
    }
    if grid.cells.iter().all(|&cell| cell == 0) {
        return Some(Vec::new());
    }

    let dilated = dilate(&grid.cells, grid_width, grid_height);
    let mut boxes = connected_boxes(&dilated, grid_width, grid_height);
    merge_boxes(&mut boxes);

    // 偶数上界：目标尺寸本身应当是偶数（编码器要求），这里再兜一层。
    let limit_width = u32::try_from(dst_width & !1).ok()?;
    let limit_height = u32::try_from(dst_height & !1).ok()?;
    let mut rects = Vec::with_capacity(boxes.len());
    for cell_box in boxes {
        if let Some(rect) = map_box(
            cell_box,
            grid.width,
            grid.height,
            dst,
            params.expand_px,
            limit_width,
            limit_height,
        ) {
            if rect.width > 0 && rect.height > 0 {
                rects.push(rect);
            }
        }
    }
    merge_rects(&mut rects);

    if rects.len() > params.max_rects {
        return None;
    }
    let area = rects.iter().fold(0_u64, |sum, rect| {
        sum.saturating_add(u64::from(rect.width).saturating_mul(u64::from(rect.height)))
    });
    let budget = u64::from(dst.0)
        .saturating_mul(u64::from(dst.1))
        .saturating_mul(u64::from(params.max_area_permille));
    if area.saturating_mul(1000) > budget {
        return None;
    }
    Some(rects)
}

/// 从采集纹理里读出给定网格对应的脏矩形，并转成目标尺寸的紧包 NV12。
///
/// 返回 `Ok(None)` 表示这一帧不适合补丁（[`extract_rects`] 判否、图集过高、Map 不可用）；
/// 调用方应**回到全帧视频路径**，不要把它当成采集失败。
///
/// # Errors
/// 纹理不是 BGRA8 之外的真实错误（`CopySubresourceRegion`/`Map`/NV12 转换失败）才返回错误。
pub fn extract_patches(
    captured: &CapturedTexture,
    grid: &DirtyGrid,
    target_width: u32,
    target_height: u32,
    params: &PatchParams,
) -> Result<Option<Vec<PatchRect>>, WinVideoError> {
    let src = (captured.width, captured.height);
    let dst = (target_width, target_height);
    let Some(rects) = extract_rects(grid, src, dst, params) else {
        return Ok(None);
    };
    if rects.is_empty() {
        return Ok(Some(Vec::new()));
    }

    // 目标矩形 → 源矩形：覆盖目标矩形需要的每一个源像素（floor 起点 / ceil 终点）。
    let mut source_rects = Vec::with_capacity(rects.len());
    let mut atlas_width = 0_u32;
    let mut atlas_height = 0_u64;
    for rect in &rects {
        let sx0 = scale_coord(rect.x, src.0, dst.0, false).min(src.0);
        let sx1 = scale_coord(rect.right(), src.0, dst.0, true).min(src.0);
        let sy0 = scale_coord(rect.y, src.1, dst.1, false).min(src.1);
        let sy1 = scale_coord(rect.bottom(), src.1, dst.1, true).min(src.1);
        let width = sx1.saturating_sub(sx0);
        let height = sy1.saturating_sub(sy0);
        if width == 0 || height == 0 {
            return Ok(None);
        }
        atlas_width = atlas_width.max(width);
        atlas_height = atlas_height.saturating_add(u64::from(height));
        source_rects.push(Rect {
            x: sx0,
            y: sy0,
            width,
            height,
        });
    }
    let Ok(atlas_height) = u32::try_from(atlas_height) else {
        return Ok(None);
    };
    if atlas_height > MAX_ATLAS_HEIGHT {
        return Ok(None);
    }

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `captured.texture` 是有效纹理；`desc` 指向本栈帧。
    unsafe { captured.texture.GetDesc(&raw mut desc) };
    let staging = PATCH_READER.with(|cell| {
        cell.borrow_mut()
            .staging(captured, desc.Format, atlas_width, atlas_height)
    })?;

    // 所有矩形拷进同一张纵向堆叠的 staging：每块只做一次 GPU→GPU 拷贝。
    let context = &captured.device.context;
    let mut y_offset = 0_u32;
    for source in &source_rects {
        let boxed = D3D11_BOX {
            left: source.x,
            top: source.y,
            front: 0,
            right: source.right(),
            bottom: source.bottom(),
            back: 1,
        };
        // SAFETY: 两张纹理同设备、同格式；`boxed` 指向本栈帧，坐标已钳在源纹理内。
        unsafe {
            context.CopySubresourceRegion(
                &staging,
                0,
                0,
                y_offset,
                0,
                &captured.texture,
                captured.subresource,
                Some(&raw const boxed),
            );
        }
        y_offset = y_offset.saturating_add(source.height);
    }

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: `staging` 是 STAGING + CPU_ACCESS_READ；`mapped` 指向本栈帧。
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }
        .map_err(|err| WinVideoError::Capture(format!("补丁图集 Map 失败: {err}")))?;
    let result = convert_patches(&mapped, &source_rects, &rects, src, dst);
    // SAFETY: 与上面的 Map 严格配对。
    unsafe { context.Unmap(&staging, 0) };
    result
}

/// 从 Map 出来的图集里逐矩形缩放 + 转 NV12。
fn convert_patches(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    source_rects: &[Rect],
    rects: &[Rect],
    src: (u32, u32),
    dst: (u32, u32),
) -> Result<Option<Vec<PatchRect>>, WinVideoError> {
    let pitch = usize::try_from(mapped.RowPitch).unwrap_or(0);
    if mapped.pData.is_null() || pitch == 0 {
        return Ok(None);
    }
    let total_rows: u64 = source_rects.iter().map(|rect| u64::from(rect.height)).sum();
    let Ok(total_rows) = usize::try_from(total_rows) else {
        return Ok(None);
    };
    let source_len = pitch.saturating_mul(total_rows);
    // SAFETY: Map 成功后驱动保证至少 `RowPitch × 图集高度` 字节可读，`total_rows` 正是
    // 图集高度（各矩形高度之和）。
    let source = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), source_len) };

    let mut rgba = Vec::new();
    let mut patches = Vec::with_capacity(rects.len());
    let mut y_offset = 0_usize;
    for (rect, source_rect) in rects.iter().zip(source_rects) {
        let rows = usize::try_from(source_rect.height).unwrap_or(0);
        let start = y_offset.saturating_mul(pitch);
        let end = start.saturating_add(pitch.saturating_mul(rows));
        let Some(rect_bgra) = source.get(start..end) else {
            return Ok(None);
        };
        scale_bgra_rect_to_rgba(
            rect_bgra,
            pitch,
            (source_rect.x, source_rect.y),
            src,
            *rect,
            dst,
            &mut rgba,
        )?;
        let mut nv12 = Vec::new();
        rgba_to_nv12(&rgba, rect.width, rect.height, &mut nv12)?;
        patches.push(PatchRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            nv12,
        });
        y_offset = y_offset.saturating_add(rows);
    }
    Ok(Some(patches))
}

/// 把一个目标像素的**绝对**盒式缩放写进 `out`。
///
/// 与整帧路径 [`crate::pixels::scale_bgra_to_rgba`] 用同一套映射（`dx * src / dst`），
/// 因此对同一个目标像素，补丁路径与全帧路径的输出**逐字节相等**——这正是「补丁拼回
/// 控制器持久帧缓冲后没有接缝」的前提。`rect_bgra` 是图集里从 `(src_origin)` 开始的
/// 一块源像素，行距为 `rect_stride`。
///
/// # Errors
/// 尺寸为 0、`rect_stride` 小于一个像素、或回读缓冲不足以覆盖某个源像素时返回错误。
pub(crate) fn scale_bgra_rect_to_rgba(
    rect_bgra: &[u8],
    rect_stride: usize,
    src_origin: (u32, u32),
    src_full: (u32, u32),
    dst_rect: Rect,
    dst_full: (u32, u32),
    out: &mut Vec<u8>,
) -> Result<(), WinVideoError> {
    let (src_width, src_height) = (
        usize::try_from(src_full.0).unwrap_or(0),
        usize::try_from(src_full.1).unwrap_or(0),
    );
    let (dst_width, dst_height) = (
        usize::try_from(dst_full.0).unwrap_or(0),
        usize::try_from(dst_full.1).unwrap_or(0),
    );
    let rect_width = usize::try_from(dst_rect.width).unwrap_or(0);
    let rect_height = usize::try_from(dst_rect.height).unwrap_or(0);
    if src_width == 0
        || src_height == 0
        || dst_width == 0
        || dst_height == 0
        || rect_width == 0
        || rect_height == 0
    {
        return Err(WinVideoError::Config(format!(
            "补丁缩放尺寸非法（源 {src_full:?}，目标 {dst_full:?}，矩形 {}x{}）",
            dst_rect.width, dst_rect.height
        )));
    }
    if rect_stride < 4 {
        return Err(WinVideoError::Config(format!(
            "补丁行距 {rect_stride} 小于一个像素"
        )));
    }
    let out_len = rect_width
        .checked_mul(rect_height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| WinVideoError::Config("补丁目标尺寸溢出".to_owned()))?;
    out.resize(out_len, 0);

    let origin_x = usize::try_from(src_origin.0).unwrap_or(0);
    let origin_y = usize::try_from(src_origin.1).unwrap_or(0);
    for local_y in 0..rect_height {
        let global_y = usize::try_from(dst_rect.y)
            .unwrap_or(0)
            .saturating_add(local_y);
        let sy0 = global_y.saturating_mul(src_height) / dst_height;
        let sy1 = ((global_y.saturating_add(1)).saturating_mul(src_height) / dst_height)
            .max(sy0.saturating_add(1))
            .min(src_height);
        for local_x in 0..rect_width {
            let global_x = usize::try_from(dst_rect.x)
                .unwrap_or(0)
                .saturating_add(local_x);
            let sx0 = global_x.saturating_mul(src_width) / dst_width;
            let sx1 = ((global_x.saturating_add(1)).saturating_mul(src_width) / dst_width)
                .max(sx0.saturating_add(1))
                .min(src_width);
            let mut sums = [0_u64; 4];
            let mut count = 0_u64;
            for sy in sy0..sy1 {
                let row_start = sy.saturating_sub(origin_y).saturating_mul(rect_stride);
                let Some(row) = rect_bgra.get(row_start..row_start.saturating_add(rect_stride))
                else {
                    return Err(WinVideoError::Config(format!("补丁源行 {sy} 超出回读缓冲")));
                };
                for sx in sx0..sx1 {
                    let offset = sx.saturating_sub(origin_x).saturating_mul(4);
                    let Some(pixel) = row.get(offset..offset.saturating_add(4)) else {
                        return Err(WinVideoError::Config(format!("补丁源列 {sx} 超出回读缓冲")));
                    };
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum = sum.saturating_add(u64::from(pixel[channel]));
                    }
                    count = count.saturating_add(1);
                }
            }
            if count == 0 {
                return Err(WinVideoError::Config(
                    "补丁盒式缩放没有覆盖任何源像素".to_owned(),
                ));
            }
            // 源是 B、G、R、A；输出是 R、G、B、A（与 `pixels` 模块一致）。
            let offset = local_y
                .saturating_mul(rect_width)
                .saturating_add(local_x)
                .saturating_mul(4);
            out[offset] = mean(sums[2], count);
            out[offset + 1] = mean(sums[1], count);
            out[offset + 2] = mean(sums[0], count);
            out[offset + 3] = mean(sums[3], count);
        }
    }
    Ok(())
}

/// 把目标坐标 `value` 按 `src / dst` 映射到源坐标（`ceil` 决定向上取整）。
fn scale_coord(value: u32, src: u32, dst: u32, ceil: bool) -> u32 {
    let numerator = u64::from(value).saturating_mul(u64::from(src));
    let denominator = u64::from(dst);
    let mapped = if ceil {
        numerator.div_ceil(denominator.max(1))
    } else {
        numerator / denominator.max(1)
    };
    u32::try_from(mapped).unwrap_or(u32::MAX)
}

/// `sum / count` 的均值（`count` 由调用方保证非零）。
fn mean(sum: u64, count: u64) -> u8 {
    u8::try_from(sum / count.max(1)).unwrap_or(0)
}

/// 每个脏格点向 3×3 邻域膨胀，把零散的单格点连成可编码的块。
fn dilate(cells: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0_u8; cells.len()];
    for y in 0..height {
        for x in 0..width {
            if cells[y.saturating_mul(width).saturating_add(x)] == 0 {
                continue;
            }
            let y_end = (y + 1).min(height - 1);
            let x_end = (x + 1).min(width - 1);
            for ny in y.saturating_sub(1)..=y_end {
                for nx in x.saturating_sub(1)..=x_end {
                    if let Some(cell) = out.get_mut(ny.saturating_mul(width).saturating_add(nx)) {
                        *cell = 1;
                    }
                }
            }
        }
    }
    out
}

/// 在膨胀后的网格上求 **8 连通分量** 的包围盒。
fn connected_boxes(cells: &[u8], width: usize, height: usize) -> Vec<CellBox> {
    let mut visited = vec![false; cells.len()];
    let mut boxes = Vec::new();
    let mut stack = Vec::new();
    for start in 0..cells.len() {
        if cells[start] == 0 || visited[start] {
            continue;
        }
        visited[start] = true;
        stack.push(start);
        let mut x0 = usize::MAX;
        let mut y0 = usize::MAX;
        let mut x1 = 0_usize;
        let mut y1 = 0_usize;
        while let Some(index) = stack.pop() {
            let x = index % width;
            let y = index / width;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            let y_end = (y + 1).min(height - 1);
            let x_end = (x + 1).min(width - 1);
            for ny in y.saturating_sub(1)..=y_end {
                for nx in x.saturating_sub(1)..=x_end {
                    let neighbour = ny.saturating_mul(width).saturating_add(nx);
                    if cells[neighbour] != 0 && !visited[neighbour] {
                        visited[neighbour] = true;
                        stack.push(neighbour);
                    }
                }
            }
        }
        if let (Ok(x0), Ok(y0), Ok(x1), Ok(y1)) = (
            u32::try_from(x0),
            u32::try_from(y0),
            u32::try_from(x1),
            u32::try_from(y1),
        ) {
            boxes.push(CellBox { x0, y0, x1, y1 });
        }
    }
    boxes
}

/// 反复合并间隔 ≤ [`MERGE_GAP`] 的格点包围盒，直到不再变化。
fn merge_boxes(boxes: &mut Vec<CellBox>) {
    let mut changed = true;
    while changed {
        changed = false;
        let mut left = 0;
        while left < boxes.len() {
            let mut right = left + 1;
            while right < boxes.len() {
                if boxes_should_merge(boxes[left], boxes[right]) {
                    boxes[left] = boxes[left].union(boxes[right]);
                    boxes.remove(right);
                    changed = true;
                } else {
                    right += 1;
                }
            }
            left += 1;
        }
    }
}

/// 两个格点包围盒在任一轴上的间隔是否都不超过 [`MERGE_GAP`]。
fn boxes_should_merge(a: CellBox, b: CellBox) -> bool {
    gap(a.x0, a.x1, b.x0, b.x1) <= MERGE_GAP && gap(a.y0, a.y1, b.y0, b.y1) <= MERGE_GAP
}

/// 两个闭区间之间隔了多少个格点（重叠/相邻为 0）。
fn gap(a0: u32, a1: u32, b0: u32, b1: u32) -> u32 {
    if a1 < b0 {
        b0.saturating_sub(a1).saturating_sub(1)
    } else if b1 < a0 {
        a0.saturating_sub(b1).saturating_sub(1)
    } else {
        0
    }
}

/// 把格点包围盒映射到目标坐标，外扩后向外对齐到偶数并钳在目标范围内。
fn map_box(
    cell_box: CellBox,
    grid_width: u32,
    grid_height: u32,
    dst: (u32, u32),
    expand: u32,
    limit_width: u32,
    limit_height: u32,
) -> Option<Rect> {
    if grid_width == 0 || grid_height == 0 {
        return None;
    }
    let tx0 = scale_coord(cell_box.x0, dst.0, grid_width, false);
    let tx1 = scale_coord(cell_box.x1.saturating_add(1), dst.0, grid_width, true);
    let ty0 = scale_coord(cell_box.y0, dst.1, grid_height, false);
    let ty1 = scale_coord(cell_box.y1.saturating_add(1), dst.1, grid_height, true);

    let x0 = tx0.saturating_sub(expand) & !1;
    let x1 = tx1
        .saturating_add(expand)
        .saturating_add(1)
        .min(limit_width)
        & !1;
    let y0 = ty0.saturating_sub(expand) & !1;
    let y1 = ty1
        .saturating_add(expand)
        .saturating_add(1)
        .min(limit_height)
        & !1;
    let width = x1.saturating_sub(x0);
    let height = y1.saturating_sub(y0);
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect {
        x: x0.min(limit_width),
        y: y0.min(limit_height),
        width,
        height,
    })
}

/// 反复合并重叠/相邻的目标矩形，直到不再变化。
fn merge_rects(rects: &mut Vec<Rect>) {
    let mut changed = true;
    while changed {
        changed = false;
        let mut left = 0;
        while left < rects.len() {
            let mut right = left + 1;
            while right < rects.len() {
                if rects[left].touches(rects[right]) {
                    rects[left] = rects[left].union(rects[right]);
                    rects.remove(right);
                    changed = true;
                } else {
                    right += 1;
                }
            }
            left += 1;
        }
    }
}

/// 可复用的 staging 图集（按设备 + `(宽, 高, 格式)` 缓存）。
#[derive(Default)]
struct PatchReader {
    /// CPU 可读的纵向堆叠 staging 纹理。
    staging: Option<ID3D11Texture2D>,
    /// 当前 staging 对应的 `(宽, 高, 格式)`。
    key: Option<(u32, u32, DXGI_FORMAT)>,
    /// staging 所属设备；设备变化时整只缓存重置。
    device: Option<Arc<crate::d3d11::D3d11>>,
}

impl PatchReader {
    /// 取（或重建）与采集纹理同设备、指定尺寸的 staging 纹理。
    fn staging(
        &mut self,
        captured: &CapturedTexture,
        format: DXGI_FORMAT,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D, WinVideoError> {
        let same_device = self
            .device
            .as_ref()
            .is_some_and(|device| Arc::ptr_eq(device, &captured.device));
        let key = (width, height, format);
        if !same_device || self.key != Some(key) || self.staging.is_none() {
            self.device = Some(Arc::clone(&captured.device));
            self.key = Some(key);
            self.staging = Some(create_staging(
                &captured.device.device,
                width,
                height,
                format,
            )?);
        }
        self.staging
            .clone()
            .ok_or_else(|| WinVideoError::Config("补丁 staging 纹理缺失".to_owned()))
    }
}

thread_local! {
    /// 逐线程复用的补丁图集：采集循环通常固定在同一个 worker 线程上，避免每帧重建纹理。
    static PATCH_READER: RefCell<PatchReader> = RefCell::new(PatchReader::default());
}

/// 创建一张指定的 staging 纹理（只读回，无绑定标志）。
fn create_staging(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D, WinVideoError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0),
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` 合法；输出指针指向栈上 Option。
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }
        .map_err(|err| WinVideoError::Capture(format!("补丁 staging 纹理创建失败: {err}")))?;
    texture.ok_or_else(|| WinVideoError::Config("CreateTexture2D 返回空纹理".to_owned()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::pixels::scale_bgra_to_rgba;

    /// 构造一块全零网格。
    fn empty_grid(width: u32, height: u32) -> DirtyGrid {
        DirtyGrid {
            width,
            height,
            cells: vec![0; usize::try_from(width * height).expect("尺寸应能放进 usize")],
        }
    }

    /// 把若干格点标脏。
    fn grid_with(width: u32, height: u32, cells: &[(u32, u32)]) -> DirtyGrid {
        let mut grid = empty_grid(width, height);
        for &(x, y) in cells {
            grid.cells[usize::try_from(y * width + x).expect("格点下标应能放进 usize")] = 1;
        }
        grid
    }

    /// 无外扩参数（便于手算坐标）。
    fn no_expand() -> PatchParams {
        PatchParams {
            expand_px: 0,
            ..PatchParams::default()
        }
    }

    #[test]
    fn default_params_match_documented_thresholds() {
        let params = PatchParams::default();
        assert_eq!(params.max_rects, 16);
        assert_eq!(params.max_area_permille, 200);
        assert_eq!(params.expand_px, 4);
    }

    #[test]
    fn empty_grid_yields_no_rects() {
        let grid = empty_grid(320, 180);
        let rects = extract_rects(&grid, (1920, 1080), (1920, 1080), &PatchParams::default());
        assert_eq!(rects, Some(Vec::new()), "没有变化应当返回空列表而非 None");
    }

    #[test]
    fn malformed_grid_is_rejected() {
        let grid = DirtyGrid {
            width: 4,
            height: 4,
            cells: vec![0; 3],
        };
        assert_eq!(
            extract_rects(&grid, (16, 16), (16, 16), &PatchParams::default()),
            None
        );
    }

    #[test]
    fn single_cell_maps_to_expanded_even_rect() {
        let grid = grid_with(320, 180, &[(50, 100)]);
        let rects = extract_rects(&grid, (320, 180), (320, 180), &PatchParams::default())
            .expect("单格点应当有效");
        assert_eq!(
            rects,
            vec![Rect {
                x: 44,
                y: 94,
                width: 12,
                height: 12
            }],
            "3×3 膨胀 + 4 像素外扩 + 偶数对齐后的坐标"
        );
    }

    #[test]
    fn two_far_apart_cells_yield_two_rects() {
        let grid = grid_with(320, 180, &[(10, 10), (100, 100)]);
        let rects = extract_rects(&grid, (320, 180), (320, 180), &no_expand())
            .expect("两个远离的格点应当有效");
        assert_eq!(rects.len(), 2, "相距很远的块不能被并成一个");
    }

    #[test]
    fn adjacent_cells_merge_into_one_rect() {
        let grid = grid_with(320, 180, &[(10, 10), (11, 10)]);
        let rects =
            extract_rects(&grid, (320, 180), (320, 180), &no_expand()).expect("相邻格点应当有效");
        assert_eq!(rects.len(), 1, "相邻（3×3 膨胀后重叠）的格点必须并成一块");
        // 膨胀覆盖 x 9..=12、y 9..=11；目标映射后 x 9..13、y 9..12，偶数向外对齐后
        // 得到 [8, 14) × [8, 12)。
        assert_eq!(
            rects[0],
            Rect {
                x: 8,
                y: 8,
                width: 6,
                height: 4
            }
        );
    }

    #[test]
    fn too_many_rects_is_rejected() {
        let grid = grid_with(320, 180, &[(10, 10), (100, 100)]);
        let params = PatchParams {
            max_rects: 1,
            expand_px: 0,
            ..PatchParams::default()
        };
        assert_eq!(
            extract_rects(&grid, (320, 180), (320, 180), &params),
            None,
            "矩形数超过上限必须整帧放弃补丁"
        );
    }

    #[test]
    fn area_over_budget_is_rejected() {
        let mut grid = empty_grid(320, 180);
        grid.cells.fill(1);
        let params = PatchParams::default();
        assert_eq!(
            extract_rects(&grid, (1920, 1080), (1920, 1080), &params),
            None,
            "满屏变化（100% 面积）远超 20% 预算，必须退回全帧"
        );
    }

    #[test]
    fn rects_are_even_and_clamped_to_target() {
        // 右下角最后一个格点：映射必须钳在目标内且保持偶数。
        let grid = grid_with(320, 180, &[(319, 179)]);
        let rects =
            extract_rects(&grid, (320, 180), (320, 180), &no_expand()).expect("角落格点应当有效");
        assert_eq!(rects.len(), 1);
        let rect = rects[0];
        assert_eq!((rect.x, rect.y), (318, 178));
        assert_eq!((rect.width, rect.height), (2, 2));
        assert_eq!(rect.right(), 320);
        assert_eq!(rect.bottom(), 180);
        assert!(rect.width.is_multiple_of(2) && rect.height.is_multiple_of(2));
    }

    #[test]
    fn target_scaling_maps_cells_proportionally() {
        // 网格 320×180 → 目标 640×360：一个格点约等于目标上的 2×2 像素。
        let grid = grid_with(320, 180, &[(160, 90)]);
        let rects =
            extract_rects(&grid, (640, 360), (640, 360), &no_expand()).expect("单格点应当有效");
        assert_eq!(rects.len(), 1);
        let rect = rects[0];
        assert_eq!((rect.x, rect.y), (318, 178));
        assert_eq!((rect.width, rect.height), (6, 6));
    }

    /// 4×4 源 → 2×2 目标：矩形路径必须与整帧路径逐字节一致（2×2 盒式均值）。
    #[test]
    fn rect_scale_matches_full_frame_box_filter() {
        let source: Vec<u8> = (0..16)
            .flat_map(|index| {
                let value = u8::try_from(index).expect("下标在 0..16");
                [value, value.wrapping_mul(3), value.wrapping_add(7), 255]
            })
            .collect();
        let expected = scale_bgra_to_rgba(&source, 4, 4, 2, 2).expect("整帧缩放应成功");

        let mut rgba = Vec::new();
        scale_bgra_rect_to_rgba(
            &source,
            4 * 4,
            (0, 0),
            (4, 4),
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            (2, 2),
            &mut rgba,
        )
        .expect("补丁缩放应成功");
        assert_eq!(rgba, expected, "补丁路径与整帧路径必须逐字节一致");
    }

    /// 子矩形（非零源原点）与整帧路径在同一绝对坐标上逐像素一致——这证明局部缩放
    /// 不会在矩形边缘引入接缝。
    #[test]
    fn sub_rect_matches_full_frame_at_absolute_coordinates() {
        let (src_width, src_height) = (8_u32, 8_u32);
        let source: Vec<u8> = (0..src_width * src_height)
            .flat_map(|index| {
                let value = u8::try_from(index % 251).expect("取模后落在 u8");
                [value, value.wrapping_mul(3), value.wrapping_add(11), 255]
            })
            .collect();
        let full =
            scale_bgra_to_rgba(&source, src_width, src_height, 4, 4).expect("整帧缩放应成功");

        // 目标右半部分（x = 2..4），源上对应 x = 4..8。按图集语义把源裁剪成
        // 「从 x = 4 开始、行距 16」的一块，源原点因此是非零的 (4, 0)。
        let crop_stride = 4 * 4;
        let mut cropped = Vec::with_capacity(
            usize::try_from(src_height).expect("高应能放进 usize") * crop_stride,
        );
        for row in 0..src_height {
            let start = usize::try_from((row * src_width + 4) * 4).expect("偏移应能放进 usize");
            cropped.extend_from_slice(&source[start..start + crop_stride]);
        }
        let rect = Rect {
            x: 2,
            y: 0,
            width: 2,
            height: 4,
        };
        let mut rgba = Vec::new();
        scale_bgra_rect_to_rgba(
            &cropped,
            crop_stride,
            (4, 0),
            (src_width, src_height),
            rect,
            (4, 4),
            &mut rgba,
        )
        .expect("子矩形缩放应成功");

        for row in 0..4_usize {
            let full_start = (row * 4 + 2) * 4;
            let rect_start = row * 2 * 4;
            assert_eq!(
                &rgba[rect_start..rect_start + 2 * 4],
                &full[full_start..full_start + 2 * 4],
                "第 {row} 行的子矩形像素必须与整帧结果相同"
            );
        }
    }

    /// 矩形尺寸的 NV12 长度与平面划分必须符合 4:2:0。
    #[test]
    fn rect_nv12_has_expected_plane_layout() {
        let (width, height) = (6_u32, 4_u32);
        let rgba =
            vec![128_u8; usize::try_from(width * height * 4).expect("尺寸应能放进 usize")];
        let mut nv12 = Vec::new();
        rgba_to_nv12(&rgba, width, height, &mut nv12).expect("NV12 转换应成功");
        let luma = usize::try_from(width * height).expect("Y 平面应能放进 usize");
        assert_eq!(nv12.len(), luma * 3 / 2);
        assert_eq!(nv12.len() - luma, luma / 2, "UV 平面必须是半高度");
    }
}
