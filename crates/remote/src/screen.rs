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
//! 线程里完成，[`FrameSink`] 拿到的永远是**已解码的整帧**（VP9 为 RGBA；Windows 的 H.264 硬件
//! 路径可以给紧包 NV12，见 [`DecodedFrame::nv12`]），UI 线程只负责上传纹理 / 必要的 CPU 转换。
//! 静止画面由上层 memcmp 去重后**完全不喂编码器**，这是带宽趋零的来源。
//!
//! 画质档位见 [`QualityPreset`]，帧率上限见 [`EncodeOptions::max_fps`]。

use crate::error::{RemoteError, RemoteResult};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

/// 一帧**不落 CPU** 的平台 GPU 画面。
///
/// `remote` 保持平台无关：这里只有类型擦除（[`std::any::Any`]），Windows 的 D3D11
/// 纹理由 `client-gui` 自己定义并实现本 trait，编码器适配器再用 `downcast_ref`
/// 取回。**不要**往这个 trait 里加平台类型或像素方法。
pub trait GpuFrame: Send + Sync {
    /// 向下转型入口（平台实现直接返回 `self`）。
    fn as_any(&self) -> &dyn std::any::Any;
}

/// 一帧原始画面（未编码）。
///
/// 两种互斥的载体：
/// - **CPU**：`rgba` 是 `width * height * 4` 的 RGBA8，`gpu` 为 `None`；
/// - **GPU**：`gpu` 为 `Some`（平台纹理），此时 `rgba` 为空。`width`/`height` 仍是
///   **逻辑编码尺寸**（已按 [`crate::video::MAX_ENCODE_WIDTH`] 封顶），可能与纹理的
///   物理像素尺寸不同（4K 采集、1920 逻辑宽）。
#[derive(Clone)]
pub struct RawFrame {
    /// 来源显示器编号。
    pub display: u32,
    /// 宽度（像素）。
    pub width: u32,
    /// 高度（像素）。
    pub height: u32,
    /// RGBA8 像素数据，长度必须为 `width * height * 4`；GPU 帧为空。
    pub rgba: Vec<u8>,
    /// GPU 帧（不落 CPU）；`Some` 时 `rgba` 为空。
    pub gpu: Option<Arc<dyn GpuFrame>>,
    /// 采集层是否判定画面**有可见变化**（默认 `true`）。
    ///
    /// GPU 路径由平台采集源填：Windows 的 WGC/DXGI 后端会在拿到新帧后做一次
    /// **像素级**检测（VideoProcessor 缩到 320×180 + staging 回读比较，4K 约 0.36 ms），
    /// 内容确实没变时给 `false`，编码循环据此跳过这一拍——这是「静止桌面不编码」的
    /// 来源（只看「有没有新合成帧」不够：WGC 在静止桌面上仍按刷新节奏产帧）。
    /// 检测器不可用时保守给 `true`。CPU 路径恒为 `true`，仍靠逐字节 memcmp 去重。
    /// 任何 `force`（首帧/切显示器/切档位/关键帧请求/几何变化）都会绕过它。
    pub changed: bool,
    /// 变化面积（千分比，`0..=1000`），采集层与 [`Self::changed`] 一起给出。
    ///
    /// `changed == false` 时恒为 0。`changed == true && changed_permille == 0` 表示
    /// 「变了但面积未知」——编码侧按**满帧**处理，保守地不压制码率/帧率。CPU 路径
    /// （没有便宜的像素级变化信号）恒填 `1000`。
    ///
    /// 它只用于**被控端本地**的码率/帧率自适应（`qos::RateController::observe_activity`），
    /// 不进线上协议：`QosFeedback` 的字段一个都没变。
    pub changed_permille: u16,
}

impl std::fmt::Debug for RawFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawFrame")
            .field("display", &self.display)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba_len", &self.rgba.len())
            .field("gpu", &self.gpu.is_some())
            .field("changed", &self.changed)
            .field("changed_permille", &self.changed_permille)
            .finish()
    }
}

impl RawFrame {
    /// 校验帧是否自洽。
    ///
    /// GPU 帧只要求宽高非零（像素数据不在 CPU 上，无法也不该校验长度）；CPU 帧保持
    /// 「`rgba.len() == width * height * 4`」这条原有约束。
    ///
    /// # Errors
    /// 尺寸为零、溢出，或 CPU 帧长度不匹配时返回 [`RemoteError::Encode`]。
    pub fn validate(&self) -> RemoteResult<()> {
        if self.gpu.is_some() {
            if self.width == 0 || self.height == 0 {
                return Err(RemoteError::Encode(format!(
                    "GPU 帧尺寸非法：{}x{}",
                    self.width, self.height
                )));
            }
            return Ok(());
        }
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

/// 解码后的一帧紧包 NV12（4:2:0）。
///
/// `y` 长度 `width * height`、`uv` 长度 `width * height / 2`，没有行填充。控制器侧的
/// H.264 解码器直接产出它，UI 可以选择在 CPU 上转 RGBA 或交给 GPU 上传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedNv12 {
    /// Y 平面。
    pub y: Vec<u8>,
    /// 交织 UV 平面（`U0 V0 U1 V1 ...`）。
    pub uv: Vec<u8>,
}

impl DecodedNv12 {
    /// 把一个脏矩形补丁**原地拼进**整帧缓冲。
    ///
    /// `frame_width` 是整帧宽度（偶数）；整帧高度由 `y.len() / frame_width` 推出。
    ///
    /// 坐标系与 [`ScreenPatch`] 一致：`x`/`y`/`width`/`height` 全部偶数，`rect.nv12` 是
    /// 紧包 NV12（`Y(w*h) ‖ UV(w*h/2)`）。UV 平面在整帧里的行宽等于 `frame_width`
    /// 字节（每个 luma 像素对占 2 字节），因此补丁的 UV 行偏移就是 `x`（`x` 为偶数，
    /// 恰好是色度对的字节偏移）。
    ///
    /// 返回 `false`（且不修改任何字节）表示参数非法：越界、奇数坐标、长度不符……调用方
    /// 应当丢弃这一帧并等下一个关键帧，而不是把错位的内容拼进去。
    #[must_use]
    pub fn apply_patch(&mut self, frame_width: u32, rect: &ScreenPatch) -> bool {
        let Some(geometry) = self.patch_geometry(frame_width, rect) else {
            return false;
        };
        let PatchGeometry {
            frame_width,
            rect_width,
            rect_height,
            x,
            y,
            uv_base,
        } = geometry;

        // Y 平面：`rect_height` 行，每行 `rect_width` 字节。
        for row in 0..rect_height {
            let src = row * rect_width;
            let dst = (y + row) * frame_width + x;
            let (Some(src_slice), Some(dst_slice)) = (
                rect.nv12.get(src..src + rect_width),
                self.y.get_mut(dst..dst + rect_width),
            ) else {
                return false;
            };
            dst_slice.copy_from_slice(src_slice);
        }
        // UV 平面：半高度，每行仍是 `rect_width` 字节（交错的 U/V 对）。
        for row in 0..(rect_height / 2) {
            let src = uv_base + row * rect_width;
            let dst = (y / 2 + row) * frame_width + x;
            let (Some(src_slice), Some(dst_slice)) = (
                rect.nv12.get(src..src + rect_width),
                self.uv.get_mut(dst..dst + rect_width),
            ) else {
                return false;
            };
            dst_slice.copy_from_slice(src_slice);
        }
        true
    }

