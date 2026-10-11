//! 远程控制的 GUI 侧胶水。
//!
//! # 职责
//!
//! - **平台分派**：[`macos`] / [`windows`] / [`linux`] 只放各平台差异（权限探测、系统设置
//!   跳转、平台名）。截屏（`xcap`）与输入注入（`enigo`）本身已由各自 crate 内部处理平台差异，
//!   故实现为三平台通用代码，放在本模块（[`XcapScreen`] / [`EnigoInput`]）。
//! - **帧 → UI 通道**：[`GuiFrameSink`] 在引擎的运行时线程被调用，只把**已解码**帧塞进共享槽
//!   并 `request_repaint`，绝不在该线程做解码/缩放。
//! - **会话视图状态**：[`RemoteView`] 持有 egui 纹理与输入捕获的边沿状态（上一鼠标位置、
//!   按键/修饰键状态）。
//! - **数据面建立**：[`LazyTransport`] 延迟构造 [`RustDeskTransport`]——`RemoteParams` 要等
//!   控制通道下发 `RemoteCapability` 才拿到，而 [`rust_tunnel_remote::RemoteManager`] 在构造时
//!   就要 `Arc<dyn TransportEstablisher>`。

use std::sync::{Arc, Mutex, PoisonError};

// enigo 的 `move_mouse` / `button` / `scroll` / `key` 由这两个 trait 提供，必须引入。
use enigo::{Keyboard as _, Mouse as _};
use rust_tunnel_remote::{
    DecodedFrame, DisplayInfo, FrameSink, InputEvent, InputSink, KeyEvent, MouseButton, RawFrame, RemoteError,
    RemoteManager, RemoteResult, ScreenSource, TileUpdate,
};

// ── 平台模块分派 ─────────────────────────────────────────────────────
//
// 每个平台模块提供：`NAME` / `permissions()` / `open_settings()` / `screen_source()` /
// `input_sink()`。截屏与注入的实体在三个平台完全一致（xcap/enigo 内部消化差异），
// 故仅权限与设置跳转按平台分派。
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod unsupported;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
use unsupported as platform;

/// 本端平台标识（下发给服务端，用于可远程列表展示与对端提示）。
#[must_use]
pub fn platform_name() -> &'static str {
    platform::NAME
}

/// 平台权限状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// 该平台是否需要/支持权限探测。
    pub supported: bool,
    /// 是否已获得屏幕录制/采集权限。
    pub screen_capture: bool,
    /// 是否已获得辅助功能（输入注入）权限。
    pub accessibility: bool,
}

impl Permissions {
    /// 是否全部就绪（可直接进入被控）。
    #[must_use]
    pub fn all_granted(self) -> bool {
        self.screen_capture && self.accessibility
    }
}

/// 需要用户到系统设置里授予的权限种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    /// 屏幕录制/采集。
    ScreenCapture,
    /// 辅助功能（输入注入）。
    Accessibility,
}

/// 探测当前平台权限（进入远程 Tab / 被控前调用）。
#[must_use]
pub fn permissions() -> Permissions {
    platform::permissions()
}

/// 打开系统隐私设置对应面板（供 UI「去授权」按钮调用）。
pub fn open_permission_settings(kind: PermissionKind) {
    platform::open_settings(kind);
}

/// 安装平台实现到引擎（截屏 + 输入注入 + 剪贴板）。
pub fn install_platform(manager: &Arc<RemoteManager>) {
    manager.install_platform(platform::screen_source(), platform::input_sink());
    manager.install_clipboard(Arc::new(SystemClipboard));
}

