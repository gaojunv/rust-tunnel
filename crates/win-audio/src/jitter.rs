//! Windows 远程音频播放侧的**抖动缓冲**：把网络侧收到的 Opus 帧按序号重排、
//! 按固定 20 ms 节拍交给解码器，并在丢包 / 欠载时给出明确信号。
//!
//! 这里只有纯逻辑：不碰 WASAPI、不碰 Opus、不依赖任何外部 crate。时间由调用方
//! 传入（生产里是 `Instant::now()`，单测里是显式推进的时间点），因此整个模块
//! 可以在任意平台单测。
//!
//! # 设计要点
//!
//! - **自适应延迟**：起播先缓冲 `target_ms`；欠载一次上调一档（封顶 `max_ms`），
//!   连续 8 秒无欠载再回落一档（不低于 `min_ms`）。
//! - **重排有界**：缓冲最多 `max_depth` 帧，溢出时从最旧序号开始删到
//!   `target_ms / 20` 帧——重排窗口不能无界增长。
//! - **丢包不等于卡顿**：中间序号缺失时 [`Pull::Missing`] 让调用方做 Opus PLC
//!   （丢包隐藏），播放时间轴照常前进。
//! - **长时间无数据即重新起播**：距上次 `push` 超过 500 ms 时清空旧帧、重置
//!   `next_seq` 与起播时刻，避免一恢复就把积压的陈旧帧一股脑吐出来。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// 一帧 20 ms（与 `crate::format::FRAME_MS` 一致）。
pub const FRAME_MS: u32 = 20;

/// 距上次 `push` 超过这个间隔就视为「重新开始」。
const RESET_GAP: Duration = Duration::from_millis(500);

/// 无欠载持续这么久，`target` 回落一档。
const RELAX_WINDOW: Duration = Duration::from_millis(8_000);

/// 默认起播缓冲（毫秒）。
const DEFAULT_TARGET_MS: u32 = 80;
/// 默认缓冲下限（毫秒）。
const DEFAULT_MIN_MS: u32 = 60;
/// 默认缓冲上限（毫秒）。
const DEFAULT_MAX_MS: u32 = 120;
/// 默认最大缓冲深度（帧，25 × 20 ms = 500 ms）。
const DEFAULT_MAX_DEPTH: usize = 25;

/// [`JitterBuffer::pull`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pull {
    /// 取到一帧编码数据，交给 Opus 解码。
    Packet(Vec<u8>),
    /// 该序号缺失（发送侧队列满丢包）：调用方应做 Opus PLC。
    Missing,
    /// 还没到播放时刻，或缓冲为空（刚起播 / 长时间无数据）。
    Idle,
}

/// [`JitterBuffer::push`] 的结果（统计用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// 已进入缓冲。
    Stored,
    /// 序号已过期（小于下一次要播的序号），丢弃。
    Late,
    /// 缓冲溢出，已按策略丢掉最旧的若干帧。
    Trimmed,
}

/// 抖动缓冲配置。默认 target 80 ms / min 60 / max 120，最大深度 25 帧（500 ms）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterConfig {
    /// 起播缓冲深度（毫秒），随欠载 / 健康播放自适应调整。
    pub target_ms: u32,
    /// 自适应延迟的下限（毫秒）。
    pub min_ms: u32,
    /// 自适应延迟的上限（毫秒）。
    pub max_ms: u32,
    /// 缓冲内最多保留的帧数（重排窗口的硬上限）。
    pub max_depth: usize,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            target_ms: DEFAULT_TARGET_MS,
            min_ms: DEFAULT_MIN_MS,
            max_ms: DEFAULT_MAX_MS,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

