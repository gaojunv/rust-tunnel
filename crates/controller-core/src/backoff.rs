//! 控制通道重连退避。
//!
//! 与 `rust-tunnel-client` 的 `ReconnectPolicy` 语义一致（初始 1s、每次翻倍、封顶
//! 30s），但在这里独立实现，便于无头宿主按自己的节拍测试与调整（例如 Android
//! 在 Doze 模式下希望更长的封顶值）。

use std::time::Duration;

/// 默认初始退避。
pub const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// 默认最大退避。
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// 指数退避计数器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    /// 第一次等待时长。
    initial: Duration,
    /// 封顶时长。
    max: Duration,
    /// 下一次要返回的时长。
    next: Duration,
}

impl Backoff {
    /// 默认策略：1s 起步、30s 封顶。
    #[must_use]
    pub fn new() -> Self {
        Self::with_bounds(DEFAULT_INITIAL_BACKOFF, DEFAULT_MAX_BACKOFF)
    }

    /// 自定义起步与封顶时长（`initial` 大于 `max` 时收敛到 `max`）。
    #[must_use]
    pub fn with_bounds(initial: Duration, max: Duration) -> Self {
        let initial = if initial.is_zero() {
            Duration::from_millis(1)
        } else {
            initial
        };
        let initial = initial.min(max);
        Self {
            initial,
            max,
            next: initial,
        }
    }

    /// 返回当前退避时长，并把下一次翻倍（不超过封顶）。
    pub fn next_delay(&mut self) -> Duration {
        let current = self.next;
        self.next = (self.next * 2).min(self.max);
        current
    }

    /// 重置到初始值（一次成功连接后调用）。
    pub fn reset(&mut self) {
        self.next = self.initial;
    }

    /// 当前退避值（不推进）。
    #[must_use]
    pub fn current(&self) -> Duration {
        self.next
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_backoff_doubles_and_caps() {
        let mut b = Backoff::new();
        let seq: Vec<u64> = (0..7).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 30, 30]);
    }

    #[test]
    fn reset_restores_initial() {
        let mut b = Backoff::new();
        b.next_delay();
        b.next_delay();
        assert_eq!(b.current().as_secs(), 4);
        b.reset();
        assert_eq!(b.current().as_secs(), 1);
        assert_eq!(b.next_delay().as_secs(), 1);
    }

    #[test]
    fn custom_bounds_are_respected() {
        let mut b = Backoff::with_bounds(Duration::from_millis(500), Duration::from_secs(2));
        assert_eq!(b.next_delay(), Duration::from_millis(500));
        assert_eq!(b.next_delay(), Duration::from_secs(1));
        assert_eq!(b.next_delay(), Duration::from_secs(2));
        assert_eq!(b.next_delay(), Duration::from_secs(2));
    }

    #[test]
    fn zero_initial_is_clamped_and_initial_above_max_converges() {
        let mut b = Backoff::with_bounds(Duration::ZERO, Duration::from_secs(4));
        assert_eq!(b.next_delay(), Duration::from_millis(1));

        let mut b = Backoff::with_bounds(Duration::from_secs(10), Duration::from_secs(4));
        assert_eq!(b.next_delay(), Duration::from_secs(4));
    }
}
