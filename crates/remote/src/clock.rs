//! 端到端延迟所需的**单调时钟**与**跨端时钟偏差**估计。
//!
//! # 为什么需要它
//!
//! 统计角标要显示「采集 → 解码完成」的端到端延迟（D2）。被控端在采集完成时打一个时间戳
//! （[`now_us`]），控制器在解码完成时打另一个——但两者的时钟起点完全不同（各自进程启动的
//! 时刻），直接相减毫无意义。于是用心跳做一次 NTP 式**四时间戳**估计：
//!
//! ```text
//! 控制器                     被控端
//!   t1 ── Heartbeat ──────►
//!                             t2（收到）
//!   ◄──── HeartbeatAck ───── t3（发出）
//!   t4（收到）
//! ```
//!
//! `offset = ((t2 - t1) + (t3 - t4)) / 2`，正值表示**对端时钟比本端快**（它的读数更大）。
//! 控制器随后用 `本端 now_us + offset` 换到对端时间轴，再减 `capture_ts_us` 即得端到端延迟。
//!
//! # 为什么要 `i128` 中间量
//!
//! 两个 `u64`（微秒，进程内单调时钟可以轻易跑到 10^15 量级）相减会溢出，而 `t3 - t4` 天然为负。
//! 用 `i128` 做中间量再饱和回 `i64`，任何输入都不会 panic 或产生错误的正负号。
//!
//! # 这是观测值，不是控制信号
//!
//! 时钟偏差与端到端延迟**只进 UI/日志**：它们不参与 QoS 闭环（那条闭环只看心跳 RTT 与
//! `QosFeedback`），也绝不参与鉴权或密钥派生。

use std::sync::OnceLock;
use std::time::Instant;

