//! CPU **变化检测 + 脏矩形提取**（纯逻辑）。
//!
//! # 为什么需要它
//!
//! ScreenCaptureKit 之类的采集接口在桌面静止时仍按合成节奏产帧；「有新帧」不等于
//! 「画面变了」。远程桌面最常见的场景正是「有新帧、像素一模一样」——照旧编码只会
//! 白白吃带宽。与 `crates/win-video/src/capture/change.rs` 同思路，但本模块走 **CPU**
//! （macOS 采集侧不保证有可用的 GPU 视频处理器），因此按块盒式降采样到 320×180 的
//! **亮度网格**后逐格比较。
//!
//! # 判据
//!
//! 任一格点的亮度 |Δ| 超过 [`PIXEL_DELTA`] 即判「有变化」。不做聚合阈值：终端光标
//! 闪烁只影响 1 个格点，但它是真实变化，漏掉会让控制器看起来像卡死。静止画面在相同
//! 输入下是**精确的 0 差异**（盒式均值是确定性的）。
//!
//! # 脏矩形
//!
//! 有变化的格点经「3×3 膨胀 → 8 连通分量 → 包围盒合并 → 映射到目标坐标 → 外扩 →
//! 偶数对齐 → 再合并」得到若干目标坐标矩形，供补丁模式只编码变化区域。矩形总数上限
//! 16、总面积不超过目标的 20%（[`RectParams`]）：超过说明画面已经不「局部」，退回
//! 全帧编码比发一堆碎片更划算。

use crate::color::bgra_luma;

/// 变化网格宽度（格点数）。偶数，与 Windows 侧 320×180 对齐。
pub const GRID_WIDTH: u32 = 320;

/// 变化网格高度（格点数）。16:9，与 [`GRID_WIDTH`] 配套。
pub const GRID_HEIGHT: u32 = 180;

/// 判定「该格有变化」的亮度差阈值（8 位）。
///
/// 取 3 而不是 1：同一画面重复降采样的结果是精确相等的，这点余量只留给不同采集后端
/// 在缩放/色彩转换上可能引入的 ±1–2。
const PIXEL_DELTA: u8 = 3;

/// 相邻脏格点合并的最大间隔（格点数）。
const MERGE_GAP: u32 = 2;

/// 一次变化检测的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeReport {
    /// 画面是否有可见变化。
    pub changed: bool,
    /// 变化面积（千分比，`0..=1000`）。`changed == false` 时恒为 0。
    pub changed_permille: u16,
}

impl ChangeReport {
    /// 无变化。
    #[must_use]
    pub const fn unchanged() -> Self {
        Self {
            changed: false,
            changed_permille: 0,
        }
    }

    /// 有变化且面积未知（按满帧处理，保守地不压制码率/帧率）。
    #[must_use]
    pub const fn changed_all() -> Self {
        Self {
            changed: true,
            changed_permille: 1000,
        }
    }
}

/// 一帧的脏格点网格：每格 1 字节，`0` = 未变、`1` = 有变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyGrid {
    /// 网格宽度（格点数）。
    pub width: u32,
    /// 网格高度（格点数）。
    pub height: u32,
    /// 行优先的格点值，长度 = `width * height`。
    pub cells: Vec<u8>,
}

impl DirtyGrid {
    /// 一张全 0 的网格（「本拍没有变化」），尺寸固定为 [`GRID_WIDTH`]×[`GRID_HEIGHT`]。
    ///
    /// 供给「采集层已从平台拿到『零脏矩形』的权威信号、不必再跑像素比较」的调用方：
    /// 空网格喂给 [`extract_rects`] 会合法地返回空矩形列表（保活），与「真的比较过、
    /// 没有任何格点变化」完全等价。
    #[must_use]
    pub fn empty() -> Self {
        Self {
            width: GRID_WIDTH,
            height: GRID_HEIGHT,
            cells: vec![0; grid_len()],
        }
    }
}