/// 控制器下载文件的落地目录（系统「下载」目录；取不到时回落到临时目录）。
#[must_use]
pub fn download_dir() -> std::path::PathBuf {
    directories::UserDirs::new()
        .and_then(|d| d.download_dir().map(std::path::Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
}

/// 被控端对外共享的目录：`<下载目录>/rust-tunnel-share`。
///
/// 放在「下载」而不是用户主目录下，是为了让被控端用户一眼能看到别人能读到什么、也能随时
/// 自己清空；反过来，主目录会让「共享」这件事变得难以察觉。
///
/// 本端始终提供共享目录，故直接返回路径；由调用方包成
/// [`rust_tunnel_remote::RemoteManagerConfig::share_dir`] 需要的 `Some(..)`。
#[must_use]
pub fn share_dir() -> std::path::PathBuf {
    download_dir().join("rust-tunnel-share")
}

/// 基于 `arboard` 的文本剪贴板（被控端用它读写本机剪贴板）。
///
/// `arboard` 的 `Clipboard` 不是 `Sync`，且每次读写都要新建（Linux 下它要独占 X11 selection），
/// 因此这里**不缓存实例**，每次操作现开现关。
#[derive(Debug, Default)]
pub struct SystemClipboard;

impl rust_tunnel_remote::Clipboard for SystemClipboard {
    fn get(&self) -> RemoteResult<String> {
        arboard::Clipboard::new()
            .and_then(|mut cb| cb.get_text())
            .map_err(|err| RemoteError::Platform(format!("读取剪贴板失败: {err}")))
    }

    fn set(&self, text: &str) -> RemoteResult<()> {
        arboard::Clipboard::new()
            .and_then(|mut cb| cb.set_text(text.to_owned()))
            .map_err(|err| RemoteError::Platform(format!("写入剪贴板失败: {err}")))
    }
}

// ── 跨平台键码约定 ───────────────────────────────────────────────────
//
// 控制器侧（egui 捕获）与被控端（enigo 注入）必须共用同一套 `code` 语义，因为
// `rust_tunnel_remote::KeyEvent.code` 只约定「由上层约定」。此处定义：
// - `TEXT`：可打印文本，键值由 `KeyEvent.text` 承载；
// - 其余为特殊键/修饰键的稳定编号。

/// 跨平台键码（控制器编码、被控端解码；见模块注释）。
pub mod keycode {
    /// 可打印文本（键值在 `KeyEvent.text` 里）。
    pub const TEXT: u32 = 0;
    /// 回车。
    pub const ENTER: u32 = 1;
    /// Tab。
    pub const TAB: u32 = 2;
    /// 退格。
    pub const BACKSPACE: u32 = 3;
    /// Esc。
    pub const ESCAPE: u32 = 4;
    /// Delete。
    pub const DELETE: u32 = 5;
    /// 左方向键。
    pub const ARROW_LEFT: u32 = 6;
    /// 右方向键。
    pub const ARROW_RIGHT: u32 = 7;
    /// 上方向键。
    pub const ARROW_UP: u32 = 8;
    /// 下方向键。
    pub const ARROW_DOWN: u32 = 9;
    /// Home。
    pub const HOME: u32 = 10;
    /// End。
    pub const END: u32 = 11;
    /// PageUp。
    pub const PAGE_UP: u32 = 12;
    /// PageDown。
    pub const PAGE_DOWN: u32 = 13;
    /// 空格。
    pub const SPACE: u32 = 14;
    /// F1（F1..=F12 为连续区间）。
    pub const F1: u32 = 20;
    /// Ctrl。
    pub const CTRL: u32 = 40;
    /// Shift。
    pub const SHIFT: u32 = 41;
    /// Alt / Option。
    pub const ALT: u32 = 42;
    /// Meta / Cmd / Win。
    pub const META: u32 = 43;
}

/// 修饰键位掩码（与 `KeyEvent.modifiers` 对应）。
pub mod modifier {
    /// Alt / Option。
    pub const ALT: u8 = 1 << 0;
    /// Ctrl。
    pub const CTRL: u8 = 1 << 1;
    /// Shift。
    pub const SHIFT: u8 = 1 << 2;
    /// Meta / Cmd / Win。
    pub const META: u8 = 1 << 3;
}

/// 把 egui 修饰键状态编码为位掩码。
#[must_use]
pub fn egui_modifiers(m: egui::Modifiers) -> u8 {
    let mut bits = 0_u8;
    if m.alt {
        bits |= modifier::ALT;
    }
    if m.ctrl {
        bits |= modifier::CTRL;
    }
    if m.shift {
        bits |= modifier::SHIFT;
    }
    if m.command {
        bits |= modifier::META;
    }
    bits
}

/// egui 逻辑键 → 跨平台键码；非特殊键（可打印字符）返回 `None`（走 `TEXT` 路径）。
#[must_use]
pub fn egui_key_to_code(key: egui::Key) -> Option<u32> {
    use egui::Key;
    Some(match key {
        Key::Enter => keycode::ENTER,
        Key::Tab => keycode::TAB,
        Key::Backspace => keycode::BACKSPACE,
        Key::Escape => keycode::ESCAPE,
        Key::Delete => keycode::DELETE,
        Key::ArrowLeft => keycode::ARROW_LEFT,
        Key::ArrowRight => keycode::ARROW_RIGHT,
        Key::ArrowUp => keycode::ARROW_UP,
        Key::ArrowDown => keycode::ARROW_DOWN,
        Key::Home => keycode::HOME,
        Key::End => keycode::END,
        Key::PageUp => keycode::PAGE_UP,
        Key::PageDown => keycode::PAGE_DOWN,
        Key::Space => keycode::SPACE,
        Key::F1 => keycode::F1,
        Key::F2 => keycode::F1 + 1,
        Key::F3 => keycode::F1 + 2,
        Key::F4 => keycode::F1 + 3,
        Key::F5 => keycode::F1 + 4,
        Key::F6 => keycode::F1 + 5,
        Key::F7 => keycode::F1 + 6,
        Key::F8 => keycode::F1 + 7,
        Key::F9 => keycode::F1 + 8,
        Key::F10 => keycode::F1 + 9,
        Key::F11 => keycode::F1 + 10,
        Key::F12 => keycode::F1 + 11,
        _ => return None,
    })
}

/// F1..=F12 对应的 enigo 键（索引 = `code - keycode::F1`）。
const F_KEYS: [enigo::Key; 12] = [
    enigo::Key::F1,
    enigo::Key::F2,
    enigo::Key::F3,
    enigo::Key::F4,
    enigo::Key::F5,
    enigo::Key::F6,
    enigo::Key::F7,
    enigo::Key::F8,
    enigo::Key::F9,
    enigo::Key::F10,
    enigo::Key::F11,
    enigo::Key::F12,
];

/// 跨平台键码 → enigo 键；未知码返回 `None`（静默忽略）。
#[must_use]
pub fn code_to_enigo(code: u32) -> Option<enigo::Key> {
    use enigo::Key;
    Some(match code {
        keycode::ENTER => Key::Return,
        keycode::TAB => Key::Tab,
        keycode::BACKSPACE => Key::Backspace,
        keycode::ESCAPE => Key::Escape,
        keycode::DELETE => Key::Delete,
        keycode::ARROW_LEFT => Key::LeftArrow,
        keycode::ARROW_RIGHT => Key::RightArrow,
        keycode::ARROW_UP => Key::UpArrow,
        keycode::ARROW_DOWN => Key::DownArrow,
        keycode::HOME => Key::Home,
        keycode::END => Key::End,
        keycode::PAGE_UP => Key::PageUp,
        keycode::PAGE_DOWN => Key::PageDown,
        keycode::SPACE => Key::Space,
        keycode::CTRL => Key::Control,
        keycode::SHIFT => Key::Shift,
        keycode::ALT => Key::Alt,
        keycode::META => Key::Meta,
        _ => {
            let offset = code.checked_sub(keycode::F1)?;
            *F_KEYS.get(usize::try_from(offset).ok()?)?
        }
    })
}

// ── 屏幕采集（xcap，三平台通用）─────────────────────────────────────

/// 基于 `xcap` 的屏幕采集源。
///
/// 注意：macOS 需「屏幕录制」权限，缺失时 [`xcap::Monitor::all`] 可能返回空或
/// [`ScreenSource::capture`] 报错；UI 侧应先经 [`permissions`] 提示。
pub struct XcapScreen;

/// 把 xcap 显示器转成引擎的 [`DisplayInfo`]（字段读取失败时给保守回退值）。
fn monitor_info(monitor: &xcap::Monitor, fallback_id: u32) -> DisplayInfo {
    DisplayInfo {
        id: monitor.id().unwrap_or(fallback_id),
        width: monitor.width().unwrap_or(0),
        height: monitor.height().unwrap_or(0),
        name: monitor.name().unwrap_or_else(|_| format!("显示器 {fallback_id}")),
        primary: monitor.is_primary().unwrap_or(false),
    }
}

impl ScreenSource for XcapScreen {
    fn displays(&self) -> Vec<DisplayInfo> {
        xcap::Monitor::all().map_or_else(
            |_| Vec::new(),
            |monitors| {
                monitors
                    .iter()
                    .enumerate()
                    .map(|(idx, monitor)| monitor_info(monitor, u32::try_from(idx).unwrap_or(0)))
                    .collect()
            },
        )
    }

    fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
        let monitors =
            xcap::Monitor::all().map_err(|e| RemoteError::Platform(format!("枚举显示器失败: {e}")))?;
        let monitor = monitors
            .iter()
            .find(|m| m.id().ok() == Some(display))
            .or_else(|| monitors.first())
            .ok_or_else(|| RemoteError::Platform("无可用显示器".to_owned()))?;
        let image = monitor
            .capture_image()
            .map_err(|e| RemoteError::Platform(format!("截屏失败: {e}")))?;
        Ok(RawFrame {
            display,
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        })
    }
}

// ── 输入注入（enigo，三平台通用）────────────────────────────────────

/// 把 enigo 注入错误转成引擎错误（作为 `map_err` 的函数指针按值接收）。
#[expect(
    clippy::needless_pass_by_value,
    reason = "直接作为 map_err 的函数指针使用，按值接收更贴合签名"
)]
fn input_err(err: enigo::InputError) -> RemoteError {
    RemoteError::Platform(format!("输入注入失败: {err}"))
}

