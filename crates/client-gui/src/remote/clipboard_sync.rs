//! 剪贴板自动同步的去重状态机（纯逻辑，可单测）。
//!
//! # 为什么必须去重
//!
//! 自动同步是**双向**的，而两个方向用的是同一份状态（本机剪贴板 + 会话内的对端文本），
//! 稍不注意就会形成回环：
//!
//! ```text
//! 本机剪贴板 →（发送）→ 对端剪贴板 →（对端也开着自动同步，发回）→ 本机剪贴板 → …
//! ```
//!
//! 所以两端各自记一个「双方已经一致的那份文本」（[`ClipboardSync::last_synced`]）：只有
//! 观察到**与它不同**的文本才产生动作。收到对端文本时不写本机（它已经一致），本机文本
//! 与它相同时不再发送——回环在第二跳就断了。
//!
//! # 为什么是轮询而不是监听
//!
//! egui 只提供**写**系统剪贴板（`Context::copy_text`），没有读取 API，也没有变更回调；
//! 本端读剪贴板要走 `arboard` 新建平台连接（X11 下可能阻塞），所以只能按
//! [`POLL_INTERVAL`] 低频轮询，且读取动作在后台线程做（见
//! [`crate::remote::RemoteView::request_local_clipboard`]）。

use std::time::{Duration, Instant};

/// 轮询本机剪贴板的间隔。
///
/// 取 1.5s 是两头的折中：更短会频繁新建平台连接（X11 下还要抢 selection）；更长则用户
/// 「复制 → 切回远程窗口」时常常还看不到同步结果。
pub const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// 单次同步的文本上限。
///
/// 剪贴板里可能是几百 MB 的富文本/文件列表；会话帧有 16 MiB 上限（见 `remote::session`），
/// 且这条文本还要占用与画面共享的带宽，故超过就不自动同步（手动按钮不受此限）。
pub const MAX_SYNC_BYTES: usize = 256 * 1024;

/// 本帧应执行的剪贴板动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardAction {
    /// 无需动作（未变化、空文本、超长，或已被对方同步过）。
    None,
    /// 把本机文本发给对端。
    Send(String),
    /// 把对端文本写入本机剪贴板。
    Apply(String),
}

/// 自动同步的去重状态。
#[derive(Debug, Default)]
pub struct ClipboardSync {
    /// 双方已经一致的那份文本；`None` 表示本次会话还没有同步过任何内容。
    last_synced: Option<String>,
    /// 上次发起本机剪贴板轮询的时刻。
    last_poll: Option<Instant>,
}

impl ClipboardSync {
    /// 是否该发起一次本机剪贴板读取（并按此推进计时）。
    ///
    /// 由调用方在**自动同步开启**且会话活跃时每帧调用；返回 true 才去起后台读取线程。
    pub fn should_poll(&mut self, now: Instant) -> bool {
        match self.last_poll {
            Some(prev) if now.saturating_duration_since(prev) < POLL_INTERVAL => false,
            _ => {
                self.last_poll = Some(now);
                true
            }
        }
    }

    /// 本机剪贴板的当前内容（轮询读回来的）→ 是否需要发给对端。
    pub fn on_local(&mut self, text: &str) -> ClipboardAction {
        if !is_syncable(text) || self.last_synced.as_deref() == Some(text) {
            return ClipboardAction::None;
        }
        self.last_synced = Some(text.to_owned());
        ClipboardAction::Send(text.to_owned())
    }

    /// 收到的对端剪贴板文本 → 是否需要写入本机。
    pub fn on_remote(&mut self, text: &str) -> ClipboardAction {
        if !is_syncable(text) || self.last_synced.as_deref() == Some(text) {
            return ClipboardAction::None;
        }
        self.last_synced = Some(text.to_owned());
        ClipboardAction::Apply(text.to_owned())
    }

