//! 瓦片差分：把整帧切成 [`TILE_SIZE`] 见方的瓦片，只编码变化的那些。
//!
//! # 为什么需要它
//!
//! 整帧 JPEG 在 1080p@10fps 约 15–25 Mbps，经 hbbr 中继不可接受。桌面画面绝大多数时间只有
//! 局部变化（光标、时钟、一行文字），瓦片差分让静止/微动场景的流量降到整帧的百分之几。
//!
//! # 关键帧
//!
//! 差分有累积误差风险（丢帧、乱序），且忽略 [`crate::screen::FrameSink::on_tiles`] 的实现
//! 需要整帧才能恢复，因此被控端必须周期性发整帧：
//!
//! - 会话首帧；
//! - 距上次关键帧超过 [`KEYFRAME_INTERVAL_FRAMES`] 帧；
//! - 变化瓦片占比超过 [`KEYFRAME_CHANGE_RATIO`]（此时一个整帧 JPEG 比几百个小 JPEG 更省）；
//! - 画面尺寸变化、切换显示器、切换画质档位。
//!
//! # 瓦片恒为原始分辨率（与整帧缩放的不对称）
//!
//! 整帧（关键帧）会按 [`EncodeOptions::scale`] 缩放，而瓦片补丁**永不缩放**：它们必须与
//! 控制器已持有的整帧像素逐格对应，缩放会让瓦片坐标与整帧错位。为杜绝「缩放后的关键帧 +
//! 原始分辨率瓦片」这种不自洽的组合，[`TileDiffer::diff`] 在 `scale != 1.0` 时**一律发关键帧**
//! （即「流畅」档只走整帧路径）。会话循环切档位时本就强制关键帧，二者叠加保证不变量成立。
//!
//! # 网格
//!
//! 瓦片从左上角按行优先铺开，列数 = `ceil(宽 / [`TILE_SIZE`])`、行数 = `ceil(高 / 64)`；
//! 边缘瓦片收窄到帧边界内（例如 100x70 → 64x64、36x64、64x6、36x6 四块，正好无缝铺满）。

use crate::error::{RemoteError, RemoteResult};
use crate::screen::{
    encode_frame, BYTES_PER_PIXEL, DecodedTile, EncodeOptions, EncodedFrame, RawFrame, TilePatch, TileUpdate, TILE_SIZE,
};
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder};
use sha2::{Digest, Sha256};

/// 关键帧间隔（帧数）。按 [`crate::screen::QualityPreset::Balanced`] 的 12 fps 约合 10 秒。
pub const KEYFRAME_INTERVAL_FRAMES: u64 = 120;

/// 变化瓦片占比超过此值时改发整帧（避免几百个小 JPEG 的编码与头部开销）。
pub const KEYFRAME_CHANGE_RATIO: f32 = 0.6;

/// 一次差分编码的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct TileEncodeResult {
    /// 是否为关键帧（整帧）。
    pub keyframe: bool,
    /// 整帧编码结果；`keyframe == true` 时为 `Some`。
    pub full: Option<EncodedFrame>,
    /// 变化瓦片；`keyframe == true` 时为空。
    pub tiles: Vec<TilePatch>,
    /// 变化瓦片占比（`0.0..=1.0`），供自适应帧率决策。
    pub changed_ratio: f32,
}

/// 瓦片差分器：持有上一帧的每瓦片哈希。
///
/// 由被控端会话循环独占持有（`&mut self`），无需内部加锁。
#[derive(Debug, Default)]
pub struct TileDiffer {
    /// 上一帧的瓦片哈希与网格尺寸；`None` 表示尚无基准帧（下一次必为关键帧）。
    state: Option<TileState>,
    /// 距上次关键帧的帧数。
    frames_since_keyframe: u64,
}

/// 上一帧的瓦片基准。
#[derive(Debug)]
struct TileState {
    /// 原始帧宽高（尺寸变化即失效）。
    width: u32,
    /// 原始帧高。
    height: u32,
    /// 每瓦片哈希，按行优先排列。
    hashes: Vec<[u8; 32]>,
}

/// 一块瓦片在整帧中的矩形：`(x, y, 宽, 高)`（整帧像素坐标）。
type TileRect = (u32, u32, u32, u32);