/// 一次观测的结果：报告 + 可选的脏网格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeOutcome {
    /// 变化报告。
    pub report: ChangeReport,
    /// 脏格点网格；`None` 表示本帧没有可用网格（首帧、输入非法），调用方**不能**
    /// 据此进入补丁模式。
    pub grid: Option<DirtyGrid>,
}

/// CPU 变化检测器（每个采集源一个，跨帧保存上一帧网格）。
#[derive(Debug, Clone)]
pub struct ChangeDetector {
    /// 上一帧的亮度网格（行优先），空表示还没有基准。
    previous: Vec<u8>,
    /// 复用的脏网格缓冲。
    grid: DirtyGrid,
}

impl Default for ChangeDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeDetector {
    /// 新建一个尚未建立基准的检测器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            previous: Vec::new(),
            grid: DirtyGrid {
                width: GRID_WIDTH,
                height: GRID_HEIGHT,
                cells: vec![0; grid_len()],
            },
        }
    }

    /// 丢弃基准（采集源重建/尺寸变化后调用，下一帧按首帧处理）。
    pub fn reset(&mut self) {
        self.previous.clear();
    }

    /// 是否已经建立了比较基准（至少成功观测过一帧）。
    ///
    /// 采集层在拿到平台级「零脏矩形」信号时可以据此跳过像素比较：基准存在 ⇒ 上一次
    /// 失败观测过的画面与当前帧逐像素相同，直接报「未变化」是安全的；基准不存在 ⇒
    /// 仍要观测一帧（首帧必须按满面积处理）。
    #[must_use]
    pub fn has_baseline(&self) -> bool {
        !self.previous.is_empty()
    }

    /// 观测一帧 BGRA，返回变化报告与脏网格。
    ///
    /// 首帧、输入长度不符或尺寸为零时返回 [`ChangeReport::changed_all`] 且网格为
    /// `None`——宁可多编码一帧，也不能丢画面，更不能把「面积未知」当成「没变化」
    /// 去压制码率。
    #[must_use]
    pub fn observe(&mut self, bgra: &[u8], width: u32, height: u32) -> ChangeOutcome {
        let Some(current) = downsample_luma(bgra, width, height) else {
            self.previous.clear();
            return ChangeOutcome {
                report: ChangeReport::changed_all(),
                grid: None,
            };
        };
        if self.previous.len() != current.len() {
            // 首帧（或长度变化）：没有基准，按满面积处理，不提供网格。
            self.previous = current;
            return ChangeOutcome {
                report: ChangeReport::changed_all(),
                grid: None,
            };
        }
        self.grid.cells.fill(0);
        let mut differing = 0usize;
        for (index, (before, now)) in self.previous.iter().zip(current.iter()).enumerate() {
            if before.abs_diff(*now) > PIXEL_DELTA {
                differing = differing.saturating_add(1);
                if let Some(cell) = self.grid.cells.get_mut(index) {
                    *cell = 1;
                }
            }
        }
        let report = if differing == 0 {
            ChangeReport::unchanged()
        } else {
            let total = current.len().max(1);
            // 千分比；有变化就至少记 1（绝不报「变了但 0‰」）。
            let permille = differing
                .saturating_mul(1000)
                .checked_div(total)
                .unwrap_or(1000)
                .clamp(1, 1000);
            ChangeReport {
                changed: true,
                changed_permille: u16::try_from(permille).unwrap_or(1000),
            }
        };
        let grid = self.grid.clone();
        self.previous = current;
        ChangeOutcome {
            report,
            grid: Some(grid),
        }
    }
}

/// 网格格点总数（320×180）。
fn grid_len() -> usize {
    usize::try_from(GRID_WIDTH)
        .unwrap_or(0)
        .saturating_mul(usize::try_from(GRID_HEIGHT).unwrap_or(0))
}

