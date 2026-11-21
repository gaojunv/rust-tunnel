//! 分阶段延迟诊断日志（**默认关闭，纯观测**）。
//!
//! # 为什么要它
//!
//! 远程控制在「切浏览器标签 / 切文件夹」这类整屏重绘时体感偏慢，而既有读数（`SessionStats`
//! 的 `capture_us` / `encode_ms` / `decode_ms` / `e2e_ms`）是**跨阶段混在一起**的：`e2e_ms`
//! 只给出「采集完成 → 解码完成」一个总数，无法回答慢在采集节拍、编码、发送 pacing、网络
//! 还是解码/重绘。本模块把一条帧的各段耗时拆开：
//!
//! ```text
//! 被控端：capture 请求 → 采集开始 → 采集完成 → 编码开始 → 编码完成 → 进画面队列 → 写出完成
//! 控制端：包到达 → 解码开始 → 解码完成 → 上屏（GUI upload）
//! 交互：  输入注入 → 下一拍采集完成              （input_to_cap）
//!         输入注入 → 第一帧变化 ≥50‰ 的采集完成  （input_to_change）
//! ```
//!
//! `input_to_change` 是回答「点击标签后是远端系统重绘慢，还是我们的采集/编码/网络慢」的关键：
//! 它从**输入注入**量到**画面真的开始变**，把「远端应用自身的重绘时间」与「我们的链路」分开。
//!
//! # 开关与采样
//!
//! 开关是**进程内可变**的（[`set_mode`]）：GUI 设置页与无人值守实例的 `host.toml` 直接覆盖，
//! 运行中切换立即生效，不必重启。环境变量只作为**默认值/命令行调试兜底**：
//!
//! - `RUST_TUNNEL_LATENCY_LOG=0`（默认 / 未设置）→ 完全关闭：除一次原子初始化外不做事。
//! - `=1`（或 `summary`/`on`）→ 每秒一行阶段汇总 + 超过阈值的慢帧明细。
//! - `=2`（或 `frames`/`all`）→ 每秒汇总 + **每一帧**明细。
//! - `RUST_TUNNEL_LATENCY_SLOW_MS`（默认 `100`）→ 慢帧阈值（毫秒）。
//!
//! 日志统一 `target: "latency"`、级别 `info`：它是给人看的诊断输出，不是运行时状态；
//! 未开启时一行都不会出现。GUI 把它送到日志面板，无人值守实例把它写进 `instance.log`。
//! **日志级别需 ≥ `info`**（GUI 配置 `log = "warn"` 时这些行会被 EnvFilter 过滤掉）。
//!
//! # 不变量
//!
//! - **不上线**：本模块不碰 [`crate::session`] 的任何字节形状，全部读数只在进程内传递。
//! - **不参与决策**：采样只写日志，QoS / 丢帧 / 关键帧 / 补丁模式一律不受影响。
//! - 同一进程内的差值一律用 [`crate::clock::now_us`]；跨端绝不相减（除既有的
//!   [`crate::clock::elapsed_ms`] + 时钟偏差估计）。

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

/// 开关环境变量（见模块文档）。
const ENV_MODE: &str = "RUST_TUNNEL_LATENCY_LOG";
/// 慢帧阈值环境变量（毫秒）。
const ENV_SLOW_MS: &str = "RUST_TUNNEL_LATENCY_SLOW_MS";
/// 慢帧阈值默认值（毫秒）。
const DEFAULT_SLOW_MS: u64 = 100;
/// [`Mode`] 的「尚未初始化」哨兵（`AtomicU8` 里的非法值）。
const MODE_UNINIT: u8 = u8::MAX;

/// 日志模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 关闭（默认）。
    Off,
    /// 每秒汇总 + 慢帧明细。
    Summary,
    /// 每秒汇总 + 每一帧明细。
    Frames,
}