    /// 只**校验**、不写任何字节：该补丁能否安全拼进本帧。
    ///
    /// 判据与 [`Self::apply_patch`] 的前置条件**一字不差**（两者共用 [`Self::patch_geometry`]），
    /// 供补丁批次层实现「先逐块全验、再原地全写」——这样不需要克隆整帧做回滚副本。
    #[must_use]
    pub fn validate_patch(&self, frame_width: u32, rect: &ScreenPatch) -> bool {
        self.patch_geometry(frame_width, rect).is_some()
    }

    /// 把一批补丁**原子地**拼进整帧：先逐块校验，全部合法才原地写；任一非法则一字节都不改。
    ///
    /// [`Self::apply_patch`] 只保证**单个**矩形原子（第二个失败时第一个已经写进去了），批次层
    /// 若直接逐个调用就会留下「半截应用」——画面缺一块且无法回滚。这里先用
    /// [`Self::validate_patch`]（与 `apply_patch` 同一套前置条件、不写字节）全验，再原地写。
    #[must_use]
    pub fn apply_patches(&mut self, frame_width: u32, rects: &[ScreenPatch]) -> bool {
        if !rects.iter().all(|rect| self.validate_patch(frame_width, rect)) {
            return false;
        }
        for rect in rects {
            // 校验与 `apply_patch` 共用 `patch_geometry`，故这里不可能失败。
            let applied = self.apply_patch(frame_width, rect);
            debug_assert!(applied, "validate_patch 通过后 apply_patch 不应失败");
            if !applied {
                return false;
            }
        }
        true
    }

