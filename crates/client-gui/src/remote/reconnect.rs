//! 断线自动重连的状态机（纯逻辑，可单测）。
//!
//! # 重连 = 重发一次连接请求，不是「续上原来的会话」
//!
//! 会话凭证（`session_id` + 一次性 token）由服务端签发且**用后即焚**（重放会被拒，见
//! `rust_tunnel_remote::auth`），没有「原样接回」的可能。所以这里的重连就是重新走一遍控制面
//! 流程（[`rust_tunnel_remote::RemoteManager::request_connect`]）：服务端重新签发凭证，被控端
//! 会**再次**经过接入确认。后者是设计使然——「拒绝是引擎强制的」，确认策略就是「允许被请求」
//! 而非「永久放行」，故重连被打扰的是被控端用户，这是本功能已知的代价。
//!
//! # 判据：会话循环观测到的**失败**结束才重连
//!
//! 判据是 [`SessionEnd::deliberate`]，而它的取值来自会话循环实际看到了什么，不是猜的：
//!
//! - 数据面 EOF / 编解码错误 → `deliberate == false`（**触发重连**）——拔网线、关掉对端进程都属于
//!   这一类，也正是本功能要解决的场景；
//! - 收到对端 `Bye`（对端用户点了「中断」/退出）→ `deliberate == true`，**不**重连：对端已经
//!   明确表示不继续了，再去拨一次只会给对方弹一个没人要的确认框；
//! - 本端自己收尾（点中断 / 关窗 / 退出应用）→ 引擎根本不写 `SessionEnd`（见
//!   `rust_tunnel_remote` 里 `RemoteUiState::session_end` 的契约），自然也谈不上重连。
//!
//! 调用方还须再比一次 `session_id`（见 [`crate::remote::RemoteView::should_auto_reconnect`]）：
//! 本端也可能**被别人控制**，那种会话断线不该由本端去拨号。
//!
//! [`SessionEnd::deliberate`]: rust_tunnel_remote::SessionEnd

use std::time::{Duration, Instant};

use rust_tunnel_common::protocol::RemoteMode;

/// 重试退避（秒）；下标 = 已完成的尝试次数。**长度即最大尝试次数**。
///
/// 末项之后不再等待（见 [`ReconnectState::poll`]）：最后一次尝试失败即放弃，避免在一个
/// 显然连不上的对端上无限打扰对方。
pub const BACKOFF_SECS: [u64; 5] = [2, 4, 8, 16, 30];

/// 最大尝试次数（= 退避表长度）。
#[must_use]
pub fn max_attempts() -> u32 {
    u32::try_from(BACKOFF_SECS.len()).unwrap_or(u32::MAX)
}

/// 重连的目标：用户上次点的那个对端与模式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectTarget {
    /// 目标客户端名（`request_connect` 的 `target_client`）。
    pub client_name: String,
    /// 请求的模式（完全控制 / 仅观看 / 仅文件传输）。
    pub mode: RemoteMode,
}

/// 状态机推进结果（每帧取一次，驱动 UI 与实际的 `request_connect`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconnectStep {
    /// 未在重连。
    Idle,
    /// 退避等待中；`remaining` 用于倒计时显示，`attempt` 从 1 开始。
    Waiting {
        /// 即将进行的这一次是第几次尝试。
        attempt: u32,
        /// 总尝试次数。
        total: u32,
        /// 距下次重试还剩多久。
        remaining: Duration,
    },
    /// 现在就该发起一次重连（调用方执行 `request_connect` 后继续等下一次）。
    Retry {
        /// 本次是第几次尝试（从 1 开始）。
        attempt: u32,
        /// 总尝试次数。
        total: u32,
        /// 重连目标。
        target: ReconnectTarget,
    },
    /// 次数用尽，已放弃（UI 应提示并停止倒计时）。
    Exhausted {
        /// 一共尝试过多少次。
        total: u32,
    },
}

/// 断线自动重连状态。
///
/// 生命周期：用户在「远程」Tab 点「连接」→ [`Self::note_connect`] 记下目标 → 会话若因断线结束
/// → [`Self::on_session_end`] 启动重试 → 每帧 [`Self::poll`] 推进 → 会话重新建立时
/// [`Self::cancel`] 收尾。
#[derive(Debug, Default)]
pub struct ReconnectState {
    /// 上次由用户发起的连接目标；`None` 表示没有可重连的对象。
    target: Option<ReconnectTarget>,
    /// 已发起的重试次数（0 = 还没重试过）。
    attempts: u32,
    /// 下次重试的时刻；`None` = 未在重连。
    due_at: Option<Instant>,
}