impl Mode {
    /// 解析环境变量的取值（**纯函数**，便于单测）。
    ///
    /// 未设置由调用方处理成 [`Self::Off`]；无法识别的值按 [`Self::Summary`] 处理（宽容：
    /// 用户显式设了一个非零值，显然是想看到东西）。
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            value if value.eq_ignore_ascii_case("off") || value == "0" => Self::Off,
            value
                if value.eq_ignore_ascii_case("false")
                    || value.eq_ignore_ascii_case("no")
                    || value.is_empty() =>
            {
                Self::Off
            }
            value
                if value == "2"
                    || value.eq_ignore_ascii_case("frames")
                    || value.eq_ignore_ascii_case("all") =>
            {
                Self::Frames
            }
            _ => Self::Summary,
        }
    }
}

/// 当前模式（进程内可变；首次访问时用环境变量初始化）。
///
/// GUI 的「延迟诊断日志」开关与无人值守实例的 `host.toml` 都通过 [`set_mode`] 直接覆盖它，
/// 运行中切换立即生效（每帧只读一次 Relaxed 原子，可忽略）。
#[must_use]
pub fn mode() -> Mode {
    // 哨兵 = 尚未由环境变量初始化过。CAS 保证并发的 `set_mode` 不会被延迟的初始化覆盖。
    let raw = MODE_ATOMIC.load(Ordering::Relaxed);
    if raw != MODE_UNINIT {
        return decode_mode(raw);
    }
    let parsed = std::env::var(ENV_MODE).map_or(Mode::Off, |raw| Mode::parse(&raw));
    match MODE_ATOMIC.compare_exchange(
        MODE_UNINIT,
        encode_mode(parsed),
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        Ok(_) => parsed,
        // 另一个线程先初始化了（或 `set_mode` 已经生效）：以它为准。
        Err(current) => decode_mode(current),
    }
}

/// 全局模式原子：`mode()` 与 [`set_mode`] 共用。
static MODE_ATOMIC: AtomicU8 = AtomicU8::new(MODE_UNINIT);

/// 运行期覆盖模式（GUI 设置页 / 无人值守实例的 `host.toml`）。
///
/// 覆盖之后 [`mode`] 不再读环境变量——环境变量只是**默认值**。
pub fn set_mode(mode: Mode) {
    MODE_ATOMIC.store(encode_mode(mode), Ordering::Relaxed);
}

/// 把 [`Mode`] 编码成原子里的值。
fn encode_mode(mode: Mode) -> u8 {
    match mode {
        Mode::Off => 0,
        Mode::Summary => 1,
        Mode::Frames => 2,
    }
}

/// 由原子值还原 [`Mode`]（未知值按 `Summary` 处理，防御性）。
fn decode_mode(raw: u8) -> Mode {
    match raw {
        0 => Mode::Off,
        2 => Mode::Frames,
        _ => Mode::Summary,
    }
}

/// 是否开启了诊断日志（任何非 [`Mode::Off`]）。
#[must_use]
pub fn enabled() -> bool {
    mode() != Mode::Off
}

/// 是否要打**每一帧**的明细（[`Mode::Frames`]）。
#[must_use]
pub fn frame_logs() -> bool {
    mode() == Mode::Frames
}

/// 慢帧阈值（微秒）：总耗时超过它的帧在 [`Mode::Summary`] 下也会打明细。
///
/// 未初始化时用 `RUST_TUNNEL_LATENCY_SLOW_MS`（默认 [`DEFAULT_SLOW_MS`]）填充；之后
/// 可用 [`set_slow_threshold_ms`] 运行期覆盖。
#[must_use]
pub fn slow_threshold_us() -> u64 {
    let raw = SLOW_US.load(Ordering::Relaxed);
    if raw != u64::MAX {
        return raw;
    }
    let parsed = std::env::var(ENV_SLOW_MS)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SLOW_MS)
        .saturating_mul(1000);
    match SLOW_US.compare_exchange(u64::MAX, parsed, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => parsed,
        Err(current) => current,
    }
}