    /// 校验并解出拼写所需的几何量（全部已转成 `usize`，且已验证不越界）；非法时 `None`。
    fn patch_geometry(&self, frame_width: u32, rect: &ScreenPatch) -> Option<PatchGeometry> {
        let frame_width = usize::try_from(frame_width).ok()?;
        if frame_width == 0 {
            return None;
        }
        let rect_width = usize::try_from(rect.width).ok()?;
        let rect_height = usize::try_from(rect.height).ok()?;
        let x = usize::try_from(rect.x).ok()?;
        let y = usize::try_from(rect.y).ok()?;
        // 偶数与整数长度校验：奇数宽高会让 UV 半分辨率错位。
        if rect_width == 0
            || rect_height == 0
            || !rect_width.is_multiple_of(2)
            || !rect_height.is_multiple_of(2)
            || !x.is_multiple_of(2)
            || !y.is_multiple_of(2)
        {
            return None;
        }
        let frame_height = self.y.len().checked_div(frame_width)?;
        if frame_height == 0 || !frame_height.is_multiple_of(2) {
            return None;
        }
        if self.y.len() != frame_width.saturating_mul(frame_height) {
            return None;
        }
        if self.uv.len() != frame_width.saturating_mul(frame_height / 2) {
            return None;
        }
        let right = x.checked_add(rect_width)?;
        let bottom = y.checked_add(rect_height)?;
        if right > frame_width || bottom > frame_height {
            return None;
        }
        let expected = rect_width
            .checked_mul(rect_height)?
            .checked_mul(3)?
            .checked_div(2)?;
        if rect.nv12.len() != expected {
            return None;
        }
        Some(PatchGeometry {
            frame_width,
            rect_width,
            rect_height,
            x,
            y,
            uv_base: rect_width * rect_height,
        })
    }
}

/// [`DecodedNv12::patch_geometry`] 解出的拼写几何量（全部已完成校验）。
struct PatchGeometry {
    /// 整帧宽度（字节，等于 luma 像素数）。
    frame_width: usize,
    /// 矩形宽度（luma 像素数 = Y 平面每行字节数）。
    rect_width: usize,
    /// 矩形高度（luma 像素行数）。
    rect_height: usize,
    /// 矩形左上角 X（偶数）。
    x: usize,
    /// 矩形左上角 Y（偶数）。
    y: usize,
    /// 补丁数据里 UV 平面相对 Y 平面的字节偏移（`rect_width * rect_height`）。
    uv_base: usize,
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
    /// RGBA8 像素数据；NV12 模式下为空。
    pub rgba: Vec<u8>,
    /// NV12 模式下的紧包 4:2:0 画面；VP9 / RGBA 模式下为 `None`。
    pub nv12: Option<DecodedNv12>,
    /// 该帧**采集完成**的时刻（被控端单调时钟，微秒；`0` = 未知）。
    ///
    /// 控制器 UI 绘制时用 `rust_tunnel_remote::clock::elapsed_ms(now_us(), offset, 它)` 算
    /// 「采集 → 上屏」的端到端延迟（A9/D2）。引擎自己不解码这一刻的时钟。
    pub capture_ts_us: u64,
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

/// 远端光标形状。
///
/// **判别式顺序就是线上契约**（[`crate::session::SessionMessage::CursorShape`] 的字段，
/// bincode 按声明顺序编码），新值一律追加在末尾（机械锁见
/// `cursor_shape_discriminants_are_stable`）。
///
/// 这是「形状枚举」而不是位图：第一阶段只覆盖 Windows 的系统光标（`IDC_*`），
/// 平台读不到时用 [`CursorShape::Unknown`]（控制端显示默认箭头），远端隐藏光标时用
/// [`CursorShape::Hidden`]（控制端同样保持默认箭头）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShape {
    /// 标准箭头。
    Arrow,
    /// 文本插入（I 形）。
    IBeam,
    /// 手型（链接）。
    Hand,
    /// 十字准星。
    Crosshair,
    /// 忙（沙漏/转圈）。
    Wait,
    /// 后台忙（箭头 + 沙漏/转圈）。
    AppStarting,
    /// 帮助（箭头 + 问号）。
    Help,
    /// 上下调整。
    SizeNS,
    /// 左右调整。
    SizeWE,
    /// 左上-右下调整。
    SizeNWSE,
    /// 右上-左下调整。
    SizeNESW,
    /// 四向移动。
    SizeAll,
    /// 禁止（圆圈斜杠）。
    No,
    /// 向上箭头。
    UpArrow,
    /// 平台读到了光标但形状不在已知表里（控制端显示默认箭头）。
    Unknown,
    /// 远端**隐藏**了光标（控制端也保持默认箭头）。
    Hidden,
}

/// 一次键盘事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    /// 完整键位码（由上层约定，见 `crates/client-gui/src/remote/mod.rs` 的 `keycode`）。
    ///
    /// v5 起可打印键（字母/数字/标点）也使用它：被控端把键位码映射成 Windows VK /
    /// macOS CGKeyCode / Linux keysym 注入，最终字符由**被控端**的键盘布局与输入法决定。
    pub code: u32,
    /// 是否按下（`false` 为抬起）。
    pub pressed: bool,
    /// 可打印文本。**遗留字段**：只有旧控制器会填（`code` 为 `TEXT` 时按 Unicode 码点
    /// 注入，绕过被控端的布局与输入法）；新控制器不填，被控端保留解码以兼容旧控制器。
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

/// 一个脏矩形补丁（**编码尺寸坐标系**的紧包 NV12）。
///
/// 布局与 [`DecodedNv12`] 相同：`nv12` = `Y(width*height) ‖ UV(width*height/2)`，无行填充。
/// `x`/`y`/`width`/`height` **全部为偶数**：NV12 的 UV 平面是半分辨率，奇数边界会让
/// 色度块错位。
///
/// 同一份类型在两段链路上复用：
/// - 被控端采集层产出的**原始**补丁（未经压缩）；
/// - 控制端解码任务解压后的补丁（交给 [`FrameSink::on_patch`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenPatch {
    /// 目标帧缓冲内的左上角 X（偶数）。
    pub x: u32,
    /// 目标帧缓冲内的左上角 Y（偶数）。
    pub y: u32,
    /// 宽度（偶数）。
    pub width: u32,
    /// 高度（偶数）。
    pub height: u32,
    /// 紧包 NV12（长度 = `width*height*3/2`）。
    pub nv12: Vec<u8>,
}

/// 被控端一次补丁采集的结果。
///
/// `width`/`height` 是**目标编码尺寸**（控制端持久帧缓冲的尺寸），不是采集源的物理尺寸；
/// 采集源负责把脏矩形缩放/转换到该坐标系。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedPatches {
    /// 来源显示器编号。
    pub display: u32,
    /// 目标帧缓冲宽度（编码尺寸）。
    pub width: u32,
    /// 目标帧缓冲高度（编码尺寸）。
    pub height: u32,
    /// 本拍的变化面积（千分比，`0..=1000`）；空补丁（保活）时为 0。
    pub changed_permille: u16,
    /// 脏矩形；空表示「本拍没有变化」，仅用于保活。
    pub rects: Vec<ScreenPatch>,
}

/// 控制端收到的一帧补丁（已从线上格式解压为 NV12）。
///
/// 与 [`CapturedPatches`] 分开是因为控制端不需要 `changed_permille`，且两者的生命周期与
/// 来源不同（一个来自平台采集，一个来自解码任务）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPatch {
    /// 来源显示器编号。
    pub display: u32,
    /// 目标帧缓冲宽度（编码尺寸）。
    pub width: u32,
    /// 目标帧缓冲高度（编码尺寸）。
    pub height: u32,
    /// 脏矩形（已解压）。
    pub rects: Vec<ScreenPatch>,
    /// 该批补丁**采集完成**的时刻（被控端单调时钟，微秒；`0` = 未知）。语义同
    /// [`DecodedFrame::capture_ts_us`]。
    pub capture_ts_us: u64,
}

/// 采集源的**平台信息**（被控端自报，A8/B2 用）。
///
/// 引擎把它塞进每秒钟的 [`crate::session::HostStats`] 上报给控制器，也留在本端 UI 上。
/// 默认实现（[`ScreenSource::capture_info`]）返回全空/全 `false`：平台实现不填也不会说谎，
/// UI 只需显示「—」。
///
/// `fallbacks` / `no_new_frame` / `open_fallbacks` 是**进程内累计值**（不是每秒增量）：
/// 引擎保存上一份快照求差后，才变成 `HostStats` 里的每秒增量。做成累计值是因为采集源自己
/// 只在事件发生点 `+1`，不需要为它引入每秒定时器。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureInfo {
    /// 采集后端（`"WGC"` / `"DXGI"` / `"xcap"` / `"BitBlt"`；空串 = 未知）。
    pub backend: String,
    /// 采集适配器名（GPU 采集时的显卡名；CPU 采集为空）。
    pub adapter: String,
    /// **最近一帧**是否真的走了 GPU 纹理路径（零拷贝）。
    ///
    /// 不是「GPU 能力可用」：VP9 会话/未协商 H.264 时引擎走 CPU 回读，能力可用但没有
    /// 零拷贝，此时必须为 `false`。平台实现据此如实上报，角标才能区分 GPU/CPU。
    pub gpu_capture: bool,
    /// GPU 路径是否已被**永久禁用**（任何一环失败后不再尝试）。
    pub gpu_disabled: bool,
    /// GPU 被禁用的原因（未禁用时为空串）。
    pub gpu_disabled_reason: String,
    /// 硬件采集失败、回退到 CPU 采集的累计次数（`WGC/DXGI → xcap`）。
    pub fallbacks: u64,
    /// 采集后端报告「没有新帧」（超时/无新合成帧）的累计次数。
    pub no_new_frame: u64,
    /// 打开采集源失败、回退到另一个后端的累计次数（如 `WGC 打开失败 → DXGI`）。
    pub open_fallbacks: u64,
}