/// 按序号重排、按固定节拍出帧的抖动缓冲。
#[derive(Debug)]
pub struct JitterBuffer {
    config: JitterConfig,
    /// 已到达但还没播出的帧，按序号排序；键恒 ≥ `next_seq`。
    packets: BTreeMap<u64, Vec<u8>>,
    /// 下一次要播的序号。
    next_seq: u64,
    /// 下一次允许出帧的时刻；`None` 表示尚未起播。
    play_at: Option<Instant>,
    /// 最近一次 `push` 的时刻，用于识别「长时间无数据后重新开始」。
    last_push_at: Option<Instant>,
    /// 最近一次欠载（或起播）的时刻，用作 target 回落窗口的起点。
    last_underrun_at: Option<Instant>,
    /// 当前生效的起播缓冲（毫秒）。
    target_ms: u32,
    underruns: u64,
    late_dropped: u64,
    overflow_dropped: u64,
}

impl JitterBuffer {
    /// 按配置创建一个空缓冲（尚未起播）。
    #[must_use]
    pub fn new(config: JitterConfig) -> Self {
        Self {
            config,
            packets: BTreeMap::new(),
            next_seq: 0,
            play_at: None,
            last_push_at: None,
            last_underrun_at: None,
            target_ms: config.target_ms,
            underruns: 0,
            late_dropped: 0,
            overflow_dropped: 0,
        }
    }

    /// 收到一帧。`now` 是本地单调时钟。
    ///
    /// 首次 `push` 时起播时刻 = `now + target`；若距上次 `push` 超过 500 ms
    /// （重新开始），清空旧帧并把起播时刻与 `next_seq` 重置为当前 `seq`。
    ///
    /// 重复序号（缓冲里已有同一 `seq`）**按已存处理、不覆盖**：返回
    /// [`Push::Stored`]，保留先到的那份数据（重复包通常是重传，先到的不会更差）。
    pub fn push(&mut self, seq: u64, data: Vec<u8>, now: Instant) -> Push {
        let restart = match self.last_push_at {
            Some(last) => now.saturating_duration_since(last) > RESET_GAP,
            None => true,
        };
        if restart {
            self.restart(seq, now);
        }
        self.last_push_at = Some(now);

        if seq < self.next_seq {
            self.late_dropped = self.late_dropped.saturating_add(1);
            return Push::Late;
        }
        if self.packets.contains_key(&seq) {
            // 重复包：不覆盖已存内容，见方法文档。
            return Push::Stored;
        }

        self.packets.insert(seq, data);
        if self.packets.len() > self.config.max_depth {
            self.trim_to_target();
            return Push::Trimmed;
        }
        Push::Stored
    }

    /// 播放侧每 20 ms 调用一次。
    ///
    /// - 未起播 / `now < play_at` → [`Pull::Idle`]；
    /// - 缓冲里有 `next_seq` → [`Pull::Packet`]，`next_seq + 1`、`play_at += 20 ms`；
    /// - `next_seq` 缺失但缓冲里有更大的序号 → [`Pull::Missing`]（PLC），同样推进；
    /// - 缓冲为空且 `now >= play_at` → [`Pull::Idle`]，累计 underrun 并把 `target`
    ///   上调 20 ms（封顶 `max_ms`），起播时刻重设为 `now + target`。
    ///
    /// 另外，若距上次欠载（或起播）已满 8 秒，本拍把 `target` 回落 20 ms
    /// （不低于 `min_ms`），并把窗口起点重置为 `now`——即每 8 秒最多回落一档。
    /// 从未欠载时窗口从起播时刻开始算，所以健康播放 8 秒后也会渐进回落。
    pub fn pull(&mut self, now: Instant) -> Pull {
        self.relax_target(now);

        let Some(play_at) = self.play_at else {
            // 尚未起播：不产生欠载计数。
            return Pull::Idle;
        };
        if now < play_at {
            return Pull::Idle;
        }

        // 键恒 ≥ next_seq（push 已挡掉更小的序号），故队首要么正好是 next_seq，
        // 要么就是断层后的更大序号。
        let head = self.packets.keys().next().copied();
        match head {
            Some(seq) if seq == self.next_seq => {
                let Some(data) = self.packets.remove(&seq) else {
                    // 刚看到的键不可能在这一刻消失；保守按 Idle 处理，不推进时间轴。
                    return Pull::Idle;
                };
                self.next_seq = self.next_seq.saturating_add(1);
                self.advance_play_at();
                Pull::Packet(data)
            }
            Some(_) => {
                // 队首序号大于 next_seq：中间有丢包，交给调用方做 PLC。
                self.next_seq = self.next_seq.saturating_add(1);
                self.advance_play_at();
                Pull::Missing
            }
            None => self.on_underrun(now),
        }
    }