    /// 复位（会话结束 / 换了对端时调用）：新会话的一致性基线无从沿用。
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// 是否值得自动同步：非空、含至少一个非控制字符、且不超过 [`MAX_SYNC_BYTES`]。
///
/// 「含非控制字符」这一条与 [`crate::remote::InputGate::text`] 同源：纯控制字符（如从终端
/// 复制的一串 `\x1b` 转义）同步过去没有意义。
fn is_syncable(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_SYNC_BYTES && text.chars().any(|c| !c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_local_change_is_sent_once() {
        let mut sync = ClipboardSync::default();
        assert_eq!(sync.on_local("hello"), ClipboardAction::Send("hello".to_owned()));
        // 同一份文本不该重复发送（轮询会一遍遍读到它）。
        assert_eq!(sync.on_local("hello"), ClipboardAction::None);
        assert_eq!(sync.on_local("world"), ClipboardAction::Send("world".to_owned()));
    }

    #[test]
    fn remote_text_is_applied_once_and_does_not_echo_back() {
        let mut sync = ClipboardSync::default();
        assert_eq!(sync.on_remote("from-peer"), ClipboardAction::Apply("from-peer".to_owned()));
        // 写进本机剪贴板后，下一次轮询会读到同一份文本——绝不能回推给对端。
        assert_eq!(sync.on_local("from-peer"), ClipboardAction::None);
        // 对端重复推送同一份也不重复写本机。
        assert_eq!(sync.on_remote("from-peer"), ClipboardAction::None);
    }

    #[test]
    fn local_text_equal_to_what_we_sent_is_not_applied_back() {
        let mut sync = ClipboardSync::default();
        assert_eq!(sync.on_local("mine"), ClipboardAction::Send("mine".to_owned()));
        // 对端把我们发出去的原样回推（对端也开着自动同步时的第一跳）。
        assert_eq!(sync.on_remote("mine"), ClipboardAction::None);
    }

    #[test]
    fn returning_to_a_previous_value_syncs_again() {
        let mut sync = ClipboardSync::default();
        assert_eq!(sync.on_local("a"), ClipboardAction::Send("a".to_owned()));
        assert_eq!(sync.on_remote("b"), ClipboardAction::Apply("b".to_owned()));
        // 本机又复制回 "a"：对端此刻持有的是 "b"，所以要再发一次。
        // （这正是「只记一个 last_synced」而不是「记 last_sent + last_applied 两个集合」的原因。）
        assert_eq!(sync.on_local("a"), ClipboardAction::Send("a".to_owned()));
    }

    #[test]
    fn unsyncable_text_never_produces_an_action() {
        let mut sync = ClipboardSync::default();
        for text in ["", "\u{0}\u{1}\u{2}", "\n\r"] {
            assert_eq!(sync.on_local(text), ClipboardAction::None, "{text:?}");
            assert_eq!(sync.on_remote(text), ClipboardAction::None, "{text:?}");
        }
        let too_long = "x".repeat(MAX_SYNC_BYTES + 1);
        assert_eq!(sync.on_local(&too_long), ClipboardAction::None);
        assert_eq!(sync.on_remote(&too_long), ClipboardAction::None);
    }

    #[test]
    fn reset_clears_the_baseline() {
        let mut sync = ClipboardSync::default();
        assert_eq!(sync.on_local("a"), ClipboardAction::Send("a".to_owned()));
        sync.reset();
        // 新会话里同一份文本要重新同步（对端可能已经不是原来那个了）。
        assert_eq!(sync.on_local("a"), ClipboardAction::Send("a".to_owned()));
    }

    #[test]
    fn should_poll_respects_the_interval() {
        let mut sync = ClipboardSync::default();
        let t0 = Instant::now();
        assert!(sync.should_poll(t0), "首次应当立刻轮询一次");
        assert!(!sync.should_poll(t0 + Duration::from_millis(100)));
        // 「差一毫秒」用 `saturating_sub` 构造：`Instant - Duration` 是 clippy 的
        // `unchecked_time_subtraction`（溢出会 panic），这里没有理由去碰它。
        let almost = POLL_INTERVAL.saturating_sub(Duration::from_millis(1));
        assert!(!sync.should_poll(t0 + almost));
        assert!(sync.should_poll(t0 + POLL_INTERVAL));
        assert!(sync.should_poll(t0 + POLL_INTERVAL * 3), "隔了很久也应放行，而不是累积补跑");
    }

    #[test]
    fn should_poll_tolerates_a_clock_that_did_not_advance() {
        // `Instant` 单调，但传入的 now 可能等于（甚至理论上早于）上次记录；不能 panic。
        let mut sync = ClipboardSync::default();
        let t0 = Instant::now();
        assert!(sync.should_poll(t0));
        assert!(!sync.should_poll(t0));
    }
}