/// 基于 `enigo` 的输入注入目标。
///
/// `enigo::Enigo` 的所有方法都要 `&mut self`，故内部用 [`Mutex`] 包裹。构造时若权限不足
/// （如 macOS 未授予「辅助功能」），注入方法返回 [`RemoteError::Platform`]。
pub struct EnigoInput {
    /// 注入器；初始化失败时为 `None`（使用时给出明确错误）。
    inner: Mutex<Option<enigo::Enigo>>,
    /// 显示器编号 → 虚拟桌面原点缓存（`(id, x, y)`）。
    origins: Mutex<Vec<(u32, i32, i32)>>,
}

impl EnigoInput {
    /// 构造注入器（不弹系统授权框；权限由 UI 显式引导）。
    #[must_use]
    pub fn new() -> Self {
        let settings = enigo::Settings {
            open_prompt_to_get_permissions: false,
            ..enigo::Settings::default()
        };
        Self {
            inner: Mutex::new(enigo::Enigo::new(&settings).ok()),
            origins: Mutex::new(Vec::new()),
        }
    }

    /// 在注入器上执行一段操作；未初始化时返回明确错误。
    fn with_enigo<R>(&self, f: impl FnOnce(&mut enigo::Enigo) -> RemoteResult<R>) -> RemoteResult<R> {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let enigo = guard.as_mut().ok_or_else(|| {
            RemoteError::Platform("输入注入不可用（请授予辅助功能权限）".to_owned())
        })?;
        f(enigo)
    }