impl ReconnectState {
    /// 记下用户主动发起的连接目标（每次「连接」都应调用，重置计数）。
    pub fn note_connect(&mut self, target: ReconnectTarget) {
        self.target = Some(target);
        self.attempts = 0;
        self.due_at = None;
    }

    /// 会话结束事件。`deliberate` 为 true（本端或对端主动结束）时不启动重连。
    ///
    /// 返回是否**新启动**了一轮重试。调用方必须按 `session_id` 去重——同一场会话结束只应喂一次，
    /// 否则每帧都会把计数清零、退避永不生效。
    pub fn on_session_end(&mut self, deliberate: bool, now: Instant) -> bool {
        if deliberate || self.target.is_none() {
            return false;
        }
        if self.is_active() {
            return false; // 已经在一轮重试里了，不叠加
        }
        self.attempts = 0;
        self.due_at = Some(now + backoff(0));
        true
    }

    /// 推进状态机（每帧调用一次）。
    pub fn poll(&mut self, now: Instant) -> ReconnectStep {
        let Some(due) = self.due_at else {
            return ReconnectStep::Idle;
        };
        let total = max_attempts();
        if self.attempts >= total {
            // 退避表用尽：最后一次尝试已给出，不再等待。
            self.due_at = None;
            return ReconnectStep::Exhausted { total };
        }
        if now < due {
            return ReconnectStep::Waiting {
                attempt: self.attempts + 1,
                total,
                remaining: due.saturating_duration_since(now),
            };
        }
        self.attempts += 1;
        // 末次之后把 due 推到「刚好过期」，让下一次 poll 走 Exhausted 分支收尾；
        // 中间各次则按退避表继续等。这样「最后一次尝试」也有一整个退避窗口去成功。
        self.due_at = Some(if self.attempts >= total {
            now
        } else {
            now + backoff(self.attempts)
        });
        match self.target.clone() {
            Some(target) => ReconnectStep::Retry {
                attempt: self.attempts,
                total,
                target,
            },
            // 理论上不可达（`on_session_end` 要求已有目标），但状态机不该假设调用序。
            None => ReconnectStep::Idle,
        }
    }

    /// 用户点「立即重试」：跳过退避，直接给出目标（次数用尽时返回 `None`）。
    ///
    /// 次数用尽时**不动状态**，让紧接着的 [`Self::poll`] 照常报 `Exhausted`——两条路径
    /// （自动退避 / 手动催）收敛到同一个可观察状态，UI 只需处理一种。
    pub fn retry_now(&mut self, now: Instant) -> Option<ReconnectTarget> {
        let target = self.target.clone()?;
        if self.attempts >= max_attempts() {
            return None;
        }
        self.attempts += 1;
        self.due_at = Some(if self.attempts >= max_attempts() {
            now
        } else {
            now + backoff(self.attempts)
        });
        Some(target)
    }

    /// 停止重试（用户点「停止」，或会话已重新建立）。**保留目标**，下次断线仍会自动重连。
    pub fn cancel(&mut self) {
        self.attempts = 0;
        self.due_at = None;
    }

    /// 是否正在重试（UI 显示用）。
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.due_at.is_some()
    }
}