/// 运行期覆盖慢帧阈值（毫秒）。
pub fn set_slow_threshold_ms(ms: u64) {
    SLOW_US.store(ms.saturating_mul(1000), Ordering::Relaxed);
}

/// 慢帧阈值的全局原子（`u64::MAX` = 尚未初始化）。
static SLOW_US: AtomicU64 = AtomicU64::new(u64::MAX);

/// 一个可累计的延迟阶段。
///
/// **顺序即数组下标**（[`Self::index`]）：只允许在末尾追加，改顺序会让历史汇总错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 采集请求（tick 派发）→ 阻塞采集任务真正开始：阻塞线程池/运行时调度等待。
    CaptureWait,
    /// 采集本身（`spawn_blocking` 内的调用耗时）。
    Capture,
    /// 采集完成 → 编码任务取走（有界(1) 队列 + 会话循环调度）。
    EncodeQueue,
    /// 编码（含在线重配置）。
    Encode,
    /// 编码完成 → 进入画面出站队列（等待队列空位）。
    OutQueue,
    /// 采集完成 → 写任务取走（含编码、在队列里的等待，用于和 `write` 对照）。
    SendQueue,
    /// 写任务真正写出（含大帧 pacing 的分块 sleep）。
    Write,
    /// 输入注入调用本身的耗时。
    Inject,
    /// 输入注入 → 下一拍采集完成（采集节拍 + 采集）。
    InputToCapture,
    /// 输入注入 → 第一帧**变化面积 ≥50‰** 的采集完成（用户感知到「画面开始变」）。
    InputToChange,
    /// 包到达 → 解码任务取走（解码队列等待）。
    RecvQueue,
    /// 解码。
    Decode,
    /// 采集完成 → 解码完成（跨端，仅控制器侧；已含时钟偏差校正）。
    CaptureToDecode,
    /// 输入事件在控制器本地缓冲里等待的时间（UI 线程 → 会话循环）。
    InputQueue,
    /// 帧到达 → GUI 上屏（重绘调度）。
    Present,
}

impl Stage {
    /// 变体总数（定长数组长度）。
    pub const COUNT: usize = 15;

    /// 全部阶段（按稳定下标排列，汇总输出按此顺序）。
    pub const ALL: [Self; Self::COUNT] = [
        Self::CaptureWait,
        Self::Capture,
        Self::EncodeQueue,
        Self::Encode,
        Self::OutQueue,
        Self::SendQueue,
        Self::Write,
        Self::Inject,
        Self::InputToCapture,
        Self::InputToChange,
        Self::RecvQueue,
        Self::Decode,
        Self::CaptureToDecode,
        Self::InputQueue,
        Self::Present,
    ];

    /// 稳定的数组下标（**追加不改顺序**）。
    #[must_use]
    pub fn index(self) -> usize {
        self as usize
    }

    /// 日志里的短键名（英文，便于 grep / 聚合）。
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::CaptureWait => "cap_wait",
            Self::Capture => "cap",
            Self::EncodeQueue => "enc_q",
            Self::Encode => "enc",
            Self::OutQueue => "out_q",
            Self::SendQueue => "send_q",
            Self::Write => "write",
            Self::Inject => "inject",
            Self::InputToCapture => "input_to_cap",
            Self::InputToChange => "input_to_change",
            Self::RecvQueue => "recv_q",
            Self::Decode => "dec",
            Self::CaptureToDecode => "e2e",
            Self::InputQueue => "input_q",
            Self::Present => "present",
        }
    }
}

