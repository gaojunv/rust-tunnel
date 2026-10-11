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

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

// enigo 的 `move_mouse` / `button` / `scroll` / `key` 由这两个 trait 提供，必须引入。
use enigo::{Keyboard as _, Mouse as _};
// `Clipboard::get` 供后台线程读取本机剪贴板（见 `RemoteView::request_local_clipboard`）。
use rust_tunnel_remote::Clipboard as _;
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

// ── 输入门控 ─────────────────────────────────────────────────────────
//
// 控制器侧的鼠标/键盘事件来自**整窗**（`ui.input(..)` 不区分是哪个控件收到的），若不门控
// 就会把用户在工具条、剪贴板输入框、文件列表上的操作一并注入远端；而远端输入是**有状态**的
// （按下与抬起必须配对），所以又不能「只看指针在不在画面上」一刀切——那样移出画面后的抬起
// 会丢，远端会卡键。下面这个状态机把「是否转达」的判定独立出来，与 egui 运行时解耦，便于单测。

/// 修饰键位掩码 → 跨平台键码（顺序固定，便于边沿检测）。
const MODIFIER_CODES: [(u8, u32); 4] = [
    (modifier::ALT, keycode::ALT),
    (modifier::CTRL, keycode::CTRL),
    (modifier::SHIFT, keycode::SHIFT),
    (modifier::META, keycode::META),
];

/// 鼠标键顺序（下标与 `mouse_seen` / `mouse_forwarded` 对应）。
const MOUSE_BUTTONS: [MouseButton; 3] = [MouseButton::Left, MouseButton::Right, MouseButton::Middle];

/// 组一个键盘 [`InputEvent`]（减少重复字面量）。
fn key_event(code: u32, pressed: bool, text: Option<String>, modifiers: u8) -> InputEvent {
    InputEvent::Key(KeyEvent { code, pressed, text, modifiers })
}

/// `MouseButton` → 数组下标（与 [`MOUSE_BUTTONS`] 对应）。
fn mouse_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}

/// 输入门控状态机（**只做「是否转达」的判定，不依赖 egui 运行时**，便于单测）。
///
/// # 为什么不能只用 `hovered()` 一把梭
///
/// 远端（被控端）的输入是**有状态**的：按下与抬起必须配对，否则会卡住一个按住不放的键或
/// 鼠标键。而 egui 的鼠标/键盘事件是**整窗**的（`ui.input(..)` 不分控件），所以两个方向
/// 要分开处理：
///
/// - **按下必须门控**：只有指针位于画面内（`pointer_in_image`）时才转达。否则点「中断会话」
///   按钮、在剪贴板/上传路径的输入框里打字、滚动文件列表，都会在最后悬停的远端坐标上凭空
///   注入一次操作；
/// - **抬起必须无条件转达**：只要这次按下**曾经**转达过，其抬起就必须发出去——哪怕指针已
///   移出画面、甚至移出窗口。否则远端会残留一个按下的键或鼠标键。
///
/// 为此这里分别跟踪「观察到的物理状态」（`*_seen`，做边沿检测）与「已转达按下、等待抬起」
/// 的集合（`*_forwarded`）。键盘与修饰键同一门控：只有「本窗口有焦点」且「没有 egui 控件在
/// 吃键盘输入」时才转发按下，避免把用户在本地输入框里敲的字、Ctrl+C 之类同时注入远端。
#[derive(Debug, Default)]
pub struct InputGate {
    /// 上一帧观察到的鼠标键是否按下（下标见 [`MOUSE_BUTTONS`]）。
    mouse_seen: [bool; 3],
    /// 已转达按下、尚未转达抬起的鼠标键。
    mouse_forwarded: [bool; 3],
    /// 已转达按下、尚未转达抬起的特殊键（跨平台键码；文本键无配对抬起，不进此集合）。
    keys_forwarded: BTreeSet<u32>,
    /// 上一帧观察到的修饰键位掩码（边沿检测基线）。
    modifiers_seen: u8,
    /// 已转达按下的修饰键位掩码。
    modifiers_forwarded: u8,
}

impl InputGate {
    /// 鼠标按键门控。`down` 为**本帧该键是否处于按下态**（不是「刚按下」——边沿由本结构自行
    /// 判断）；`pointer_in_image` 为指针是否位于画面内。返回要转达的按下/抬起事件，不转达时
    /// 为 `None`。语义见类型文档。
    #[must_use]
    pub fn mouse_button(&mut self, button: MouseButton, down: bool, pointer_in_image: bool) -> Option<InputEvent> {
        let idx = mouse_index(button);
        if down == self.mouse_seen[idx] {
            return None; // 无状态变化
        }
        self.mouse_seen[idx] = down;
        if down {
            // 按下：只有指针在画面上才转达。
            if !pointer_in_image {
                return None;
            }
            self.mouse_forwarded[idx] = true;
            Some(InputEvent::MouseButton { button, pressed: true })
        } else if self.mouse_forwarded[idx] {
            // 抬起：这次按下曾转达过 → 无条件转达（哪怕指针已移出画面）。
            self.mouse_forwarded[idx] = false;
            Some(InputEvent::MouseButton { button, pressed: false })
        } else {
            // 这次按下从未转达（当时指针在画面外）→ 不能凭空给远端发抬起。
            None
        }
    }

