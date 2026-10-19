//! 控制台会话（可移植部分 + Win32 查询）。
//!
//! # 为什么需要它
//!
//! 无人值守被控端必须待在**控制台会话**里——也就是物理屏幕上那个会话。
//! 服务本身跑在会话 0（服务会话），跟用户桌面毫无关系，所以「把实例拉进哪个会话」
//! 是这套设计的核心参数。
//!
//! `WTSGetActiveConsoleSessionId` 在没有控制台会话时返回 `0xFFFF_FFFF`（这是
//! **正常返回**，不是错误），而它在服务启动早期、以及机器无人登录时都可能这样。
//! 这一层把这套 raw 语义包装成 [`ConsoleSession`]，让调用方不必到处写魔数。

// ── 会话变化事件的类型（`SERVICE_CONTROL_SESSIONCHANGE` 的 `event_type`）──
//
// 值抄自 WTSAPI32.h（`WTS_EVENT_*` / `WTS_*` 宏）。**可移植地**放在这里而不是只在
// `#[cfg(windows)]` 分支里用：判断「哪些事件要重建实例」是一条纯逻辑，必须在 macOS 上
// 也能被单测钉住（见 `win-service` 的 `supervise::classify_session_change`）。
//
// 语义上真正用得到的两条：
// - `WTS_SESSION_LOCK` / `WTS_SESSION_UNLOCK`：**锁屏/解锁不改控制台会话 id**，
//   而那正是无人值守被控端要覆盖的场景，故**不重建**（路径上零重启）。
// - 其余（注销/登录/控制台接入断开）：会话真的变了，实例必须跟着换。

/// `WTS_CONSOLE_CONNECT`：有会话接入了控制台（物理屏幕）。
pub const WTS_CONSOLE_CONNECT: u32 = 0x1;
/// `WTS_CONSOLE_DISCONNECT`：控制台会话断开（如快速用户切换把它切走）。
pub const WTS_CONSOLE_DISCONNECT: u32 = 0x2;
/// `WTS_REMOTE_CONNECT`：有会话通过远程（RDP）接入。
pub const WTS_REMOTE_CONNECT: u32 = 0x3;
/// `WTS_REMOTE_DISCONNECT`：远程会话断开。
pub const WTS_REMOTE_DISCONNECT: u32 = 0x4;
/// `WTS_SESSION_LOGON`：某会话完成登录。
pub const WTS_SESSION_LOGON: u32 = 0x5;
/// `WTS_SESSION_LOGOFF`：某会话注销。
pub const WTS_SESSION_LOGOFF: u32 = 0x6;
/// `WTS_SESSION_LOCK`：某会话锁屏（**控制台会话 id 不变**）。
pub const WTS_SESSION_LOCK: u32 = 0x7;
/// `WTS_SESSION_UNLOCK`：某会话解锁（**控制台会话 id 不变**）。
pub const WTS_SESSION_UNLOCK: u32 = 0x8;
/// `WTS_SESSION_REMOTE_CONTROL`：某会话被远程控制（不是本项目的远程控制）。
pub const WTS_SESSION_REMOTE_CONTROL: u32 = 0x9;
/// `WTS_SESSION_CREATE`：新会话被创建。
pub const WTS_SESSION_CREATE: u32 = 0xA;
/// `WTS_SESSION_TERMINATE`：某会话被终止。
pub const WTS_SESSION_TERMINATE: u32 = 0xB;

/// `WTSGetActiveConsoleSessionId` 表示「当前没有控制台会话」的哨兵值。
pub const CONSOLE_SESSION_NONE: u32 = u32::MAX;

/// 控制台会话。`0`（服务会话）也算「没有控制台会话」——那永远不是用户看到的桌面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConsoleSession {
    /// 没有可用的控制台会话（`0xFFFF_FFFF`，或系统报了个 `0`）。
    Inactive,
    /// 控制台会话存在，id 即 `WTSGetActiveConsoleSessionId` 的返回值。
    Active(u32),
}

impl ConsoleSession {
    /// 从 `WTSGetActiveConsoleSessionId` 的原始返回值构造。
    ///
    /// `0xFFFF_FFFF`（无会话）与 `0`（会话 0 = 服务会话）都归入
    /// [`ConsoleSession::Inactive`]。
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        if raw == CONSOLE_SESSION_NONE || raw == 0 {
            Self::Inactive
        } else {
            Self::Active(raw)
        }
    }

    /// 还原成 `WTSGetActiveConsoleSessionId` 风格的原始值。
    #[must_use]
    pub const fn raw(self) -> u32 {
        match self {
            Self::Inactive => CONSOLE_SESSION_NONE,
            Self::Active(id) => id,
        }
    }

    /// 会话 id；[`ConsoleSession::Inactive`] 时为 `None`。
    #[must_use]
    pub const fn id(self) -> Option<u32> {
        match self {
            Self::Inactive => None,
            Self::Active(id) => Some(id),
        }
    }

    /// 是否有可用的控制台会话。
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active(_))
    }
}

