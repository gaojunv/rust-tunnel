//! 屏幕/输入平台抽象与屏幕编码。
//!
//! # 平台 trait
//!
//! - [`ScreenSource`]：被控端采集屏幕（`xcap` 实现）。
//! - [`InputSink`]：被控端注入鼠标键盘（`enigo` 实现）。
//! - [`FrameSink`]：控制器侧消费已解码画面（egui 实现）。
//! - [`Clipboard`]：被控端读写文本剪贴板（`arboard` 实现）。
//!
//! 本 crate 只定义接口，平台实现放在 `crates/client-gui`。文件传输**不是**平台相关的
//! （三平台都有文件系统），故不走 trait，由 [`crate::file`] 直接实现。
//!
//! # 编码
//!
//! 两条路径，都在引擎的运行时线程里完成，[`FrameSink`] 拿到的永远是**已解码的 RGBA**
//! （UI 线程只负责上传纹理，避免在 egui 线程里解码掉帧）：
//!
//! - **整帧**：[`encode_frame`] 是纯函数，输入「上一帧的原始 RGBA」与当前 RGBA 帧，相同则返回
//!   `None`（静止桌面零输出）。用作首帧与关键帧。
//! - **瓦片差分**：[`crate::tiles`] 把整帧切成 [`TILE_SIZE`] 见方的瓦片，只编码变化的那些，
//!   经 [`FrameSink::on_tiles`] 交给 UI 做局部纹理更新。这是带宽的关键——整帧 JPEG 在
//!   1080p@10fps 约 15–25 Mbps，经中继不可接受；静止桌面下瓦片路径趋近零流量。
//!
//! 画质档位见 [`QualityPreset`]，帧率上限见 [`EncodeOptions::max_fps`]。

use crate::error::{RemoteError, RemoteResult};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use serde::{Deserialize, Serialize};

/// 每像素字节数（RGBA）。
pub const BYTES_PER_PIXEL: usize = 4;

/// 瓦片边长（像素）。见 [`crate::tiles`] 的差分说明。
pub const TILE_SIZE: u32 = 64;

/// 一台显示器的元信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// 平台显示器编号（采集/注入时回填）。
    pub id: u32,
    /// 宽度（像素）。
    pub width: u32,
    /// 高度（像素）。
    pub height: u32,
    /// 显示器名称。
    pub name: String,
    /// 是否为主显示器。
    pub primary: bool,
}

/// 一帧原始 RGBA 画面（未编码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// 来源显示器编号。
    pub display: u32,
    /// 宽度（像素）。
    pub width: u32,
    /// 高度（像素）。
    pub height: u32,
    /// RGBA8 像素数据，长度必须为 `width * height * 4`。
    pub rgba: Vec<u8>,
}

impl RawFrame {
    /// 校验像素数据长度是否与宽高一致。
    ///
    /// # Errors
    /// 长度不匹配时返回 [`RemoteError::Encode`]。
    pub fn validate(&self) -> RemoteResult<()> {
        let expected = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
            .ok_or_else(|| RemoteError::Encode("帧尺寸溢出".to_owned()))?;
        if self.rgba.len() != expected {
            return Err(RemoteError::Encode(format!(
                "RGBA 长度 {} 与 {}x{} 不符（应为 {}）",
                self.rgba.len(),
                self.width,
                self.height,
                expected
            )));
        }
        Ok(())
    }
}

/// 已解码的一帧画面（交给 UI 上传纹理）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// 来源显示器编号。
    pub display: u32,
    /// 宽度（像素）。
    pub width: u32,
    /// 高度（像素）。
    pub height: u32,
    /// 帧序号（对端递增，用于丢帧判断）。
    pub seq: u64,
    /// RGBA8 像素数据。
    pub rgba: Vec<u8>,
}

/// 鼠标按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// 左键。
    Left,
    /// 右键。
    Right,
    /// 中键。
    Middle,
}

/// 一次键盘事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    /// 平台扫描码/键码（由上层约定）。
    pub code: u32,
    /// 是否按下（`false` 为抬起）。
    pub pressed: bool,
    /// 可打印文本（可选，供上层按需注入）。
    pub text: Option<String>,
    /// 修饰键位掩码（Ctrl/Alt/Shift/Meta，位序由上层约定）。
    pub modifiers: u8,
}

