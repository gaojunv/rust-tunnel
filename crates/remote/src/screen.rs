//! 屏幕/输入平台抽象与屏幕编码。
//!
//! # 三个 trait
//!
//! - [`ScreenSource`]：被控端采集屏幕（`xcap` 实现）。
//! - [`InputSink`]：被控端注入鼠标键盘（`enigo` 实现）。
//! - [`FrameSink`]：控制器侧消费已解码画面（egui 实现）。
//!
//! 本 crate 只定义接口，平台实现放在 `crates/client-gui`。
//!
//! # 编码（阶段 1：整帧 JPEG + 帧哈希去重）
//!
//! [`encode_frame`] 是**纯函数**：输入「上一帧哈希」与当前 RGBA 帧，输出可选编码结果。
//! 若当前帧哈希与上一帧相同则返回 `None`（静止桌面近零流量，只留心跳）。JPEG 编码在
//! 引擎的运行时线程里完成；[`FrameSink::on_frame`] 拿到的是**已解码的 RGBA**，UI 线程
//! 只负责上传纹理，避免在 egui 线程里解码掉帧。
//!
//! 瓦片差分是阶段 2，本轮不做，但 [`EncodeOptions`] / [`EncodedFrame`] 已为多路复用留位。

use crate::error::{RemoteError, RemoteResult};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 每像素字节数（RGBA）。
pub const BYTES_PER_PIXEL: usize = 4;

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
/// **调用发生在引擎的运行时线程**：`on_frame` 收到的已是解码后的 RGBA，UI 线程只需上传纹理。
pub trait FrameSink: Send + Sync + 'static {
    /// 显示器列表（会话建立时下发一次，变更时再次下发）。
    fn on_displays(&self, displays: Vec<DisplayInfo>);

    /// 一帧已解码画面。
    fn on_frame(&self, frame: DecodedFrame);

    /// 会话关闭。
    fn on_closed(&self, reason: String);
}

/// 屏幕编码参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeOptions {
    /// JPEG 质量（0..=100）。
    pub quality: u8,
    /// 缩放比例（1.0 = 原始尺寸；仅缩放发送，不影响哈希比对）。
    pub scale: f32,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            quality: 75,
            scale: 1.0,
        }
    }
}

/// 一帧编码结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// 来源显示器编号。
    pub display: u32,
    /// 编码后尺寸的宽度。
    pub width: u32,
    /// 编码后尺寸的高度。
    pub height: u32,
    /// 原始 RGBA 的 SHA-256 哈希（用于与上一帧比对去重）。
    pub hash: [u8; 32],
    /// JPEG 字节。
    pub jpeg: Vec<u8>,
}

/// 计算一帧 RGBA 的 SHA-256 哈希。
#[must_use]
pub fn frame_hash(rgba: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(rgba);
    hasher.finalize().into()
}

/// 按比例计算缩放后的尺寸（下限 1 像素）。
// 缩放比例是有界浮点、尺寸远小于 u32 上限，截断/符号转换在此安全。
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "缩放比例由调用方给出且远小于 u32 上限，结果为非负整数像素"
)]
fn scaled_dimension(original: u32, scale: f32) -> u32 {
    let scaled = (f64::from(original) * f64::from(scale)).round();
    if scaled < 1.0 {
        1
    } else {
        scaled as u32
    }
}

/// 把一帧 RGBA 编码为 JPEG；**与上一帧相同则返回 `None`**。
///
/// 这是本模块的核心纯函数：`prev_hash` 为上一帧的 [`frame_hash`]（无上一帧时传 `None`）。
/// 哈希比对基于**原始 RGBA**，因此缩放不影响去重判定。
///
/// # Errors
/// 帧尺寸与像素数据不匹配、JPEG 编码失败时返回 [`RemoteError::Encode`]。
pub fn encode_frame(
    prev_hash: Option<&[u8; 32]>,
    frame: &RawFrame,
    options: &EncodeOptions,
) -> RemoteResult<Option<EncodedFrame>> {
    frame.validate()?;
    let hash = frame_hash(&frame.rgba);
    if prev_hash == Some(&hash) {
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
        hash,
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
        let first = encode_frame(None, &frame, &opts).unwrap().expect("首帧应有输出");
        // 同一帧（哈希相同）→ 零输出。
        let second = encode_frame(Some(&first.hash), &frame, &opts).unwrap();
        assert!(second.is_none(), "同帧应零输出");
    }

    #[test]
    fn different_frame_yields_output() {
        let opts = EncodeOptions::default();
        let a = solid(0, 32, 24, [10, 20, 30, 255]);
        let b = solid(0, 32, 24, [200, 100, 50, 255]);
        let ea = encode_frame(None, &a, &opts).unwrap().unwrap();
        let eb = encode_frame(Some(&ea.hash), &b, &opts).unwrap().expect("不同帧应有输出");
        assert_ne!(ea.hash, eb.hash);
        assert!(!eb.jpeg.is_empty());
    }

    #[test]
    fn scale_changes_encoded_dimensions_not_hash() {
        let opts = EncodeOptions {
            quality: 80,
            scale: 0.5,
        };
        let frame = solid(1, 40, 20, [1, 2, 3, 255]);
        let encoded = encode_frame(None, &frame, &opts).unwrap().unwrap();
        assert_eq!((encoded.width, encoded.height), (20, 10), "缩放后尺寸应为一半");
        // 哈希基于原始 RGBA，与是否缩放无关。
        assert_eq!(encoded.hash, frame_hash(&frame.rgba));
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
}
