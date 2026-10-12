//! 会话循环 → 管理器的统计数据上报。
//!
//! 会话循环跑在数据面任务里，管理器持有 UI 状态；两者不能直接互调（循环不该依赖管理器，
//! 管理器也不该被数据面拖住）。这里用一个单向通道解耦：循环通过 [`SessionStatsReporter`]
//! 上报，管理器侧的任务把事件泵进 [`crate::manager::RemoteUiState`]。
//!
//! **不是控制协议的一部分**：数据面的 `SessionMessage` 变体一个都不加（新增变体会让混合
//! 版本的一端 bincode 解码失败而硬断链），这里只在**进程内**传递，不上线。

use std::time::Duration;
use tokio::sync::mpsc;

/// 会话循环 → 管理器的统计数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStats {
    /// 一次心跳往返测得的 RTT（复用既有 `Heartbeat` → `HeartbeatAck`，不新增数据面变体）。
    Rtt(Duration),
}

/// 上报句柄。
///
/// 用 [`SessionStatsReporter::noop`] 构造的实例 [`SessionStatsReporter::report`] 静默丢弃；
/// 接收端已 drop 时同样静默丢弃——统计是**尽力而为**的观测数据，任何一端缺席都不该影响会话。
#[derive(Debug, Clone)]
pub struct SessionStatsReporter(Option<mpsc::UnboundedSender<SessionStats>>);

impl SessionStatsReporter {
    /// 造一个静默丢弃的上报句柄（无管理端时使用，例如单测与只跑会话循环的场景）。
    #[must_use]
    pub fn noop() -> Self {
        Self(None)
    }

    /// 以通道发送端构造。
    #[must_use]
    pub fn new(tx: mpsc::UnboundedSender<SessionStats>) -> Self {
        Self(Some(tx))
    }

    /// 上报一条事件；接收端已 drop 时静默丢弃。
    pub fn report(&self, ev: SessionStats) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_reporter_discards_silently() {
        // 无接收端也不该 panic。
        SessionStatsReporter::noop().report(SessionStats::Rtt(Duration::from_millis(12)));
    }

    #[test]
    fn dropping_receiver_does_not_panic() {
        let (tx, rx) = mpsc::unbounded_channel();
        let reporter = SessionStatsReporter::new(tx);
        reporter.report(SessionStats::Rtt(Duration::from_millis(1)));
        drop(rx);
        // 接收端已 drop：静默丢弃，不 panic。
        reporter.report(SessionStats::Rtt(Duration::from_millis(2)));
    }
}
