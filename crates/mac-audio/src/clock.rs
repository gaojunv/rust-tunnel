//! 进程内单调时钟（微秒）。
//!
//! 与 `rust-tunnel-remote::clock` 的口径一致：只用于**观测**（采集时刻/端到端延迟），
//! 不参与任何控制决策，也不是跨进程可比的墙钟。

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// 进程启动以来的微秒数。
pub(crate) fn now_us() -> u64 {
    let start = START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}
