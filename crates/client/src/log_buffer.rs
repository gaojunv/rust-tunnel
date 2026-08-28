//! 日志环形缓冲：供桌面托盘的日志面板展示最近日志。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rust_tunnel_common::protocol::ClientLogEntry;

/// 环形缓冲，默认容量 500 条，线程安全。
#[derive(Debug, Clone)]
pub struct LogBuffer {
    entries: Arc<Mutex<VecDeque<ClientLogEntry>>>,
    cap: usize,
}

impl LogBuffer {
    /// 创建指定容量的环形缓冲。
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(VecDeque::with_capacity(cap))),
            cap,
        }
    }

    /// 创建默认容量（500）的缓冲。
    #[must_use]
    pub fn with_default_cap() -> Self {
        Self::new(500)
    }

    /// 推入一条日志，超出容量时丢弃最旧的。
    pub fn push(&self, entry: ClientLogEntry) {
        if let Ok(mut guard) = self.entries.lock() {
            if guard.len() >= self.cap {
                guard.pop_front();
            }
            guard.push_back(entry);
        }
    }

    /// 返回最近 `n` 条日志（按时间正序，最旧在前）。
    #[must_use]
    pub fn recent(&self, n: usize) -> Vec<ClientLogEntry> {
        if let Ok(guard) = self.entries.lock() {
            let len = guard.len();
            let start = len.saturating_sub(n);
            guard.iter().skip(start).cloned().collect()
        } else {
            Vec::new()
        }
    }

    /// 返回最近 `n` 条日志（`recent_logs` 为规范名，`recent` 为别名）。
    #[must_use]
    pub fn recent_logs(&self, n: usize) -> Vec<ClientLogEntry> {
        self.recent(n)
    }

    /// 返回全部日志（按时间正序）。
    #[must_use]
    pub fn all(&self) -> Vec<ClientLogEntry> {
        self.recent(self.cap)
    }

    /// 当前条数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |g| g.len())
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 清空缓冲。
    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::with_default_cap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(msg: &str) -> ClientLogEntry {
        ClientLogEntry {
            timestamp: 1,
            level: "INFO".into(),
            target: "test".into(),
            message: msg.into(),
        }
    }

    #[test]
    fn test_push_and_recent() {
        let buf = LogBuffer::new(3);
        buf.push(entry("a"));
        buf.push(entry("b"));
        buf.push(entry("c"));
        assert_eq!(buf.len(), 3);
        let r = buf.recent(2);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].message, "b");
        assert_eq!(r[1].message, "c");
    }

    #[test]
    fn test_cap_overflow() {
        let buf = LogBuffer::new(2);
        buf.push(entry("a"));
        buf.push(entry("b"));
        buf.push(entry("c"));
        assert_eq!(buf.len(), 2);
        let r = buf.all();
        assert_eq!(r[0].message, "b");
        assert_eq!(r[1].message, "c");
    }

    #[test]
    fn test_clear() {
        let buf = LogBuffer::new(5);
        buf.push(entry("a"));
        buf.push(entry("b"));
        assert!(!buf.is_empty());
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn test_default_cap() {
        let buf = LogBuffer::default();
        assert_eq!(buf.cap, 500);
    }
}
