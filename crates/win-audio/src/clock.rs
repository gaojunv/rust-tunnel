//! 进程内单调时钟（微秒）。
//!
//! 与 `rust-tunnel-remote::clock` 的口径一致：只用于**观测**（采集时刻/端到端延迟），
//! 不参与任何控制决策，也不是跨进程可比的墙钟。两端各自从进程启动计时，控制器用
//! NTP 式四时间戳估计偏差（视频已有那套；音频当前只做本地统计）。

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// 进程启动以来的微秒数。
#[must_use]
pub fn now_us() -> u64 {
    let start = START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}