    /// 取指定显示器在虚拟桌面中的原点（缓存未命中时刷新一次）。
    fn origin_for(&self, display: u32) -> (i32, i32) {
        let mut origins = self.origins.lock().unwrap_or_else(PoisonError::into_inner);
        if !origins.iter().any(|(id, _, _)| *id == display) {
            *origins = query_origins();
        }
        origins
            .iter()
            .find(|(id, _, _)| *id == display)
            .map_or((0, 0), |(_, x, y)| (*x, *y))
    }
}

impl Default for EnigoInput {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSink for EnigoInput {
    fn mouse_move(&self, x: i32, y: i32, display: u32) -> RemoteResult<()> {
        let (origin_x, origin_y) = self.origin_for(display);
        let (global_x, global_y) = (origin_x.saturating_add(x), origin_y.saturating_add(y));
        self.with_enigo(|enigo| {
            enigo
                .move_mouse(global_x, global_y, enigo::Coordinate::Abs)
                .map_err(input_err)
        })
    }

    fn mouse_button(&self, button: MouseButton, pressed: bool) -> RemoteResult<()> {
        let button = match button {
            MouseButton::Left => enigo::Button::Left,
            MouseButton::Right => enigo::Button::Right,
            MouseButton::Middle => enigo::Button::Middle,
        };
        let direction = if pressed {
            enigo::Direction::Press
        } else {
            enigo::Direction::Release
        };
        self.with_enigo(|enigo| enigo.button(button, direction).map_err(input_err))
    }

