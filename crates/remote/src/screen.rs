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
//! 数据面自阶段 2 起走 **VP9 4:4:4 时域编码**（见 [`crate::video`]）：编解码都在引擎的运行时
//! 线程里完成，[`FrameSink`] 拿到的永远是**已解码的整帧 RGBA**（UI 线程只负责上传纹理）。
//! 静止画面由上层 memcmp 去重后**完全不喂编码器**，这是带宽趋零的来源。
//!
//! 画质档位见 [`QualityPreset`]，帧率上限见 [`EncodeOptions::max_fps`]。

use crate::error::{RemoteError, RemoteResult};
use serde::{Deserialize, Serialize};

/// 每像素字节数（RGBA）。
pub const BYTES_PER_PIXEL: usize = 4;

/// 一台显示器的元信息。
///
/// **一个字段都不能加、不能改**：它内嵌在 `SessionMessage::HelloAck.displays` 里，改形状会破坏
/// 「混合版本先解出 `Hello`/`HelloAck`、再明文干净拒绝」这条路径。所有新的每显示器元数据
/// （codec、编码尺寸）都走握手后的 `SessionMessage::ScreenConfig`。
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
/// **调用发生在引擎的运行时线程**：`on_frame` 收到的已是解码后的 RGBA，UI 线程只需上传纹理。
pub trait FrameSink: Send + Sync + 'static {
    /// 显示器列表（会话建立时下发一次，变更时再次下发）。
    fn on_displays(&self, displays: Vec<DisplayInfo>);

    /// 一帧已解码画面（VP9 时域编码下**一律是整帧**）。
    fn on_frame(&self, frame: DecodedFrame);

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

/// 屏幕编码参数——自阶段 3 起是**档位上限**。
///
/// 语义变更（阶段 3 QoS 闭环）：本结构描述的是「这个档位最多给到多少」，**不是**实际生效值。
/// 实际生效值由被控端的 AIMD（[`crate::qos::RateController`]）在上限之下浮动——弱网时降码率 /
/// 帧率 / 分辨率，链路恢复后逐步回升。要拿「当前生效值」用 [`EncodeRates`]。
///
/// 由此产生的一条硬约束：**上限字段的变化路径与生效值不同**。几何（`scale` → 编码尺寸）变化
/// 需要重建编码器 + 强制 IDR；而 `target_bitrate_kbps` / `max_fps` 由闭环**在线重配置**，不重建、
/// 不插 IDR（见 `vendor/shiguredo_libvpx` 的补丁 6）。把码率放进「重建键」会让闭环每秒插一个关键帧。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeOptions {
    /// 目标码率**上限**（kbps）。VP9 CBR 的目标值；实际值由闭环在 `[BR_MIN, 本值]` 内浮动。
    /// 秒级以下的收敛由 [`crate::video`] 的 `rc_buffer_size_ms`（交互级 300 ms）保证。
    pub target_bitrate_kbps: u32,
    /// 缩放比例**上限**（1.0 = 原始尺寸）。编码尺寸还会被 [`crate::video::MAX_ENCODE_WIDTH`] 封顶；
    /// 闭环可在必要时进一步下调（降编码分辨率）。
    pub scale: f32,
    /// 目标帧率**上限**（被控端据此定采集间隔，也写进编码器的 `fps`）。
    pub max_fps: u8,
}

/// 闭环**当前生效**的编码参数（各字段均 ≤ 对应的 [`EncodeOptions`] 上限）。
///
/// 由 [`crate::qos::RateController`] 产出，随每个编码任务交给编码流水线。与 [`EncodeOptions`]
/// 分开，是为了让「上限」与「生效值」在类型上不可混用——把上限当生效值用会悄悄绕过闭环。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeRates {
    /// 当前生效目标码率（kbps）。
    pub bitrate_kbps: u32,
    /// 当前生效帧率。
    pub fps: u8,
    /// 当前生效缩放比例（越小分辨率越低）。
    pub scale: f32,
}

impl EncodeRates {
    /// 取某档位上限对应的初始生效值（闭环起步点 = 上限）。
    #[must_use]
    pub fn ceiling(options: EncodeOptions) -> Self {
        Self {
            bitrate_kbps: options.target_bitrate_kbps,
            fps: options.max_fps,
            scale: options.scale,
        }
    }

    /// 取该生效值的采集间隔（`fps` 为 0 时按 1 fps 兜底，避免除零）。
    #[must_use]
    pub fn capture_interval(&self) -> std::time::Duration {
        let fps = self.fps.max(1);
        std::time::Duration::from_millis(1000 / u64::from(fps))
    }
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
///
/// 自阶段 3 起档位是**上限**而非固定参数：切换档位把闭环的封顶值改成该档位，实际生效值仍由
/// AIMD 在上限之下浮动（弱网时可能低于档位标称值）。UI 选择器因此显示的是「最高给到多少」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum QualityPreset {
    /// 流畅：半分辨率 + 低码率 + 8 fps（上限）。窄带/跨地域中继场景。
    Smooth,
    /// 均衡：原始分辨率（≤ [`crate::video::MAX_ENCODE_WIDTH`]）+ 中码率 + 12 fps（上限）。默认档。
    #[default]
    Balanced,
    /// 高清：原始分辨率（≤ 封顶宽度）+ 高码率 + 20 fps（上限）。同内网直连场景。
    Hd,
}

impl QualityPreset {
    /// 映射到编码参数**上限**。
    #[must_use]
    pub fn options(self) -> EncodeOptions {
        match self {
            Self::Smooth => EncodeOptions {
                target_bitrate_kbps: 1200,
                scale: 0.5,
                max_fps: 8,
            },
            Self::Balanced => EncodeOptions {
                target_bitrate_kbps: 4000,
                scale: 1.0,
                max_fps: 12,
            },
            Self::Hd => EncodeOptions {
                target_bitrate_kbps: 8000,
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

    #[test]
    fn scaled_dimension_has_floor_of_one_pixel() {
        assert_eq!(scaled_dimension(1920, 0.5), 960);
        assert_eq!(scaled_dimension(1920, 1.0), 1920);
        // 极小值不会塌到 0（0 尺寸会让后续网格/分配除零）。
        assert_eq!(scaled_dimension(1, 0.01), 1);
    }

    #[test]
    fn presets_are_ordered_by_fidelity_and_cost() {
        let smooth = QualityPreset::Smooth.options();
        let balanced = QualityPreset::Balanced.options();
        let hd = QualityPreset::Hd.options();
        // 码率、帧率单调递增；流畅档降分辨率。
        assert!(
            smooth.target_bitrate_kbps < balanced.target_bitrate_kbps
                && balanced.target_bitrate_kbps < hd.target_bitrate_kbps
        );
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