/// 单个阶段的累计读数。
///
/// `values` 保留这一秒的原始样本，用于算分位；关闭诊断时调用方根本不进入这里，开启后
/// 每秒最多几十到几百个样本（60 fps × 各阶段），排序成本可忽略。
#[derive(Debug, Clone, Default)]
struct Sample {
    /// 采样次数（含因窗口上限而未保留原始值的样本）。
    count: u64,
    /// 累计耗时（微秒，饱和加法）。
    sum_us: u64,
    /// 单次最大值（微秒）。
    max_us: u64,
    /// 原始样本（仅保留前 [`MAX_SAMPLES_PER_STAGE`] 个）。
    values: Vec<u32>,
}

/// 单个阶段每秒最多保留多少原始样本。
///
/// 正常峰值是 60 fps（编码/解码/出队各一份），远低于此值。上限只防御「某个错误循环
/// 每秒打几十万次」这类病态场景：超出后仍计数/求和/取最大，只是分位基于已保留的样本。
const MAX_SAMPLES_PER_STAGE: usize = 8192;

/// 一个阶段在某个统计窗口内的分布。
///
/// 分位用**微秒**给出（不是毫秒），调用方自己决定显示精度；`p99` 是判断「偶发波动」的
/// 主读数——均值/最大值都会把一次 300 ms 的尖峰淹没或放大成孤例，p99 才看得出尾部。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageReport {
    /// 采样次数。
    pub count: u64,
    /// 均值（微秒）。
    pub mean_us: u64,
    /// 中位数（微秒）。
    pub p50_us: u64,
    /// 90 分位（微秒）。
    pub p90_us: u64,
    /// 99 分位（微秒）。
    pub p99_us: u64,
    /// 最大值（微秒）。
    pub max_us: u64,
}

/// 一次窗口汇总（由 [`LatencyWindow::take`] 产出）。
///
/// 保留**已排序**的原始样本，压测程序可以把多个窗口的样本拼起来算整场会话的分位；
/// 生产日志只需要 [`Self::summary`]。
#[derive(Debug, Clone, Default)]
pub struct WindowReport {
    /// 每个阶段一份。
    stages: [StageReport; Stage::COUNT],
    /// 每个阶段已排序的原始样本。
    values: [Vec<u32>; Stage::COUNT],
}

impl WindowReport {
    /// 某个阶段的分布（无样本时全零）。
    #[must_use]
    pub fn stage(&self, stage: Stage) -> StageReport {
        self.stages[stage.index()]
    }

    /// 某个阶段已排序的原始样本（微秒）。
    #[must_use]
    pub fn samples(&self, stage: Stage) -> &[u32] {
        &self.values[stage.index()]
    }

    /// 是否一个样本都没有。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stages.iter().all(|stage| stage.count == 0)
    }

    /// 汇总成一行紧凑文本：`cap=8.4/9.1/12.0ms`（均值/p99/最大）。
    ///
    /// 只输出有样本的阶段；毫秒取一位小数（整数运算，避免浮点 lint 与精度抖动）。
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for stage in Stage::ALL {
            let report = self.stage(stage);
            if report.count == 0 {
                continue;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(stage.key());
            out.push('=');
            push_ms(&mut out, report.mean_us);
            out.push('/');
            push_ms(&mut out, report.p99_us);
            out.push('/');
            push_ms(&mut out, report.max_us);
            out.push_str("ms");
        }
        out
    }
}

/// 每个阶段一份累计值的滑窗（由调用方每秒取走）。
///
/// 只在延迟诊断打开时被写入；关闭后 `record` 不会被调用。
#[derive(Debug, Clone)]
pub struct LatencyWindow {
    /// 每个阶段一份。
    samples: [Sample; Stage::COUNT],
}

impl Default for LatencyWindow {
    fn default() -> Self {
        Self {
            samples: std::array::from_fn(|_| Sample::default()),
        }
    }
}

impl LatencyWindow {
    /// 空窗口。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一次采样（饱和，不 panic）。
    pub fn record(&mut self, stage: Stage, us: u64) {
        let sample = &mut self.samples[stage.index()];
        sample.count = sample.count.saturating_add(1);
        sample.sum_us = sample.sum_us.saturating_add(us);
        sample.max_us = sample.max_us.max(us);
        if sample.values.len() < MAX_SAMPLES_PER_STAGE {
            sample.values.push(u32::try_from(us).unwrap_or(u32::MAX));
        }
    }