/// 计算第 `col` 列、第 `row` 行瓦片的矩形（边缘瓦片自动收窄到帧边界内）。
///
/// `col`/`row` 由网格列数/行数（`ceil(尺寸 / TILE_SIZE)`）产生，故 `x < width`、`y < height`
/// 恒成立：减法不下溢，结果宽高至少 1 像素。
fn tile_rect(col: u32, row: u32, width: u32, height: u32) -> TileRect {
    let x = col * TILE_SIZE;
    let y = row * TILE_SIZE;
    (x, y, TILE_SIZE.min(width - x), TILE_SIZE.min(height - y))
}

/// 就地计算一块瓦片的 SHA-256。
///
/// 瓦片在整帧缓冲里**不连续**（行间有跨行的步长），故逐行把该瓦片的行切片喂进同一个 hasher，
/// 不额外分配整块拷贝。
fn tile_hash(rgba: &[u8], stride_bytes: usize, x: u32, y: u32, w: u32, h: u32) -> [u8; 32] {
    let mut hasher = Sha256::new();
    let x_offset = x as usize * BYTES_PER_PIXEL;
    let row_bytes = w as usize * BYTES_PER_PIXEL;
    for row in 0..h as usize {
        let start = (y as usize + row) * stride_bytes + x_offset;
        hasher.update(&rgba[start..start + row_bytes]);
    }
    hasher.finalize().into()
}

/// 把一块瓦片的像素拷成连续的 RGB8 缓冲（丢弃 alpha——JPEG 编码器不支持 RGBA8）。
fn crop_tile_rgb(rgba: &[u8], stride_bytes: usize, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(w as usize * h as usize * 3);
    let x_offset = x as usize * BYTES_PER_PIXEL;
    let row_bytes = w as usize * BYTES_PER_PIXEL;
    for row in 0..h as usize {
        let start = (y as usize + row) * stride_bytes + x_offset;
        for px in rgba[start..start + row_bytes].as_chunks::<BYTES_PER_PIXEL>().0 {
            out.extend_from_slice(&px[..3]);
        }
    }
    out
}

/// 变化瓦片占比。计数远小于 f32 可精确表示的 2^24 上限，精度损失不可能发生。
#[expect(
    clippy::cast_precision_loss,
    reason = "瓦片计数远小于 f32 可精确表示的 2^24 上限，占比计算不会丢精度"
)]
fn changed_ratio(changed: usize, total: usize) -> f32 {
    changed as f32 / total as f32
}

impl TileDiffer {
    /// 构造空差分器。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 重置基准（切换显示器、切换画质档位后调用），下一次必定是关键帧。
    pub fn reset(&mut self) {
        self.state = None;
        self.frames_since_keyframe = KEYFRAME_INTERVAL_FRAMES;
    }