/// 把 BGRA 画面按块盒式降采样成 [`GRID_WIDTH`]×[`GRID_HEIGHT`] 的亮度网格。
///
/// 输入不合法（尺寸为零、长度不符）时返回 `None`。
fn downsample_luma(bgra: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let (sw, sh) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    if sw == 0 || sh == 0 {
        return None;
    }
    let expected = sw.checked_mul(sh)?.checked_mul(4)?;
    if bgra.len() != expected {
        return None;
    }
    let gw = usize::try_from(GRID_WIDTH).ok()?;
    let gh = usize::try_from(GRID_HEIGHT).ok()?;
    let mut out = vec![0u8; gw.checked_mul(gh)?];
    for gy in 0..gh {
        let sy0 = gy.saturating_mul(sh) / gh;
        let sy1 = ((gy.saturating_add(1)).saturating_mul(sh) / gh)
            .max(sy0.saturating_add(1))
            .min(sh);
        for gx in 0..gw {
            let sx0 = gx.saturating_mul(sw) / gw;
            let sx1 = ((gx.saturating_add(1)).saturating_mul(sw) / gw)
                .max(sx0.saturating_add(1))
                .min(sw);
            let mut sum = 0u64;
            let mut count = 0u64;
            for sy in sy0..sy1 {
                let row_start = sy.saturating_mul(sw).saturating_mul(4);
                for sx in sx0..sx1 {
                    let offset = row_start.saturating_add(sx.saturating_mul(4));
                    let (Some(&b), Some(&g), Some(&r)) = (
                        bgra.get(offset),
                        bgra.get(offset.saturating_add(1)),
                        bgra.get(offset.saturating_add(2)),
                    ) else {
                        return None;
                    };
                    sum = sum.saturating_add(u64::from(bgra_luma(b, g, r)));
                    count = count.saturating_add(1);
                }
            }
            out[gy.saturating_mul(gw).saturating_add(gx)] =
                u8::try_from(sum / count.max(1)).unwrap_or(0);
        }
    }
    Some(out)
}

/// 目标（编码尺寸）坐标系里的一个矩形，全部为**偶数**且非零。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// 左上角 X（偶数）。
    pub x: u32,
    /// 左上角 Y（偶数）。
    pub y: u32,
    /// 宽度（偶数、非零）。
    pub width: u32,
    /// 高度（偶数、非零）。
    pub height: u32,
}

impl Rect {
    /// 右边界（不含）。
    #[must_use]
    fn right(self) -> u32 {
        self.x.saturating_add(self.width)
    }

    /// 下边界（不含）。
    #[must_use]
    fn bottom(self) -> u32 {
        self.y.saturating_add(self.height)
    }

    /// 两个矩形的包围盒。
    #[must_use]
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
    #[must_use]
    fn touches(self, other: Self) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }
}

/// 补丁提取参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RectParams {
    /// 允许的最大矩形数。超过即放弃补丁（返回 `None`）。
    pub max_rects: usize,
    /// 允许的最大总覆盖面积（千分比，`0..=1000`）。
    pub max_area_permille: u16,
    /// 每个矩形向四周外扩的目标像素数（再按偶数对齐）。
    pub expand_px: u32,
}