    /// 清空全部累计（每秒汇总之后调用）。
    pub fn clear(&mut self) {
        self.samples = std::array::from_fn(|_| Sample::default());
    }

    /// 是否一个样本都没有。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.iter().all(|sample| sample.count == 0)
    }

    /// 取走并清空本窗口，算出每个阶段的分布。
    ///
    /// 排序在这里做（每秒一次），不在热路径上。
    #[must_use]
    pub fn take(&mut self) -> WindowReport {
        let stages = std::array::from_fn(|index| {
            let sample = &mut self.samples[index];
            if sample.count == 0 {
                return StageReport::default();
            }
            sample.values.sort_unstable();
            StageReport {
                count: sample.count,
                mean_us: sample.sum_us / sample.count,
                p50_us: percentile(&sample.values, 50),
                p90_us: percentile(&sample.values, 90),
                p99_us: percentile(&sample.values, 99),
                max_us: sample.max_us.max(u64::from(sample.values.last().copied().unwrap_or(0))),
            }
        });
        let values = std::array::from_fn(|index| std::mem::take(&mut self.samples[index].values));
        self.clear();
        WindowReport { stages, values }
    }

    /// 只汇总、不清空（单测/调试用）。
    #[must_use]
    pub fn summary(&self) -> String {
        let mut clone = self.clone();
        clone.take().summary()
    }
}

/// 取 `sorted` 的 `percent` 分位（0..=100；空切片返回 0）。
///
/// 用最近秩（nearest-rank），对 30 个样本的 p99 就是最大值——这正是我们想要的语义：
/// **尾部指标不能被平均掉**。
fn percentile(sorted: &[u32], percent: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let last = sorted.len() - 1;
    let index = last.saturating_mul(percent).div_ceil(100);
    u64::from(sorted[index.min(last)])
}

/// 往 `out` 追加「毫秒，一位小数」（整数运算，截断而非四舍五入）。
fn push_ms(out: &mut String, us: u64) {
    let whole = us / 1000;
    let tenth = (us % 1000) / 100;
    // 写 `String` 不会失败；忽略返回值即可。
    let _ = write!(out, "{whole}.{tenth}");
}

/// 会话的哪一侧在产生读数（只用于区分同一个进程里的两条会话循环）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// 被控端（采集 → 编码 → 写出）。
    Controlled,
    /// 控制器（收包 → 解码 → 上屏）。
    Controller,
}

impl Side {
    /// 短键名（日志/压测报告用）。
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Controlled => "controlled",
            Self::Controller => "controller",
        }
    }
}

/// 进程内观察者：每次窗口汇总时收到一份结构化报告。
///
/// 生产路径不安装它（零开销）；`examples/loopback_bench.rs` 用它把整场会话的分阶段
/// 样本聚合成 p50/p95/p99，而不必解析日志文本。
pub trait Observer: Send + Sync {
    /// 收到某一侧的窗口汇总。
    fn on_window(&self, side: Side, report: &WindowReport);
}

/// 全局观察者（`None` = 未安装）。
static OBSERVER: std::sync::RwLock<Option<std::sync::Arc<dyn Observer>>> =
    std::sync::RwLock::new(None);

/// 安装/卸载进程内观察者（压测程序用；生产路径保持未安装）。
pub fn set_observer(observer: Option<std::sync::Arc<dyn Observer>>) {
    *OBSERVER
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = observer;
}

/// 在共享窗口里记一次采样（中毒时恢复内部数据：本模块不 panic，中毒只可能来自外部）。
pub fn record(window: &Mutex<LatencyWindow>, stage: Stage, us: u64) {
    window
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record(stage, us);
}