/// 查询当前控制台会话（`WTSGetActiveConsoleSessionId`）。
///
/// # 实现契约
///
/// ```text
/// pub fn active_console_session() -> ConsoleSession
/// ```
///
/// - 调 `WTSGetActiveConsoleSessionId()`（`Win32::System::RemoteDesktop`）。
/// - **该函数不失败**（无返回值判错），故返回 `ConsoleSession` 而不是 `WinResult`。
/// - 结果经 [`ConsoleSession::from_raw`] 归一。
#[cfg(windows)]
#[must_use]
pub fn active_console_session() -> ConsoleSession {
    // SAFETY: 无参数、无副作用；返回值语义即“无会话时为 0xFFFFFFFF”。
    let raw = unsafe { windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };
    ConsoleSession::from_raw(raw)
}

/// 查询指定进程所在会话（`ProcessIdToSessionId`）。
///
/// # 实现契约
///
/// ```text
/// pub fn process_session_id(pid: u32) -> WinResult<u32>
/// ```
///
/// - `ProcessIdToSessionId(pid, &mut sid)`，失败时用 `check_bool` 转 [`crate::WinError`]。
/// - 用途一：探针报告实例自己在哪个会话（证明「拉进控制台会话」这件事到底成没成）。
/// - 用途二：阶段 1 判断「服务是不是已经在目标会话里」。
///
/// # Errors
///
/// 进程不存在或无权查询时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn process_session_id(pid: u32) -> crate::WinResult<u32> {
    let mut sid = 0_u32;
    let ok = {
        // SAFETY: sid 是本栈上的 u32，指针有效且可写。
        unsafe {
            windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(pid, &raw mut sid)
        }
    };
    crate::error::check_bool("ProcessIdToSessionId", ok)?;
    Ok(sid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_is_inactive() {
        assert_eq!(
            ConsoleSession::from_raw(CONSOLE_SESSION_NONE),
            ConsoleSession::Inactive
        );
        assert_eq!(
            ConsoleSession::from_raw(0xFFFF_FFFF),
            ConsoleSession::Inactive
        );
        assert!(!ConsoleSession::from_raw(0xFFFF_FFFF).is_active());
    }

    #[test]
    fn session_zero_is_not_a_desktop_session() {
        // 会话 0 是服务会话：它永远不是用户在屏幕上看到的那个会话。
        assert_eq!(ConsoleSession::from_raw(0), ConsoleSession::Inactive);
    }

    #[test]
    fn real_session_ids_round_trip() {
        for id in [1_u32, 2, 42] {
            let s = ConsoleSession::from_raw(id);
            assert_eq!(s, ConsoleSession::Active(id));
            assert_eq!(s.id(), Some(id));
            assert_eq!(s.raw(), id);
        }
        assert_eq!(ConsoleSession::Inactive.raw(), CONSOLE_SESSION_NONE);
        assert_eq!(ConsoleSession::Inactive.id(), None);
    }

    /// 机械锁：这些值抄自 WTSAPI32.h，是 SCM 传回来的 ABI（`SERVICE_CONTROL_SESSIONCHANGE`
    /// 的 `event_type`）。改错一个，监督逻辑就会把「注销」认成「锁屏」而不重建实例。
    #[test]
    fn wts_event_types_match_the_win32_abi() {
        assert_eq!(WTS_CONSOLE_CONNECT, 0x1);
        assert_eq!(WTS_CONSOLE_DISCONNECT, 0x2);
        assert_eq!(WTS_REMOTE_CONNECT, 0x3);
        assert_eq!(WTS_REMOTE_DISCONNECT, 0x4);
        assert_eq!(WTS_SESSION_LOGON, 0x5);
        assert_eq!(WTS_SESSION_LOGOFF, 0x6);
        assert_eq!(WTS_SESSION_LOCK, 0x7);
        assert_eq!(WTS_SESSION_UNLOCK, 0x8);
        assert_eq!(WTS_SESSION_REMOTE_CONTROL, 0x9);
        assert_eq!(WTS_SESSION_CREATE, 0xA);
        assert_eq!(WTS_SESSION_TERMINATE, 0xB);
        // 两两不同：撞值会让分类把一种事件当成另一种。
        let all = [
            WTS_CONSOLE_CONNECT,
            WTS_CONSOLE_DISCONNECT,
            WTS_REMOTE_CONNECT,
            WTS_REMOTE_DISCONNECT,
            WTS_SESSION_LOGON,
            WTS_SESSION_LOGOFF,
            WTS_SESSION_LOCK,
            WTS_SESSION_UNLOCK,
            WTS_SESSION_REMOTE_CONTROL,
            WTS_SESSION_CREATE,
            WTS_SESSION_TERMINATE,
        ];
        let mut sorted: Vec<u32> = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "事件码必须两两不同");
    }

    #[test]
    fn derived_equality_is_what_the_supervisor_will_use() {
        // 阶段 1 的监督逻辑是「会话变了就重启实例」，靠的就是这个比较。
        assert_ne!(ConsoleSession::from_raw(1), ConsoleSession::from_raw(2));
        assert_eq!(ConsoleSession::from_raw(1), ConsoleSession::from_raw(1));
        assert_ne!(ConsoleSession::Inactive, ConsoleSession::from_raw(1));
    }
}