    /// 计算变化瓦片并编码。
    ///
    /// `force_keyframe` 为真时无条件发整帧（首帧、切档位、切显示器）。
    /// 返回 `Ok(None)` 表示画面完全没变（静止桌面 → 零输出，只留心跳）。
    ///
    /// # Errors
    /// 帧尺寸与像素数据不匹配、尺寸为零、JPEG 编码失败时返回 [`crate::error::RemoteError::Encode`]。
    pub fn diff(
        &mut self,
        frame: &RawFrame,
        options: &EncodeOptions,
        force_keyframe: bool,
    ) -> RemoteResult<Option<TileEncodeResult>> {
        frame.validate()?;
        // 零尺寸会让网格为 0x0、占比除零（NaN）；真实屏幕不可能如此，直接拒绝。
        if frame.width == 0 || frame.height == 0 {
            return Err(RemoteError::Encode(format!(
                "帧尺寸不能为零（{}x{}）",
                frame.width, frame.height
            )));
        }

        let columns = frame.width.div_ceil(TILE_SIZE);
        let rows = frame.height.div_ceil(TILE_SIZE);
        let total_tiles = columns as usize * rows as usize; // 宽高均 >= 1，故 >= 1
        let stride = frame.width as usize * BYTES_PER_PIXEL;

        // 1) 当前帧每瓦片哈希（行优先）。
        let mut hashes = Vec::with_capacity(total_tiles);
        for row in 0..rows {
            for col in 0..columns {
                let (x, y, w, h) = tile_rect(col, row, frame.width, frame.height);
                hashes.push(tile_hash(&frame.rgba, stride, x, y, w, h));
            }
        }

        // 2) 与基准帧比对。尺寸或网格规模变化时基准失效（视为全变 → 必然关键帧）。
        let state_valid = self.state.as_ref().is_some_and(|state| {
            state.width == frame.width && state.height == frame.height && state.hashes.len() == total_tiles
        });
        let mut changed: Vec<TileRect> = Vec::new();
        if let Some(prev) = self.state.as_ref().filter(|_| state_valid) {
            for row in 0..rows {
                for col in 0..columns {
                    let idx = row as usize * columns as usize + col as usize;
                    if prev.hashes[idx] != hashes[idx] {
                        changed.push(tile_rect(col, row, frame.width, frame.height));
                    }
                }
            }
        }
        let changed_count = if state_valid { changed.len() } else { total_tiles };
        let ratio = changed_ratio(changed_count, total_tiles);

        // 3) 关键帧判定。`scale != 1.0` 一律关键帧（见模块文档：瓦片恒为原始分辨率）。
        let scale_forces_keyframe = (options.scale - 1.0).abs() >= f32::EPSILON;
        let keyframe = force_keyframe
            || !state_valid
            || scale_forces_keyframe
            || self.frames_since_keyframe >= KEYFRAME_INTERVAL_FRAMES
            || ratio > KEYFRAME_CHANGE_RATIO;

        // 4) 关键帧：无条件整帧。prev_hash 传 None 以绕开整帧去重——关键帧必须无条件下发，
        //    否则「控制器漏帧后请求恢复」会因像素恰好相同而拿不到整帧。
        if keyframe {
            let full = encode_frame(None, frame, options)?;
            self.state = Some(TileState {
                width: frame.width,
                height: frame.height,
                hashes,
            });
            self.frames_since_keyframe = 0;
            // 传入 None 时 encode_frame 必返回 Some；此处仍兜底处理 None，不引入 panic 路径。
            return Ok(full.map(|full| TileEncodeResult {
                keyframe: true,
                full: Some(full),
                tiles: Vec::new(),
                changed_ratio: ratio,
            }));
        }

        // 5) 非关键帧且无变化：零输出（静止桌面），只推进关键帧计时。
        if changed.is_empty() {
            self.state = Some(TileState {
                width: frame.width,
                height: frame.height,
                hashes,
            });
            self.frames_since_keyframe = self.frames_since_keyframe.saturating_add(1);
            return Ok(None);
        }

        // 6) 非关键帧：只编码变化瓦片。瓦片恒为原始分辨率，**不应用 options.scale**（见模块文档）。
        let mut tiles = Vec::with_capacity(changed.len());
        for (x, y, w, h) in changed {
            let rgb = crop_tile_rgb(&frame.rgba, stride, x, y, w, h);
            let mut jpeg = Vec::new();
            let encoder = JpegEncoder::new_with_quality(&mut jpeg, options.quality.min(100));
            encoder
                .write_image(&rgb, w, h, ExtendedColorType::Rgb8)
                .map_err(|err| RemoteError::Encode(format!("瓦片 JPEG 编码失败: {err}")))?;
            tiles.push(TilePatch {
                x,
                y,
                width: w,
                height: h,
                jpeg,
            });
        }
        self.state = Some(TileState {
            width: frame.width,
            height: frame.height,
            hashes,
        });
        self.frames_since_keyframe = self.frames_since_keyframe.saturating_add(1);
        Ok(Some(TileEncodeResult {
            keyframe: false,
            full: None,
            tiles,
            changed_ratio: ratio,
        }))
    }
}

/// 整帧声明尺寸的合理性上限（像素，对宽、高各自生效）。
///
/// 这不是「能显示多大」的限制，而是给**对端单方声明**的尺寸兜底：`width*height` 常被想当然地
/// 当成分配依据，放任 `10000x10000` 之类的声明，任何一处「按声明分配整帧缓冲」的代码都会被
/// 打爆。真实单台显示器最大为 8K（7680x4320），且本工具**一帧只对应一台物理显示器**（多显示器
/// 各自成帧），因此 16384（≈2 倍 8K 宽、≈4 倍 8K 高）已远超任何真实画面，同时把
/// `width*height*4` 的最坏情形压在 1 GiB 量级（真正的内存炸弹由 [`TILE_SIZE`] 上限在解码前挡下）。
pub const MAX_FRAME_DIM: u32 = 16384;

