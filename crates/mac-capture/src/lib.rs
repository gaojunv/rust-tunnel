//! macOS 远程控制采集层（ScreenCaptureKit）。
//!
//! 本 crate 只做**采集与像素处理**，不依赖 `rust-tunnel-remote`（与 `win-video` 的
//! 分层一致）：`client-gui/src/remote/mac_capture.rs` 负责把这里的结果适配成
//! `ScreenSource`。
//!
//! # 数据通路
//!
//! `SCStream`（ScreenCaptureKit，macOS 14+）在**专用线程**上创建与释放，画面按
//! `kCVPixelFormatType_32BGRA` 送进 [`SCStreamOutput`] 回调；回调只做 `CFRetain`
//! 与一次覆盖式入槽（不做像素拷贝，4K 一帧 33 MB 的拷贝会阻塞 SCK 的派发队列）。
//! 之后按消费方式分流：GPU 直通原样交出缓冲；补丁路径只按脏矩形读取源区间；只有
//! CPU 整帧路径才按 `bytesPerRow` 拷成紧包 BGRA。
//!
//! 支持四类消费方式：
//!
//! - [`MacCapture::capture`]：整帧 RGBA（按 `max_encode_width` 在采集层一步封顶）；
//! - [`MacCapture::capture_gpu`]：**零拷贝**路径——ScreenCaptureKit 按 `max_encode_width`
//!   直接输出编码尺寸的 `CVPixelBuffer`（GPU 内缩放），编码器原样直通 VideoToolbox；
//! - [`MacCapture::capture_patches`]：脏矩形补丁（目标编码坐标系的紧包 NV12；
//!   只按脏矩形从源缓冲读取，不做整帧拷贝）；
//! - [`MacCapture::cursor_shape`]：远端光标**形状**（位置仍由控制端本机系统光标渲染）。
//!
//! # 为什么变化检测在采集层
//!
//! ScreenCaptureKit 在静止桌面上仍按合成节奏产帧（与 Windows WGC 相同），「有新帧」
//! 不等于「画面变了」。采集层用 `mac-video` 的 [`ChangeDetector`]（320×180 亮度网格）
//! 做像素级判定，`changed == false` 的拍由引擎**完全跳过编码/发送**；同一份网格再喂给
//! 脏矩形提取，低活跃度时改发补丁。
//!
//! # 编译面
//!
//! ScreenCaptureKit / AppKit 实现全部在 `cfg(target_os = "macos")` 下；[`geometry`]
//! 与错误类型在所有平台编译，因此 Windows 开发机可以直接
//! `cargo test -p rust-tunnel-mac-capture --lib`（本 crate 在 Windows 上是一个可编译的壳）。
//!
//! [`ChangeDetector`]: rust_tunnel_mac_video::change::ChangeDetector
//! [`SCStreamOutput`]: https://developer.apple.com/documentation/screencapturekit/scstreamoutput

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::float_cmp
    )
)]
#![cfg_attr(
    not(target_os = "macos"),
    allow(
        unused_crate_dependencies,
        reason = "macOS-only crate：非 macOS 平台没有 objc2 依赖的使用点"
    )
)]

pub mod error;
pub mod geometry;

#[cfg(target_os = "macos")]
mod sck;
#[cfg(not(target_os = "macos"))]
mod stub;

#[cfg(target_os = "macos")]
pub use sck::MacCapture;
#[cfg(not(target_os = "macos"))]
pub use stub::MacCapture;

pub use error::{MacCaptureError, MacCaptureResult};

/// GPU 采集帧的像素缓冲句柄（macOS 上是引用计数的 `CVPixelBuffer`）。
///
/// 非 macOS 用 [`std::convert::Infallible`]：占位实现永远不会产出 GPU 帧，
/// 采集调用恒返回 [`MacCaptureError::Unsupported`]。这样 [`CapturedGpuFrame`] 只需一份
/// 定义，适配层也不必按平台分叉类型。
#[cfg(target_os = "macos")]
pub type GpuFrameBuffer = objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>;
/// 非 macOS 的 GPU 帧句柄（不可能存在）。
#[cfg(not(target_os = "macos"))]
pub type GpuFrameBuffer = std::convert::Infallible;