/// 取出并清空共享窗口的一份报告；窗口为空（这一秒没有任何画面）时返回 `None`。
///
/// 同时把报告交给已安装的 [`Observer`]（在窗口锁之外调用，观察者可以安全地再取别的锁）。
pub fn take_report(window: &Mutex<LatencyWindow>, side: Side) -> Option<WindowReport> {
    let report = {
        let mut guard = window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_empty() {
            return None;
        }
        guard.take()
    };
    let observer = OBSERVER
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(observer) = observer {
        observer.on_window(side, &report);
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_documented_values() {
        for raw in ["", "0", "off", "OFF", "false", "no", " 0 "] {
            assert_eq!(Mode::parse(raw), Mode::Off, "{raw:?} 应关闭");
        }
        for raw in ["1", "on", "summary", "on ", "true"] {
            assert_eq!(Mode::parse(raw), Mode::Summary, "{raw:?} 应汇总");
        }
        for raw in ["2", "frames", "all", "ALL"] {
            assert_eq!(Mode::parse(raw), Mode::Frames, "{raw:?} 应逐帧");
        }
        // 无法识别的非零值：宽容地当成 Summary。
        assert_eq!(Mode::parse("yes-please"), Mode::Summary);
    }

    #[test]
    fn window_aggregates_average_p99_and_max() {
        let mut window = LatencyWindow::new();
        assert!(window.is_empty());
        assert_eq!(window.summary(), "");
        window.record(Stage::Capture, 1_000);
        window.record(Stage::Capture, 3_000);
        window.record(Stage::Encode, 12_000);
        assert!(!window.is_empty());
        assert_eq!(
            window.summary(),
            "cap=2.0/3.0/3.0ms enc=12.0/12.0/12.0ms",
            "均值/p99/最大 + 一位毫秒小数"
        );
        window.clear();
        assert!(window.is_empty());
        assert_eq!(window.summary(), "");
    }

    #[test]
    fn percentiles_expose_the_tail() {
        let mut window = LatencyWindow::new();
        // 99 个 1 ms + 1 个 500 ms：均值被稀释到 ~6 ms，但 p99 必须看到尾部。
        for _ in 0..99 {
            window.record(Stage::Decode, 1_000);
        }
        window.record(Stage::Decode, 500_000);
        let report = window.take();
        let decode = report.stage(Stage::Decode);
        assert_eq!(decode.count, 100);
        assert_eq!(decode.p50_us, 1_000);
        assert_eq!(decode.p99_us, 500_000, "p99 必须落在尾部样本上");
        assert_eq!(decode.max_us, 500_000);
        assert_eq!(report.samples(Stage::Decode).len(), 100);
        // 报告保留数据，被取走的窗口才应清空（`take_clears_the_window` 也锁这一点）。
        assert!(!report.is_empty(), "报告本身必须保留样本");
        assert!(window.is_empty(), "take 之后窗口应清空");
    }

    #[test]
    fn take_clears_the_window() {
        let mut window = LatencyWindow::new();
        window.record(Stage::Write, 2_000);
        let report = window.take();
        assert_eq!(report.stage(Stage::Write).p50_us, 2_000);
        assert!(window.is_empty());
        assert!(window.take().is_empty());
    }

    #[test]
    fn window_saturates_instead_of_panicking() {
        let mut window = LatencyWindow::new();
        window.record(Stage::Write, u64::MAX);
        window.record(Stage::Write, u64::MAX);
        // 不 panic、不溢出回绕（饱和）。
        assert!(window.summary().contains("write="));
    }

    #[test]
    fn stage_indices_are_stable_and_cover_the_window() {
        for (index, stage) in Stage::ALL.into_iter().enumerate() {
            assert_eq!(stage.index(), index, "ALL 顺序必须与 index 一致");
            assert!(!stage.key().is_empty());
        }
        assert_eq!(Stage::ALL.len(), Stage::COUNT);
    }
}