/// 第 `attempts` 次尝试之后应等待的时长（下标越界时取末项）。
fn backoff(attempts: u32) -> Duration {
    let idx = usize::try_from(attempts).unwrap_or(usize::MAX).min(BACKOFF_SECS.len() - 1);
    Duration::from_secs(BACKOFF_SECS[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ReconnectTarget {
        ReconnectTarget {
            client_name: "home-nas".to_owned(),
            mode: RemoteMode::Control,
        }
    }

    #[test]
    fn no_retry_without_a_recorded_target() {
        let mut state = ReconnectState::default();
        let t0 = Instant::now();
        assert!(!state.on_session_end(false, t0));
        assert_eq!(state.poll(t0 + Duration::from_secs(60)), ReconnectStep::Idle);
    }

    #[test]
    fn deliberate_ends_never_retry() {
        let mut state = ReconnectState::default();
        state.note_connect(target());
        let t0 = Instant::now();
        assert!(!state.on_session_end(true, t0), "用户主动中断不该触发重连");
        assert!(!state.is_active());
        assert_eq!(state.poll(t0 + Duration::from_secs(60)), ReconnectStep::Idle);
    }

    #[test]
    fn backoff_then_retry_then_exhaust() {
        let mut state = ReconnectState::default();
        state.note_connect(target());
        let t0 = Instant::now();
        assert!(state.on_session_end(false, t0));
        assert!(state.is_active());

        // 第一次尝试前要等 BACKOFF_SECS[0]。
        assert_eq!(
            state.poll(t0),
            ReconnectStep::Waiting {
                attempt: 1,
                total: 5,
                remaining: Duration::from_secs(2),
            }
        );

        let mut now = t0;
        let mut retries = 0;
        for expected in 1..=5 {
            now += Duration::from_secs(40); // 足够跨过任何一次退避
            match state.poll(now) {
                ReconnectStep::Retry { attempt, total, target: t } => {
                    assert_eq!(attempt, expected);
                    assert_eq!(total, 5);
                    assert_eq!(t, target());
                    retries += 1;
                }
                other => panic!("第 {expected} 次应当重试，实际 {other:?}"),
            }
        }
        assert_eq!(retries, 5);
        // 用尽之后不再无限重试。
        now += Duration::from_secs(120);
        assert_eq!(state.poll(now), ReconnectStep::Exhausted { total: 5 });
        assert!(!state.is_active());
        assert_eq!(state.poll(now), ReconnectStep::Idle, "放弃后归于静止");
    }

    #[test]
    fn backoff_actually_grows_between_attempts() {
        let mut state = ReconnectState::default();
        state.note_connect(target());
        let t0 = Instant::now();
        state.on_session_end(false, t0);
        // 恰好在第一次退避边界上：应当立刻重试。
        let t1 = t0 + Duration::from_secs(2);
        assert!(matches!(state.poll(t1), ReconnectStep::Retry { attempt: 1, .. }));
        // 紧接着的一帧不该再重试（第二次要等 4s）。
        assert!(matches!(
            state.poll(t1),
            ReconnectStep::Waiting { attempt: 2, .. }
        ));
        assert!(matches!(
            state.poll(t1 + Duration::from_secs(3)),
            ReconnectStep::Waiting { attempt: 2, .. }
        ));
        assert!(matches!(
            state.poll(t1 + Duration::from_secs(4)),
            ReconnectStep::Retry { attempt: 2, .. }
        ));
    }

    #[test]
    fn session_end_while_already_retrying_does_not_reset() {
        let mut state = ReconnectState::default();
        state.note_connect(target());
        let t0 = Instant::now();
        assert!(state.on_session_end(false, t0));
        let _ = state.poll(t0 + Duration::from_secs(2)); // 第 1 次
        assert!(!state.on_session_end(false, t0 + Duration::from_secs(3)), "不叠加");
        assert!(matches!(
            state.poll(t0 + Duration::from_secs(6)),
            ReconnectStep::Retry { attempt: 2, .. }
        ));
    }

    #[test]
    fn retry_now_skips_the_wait() {
        let mut state = ReconnectState::default();
        state.note_connect(target());
        let t0 = Instant::now();
        state.on_session_end(false, t0);
        assert_eq!(state.retry_now(t0), Some(target()));
        assert!(matches!(
            state.poll(t0),
            ReconnectStep::Waiting { attempt: 2, .. }
        ));
    }

    #[test]
    fn retry_now_stops_once_exhausted() {
        let mut state = ReconnectState::default();
        state.note_connect(target());
        let t0 = Instant::now();
        state.on_session_end(false, t0);
        for _ in 0..max_attempts() {
            assert!(state.retry_now(t0).is_some());
        }
        assert_eq!(state.retry_now(t0), None);
        assert_eq!(state.poll(t0 + Duration::from_secs(60)), ReconnectStep::Exhausted { total: 5 });
    }

    #[test]
    fn cancel_stops_but_keeps_the_target_for_the_next_drop() {
        let mut state = ReconnectState::default();
        state.note_connect(target());
        let t0 = Instant::now();
        state.on_session_end(false, t0);
        state.cancel();
        assert!(!state.is_active());
        assert_eq!(state.poll(t0 + Duration::from_secs(60)), ReconnectStep::Idle);
        // 下一次断线仍会自动重连（目标还在）。
        assert!(state.on_session_end(false, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn note_connect_resets_the_counter() {
        let mut state = ReconnectState::default();
        state.note_connect(target());
        let t0 = Instant::now();
        state.on_session_end(false, t0);
        for _ in 0..max_attempts() {
            state.retry_now(t0);
        }
        assert_eq!(state.poll(t0 + Duration::from_secs(60)), ReconnectStep::Exhausted { total: 5 });
        // 用户手动再连一次 → 计数重置，下一次断线从头开始重试。
        state.note_connect(target());
        assert!(state.on_session_end(false, t0 + Duration::from_secs(120)));
        assert!(matches!(
            state.poll(t0 + Duration::from_secs(122)),
            ReconnectStep::Retry { attempt: 1, .. }
        ));
    }
}