/// 进程内单调时钟的起点（首次调用 [`now_us`] 的时刻）。
fn origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// 本进程的单调时钟读数（微秒，自 [`now_us`] 首次被调用起）。
///
/// **单调**：不受系统时间调整/NTP 跳变影响（底层是 `Instant`），可安全用于跨端时间差。
/// 溢出（进程跑满约 58 万年）时饱和到 `u64::MAX`，不 panic。
///
/// `Hello` / `HelloAck` / `DisplayInfo` 的字节形状受冻结保护，所以这个读数只出现在 v8 新增的
/// 心跳时间戳与 `capture_ts_us` 字段里。
#[must_use]
pub fn now_us() -> u64 {
    u64::try_from(origin().elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// 由 NTP 式四时间戳估计**对端时钟相对本端的偏差**（微秒）。
///
/// `offset = ((t2 - t1) + (t3 - t4)) / 2`，**正值 = 对端时钟更快**（读数更大）。
///
/// - `t1`：本端发出 `Heartbeat` 的时刻；
/// - `t2`：对端收到它的时刻（对端时钟）；
/// - `t3`：对端发出 `HeartbeatAck` 的时刻（对端时钟）；
/// - `t4`：本端收到 ack 的时刻。
///
/// 用 `i128` 做中间量：`t2 - t1` 与 `t3 - t4` 都可能为负，且 `u64` 直接相减会溢出。结果超出
/// `i64` 范围时饱和（现实中不可能，除非对端送垃圾时间戳）。
///
/// 注意：公式假设上下行路径对称，链路的**排队不对称**会直接变成偏差误差（这是 NTP 的固有
/// 局限）。因此调用方用**最小 RTT 的样本**做基准，再对其余样本做 EMA 平滑。
#[must_use]
pub fn clock_offset_us(t1: u64, t2: u64, t3: u64, t4: u64) -> i64 {
    let forward = i128::from(t2) - i128::from(t1);
    let backward = i128::from(t3) - i128::from(t4);
    // 先求和再除（与公式逐字一致：向零截断），而不是 `midpoint`（它向负无穷取整）。
    // 两者只在和为负奇数时差 1 µs，但公式是规格的一部分，保持可逐字核对。
    let sum = forward + backward;
    let half = sum / 2;
    i64::try_from(half).unwrap_or(if half.is_negative() { i64::MIN } else { i64::MAX })
}

/// 把「本端时刻 + 时钟偏差」换成对端时间轴后，算一个时间戳距今多少毫秒。
///
/// 语义与控制器解码任务里的端到端计算**完全一致**（同一份代码）：
///
/// - `offset_us == i64::MIN`（[`UNKNOWN_OFFSET`]）：偏差未知，返回 `0`；
/// - `capture_ts_us == 0`：对端没填时间戳，返回 `0`（不拿它当「进程刚启动」去算一个巨大值）；
/// - 结果早于当前时刻（负差，可能来自偏差误差/乱序）：饱和到 `0`。
///
/// UI 在绘制时用它算「采集 → 上屏」：`elapsed_ms(now_us(), offset, frame.capture_ts_us)`。
#[must_use]
pub fn elapsed_ms(now_us: u64, offset_us: i64, capture_ts_us: u64) -> u32 {
    if offset_us == UNKNOWN_OFFSET || capture_ts_us == 0 {
        return 0;
    }
    let peer_now = i128::from(now_us) + i128::from(offset_us);
    let delta = peer_now - i128::from(capture_ts_us);
    if delta <= 0 {
        return 0;
    }
    u32::try_from((delta / 1000).min(i128::from(u32::MAX))).unwrap_or(u32::MAX)
}

/// 「时钟偏差未知」的哨兵值。
///
/// `AtomicI64` 没有 `Option`，故用一个**不可能出现的偏差**表示「尚未采到可信样本」。真实偏差
/// 是两端进程启动时刻之差，量级在 ±(进程寿命)，绝不可能恰好等于 `i64::MIN`。
pub const UNKNOWN_OFFSET: i64 = i64::MIN;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_us_is_monotonic_and_nonzero() {
        // 首次调用就是起点本身，读数可能恰为 0（elapsed < 1µs），不能断言「立即 > 0」。
        let a = now_us();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let b = now_us();
        assert!(b >= a, "单调时钟不得回退");
        assert!(b >= 1_000, "毫秒级等待后读数应至少增长 1000 µs，实得 {b}");
    }

    #[test]
    fn offset_is_zero_for_symmetric_path() {
        // t1=1000 → t2=2000（去程 1ms），t3=2000 → t4=3000（返程 1ms），两端钟一样。
        assert_eq!(clock_offset_us(1_000, 2_000, 2_000, 3_000), 0);
        // 完全无延迟的往返。
        assert_eq!(clock_offset_us(0, 0, 0, 0), 0);
        assert_eq!(clock_offset_us(5, 5, 5, 5), 0);
    }

    #[test]
    fn offset_positive_when_peer_clock_is_fast() {
        // 对端快 1000 µs：t2/t3 都多 1000。
        // offset = ((2000-1000) + (2000-3000)) / 2 = (1000 - 1000)/2 = 0 ... →
        // 构造更直观的一组：本端 t1=0,t4=2000（RTT 2ms）；对端 t2=1500,t3=1500（快 500）。
        // offset = ((1500-0) + (1500-2000))/2 = (1500 - 500)/2 = 500。
        assert_eq!(clock_offset_us(0, 1_500, 1_500, 2_000), 500);
    }

    #[test]
    fn offset_negative_when_peer_clock_is_slow() {
        // 对端慢 500：t2/t3 比本端时间轴少 500。
        // offset = ((500-0) + (500-1000))/2 = (500 - 500)/2 = 0 → 换一组非对称的。
        // 本端 t1=0, t4=2000；对端 t2=400, t3=900（去程 400+慢 900？直接套公式）：
        // offset = ((400-0) + (900-2000))/2 = (400 - 1100)/2 = -350。
        assert_eq!(clock_offset_us(0, 400, 900, 2_000), -350);
    }

    #[test]
    fn offset_handles_asymmetric_paths() {
        // 去程 3ms、返程 1ms，两端钟相同：t1=0, t4=4000, t2=t3=3000。
        // 公式给出 (3000 + (3000-4000))/2 = 2000/2 = 1000 —— 这正是「排队不对称」造成的
        // 已知误差（偏大的那一半），不是 bug：调用方靠「最小 RTT 样本优先」把它压到最小。
        assert_eq!(clock_offset_us(0, 3_000, 3_000, 4_000), 1_000);
        // 反过来的不对称给出相反符号。
        assert_eq!(clock_offset_us(0, 1_000, 1_000, 4_000), -1_000);
    }

    #[test]
    fn offset_survives_huge_values_and_never_panics() {
        // u64 上限附近的读数：中间量必须是 i128，直接相减会溢出 panic（debug）或回绕。
        let offset = clock_offset_us(0, u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(offset, i64::try_from(u64::MAX / 2).unwrap());
        // 极端负向：饱和到 i64::MIN，不 panic。
        assert_eq!(clock_offset_us(u64::MAX, 0, 0, u64::MAX), i64::MIN);
    }

    #[test]
    fn elapsed_ms_saturates_and_rejects_unknown() {
        // 偏差未知、时间戳缺失：一律 0。
        assert_eq!(elapsed_ms(10_000, UNKNOWN_OFFSET, 1_000), 0);
        assert_eq!(elapsed_ms(10_000, 0, 0), 0);
        // 正常：本端 10ms + 偏差 5ms = 对端 15ms，采集于 12ms → 3ms。
        assert_eq!(elapsed_ms(10_000, 5_000, 12_000), 3);
        // 负差饱和到 0（偏差误差/乱序）。
        assert_eq!(elapsed_ms(10_000, 0, 20_000), 0);
        // 亚毫秒截断。
        assert_eq!(elapsed_ms(10_000, 0, 9_100), 0);
        assert_eq!(elapsed_ms(10_000, 0, 8_999), 1);
        // 巨大差值饱和到 u32::MAX。
        assert_eq!(elapsed_ms(u64::MAX / 2, 0, 1), u32::MAX);
    }
}