    fn wheel(&self, dx: i32, dy: i32) -> RemoteResult<()> {
        self.with_enigo(|enigo| {
            if dy != 0 {
                enigo.scroll(dy, enigo::Axis::Vertical).map_err(input_err)?;
            }
            if dx != 0 {
                enigo.scroll(dx, enigo::Axis::Horizontal).map_err(input_err)?;
            }
            Ok(())
        })
    }

    fn key(&self, event: KeyEvent) -> RemoteResult<()> {
        // 特殊键/修饰键：按按下/抬起方向注入（有配对事件）。
        if event.code != keycode::TEXT {
            if let Some(key) = code_to_enigo(event.code) {
                let direction = if event.pressed {
                    enigo::Direction::Press
                } else {
                    enigo::Direction::Release
                };
                return self.with_enigo(|enigo| enigo.key(key, direction).map_err(input_err));
            }
        }
        // 文本：egui 只发一次文本事件（无配对抬起），故用 Click（按下+抬起）。
        if let Some(text) = event.text.as_deref() {
            for ch in text.chars() {
                self.with_enigo(|enigo| enigo.key(enigo::Key::Unicode(ch), enigo::Direction::Click).map_err(input_err))?;
            }
        }
        Ok(())
    }
}

/// 查询所有显示器在虚拟桌面中的原点。
fn query_origins() -> Vec<(u32, i32, i32)> {
    xcap::Monitor::all().map_or_else(
        |_| Vec::new(),
        |monitors| {
            monitors
                .iter()
                .filter_map(|m| Some((m.id().ok()?, m.x().ok()?, m.y().ok()?)))
                .collect()
        },
    )
}

// ── 帧 → UI 通道 ────────────────────────────────────────────────────

/// 引擎侧 [`FrameSink`] 与 UI 共享的画面槽。
#[derive(Default)]
pub struct FrameState {
    /// 最新一帧**整帧**（首帧/关键帧）。UI 取走即清；未消费时被覆盖，只保留最新。
    pub latest: Option<DecodedFrame>,
    /// 最新一次增量瓦片更新。UI 取走即清；被整帧（关键帧）作废（差分相对旧帧，不可再用）。
    pub tiles: Option<TileUpdate>,
    /// 显示器列表（会话建立时下发一次）。
    pub displays: Vec<DisplayInfo>,
    /// 会话关闭原因（UI 取走即清）。
    pub closed: Option<String>,
    /// 最近收到的对端剪贴板文本（UI 取走即清）。
    pub clipboard_in: Option<String>,
}

/// [`FrameSink`] 的 egui 实现：把帧塞进共享槽并唤醒 UI 重绘。
///
/// **在引擎的运行时线程被调用**，故只做加锁写入 + `request_repaint`，不做任何解码/缩放。
pub struct GuiFrameSink {
    /// 与 [`RemoteView`] 共享的画面槽。
    shared: Arc<Mutex<FrameState>>,
    /// egui 上下文（唤醒重绘）。
    ctx: egui::Context,
}

impl FrameSink for GuiFrameSink {
    fn on_displays(&self, displays: Vec<DisplayInfo>) {
        if let Ok(mut guard) = self.shared.lock() {
            guard.displays = displays;
        }
        self.ctx.request_repaint();
    }

    fn on_frame(&self, frame: DecodedFrame) {
        if let Ok(mut guard) = self.shared.lock() {
            guard.latest = Some(frame);
        }
        self.ctx.request_repaint();
    }

    fn on_tiles(&self, update: TileUpdate) {
        // 瓦片已是 RGBA，本线程（引擎运行时线程）只做搬运，解码/上传留给 UI 线程。
        if let Ok(mut guard) = self.shared.lock() {
            guard.tiles = Some(update);
        }
        self.ctx.request_repaint();
    }

    fn on_clipboard(&self, text: String) {
        if let Ok(mut guard) = self.shared.lock() {
            guard.clipboard_in = Some(text);
        }
        self.ctx.request_repaint();
    }