/// 一块瓦片补丁（只含变化区域，JPEG 编码）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TilePatch {
    /// 瓦片左上角 X（整帧像素坐标）。
    pub x: u32,
    /// 瓦片左上角 Y（整帧像素坐标）。
    pub y: u32,
    /// 瓦片宽（像素，边缘瓦片可小于 [`TILE_SIZE`]）。
    pub width: u32,
    /// 瓦片高（像素，边缘瓦片可小于 [`TILE_SIZE`]）。
    pub height: u32,
    /// JPEG 字节。
    pub jpeg: Vec<u8>,
}

/// 已解码的一块瓦片（交给 UI 做局部纹理更新）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedTile {
    /// 瓦片左上角 X（整帧像素坐标）。
    pub x: u32,
    /// 瓦片左上角 Y（整帧像素坐标）。
    pub y: u32,
    /// 瓦片宽（像素）。
    pub width: u32,
    /// 瓦片高（像素）。
    pub height: u32,
    /// RGBA8 像素数据，长度 = `width * height * 4`。
    pub rgba: Vec<u8>,
}

/// 一次增量画面更新（整帧坐标系 + 若干已解码瓦片）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileUpdate {
    /// 来源显示器编号。
    pub display: u32,
    /// 整帧宽（像素）。
    pub width: u32,
    /// 整帧高（像素）。
    pub height: u32,
    /// 帧序号（对端递增）。
    pub seq: u64,
    /// 本次更新的瓦片。
    pub tiles: Vec<DecodedTile>,
}

/// 目录项（文件传输用；`crate::file` 与 UI 共用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// 文件名（不含路径）。
    pub name: String,
    /// 相对共享根的虚拟路径（`/` 分隔，根为 `""`）。
    pub path: String,
    /// 是否目录。
    pub is_dir: bool,
    /// 字节数（目录为 0）。
    pub size: u64,
    /// 修改时间（毫秒，自 epoch）；取不到为 0。
    pub modified_ms: u64,
}

/// 一条输入事件（控制器 → 被控端）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// 鼠标移动。
    MouseMove {
        /// X 坐标。
        x: i32,
        /// Y 坐标。
        y: i32,
        /// 目标显示器编号。
        display: u32,
    },
    /// 鼠标按键。
    MouseButton {
        /// 按键。
        button: MouseButton,
        /// 是否按下。
        pressed: bool,
    },
    /// 滚轮。
    Wheel {
        /// 水平滚动量。
        dx: i32,
        /// 垂直滚动量。
        dy: i32,
    },
    /// 键盘事件。
    Key(KeyEvent),
}

/// 屏幕采集源（被控端实现）。
pub trait ScreenSource: Send + Sync + 'static {
    /// 当前所有显示器。
    fn displays(&self) -> Vec<DisplayInfo>;

    /// 采集指定显示器的一帧。
    ///
    /// # Errors
    /// 采集失败（显示器不存在、平台权限不足等）时返回 [`RemoteError::Platform`]。
    fn capture(&self, display: u32) -> RemoteResult<RawFrame>;
}

/// 输入注入目标（被控端实现）。
pub trait InputSink: Send + Sync + 'static {
    /// 鼠标移动到绝对坐标。
    ///
    /// # Errors
    /// 注入失败时返回 [`RemoteError::Platform`]。
    fn mouse_move(&self, x: i32, y: i32, display: u32) -> RemoteResult<()>;

    /// 鼠标按键按下/抬起。
    ///
    /// # Errors
    /// 注入失败时返回 [`RemoteError::Platform`]。
    fn mouse_button(&self, button: MouseButton, pressed: bool) -> RemoteResult<()>;

    /// 滚轮滚动。
    ///
    /// # Errors
    /// 注入失败时返回 [`RemoteError::Platform`]。
    fn wheel(&self, dx: i32, dy: i32) -> RemoteResult<()>;

    /// 键盘事件。
    ///
    /// # Errors
    /// 注入失败时返回 [`RemoteError::Platform`]。
    fn key(&self, event: KeyEvent) -> RemoteResult<()>;

    /// 按 [`InputEvent`] 分发到上述四个方法（默认实现，平台实现可覆写）。
    ///
    /// # Errors
    /// 分发到具体注入方法失败时透传其错误。
    fn apply(&self, event: InputEvent) -> RemoteResult<()> {
        match event {
            InputEvent::MouseMove { x, y, display } => self.mouse_move(x, y, display),
            InputEvent::MouseButton { button, pressed } => self.mouse_button(button, pressed),
            InputEvent::Wheel { dx, dy } => self.wheel(dx, dy),
            InputEvent::Key(event) => self.key(event),
        }
    }
}