/// 把一组瓦片补丁解码成 [`TileUpdate`]（控制器侧调用）。
///
/// 允许空 `tiles`（无害但无意义，等价于一次空更新）。
///
/// # 为什么校验不能只跟「同一条消息里的声明」比
///
/// `width`/`height` 与每个 [`TilePatch`] 的矩形都是**对端（被控端）单方给出**的，同一条消息
/// 内部一致并不构成任何约束：攻击者可以声明一个 `10000x10000` 的整帧，再附上几块几十 KB 的
/// 纯色 JPEG（JPEG 对平坦色块压缩率极高，几十 KB 就能编码出上亿像素），让解码后的 RGBA 把
/// 控制器内存吃爆。所以这里必须用**与声明无关**的规则（[`TILE_SIZE`] 上限、[`TILE_SIZE`] 网格
/// 对齐、[`MAX_FRAME_DIM`]）在**解码之前**拒掉非法布局，绝不允许「先解出大图再比对尺寸」。
///
/// # 定位：纵深防御
///
/// 数据面本身已由一次性 token + AEAD 认证（见 `crate::crypto`），真正的对端是已授权的被控端，
/// 正常路径不会产出这里的任何非法布局。本层不是鉴权边界，而是**纵深防御**：令牌泄漏、被控端
/// 被攻陷或实现跑飞（bug 产出畸形帧）时，也不该让一条 ≤16 MiB 的消息把控制器打爆。
///
/// # Errors
/// 整帧声明尺寸为零或超过 [`MAX_FRAME_DIM`]、瓦片尺寸为零或超过 [`TILE_SIZE`]、坐标未按
/// [`TILE_SIZE`] 网格对齐、声明矩形越出整帧边界、JPEG 非法、或声明尺寸与解码结果不符时返回
/// [`crate::error::RemoteError::Encode`]。
pub fn decode_tiles(
    display: u32,
    seq: u64,
    width: u32,
    height: u32,
    tiles: &[TilePatch],
) -> RemoteResult<TileUpdate> {
    // 先校验整帧声明尺寸：它是所有瓦片坐标的参照系，也可能被别处当作分配依据。
    if width == 0 || height == 0 {
        return Err(RemoteError::Encode(format!(
            "整帧尺寸为零：{width}x{height}"
        )));
    }
    if width > MAX_FRAME_DIM || height > MAX_FRAME_DIM {
        return Err(RemoteError::Encode(format!(
            "整帧声明尺寸超出上限：{width}x{height}（上限 {MAX_FRAME_DIM}）"
        )));
    }

    let mut decoded = Vec::with_capacity(tiles.len());
    for patch in tiles {
        // 尺寸规则（**解码前**）：瓦片边长不得超过 TILE_SIZE，且坐标必须网格对齐。
        // 这两条是被控端编码侧的硬不变量（见 `tile_rect`/`TileDiffer::diff`：瓦片从左上角按
        // TILE_SIZE 步伐铺开，末列/末行的**部分**瓦片只收窄宽高、起点不变），同时把「声明超大
        // 尺寸」的内存炸弹挡在解码之前——攻击者无需真的构造一张巨图，几个字节的声明就会被拒。
        if patch.width == 0 || patch.height == 0 {
            return Err(RemoteError::Encode(format!(
                "瓦片尺寸为零：{}x{}",
                patch.width, patch.height
            )));
        }
        if patch.width > TILE_SIZE || patch.height > TILE_SIZE {
            return Err(RemoteError::Encode(format!(
                "瓦片尺寸超过 TILE_SIZE：{}x{}（上限 {}）",
                patch.width, patch.height, TILE_SIZE
            )));
        }
        if patch.x % TILE_SIZE != 0 || patch.y % TILE_SIZE != 0 {
            return Err(RemoteError::Encode(format!(
                "瓦片坐标未网格对齐：({}, {})（须为 {} 的整数倍）",
                patch.x, patch.y, TILE_SIZE
            )));
        }
        // 越界校验（声明矩形相对整帧；整帧尺寸本身已在上方校验）。
        let in_bounds = patch
            .x
            .checked_add(patch.width)
            .zip(patch.y.checked_add(patch.height))
            .is_some_and(|(right, bottom)| right <= width && bottom <= height);
        if !in_bounds {
            return Err(RemoteError::Encode(format!(
                "瓦片越界：({}, {}) {}x{} 超出整帧 {}x{}",
                patch.x, patch.y, patch.width, patch.height, width, height
            )));
        }

        let image = image::load_from_memory_with_format(&patch.jpeg, image::ImageFormat::Jpeg)
            .map_err(|err| RemoteError::Encode(format!("瓦片 JPEG 解码失败: {err}")))?;
        let rgba = image.to_rgba8();
        if rgba.width() != patch.width || rgba.height() != patch.height {
            return Err(RemoteError::Encode(format!(
                "瓦片尺寸与声明不符：声明 {}x{}，解码得 {}x{}",
                patch.width,
                patch.height,
                rgba.width(),
                rgba.height()
            )));
        }
        decoded.push(DecodedTile {
            x: patch.x,
            y: patch.y,
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        });
    }
    Ok(TileUpdate {
        display,
        width,
        height,
        seq,
        tiles: decoded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::QualityPreset;

    /// 均衡档编码参数（原始分辨率，瓦片路径生效）。
    fn opts() -> EncodeOptions {
        QualityPreset::Balanced.options()
    }

    /// 造一帧纯色 RGBA。
    fn solid(display: u32, w: u32, h: u32, color: [u8; 4]) -> RawFrame {
        RawFrame {
            display,
            width: w,
            height: h,
            rgba: color.repeat((w * h) as usize),
        }
    }

    /// 把帧内子矩形填成指定颜色（必须落在帧内）。
    fn fill_rect(frame: &mut RawFrame, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
        let stride = frame.width as usize * BYTES_PER_PIXEL;
        for yy in y..y + h {
            for xx in x..x + w {
                let off = yy as usize * stride + xx as usize * BYTES_PER_PIXEL;
                frame.rgba[off..off + BYTES_PER_PIXEL].copy_from_slice(&color);
            }
        }
    }

    /// 造一张 `w x h` 的合法 JPEG（借整帧编码路径，避免手写字节）。
    fn jpeg_of(w: u32, h: u32) -> Vec<u8> {
        let frame = solid(0, w, h, [1, 2, 3, 255]);
        let mut differ = TileDiffer::new();
        let out = differ.diff(&frame, &opts(), false).unwrap().expect("首帧应有输出");
        out.full.expect("关键帧应带整帧").jpeg
    }

    #[test]
    fn first_diff_is_keyframe() {
        let mut differ = TileDiffer::new();
        let frame = solid(0, 128, 96, [10, 20, 30, 255]);
        let out = differ.diff(&frame, &opts(), false).unwrap().expect("首帧应有输出");
        assert!(out.keyframe, "首帧必为关键帧");
        assert!(out.full.is_some(), "关键帧应带整帧");
        assert!(out.tiles.is_empty(), "关键帧不应带瓦片");
        assert!((out.changed_ratio - 1.0).abs() < f32::EPSILON, "首帧视作全变");
    }

    #[test]
    fn identical_frames_yield_no_output() {
        let mut differ = TileDiffer::new();
        let frame = solid(0, 128, 96, [10, 20, 30, 255]);
        differ.diff(&frame, &opts(), false).unwrap().expect("首帧应有输出");
        let second = differ.diff(&frame, &opts(), false).unwrap();
        assert!(second.is_none(), "同帧应零输出");
    }

    #[test]
    fn single_changed_tile_reports_only_that_tile() {
        // 192x192 → 3x3 = 9 块瓦片。
        let mut differ = TileDiffer::new();
        let mut frame = solid(0, 192, 192, [0, 0, 0, 255]);
        differ.diff(&frame, &opts(), false).unwrap().expect("首帧应有输出");

        // 改动严格落在瓦片 (1,1)（x,y ∈ [64,128)）内部。
        fill_rect(&mut frame, 70, 70, 10, 10, [255, 0, 0, 255]);
        let out = differ.diff(&frame, &opts(), false).unwrap().expect("应有瓦片输出");

        assert!(!out.keyframe, "局部改动不应触发关键帧");
        assert!(out.full.is_none(), "非关键帧不带整帧");
        assert_eq!(out.tiles.len(), 1, "只应报告变化的那一块瓦片");
        let tile = &out.tiles[0];
        assert_eq!((tile.x, tile.y, tile.width, tile.height), (64, 64, 64, 64));
        assert!(!tile.jpeg.is_empty(), "瓦片应有 JPEG 字节");
        assert!((out.changed_ratio - 1.0 / 9.0).abs() < 1e-6, "占比应为 1/9");
    }

    #[test]
    fn size_change_forces_keyframe() {
        let mut differ = TileDiffer::new();
        differ
            .diff(&solid(0, 64, 64, [1, 2, 3, 255]), &opts(), false)
            .unwrap()
            .expect("首帧应有输出");
        let out = differ
            .diff(&solid(0, 128, 128, [1, 2, 3, 255]), &opts(), false)
            .unwrap()
            .expect("尺寸变化应有输出");
        assert!(out.keyframe, "尺寸变化应强制关键帧");
        assert!(out.full.is_some());
        assert!(out.tiles.is_empty());
    }

    #[test]
    fn forced_keyframe_on_identical_pixels() {
        let mut differ = TileDiffer::new();
        let frame = solid(0, 128, 128, [7, 7, 7, 255]);
        differ.diff(&frame, &opts(), false).unwrap().expect("首帧应有输出");
        let out = differ.diff(&frame, &opts(), true).unwrap().expect("强制关键帧应有输出");
        assert!(out.keyframe, "force_keyframe 应压过同帧去重");
        assert!(out.full.is_some());
        assert!(out.tiles.is_empty());
    }

    #[test]
    fn keyframe_after_interval() {
        // 128x128 → 2x2 = 4 块瓦片；每帧只改 1 块 → 占比 0.25 < 0.6，不会因占比触发关键帧。
        let mut differ = TileDiffer::new();
        let mut frame = solid(0, 128, 128, [0, 0, 0, 255]);
        differ.diff(&frame, &opts(), false).unwrap().expect("首帧应有输出");

        for i in 0..KEYFRAME_INTERVAL_FRAMES {
            let shade = u8::try_from((i % 200) + 1).unwrap();
            fill_rect(&mut frame, 10, 10, 4, 4, [shade, 0, 0, 255]);
            let out = differ.diff(&frame, &opts(), false).unwrap().expect("应有输出");
            assert!(!out.keyframe, "第 {i} 次非关键帧差分不应提前发关键帧");
        }

        // 已累计 KEYFRAME_INTERVAL_FRAMES 次非关键帧 → 下一次必为关键帧。
        fill_rect(&mut frame, 10, 10, 4, 4, [250, 0, 0, 255]);
        let out = differ.diff(&frame, &opts(), false).unwrap().expect("应有输出");
        assert!(out.keyframe, "达到关键帧间隔后应发关键帧");
        assert!(out.full.is_some());
    }

    #[test]
    fn changed_ratio_forces_keyframe() {
        // 128x128 → 4 块瓦片；改 3 块 → 占比 0.75 > KEYFRAME_CHANGE_RATIO。
        let mut differ = TileDiffer::new();
        let mut frame = solid(0, 128, 128, [0, 0, 0, 255]);
        differ.diff(&frame, &opts(), false).unwrap().expect("首帧应有输出");

        fill_rect(&mut frame, 0, 0, 64, 64, [255, 0, 0, 255]);
        fill_rect(&mut frame, 64, 0, 64, 64, [0, 255, 0, 255]);
        fill_rect(&mut frame, 0, 64, 64, 64, [0, 0, 255, 255]);
        let out = differ.diff(&frame, &opts(), false).unwrap().expect("应有输出");
        assert!(out.keyframe, "变化占比超过阈值应改发整帧");
        assert!(out.changed_ratio > KEYFRAME_CHANGE_RATIO);
        assert!(out.tiles.is_empty());
    }

    #[test]
    fn non_unit_scale_always_keyframe() {
        // 「流畅」档降分辨率：瓦片恒为原始分辨率，故一律走整帧以保证不自洽组合不出现。
        let smooth = QualityPreset::Smooth.options();
        assert!((smooth.scale - 1.0).abs() >= f32::EPSILON, "用例前提：流畅档非原始分辨率");

        let mut differ = TileDiffer::new();
        let frame = solid(0, 128, 128, [5, 5, 5, 255]);
        let first = differ.diff(&frame, &smooth, false).unwrap().expect("应有输出");
        assert!(first.keyframe, "非 1.0 缩放应强制关键帧");

        // 同一帧再次差分仍应是关键帧（不能被整帧去重静默掉）。
        let second = differ.diff(&frame, &smooth, false).unwrap().expect("缩放档应始终关键帧");
        assert!(second.keyframe);
        assert!(second.full.is_some());
    }

    #[test]
    fn partial_edge_tiles_cover_frame_exactly() {
        let (width, height) = (100u32, 70u32);
        let mut differ = TileDiffer::new();
        let out = differ
            .diff(&solid(0, width, height, [1, 2, 3, 255]), &opts(), false)
            .unwrap()
            .expect("首帧应有输出");
        assert!(out.keyframe);

        let columns = width.div_ceil(TILE_SIZE);
        let rows = height.div_ceil(TILE_SIZE);
        assert_eq!((columns, rows), (2, 2));

        let mut rects = Vec::new();
        for row in 0..rows {
            for col in 0..columns {
                rects.push(tile_rect(col, row, width, height));
            }
        }
        assert_eq!(
            rects,
            vec![(0, 0, 64, 64), (64, 0, 36, 64), (0, 64, 64, 6), (64, 64, 36, 6)],
            "边缘瓦片应精确收窄"
        );
        // 面积之和等于整帧面积，且各块互不重叠（矩形按网格枚举，天然是划分）→ 无缝铺满。
        let area: u32 = rects.iter().map(|(_, _, w, h)| w * h).sum();
        assert_eq!(area, width * height, "瓦片应无缝铺满整帧");
        for (x, y, w, h) in rects {
            assert!(x + w <= width && y + h <= height, "瓦片不应越界");
            assert!(w > 0 && h > 0, "瓦片尺寸应非零");
        }
    }

    #[test]
    fn decode_tiles_roundtrip() {
        let mut differ = TileDiffer::new();
        let mut frame = solid(0, 128, 128, [20, 40, 60, 255]);
        differ.diff(&frame, &opts(), false).unwrap().expect("首帧应有输出");

        fill_rect(&mut frame, 70, 70, 20, 20, [200, 100, 50, 255]);
        let out = differ.diff(&frame, &opts(), false).unwrap().expect("应有瓦片输出");
        assert_eq!(out.tiles.len(), 1);

        let update = decode_tiles(3, 7, 128, 128, &out.tiles).unwrap();
        assert_eq!((update.display, update.seq), (3, 7));
        assert_eq!((update.width, update.height), (128, 128));
        assert_eq!(update.tiles.len(), 1);
        let tile = &update.tiles[0];
        assert_eq!((tile.x, tile.y, tile.width, tile.height), (64, 64, 64, 64));
        assert_eq!(tile.rgba.len(), 64 * 64 * BYTES_PER_PIXEL);

        // 采样瓦片内 (16, 16)（= 整帧 (80, 80)，改动区 70..90 的正中），远离 JPEG 块边界以避开
        // 有损压缩在色块交界处的振铃/色度下采样误差。
        let off = (16 * 64 + 16) * BYTES_PER_PIXEL;
        assert!((i32::from(tile.rgba[off]) - 200).abs() <= 12, "R 通道还原偏差过大");
        assert!((i32::from(tile.rgba[off + 1]) - 100).abs() <= 12, "G 通道还原偏差过大");
        assert!((i32::from(tile.rgba[off + 2]) - 50).abs() <= 12, "B 通道还原偏差过大");
        assert_eq!(tile.rgba[off + 3], 255, "alpha 应为不透明");
    }

    #[test]
    fn decode_tiles_rejects_out_of_bounds_patch() {
        // x = 128 为网格对齐（128 % 64 == 0），故能越过新的「尺寸/对齐」校验而落在越界分支：
        // 若沿用旧坐标 x = 100（未对齐）会先被对齐规则拒绝，无法再验证越界语义。
        let patch = TilePatch {
            x: 128,
            y: 0,
            width: 64,
            height: 64,
            jpeg: jpeg_of(64, 64),
        };
        let err = decode_tiles(0, 0, 128, 128, &[patch]).expect_err("越界瓦片应被拒绝");
        assert!(err.to_string().contains("越界"), "错误应指向越界：{err}");
    }

    #[test]
    fn decode_tiles_rejects_oversized_tile_before_decoding() {
        // 声明 10000x10000 的超大瓦片、坐标网格对齐（0,0）、JPEG 字段给**垃圾字节**。
        // 若实现是「先解码再比对尺寸」，这里会先因垃圾字节报「JPEG 解码失败」；本用例要求错误
        // 指向**尺寸规则**，即证明校验发生在解码之前——攻击者无需真的构造一张巨图（几字节的
        // 声明即可被拒），这正是挡下内存炸弹的关键。
        let patch = TilePatch {
            x: 0,
            y: 0,
            width: 10_000,
            height: 10_000,
            jpeg: b"not a jpeg at all".to_vec(),
        };
        let err = decode_tiles(0, 0, 128, 128, &[patch]).expect_err("超大瓦片应被拒绝");
        let msg = err.to_string();
        assert!(msg.contains("TILE_SIZE"), "错误应指向瓦片尺寸上限：{msg}");
        assert!(!msg.contains("JPEG"), "不应走到 JPEG 解码：{msg}");
    }

    #[test]
    fn decode_tiles_rejects_unaligned_coordinates() {
        // 网格对齐是硬不变量：瓦片恒为原生分辨率、与整帧的关键帧网格逐格对应（见模块文档），
        // 非对齐坐标既无法与控制器已持有的整帧像素对应，也说明对端在伪造布局。
        let patch = TilePatch {
            x: 1,
            y: 0,
            width: 64,
            height: 64,
            jpeg: jpeg_of(64, 64),
        };
        let err = decode_tiles(0, 0, 128, 128, &[patch]).expect_err("未对齐坐标应被拒绝");
        assert!(err.to_string().contains("对齐"), "错误应指向网格对齐：{err}");
    }

    #[test]
    fn decode_tiles_accepts_aligned_and_partial_edge_tiles() {
        // 整帧 100 宽 → 网格 2 列：col0 起点 0、宽 64；col1 起点 64、宽 36（末列部分瓦片）。
        // 两条都必须通过——这正是编码侧 `tile_rect` 的产出（起点对齐、末列只收窄宽高）。
        let full = TilePatch {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            jpeg: jpeg_of(64, 64),
        };
        let edge = TilePatch {
            x: 64,
            y: 0,
            width: 36,
            height: 64,
            jpeg: jpeg_of(36, 64),
        };
        let update = decode_tiles(0, 0, 100, 100, &[full, edge]).expect("对齐瓦片与部分边缘瓦片应可解码");
        assert_eq!(update.tiles.len(), 2);
        assert_eq!(
            (update.tiles[0].x, update.tiles[0].width, update.tiles[0].height),
            (0, 64, 64),
            "整瓦片尺寸应与声明一致"
        );
        assert_eq!(
            (update.tiles[1].x, update.tiles[1].width, update.tiles[1].height),
            (64, 36, 64),
            "末列部分瓦片尺寸应与声明一致"
        );
        assert_eq!(update.tiles[1].rgba.len(), 36 * 64 * BYTES_PER_PIXEL, "部分瓦片 RGBA 长度应正确");
    }

    #[test]
    fn decode_tiles_rejects_zero_size_patch() {
        let patch = TilePatch {
            x: 0,
            y: 0,
            width: 0,
            height: 64,
            jpeg: jpeg_of(64, 64),
        };
        assert!(matches!(decode_tiles(0, 0, 128, 128, &[patch]), Err(RemoteError::Encode(_))));
    }

    #[test]
    fn decode_tiles_rejects_dimension_mismatch() {
        // 声明 32x32，实际 JPEG 是 64x64。
        let patch = TilePatch {
            x: 0,
            y: 0,
            width: 32,
            height: 32,
            jpeg: jpeg_of(64, 64),
        };
        assert!(matches!(decode_tiles(0, 0, 128, 128, &[patch]), Err(RemoteError::Encode(_))));
    }

    #[test]
    fn decode_tiles_rejects_garbage_jpeg() {
        let patch = TilePatch {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            jpeg: b"not a jpeg".to_vec(),
        };
        assert!(matches!(decode_tiles(0, 0, 128, 128, &[patch]), Err(RemoteError::Encode(_))));
    }

    #[test]
    fn decode_tiles_allows_empty() {
        let update = decode_tiles(0, 3, 64, 64, &[]).unwrap();
        assert!(update.tiles.is_empty());
        assert_eq!((update.display, update.seq, update.width, update.height), (0, 3, 64, 64));
    }

    #[test]
    fn reset_forces_keyframe() {
        let mut differ = TileDiffer::new();
        let frame = solid(0, 128, 128, [9, 9, 9, 255]);
        differ.diff(&frame, &opts(), false).unwrap().expect("首帧应有输出");
        assert!(differ.diff(&frame, &opts(), false).unwrap().is_none(), "同帧应零输出");

        differ.reset();
        let out = differ.diff(&frame, &opts(), false).unwrap().expect("重置后应有输出");
        assert!(out.keyframe, "重置后下一次必为关键帧");
        assert!(out.full.is_some());
    }

    #[test]
    fn invalid_frame_is_rejected() {
        let mut differ = TileDiffer::new();
        let bad = RawFrame {
            display: 0,
            width: 4,
            height: 4,
            rgba: vec![0u8; 10],
        };
        assert!(matches!(differ.diff(&bad, &opts(), false), Err(RemoteError::Encode(_))));
    }

    #[test]
    fn zero_sized_frame_is_rejected() {
        let mut differ = TileDiffer::new();
        let empty = RawFrame {
            display: 0,
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
        assert!(matches!(differ.diff(&empty, &opts(), false), Err(RemoteError::Encode(_))));
    }
}