    fn on_closed(&self, reason: String) {
        if let Ok(mut guard) = self.shared.lock() {
            guard.closed = Some(reason);
        }
        self.ctx.request_repaint();
    }
}

// ── 会话视图状态 ────────────────────────────────────────────────────

/// 会话窗口内需要跨帧留存的**本地输入态**（纯 UI 用，引擎不读取）。
///
/// egui 是即时模式：文本框内容必须自己存着，否则每帧都会被清空。放在这里而不是 egui
/// 的临时存储里，是为了随 [`RemoteView`] 一起显式管理。
#[derive(Debug, Default)]
pub struct SessionInputs {
    /// 待发送给被控端的剪贴板文本（可在文本框编辑；「读取本机」会填入）。
    pub clipboard_out: String,
    /// 最近收到的对端剪贴板文本（供展示与「复制到本机」）。
    pub clipboard_in: Option<String>,
    /// 待上传文件的**本机绝对路径**（无原生文件选择器，只能手填）。
    pub upload_local: String,
    /// 上传到被控端的目标文件名（相对当前虚拟目录）。
    pub upload_name: String,
}

/// 远程会话窗口的 GUI 侧状态（纹理、输入边沿状态）。
pub struct RemoteView {
    /// 与 [`GuiFrameSink`] 共享的画面槽。
    shared: Arc<Mutex<FrameState>>,
    /// 复用的纹理句柄（尺寸变化才重建）。
    texture: Option<egui::TextureHandle>,
    /// 当前纹理尺寸 `(宽, 高)`。
    tex_size: (u32, u32),
    /// 是否已安装 [`FrameSink`]。
    sink_installed: bool,
    /// 当前观看的显示器编号。
    selected_display: Option<u32>,
    /// 会话窗口内的本地输入态（剪贴板文本、上传路径等）。
    inputs: SessionInputs,
    /// 上一次发出的鼠标位置（去重）。
    last_mouse: Option<(i32, i32)>,
    /// 上一次的三个鼠标按键状态 `[左, 右, 中]`。
    buttons: [bool; 3],
    /// 上一次的修饰键位掩码。
    modifiers: u8,
}

impl RemoteView {
    /// 构造空视图。
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(FrameState::default())),
            texture: None,
            tex_size: (0, 0),
            sink_installed: false,
            selected_display: None,
            inputs: SessionInputs::default(),
            last_mouse: None,
            buttons: [false; 3],
            modifiers: 0,
        }
    }

    /// 管理器就绪后安装 [`FrameSink`]（幂等；需 egui 上下文以唤醒重绘）。
    pub fn ensure_sink(&mut self, ctx: &egui::Context, manager: &Arc<RemoteManager>) {
        if self.sink_installed {
            return;
        }
        manager.install_frame_sink(Arc::new(GuiFrameSink {
            shared: Arc::clone(&self.shared),
            ctx: ctx.clone(),
        }));
        self.sink_installed = true;
    }

    /// 会话结束后清空纹理与输入边沿状态。
    pub fn reset_session(&mut self) {
        self.texture = None;
        self.tex_size = (0, 0);
        self.last_mouse = None;
        self.buttons = [false; 3];
        self.modifiers = 0;
        self.selected_display = None;
    }

    /// 当前显示器列表（引擎下发）。
    #[must_use]
    pub fn displays(&self) -> Vec<DisplayInfo> {
        self.shared
            .lock()
            .map_or_else(|e| e.into_inner().displays.clone(), |guard| guard.displays.clone())
    }

    /// 当前观看的显示器编号。
    #[must_use]
    pub fn selected_display(&self) -> Option<u32> {
        self.selected_display
    }

    /// 会话窗口的本地输入态（可编辑）。
    pub fn inputs_mut(&mut self) -> &mut SessionInputs {
        &mut self.inputs
    }

    /// 取走最近收到的对端剪贴板文本（取走即清）。
    pub fn take_clipboard(&mut self) -> Option<String> {
        let mut guard = self.shared.lock().ok()?;
        guard.clipboard_in.take()
    }

    /// 切换观看的显示器：同步引擎，并作废本地纹理（新显示器尺寸不同，等下一个关键帧重建）。
    pub fn set_selected_display(&mut self, id: u32, manager: &Arc<RemoteManager>) {
        if self.selected_display != Some(id) {
            self.selected_display = Some(id);
            self.texture = None;
            self.tex_size = (0, 0);
            // 引擎会据此强制下一个关键帧（尺寸变了，差分基准失效）。
            manager.select_display(id);
        }
    }

    /// 最近一次会话关闭原因（取走即清）。
    pub fn take_closed(&mut self) -> Option<String> {
        let mut guard = self.shared.lock().ok()?;
        guard.closed.take()
    }

    /// 把最新画面（整帧或增量瓦片）落到纹理，返回 `(纹理 ID, 宽, 高)`；无新画面时返回既有纹理。
    ///
    /// 整帧（首帧/关键帧）优先：它会让任何在途瓦片失效（瓦片是相对旧帧的差分）。瓦片只在
    /// **已有纹理、尺寸一致、且属于当前显示器**时应用，否则整批丢弃——下一个关键帧会修正。
    pub fn upload(&mut self, ctx: &egui::Context) -> Option<(egui::TextureId, u32, u32)> {
        // 1) 整帧（首帧/关键帧）：重建或整幅更新纹理。
        let frame = self.shared.lock().ok().and_then(|mut guard| guard.latest.take());
        if let Some(frame) = frame {
            if let Some((pixel_w, pixel_h)) = valid_frame_dims(&frame) {
                let (width, height) = (frame.width, frame.height);
                let image = egui::ColorImage::from_rgba_unmultiplied([pixel_w, pixel_h], &frame.rgba);
                if self.texture.is_some() && self.tex_size == (width, height) {
                    if let Some(texture) = self.texture.as_mut() {
                        texture.set(image, egui::TextureOptions::LINEAR);
                    }
                } else {
                    self.texture = Some(ctx.load_texture("remote-frame", image, egui::TextureOptions::LINEAR));
                }
                self.tex_size = (width, height);
            }
            // 关键帧已重建整幅画面，任何在途瓦片都相对旧帧，作废。
            if let Ok(mut guard) = self.shared.lock() {
                guard.tiles = None;
            }
        }

        // 2) 增量瓦片：应用在**同一帧**返回的纹理上，UI 本帧即可看到更新。
        if let Some(update) = self.shared.lock().ok().and_then(|mut guard| guard.tiles.take()) {
            self.apply_tiles(&update);
        }

        self.texture
            .as_ref()
            .map(|texture| (texture.id(), self.tex_size.0, self.tex_size.1))
    }

    /// 把一次增量瓦片更新应用到当前纹理（尚无纹理/尺寸或显示器不符时整批丢弃）。
    fn apply_tiles(&mut self, update: &TileUpdate) {
        // 尚无纹理（首帧未到）或尺寸不符（切显示器/切档位后的陈旧更新）→ 丢弃，等关键帧重建。
        if (update.width, update.height) != self.tex_size {
            return;
        }
        // 不属于当前观看的显示器 → 丢弃。
        if self.selected_display.is_some_and(|display| display != update.display) {
            return;
        }
        let Some(texture) = self.texture.as_mut() else {
            return;
        };
        for tile in &update.tiles {
            let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
                usize::try_from(tile.x),
                usize::try_from(tile.y),
                usize::try_from(tile.width),
                usize::try_from(tile.height),
            ) else {
                continue;
            };
            if w == 0 || h == 0 || tile.rgba.len() != w * h * 4 {
                continue;
            }
            let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &tile.rgba);
            texture.set_partial([x, y], image, egui::TextureOptions::LINEAR);
        }
    }

    /// 捕获会话窗口内的鼠标/键盘并转成 [`InputEvent`] 投递给引擎。
    ///
    /// `rect` 为画面显示区域，`remote_w`/`remote_h` 为远程帧像素尺寸；鼠标坐标从窗口内
    /// 显示区域**反变换**到远程屏幕坐标（画面可能被等比缩放）。
    pub fn capture_input(
        &mut self,
        ui: &egui::Ui,
        manager: &Arc<RemoteManager>,
        response: &egui::Response,
        rect: egui::Rect,
        remote_w: u32,
        remote_h: u32,
    ) {
        let display = self.selected_display.unwrap_or(0);

        // 1) 鼠标移动（去重后发送）。
        if let Some(pos) = response.hover_pos() {
            let x = scale_axis(pos.x - rect.min.x, rect.width(), remote_w);
            let y = scale_axis(pos.y - rect.min.y, rect.height(), remote_h);
            if self.last_mouse != Some((x, y)) {
                self.last_mouse = Some((x, y));
                manager.send_input(InputEvent::MouseMove { x, y, display });
            }
        }

        // 2) 鼠标按键 / 滚轮 / 修饰键（按边沿变化发送）。
        let (buttons, scroll, mods) = ui.input(|i| {
            (
                [
                    i.pointer.button_down(egui::PointerButton::Primary),
                    i.pointer.button_down(egui::PointerButton::Secondary),
                    i.pointer.button_down(egui::PointerButton::Middle),
                ],
                i.raw_scroll_delta,
                egui_modifiers(i.modifiers),
            )
        });
        for (idx, down) in buttons.iter().enumerate() {
            if *down != self.buttons[idx] {
                self.buttons[idx] = *down;
                let button = match idx {
                    0 => MouseButton::Left,
                    1 => MouseButton::Right,
                    _ => MouseButton::Middle,
                };
                manager.send_input(InputEvent::MouseButton {
                    button,
                    pressed: *down,
                });
            }
        }
        if scroll.x != 0.0 || scroll.y != 0.0 {
            // egui 的滚动量为正表示内容上移，enigo 正值为向下滚动 → 取反。
            manager.send_input(InputEvent::Wheel {
                dx: -round_i32(scroll.x),
                dy: -round_i32(scroll.y),
            });
        }
        if mods != self.modifiers {
            let changed = mods ^ self.modifiers;
            for (bit, code) in [
                (modifier::ALT, keycode::ALT),
                (modifier::CTRL, keycode::CTRL),
                (modifier::SHIFT, keycode::SHIFT),
                (modifier::META, keycode::META),
            ] {
                if changed & bit != 0 {
                    let pressed = mods & bit != 0;
                    manager.send_input(InputEvent::Key(KeyEvent {
                        code,
                        pressed,
                        text: None,
                        modifiers: mods,
                    }));
                }
            }
            self.modifiers = mods;
        }

        // 3) 键盘事件（特殊键走 code；文本走 TEXT）。
        let events = ui.input(|i| i.events.clone());
        for event in events {
            match event {
                egui::Event::Key { key, pressed, .. } => {
                    if let Some(code) = egui_key_to_code(key) {
                        manager.send_input(InputEvent::Key(KeyEvent {
                            code,
                            pressed,
                            text: None,
                            modifiers: mods,
                        }));
                    }
                }
                // 控制字符（Enter/Tab 等）已由 Key 事件处理，避免重复。
                egui::Event::Text(text) if text.chars().any(|c| !c.is_control()) => {
                    manager.send_input(InputEvent::Key(KeyEvent {
                        code: keycode::TEXT,
                        pressed: true,
                        text: Some(text),
                        modifiers: mods,
                    }));
                }
                _ => {}
            }
        }
    }
}