/// 控制器侧的画面上报目标（UI 实现）。
///
/// **调用发生在引擎的运行时线程**：`on_frame` / `on_tiles` 收到的已是解码后的 RGBA，
/// UI 线程只需上传纹理。
pub trait FrameSink: Send + Sync + 'static {
    /// 显示器列表（会话建立时下发一次，变更时再次下发）。
    fn on_displays(&self, displays: Vec<DisplayInfo>);

    /// 一帧已解码**整帧**画面（首帧与关键帧走这里）。
    fn on_frame(&self, frame: DecodedFrame);

    /// 一次增量瓦片更新（只含变化区域）。
    ///
    /// 默认实现为空操作：忽略它的 sink 会在下一个整帧（关键帧）自行恢复——被控端保证
    /// 周期性发关键帧，所以这不是正确性缺口，只是「静止画面之外的更新被推迟到关键帧」。
    /// egui 实现应覆写它并用 `TextureHandle::set_partial` 做局部更新。
    fn on_tiles(&self, update: TileUpdate) {
        let _ = update;
    }

    /// 对端（被控端）发来的剪贴板文本。
    ///
    /// 控制器侧不实现 [`Clipboard`]：它读写的本来就是**本机**剪贴板，UI 直接用自己的实现即可。
    /// 默认实现为空操作——忽略它只意味着不做剪贴板同步，不影响会话。
    fn on_clipboard(&self, text: String) {
        let _ = text;
    }

    /// 会话关闭。
    fn on_closed(&self, reason: String);
}

/// 文本剪贴板读写（被控端实现；控制器侧直接用 egui 自带的剪贴板）。
pub trait Clipboard: Send + Sync + 'static {
    /// 读取当前文本剪贴板。
    ///
    /// # Errors
    /// 剪贴板不可用（无显示、被占用、内容非文本）时返回 [`RemoteError::Platform`]。
    fn get(&self) -> RemoteResult<String>;

    /// 写入文本剪贴板。
    ///
    /// # Errors
    /// 剪贴板不可用时返回 [`RemoteError::Platform`]。
    fn set(&self, text: &str) -> RemoteResult<()>;
}

/// 屏幕编码参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeOptions {
    /// JPEG 质量（0..=100）。
    pub quality: u8,
    /// 缩放比例（1.0 = 原始尺寸）。既决定整帧编码尺寸，也决定瓦片差分所在的坐标系
    /// （见 [`crate::tiles`]：差分在缩放后的帧上进行，保证瓦片与控制器持有的整帧同格）。
    pub scale: f32,
    /// 目标帧率上限（被控端据此定采集间隔）。
    pub max_fps: u8,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        QualityPreset::default().options()
    }
}

impl EncodeOptions {
    /// 取该档位的目标采集间隔（`max_fps` 为 0 时按 1 fps 兜底，避免除零）。
    #[must_use]
    pub fn capture_interval(&self) -> std::time::Duration {
        let fps = self.max_fps.max(1);
        std::time::Duration::from_millis(1000 / u64::from(fps))
    }
}

/// 画质档位：UI 只暴露这三档，映射到具体的 [`EncodeOptions`]。
///
/// 档位经 `SessionMessage::SetQuality` 下发给被控端，**由被控端生效**（编码在它那边）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum QualityPreset {
    /// 流畅：半分辨率 + 低质量 + 8 fps。窄带/跨地域中继场景。
    Smooth,
    /// 均衡：原始分辨率 + 中质量 + 12 fps。默认档。
    #[default]
    Balanced,
    /// 高清：原始分辨率 + 高质量 + 20 fps。同内网直连场景。
    Hd,
}

impl QualityPreset {
    /// 映射到编码参数。
    #[must_use]
    pub fn options(self) -> EncodeOptions {
        match self {
            Self::Smooth => EncodeOptions {
                quality: 45,
                scale: 0.5,
                max_fps: 8,
            },
            Self::Balanced => EncodeOptions {
                quality: 70,
                scale: 1.0,
                max_fps: 12,
            },
            Self::Hd => EncodeOptions {
                quality: 85,
                scale: 1.0,
                max_fps: 20,
            },
        }
    }

    /// UI 中文名。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Smooth => "流畅",
            Self::Balanced => "均衡",
            Self::Hd => "高清",
        }
    }

    /// 全部档位（UI 选择器用）。
    #[must_use]
    pub fn all() -> [Self; 3] {
        [Self::Smooth, Self::Balanced, Self::Hd]
    }
}