impl Default for RectParams {
    fn default() -> Self {
        Self {
            max_rects: 16,
            max_area_permille: 200,
            expand_px: 4,
        }
    }
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
    #[must_use]
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
/// 返回 `None` 表示这一帧不适合补丁：网格非法、目标尺寸太小（放不下偶数 NV12 矩形）、
/// 矩形数超过 [`RectParams::max_rects`]、或覆盖面积超过
/// [`RectParams::max_area_permille`]。空网格（没有变化）返回 `Some(vec![])`——那是
/// 「本拍只有保活、没有补丁」的合法结果，不是失败。
#[must_use]
pub fn extract_rects(grid: &DirtyGrid, dst: (u32, u32), params: &RectParams) -> Option<Vec<Rect>> {
    let grid_width = usize::try_from(grid.width).ok()?;
    let grid_height = usize::try_from(grid.height).ok()?;
    let dst_width = usize::try_from(dst.0).ok()?;
    let dst_height = usize::try_from(dst.1).ok()?;
    // 宽/高 < 2 连一个合法的 NV12 块都放不下（UV 平面是半分辨率）。
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

/// 每个脏格点向 3×3 邻域膨胀，把零散单格点连成可编码的块。
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

/// 把目标坐标 `value` 按 `src / dst` 映射（`ceil` 决定向上取整）。
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 造一张纯色 BGRA。
    fn solid(width: u32, height: u32, value: u8) -> Vec<u8> {
        vec![value; (width * height) as usize * 4]
    }

    /// 首帧：满面积、无网格。
    #[test]
    fn first_frame_is_changed_all() {
        let mut detector = ChangeDetector::new();
        let outcome = detector.observe(&solid(64, 48, 0), 64, 48);
        assert_eq!(outcome.report, ChangeReport::changed_all());
        assert!(outcome.grid.is_none());
    }

    /// `has_baseline` 只在成功观测后为真；`reset` 之后回到「无基准」。
    #[test]
    fn has_baseline_tracks_detector_state() {
        let mut detector = ChangeDetector::new();
        assert!(!detector.has_baseline());
        let _ = detector.observe(&solid(32, 32, 7), 32, 32);
        assert!(detector.has_baseline());
        detector.reset();
        assert!(!detector.has_baseline());
        // 非法输入同样把基准清掉。
        let _ = detector.observe(&[0; 4], 32, 32);
        assert!(!detector.has_baseline());
    }

    /// 空网格是「无变化」的合法输入：`extract_rects` 返回空列表，尺寸为 320×180。
    #[test]
    fn empty_grid_is_well_formed() {
        let grid = DirtyGrid::empty();
        assert_eq!((grid.width, grid.height), (GRID_WIDTH, GRID_HEIGHT));
        assert_eq!(grid.cells.len(), (GRID_WIDTH * GRID_HEIGHT) as usize);
        assert!(grid.cells.iter().all(|&cell| cell == 0));
        let rects =
            extract_rects(&grid, (1920, 1080), &RectParams::default()).expect("空网格不是失败");
        assert!(rects.is_empty());
    }

    /// 完全相同两帧：无变化、面积 0、网格全 0。
    #[test]
    fn identical_frames_are_unchanged() {
        let mut detector = ChangeDetector::new();
        let frame = solid(64, 48, 100);
        let _ = detector.observe(&frame, 64, 48);
        let outcome = detector.observe(&frame, 64, 48);
        assert_eq!(outcome.report, ChangeReport::unchanged());
        let grid = outcome.grid.expect("第二帧应有网格");
        assert!(grid.cells.iter().all(|&cell| cell == 0));
    }

    /// 单点变化：报告有变化，面积很小，网格里只有少量格点被置位。
    #[test]
    fn single_point_change() {
        let mut detector = ChangeDetector::new();
        let _ = detector.observe(&solid(320, 180, 0), 320, 180);
        let mut frame = solid(320, 180, 0);
        // 把中心 4×4 像素刷白（网格上约对应 4 个格点）。
        for y in 88usize..92 {
            for x in 158usize..162 {
                let offset = (y * 320 + x) * 4;
                frame[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        let outcome = detector.observe(&frame, 320, 180);
        assert!(outcome.report.changed);
        assert!(
            outcome.report.changed_permille <= 5,
            "{}",
            outcome.report.changed_permille
        );
        let grid = outcome.grid.expect("应有网格");
        let dirty = grid.cells.iter().filter(|&&cell| cell != 0).count();
        assert!((1..32).contains(&dirty), "脏格点 {dirty}");
    }

    /// 大面积变化（左半屏刷白）：变化面积接近一半。
    #[test]
    fn large_area_change() {
        let mut detector = ChangeDetector::new();
        let _ = detector.observe(&solid(320, 180, 0), 320, 180);
        let mut frame = solid(320, 180, 0);
        for y in 0..180usize {
            for x in 0..160usize {
                let offset = (y * 320 + x) * 4;
                frame[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        let outcome = detector.observe(&frame, 320, 180);
        assert!(outcome.report.changed);
        assert!(
            outcome.report.changed_permille > 300,
            "{}",
            outcome.report.changed_permille
        );
    }

    /// 输入非法：满面积、无网格（不能把「读不懂」当成「静止」）。
    #[test]
    fn invalid_input_is_changed_all() {
        let mut detector = ChangeDetector::new();
        let outcome = detector.observe(&[0; 4], 64, 48);
        assert_eq!(outcome.report, ChangeReport::changed_all());
        assert!(outcome.grid.is_none());
    }

    /// 单块小变化的矩形：坐标偶数、落在目标内、面积远小于 20%。
    #[test]
    fn extract_rects_single_region() {
        let mut cells = vec![0u8; (GRID_WIDTH * GRID_HEIGHT) as usize];
        cells[(90 * GRID_WIDTH + 160) as usize] = 1;
        let grid = DirtyGrid {
            width: GRID_WIDTH,
            height: GRID_HEIGHT,
            cells,
        };
        let rects = extract_rects(&grid, (1920, 1080), &RectParams::default())
            .expect("单块变化应能转成矩形");
        assert_eq!(rects.len(), 1);
        let rect = rects[0];
        assert_eq!(rect.x % 2, 0);
        assert_eq!(rect.y % 2, 0);
        assert_eq!(rect.width % 2, 0);
        assert_eq!(rect.height % 2, 0);
        assert!(rect.x + rect.width <= 1920);
        assert!(rect.y + rect.height <= 1080);
        let area = u64::from(rect.width) * u64::from(rect.height);
        assert!(area * 1000 <= 1920 * 1080 * 200);
    }

    /// 空网格（无变化）：合法地返回空矩形列表。
    #[test]
    fn extract_rects_empty_grid() {
        let grid = DirtyGrid {
            width: GRID_WIDTH,
            height: GRID_HEIGHT,
            cells: vec![0; (GRID_WIDTH * GRID_HEIGHT) as usize],
        };
        let rects =
            extract_rects(&grid, (1920, 1080), &RectParams::default()).expect("空网格不是失败");
        assert!(rects.is_empty());
    }

    /// 矩形数量上限：散布的孤立变化超过 16 块时判否。
    #[test]
    fn extract_rects_rejects_too_many_regions() {
        let mut cells = vec![0u8; (GRID_WIDTH * GRID_HEIGHT) as usize];
        // 32 个彼此相距 ≥8 格的点（膨胀后仍是独立分量）。
        let mut placed = 0;
        for y in (10..170).step_by(20) {
            for x in (10..310).step_by(20) {
                if placed >= 32 {
                    break;
                }
                cells[(y * GRID_WIDTH + x) as usize] = 1;
                placed += 1;
            }
        }
        let grid = DirtyGrid {
            width: GRID_WIDTH,
            height: GRID_HEIGHT,
            cells,
        };
        assert!(
            extract_rects(&grid, (1920, 1080), &RectParams::default()).is_none(),
            "超过 16 块应放弃补丁"
        );
    }

    /// 面积预算：整屏都脏时面积超过 20%，判否。
    #[test]
    fn extract_rects_rejects_large_area() {
        let grid = DirtyGrid {
            width: GRID_WIDTH,
            height: GRID_HEIGHT,
            cells: vec![1; (GRID_WIDTH * GRID_HEIGHT) as usize],
        };
        assert!(extract_rects(&grid, (1920, 1080), &RectParams::default()).is_none());
    }

    /// 非法网格（长度与宽高不符）判否，不 panic。
    #[test]
    fn extract_rects_rejects_malformed_grid() {
        let grid = DirtyGrid {
            width: 10,
            height: 10,
            cells: vec![1; 5],
        };
        assert!(extract_rects(&grid, (1920, 1080), &RectParams::default()).is_none());
    }
}