    /// 滚轮门控：只有指针位于画面内时才转达（避免在文件列表 / 输入框上滚动时误滚远端）。
    #[must_use]
    pub fn wheel(pointer_in_image: bool, dx: i32, dy: i32) -> Option<InputEvent> {
        if !pointer_in_image || (dx == 0 && dy == 0) {
            return None;
        }
        Some(InputEvent::Wheel { dx, dy })
    }

    /// 特殊键门控。`code` 为跨平台键码（非 [`keycode::TEXT`]），`pressed` 由 egui 事件直接给出
    /// （自带边沿）；`keyboard_enabled` = 本窗口有焦点且无控件在吃键盘输入。
    ///
    /// 与 [`Self::modifiers`] 的补偿路径对称：`keyboard_enabled == false` 时不再转发任何按下，
    /// 而是返回 [`Self::release_keys`] 的结果（回收所有「已转达按下」的键并清空状态）。这条路径
    /// 必不可少——窗口失焦时 egui **不会**合成 key-up（egui 0.31 的 `keys_down` 只由真实的
    /// `Event::Key` 清除，物理抬起交给了获得焦点的另一个应用），若不在此回收，被控端会一直保持
    /// 该键按下，直到用户再按一次并释放。
    #[must_use]
    pub fn key(&mut self, code: u32, pressed: bool, keyboard_enabled: bool, modifiers: u8) -> Vec<InputEvent> {
        if !keyboard_enabled {
            return self.release_keys();
        }
        if pressed {
            self.keys_forwarded.insert(code);
            return vec![key_event(code, true, None, modifiers)];
        }
        if self.keys_forwarded.remove(&code) {
            // 抬起：曾转达过就无条件转达（与鼠标键同一条不变量）。
            return vec![key_event(code, false, None, modifiers)];
        }
        Vec::new()
    }

    /// 文本输入门控（可打印字符走此路径）。与键盘同一门控；egui 的文本事件没有配对抬起
    /// （注入侧按「点按」处理），故只做单向转发。纯控制字符不转发。
    #[must_use]
    pub fn text(text: &str, keyboard_enabled: bool, modifiers: u8) -> Option<InputEvent> {
        if !keyboard_enabled || !text.chars().any(|c| !c.is_control()) {
            return None;
        }
        Some(key_event(keycode::TEXT, true, Some(text.to_owned()), modifiers))
    }

    /// 修饰键门控。`bits` 为当前位掩码（[`egui_modifiers`] 的结果），`keyboard_enabled` 同键盘。
    /// 返回本帧需要转达的修饰键按下/抬起事件。
    #[must_use]
    pub fn modifiers(&mut self, bits: u8, keyboard_enabled: bool) -> Vec<InputEvent> {
        if !keyboard_enabled {
            // 门控关闭：把已转达按下的修饰键补发抬起，并把观察基线同步到当前物理状态——
            // 否则重新放行时会凭空产生一次「按下」边沿（远端会多按一个修饰键）。
            let out = self.release_modifiers();
            self.modifiers_seen = bits;
            return out;
        }
        let changed = bits ^ self.modifiers_seen;
        self.modifiers_seen = bits;
        let mut out = Vec::new();
        for (mask, code) in MODIFIER_CODES {
            if changed & mask != 0 {
                let pressed = bits & mask != 0;
                if pressed {
                    self.modifiers_forwarded |= mask;
                } else {
                    self.modifiers_forwarded &= !mask;
                }
                out.push(key_event(code, pressed, None, bits));
            }
        }
        out
    }

    /// 释放所有「已转达按下」的鼠标键 / 修饰键 / 特殊键（补发抬起），并清空状态。
    ///
    /// 退出捕获（会话结束、切 Tab、切到非「完全控制」模式）时调用；返回的事件应投递给引擎——
    /// 会话已结束时 `send_input` 会静默丢弃，无副作用。
    #[must_use]
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for (idx, button) in MOUSE_BUTTONS.iter().enumerate() {
            if self.mouse_forwarded[idx] {
                self.mouse_forwarded[idx] = false;
                out.push(InputEvent::MouseButton { button: *button, pressed: false });
            }
        }
        out.extend(self.release_modifiers());
        out.extend(self.release_keys());
        // 观察基线一并清零：重新进入捕获时不因残留基线凭空产生边沿。
        self.mouse_seen = [false; 3];
        self.modifiers_seen = 0;
        out
    }

    /// 回收所有「已转达按下」的特殊键（补发抬起）并清空集合，返回事件列表。
    ///
    /// 与 [`Self::release_all`] 的区别：只动键盘、不动鼠标——键盘门控关闭（失焦 / 有控件在吃
    /// 键盘输入）但鼠标仍可能在使用时只回收键盘。**幂等**：状态清空后再调用返回空，故可每帧安全
    /// 调用而不重发抬起。修饰键不在此处处理，由 [`Self::modifiers`] 的补偿路径负责释放并同步基线。
    #[must_use]
    pub fn release_keys(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.keys_forwarded)
            .into_iter()
            .map(|code| key_event(code, false, None, 0))
            .collect()
    }

    /// 清空所有状态、**不**补发事件（会话已确定不可达、或纯 UI 复位时用）。
    pub fn reset(&mut self) {
        self.mouse_seen = [false; 3];
        self.mouse_forwarded = [false; 3];
        self.keys_forwarded.clear();
        self.modifiers_seen = 0;
        self.modifiers_forwarded = 0;
    }

    /// 把已转达按下的修饰键补发抬起并清零，返回事件列表。
    fn release_modifiers(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for (mask, code) in MODIFIER_CODES {
            if self.modifiers_forwarded & mask != 0 {
                out.push(key_event(code, false, None, 0));
            }
        }
        self.modifiers_forwarded = 0;
        out
    }
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