/// 一帧编码结果。
///
/// **不带哈希**：帧去重的判据是「与上一帧的像素是否逐字节相同」，[`encode_frame`] 已经用返回值
/// （`None` = 与上一帧相同）表达了这个信息，再挂一个哈希字段既冗余、又容易让人误以为它是密码学
/// 意义上的完整性摘要。去重本身也不需要哈希，直接比对上一帧的 RGBA 副本即可（memcmp 远快于
/// SHA-256：4K 逐帧实测 54 ms → 数 ms）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// 来源显示器编号。
    pub display: u32,
    /// 编码后尺寸的宽度。
    pub width: u32,
    /// 编码后尺寸的高度。
    pub height: u32,
    /// JPEG 字节。
    pub jpeg: Vec<u8>,
}

/// 按比例计算缩放后的尺寸（下限 1 像素）。
// 缩放比例是有界浮点、尺寸远小于 u32 上限，截断/符号转换在此安全。
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "缩放比例由调用方给出且远小于 u32 上限，结果为非负整数像素"
)]
pub(crate) fn scaled_dimension(original: u32, scale: f32) -> u32 {
    let scaled = (f64::from(original) * f64::from(scale)).round();
    if scaled < 1.0 {
        1
    } else {
        scaled as u32
    }
}

/// 把一帧 RGBA 编码为 JPEG；**与上一帧像素完全相同则返回 `None`**。
///
/// 这是本模块的核心纯函数：`prev_rgba` 为上一帧的原始 RGBA（无上一帧时传 `None`）。
/// 比对基于**原始 RGBA**，因此缩放不影响去重判定。
///
/// 用切片相等（memcmp）而不是密码学哈希：去重只需要「与上一帧是否相同」这一个布尔答案，不需要
/// 抗碰撞性。保留上一帧的副本是必须的——这正是去重的定义。
///
/// # Errors
/// 帧尺寸与像素数据不匹配、JPEG 编码失败时返回 [`RemoteError::Encode`]。
pub fn encode_frame(
    prev_rgba: Option<&[u8]>,
    frame: &RawFrame,
    options: &EncodeOptions,
) -> RemoteResult<Option<EncodedFrame>> {
    frame.validate()?;
    if prev_rgba == Some(frame.rgba.as_slice()) {
        return Ok(None);
    }

    let image = RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| RemoteError::Encode("无法从 RGBA 构造图像".to_owned()))?;

    let (out_w, out_h) = if (options.scale - 1.0).abs() < f32::EPSILON {
        (frame.width, frame.height)
    } else {
        (
            scaled_dimension(frame.width, options.scale),
            scaled_dimension(frame.height, options.scale),
        )
    };
    let image = if (out_w, out_h) == (frame.width, frame.height) {
        image
    } else {
        image::imageops::resize(&image, out_w, out_h, FilterType::Triangle)
    };

    // image 的 JPEG 编码器不支持 RGBA8，需先去掉 alpha 通道转 RGB8（桌面画面本就不透明）。
    let rgb = image::DynamicImage::ImageRgba8(image).to_rgb8();
    let mut jpeg = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut jpeg, options.quality.min(100));
    encoder
        .write_image(rgb.as_raw(), out_w, out_h, ExtendedColorType::Rgb8)
        .map_err(|err| RemoteError::Encode(format!("JPEG 编码失败: {err}")))?;

    Ok(Some(EncodedFrame {
        display: frame.display,
        width: out_w,
        height: out_h,
        jpeg,
    }))
}

/// 把一帧 JPEG 解码为 RGBA。
///
/// # Errors
/// JPEG 数据非法时返回 [`RemoteError::Encode`]。
pub fn decode_frame(display: u32, seq: u64, jpeg: &[u8]) -> RemoteResult<DecodedFrame> {
    let image = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .map_err(|err| RemoteError::Encode(format!("JPEG 解码失败: {err}")))?;
    let rgba = image.to_rgba8();
    Ok(DecodedFrame {
        display,
        width: rgba.width(),
        height: rgba.height(),
        seq,
        rgba: rgba.into_raw(),
    })
}

/// 一个什么都不做的 [`FrameSink`]（UI 尚未安装时的兜底，避免会话因缺 sink 而失败）。
#[derive(Debug, Default)]
pub struct NullFrameSink;

impl FrameSink for NullFrameSink {
    fn on_displays(&self, _displays: Vec<DisplayInfo>) {}
    fn on_frame(&self, _frame: DecodedFrame) {}
    fn on_closed(&self, _reason: String) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一帧纯色 RGBA。
    fn solid(display: u32, w: u32, h: u32, color: [u8; 4]) -> RawFrame {
        let mut rgba = Vec::with_capacity((w * h) as usize * BYTES_PER_PIXEL);
        for _ in 0..(w * h) {
            rgba.extend_from_slice(&color);
        }
        RawFrame {
            display,
            width: w,
            height: h,
            rgba,
        }
    }