    /// 当前缓冲帧数。
    #[must_use]
    pub fn depth(&self) -> usize {
        self.packets.len()
    }

    /// 当前 target（毫秒）。
    #[must_use]
    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    /// 累计欠载次数（缓冲为空且到点）。
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// 累计因序号过期而丢弃的帧数。
    #[must_use]
    pub fn late_dropped(&self) -> u64 {
        self.late_dropped
    }

    /// 累计因缓冲溢出而丢弃的帧数。
    #[must_use]
    pub fn overflow_dropped(&self) -> u64 {
        self.overflow_dropped
    }

    /// 重新起播：清掉旧帧、重置序号与起播时刻。
    fn restart(&mut self, seq: u64, now: Instant) {
        self.packets.clear();
        self.next_seq = seq;
        self.play_at = Some(now + self.target_duration());
        // 起播同时作为「无欠载窗口」的起点。
        self.last_underrun_at = Some(now);
    }

    /// 出帧后把播放时刻推进一帧。
    fn advance_play_at(&mut self) {
        if let Some(play_at) = self.play_at.as_mut() {
            *play_at += Duration::from_millis(u64::from(FRAME_MS));
        }
    }

    /// 缓冲为空且已到点：记欠载、上调 target、把起播时刻重设到 `now + target`。
    fn on_underrun(&mut self, now: Instant) -> Pull {
        self.underruns = self.underruns.saturating_add(1);
        self.target_ms = self
            .target_ms
            .saturating_add(FRAME_MS)
            .min(self.config.max_ms);
        self.play_at = Some(now + self.target_duration());
        self.last_underrun_at = Some(now);
        Pull::Idle
    }

    /// 连续 [`RELAX_WINDOW`] 无欠载时把 target 回落一档。
    fn relax_target(&mut self, now: Instant) {
        let Some(last) = self.last_underrun_at else {
            return;
        };
        if now.saturating_duration_since(last) < RELAX_WINDOW {
            return;
        }
        self.target_ms = self
            .target_ms
            .saturating_sub(FRAME_MS)
            .max(self.config.min_ms);
        // 重置窗口起点：每 8 秒最多回落一档。
        self.last_underrun_at = Some(now);
    }

    /// 溢出裁剪：从最小序号开始删到 `target_ms / 20` 帧。
    ///
    /// 保留上限取 `min(target_frames, max_depth)`，保证即使在
    /// `target_ms / 20 > max_depth` 的异常配置下也不会「删了却仍超上限」。
    fn trim_to_target(&mut self) {
        let keep = self.target_frames().min(self.config.max_depth);
        while self.packets.len() > keep {
            let Some(&oldest) = self.packets.keys().next() else {
                break;
            };
            self.packets.remove(&oldest);
            self.overflow_dropped = self.overflow_dropped.saturating_add(1);
        }
    }

    /// `target_ms` 对应的帧数。
    fn target_frames(&self) -> usize {
        // u32 → usize 在 32/64 位目标上都无损；失败只可能发生在 16 位目标上。
        usize::try_from(self.target_ms / FRAME_MS).unwrap_or(usize::MAX)
    }