/// 本机剪贴板异步读取的结果槽：`(请求代, 结果)`。`Arc<Mutex<..>>` 供短命后台线程写入、
/// UI 线程读取（非阻塞）；「代」用于丢弃跨会话 / 被后续请求取代的陈旧结果。
type ClipboardReadSlot = Arc<Mutex<Option<(u64, Result<String, String>)>>>;

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
    /// 输入门控状态机（按下需门控、抬起按「是否转达过」处理）。
    gate: InputGate,
    /// 本帧 [`Self::capture_input`] 是否运行过的戳记（egui 输入时间）；见
    /// [`Self::reconcile_input_capture`]。
    capture_stamp: Option<f64>,
    /// 后台读取本机剪贴板的结果槽（非阻塞）；携带发起请求时的「代」编号（见
    /// [`Self::request_local_clipboard`] 与 [`Self::take_clipboard_read`]）。
    clipboard_read: ClipboardReadSlot,
    /// 剪贴板读取的「代」：每次发起请求 +1；[`Self::reset_session`] 也 +1 以作废在途读取。
    /// 取回时只认当前代的结果，从而丢弃跨会话 / 被后续请求取代的陈旧结果。
    clipboard_read_gen: u64,
    /// 接入确认弹窗的「本次运行内不再询问该对端」勾选态（请求消失时由弹窗清零）。
    approval_remember: bool,
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
            gate: InputGate::default(),
            capture_stamp: None,
            clipboard_read: Arc::new(Mutex::new(None)),
            clipboard_read_gen: 0,
            approval_remember: false,
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

    /// 会话结束后清空纹理与输入门控状态。
    ///
    /// 这里**不**补发抬起：会话已经结束，引擎会丢弃输入事件。会话仍在、只是退出「完全控制」
    /// 捕获的场景由 [`Self::reconcile_input_capture`] 负责补发抬起。
    pub fn reset_session(&mut self) {
        self.texture = None;
        self.tex_size = (0, 0);
        self.last_mouse = None;
        self.gate.reset();
        self.capture_stamp = None;
        self.selected_display = None;
        // 本机剪贴板读取结果属于上一会话：清空槽位，并推进「代」以作废在途读取——后台线程若在
        // 复位之后才写回旧结果，其代号与当前不符，会被 `take_clipboard_read` 丢弃，不会流入下一
        // 个会话的输入框。
        if let Ok(mut guard) = self.clipboard_read.lock() {
            *guard = None;
        }
        self.clipboard_read_gen = self.clipboard_read_gen.wrapping_add(1);
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

    /// 接入确认弹窗的「本次运行内不再询问该对端」勾选态（可编辑；无请求时由弹窗清零）。
    pub fn approval_remember_mut(&mut self) -> &mut bool {
        &mut self.approval_remember
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
    ///
    /// 事件来源是**整窗**的，故按键/滚轮/键盘都要经 [`InputGate`] 门控（详见其文档）：鼠标
    /// 按下只在指针位于画面上时转达、抬起只要曾转达过就无条件转达；滚轮只在指针位于画面上
    /// 时转达；键盘只在窗口有焦点且无控件吃键盘输入时转达。鼠标移动仍只在指针位于画面上时
    /// 更新（`hover_pos` 本身即以 `hovered()` 为准）。
    pub fn capture_input(
        &mut self,
        ui: &egui::Ui,
        manager: &Arc<RemoteManager>,
        response: &egui::Response,
        rect: egui::Rect,
        remote_w: u32,
        remote_h: u32,
    ) {
        // 记录「本帧已捕获」的戳记：`reconcile_input_capture` 靠它判断捕获是否仍在进行。
        self.capture_stamp = Some(ui.ctx().input(|i| i.time));

        let display = self.selected_display.unwrap_or(0);

        // 指针是否在画面内：拖动本控件（画面）期间 `hovered()` 会变 false，故把
        // `is_pointer_button_down_on()` 也算进来，让「在画面上按下后拖出画面」的拖动继续生效。
        let pointer_in_image = response.hovered() || response.is_pointer_button_down_on();

        // 1) 鼠标移动（去重后发送；仅在指针位于画面上时更新）。
        if let Some(pos) = response.hover_pos() {
            let x = scale_axis(pos.x - rect.min.x, rect.width(), remote_w);
            let y = scale_axis(pos.y - rect.min.y, rect.height(), remote_h);
            if self.last_mouse != Some((x, y)) {
                self.last_mouse = Some((x, y));
                manager.send_input(InputEvent::MouseMove { x, y, display });
            }
        }

        // 键盘门控：本窗口有焦点，且当前没有 egui 控件在吃键盘输入。否则用户在本地输入框
        // （剪贴板 / 上传路径）里敲的字、Ctrl+C 之类会被同时注入远端——egui 不会把这些事件
        // 从 `i.events` 里摘掉，必须由我们判断。
        let keyboard_enabled = ui.ctx().input(|i| i.focused) && !ui.ctx().wants_keyboard_input();

        // 2) 鼠标按键 / 滚轮 / 修饰键（经门控，按边沿发送）。
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
        for (button, down) in [
            (MouseButton::Left, buttons[0]),
            (MouseButton::Right, buttons[1]),
            (MouseButton::Middle, buttons[2]),
        ] {
            if let Some(event) = self.gate.mouse_button(button, down, pointer_in_image) {
                manager.send_input(event);
            }
        }
        // egui 的滚动量为正表示内容上移，enigo 正值为向下滚动 → 取反。
        if let Some(event) = InputGate::wheel(pointer_in_image, -round_i32(scroll.x), -round_i32(scroll.y)) {
            manager.send_input(event);
        }
        for event in self.gate.modifiers(mods, keyboard_enabled) {
            manager.send_input(event);
        }
        // 键盘门控关闭（失焦 / 有控件在吃键盘输入）时每帧收敛：把「已转达按下」的特殊键补发抬起。
        // 这里不能只依赖下面的事件循环——egui 在窗口失焦时不会合成 key-up，若不在门控关闭的这一帧
        // 回收，被控端会残留一个按住不放的键。与 `modifiers()` 的补偿路径对称；`release_keys` 幂等，
        // 状态清空后重复调用返回空，故不会每帧重发抬起。
        if !keyboard_enabled {
            for event in self.gate.release_keys() {
                manager.send_input(event);
            }
        }

        // 3) 键盘事件（特殊键走 code；文本走 TEXT）。控制字符（Enter/Tab 等）已由 Key 事件
        //    处理，`InputGate::text` 会把纯控制文本滤掉，避免重复。
        let events = ui.input(|i| i.events.clone());
        for event in events {
            match event {
                egui::Event::Key { key, pressed, .. } => {
                    if let Some(code) = egui_key_to_code(key) {
                        for out in self.gate.key(code, pressed, keyboard_enabled, mods) {
                            manager.send_input(out);
                        }
                    }
                }
                egui::Event::Text(text) => {
                    if let Some(out) = InputGate::text(&text, keyboard_enabled, mods) {
                        manager.send_input(out);
                    }
                }
                _ => {}
            }
        }
    }

    /// 每帧收敛输入捕获状态：若本帧 [`Self::capture_input`] 没有运行（用户切到别的 Tab、模式
    /// 不再是「完全控制」、会话已结束、或画面还没到），就把上一轮「已转达按下」的鼠标键 / 键 /
    /// 修饰键补发抬起并清空门控，避免远端残留按下的键。
    ///
    /// 「本帧是否捕获过」用 egui 的输入时间戳判断（同一帧的多次 pass 该值不变，跨帧递增）。
    ///
    /// 调用点：由 [`crate::ui::remote::approval_modal`] 每帧调用——它是 `GuiApp::update` 里
    /// **每帧**都会执行、且同时拿到 `&mut RemoteView` 与 [`egui::Context`] 的唯一入口
    /// （`ui::remote::show` 只在远程 Tab 被选中时才渲染）。
    pub fn reconcile_input_capture(&mut self, ctx: &egui::Context, manager: Option<&Arc<RemoteManager>>) {
        let now = ctx.input(|i| i.time);
        if self.capture_stamp == Some(now) {
            return; // 本帧已捕获输入，无需收敛
        }
        self.capture_stamp = None;
        let releases = self.gate.release_all();
        if let Some(manager) = manager {
            for event in releases {
                manager.send_input(event);
            }
        }
    }

    /// 请求异步读取本机剪贴板：立刻返回，结果在下一次 [`Self::take_clipboard_read`] 取回。
    ///
    /// **不能**在 `update` 里同步调用 `SystemClipboard::get`：`arboard::Clipboard::new()` 每次
    /// 都要新建平台连接，X11 下若别的进程正持有 selection 可能长时间阻塞，会直接卡死 UI 线程。
    /// 故丢到短命线程里读，结果经共享槽交回 UI 线程。每次请求推进「代」，后发起的读取胜出；被
    /// [`Self::reset_session`] 作废的旧请求即使晚到也不会被取回。
    pub fn request_local_clipboard(&mut self) {
        self.clipboard_read_gen = self.clipboard_read_gen.wrapping_add(1);
        let gen = self.clipboard_read_gen;
        let slot = Arc::clone(&self.clipboard_read);
        std::thread::spawn(move || {
            // 后台线程 panic 会静默丢掉结果并让槽位空转；用 catch_unwind 兜底成错误信息。
            let result = std::panic::catch_unwind(|| SystemClipboard.get().map_err(|err| err.to_string()))
                .unwrap_or_else(|_| Err("读取本机剪贴板时发生内部错误".to_owned()));
            if let Ok(mut guard) = slot.lock() {
                // 只写入不比槽内更旧的结果：连点两次时后发起的（代更大）胜出，先发起者若晚到也不
                // 覆盖它；复位后旧代的结果同样在此被丢弃。
                let stale = matches!(&*guard, Some((older, _)) if gen < *older);
                if !stale {
                    *guard = Some((gen, result));
                }
            }
        });
    }

    /// 取回后台读取的本机剪贴板结果（取走即清）；仍在读取、无请求、或结果属于旧的一代（上一会话 /
    /// 已被更新的请求取代）时 `None`。
    #[must_use]
    pub fn take_clipboard_read(&mut self) -> Option<Result<String, String>> {
        let mut guard = self.clipboard_read.lock().ok()?;
        match guard.take() {
            Some((gen, result)) if gen == self.clipboard_read_gen => Some(result),
            _ => None,
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

#[cfg(test)]
mod tests {
    //! 只覆盖**纯逻辑**：门控状态机与坐标/尺寸换算。真实鼠标/键盘行为（egui 的 `hovered()`、
    //! `wants_keyboard_input()`、平台注入）无法在无头环境验证，只能由这些单测锁住判定规则。

    use super::*;

    /// 构造一条鼠标按键事件（测试用）。
    fn mouse_btn(button: MouseButton, pressed: bool) -> InputEvent {
        InputEvent::MouseButton { button, pressed }
    }

    /// 构造一条键盘事件（测试用）。
    fn key_ev(code: u32, pressed: bool) -> InputEvent {
        InputEvent::Key(KeyEvent { code, pressed, text: None, modifiers: 0 })
    }

    // ── InputGate：鼠标按键门控矩阵 ──────────────────────────────────

    #[test]
    fn mouse_press_outside_image_is_not_forwarded() {
        let mut gate = InputGate::default();
        // 指针不在画面上（点工具条 / 在输入框里点）→ 按下绝不转达。
        assert_eq!(gate.mouse_button(MouseButton::Left, true, false), None);
        // 这次按下从未转达，抬起也不能凭空发出。
        assert_eq!(gate.mouse_button(MouseButton::Left, false, false), None);
    }

    #[test]
    fn mouse_press_inside_image_is_forwarded_once() {
        let mut gate = InputGate::default();
        assert_eq!(gate.mouse_button(MouseButton::Left, true, true), Some(mouse_btn(MouseButton::Left, true)));
        // 仍按住（无新边沿）→ 不重复发。
        assert_eq!(gate.mouse_button(MouseButton::Left, true, true), None);
    }

    #[test]
    fn mouse_release_is_forwarded_even_after_pointer_left_image() {
        let mut gate = InputGate::default();
        assert!(gate.mouse_button(MouseButton::Left, true, true).is_some());
        // 按下已转达过 → 即便指针已移出画面（甚至窗口），抬起仍必须转达。
        assert_eq!(gate.mouse_button(MouseButton::Left, false, false), Some(mouse_btn(MouseButton::Left, false)));
        // 再抬起（本就没按下）→ 不转达。
        assert_eq!(gate.mouse_button(MouseButton::Left, false, false), None);
    }

    #[test]
    fn mouse_button_gating_is_per_button() {
        let mut gate = InputGate::default();
        // 右键在画面外按下不转达；左键在画面内按下转达——两者互不影响。
        assert_eq!(gate.mouse_button(MouseButton::Right, true, false), None);
        assert!(gate.mouse_button(MouseButton::Left, true, true).is_some());
        assert_eq!(gate.mouse_button(MouseButton::Right, false, true), None);
        assert!(gate.mouse_button(MouseButton::Left, false, true).is_some());
    }

    /// 在画面上拖出画面：按下时在画面内、抬起时指针已在画面外 —— 抬起仍要转达。
    #[test]
    fn mouse_drag_starting_in_image_releases_when_outside() {
        let mut gate = InputGate::default();
        assert!(gate.mouse_button(MouseButton::Left, true, true).is_some());
        // 拖动期间指针离开画面：`pointer_in_image` 由 hovered||is_pointer_button_down_on 决定，
        // 这里直接验证「指针在画面外」时按下状态不变则不重复发、抬起照发。
        assert_eq!(gate.mouse_button(MouseButton::Left, true, false), None);
        assert_eq!(gate.mouse_button(MouseButton::Left, false, false), Some(mouse_btn(MouseButton::Left, false)));
    }

    // ── InputGate：滚轮门控 ──────────────────────────────────────────

    #[test]
    fn wheel_only_forwards_when_pointer_in_image() {
        assert_eq!(InputGate::wheel(false, 3, -4), None);
        assert_eq!(InputGate::wheel(true, 0, 0), None);
        assert_eq!(InputGate::wheel(true, 3, -4), Some(InputEvent::Wheel { dx: 3, dy: -4 }));
    }

    // ── InputGate：键盘 / 文本 / 修饰键门控 ──────────────────────────

    #[test]
    fn key_press_requires_keyboard_gate() {
        let mut gate = InputGate::default();
        // 有控件在吃键盘（或窗口失焦）→ 按下不转达。
        assert!(gate.key(keycode::ENTER, true, false, 0).is_empty());
        // 未转达过按下的抬起也不转达。
        assert!(gate.key(keycode::ENTER, false, false, 0).is_empty());
    }

    #[test]
    fn key_release_is_forwarded_after_gated_press_even_if_gate_closed() {
        let mut gate = InputGate::default();
        assert_eq!(gate.key(keycode::ENTER, true, true, 0), vec![key_ev(keycode::ENTER, true)]);
        // 随后门控关闭（失焦 / 输入框获得焦点）→ key() 的补偿路径把该键补发抬起，远端不会卡住 Enter。
        assert_eq!(gate.key(keycode::ENTER, false, false, 0), vec![key_ev(keycode::ENTER, false)]);
    }

    /// 门控关闭时 `key()` 走与 `modifiers()` 对称的补偿路径：回收已转达按下的键。
    #[test]
    fn key_recovers_forwarded_keys_when_gate_closes() {
        let mut gate = InputGate::default();
        // 门控开启时按下方向键 → 转达按下。
        assert_eq!(gate.key(keycode::ARROW_UP, true, true, 0), vec![key_ev(keycode::ARROW_UP, true)]);
        // 失焦（门控关闭）：不再转发任何按下，而是把该键补发抬起。
        assert_eq!(gate.key(keycode::ARROW_UP, true, false, 0), vec![key_ev(keycode::ARROW_UP, false)]);
        // 幂等：状态已清干净，重复调用（无论 pressed 真假）都不再重发抬起。
        assert!(gate.key(keycode::ARROW_UP, true, false, 0).is_empty());
        assert!(gate.key(keycode::ARROW_UP, false, false, 0).is_empty());
        // 退出捕获用的 `release_all` 也已是空。
        assert!(gate.release_all().is_empty());
    }

    /// 门控关闭后重新开启，同一个键必须能再次产生「按下」边沿（不因状态残留被吞掉）。
    #[test]
    fn key_can_press_again_after_gate_reopens() {
        let mut gate = InputGate::default();
        assert_eq!(gate.key(keycode::ENTER, true, true, 0), vec![key_ev(keycode::ENTER, true)]);
        // 门控关闭 → 补发抬起、清空 keys_forwarded。
        assert_eq!(gate.key(keycode::ENTER, true, false, 0), vec![key_ev(keycode::ENTER, false)]);
        // 重新放行：同一键再次按下必须能产生新的「按下」边沿。
        assert_eq!(gate.key(keycode::ENTER, true, true, 0), vec![key_ev(keycode::ENTER, true)]);
    }

    /// `release_keys` 只回收键盘（不动鼠标），且幂等。
    #[test]
    fn release_keys_leaves_mouse_untouched_and_is_idempotent() {
        let mut gate = InputGate::default();
        assert!(gate.mouse_button(MouseButton::Left, true, true).is_some());
        assert_eq!(gate.key(keycode::ENTER, true, true, 0), vec![key_ev(keycode::ENTER, true)]);
        assert_eq!(gate.key(keycode::F1, true, true, 0), vec![key_ev(keycode::F1, true)]);
        let released = gate.release_keys();
        assert!(released.contains(&key_ev(keycode::ENTER, false)));
        assert!(released.contains(&key_ev(keycode::F1, false)));
        // 鼠标键未被键盘回收触碰，仍处于「已转达按下」。
        assert_eq!(gate.mouse_button(MouseButton::Left, false, true), Some(mouse_btn(MouseButton::Left, false)));
        // 幂等：再次回收键盘返回空。
        assert!(gate.release_keys().is_empty());
    }

    #[test]
    fn text_is_gated_and_ignores_control_only_text() {
        assert_eq!(InputGate::text("a", false, 0), None);
        assert_eq!(
            InputGate::text("a", true, 0),
            Some(InputEvent::Key(KeyEvent { code: keycode::TEXT, pressed: true, text: Some("a".to_owned()), modifiers: 0 }))
        );
        // 纯控制字符（Enter/Tab 等）已由 Key 事件处理，不重复注入。
        assert_eq!(InputGate::text("\n", true, 0), None);
    }

    #[test]
    fn modifiers_forward_edge_then_release_when_gate_closes() {
        let mut gate = InputGate::default();
        // Ctrl 按下边沿 → 转达按下。
        assert_eq!(
            gate.modifiers(modifier::CTRL, true),
            vec![InputEvent::Key(KeyEvent { code: keycode::CTRL, pressed: true, text: None, modifiers: modifier::CTRL })]
        );
        // 门控关闭 → 补发 Ctrl 抬起（否则远端残留一个按下的 Ctrl）。
        assert_eq!(gate.modifiers(modifier::CTRL, false), vec![key_ev(keycode::CTRL, false)]);
        // 重新放行：基线已同步为 CTRL，不应凭空再产生一次「按下」边沿。
        assert!(gate.modifiers(modifier::CTRL, true).is_empty());
        // 真正的抬起边沿照常转达。
        assert_eq!(gate.modifiers(0, true), vec![key_ev(keycode::CTRL, false)]);
    }

    #[test]
    fn modifiers_single_disabled_frame_without_forwarded_modifier_emits_nothing() {
        let mut gate = InputGate::default();
        // 从没转达过修饰键按下时，门控关闭不应凭空发抬起。
        assert!(gate.modifiers(modifier::SHIFT, false).is_empty());
    }

    // ── InputGate：退出捕获时的清理 ─────────────────────────────────

    #[test]
    fn reset_clears_state_without_releasing() {
        let mut gate = InputGate::default();
        assert!(gate.mouse_button(MouseButton::Left, true, true).is_some());
        assert!(!gate.key(keycode::ENTER, true, true, 0).is_empty());
        assert!(!gate.modifiers(modifier::CTRL, true).is_empty());
        // 会话结束 / 切模式：状态清干净，且不补发（会话已不可达）。
        gate.reset();
        assert!(gate.release_all().is_empty());
        // 复位后，未转达过按下的抬起不会被转发（避免凭空发松开）。
        assert_eq!(gate.mouse_button(MouseButton::Left, false, true), None);
        assert!(gate.key(keycode::ENTER, false, true, 0).is_empty());
        assert!(gate.modifiers(0, true).is_empty());
    }

    #[test]
    fn release_all_emits_release_for_everything_forwarded_then_empties() {
        let mut gate = InputGate::default();
        assert!(gate.mouse_button(MouseButton::Left, true, true).is_some());
        assert!(gate.mouse_button(MouseButton::Middle, true, true).is_some());
        assert!(!gate.key(keycode::ENTER, true, true, 0).is_empty());
        assert!(!gate.modifiers(modifier::CTRL, true).is_empty());

        let released = gate.release_all();
        assert!(released.contains(&mouse_btn(MouseButton::Left, false)));
        assert!(released.contains(&mouse_btn(MouseButton::Middle, false)));
        assert!(released.contains(&key_ev(keycode::ENTER, false)));
        assert!(released.contains(&key_ev(keycode::CTRL, false)));
        // 第二次调用已无内容可释放。
        assert!(gate.release_all().is_empty());
    }

    // ── 键码 / 修饰键映射 ───────────────────────────────────────────

    #[test]
    fn egui_key_to_code_maps_specials_and_rejects_unknown() {
        assert_eq!(egui_key_to_code(egui::Key::Enter), Some(keycode::ENTER));
        assert_eq!(egui_key_to_code(egui::Key::Escape), Some(keycode::ESCAPE));
        assert_eq!(egui_key_to_code(egui::Key::ArrowUp), Some(keycode::ARROW_UP));
        assert_eq!(egui_key_to_code(egui::Key::ArrowLeft), Some(keycode::ARROW_LEFT));
        assert_eq!(egui_key_to_code(egui::Key::F1), Some(keycode::F1));
        assert_eq!(egui_key_to_code(egui::Key::F12), Some(keycode::F1 + 11));
        // 普通字符键走 TEXT 路径；未知键返回 None 而不是 panic。
        assert_eq!(egui_key_to_code(egui::Key::A), None);
        assert_eq!(egui_key_to_code(egui::Key::Num0), None);
    }

    #[test]
    fn code_to_enigo_round_trips_specials_and_f_keys() {
        assert_eq!(code_to_enigo(keycode::ENTER), Some(enigo::Key::Return));
        assert_eq!(code_to_enigo(keycode::ESCAPE), Some(enigo::Key::Escape));
        assert_eq!(code_to_enigo(keycode::CTRL), Some(enigo::Key::Control));
        assert_eq!(code_to_enigo(keycode::F1), Some(enigo::Key::F1));
        assert_eq!(code_to_enigo(keycode::F1 + 11), Some(enigo::Key::F12));
        // 未知码静默忽略（不 panic）。
        assert_eq!(code_to_enigo(keycode::TEXT), None);
        assert_eq!(code_to_enigo(keycode::F1 + 12), None);
        assert_eq!(code_to_enigo(u32::MAX), None);
    }

    #[test]
    fn egui_modifiers_sets_each_bit() {
        assert_eq!(egui_modifiers(egui::Modifiers::NONE), 0);
        assert_eq!(egui_modifiers(egui::Modifiers::ALT), modifier::ALT);
        assert_eq!(egui_modifiers(egui::Modifiers::CTRL), modifier::CTRL);
        assert_eq!(egui_modifiers(egui::Modifiers::SHIFT), modifier::SHIFT);
        assert_eq!(egui_modifiers(egui::Modifiers::COMMAND), modifier::META);
        assert_eq!(
            egui_modifiers(egui::Modifiers::CTRL | egui::Modifiers::ALT | egui::Modifiers::SHIFT),
            modifier::CTRL | modifier::ALT | modifier::SHIFT
        );
    }

    // ── 坐标 / 尺寸换算的边界 ───────────────────────────────────────

    #[test]
    fn scale_axis_is_bounded_and_never_panics() {
        // 显示区域宽度为 0（画面还没布局好）→ 0，不做除零。
        assert_eq!(scale_axis(10.0, 0.0, 1920), 0);
        assert_eq!(scale_axis(0.0, -5.0, 1920), 0);
        // 左 / 上溢出被夹到 0；右 / 下溢出被夹到 max。
        assert_eq!(scale_axis(-100.0, 100.0, 100), 0);
        assert_eq!(scale_axis(50.0, 100.0, 100), 50);
        assert_eq!(scale_axis(200.0, 100.0, 100), 100);
        // 极大 / 极小偏移不 panic、不产生负数或溢出。
        assert!(scale_axis(f32::MAX, 1.0, u32::MAX) > 0);
        assert_eq!(scale_axis(f32::MIN, 1.0, u32::MAX), 0);
    }

    #[test]
    fn round_i32_handles_extremes() {
        assert_eq!(round_i32(0.0), 0);
        assert_eq!(round_i32(1.4), 1);
        assert_eq!(round_i32(-1.6), -2);
        // f32 越界 → 饱和到 i32 边界，不 panic、不 wrap。
        assert_eq!(round_i32(f32::MAX), i32::MAX);
        assert_eq!(round_i32(f32::MIN), i32::MIN);
    }

    #[test]
    fn valid_frame_dims_checks_length_and_nonzero_size() {
        let frame = DecodedFrame { display: 0, width: 2, height: 2, seq: 0, rgba: vec![0; 16] };
        assert_eq!(valid_frame_dims(&frame), Some((2, 2)));
        // 像素数据长度与尺寸不符 → 拒。
        let short = DecodedFrame { rgba: vec![0; 15], ..frame.clone() };
        assert_eq!(valid_frame_dims(&short), None);
        let long = DecodedFrame { rgba: vec![0; 17], ..frame.clone() };
        assert_eq!(valid_frame_dims(&long), None);
        // 任一维为 0 → 拒。
        let zero_w = DecodedFrame { width: 0, height: 2, rgba: Vec::new(), ..frame.clone() };
        assert_eq!(valid_frame_dims(&zero_w), None);
        let zero_h = DecodedFrame { width: 2, height: 0, rgba: Vec::new(), ..frame.clone() };
        assert_eq!(valid_frame_dims(&zero_h), None);
        // 尺寸乘积溢出 usize → 拒（不 panic）。用 u32::MAX×u32::MAX×4 会溢出。
        let huge = DecodedFrame { width: u32::MAX, height: u32::MAX, rgba: Vec::new(), ..frame };
        assert_eq!(valid_frame_dims(&huge), None);
    }

    // ── RemoteView：会话复位时的剪贴板读取槽清理 ─────────────────────

    /// 会话复位必须清空本机剪贴板读取槽，并作废上一会话在途读取的结果（防跨会话残留）。
    #[test]
    fn reset_session_clears_clipboard_read_slot_and_invalidates_in_flight() {
        let mut view = RemoteView::new();
        // 上一会话遗留的读取结果：复位后应被清空。
        let stale_gen = view.clipboard_read_gen;
        if let Ok(mut guard) = view.clipboard_read.lock() {
            *guard = Some((stale_gen, Ok("上一会话".to_owned())));
        }
        view.reset_session();
        assert_eq!(view.take_clipboard_read(), None);

        // 在途线程在复位之后才写回旧一代的结果：必须被丢弃，不能流入下一次取回。
        if let Ok(mut guard) = view.clipboard_read.lock() {
            *guard = Some((stale_gen, Ok("迟到的上一会话".to_owned())));
        }
        assert_eq!(view.take_clipboard_read(), None);

        // 当前一代的结果正常返回。
        let fresh_gen = view.clipboard_read_gen;
        if let Ok(mut guard) = view.clipboard_read.lock() {
            *guard = Some((fresh_gen, Ok("当前会话".to_owned())));
        }
        assert_eq!(view.take_clipboard_read(), Some(Ok("当前会话".to_owned())));
        // 取走即清，第二次取回为空。
        assert_eq!(view.take_clipboard_read(), None);
    }
}