/// 一帧 **GPU 直通**采集结果：ScreenCaptureKit 输出的 `CVPixelBuffer`，不落 CPU。
///
/// SCK 的 `SCStreamConfiguration.width/height` 在**建流时**就按 [`MacCapture::set_max_encode_width`]
/// 的宽度上限配置（GPU 内缩放，几乎零成本），因此 `source_width/source_height` 通常已等于
/// `width/height`：编码器拿到同尺寸的 IOSurface 后**零拷贝直通**，不再每帧做一次
/// 4K→1080p 的 `VTPixelTransferSession`（真机实测 ~3 ms/帧）。两者仍分开表达：
/// `width/height` 是逻辑编码尺寸，`source_*` 是缓冲实际尺寸（显示器物理尺寸小于上限时
/// 二者相等；上限变化会触发采集流重建）。
///
/// `changed`/`changed_permille` 由采集层如实填写：SCK 样本附件报告「零脏矩形」时直接
/// 采信（这是平台给的权威信号）；否则把帧缩到 320×180 后做像素级比较，首帧/检测不可用
/// 时保守给 `true` + `1000`。
pub struct CapturedGpuFrame {
    /// 来源显示器编号。
    pub display: u32,
    /// 逻辑编码宽度（像素，偶数）。
    pub width: u32,
    /// 逻辑编码高度（像素，偶数）。
    pub height: u32,
    /// 缓冲的实际宽度（像素；已按编码宽度上限封顶）。
    pub source_width: u32,
    /// 缓冲的实际高度（像素；已按编码宽度上限封顶）。
    pub source_height: u32,
    /// 采集输出的像素缓冲（引用计数，不拷贝像素）。
    pub buffer: GpuFrameBuffer,
    /// 采集层是否判定画面**有可见变化**。
    pub changed: bool,
    /// 变化面积（千分比，`0..=1000`）；`changed == false` 时恒为 `0`。
    pub changed_permille: u16,
}

/// 显示器元信息（与 `remote::screen::DisplayInfo` 形状一致，由适配器逐字段搬运）。
///
/// `id` 是 CoreGraphics 的 `CGDirectDisplayID`：采集（`SCDisplay.displayID`）与
/// 输入注入（`CGEvent` 的显示器坐标）共用同一个编号体系。
///
/// **`width`/`height` 是 macOS 全局逻辑点尺寸（points），不是物理像素**：
/// 控制端用它们把窗口坐标映射成远端坐标，而 enigo / `CGEvent` 的绝对坐标单位就是
/// 点（4K 屏是 1920×1080 点，物理面板是 3840×2160 像素）。采集与编码仍使用
/// [`display_physical_size`] 的物理像素，两者不能混用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    /// 平台显示器编号（`CGDirectDisplayID`）。
    pub id: u32,
    /// 宽度（**逻辑点**，不是物理像素）。
    pub width: u32,
    /// 高度（逻辑点）。
    pub height: u32,
    /// 显示器名称（SCK 不提供名称，这里是 `显示器 <id>`）。
    pub name: String,
    /// 是否为主显示器（`CGDisplayIsMain`）。
    pub primary: bool,
}

/// 显示器的**物理像素**尺寸（采集/编码用；与 [`DisplayInfo`] 的逻辑点尺寸不同）。
///
/// 供探针/排障同时打印两套尺寸；采集流本身由 `MacCapture` 内部使用同一函数。
#[cfg(target_os = "macos")]
#[must_use]
pub fn display_physical_size(display_id: u32) -> Option<(u32, u32)> {
    sck::physical_size(display_id)
}

/// 枚举当前显示器（**不经过采集实例**，CoreGraphics 同步枚举，亚毫秒级）。
///
/// 引擎每 5 秒在会话循环里内联调用一次 `displays()`：这里必须足够快，绝不能用
/// ScreenCaptureKit 的异步内容枚举（真机 70–80 ms，会把输入注入与采集节拍冻住）。
/// `DisplayInfo.width/height` 是 **macOS 全局逻辑点**，控制端输入映射用。
///
/// # Errors
/// CoreGraphics 枚举失败或没有任何活动显示器时返回错误（**不检查**「屏幕录制」权限：
/// 枚举本身不需要它，缺权限由采集路径报错）。
#[cfg(target_os = "macos")]
pub fn displays() -> MacCaptureResult<Vec<DisplayInfo>> {
    sck::list_displays()
}