/// 编码器的**平台信息**（被控端自报，B2 用）。
///
/// 与 [`CaptureInfo`] 一样进每秒的 [`crate::session::HostStats`]。默认实现（
/// [`crate::codec::VideoEncoderApi::info`]）全空，平台实现按实际填。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncoderInfo {
    /// 编码器后端（`"NVENC"` / `"Intel QSV"` / `"AMD VCE"` / `"MFT"` / `"libvpx"`）。
    pub backend: String,
    /// 编码器名称（例如 `"NVIDIA NVENC H.264"`、`"VP9 4:4:4"`）。
    pub name: String,
    /// 编码器所在适配器名（显卡名；软件编码为空）。
    pub adapter: String,
    /// 编码器是否直接吃 GPU 纹理（零拷贝直通，不落 CPU）。
    pub gpu_input: bool,
}

/// 解码器的**后端信息**（控制器自报，A3 用）。
///
/// 默认实现（[`crate::codec::VideoDecoderApi::info`]）全空/`false`。控制器在解码器创建/尺寸
/// 配置成功后读一次，只在**变化时**经 [`crate::stats::SessionStats::DecoderInfo`] 报给 UI。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecoderInfo {
    /// 解码后端（`"MF DXVA"` / `"MF Software"` / `"libvpx"`）。
    pub backend: String,
    /// 解码器名称（例如 `"MSH264DecoderMFT"`）。
    pub name: String,
    /// 解码适配器名（硬件解码时的显卡名；软件解码为空）。
    pub adapter: String,
    /// 是否硬件解码。
    pub hardware: bool,
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

    /// 平台**采集/编码像素尺寸**（可选；默认 `None` = 与 [`DisplayInfo`] 相同）。
    ///
    /// [`DisplayInfo`] 是**控制端输入映射坐标系**：macOS 上报的是全局逻辑点（4K 屏为
    /// 1920×1080 点），而采集/编码像素是它的整数倍（3840×2160）。原生码率倍率
    /// （[`crate::screen::bitrate_permille_for_pixels`]）与补丁目标帧缓冲必须按**采集
    /// 像素**算，否则 4K 原生会话会拿到 1080p 的码率、补丁尺寸也会与全帧对不上。
    ///
    /// Windows 等平台的 `DisplayInfo` 本来就是物理像素，返回 `None` 即可。
    fn encode_pixels(&self, display: u32) -> Option<(u32, u32)> {
        let _ = display;
        None
    }

    /// 采集指定显示器的一帧。
    ///
    /// # Errors
    /// 采集失败（显示器不存在、平台权限不足等）时返回 [`RemoteError::Platform`]。
    fn capture(&self, display: u32) -> RemoteResult<RawFrame>;

    /// 设置本会话的**编码宽度上限**（引擎在每次会话开始、codec 协商完成后调用一次）。
    ///
    /// 平台实现据此决定采集侧预缩放的上限：Windows 硬件路径会把 4K 采集一步缩到该
    /// 宽度（见 `WinVideoScreen`），避免整帧 33 MB 的中间缓冲。默认实现**忽略**它，
    /// 即始终使用 [`crate::video::MAX_ENCODE_WIDTH`]（1920）——实例/安全桌面那条
    /// GDI/BitBlt 路径就属于这种情况，不做 4K。
    ///
    /// 引擎传入的值只会是 [`crate::video::MAX_ENCODE_WIDTH`] 或
    /// [`crate::video::NATIVE_MAX_ENCODE_WIDTH`]（后者仅硬件 H.264 的显式选项）。
    fn set_max_encode_width(&self, _width: u32) {}

    /// 本采集源是否支持**不落 CPU** 的 GPU 采集（默认 `false`）。
    ///
    /// 引擎只在协商到 H.264 且本方法为真时才走 [`ScreenSource::capture_gpu`]；
    /// 其余情况一律 [`ScreenSource::capture`]（CPU 路径，永远可用）。
    fn supports_gpu_capture(&self) -> bool {
        false
    }

    /// 采集一帧 GPU 画面（默认回退到 CPU 采集）。
    ///
    /// 实现返回的 [`RawFrame::gpu`] 必须为 `Some`，且宽高是**逻辑编码尺寸**（平台
    /// 实现负责先按封顶宽度换算）；失败时可以自行回退 CPU，也可以返回错误让引擎
    /// 走其它兜底。
    ///
    /// # Errors
    /// GPU 采集失败且实现没有自行回退时返回 [`RemoteError::Platform`]。
    fn capture_gpu(&self, display: u32) -> RemoteResult<RawFrame> {
        self.capture(display)
    }

    /// 本采集源**现在**是否已能为本次会话改用 GPU 采集（默认 `false`）。
    ///
    /// # 与会话开始的 [`ScreenSource::supports_gpu_capture`] 的区别
    ///
    /// `supports_gpu_capture` 只在会话建立时调用**一次**，实现可以借机重置会话级状态
    /// （例如 `WinVideoScreen` 会把「编码器能否吃 GPU 纹理」的 hint 重置为乐观值）。
    /// **本方法由会话循环逐拍轮询，必须无副作用**：不得重置任何能力标志、不得打开
    /// 设备、不得做阻塞调用。默认实现恒 `false` 表示「不支持会话中途升级」。
    ///
    /// # 引擎怎么用它
    ///
    /// 引擎只在当前 `gpu_capture == false`、codec 是 H.264/H.265 时调用它，且**只做
    /// false → true 的升级**（绝不反向关闭已打开的 GPU 采集）：升级后下一拍起改走
    /// [`ScreenSource::capture_gpu`]。
    ///
    /// # 典型场景
    ///
    /// Windows 无人值守实例在登录界面（`Winlogon` 安全桌面）建立会话时只有 GDI
    /// `BitBlt`；用户登录回到 `Default` 桌面后本方法转为 `true`，**同一场会话**不必
    /// 重连就能切到 WGC/DXGI + GPU 纹理（编码器由编码任务在首张 GPU 帧上按新采集设备
    /// 重建一次）。
    fn gpu_capture_ready(&self) -> bool {
        false
    }

    /// 本采集源是否支持**脏矩形补丁**（默认 `false`）。
    ///
    /// 引擎只在协商到 H.264、且本方法为真、且画面活跃度足够低时才进入补丁模式。VP9 路径
    /// 不启用：它的解码输出是 RGBA，控制端需要另一套持久帧缓冲。
    fn supports_patches(&self) -> bool {
        false
    }

    /// 采集一帧并产出**脏矩形补丁**（编码尺寸坐标系，紧包 NV12）。
    ///
    /// 返回 `Ok(None)` 表示「本拍不适合补丁」（活跃度过高、脏矩形过多/过大、变化检测不可用、
    /// 首帧……）：引擎随后回到全帧视频路径。**不要在实现里自行降级成全帧**——引擎需要知道
    /// 模式已经切换，才能强制关键帧。
    ///
    /// `scale` / `max_width` 与 [`crate::video::encode_dimensions`] 的语义一致：实现先按采集
    /// 纹理的**实际**物理尺寸算出目标编码尺寸，再把脏矩形缩放/转换到该坐标系。这样显示器
    /// 分辨率在会话中途变化时也不会用陈旧尺寸。
    ///
    /// # Errors
    /// 采集/回读/转换失败时返回 [`RemoteError::Platform`]；引擎会记日志并退回全帧。
    fn capture_patches(
        &self,
        display: u32,
        scale: f32,
        max_width: u32,
    ) -> RemoteResult<Option<CapturedPatches>> {
        let _ = (display, scale, max_width);
        Ok(None)
    }

    /// 本采集源的平台信息（被控端自报，进 [`crate::session::HostStats`]）。
    ///
    /// 默认实现返回 [`CaptureInfo::default`]（全空/全 `false`）：不实现它只是让对端的统计角标
    /// 少几行，会话本身不受影响。**实现可以在会话中途变化**（GPU 被禁用、后端回退），引擎每秒
    /// 读一次。
    fn capture_info(&self) -> CaptureInfo {
        CaptureInfo::default()
    }

    /// 远端当前的光标形状（被控端自报，随采集结果每拍采样）。
    ///
    /// 返回 `None` 表示**平台不支持或读不到**（第一阶段只有 Windows 实现；macOS/Linux 走默认
    /// 实现 → `None`）：引擎不会发送任何消息，控制端保持默认箭头。已读到但不在已知表里的形状
    /// 用 [`CursorShape::Unknown`]；远端隐藏光标时用 [`CursorShape::Hidden`]（控制端同样保持
    /// 默认箭头）。
    ///
    /// 它只走**进程内**通道（`CaptureOutcome::cursor`），真正上线的是
    /// [`crate::session::SessionMessage::CursorShape`]。
    fn cursor_shape(&self) -> Option<CursorShape> {
        None
    }
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

    /// 锁定本机屏幕（被控端）。默认实现返回「平台不支持」。
    ///
    /// 引擎在**完全控制**模式下收到控制器的 [`crate::session::SessionMessage::LockScreen`]
    /// 时调用它，也在会话收尾（用户偏好开启时）自动调用一次。平台实现见
    /// `crates/client-gui/src/remote/mod.rs` 的 `lock_platform_screen`。
    ///
    /// # Errors
    /// 平台不支持或系统调用失败时返回 [`RemoteError::Platform`]。
    fn lock_screen(&self) -> RemoteResult<()> {
        Err(RemoteError::Platform("本平台不支持锁定屏幕".to_owned()))
    }

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
/// **调用发生在引擎的运行时线程**：`on_frame` 收到的已是解码后的整帧（RGBA，或 H.264 硬件
/// 路径下的紧包 NV12），UI 线程只需上传纹理或做 CPU 转换。
pub trait FrameSink: Send + Sync + 'static {
    /// 显示器列表（会话建立时下发一次，变更时再次下发）。
    fn on_displays(&self, displays: Vec<DisplayInfo>);

    /// 一帧已解码画面（VP9 时域编码下**一律是整帧**）。
    fn on_frame(&self, frame: DecodedFrame);

    /// 一批脏矩形补丁（H.264 补丁模式；已解压为 NV12）。
    ///
    /// **必须应用在上一次 [`FrameSink::on_frame`] 的全帧之上**：引擎保证两者严格同序
    /// （都走同一条解码队列），且补丁不可丢（队列满时会请求关键帧、由被控端退出补丁模式）。
    /// 默认实现为空操作——不实现它只意味着不做补丁上屏，会话本身仍然正常。
    fn on_patch(&self, patch: DecodedPatch) {
        let _ = patch;
    }

    /// 对端（被控端）发来的剪贴板文本。
    ///
    /// 控制器侧不实现 [`Clipboard`]：它读写的本来就是**本机**剪贴板，UI 直接用自己的实现即可。
    /// 默认实现为空操作——忽略它只意味着不做剪贴板同步，不影响会话。
    fn on_clipboard(&self, text: String) {
        let _ = text;
    }

    /// 远端光标形状（被控端 → 控制器；形状变化时下发）。
    ///
    /// 默认实现为空操作——忽略它只意味着控制端不跟随远端光标，会话本身不受影响。
    fn on_cursor(&self, shape: CursorShape) {
        let _ = shape;
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

/// 原生分辨率（硬件 H.264 4K）会话的绝对码率上限（kbps）。
///
/// 档位码率按像素数放大后再封顶到这里：4K 是 1080p 的 4 倍像素，同档位的「每像素比特」
/// 需要 4 倍码率才对等。放大后均衡/高清档分别是 24 / 40 Mbps。**只约束被控端的编码
/// 上限**，QoS 闭环仍可在其下下调（内容上限也按面积同步放大）。
pub const NATIVE_MAX_BITRATE_KBPS: u32 = 60_000;

/// 原生分辨率相对 1080p 的码率倍数上限（像素数之比：4K = 1080p 的 4 倍）。
pub const NATIVE_BITRATE_FACTOR: u32 = 4;

/// 1080p 的像素数：码率按面积放大的基准。
pub const FULLHD_PIXELS: u64 = 1920 * 1080;

/// 显示器像素数 → 码率倍数（千分比，钳在 `[1000, 4000]`）。
///
/// 1080p 显示器的会话拿不到任何放大（原生与否都没有额外像素）；2.5K 约 1778‰；
/// 4K 及以上封顶 4000‰。用**实际显示器**而不是「开关是否打开」算，避免 1080p
/// 显示器在开了原生开关后凭空拿到 4 倍码率上限。
#[must_use]
pub fn bitrate_permille_for_pixels(pixels: u64) -> u32 {
    let permille = pixels.saturating_mul(1000) / FULLHD_PIXELS;
    let permille = permille.clamp(1000, u64::from(NATIVE_BITRATE_FACTOR) * 1000);
    u32::try_from(permille).unwrap_or(1000)
}

/// 按面积倍数放大档位码率上限，并封顶到 [`NATIVE_MAX_BITRATE_KBPS`]。
///
/// `permille == 1000` = 原样返回（1080p / 非原生会话）。
#[must_use]
pub const fn scale_bitrate(preset_kbps: u32, permille: u32) -> u32 {
    let scaled = preset_kbps.saturating_mul(permille) / 1000;
    if scaled > NATIVE_MAX_BITRATE_KBPS {
        NATIVE_MAX_BITRATE_KBPS
    } else {
        scaled
    }
}

/// 原生分辨率会话把档位码率上限按**最大**像素倍数放大（封顶
/// [`NATIVE_MAX_BITRATE_KBPS`]），帧率不变。
///
/// 只有 `native_resolution == true` **且**协商到硬件 H.264 时才会用到；真正下发时
/// 用 [`bitrate_permille_for_pixels`] 按当前显示器算，1080p 显示器不会拿到 4 倍。
#[must_use]
pub const fn native_bitrate_ceiling(preset_kbps: u32) -> u32 {
    scale_bitrate(preset_kbps, NATIVE_BITRATE_FACTOR * 1000)
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
///
/// **原生分辨率会话**（`ControlledConfig::native_resolution` 且协商到硬件 H.264）：封顶宽度换成
/// [`crate::video::NATIVE_MAX_ENCODE_WIDTH`]（3840），`target_bitrate_kbps` 经
/// [`native_bitrate_ceiling`] 翻倍。VP9 与默认会话不变。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeOptions {
    /// 目标码率**上限**（kbps）。VP9 CBR 的目标值；实际值由闭环在 `[BR_MIN, 本值]` 内浮动。
    /// 秒级以下的收敛由 [`crate::video`] 的 `rc_buffer_size_ms`（交互级 300 ms）保证。
    pub target_bitrate_kbps: u32,
    /// 缩放比例**上限**（1.0 = 原始尺寸）。编码尺寸还会被 [`crate::video::MAX_ENCODE_WIDTH`] 封顶；
    /// 闭环可在必要时进一步下调（降编码分辨率）。
    ///
    /// **平台采集源可以预先封顶**：Windows 的 WGC/DXGI 采集在交给引擎前就把源缩到
    /// [`crate::video::MAX_ENCODE_WIDTH`]（避免 4K 的 33 MB 中间缓冲），此时「原始尺寸」
    /// 指封顶后的尺寸——「流畅」档的 0.5 会在 1920 宽的源上得到 960，而不是 4K 源上
    /// 先乘 0.5 再被 1920 封顶（后者仍会得到 1920，等于该档降分辨率在 4K 上失效）。
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
    /// 流畅：半分辨率 + 2 Mbps + 20 fps（上限）。窄带/跨地域中继场景。
    Smooth,
    /// 均衡：原始分辨率（≤ [`crate::video::MAX_ENCODE_WIDTH`]）+ 6 Mbps + 30 fps（上限）。默认档。
    #[default]
    Balanced,
    /// 高清：原始分辨率（≤ 封顶宽度）+ 10 Mbps + 60 fps（上限）。同内网直连场景。
    Hd,
}

impl QualityPreset {
    /// 映射到编码参数**上限**。
    #[must_use]
    pub fn options(self) -> EncodeOptions {
        match self {
            Self::Smooth => EncodeOptions {
                target_bitrate_kbps: 2000,
                scale: 0.5,
                max_fps: 20,
            },
            Self::Balanced => EncodeOptions {
                target_bitrate_kbps: 6000,
                scale: 1.0,
                max_fps: 30,
            },
            Self::Hd => EncodeOptions {
                target_bitrate_kbps: 10_000,
                scale: 1.0,
                max_fps: 60,
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

    /// 原生分辨率的码率规则：档位按像素数放大（4×），封顶 60 Mbps，帧率不动。
    ///
    /// 钉住两条：① 默认三档放大后仍在封顶之下（2000→8000 / 6000→24000 / 10000→40000）；
    /// ② 任何输入都不越过 [`NATIVE_MAX_BITRATE_KBPS`]（饱和乘法 + 封顶，不溢出）。
    #[test]
    fn native_bitrate_ceiling_scales_with_pixels_and_caps() {
        assert_eq!(native_bitrate_ceiling(2000), 8000);
        assert_eq!(native_bitrate_ceiling(6000), 24_000);
        assert_eq!(native_bitrate_ceiling(10_000), 40_000);
        assert_eq!(native_bitrate_ceiling(20_000), NATIVE_MAX_BITRATE_KBPS);
        assert_eq!(native_bitrate_ceiling(u32::MAX), NATIVE_MAX_BITRATE_KBPS);
        // 4K 是 1080p 的 4 倍像素：每像素比特与会话的档位标定一致。
        assert_eq!(NATIVE_BITRATE_FACTOR, 4);
        // 按显示器像素数算的倍数：1080p 不放大、2.5K 约 1.78×、4K 封顶 4×。
        assert_eq!(bitrate_permille_for_pixels(FULLHD_PIXELS), 1000);
        assert_eq!(bitrate_permille_for_pixels(2560 * 1440), 1777);
        assert_eq!(bitrate_permille_for_pixels(3840 * 2160), 4000);
        assert_eq!(bitrate_permille_for_pixels(5120 * 2880), 4000);
        assert_eq!(bitrate_permille_for_pixels(800 * 600), 1000);
        // `scale_bitrate(1000‰)` 是原样（非原生会话不受影响）。
        assert_eq!(scale_bitrate(6000, 1000), 6000);
        assert_eq!(scale_bitrate(6000, 1777), 10_662);
        assert_eq!(scale_bitrate(u32::MAX, 4000), NATIVE_MAX_BITRATE_KBPS);
        // 帧率与分辨率无关：档位原样给出（这里用现有档位钉住「只放大码率」）。
        for preset in QualityPreset::all() {
            let scaled = native_bitrate_ceiling(preset.options().target_bitrate_kbps);
            assert_eq!(
                scaled,
                preset.options().target_bitrate_kbps * NATIVE_BITRATE_FACTOR,
                "{} 档应按像素数放大",
                preset.label()
            );
        }
    }

    #[test]
    fn scaled_dimension_has_floor_of_one_pixel() {
        assert_eq!(scaled_dimension(1920, 0.5), 960);
        assert_eq!(scaled_dimension(1920, 1.0), 1920);
        // 极小值不会塌到 0（0 尺寸会让后续网格/分配除零）。
        assert_eq!(scaled_dimension(1, 0.01), 1);
    }

    /// **`CursorShape` 判别式黄金测试**：变体按声明顺序从 0 连续递增，新值只能追加在末尾。
    ///
    /// 它是 [`crate::session::SessionMessage::CursorShape`] 的字段，bincode 用 u32 小端判别式。
    /// 把任何变体插到中间都会让老端把一种形状解成另一种（例如 I 形光标被画成手型），
    /// 且没有任何版本错误可查——故单独锁一条。
    #[test]
    fn cursor_shape_discriminants_are_stable() {
        let all = [
            CursorShape::Arrow,
            CursorShape::IBeam,
            CursorShape::Hand,
            CursorShape::Crosshair,
            CursorShape::Wait,
            CursorShape::AppStarting,
            CursorShape::Help,
            CursorShape::SizeNS,
            CursorShape::SizeWE,
            CursorShape::SizeNWSE,
            CursorShape::SizeNESW,
            CursorShape::SizeAll,
            CursorShape::No,
            CursorShape::UpArrow,
            CursorShape::Unknown,
            CursorShape::Hidden,
        ];
        for (index, shape) in all.into_iter().enumerate() {
            let bytes = bincode::serialize(&shape).unwrap();
            let got = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            assert_eq!(
                got,
                u32::try_from(index).unwrap(),
                "变体 `{shape:?}` 的判别式错位（应连续递增）"
            );
        }
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

    /// 一个只用于测试的类型擦除载体。
    struct DummyGpuFrame;

    impl GpuFrame for DummyGpuFrame {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// GPU 帧只校验宽高非零（像素不在 CPU 上）；CPU 帧仍校验 RGBA 长度。
    #[test]
    fn gpu_frame_validate_only_checks_dimensions() {
        let gpu = RawFrame {
            display: 0,
            width: 1920,
            height: 1080,
            rgba: Vec::new(),
            gpu: Some(std::sync::Arc::new(DummyGpuFrame)),
            changed: true,
            changed_permille: 1000,
        };
        assert!(gpu.validate().is_ok(), "GPU 帧不应要求 CPU 像素数据");
        let zero = RawFrame {
            width: 0,
            ..gpu
        };
        assert!(zero.validate().is_err(), "GPU 帧仍要求宽高非零");

        let cpu = RawFrame {
            display: 0,
            width: 2,
            height: 2,
            // 2×2 应为 16 字节。
            rgba: vec![0; 4],
            gpu: None,
            changed: true,
            changed_permille: 1000,
        };
        assert!(cpu.validate().is_err(), "CPU 帧仍应校验 RGBA 长度");
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
            std::time::Duration::from_millis(1000 / 60)
        );
    }

    /// 补丁必须**只覆写矩形内的 Y/UV**，矩形外一个字节都不动。
    #[test]
    fn apply_patch_overwrites_only_the_rect() {
        // 8×6 的整帧：Y = 48 字节，UV = 8×3 = 24 字节。
        let mut frame = DecodedNv12 {
            y: vec![0xAA; 48],
            uv: vec![0xBB; 24],
        };
        let patch = ScreenPatch {
            x: 2,
            y: 2,
            width: 4,
            height: 2,
            // Y 8 字节（2 行 × 4），UV 4 字节（1 行 × 4）。
            nv12: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        };
        assert!(frame.apply_patch(8, &patch));

        // Y：第 2 行从 x=2 开始 4 字节，第 3 行同理。
        assert_eq!(&frame.y[2 * 8 + 2..2 * 8 + 6], &[1, 2, 3, 4]);
        assert_eq!(&frame.y[3 * 8 + 2..3 * 8 + 6], &[5, 6, 7, 8]);
        // UV：y=2 → UV 行 1，字节偏移 = x = 2。
        assert_eq!(&frame.uv[8 + 2..8 + 6], &[9, 10, 11, 12]);

        // 矩形外的字节保持原值（抽查四角与相邻行）。
        assert_eq!(frame.y[0], 0xAA);
        assert_eq!(frame.y[2 * 8], 0xAA, "矩形左侧不应被写");
        assert_eq!(frame.y[2 * 8 + 1], 0xAA, "矩形左侧不应被写");
        assert_eq!(frame.y[2 * 8 + 6], 0xAA, "矩形右侧不应被写");
        assert_eq!(frame.y[4 * 8 + 2], 0xAA, "矩形下方不应被写");
        assert_eq!(frame.uv[0], 0xBB);
        assert_eq!(frame.uv[8], 0xBB);
        assert_eq!(frame.uv[8 + 6], 0xBB);
    }

    /// 非法补丁必须被拒绝，且**不修改**帧缓冲（宁可少拼一帧，也不能拼错位）。
    #[test]
    fn apply_patch_rejects_invalid_rects_without_touching_the_frame() {
        let original = DecodedNv12 {
            y: vec![0xAA; 48],
            uv: vec![0xBB; 24],
        };
        let valid = ScreenPatch {
            x: 2,
            y: 2,
            width: 4,
            height: 2,
            nv12: vec![0; 12],
        };
        let cases = [
            // 奇数宽度。
            ScreenPatch { width: 3, ..valid.clone() },
            // 奇数坐标。
            ScreenPatch { x: 3, ..valid.clone() },
            // 越界（x + w > 8）。
            ScreenPatch { x: 6, ..valid.clone() },
            // 越界（y + h > 6）。
            ScreenPatch { y: 6, ..valid.clone() },
            // NV12 长度不符。
            ScreenPatch { nv12: vec![0; 11], ..valid.clone() },
            // 尺寸为零。
            ScreenPatch { width: 0, ..valid.clone() },
        ];
        for (index, patch) in cases.into_iter().enumerate() {
            let mut frame = original.clone();
            assert!(!frame.apply_patch(8, &patch), "第 {index} 个非法补丁应被拒绝");
            assert_eq!(frame, original, "第 {index} 个非法补丁不应修改帧");
        }
        // 宽度为奇数/零的整帧同样拒绝。
        let mut frame = original.clone();
        assert!(!frame.apply_patch(0, &valid));
        assert!(!frame.apply_patch(7, &valid));
        assert_eq!(frame, original);
    }

    /// 一批补丁要么**全部应用**，要么一字节都不改。
    ///
    /// [`DecodedNv12::apply_patch`] 只保证单个矩形原子：第二个矩形失败时第一个已经写进去了。
    /// 这种半截应用会永久留在持久合成帧里（画面缺一块且无法回滚），故批次层用
    /// [`DecodedNv12::apply_patches`] 先全验、再原地写——**不克隆整帧做回滚副本**。
    #[test]
    fn apply_patches_is_all_or_nothing() {
        // 8×6 整帧：Y 48 字节、UV 24 字节。
        let original = DecodedNv12 {
            y: vec![0xAA; 48],
            uv: vec![0xBB; 24],
        };
        let valid = ScreenPatch {
            x: 2,
            y: 2,
            width: 4,
            height: 2,
            nv12: vec![1; 12],
        };
        // 全部合法：两块都写进去。
        let mut composite = original.clone();
        let second = ScreenPatch {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
            nv12: vec![2; 6],
        };
        assert!(composite.apply_patches(8, &[valid.clone(), second]));
        assert_eq!(composite.y[2 * 8 + 2], 1, "第一块应已应用");
        assert_eq!(composite.y[0], 2, "第二块应已应用");

        // 第二块越界：第一块也不能留下。
        let mut composite = original.clone();
        let bad = ScreenPatch {
            x: 6,
            y: 0,
            width: 4,
            height: 2,
            nv12: vec![3; 12],
        };
        assert!(!composite.apply_patches(8, &[valid, bad]));
        assert_eq!(composite, original, "整批失败时不得留下半截应用");

        // 空批次是合法空操作（补丁保活）。
        let mut composite = original.clone();
        assert!(composite.apply_patches(8, &[]));
        assert_eq!(composite, original);
    }

    /// `validate_patch` 与 `apply_patch` 的判据必须完全一致：批次层靠它实现「先全验、再全写」，
    /// 若两者有分歧，就会出现「校验通过却应用失败」的半截写入。
    #[test]
    fn validate_patch_matches_apply_patch_preconditions() {
        let frame = DecodedNv12 {
            y: vec![0xAA; 48],
            uv: vec![0xBB; 24],
        };
        let valid = ScreenPatch {
            x: 2,
            y: 2,
            width: 4,
            height: 2,
            nv12: vec![0; 12],
        };
        assert!(frame.validate_patch(8, &valid));

        let cases = [
            ScreenPatch { width: 3, ..valid.clone() },
            ScreenPatch { height: 3, ..valid.clone() },
            ScreenPatch { x: 3, ..valid.clone() },
            ScreenPatch { y: 3, ..valid.clone() },
            ScreenPatch { x: 6, ..valid.clone() },
            ScreenPatch { y: 6, ..valid.clone() },
            ScreenPatch { nv12: vec![0; 11], ..valid.clone() },
            ScreenPatch { nv12: vec![0; 13], ..valid.clone() },
            ScreenPatch { width: 0, ..valid.clone() },
            ScreenPatch { height: 0, ..valid.clone() },
        ];
        for (index, patch) in cases.into_iter().enumerate() {
            assert!(!frame.validate_patch(8, &patch), "第 {index} 个非法补丁应被拒");
            // 与真正应用的结果一致（且 `apply_patch` 不改动缓冲）。
            let mut copy = frame.clone();
            assert!(!copy.apply_patch(8, &patch), "第 {index} 个非法补丁应被 apply 拒绝");
            assert_eq!(copy, frame);
        }
        // 整帧宽度非法（0 / 奇数）同样拒绝。
        assert!(!frame.validate_patch(0, &valid));
        assert!(!frame.validate_patch(7, &valid));
        // 平面长度与帧宽不自洽的缓冲：任何补丁都不该被接受。
        let ragged = DecodedNv12 {
            y: vec![0; 47],
            uv: vec![0; 24],
        };
        assert!(!ragged.validate_patch(8, &valid));
    }
}