impl Default for RemoteView {
    fn default() -> Self {
        Self::new()
    }
}

/// 校验整帧像素数据是否自洽，返回 `(宽像素, 高像素)`；不合法时 `None`。
fn valid_frame_dims(frame: &DecodedFrame) -> Option<(usize, usize)> {
    let (pixel_w, pixel_h) = (
        usize::try_from(frame.width).ok()?,
        usize::try_from(frame.height).ok()?,
    );
    let expected = pixel_w.checked_mul(pixel_h)?.checked_mul(4)?;
    (pixel_w != 0 && pixel_h != 0 && frame.rgba.len() == expected).then_some((pixel_w, pixel_h))
}

/// 把窗口内偏移按比例映射到远程像素坐标（有界，非负）。
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "屏幕像素坐标有界且非负，远小于 u32/i32 上限"
)]
fn scale_axis(offset: f32, extent: f32, max: u32) -> i32 {
    if extent <= 0.0 {
        return 0;
    }
    let fraction = (offset / extent).clamp(0.0, 1.0);
    (fraction * max as f32).round() as i32
}

/// 把滚动量取整为整数（有界小值）。
#[expect(clippy::cast_possible_truncation, reason = "滚动量为有界小整数")]
fn round_i32(value: f32) -> i32 {
    value.round() as i32
}

// ── 数据面建立（延迟构造）────────────────────────────────────────────
//
// 实现在 `rust-tunnel-remote` 的 `signaling::LazyTransport`：它把 `RustDeskTransport` 的构造
// 推迟到 `RemoteParams` 到手，并在拿到参数时**立即**构造（= 立即向会合服务器登记），
// 避免「控制器先发中继请求、被控端还没登记」导致的推送丢失。这里只做转出，便于本模块引用。
pub use rust_tunnel_remote::signaling::LazyTransport;