/// 枚举当前显示器（非 macOS 恒为空）。
///
/// # Errors
/// 占位实现不会失败。
#[cfg(not(target_os = "macos"))]
pub fn displays() -> MacCaptureResult<Vec<DisplayInfo>> {
    Ok(Vec::new())
}

/// 一帧采集结果（未编码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedFrame {
    /// 来源显示器编号。
    pub display: u32,
    /// 宽度（已按 `max_encode_width` 封顶后的**编码尺寸**）。
    pub width: u32,
    /// 高度（编码尺寸）。
    pub height: u32,
    /// 紧包 RGBA8，长度恒为 `width * height * 4`。
    pub rgba: Vec<u8>,
    /// 采集层是否判定画面**有可见变化**。
    ///
    /// `false` 表示这一拍复用了上一帧（SCK 没有新帧）或与上一帧逐格相同：引擎据此
    /// **完全跳过编码与发送**。首帧/检测不可用时恒为 `true`（宁可多编一帧）。
    pub changed: bool,
    /// 变化面积（千分比，`0..=1000`）。`changed == false` 时恒为 `0`。
    pub changed_permille: u16,
}

/// 一个脏矩形补丁（目标**编码尺寸**坐标系的紧包 NV12）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchRect {
    /// 左上角 X（偶数）。
    pub x: u32,
    /// 左上角 Y（偶数）。
    pub y: u32,
    /// 宽度（偶数）。
    pub width: u32,
    /// 高度（偶数）。
    pub height: u32,
    /// 紧包 NV12（`Y(width*height) ‖ UV(width*height/2)`，BT.709 limited）。
    pub nv12: Vec<u8>,
}

/// 一次补丁采集的结果。
///
/// `width`/`height` 是**目标编码尺寸**（与控制端持久帧缓冲一致），不是采集源的物理尺寸。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedPatches {
    /// 来源显示器编号。
    pub display: u32,
    /// 目标帧缓冲宽度（编码尺寸，偶数）。
    pub width: u32,
    /// 目标帧缓冲高度（编码尺寸，偶数）。
    pub height: u32,
    /// 本拍的变化面积（千分比，`0..=1000`）；空补丁（保活）时为 `0`。
    pub changed_permille: u16,
    /// 脏矩形；空表示「本拍没有变化」，仅用于保活。
    pub rects: Vec<PatchRect>,
}

/// 远端光标形状。
///
/// 变体顺序与名称**逐一对应** `remote::screen::CursorShape`（适配器据此做一次穷举
/// `match`，漏一个变体会编译失败）。位置不上线：完全控制模式下控制端本机指针就是
/// 远端指针，本枚举只让控制端把**图标**跟着换。读不到时返回 `None`；读到但不在
/// 已知表里用 [`CursorShape::Unknown`]（控制端显示默认箭头）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// 采集源自报的平台信息（进会话统计角标，`CaptureInfo` 同口径）。
///
/// `fallbacks` / `no_new_frame` / `open_fallbacks` 都是**进程内累计值**，引擎保存上一份
/// 快照求差后才是每秒增量。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureInfo {
    /// 采集后端（本 crate 恒为 `"ScreenCaptureKit"`）。
    pub backend: String,
    /// 采集适配器（CPU 采集没有显卡名，恒为空串）。
    pub adapter: String,
    /// **最近一帧**是否真的走了 `CVPixelBuffer` 直通：GPU 编解码会话的 `capture_gpu`
    /// 路径为 `true`，CPU `capture()` 与补丁路径为 `false`。
    pub gpu_capture: bool,
    /// GPU 路径是否已被永久禁用（本 crate 没有「永久禁用」语义：GPU 失败只回退单帧，
    /// 恒 `false`）。
    pub gpu_disabled: bool,
    /// GPU 被禁用的原因（未禁用时为空串）。
    pub gpu_disabled_reason: String,
    /// 硬件采集失败、回退到 CPU 采集的累计次数（本 crate 无回退后端，恒 0）。
    pub fallbacks: u64,
    /// 采集后端报告「没有新帧」（超时复用了上一帧）的累计次数。
    pub no_new_frame: u64,
    /// 打开采集源失败、回退到另一个后端的累计次数（本 crate 无回退后端，恒 0）。
    pub open_fallbacks: u64,
}