    #[test]
    fn same_frame_yields_no_output() {
        let opts = EncodeOptions::default();
        let frame = solid(0, 32, 24, [10, 20, 30, 255]);
        encode_frame(None, &frame, &opts).unwrap().expect("首帧应有输出");
        // 同一帧（像素逐字节相同）→ 零输出。基准就是上一帧的 RGBA 副本。
        let second = encode_frame(Some(&frame.rgba), &frame, &opts).unwrap();
        assert!(second.is_none(), "同帧应零输出");
    }

    #[test]
    fn different_frame_yields_output() {
        let opts = EncodeOptions::default();
        let a = solid(0, 32, 24, [10, 20, 30, 255]);
        let b = solid(0, 32, 24, [200, 100, 50, 255]);
        let eb = encode_frame(Some(&a.rgba), &b, &opts).unwrap().expect("不同帧应有输出");
        assert!(!eb.jpeg.is_empty());
    }

    #[test]
    fn scale_changes_encoded_dimensions_not_dedup() {
        let opts = EncodeOptions {
            quality: 80,
            scale: 0.5,
            ..EncodeOptions::default()
        };
        let frame = solid(1, 40, 20, [1, 2, 3, 255]);
        let encoded = encode_frame(None, &frame, &opts).unwrap().unwrap();
        assert_eq!((encoded.width, encoded.height), (20, 10), "缩放后尺寸应为一半");
        // 去重基于原始 RGBA，与是否缩放无关：同一原始帧仍判为「无变化」。
        assert!(
            encode_frame(Some(&frame.rgba), &frame, &opts).unwrap().is_none(),
            "缩放不应影响同帧去重"
        );
    }

    #[test]
    fn roundtrip_encode_decode() {
        let opts = EncodeOptions::default();
        let frame = solid(0, 16, 16, [123, 45, 67, 255]);
        let encoded = encode_frame(None, &frame, &opts).unwrap().unwrap();
        let decoded = decode_frame(0, 7, &encoded.jpeg).unwrap();
        assert_eq!(decoded.display, 0);
        assert_eq!(decoded.seq, 7);
        assert_eq!((decoded.width, decoded.height), (16, 16));
        assert_eq!(decoded.rgba.len(), 16 * 16 * BYTES_PER_PIXEL);
        // 纯色 JPEG 解码后各通道应基本还原（允许有损误差）。
        assert!((i32::from(decoded.rgba[0]) - 123).abs() <= 8, "R 通道还原偏差过大");
    }

    #[test]
    fn mismatched_rgba_length_is_rejected() {
        let opts = EncodeOptions::default();
        let bad = RawFrame {
            display: 0,
            width: 4,
            height: 4,
            rgba: vec![0u8; 10],
        };
        assert!(matches!(encode_frame(None, &bad, &opts), Err(RemoteError::Encode(_))));
    }

    #[test]
    fn garbage_jpeg_is_rejected() {
        assert!(matches!(decode_frame(0, 0, b"not a jpeg"), Err(RemoteError::Encode(_))));
    }

    #[test]
    fn presets_are_ordered_by_fidelity_and_cost() {
        let smooth = QualityPreset::Smooth.options();
        let balanced = QualityPreset::Balanced.options();
        let hd = QualityPreset::Hd.options();
        // 画质、帧率单调递增；流畅档降分辨率。
        assert!(smooth.quality < balanced.quality && balanced.quality < hd.quality);
        assert!(smooth.max_fps < balanced.max_fps && balanced.max_fps < hd.max_fps);
        assert!(smooth.scale < 1.0, "流畅档应降分辨率以省带宽");
        assert!((balanced.scale - 1.0).abs() < f32::EPSILON);
        assert!((hd.scale - 1.0).abs() < f32::EPSILON);
        // 默认档即均衡档（避免两处默认值漂移）。
        assert_eq!(EncodeOptions::default(), balanced);
        assert_eq!(QualityPreset::all().len(), 3);
    }

    #[test]
    fn capture_interval_never_divides_by_zero() {
        let opts = EncodeOptions {
            max_fps: 0,
            ..EncodeOptions::default()
        };
        assert_eq!(opts.capture_interval(), std::time::Duration::from_millis(1000));
        assert_eq!(
            QualityPreset::Hd.options().capture_interval(),
            std::time::Duration::from_millis(50)
        );
    }
}
