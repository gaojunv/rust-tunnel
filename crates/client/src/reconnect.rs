//! 重连策略：指数退避与可重连判定。

use rust_tunnel_common::TunnelError;

/// 指数退避重连策略。
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    backoff_secs: u64,
    max_backoff_secs: u64,
}

impl ReconnectPolicy {
    /// 创建默认策略：初始 1s，最大 30s。
    #[must_use]
    pub fn new() -> Self {
        Self {
            backoff_secs: 1,
            max_backoff_secs: 30,
        }
    }

    /// 判断错误是否应该重连；认证失败或注册被拒不重连。
    #[must_use]
    pub fn should_reconnect(err: &TunnelError) -> bool {
        let msg = format!("{err}").to_lowercase();
        if msg.contains("authentication failed") {
            return false;
        }
        if let TunnelError::ControlChannel(s) = err {
            if s.contains("register failed") {
                return false;
            }
        }
        // 兜底：经 Display 包装的 WithSource 等也可能含 register failed 文案
        if msg.contains("register failed") {
            return false;
        }
        true
    }

    /// 返回当前退避时长并将内部计数翻倍（上限 `max_backoff_secs`）。
    pub fn next_backoff(&mut self) -> u64 {
        let cur = self.backoff_secs;
        self.backoff_secs = (self.backoff_secs * 2).min(self.max_backoff_secs);
        cur
    }

    /// 重置退避到初始值。
    pub fn reset(&mut self) {
        self.backoff_secs = 1;
    }

    /// 当前退避值（不推进）。
    #[must_use]
    pub fn current(&self) -> u64 {
        self.backoff_secs
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_err() -> TunnelError {
        TunnelError::ControlChannel("register failed: authentication failed".into())
    }

    fn other_register_err() -> TunnelError {
        TunnelError::ControlChannel("register failed: server busy".into())
    }

    #[test]
    fn test_should_not_reconnect_on_auth_failed() {
        assert!(!ReconnectPolicy::should_reconnect(&auth_err()));
    }

    #[test]
    fn test_should_reconnect_on_other_register_failed() {
        // 按规范：任何 register failed 均视为不可重连（需用户修配置）
        assert!(!ReconnectPolicy::should_reconnect(&other_register_err()));
    }

    #[test]
    fn test_should_reconnect_on_io_error() {
        let err = TunnelError::Io(std::io::Error::other("reset"));
        assert!(ReconnectPolicy::should_reconnect(&err));
    }

    #[test]
    fn test_backoff_doubles_and_caps() {
        let mut p = ReconnectPolicy::new();
        assert_eq!(p.next_backoff(), 1);
        assert_eq!(p.next_backoff(), 2);
        assert_eq!(p.next_backoff(), 4);
        assert_eq!(p.next_backoff(), 8);
        assert_eq!(p.next_backoff(), 16);
        assert_eq!(p.next_backoff(), 30);
        assert_eq!(p.next_backoff(), 30);
    }

    #[test]
    fn test_reset() {
        let mut p = ReconnectPolicy::new();
        p.next_backoff();
        p.next_backoff();
        p.reset();
        assert_eq!(p.current(), 1);
    }
}
