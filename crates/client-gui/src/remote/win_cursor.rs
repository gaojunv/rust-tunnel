//! 远端光标形状采样（仅 Windows）。
//!
//! 被控端每拍采集后调 [`shape`]，把结果随采集结果交给引擎；引擎只在**形状变化**时下发
//! `SessionMessage::CursorShape`。这里是「形状枚举」而不是位图：把系统光标句柄与
//! `IDC_*` 的系统资源逐一比对即可，不需要抓取光标位图。
//!
//! 语义：
//! - [`GetCursorInfo`] 失败 → `None`（平台读不到，引擎不发消息，控制端保持默认箭头）；
//! - `flags` 不含 [`CURSOR_SHOWING`] → [`CursorShape::Hidden`]（远端隐藏了光标）；
//! - 句柄不在已知表里 → [`CursorShape::Unknown`]（例如应用自定义的彩色光标）。

use rust_tunnel_remote::screen::CursorShape;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, LoadCursorW, CURSORINFO, CURSOR_SHOWING, HCURSOR, IDC_APPSTARTING, IDC_ARROW,
    IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS,
    IDC_SIZENWSE, IDC_SIZEWE, IDC_UPARROW, IDC_WAIT,
};

/// 采样当前桌面光标形状。
///
/// 返回 `None` 表示**读不到**（例如调用线程不在交互式桌面 / API 失败）：引擎把 `None` 当作
/// 「本拍没有样本」，既不下发消息也不清掉上一次形状。
pub(super) fn shape() -> Option<CursorShape> {
    let mut info = CURSORINFO {
        // `cbSize` 必须由调用方填写，否则 `GetCursorInfo` 直接失败。
        cbSize: u32::try_from(std::mem::size_of::<CURSORINFO>()).ok()?,
        ..CURSORINFO::default()
    };
    // SAFETY: `info` 是有效的 `CURSORINFO`，`cbSize` 已按要求填成本结构体大小；
    // 该调用只写 `info`，不持有任何指针。
    if unsafe { GetCursorInfo(&raw mut info) } == 0 {
        return None;
    }
    if info.flags & CURSOR_SHOWING == 0 {
        return Some(CursorShape::Hidden);
    }
    Some(shape_of(info.hCursor))
}

/// 把系统光标句柄映射成形状枚举（未知句柄 → [`CursorShape::Unknown`]）。
fn shape_of(cursor: HCURSOR) -> CursorShape {
    // 顺序只影响「同一句柄命中多个 ID」这种理论情况；系统 IDC_* 句柄互不相同。
    if is_system_cursor(cursor, IDC_ARROW) {
        CursorShape::Arrow
    } else if is_system_cursor(cursor, IDC_IBEAM) {
        CursorShape::IBeam
    } else if is_system_cursor(cursor, IDC_HAND) {
        CursorShape::Hand
    } else if is_system_cursor(cursor, IDC_CROSS) {
        CursorShape::Crosshair
    } else if is_system_cursor(cursor, IDC_WAIT) {
        CursorShape::Wait
    } else if is_system_cursor(cursor, IDC_APPSTARTING) {
        CursorShape::AppStarting
    } else if is_system_cursor(cursor, IDC_HELP) {
        CursorShape::Help
    } else if is_system_cursor(cursor, IDC_SIZENS) {
        CursorShape::SizeNS
    } else if is_system_cursor(cursor, IDC_SIZEWE) {
        CursorShape::SizeWE
    } else if is_system_cursor(cursor, IDC_SIZENWSE) {
        CursorShape::SizeNWSE
    } else if is_system_cursor(cursor, IDC_SIZENESW) {
        CursorShape::SizeNESW
    } else if is_system_cursor(cursor, IDC_SIZEALL) {
        CursorShape::SizeAll
    } else if is_system_cursor(cursor, IDC_NO) {
        CursorShape::No
    } else if is_system_cursor(cursor, IDC_UPARROW) {
        CursorShape::UpArrow
    } else {
        CursorShape::Unknown
    }
}

/// 该句柄是否就是某个系统光标（`LoadCursorW(null, IDC_*)` 返回的共享句柄）。
fn is_system_cursor(cursor: HCURSOR, id: windows_sys::core::PCWSTR) -> bool {
    // SAFETY: 系统光标 ID 都是 `MAKEINTRESOURCE` 形式的整数常量（不是指针），
    // `hInstance` 传空表示取系统资源；该调用不写调用方的内存。
    unsafe { LoadCursorW(std::ptr::null_mut(), id) == cursor }
}