    /// 当前 target 对应的 [`Duration`]。
    fn target_duration(&self) -> Duration {
        Duration::from_millis(u64::from(self.target_ms))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 用序号本身作为载荷，避免测试里出现无意义的常量数据。
    fn packet(seq: u64) -> Vec<u8> {
        seq.to_le_bytes().to_vec()
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn sequential_frames_play_in_order_after_target_delay() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        for seq in 1..=4 {
            assert_eq!(jb.push(seq, packet(seq), base), Push::Stored);
        }
        assert_eq!(jb.depth(), 4);

        // 起播延迟 = target（默认 80 ms）。
        assert_eq!(jb.pull(at(base, 79)), Pull::Idle);
        assert_eq!(jb.pull(at(base, 80)), Pull::Packet(packet(1)));
        assert_eq!(jb.pull(at(base, 100)), Pull::Packet(packet(2)));
        assert_eq!(jb.pull(at(base, 120)), Pull::Packet(packet(3)));
        assert_eq!(jb.pull(at(base, 140)), Pull::Packet(packet(4)));

        // 取空且到点 → 欠载。
        assert_eq!(jb.pull(at(base, 160)), Pull::Idle);
        assert_eq!(jb.underruns(), 1);
        assert_eq!(jb.late_dropped(), 0);
    }

    #[test]
    fn out_of_order_frames_are_reordered() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.push(1, packet(1), base), Push::Stored);
        assert_eq!(jb.push(3, packet(3), base), Push::Stored);
        assert_eq!(jb.push(2, packet(2), base), Push::Stored);

        assert_eq!(jb.pull(at(base, 80)), Pull::Packet(packet(1)));
        assert_eq!(jb.pull(at(base, 100)), Pull::Packet(packet(2)));
        assert_eq!(jb.pull(at(base, 120)), Pull::Packet(packet(3)));
    }

    #[test]
    fn late_sequence_is_dropped_and_not_buffered() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.push(1, packet(1), base), Push::Stored);
        assert_eq!(jb.pull(at(base, 80)), Pull::Packet(packet(1)));

        // next_seq 已是 2，序号 1 过期。
        assert_eq!(jb.push(1, packet(1), at(base, 80)), Push::Late);
        assert_eq!(jb.depth(), 0);
        assert_eq!(jb.late_dropped(), 1);
    }

    #[test]
    fn duplicate_sequence_keeps_first_copy() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.push(1, vec![0xAA], base), Push::Stored);
        assert_eq!(jb.push(1, vec![0xBB], base), Push::Stored);
        assert_eq!(jb.depth(), 1);
        assert_eq!(jb.pull(at(base, 80)), Pull::Packet(vec![0xAA]));
    }

    #[test]
    fn sequence_gap_reports_missing_then_continues() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.push(1, packet(1), base), Push::Stored);
        assert_eq!(jb.push(3, packet(3), base), Push::Stored);

        assert_eq!(jb.pull(at(base, 80)), Pull::Packet(packet(1)));
        // 2 缺失 → PLC，时间轴继续走。
        assert_eq!(jb.pull(at(base, 100)), Pull::Missing);
        // Missing 之后不能再连续报 Missing：有 3 就取 3。
        assert_eq!(jb.pull(at(base, 120)), Pull::Packet(packet(3)));
        // 取空且到点才是欠载。
        assert_eq!(jb.pull(at(base, 140)), Pull::Idle);
        assert_eq!(jb.underruns(), 1);
    }

    #[test]
    fn wider_gap_reports_one_missing_per_lost_frame_then_idle() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.push(1, packet(1), base), Push::Stored);
        assert_eq!(jb.push(4, packet(4), base), Push::Stored);

        assert_eq!(jb.pull(at(base, 80)), Pull::Packet(packet(1)));
        // 2、3 各丢一帧 → 各报一次 Missing（PLC），时间轴连续前进。
        assert_eq!(jb.pull(at(base, 100)), Pull::Missing);
        assert_eq!(jb.pull(at(base, 120)), Pull::Missing);
        assert_eq!(jb.pull(at(base, 140)), Pull::Packet(packet(4)));
        // 取空且到点 → Idle（不是 Missing），并欠载一次。
        assert_eq!(jb.pull(at(base, 160)), Pull::Idle);
        assert_eq!(jb.underruns(), 1);
        // 欠载后起播时刻被重设到 now + target(100) = 260，否则不会再次欠载。
        assert_eq!(jb.pull(at(base, 200)), Pull::Idle);
        assert_eq!(jb.underruns(), 1);
        assert_eq!(jb.pull(at(base, 260)), Pull::Idle);
        assert_eq!(jb.underruns(), 2);
    }

    #[test]
    fn underrun_raises_target_and_caps_at_max() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.push(1, packet(1), base), Push::Stored);

        // 取掉唯一的帧，播放时刻推进到 100。
        assert_eq!(jb.pull(at(base, 80)), Pull::Packet(packet(1)));
        assert_eq!(jb.target_ms(), 80);

        // 空且到点 → 欠载，target 80 → 100，起播时刻重设为 now + 100。
        assert_eq!(jb.pull(at(base, 100)), Pull::Idle);
        assert_eq!(jb.underruns(), 1);
        assert_eq!(jb.target_ms(), 100);

        // 未到重设后的时刻 → Idle，不产生欠载。
        assert_eq!(jb.pull(at(base, 199)), Pull::Idle);
        assert_eq!(jb.underruns(), 1);

        // 再次欠载 → 120（上限）。
        assert_eq!(jb.pull(at(base, 200)), Pull::Idle);
        assert_eq!(jb.underruns(), 2);
        assert_eq!(jb.target_ms(), 120);

        // 已封顶，继续欠载只计数。
        assert_eq!(jb.pull(at(base, 320)), Pull::Idle);
        assert_eq!(jb.underruns(), 3);
        assert_eq!(jb.target_ms(), 120);
    }

    #[test]
    fn overflow_trims_to_target_frames() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut saw_trim = false;

        for seq in 1..=30 {
            match jb.push(seq, packet(seq), base) {
                Push::Stored => {}
                Push::Trimmed => {
                    saw_trim = true;
                    // target 80 ms → 4 帧。
                    assert!(jb.depth() <= 4, "depth={}", jb.depth());
                }
                Push::Late => panic!("顺序 push 不应迟到"),
            }
        }

        assert!(saw_trim);
        assert_eq!(jb.overflow_dropped(), 22);
        assert!(jb.depth() <= JitterConfig::default().max_depth);
    }

    #[test]
    fn long_silence_restarts_playout_without_stale_frames() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.push(1, packet(1), base), Push::Stored);

        // 1 秒后才来下一帧：超过 500 ms → 重新起播，旧的 seq 1 被清空。
        assert_eq!(jb.push(5, packet(5), at(base, 1_000)), Push::Stored);
        assert_eq!(jb.depth(), 1);

        // 新起播时刻 = 1000 + target(80) = 1080。
        assert_eq!(jb.pull(at(base, 1_000)), Pull::Idle);
        assert_eq!(jb.pull(at(base, 1_079)), Pull::Idle);
        assert_eq!(jb.pull(at(base, 1_080)), Pull::Packet(packet(5)));
        assert_eq!(jb.underruns(), 0);
    }

    #[test]
    fn target_relaxes_after_eight_seconds_without_underruns() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut seq = 0_u64;
        let mut t_ms = 0_u64;

        // 每 20 ms 一帧、同时 pull：缓冲始终非空，因此不会产生欠载。
        while t_ms <= 9_000 {
            seq = seq.saturating_add(1);
            assert_eq!(jb.push(seq, packet(seq), at(base, t_ms)), Push::Stored);
            let _ = jb.pull(at(base, t_ms));

            if t_ms == 7_980 {
                assert_eq!(jb.target_ms(), 80, "8 秒前不应回落");
            }
            if t_ms == 8_000 {
                assert_eq!(jb.target_ms(), 60, "满 8 秒回落一档");
            }
            t_ms += 20;
        }

        assert_eq!(jb.target_ms(), 60);
        assert_eq!(jb.underruns(), 0);
        assert_eq!(jb.late_dropped(), 0);
        assert_eq!(jb.overflow_dropped(), 0);
    }

    #[test]
    fn pull_before_first_push_is_idle_without_underrun() {
        let base = Instant::now();
        let mut jb = JitterBuffer::new(JitterConfig::default());
        assert_eq!(jb.pull(base), Pull::Idle);
        assert_eq!(jb.pull(at(base, 10_000)), Pull::Idle);
        assert_eq!(jb.underruns(), 0);
        assert_eq!(jb.target_ms(), 80);
    }
}
