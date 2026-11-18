//! 控制器侧会话循环：握手 → 收画面 → 回传输入 → 心跳。
//!
//! 传输建立阶段已把 `Hello` 帧与中继的 `RequestRelay` 帧合并写出（见 [`crate::transport`]），
//! 因此这里**只等 `HelloAck`**。收到后派生会话密钥、装上 [`crate::crypto::Cipher`]，
//! 之后的帧全部加密。

use crate::auth::verify_ack_mac;
use crate::clock::{clock_offset_us, elapsed_ms, now_us, UNKNOWN_OFFSET};
use crate::codec::{vp9_decoder_factory, VideoDecoderApi, VideoDecoderFactory};
use crate::controlled::{send_cancellable, writer_loop, OutboundQueues};
use crate::crypto::{derive_session_keys, Cipher};
use crate::error::{RemoteError, RemoteResult};
use crate::input_buffer::InputBuffer;
use crate::latency::{self, LatencyWindow, Stage};
use crate::reliable::{ReliableMetrics, ReliableSnapshot};
use crate::screen::{
    DecodedFrame, DecodedPatch, DecoderInfo, FrameSink, InputEvent, QualityPreset, ScreenPatch,
};
use crate::session::{
    trusted_rtt, CodecId, ControllerHandshake, RegionRect, SessionCodec, SessionEnd, SessionFrame,
    SessionMessage, HEARTBEAT_INTERVAL, LIVENESS_TIMEOUT, MAX_REGION_RECTS, REGION_FLAG_ZSTD,
    VIDEO_FLAG_KEYFRAME,
};
use crate::stats::{KeyframeReason, SessionStats, SessionStatsReporter, TransportStats};
use crate::transfer::{TransferCommand, TransferHub, CHUNK_PAUSE};
use crate::transport::BoxDuplex;
use crate::video::MAX_FRAME_DIM;
use futures_util::StreamExt as _;
use rust_tunnel_common::protocol::RemoteMode;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// 等待 `HelloAck` 的超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 收尾 `Bye` 的发送上限。对端可能已停读（写缓冲满），不加限时会让 teardown 永久卡住、`on_closed`
/// 永不投递；`Bye` 只是尽力通知，超时丢弃即可。
const BYE_TIMEOUT: Duration = Duration::from_secs(2);

/// 待解码载荷队列深度。
///
/// 解码在独立任务里串行进行（1080p 单帧约十几毫秒）。队列的职责只是**吸收一次抖动**
/// （一次心跳迟到、一次调度抖动）：4 帧在 30 fps 下约 133 ms，刚好覆盖这类毛刺；更深的
/// 队列不会让解码变快，只会把「解码跟不上」变成「卡顿结束后先回放一段旧画面」——16 帧
/// @30fps 就是 533 ms 的陈旧画面缓冲，正是延迟审查要消掉的东西。解码在常用码率下本来
/// 就跟得上，队列深度的正确量级是「吸收抖动」而不是「存下整个卡顿」。
///
/// 队列满时的策略见 [`DecodeQueue`]：**淘汰最旧的一条、让最新的包一定进队**（旧实现用
/// `mpsc::try_send`，满了丢的是刚到的包，队列里继续解 4 张更旧的帧）。「按年龄丢旧帧」
/// 因此不需要独立的时钟判据——队列本身就是按到达顺序老化的。
const DECODE_QUEUE: usize = 4;

/// 关键帧请求的最小间隔（同时也是「等 IDR 期间的**重试周期**」）。
///
/// 序号断层/队列溢出/解码失败都可能每帧触发一次「请补 IDR」。不限频就会形成 IDR 风暴：
/// 每个 IDR 都是带宽尖峰，反而让解码队列更满、继续丢包。400 ms 内最多一次，既压住风暴，
/// 又保证等待中的请求会在该窗口内被重试（不会永久卡在等 IDR）。
///
/// **为什么是 400 ms**：它是「等 IDR 期间的探测周期」，不是「一张 IDR 的生效时间」。被控端
/// 的去重窗口（`KEYFRAME_DEDUP_WINDOW` = 1.2 s，见 `crates/remote/src/controlled.rs`）按
/// 「写完全帧 281 ms + RTT 40 ms + 解码 516 ms ≈ 837 ms」标定，**大于**本周期：在途 IDR 期间
/// 到达的重试会被对端吸收（这正是目的——真机一次丢帧事件曾换回 3 张 ~223 KB 的 IDR）。
///
/// 代价是「IDR 真的丢了/解码失败」时，恢复要等窗口过期：上界 ≈ 1200 + 400 ms。这个取舍是
/// 有意的——Punch 路径有用户态可靠层（ARQ）保交付、直连/中继是 TCP，IDR 本身丢包不是常见
/// 故障；把周期压到窗口以内只会把「偶尔的重发」变成常态化的 IDR 风暴。
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(400);

/// 在途心跳时刻队列的上限（1s 探测、10s 可信上限 ⇒ 8 个足够覆盖任何可信延迟）。
const HEARTBEAT_SENT_CAP: usize = 8;

/// QoS 反馈间隔（秒级）。每秒向被控端发一条 `QosFeedback`（含 RTT / 收码率 / 解码耗时 / 积压），
/// 驱动被控端的 AIMD 闭环（见 [`crate::qos`]）。同时按同一节拍把解码侧读数经进程内
/// [`SessionStatsReporter`] 送到 UI。
const QOS_INTERVAL: Duration = Duration::from_secs(1);

/// 「长时间无帧」的判据（超过它即请求关键帧并告警）。
///
/// 静止桌面**合法地**不产帧（被控端按「无变化」去重后完全不喂编码器），所以这不是错误检测，而是
/// **卡死检测**：对端采集/编码挂住、或中继半死时，画面会永久冻结且毫无报错。取 3s——足够区分
/// 「静止」与「卡死」，代价是被控端在静止桌面上每 3s 补一个 IDR（1080p 静止帧只有几十字节到
/// 几 KB，取决于编码器与码率控制模式，可忽略）。请求本身也按同一窗口限频，避免刷屏。
const NO_FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// 慢解码判据（微秒）：超过它才把解码的四段细分打进延迟日志。
///
/// 真机上 p50 只有 6.5 ms，正常帧没必要刷四行数字；而 300–500 ms 的停顿一次都不能漏。
/// 取 100 ms 与延迟诊断的默认慢帧阈值（`RUST_TUNNEL_LATENCY_SLOW_MS`）同量级，
/// 又与「体感卡顿」的门槛相当。
const SLOW_DECODE_US: u64 = 100_000;

/// 一条待解码的画面载荷（尚未解码）。
#[derive(Debug)]
enum DecodeJob {
    /// 被控端选定 codec（`CodecSelect`）。解码任务据此换解码器；新 codec 的码流仍从关键帧起步。
    SelectCodec(CodecId),
    /// 一个视频包（长度前缀拼接的帧，codec 由 `CodecSelect` 决定）。
    Packet {
        /// 来源显示器编号。
        display: u32,
        /// 帧序号（被控端递增）。
        ///
        /// 只在**解码任务实际看到**的包之间比较：主循环有意丢掉的包根本不会进到这里，所以
        /// 丢弃不会再表现成「断层」（见 [`seq_is_gap`]）。旧实现里每次丢弃之后到达的 IDR
        /// 都会被判成断层，于是销毁解码器、再请求一张 IDR——一次事件换回 3 张 IDR。
        seq: u64,
        /// 编解码器（应与 `SelectCodec` 一致；缺失时作为兜底）。
        codec: CodecId,
        /// 标志位（[`VIDEO_FLAG_KEYFRAME`] 等）。
        flags: u8,
        /// 声明的**编码**宽度。
        width: u32,
        /// 声明的**编码**高度。
        height: u32,
        /// 帧数据。
        data: Vec<u8>,
        /// 该帧采集完成的时刻（被控端单调时钟，微秒；`0` = 未知）。端到端延迟用。
        capture_ts_us: u64,
        /// 本端收到该包的时刻（本端单调时钟，微秒）。诊断用：解码队列等待 = 解码开始 - 它。
        recv_us: u64,
    },
    /// 一批脏矩形补丁（协议 v5 `RegionUpdate`，仅 H.264 补丁模式）。
    ///
    /// 与 `Packet` 走**同一条**队列，因此 [`FrameSink::on_frame`] / [`FrameSink::on_patch`]
    /// 的调用顺序就是线上顺序——补丁永远应用在它前面那个全帧之上。这是补丁模式能够
    /// 「只带脏矩形」的全部前提（见 [`decode_loop`]）。
    Region {
        /// 目标显示器编号。
        display: u32,
        /// 目标帧缓冲宽度（编码尺寸，偶数）。
        width: u32,
        /// 目标帧缓冲高度（编码尺寸，偶数）。
        height: u32,
        /// 已通过主循环粗校验的矩形；解压与逐矩形细校验在解码任务里做。
        rects: Vec<RegionRect>,
        /// 该批补丁采集完成的时刻（被控端单调时钟，微秒；`0` = 未知）。端到端延迟用。
        capture_ts_us: u64,
        /// 本端收到该批补丁的时刻（本端单调时钟，微秒）。诊断用。
        recv_us: u64,
    },
}

/// [`DecodeJob`] 的类别（截断策略只关心类别，不需要整条载荷）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    /// `SelectCodec`：控制项，**永不丢弃**（丢了它解码器就不会切 codec）。
    Codec,
    /// 关键帧：参考链上**唯一安全的切点**。
    Keyframe,
    /// 普通视频帧：直接或间接引用前面的帧，单独丢它会花屏。
    Frame,
    /// 补丁批：可增量应用，丢一批会在合成帧上永久缺块，因此**不主动丢弃**。
    Patch,
}

impl DecodeJob {
    fn kind(&self) -> JobKind {
        match self {
            Self::SelectCodec(_) => JobKind::Codec,
            Self::Packet { flags, .. } if flags & VIDEO_FLAG_KEYFRAME != 0 => JobKind::Keyframe,
            Self::Packet { .. } => JobKind::Frame,
            Self::Region { .. } => JobKind::Patch,
        }
    }

    /// 载荷对应的视频序号（补丁/控制项没有序号）。
    ///
    /// 只给单测用：运行时日志改成输出计数/布尔（`flushed=n kept_from_keyframe=…`），
    /// 不再逐条打印被截断的序号。
    #[cfg(test)]
    fn seq(&self) -> Option<u64> {
        match self {
            Self::Packet { seq, .. } => Some(*seq),
            Self::SelectCodec(_) | Self::Region { .. } => None,
        }
    }
}

/// 队列满时**按参考链截断**：返回与输入等长的「是否保留」掩码（**纯函数**，便于单测）。
///
/// H.264/VP9 的参考链是**线性**的：`P5` 依赖 `P4` 依赖 `P3`……所以「挑一帧丢掉」必然让排在
/// 它后面的帧全部解出花屏。唯一安全的切点是**关键帧**——它自己就是新链的起点。故：
///
/// - 找到队列里**最新**的一张关键帧 `k`：保留 `k..`（它们的参考链从 `k` 重新起算，完好），
///   丢弃 `k` 之前的全部载荷（它们排在关键帧之前，本来就会被跳过）。
/// - 队列里没有关键帧：一个都不保留（整条链作废）。
/// - `SelectCodec` 是控制项，任何情况下都保留。
///
/// 于是「淘汰」不再是「丢一帧换一帧」，而是「截掉一段作废的链、留下可以正常解码的那一段」：
/// 一次 445 ms 停顿把队列塞满时，**不会先闪 1–4 帧花屏再等 IDR**。
#[must_use]
fn flush_keep(kinds: &[JobKind]) -> Vec<bool> {
    let anchor = kinds.iter().rposition(|kind| *kind == JobKind::Keyframe);
    kinds
        .iter()
        .enumerate()
        .map(|(index, kind)| *kind == JobKind::Codec || anchor.is_some_and(|anchor| index >= anchor))
        .collect()
}

/// 队列满时的截断结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlushReport {
    /// 被丢弃的**视频帧**数（普通帧 + 关键帧，不含本包）。
    dropped_frames: usize,
    /// 被丢弃的补丁批数（补丁不参与 `SessionStats::Decode::dropped` 的口径）。
    dropped_patches: usize,
    /// 本包是否入队。
    ///
    /// `false` = 参考链上留了缺口（本包引用的帧全被丢了），**本包也一起丢**——留下它只会
    /// 解出一张花屏。调用方必须关闸门并请求一张 IDR。
    admitted: bool,
    /// 截断后队列里是否有关键帧作为新基准（本包本身是关键帧时也算）。`admitted` 为真时必为真。
    anchored: bool,
}

/// [`DecodeQueue::push`] 的结果。
#[derive(Debug)]
enum PushOutcome {
    /// 已入队。
    Queued,
    /// 队列已满，按参考链截断后又收下了本包（见 [`FlushReport`]）。
    Flushed(FlushReport),
    /// 队列已满且本批不可截断（补丁），本批被丢弃。调用方应请求关键帧。
    Dropped,
    /// 解码任务已结束。
    Closed,
}

/// 待解码队列：**按参考链截断**的最小队列。
///
/// 为什么不用 `tokio::mpsc`：它只能丢**最新**的那一条（`try_send` 满即失败），而解码落后时
/// 最新的画面恰恰是最该保留的。2026-11-18 真机日志（1080p30 / H.264 / QSV / DXGI）里，一次
/// 445 ms 的 DXVA 回读停顿就把 4 深队列塞满，旧语义下丢的是刚到的包、队列里继续解 4 张更旧
/// 的帧（最深 ≈1.8 s 陈旧画面），随后的 `drop_epoch` 又把它们整体作废——一次事件丢 5 帧，
/// 而且最关键的那张 IDR（恢复参考链的唯一手段）也常常是被丢的那一张。
///
/// 满队列的处理见 [`flush_keep`]：截断到**最新关键帧之后**（没有关键帧就整队清空），
/// 这样留下的帧参考链完好、不会花屏，也不需要为此请 IDR。于是：
/// ① 关键帧永远不会因为队列满而错过；② 用户追到的是最新画面；③ 「按年龄丢旧帧」不再需要
/// 采集时间戳与时钟偏差估计（旧的 `should_drop_stale` 要四个条件同时成立，91 秒里一次都没
/// 命中）。
struct DecodeQueue {
    jobs: std::sync::Mutex<VecDeque<DecodeJob>>,
    /// 入队唤醒。用 `notify_one`（会存 permit）而不是 `notify_waiters`：后者不存 permit，
    /// 「检查队列为空 → 对端入队并通知 → 本端才注册等待」这一步会丢唤醒。
    notify: tokio::sync::Notify,
    /// 会话结束（主循环收尾）：之后的 `push` 一律 `Closed`，等待中的 `pop` 返回 `None`。
    closed: AtomicBool,
}

impl DecodeQueue {
    fn new() -> Self {
        Self {
            jobs: std::sync::Mutex::new(VecDeque::with_capacity(DECODE_QUEUE)),
            notify: tokio::sync::Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// 锁住队列。持锁期间只做 `VecDeque` 操作（无 `await`），毒化按「继续用」处理。
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<DecodeJob>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 投递一条载荷（**不阻塞**，满时按参考链截断）。
    fn push(&self, job: DecodeJob) -> PushOutcome {
        if self.closed.load(Ordering::Acquire) {
            return PushOutcome::Closed;
        }
        let mut jobs = self.lock();
        if jobs.len() < DECODE_QUEUE {
            jobs.push_back(job);
            drop(jobs);
            self.notify.notify_one();
            return PushOutcome::Queued;
        }
        match job.kind() {
            // 补丁不可截断：丢一批会在合成帧上永久缺块，唯一安全动作是换一整帧重建。
            JobKind::Patch => return PushOutcome::Dropped,
            // 控制项正常不会在队列满时到达（每路 codec 至多一条，且紧接握手）；真到了就
            // 直接放行，队列短暂超一格——控制项数量本身有界。
            JobKind::Codec => {
                jobs.push_back(job);
                drop(jobs);
                self.notify.notify_one();
                return PushOutcome::Queued;
            }
            JobKind::Frame | JobKind::Keyframe => {}
        }
        // 满队列：先按参考链截断，再决定本包能不能进。
        //
        // 进来的**关键帧**把整条旧链作废（它自己是新起点），所以什么都不留——否则「守着一个
        // 旧基准 + 一堆旧帧」会让队列永远满着。普通帧只保留最新关键帧及其之后的部分。
        let incoming_keyframe = job.kind() == JobKind::Keyframe;
        let keep = if incoming_keyframe {
            vec![false; jobs.len()]
        } else {
            flush_keep(&jobs.iter().map(DecodeJob::kind).collect::<Vec<_>>())
        };
        let mut dropped_frames = 0_usize;
        let mut dropped_patches = 0_usize;
        let mut index = 0_usize;
        jobs.retain(|old| {
            let keep_it = keep[index];
            if !keep_it {
                match old.kind() {
                    JobKind::Frame | JobKind::Keyframe => dropped_frames += 1,
                    JobKind::Patch => dropped_patches += 1,
                    JobKind::Codec => {}
                }
            }
            index += 1;
            keep_it
        });
        // 本包自己就是关键帧时它就是新基准，否则看截断后有没有留下关键帧。
        let anchored = incoming_keyframe
            || jobs
                .iter()
                .any(|old| matches!(old.kind(), JobKind::Keyframe));
        // 有关键帧基准时，留下的帧与紧随其后的这一帧都能按序解码；截断后仍然满
        // （锚点就在队首、后面的帧已占满）或根本没有锚点时，本包不能进——它与留下的链之间
        // 已经断开了。
        let admitted = incoming_keyframe || (anchored && jobs.len() < DECODE_QUEUE);
        if admitted {
            jobs.push_back(job);
            drop(jobs);
            self.notify.notify_one();
        }
        PushOutcome::Flushed(FlushReport {
            dropped_frames,
            dropped_patches,
            admitted,
            anchored,
        })
    }

    /// 取一条待解码载荷；队列空则等通知，队列关闭（且排空）后返回 `None`。
    ///
    /// 取消安全：`Notify::notified()` 被丢弃不会丢通知（permit 由 `notify_one` 保存），
    /// 因此可以安全地放进 `select!`。
    async fn pop(&self) -> Option<DecodeJob> {
        loop {
            if let Some(job) = self.lock().pop_front() {
                return Some(job);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            self.notify.notified().await;
        }
    }

    /// 当前积压条数。
    fn len(&self) -> usize {
        self.lock().len()
    }

    /// 当前积压条数（`u32`，直接填 `QosFeedback` / 日志）。
    fn backlog(&self) -> u32 {
        u32::try_from(self.len()).unwrap_or(u32::MAX)
    }

    /// 队列里是否已经有一张关键帧（含正在被淘汰/刚入队的那张）。
    ///
    /// 解码任务用它抑制「多余的 IDR 请求」：参考链断了但关键帧已经在队列里排着，再请求一张
    /// 只会多花 200–400 KB 带宽。
    fn has_keyframe(&self) -> bool {
        self.lock().iter().any(|job| job.kind() == JobKind::Keyframe)
    }

    /// 关闭队列：等待中的 `pop` 立即醒来并返回 `None`。
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        // `notify_one` 会存 permit（`notify_waiters` 不会），故「检查 closed 之后才注册等待」
        // 的那一瞬也不会漏唤醒。
        self.notify.notify_one();
    }
}

/// 主循环与解码任务共享的原子读数/状态。
///
/// 都是 `Relaxed` 足够的观测值：写的是每帧热路径，读的是每秒一次或入队前的一次判断，
/// 不承担跨线程的同步顺序语义。打包成一个结构是为了避免 [`decode_loop`] 的参数爆炸。
struct DecodeShared {
    /// 最近一帧的解码耗时（毫秒）。
    decode_ms: Arc<AtomicU32>,
    /// 解码任务**已应用**到合成帧的补丁批数（主循环每秒 `swap(0)` 取走，填进
    /// [`SessionStats::Decode`] 的 `patches` 字段）。
    ///
    /// 计数放在解码侧而不是主循环：**收到**的补丁可能因等待关键帧/几何不符被丢弃，
    /// 只有真正交给 `on_patch` 的才算生效。
    applied_patches: Arc<AtomicU32>,
    /// 当前估计的**对端时钟相对本端偏差**（微秒；[`UNKNOWN_OFFSET`] = 尚未估计）。
    ///
    /// 主循环在心跳 ack 上更新它（最小 RTT 样本优先 + EMA），解码任务读它算端到端延迟。
    clock_offset_us: Arc<AtomicI64>,
    /// 解码任务最近一次算出的端到端延迟（毫秒；采集完成 → 解码完成）。
    e2e_ms: Arc<AtomicU32>,
    /// 诊断：主循环与解码任务共享的分阶段延迟窗口（见 [`crate::latency`]）。
    latency: Arc<std::sync::Mutex<LatencyWindow>>,
}

impl DecodeShared {
    fn new() -> Self {
        Self {
            decode_ms: Arc::new(AtomicU32::new(0)),
            applied_patches: Arc::new(AtomicU32::new(0)),
            clock_offset_us: Arc::new(AtomicI64::new(UNKNOWN_OFFSET)),
            e2e_ms: Arc::new(AtomicU32::new(0)),
            latency: Arc::new(std::sync::Mutex::new(LatencyWindow::new())),
        }
    }
}

/// UI 投递给控制器会话的一条命令。
///
/// 会话循环只有一个输入口：把输入事件与「切显示器 / 切画质 / 剪贴板 / 文件」放在同一个通道里，
/// 顺序天然与用户操作一致（例如先切显示器再点鼠标，不会被重排到切换之前）。
#[derive(Debug)]
pub enum ControllerCommand {
    /// 一条输入事件。
    Input(InputEvent),
    /// 切换观看的显示器。
    SelectDisplay(u32),
    /// 切换画质档位。
    SetQuality(QualityPreset),
    /// 切换帧率上限（30 / 60；被控端会钳到 `[min_fps, 120]`）。
    SetFrameRate(u8),
    /// 把本机剪贴板文本发给被控端。
    Clipboard(String),
    /// 请求被控端把它的剪贴板发过来。
    ClipboardRequest,
    /// 文件传输操作。
    File(TransferCommand),
    /// 请求被控端**锁定屏幕**（纯 UI 动作；被控端只在完全控制模式下执行）。
    LockScreen,
    /// 立即探测一次延迟（**纯进程内命令，不上线**）。
    ///
    /// **仅当当前无在途心跳样本时**才真正发一次 `Heartbeat`：否则会覆盖既有样本，让那次往返
    /// 的读数作废。用户切到会话窗口时 UI 可据此立刻刷一个读数，不必等下一个 [`HEARTBEAT_INTERVAL`]。
    Ping,
}

/// 控制器会话配置。
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// 会话 ID。
    pub session_id: String,
    /// 服务端下发的一次性会话 token。**同时用于 KDF 认证**（见 [`crate::crypto`] 模块文档），
    /// 必须与被控端持有的那个一致，否则会话密钥不一致、第一帧即认证失败。
    pub token: String,
    /// 本端（控制器）RustDesk ID。
    pub local_rd_id: String,
    /// 会话模式。
    pub mode: RemoteMode,
    /// 统计数据上报句柄（延迟读数）；无管理端时用 [`SessionStatsReporter::noop`]。
    pub stats: SessionStatsReporter,
    /// 可选的 H.264 解码器工厂（通常由 `crates/win-video` 注入）。
    ///
    /// `None` = 本端只能解 VP9，`CodecOffer.h264` 因此为 `false`，被控端不会选 H.264。
    pub h264_decoder_factory: Option<Arc<dyn VideoDecoderFactory>>,
    /// 数据面可靠层指标句柄（仅 UDP 打洞路径为 `Some`，见
    /// [`crate::transport::EstablishedStream::reliable`]）。
    ///
    /// 主循环每秒取一份快照与上一份求差，上报 [`SessionStats::Transport`]（A7 的重传/乱序
    /// 读数）。直连/中继路径没有本端可靠层，为 `None` 时**不发**该事件（没有数据就不该显示 0）。
    pub reliable: Option<Arc<ReliableMetrics>>,
}

/// 跑一次控制器会话，直到取消、对端关闭或出错（**不注入外部输入来源**）。
///
/// 这是 [`run_controller_with_input`] 的兼容入口：生产路径（[`crate::manager::RemoteManager`]）
/// 走带 [`InputBuffer`] 的那条，UI 输入因此合并鼠标移动、且抬起永不丢。这里保留旧签名给单测
/// 与无 UI 的调用方——输入仍可经 [`ControllerCommand::Input`] 投递。
///
/// # Errors
/// 同 [`run_controller_with_input`]。
pub async fn run_controller(
    duplex: BoxDuplex,
    config: ControllerConfig,
    handshake: ControllerHandshake,
    cmd_rx: mpsc::Receiver<ControllerCommand>,
    transfer: TransferHub,
    sink: std::sync::Arc<dyn FrameSink>,
    cancel: CancellationToken,
) -> RemoteResult<SessionEnd> {
    run_controller_with_input(
        duplex,
        config,
        handshake,
        cmd_rx,
        Arc::new(InputBuffer::new()),
        transfer,
        sink,
        cancel,
    )
    .await
}

/// 跑一次控制器会话，直到取消、对端关闭或出错（**带外部输入来源**）。
///
/// `input` 是 UI 经 [`crate::manager::RemoteManager::send_input`] 投递的输入缓冲：主循环用
/// **biased 优先**分支每轮先把它 drain 干净，因此按键按下/抬起不会被背压丢掉（鼠标移动则合并、
/// 满时只挤掉最旧的移动，见 [`InputBuffer`]）。`cmd_rx` 仍承载显示器/画质/剪贴板/文件等命令；
/// `sink` 接收已解码画面（[`FrameSink::on_frame`] 拿到的是 RGBA，不在 UI 线程解码）。
///
/// # Errors
/// 握手被拒、编解码失败、IO 失败时返回 [`RemoteError`]；正常结束（收到 `Bye` / 对端关闭 /
/// 取消 / 心跳超时）返回 `Ok(`[`SessionEnd`]`)`，由调用方据此判定是否需要自动重连。
#[expect(
    clippy::too_many_lines,
    reason = "握手校验 + 画面/输入/剪贴板/文件/心跳的单会话循环，拆开反而丢失控制流全貌"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "会话循环需要完整上下文；输入缓冲与命令通道必须分开（见 `InputBuffer`）"
)]
pub(crate) async fn run_controller_with_input(
    duplex: BoxDuplex,
    config: ControllerConfig,
    handshake: ControllerHandshake,
    mut cmd_rx: mpsc::Receiver<ControllerCommand>,
    input: Arc<InputBuffer>,
    mut transfer: TransferHub,
    sink: std::sync::Arc<dyn FrameSink>,
    cancel: CancellationToken,
) -> RemoteResult<SessionEnd> {
    let (read_half, write_half) = tokio::io::split(duplex);
    let mut reader = FramedRead::new(read_half, SessionCodec::new());
    let mut writer = FramedWrite::new(write_half, SessionCodec::new());

    // 1) 等 HelloAck（Hello 已由建立阶段流水线写出）。
    let ack = match timeout(HANDSHAKE_TIMEOUT, reader.next()).await {
        Ok(Some(Ok(frame))) => frame,
        Ok(Some(Err(err))) => return Err(RemoteError::Codec(err.to_string())),
        Ok(None) => return Err(RemoteError::Handshake("对端在握手前关闭连接".to_owned())),
        Err(_) => return Err(RemoteError::Handshake("等待 HelloAck 超时".to_owned())),
    };
    let SessionMessage::HelloAck {
        accepted,
        reason,
        displays,
        x25519_pub,
        hmac_ack,
    } = ack.message
    else {
        return Err(RemoteError::Handshake(format!(
            "期望 HelloAck，实得 {:?}",
            ack.message
        )));
    };
    if !accepted {
        return Err(RemoteError::Handshake(
            reason.unwrap_or_else(|| "被控端拒绝".to_owned()),
        ));
    }

    // 校验 HelloAck 的 MAC：证明被控端持有 token，且其临时公钥未被中继方替换。
    // 只构造不校验等于没做——这里必须真的比对。
    if !verify_ack_mac(
        &config.token,
        &config.session_id,
        &x25519_pub,
        &handshake.keypair.public_bytes(),
        &hmac_ack,
    ) {
        return Err(RemoteError::Handshake(
            "HelloAck 认证失败（token 不符或被中继方篡改）".to_owned(),
        ));
    }

    // 2) 派生密钥并装上加密（握手帧明文，此后全密文）。
    let shared = handshake.keypair.diffie_hellman(&x25519_pub)?;
    let responder_pub: [u8; 32] = x25519_pub
        .as_slice()
        .try_into()
        .map_err(|_| RemoteError::Auth("HelloAck 的 X25519 公钥长度非法".to_owned()))?;
    let keys = derive_session_keys(
        &config.token,
        &shared,
        &config.session_id,
        &handshake.keypair.public_bytes(),
        &responder_pub,
        true,
    );
    reader.decoder_mut().set_cipher(Cipher::new(keys.recv)?);
    writer.encoder_mut().set_cipher(Cipher::new(keys.send)?);
    debug!(session = %config.session_id, "控制器会话已加密");

    sink.on_displays(displays.clone());

    // 2.5) 编解码协商：先声明本端**能解码**的 codec，被控端据此（并与自己的编码能力求交集）
    // 回一条 `CodecSelect`。必须在任何画面帧之前到达，故紧接握手发出。
    let offer = SessionMessage::CodecOffer {
        h264: config.h264_decoder_factory.is_some(),
        vp9: true,
    };
    match send_cancellable(&mut writer, &cancel, SessionFrame::control(offer)).await {
        Ok(()) => {}
        Err(RemoteError::Cancelled) => {
            return Ok(SessionEnd {
                reason: "用户中断".to_owned(),
                deliberate: true,
            });
        }
        Err(err) => return Err(err),
    }

    // 2.6) 出站写任务：独占写半边，按 close → ctrl → screen → file 的优先级取帧。
    //
    // 握手阶段的 `CodecOffer` 仍直接写 writer（上面那一步），此后所有帧都只投队列。这样主循环
    // 不会在任何一次网络写上阻塞——对端停读/中继停走时，读画面、心跳、输入仍能继续轮询。
    // `queues` 与写任务共享同一组通道；`file_staging` 保证文件块在队列满时不丢字节。
    let (mut queues, receivers) = OutboundQueues::new();
    let writer_task = tokio::spawn(writer_loop(
        writer,
        receivers,
        cancel.clone(),
        None,
        // 控制器不产生 IDR（它只发输入/心跳/文件消息），IDR 去重是**被控端**的发送侧策略。
        None,
    ));

    // 解码任务：解码是 CPU 密集的**同步**调用，放在会话 `select!` 的分支里执行会让输入转发
    // 被它占住。故把解码挪到独立任务，循环只把原始载荷投进有界队列，输入/心跳照常得到轮询。
    //
    // 解码任务会占住运行时的**一个** worker 线程（解码期间不同让）。这是 CPU 密集工作的必然代价，
    // 且比原先占住会话循环本身好得多。
    let decode_queue = Arc::new(DecodeQueue::new());
    // 解码任务 → 循环的通知通道（无界：只承载低频控制/状态变化，**不上线**）。
    // 循环自己保留 `notice_tx`，故 `recv()` 永不返回 `None`（否则会 100% 空转）。
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<DecodeNotice>();
    // 主循环与解码任务共享的原子读数（见 [`DecodeShared`]）：解码耗时、补丁计数、时钟偏差、
    // 端到端延迟、延迟窗口。**参考链/等待关键帧的状态不在这里**——它由主循环的 [`ChainGate`]
    // 独立持有，解码侧只经 [`DecodeNotice`] 告知（见 `ChainGate` 的文档：共享一个原子会让
    // 「IDR 已入队」与「IDR 已解出」两个清除点互相覆盖）。
    let decode_shared = Arc::new(DecodeShared::new());
    let decode_task = tokio::spawn(decode_loop(
        Arc::clone(&decode_queue),
        notice_tx.clone(),
        std::sync::Arc::clone(&sink),
        cancel.clone(),
        Arc::clone(&decode_shared),
        config.h264_decoder_factory.clone(),
        config.stats.clone(),
    ));
    // 循环自己持有发送端（`recv()` 因此不会返回 `None`）；命名带下划线表明此处不再使用。
    let _notice_tx = notice_tx;

    // 3) 主循环。
    // 起点取 `now` 而非 `now + HEARTBEAT_INTERVAL`：**首个 tick 因此立即到期**（起点已过去），
    // 会话一建立就发一次心跳，用户开窗即有读数，不必空等一个 [`HEARTBEAT_INTERVAL`]。
    let mut heartbeat = interval_at(Instant::now(), HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 出站文件块：只在「有在途传输或有暂存块」时启用（否则该分支不被轮询，不产生空转）。
    let mut file_tick = interval_at(Instant::now(), CHUNK_PAUSE);
    file_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // QoS 反馈节拍：每秒一条 `QosFeedback`（驱动被控端 AIMD）+ 一次解码侧读数上报。
    // 起点取 `now + QOS_INTERVAL`：第一个样本要有一整秒的统计窗口才有意义。
    let mut qos_tick = interval_at(Instant::now() + QOS_INTERVAL, QOS_INTERVAL);
    qos_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // RTT 样本：发 `Heartbeat` 时把 `(发出时刻, t1_us)` 压入队列，收到 `HeartbeatAck` 时从队首
    // 弹出配对。`t1_us` 是本端单调时钟读数，用于估计两端时钟偏差（见 [`crate::clock`]）。
    //
    // 用队列而不是单个 `Option`：1s 探测在高 RTT 链路上会有多个心跳同时在途，单槽位会把迟到的
    // ack 与更新的心跳配错对。FIFO 配对最坏只让样本旧一个周期；上限 8 防止对端不应答时无限增长。
    let mut heartbeat_sent: VecDeque<(Instant, u64)> = VecDeque::new();
    // 时钟偏差估计状态：当前 offset + 对应的最小 RTT 样本。最小 RTT 样本直接覆盖，其余样本
    // 走 EMA（排队不对称会污染单次估计，最小 RTT 的那个样本污染最小）。
    let mut clock_sync: Option<(Duration, i64)> = None;
    // 最近一次收到 `HeartbeatAck` 的时刻：仅用于超时告警里区分「从未通过」与「刚刚断掉」，
    // 便于现场判断（不上线、不进协议）。
    let mut last_ack_at: Option<Instant> = None;
    // 最近一次可信 RTT（毫秒）：填进 `QosFeedback`。无样本时为 0（闭环此时按「未知」处理，
    // 不会误判为健康——`0 < fast_threshold` 但队列非空时仍可能判拥塞）。
    let mut last_rtt_ms: Option<u32> = None;
    // QoS 窗口内收到的画面字节数（算收码率）。
    let mut recv_bytes: u64 = 0;
    // 本窗口收到的视频包数 / 丢弃数（含等待 IDR 时主动丢掉的 P 帧与队列满）。
    let mut window_packets: u32 = 0;
    let mut window_dropped: u32 = 0;
    // 按原因的丢帧拆分：**只进延迟诊断日志**，不进 `SessionStats`（`dropped` 语义保持不变）。
    // `flushed` 是 `queue_full` 的子集：队列满时被**截断丢掉**的旧帧数（按参考链截断，见
    // [`flush_keep`]）；两者之差是被拒绝的补丁批。
    let mut window_drop_awaiting: u32 = 0;
    let mut window_drop_queue_full: u32 = 0;
    let mut window_flushed: u32 = 0;
    let mut qos_window_start = Instant::now();
    // 帧到达抖动（A5）：上一个视频包的到达时刻、再上一个到达间隔、本窗口的抖动累加与样本数。
    let mut last_packet_at: Option<Instant> = None;
    let mut prev_packet_interval: Option<Duration> = None;
    let mut jitter_sum_us: u64 = 0;
    let mut jitter_samples: u32 = 0;
    // 已上报给 UI 的编码尺寸（去重：`ScreenConfig` 与每个 `VideoPacket` 都会声明它）。
    let mut reported_dims: Option<(u32, u32)> = None;
    // 可靠层上一份快照（每秒求差 → `SessionStats::Transport`）。
    let mut last_transport: Option<ReliableSnapshot> = None;
    // 最近一次收到画面帧的时刻（「长时间无帧」看门狗用）。
    let mut last_frame_at = Instant::now();
    // 最近一次请求关键帧的时刻（限频，避免静止桌面上刷屏）。语义是**无帧看门狗**的 3s 限频。
    let mut last_keyframe_request_at: Option<Instant> = None;
    // 主循环独占的「参考链已断」闸门：见 [`ChainGate`]。
    let mut chain = ChainGate::default();
    // 关键帧请求闸门（单飞 + 去重 + 重试）：见 [`KeyframeGate`]。**只有它**发 `RequestKeyframe`，
    // 解码任务只负责把「链断了 / IDR 解出来了」告诉主循环。
    let mut keyframe_gate = KeyframeGate::default();
    // 重试节拍：只在等待期间被轮询（`if keyframe_gate.is_waiting()`），空闲时不产生唤醒。
    let mut keyframe_tick = interval_at(
        Instant::now() + KEYFRAME_REQUEST_INTERVAL,
        KEYFRAME_REQUEST_INTERVAL,
    );
    keyframe_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 自最近一次收到画面帧以来已发出的关键帧请求数：0 = 这次是首次请求（静止桌面的常规情况，
    // 只记 `debug`），≥1 = 上次请求没换回帧（对端可能真挂住，升到 `warn`）。收到帧即清零。
    let mut stall_requests: u32 = 0;
    // 被控端通告的 codec（收到 `CodecSelect` 之前为 `None`）。用于校验后续帧的声明。
    let mut selected_codec: Option<CodecId> = None;
    // 当前观看的显示器：请求关键帧要指名道姓（被控端只认自己正在采集的那台）。
    // 初值与 UI 的选择器一致：优先主显示器，否则第一台。
    let mut selected_display = displays
        .iter()
        .find(|d| d.primary)
        .or_else(|| displays.first())
        .map_or(0, |d| d.id);
    let mut reason = String::from("会话结束");

    let result: RemoteResult<SessionEnd> = 'session: loop {
        tokio::select! {
            // 取消优先（`biased`）：收尾时 `cancel.cancel()` 与 `lock_cmd().take()` 是背靠背的两条
            // 语句，本循环下一次轮询时两个分支**同时就绪**，而默认的随机轮询会任选其一——于是同一次
            // 「用户主动挂断」有一半概率走下面 `recv() == None` 那条，被记成 `deliberate: false`。
            // 那个 `false` 正是断线自动重连的唯一判据，本端会去重连用户刚刚亲手结束的会话；发给对端的
            // `Bye.reason` 也会变成「会话命令通道已关闭」。`biased` 让取消分支恒定胜出，判定不再随机
            // （与 `send_cancellable` 同一手法，见 `crates/remote/src/signaling.rs` 的取消优先）。
            // 命令通道**单独**关闭（未取消）仍走原分支：那是上层异常丢弃，不是用户中断。
            biased;
            () = cancel.cancelled() => {
                "用户中断".clone_into(&mut reason);
                break Ok(SessionEnd { reason: reason.clone(), deliberate: true });
            }
            // 输入**优先于**定时器与读分支：UI 产生的按键抬起不能被心跳/QoS 的投递挡在后面。
            // `drain` 一次性取走整批并按入队顺序写出，因此「先移动、再按下」的先后关系不变；
            // 输入走控制队列（写任务里优先级最高的一档），可取消地等空位、绝不丢弃。
            () = input.notified(), if input.has_pending() => {
                // 诊断：最老一条事件在本地缓冲里等了多久（UI 线程 → 会话循环）。
                // 正常应接近 0；持续偏大说明会话循环被别的分支占住。
                let (events, oldest_wait) = input.drain_with_oldest_age();
                if latency::enabled() {
                    if let Some(wait) = oldest_wait {
                        latency::record(
                            &decode_shared.latency,
                            Stage::InputQueue,
                            u64::try_from(wait.as_micros()).unwrap_or(u64::MAX),
                        );
                    }
                }
                for event in events {
                    if let Err(err) = queues
                        .enqueue_ctrl(&cancel, SessionFrame::control(SessionMessage::Input(event)))
                        .await
                    {
                        break 'session Err(err);
                    }
                }
            }
            _ = heartbeat.tick() => {
                // 存活检测：队首是最老的未应答心跳。超过 `LIVENESS_TIMEOUT` 还没配对，说明链路
                // 已经断了（中继停转发但 TCP 未断、接收方零窗口时对端不会给我们任何错误）。
                // **必须在下面容量上限的 `pop_front` 之前判**——上限（8）大于超时对应的 4 个周期，
                // 正常配置挤不到队首，但把判据放在前面才不会在将来调参后悄悄把超时掩盖掉。
                // 队列空（还没发过心跳）时 `front()` 为 `None`，不判。
                if heartbeat_sent.front().is_some_and(|(sent, _)| sent.elapsed() >= LIVENESS_TIMEOUT) {
                    let last_ack_ms = last_ack_at
                        .map(|at| u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX));
                    warn!(?last_ack_ms, "对端心跳超时（链路可能已中断），结束会话");
                    "对端心跳超时（链路可能已中断）".clone_into(&mut reason);
                    break Ok(SessionEnd { reason: reason.clone(), deliberate: false });
                }
                // FIFO 配对：迟到的 ack 认领最老的未应答心跳，而不是最新那个。
                if heartbeat_sent.len() >= HEARTBEAT_SENT_CAP {
                    heartbeat_sent.pop_front();
                }
                let t1_us = now_us();
                heartbeat_sent.push_back((Instant::now(), t1_us));
                if let Err(err) = queues
                    .enqueue_ctrl(
                        &cancel,
                        SessionFrame::control(SessionMessage::Heartbeat { t1_us }),
                    )
                    .await
                {
                    break Err(err);
                }
            }
            _ = qos_tick.tick() => {
                // 每秒一条 `QosFeedback`，驱动被控端 AIMD（见 [`crate::qos`]）。**不新增数据面
                // 变体**——`QosFeedback` 自阶段 2 起就在协议里，阶段 2 只是定义未消费。
                let elapsed = qos_window_start.elapsed().as_secs_f64().max(f64::EPSILON);
                let window_bytes = f64::from(u32::try_from(recv_bytes).unwrap_or(u32::MAX));
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "收码率为有界非负值，取整后落在 u32 内"
                )]
                let recv_kbps = (window_bytes * 8.0 / 1000.0 / elapsed).round() as u32;
                let decode_ms = decode_shared.decode_ms.load(Ordering::Relaxed);
                let backlog = decode_queue.backlog();
                let feedback = SessionMessage::QosFeedback {
                    rtt_ms: last_rtt_ms.unwrap_or(0),
                    recv_kbps,
                    decode_ms,
                    backlog,
                };
                // `QosFeedback` 在线上仍是 `Screen` lane，但内容只有几十字节且时延敏感，
                // 按 `outbound_class` 走控制队列（见 `OutboundClass`）。
                if let Err(err) = queues
                    .enqueue_ctrl(&cancel, SessionFrame::screen(feedback))
                    .await
                {
                    break Err(err);
                }
                // 同节拍把解码侧读数经进程内通道送到 UI（不落线）。`QosFeedback` 的字段
                // **不变**：收包/丢包/补丁/端到端/抖动只是本地观测，不进协议。
                let jitter_ms = if jitter_samples == 0 {
                    0
                } else {
                    u32::try_from(jitter_sum_us / u64::from(jitter_samples) / 1000).unwrap_or(u32::MAX)
                };
                config.stats.report(SessionStats::Decode {
                    decode_ms,
                    recv_kbps,
                    packets: window_packets,
                    dropped: window_dropped,
                    patches: decode_shared.applied_patches.swap(0, Ordering::Relaxed),
                    e2e_ms: decode_shared.e2e_ms.load(Ordering::Relaxed),
                    backlog,
                    jitter_ms,
                    clock_offset_us: decode_shared.clock_offset_us.load(Ordering::Relaxed),
                });
                // 诊断：每秒一行分阶段延迟汇总（**只在延迟诊断打开时**）。
                // 与上面的 `SessionStats::Decode` 分开：那条是 UI 角标读数，这条是逐段拆解。
                if latency::enabled() {
                    if let Some(summary) = latency::take_summary(&decode_shared.latency) {
                        // 全部信息塞进 message：GUI 日志面板 / 服务端日志只保留 `message` 字段。
                        //
                        // `flushed` 是 `queue_full` 的子集：队列满时被**按参考链截断**丢掉的
                        // 旧帧数；两者之差是被拒绝的补丁批。`stale` 已随按年龄老化一起删除——
                        // 截断点只看参考链（见 [`flush_keep`]），不看时间戳。
                        info!(
                            target: "latency",
                            "controller packets={} dropped={} drop_split=awaiting:{} queue_full:{} flushed:{} rtt_ms={} {}",
                            window_packets,
                            window_dropped,
                            window_drop_awaiting,
                            window_drop_queue_full,
                            window_flushed,
                            last_rtt_ms.unwrap_or(0),
                            summary
                        );
                    }
                }
                // 可靠层每秒增量（A7）。只有打洞路径有句柄；直连/中继不发（见 `ControllerConfig`）。
                if let Some(metrics) = &config.reliable {
                    let now_snapshot = metrics.snapshot();
                    if let Some(prev) = last_transport {
                        config.stats.report(SessionStats::Transport(transport_delta(prev, now_snapshot)));
                    }
                    last_transport = Some(now_snapshot);
                }
                recv_bytes = 0;
                window_packets = 0;
                window_dropped = 0;
                window_drop_awaiting = 0;
                window_drop_queue_full = 0;
                window_flushed = 0;
                jitter_sum_us = 0;
                jitter_samples = 0;
                qos_window_start = Instant::now();

                // 「长时间无帧」看门狗（阶段 3）：对端采集/编码挂住、或中继半死时，画面会永久冻结
                // 且毫无报错。静止桌面**合法地**不产帧（被控端 memcmp 去重），故这不是错误检测——
                // 只请求一次关键帧并按同一窗口限频（见 [`NO_FRAME_TIMEOUT`]）。
                if last_frame_at.elapsed() >= NO_FRAME_TIMEOUT {
                    let due = last_keyframe_request_at.is_none_or(|t| t.elapsed() >= NO_FRAME_TIMEOUT);
                    if due {
                        // **静止桌面上这条看门狗每 3s 必触发一次，且是假警报**：被控端的 memcmp 去重
                        // 让静止画面合法地不产帧，关键帧请求会立刻换回一帧。所以常规情况只记 `debug`——
                        // `warn` 会被 `ClientLogLayer` 批量上报到服务端并落 SQLite，一个静止会话就是
                        // 1200 条/小时，既冲垮服务端日志库也淹没真正的告警。只有**上一次请求也没换回帧**
                        // （对端采集/编码真的挂住）才升到 `warn`，那时它才是真信号。
                        if stall_requests == 0 {
                            debug!(
                                since_ms = last_frame_at.elapsed().as_millis(),
                                "长时间未收到画面帧，请求关键帧"
                            );
                        } else {
                            warn!(
                                since_ms = last_frame_at.elapsed().as_millis(),
                                stall_requests,
                                "请求关键帧后仍无画面帧，对端采集或编码可能已挂住"
                            );
                        }
                        stall_requests = stall_requests.saturating_add(1);
                        let request = SessionMessage::RequestKeyframe {
                            display: selected_display,
                        };
                        if let Err(err) = queues
                            .enqueue_ctrl(&cancel, SessionFrame::screen(request))
                            .await
                        {
                            break Err(err);
                        }
                        config.stats.report(SessionStats::KeyframeRequest {
                            reason: KeyframeReason::StalledNoFrame,
                        });
                        last_keyframe_request_at = Some(Instant::now());
                    }
                }
            }
            maybe_cmd = cmd_rx.recv() => {
                // 发送端被 drop = 会话已被上层收走，按正常结束处理。**不能 `continue`**：已关闭的
                // `mpsc::Receiver::recv()` 每次都立刻返回 `Ready(None)`，继续轮询会 100% 空转。
                let Some(cmd) = maybe_cmd else {
                    "会话命令通道已关闭".clone_into(&mut reason);
                    break Ok(SessionEnd { reason: reason.clone(), deliberate: false });
                };
                match cmd {
                    ControllerCommand::Input(event) => {
                        // 兼容入口（无 UI 的调用方仍可经命令通道投递输入）。生产路径走
                        // `InputBuffer`，两者最终都进控制队列。
                        if let Err(err) = queues
                            .enqueue_ctrl(&cancel, SessionFrame::control(SessionMessage::Input(event)))
                            .await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::SelectDisplay(display) => {
                        if let Err(err) = queues
                            .enqueue_ctrl(
                                &cancel,
                                SessionFrame::control(SessionMessage::SelectDisplay { display }),
                            )
                            .await
                        {
                            break Err(err);
                        }
                        // 记住当前观看的显示器：`RequestKeyframe` 要指名道姓（被控端只认自己正在
                        // 采集的那台）。
                        selected_display = display;
                    }
                    ControllerCommand::SetQuality(preset) => {
                        if let Err(err) = queues
                            .enqueue_ctrl(
                                &cancel,
                                SessionFrame::control(SessionMessage::SetQuality { preset }),
                            )
                            .await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::SetFrameRate(fps) => {
                        if let Err(err) = queues
                            .enqueue_ctrl(
                                &cancel,
                                SessionFrame::control(SessionMessage::SetFrameRate { fps }),
                            )
                            .await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::Clipboard(text) => {
                        if let Err(err) = queues
                            .enqueue_ctrl(&cancel, SessionFrame::control(SessionMessage::Clipboard { text }))
                            .await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::ClipboardRequest => {
                        if let Err(err) = queues
                            .enqueue_ctrl(&cancel, SessionFrame::control(SessionMessage::ClipboardRequest))
                            .await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::LockScreen => {
                        // v7 的锁屏是控制帧，走控制队列（与其它小控制消息同一优先级）。
                        if let Err(err) = queues
                            .enqueue_ctrl(&cancel, SessionFrame::control(SessionMessage::LockScreen))
                            .await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::File(cmd) => match transfer.on_command(cmd).await {
                        // 按 `outbound_class` 分流：Chunk/Done/Error 走 file（可暂存），其余走控制队列。
                        Ok(Some(msg)) => {
                            if let Err(err) = queues
                                .dispatch_frame(&cancel, SessionFrame::file(msg))
                                .await
                            {
                                break Err(err);
                            }
                        }
                        Ok(None) => {}
                        Err(err) => warn!(error = %err, "文件操作发起失败"),
                    },
                    ControllerCommand::Ping => {
                        // 已有在途样本时不打扰：否则会多一条在途心跳，让用户此刻想要的读数被推迟。
                        if heartbeat_sent.is_empty() {
                            let t1_us = now_us();
                            heartbeat_sent.push_back((Instant::now(), t1_us));
                            if let Err(err) = queues
                                .enqueue_ctrl(
                                    &cancel,
                                    SessionFrame::control(SessionMessage::Heartbeat { t1_us }),
                                )
                                .await
                            {
                                break Err(err);
                            }
                        }
                    }
                }
            }
            // 解码任务的通知：参考链断了（需要一张 IDR）/ 关键帧已解出（等待结束）。
            // `notice_tx` 常驻于本函数 → `recv()` 不会返回 `None`，本分支只在真有事件时触发。
            // 原因只进管理面（进程内上报），线上 `RequestKeyframe` 仍只有 `display` 一个字段。
            Some(notice) = notice_rx.recv() => {
                match notice {
                    DecodeNotice::Broken { display, reason } => {
                        // 解码侧发现参考链断了：闸门立刻开始丢后续 P 帧（直到下一张关键帧
                        // **入队**为止，见 [`ChainGate`]），并按 [`KeyframeGate`] 的去重/重试
                        // 规则决定现在要不要真的发一次请求。
                        //
                        // 但队列里**已经排着**一张关键帧时不能关闸门：那条通知已经过时
                        // （关键帧一到就恢复），关上去反而会一直关到下一张关键帧*入队*，
                        // 把「一张 IDR 就能恢复」拖成两次 IDR。
                        if !decode_queue.has_keyframe() {
                            chain.on_drop();
                        }
                        if let Some(request) = keyframe_gate.on_break(display, reason, Instant::now()) {
                            if let Err(err) =
                                dispatch_keyframe_request(&mut queues, &cancel, &config.stats, request)
                                    .await
                            {
                                break 'session Err(err);
                            }
                        }
                    }
                    // IDR 已解出：本次等待事件结束，清空重试状态（下一次断链会立刻重发）。
                    DecodeNotice::Satisfied => keyframe_gate.satisfied(),
                }
            }
            // 等待 IDR 期间的定时重试：**保证只要还在等就一定有请求在路上**。
            //
            // 旧实现把重试放在解码任务里（`kf_tick`），而解码任务正卡在几百毫秒的
            // `spawn_blocking` 里，重试被一起挡住；主循环侧又有三个互不相干的限频器，
            // 请求被任何一个吞掉都没有人补发。现在只有 [`KeyframeGate`] 一处状态。
            //
            // 分支带 `if` 守卫：不等待时不轮询该 interval（不产生空闲唤醒）。
            _ = keyframe_tick.tick(), if keyframe_gate.is_waiting() => {
                if let Some(request) = keyframe_gate.on_tick(Instant::now()) {
                    if let Err(err) =
                        dispatch_keyframe_request(&mut queues, &cancel, &config.stats, request).await
                    {
                        break 'session Err(err);
                    }
                }
            }
            // 文件块生产**永不阻塞主循环**：先补投暂存区，再在 file 队列确实有空位时才读下一块。
            // `dispatch_frame` 对 File 类只做 `try_send`（满了留暂存区），绝不 `.await` 空位，
            // 因此上传/下载期间画面读取、输入与心跳都不会被文件传输堵住。
            _ = file_tick.tick(), if transfer.has_outgoing() || queues.has_staged_file() => {
                if let Err(err) = queues.drain_file() {
                    break Err(err);
                }
                if queues.file_has_room() && transfer.has_outgoing() {
                    match transfer.next_outbound().await {
                        Ok(Some(msg)) => {
                            if let Err(err) = queues
                                .dispatch_frame(&cancel, SessionFrame::file(msg))
                                .await
                            {
                                break Err(err);
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            warn!(error = %err, "文件读取失败，中断该次传输");
                            // 回发 `FileError`：发送方不再有 chunk 流动，对端的 `Incoming` 会一直挂着。
                            if let Err(send_err) =
                                send_file_errors(&mut queues, &cancel, transfer.abort_all(&err.to_string())).await
                            {
                                break Err(send_err);
                            }
                        }
                    }
                }
            }
            incoming = reader.next() => {
                match incoming {
                    Some(Ok(frame)) => match frame.message {
                        SessionMessage::VideoPacket {
                            display,
                            seq,
                            codec,
                            flags,
                            width,
                            height,
                            data,
                            capture_ts_us,
                        } => {
                            // 只投递原始载荷；解码在独立任务里做（见 `decode_loop`）。
                            // 同时记收码率窗口与「最近收帧时刻」（QoS 反馈 / 无帧看门狗用）。
                            recv_bytes = recv_bytes.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                            // 帧到了：看门狗若是「首次请求」路径，计数回到 0，下次仍只记 `debug`。
                            // 注意 `last_frame_at` 衡量的是「收到包」，不是「解出帧」，故下面
                            // 主动丢弃 P 帧时也要更新它。
                            stall_requests = 0;
                            last_frame_at = Instant::now();
                            window_packets = window_packets.saturating_add(1);
                            // 帧到达抖动（A5）：相邻到达间隔的**二阶差分**，间隔 > 1s 的样本
                            // 丢弃（那是「重新开始」，不是抖动）。
                            if let Some(prev_at) = last_packet_at.replace(Instant::now()) {
                                let interval = prev_at.elapsed();
                                if interval <= Duration::from_secs(1) {
                                    if let Some(prev_interval) = prev_packet_interval {
                                        let delta = interval.abs_diff(prev_interval);
                                        jitter_sum_us = jitter_sum_us
                                            .saturating_add(u64::try_from(delta.as_micros()).unwrap_or(u64::MAX));
                                        jitter_samples = jitter_samples.saturating_add(1);
                                    }
                                    prev_packet_interval = Some(interval);
                                } else {
                                    prev_packet_interval = None;
                                }
                            }
                            // 编码尺寸（A2）：`ScreenConfig` 也会报，这里按包兜底去重。
                            if reported_dims != Some((width, height)) {
                                reported_dims = Some((width, height));
                                config.stats.report(SessionStats::EncodeDims { width, height });
                            }
                            if selected_codec.is_some_and(|selected| selected != codec) {
                                debug!(?codec, ?selected_codec, "视频包 codec 与已选 codec 不一致");
                            }
                            let keyframe = flags & VIDEO_FLAG_KEYFRAME != 0;
                            // 第一道闸门：参考链断了、且还没有新的关键帧**入队** → P 帧不入队。
                            //
                            // 旧实现读的是解码任务共享的 `awaiting_keyframe`，而它在 **IDR 解码
                            // 成功之后**才清；真机上一次 IDR 解码本身就可能 445 ms（DXVA 回读
                            // 停顿，卷宗 §2.1），这段时间里到达的 2–5 张 P 帧全被丢掉 → 每张都让
                            // 解码任务的序号出现断层 → 解码任务再重置一次、再请一张 IDR →
                            // 一次事件换回 3 张 IDR（91 秒 27 张，39% 带宽）。
                            //
                            // 正确语义是「直到 IDR **已入队**」：IDR 一旦进入队列，它之后的 P 帧
                            // 在队列里天然排在它后面、可以按序解码，闸门必须立刻打开；只有
                            // 「请求已发出、IDR 还没到」期间到达的 P 帧才必须丢。清除点见下面
                            // `chain.on_keyframe_enqueued()`。[`ChainGate`] 有单测锁住这一点。
                            if chain.should_drop(keyframe) {
                                window_dropped = window_dropped.saturating_add(1);
                                window_drop_awaiting = window_drop_awaiting.saturating_add(1);
                                if latency::enabled() {
                                    info!(
                                        target: "latency",
                                        "controller_drop seq={} reason=awaiting",
                                        seq
                                    );
                                }
                                debug!("等待关键帧期间丢弃 P 帧（不让它占用解码队列）");
                                continue;
                            }
                            let backlog = decode_queue.backlog();
                            // 队列满时**按参考链截断**（见 [`flush_keep`]）：留下最新关键帧及其
                            // 之后的帧（参考链完好，不会花屏），丢掉它之前那段作废的链。
                            match decode_queue.push(DecodeJob::Packet {
                                display,
                                seq,
                                codec,
                                flags,
                                width,
                                height,
                                data,
                                capture_ts_us,
                                recv_us: now_us(),
                            }) {
                                PushOutcome::Queued => {
                                    // **闸门的清除点**：IDR 已入队，后面的 P 帧可以正常入队。
                                    if keyframe {
                                        chain.on_keyframe_enqueued();
                                    }
                                }
                                PushOutcome::Flushed(report) => {
                                    // 本包被拒时它自己也算一帧丢弃。
                                    let dropped = report
                                        .dropped_frames
                                        .saturating_add(usize::from(!report.admitted));
                                    let dropped_u32 = u32::try_from(dropped).unwrap_or(u32::MAX);
                                    window_dropped = window_dropped.saturating_add(dropped_u32);
                                    window_drop_queue_full =
                                        window_drop_queue_full.saturating_add(dropped_u32);
                                    window_flushed = window_flushed.saturating_add(dropped_u32);
                                    if latency::enabled() {
                                        // `flushed=n` 是**截断丢掉**的旧帧数；`admitted=false`
                                        // 表示本包也一起丢了（它与留下的链之间已断开）。
                                        info!(
                                            target: "latency",
                                            "controller_drop seq={} reason=queue_full flushed={} patches={} kept_from_keyframe={} admitted={} backlog={}",
                                            seq,
                                            report.dropped_frames,
                                            report.dropped_patches,
                                            report.anchored,
                                            report.admitted,
                                            backlog
                                        );
                                    }
                                    if report.admitted {
                                        // 截断后队列里有关键帧基准：留下的帧与紧随其后的这一帧
                                        // 都能按序解码，闸门打开、不必再请 IDR。
                                        chain.on_keyframe_enqueued();
                                    } else {
                                        // 参考链上留了缺口（大概率是整队清空）：关闸门并请求
                                        // 一张 IDR，后续 P 帧一律不入队。
                                        chain.on_drop();
                                        if let Some(request) = keyframe_gate.on_break(
                                            display,
                                            KeyframeReason::QueueOverflow,
                                            Instant::now(),
                                        ) {
                                            if let Err(err) = dispatch_keyframe_request(
                                                &mut queues,
                                                &cancel,
                                                &config.stats,
                                                request,
                                            )
                                            .await
                                            {
                                                break 'session Err(err);
                                            }
                                        }
                                    }
                                }
                                // 视频包永远可截断/可入队（见 `DecodeQueue::push`）。
                                PushOutcome::Dropped => {
                                    debug!(seq, "视频包被解码队列拒绝（不应发生）");
                                }
                                PushOutcome::Closed => {
                                    break Err(RemoteError::Platform(
                                        "画面解码任务已结束".to_owned(),
                                    ));
                                }
                            }
                        }
                        // `display` 会与 tracing 宏展开里引入的 `field::display` 撞名，故绑定名用 monitor。
                        SessionMessage::RegionUpdate {
                            display: monitor,
                            width,
                            height,
                            rects,
                            capture_ts_us,
                        } => {
                            // 补丁也是「收到画面帧」：它证明对端采集/编码还活着，「长时间无帧」
                            // 看门狗必须因此保持安静（否则静止桌面在补丁模式下每 3s 白请 IDR）。
                            stall_requests = 0;
                            last_frame_at = Instant::now();
                            // 抖动读数跨模式切换不采样：补丁模式期间没有视频包，若沿用上一个
                            // 视频包的到达时刻，模式切换那一刻会被算成一个巨大的「帧间隔」，
                            // 把 A5 抖动读数冲高。补丁到达即视为序列中断，重新起算相邻视频包
                            // 间隔——抖动的定义不变（仍是相邻视频包到达间隔的二阶差分）。
                            last_packet_at = None;
                            prev_packet_interval = None;
                            // 空批 = 保活：不入队、不重绘，只让上面的看门狗计时生效。
                            if rects.is_empty() {
                                continue;
                            }
                            // 补丁载荷同样是接收码率的一部分：补丁模式下几乎没有全帧，
                            // 只统计 `VideoPacket` 会让 `recv_kbps` 失真。
                            let patch_bytes = rects.iter().fold(0_u64, |acc, rect| {
                                acc.saturating_add(
                                    u64::try_from(rect.data.len()).unwrap_or(u64::MAX),
                                )
                            });
                            recv_bytes = recv_bytes.saturating_add(patch_bytes);
                            // 廉价校验放在主循环：畸形几何/超量矩形不该占用解码队列。
                            // 补丁**不允许静默丢弃**——丢弃必须换回一个关键帧。
                            if width == 0
                                || height == 0
                                || width > MAX_FRAME_DIM
                                || height > MAX_FRAME_DIM
                                || !width.is_multiple_of(2)
                                || !height.is_multiple_of(2)
                                || rects.len() > MAX_REGION_RECTS
                            {
                                debug!(
                                    monitor,
                                    width,
                                    height,
                                    rects = rects.len(),
                                    "补丁批的帧几何非法，丢弃并请求关键帧"
                                );
                                if let Some(request) =
                                    keyframe_gate.on_break(monitor, KeyframeReason::DecodeError, Instant::now())
                                {
                                    if let Err(err) = dispatch_keyframe_request(
                                        &mut queues,
                                        &cancel,
                                        &config.stats,
                                        request,
                                    )
                                    .await
                                    {
                                        break 'session Err(err);
                                    }
                                }
                                continue;
                            }
                            // 补丁入队同样**不阻塞**：队列满时拒绝本批（补丁不可淘汰，见
                            // [`DecodeQueue`]），换一整帧重建合成帧。
                            match decode_queue.push(DecodeJob::Region {
                                display: monitor,
                                width,
                                height,
                                rects,
                                capture_ts_us,
                                recv_us: now_us(),
                            }) {
                                // 补丁只会是 `Queued` 或 `Dropped`（截断只对视频包生效）。
                                PushOutcome::Queued | PushOutcome::Flushed(_) => {}
                                PushOutcome::Dropped => {
                                    // 补丁不是参考链上的帧，**不**动 [`ChainGate`]：丢一批补丁
                                    // 不会让后续 P 帧不可解码，只需要一张全帧把合成帧重建。
                                    if latency::enabled() {
                                        // 补丁不计入 `window_drop_*`（那套计数器沿用视频包
                                        // 口径，与 `SessionStats::Decode::dropped` 对齐）。
                                        info!(
                                            target: "latency",
                                            "controller_drop seq=- reason=queue_full kind=patch monitor={}",
                                            monitor
                                        );
                                    }
                                    debug!(monitor, "解码队列已满，丢弃补丁批并请求关键帧");
                                    if let Some(request) = keyframe_gate.on_break(
                                        monitor,
                                        KeyframeReason::QueueOverflow,
                                        Instant::now(),
                                    ) {
                                        if let Err(err) = dispatch_keyframe_request(
                                            &mut queues,
                                            &cancel,
                                            &config.stats,
                                            request,
                                        )
                                        .await
                                        {
                                            break 'session Err(err);
                                        }
                                    }
                                }
                                PushOutcome::Closed => {
                                    break Err(RemoteError::Platform(
                                        "画面解码任务已结束".to_owned(),
                                    ));
                                }
                            }
                        }
                        // `display` 会与 tracing 宏展开里引入的 `field::display` 撞名，故绑定名用 monitor。
                        SessionMessage::ScreenConfig { display: monitor, codec, width, height } => {
                            // 编码尺寸/codec 的通告。解码器本身能在关键帧处跟随分辨率变化
                            // （由 `video` 的单测锁定），故这里只记录、不重建。
                            if selected_codec.is_some_and(|selected| selected != codec) {
                                debug!(monitor, ?codec, ?selected_codec, "ScreenConfig codec 与已选 codec 不一致");
                            }
                            if reported_dims != Some((width, height)) {
                                reported_dims = Some((width, height));
                                config.stats.report(SessionStats::EncodeDims { width, height });
                            }
                            debug!(monitor, ?codec, width, height, "收到屏幕配置");
                        }
                        SessionMessage::CodecSelect { codec } => {
                            // 被控端在开始出帧前通告最终 codec。本端必须真的能解它；否则继续
                            // 解码只会每帧失败并无限请求关键帧，不如明确报协议错误。
                            if codec == CodecId::H264 && config.h264_decoder_factory.is_none() {
                                break Err(RemoteError::Codec(
                                    "被控端选择了本端未提供的 H.264 编解码器".to_owned(),
                                ));
                            }
                            if selected_codec != Some(codec) {
                                // 换 codec 意味着参考链作废：解码任务会重置解码器并等关键帧。
                                selected_codec = Some(codec);
                                if matches!(
                                    decode_queue.push(DecodeJob::SelectCodec(codec)),
                                    PushOutcome::Closed
                                ) {
                                    break Err(RemoteError::Platform(
                                        "画面解码任务已结束".to_owned(),
                                    ));
                                }
                                // 换 codec 后解码器是新的，下一张关键帧之前不能解码任何 P 帧。
                                chain.on_drop();
                                // A1：协商结果上报本端 UI（角标显示 codec）。
                                config.stats.report(SessionStats::Codec { codec });
                            }
                            info!("编解码协商完成（控制器） codec={codec:?}");
                        }
                        SessionMessage::RequestKeyframe { .. } => {
                            // 该变体只应由**控制器**发出；收到即说明对端实现异常。
                            debug!("控制器收到 RequestKeyframe，忽略");
                        }
                        SessionMessage::QosFeedback { .. } => {
                            // 该变体只应由**控制器**发出；收到即说明对端实现异常。
                            debug!("控制器收到 QosFeedback，忽略");
                        }
                        SessionMessage::DisplayList(list) => sink.on_displays(list),
                        SessionMessage::Clipboard { text } => sink.on_clipboard(text),
                        SessionMessage::ClipboardRequest => {
                            // 引擎里**没有**任何路径会让被控端发出该请求（它只负责应答），且控制器
                            // 侧根本没有 `Clipboard` 实现可应答。收到即说明对端实现异常，记一条日志。
                            debug!("控制器收到 ClipboardRequest，但控制器不实现剪贴板应答，忽略");
                        }
                        SessionMessage::Heartbeat { .. } => {
                            // 本端是**应答方**：t2 = 收到时刻、t3 = 发出时刻（都是本端单调时钟）。
                            // 发起方用 t1..t4 估计两端时钟偏差（见 [`crate::clock`]）。
                            let t2_us = now_us();
                            let ack = SessionMessage::HeartbeatAck {
                                t2_us,
                                t3_us: now_us(),
                            };
                            if let Err(err) = queues
                                .enqueue_ctrl(&cancel, SessionFrame::control(ack))
                                .await
                            {
                                break Err(err);
                            }
                        }
                        SessionMessage::HeartbeatAck { t2_us, t3_us } => {
                            let now = Instant::now();
                            let t4_us = now_us();
                            last_ack_at = Some(now);
                            if let Some((sent, t1_us)) = heartbeat_sent.pop_front() {
                                if let Some(rtt) = trusted_rtt(sent, now) {
                                    // 存一份毫秒读数：`QosFeedback` 要用它填 `rtt_ms`。
                                    last_rtt_ms = Some(u32::try_from(rtt.as_millis()).unwrap_or(u32::MAX));
                                    config.stats.report(SessionStats::Rtt(rtt));
                                    // 时钟偏差：最小 RTT 样本直接覆盖（排队最少的那个最可信），
                                    // 其余样本走 1/8 EMA。偏差未知时先落最小 RTT 样本。
                                    let sample = clock_offset_us(t1_us, t2_us, t3_us, t4_us);
                                    let offset = match clock_sync {
                                        None => {
                                            clock_sync = Some((rtt, sample));
                                            sample
                                        }
                                        Some((min_rtt, _old)) if rtt <= min_rtt => {
                                            clock_sync = Some((rtt, sample));
                                            sample
                                        }
                                        Some((min_rtt, old)) => {
                                            let smoothed = i64::try_from(
                                                (i128::from(old) * 7 + i128::from(sample)) / 8,
                                            )
                                            .unwrap_or(old);
                                            clock_sync = Some((min_rtt, smoothed));
                                            smoothed
                                        }
                                    };
                                    decode_shared
                                        .clock_offset_us
                                        .store(offset, Ordering::Relaxed);
                                }
                            }
                        }
                        // 被控端每秒上报的整机遥测：只进 UI（不参与任何控制决策）。
                        SessionMessage::HostStats(host) => {
                            config.stats.report(SessionStats::Host(Box::new(host)));
                        }
                        // 远端光标形状：交给 UI 决定是否跟随（只有完全控制模式会应用；
                        // `Hidden` / `Unknown` 显示默认箭头）。不影响会话本身。
                        SessionMessage::CursorShape { shape } => sink.on_cursor(shape),
                        SessionMessage::Bye { reason: r } => {
                            reason = r;
                            break Ok(SessionEnd { reason: reason.clone(), deliberate: true });
                        }
                        file @ (SessionMessage::FileList { .. }
                        | SessionMessage::FileListRequest { .. }
                        | SessionMessage::FileDownloadRequest { .. }
                        | SessionMessage::FileUploadRequest { .. }
                        | SessionMessage::FileAccept { .. }
                        | SessionMessage::FileChunk { .. }
                        | SessionMessage::FileDone { .. }
                        | SessionMessage::FileError { .. }) => {
                            match transfer.on_message(file).await {
                                Ok(replies) => {
                                    let mut write_err = None;
                                    for reply in replies {
                                        // `FileDone`/`FileError`/数据块按分类走 file 队列（可暂存），
                                        // 其余小消息走控制队列；两条队列各自保序。
                                        if let Err(err) = queues
                                            .dispatch_frame(&cancel, SessionFrame::file(reply))
                                            .await
                                        {
                                            write_err = Some(err);
                                            break;
                                        }
                                    }
                                    if let Some(err) = write_err {
                                        break Err(err);
                                    }
                                }
                                Err(err) => {
                                    // 本地落盘失败：这次传输已不可信，报错并中断它，会话本身继续。
                                    warn!(error = %err, "文件接收失败，中断该次传输");
                                    if let Err(send_err) =
                                        send_file_errors(&mut queues, &cancel, transfer.abort_all(&err.to_string())).await
                                    {
                                        break Err(send_err);
                                    }
                                }
                            }
                        }
                        other => debug!(?other, "控制器收到未处理的消息"),
                    },
                    Some(Err(err)) => break Err(RemoteError::Codec(err.to_string())),
                    None => {
                        "对端已关闭连接".clone_into(&mut reason);
                        break Ok(SessionEnd { reason: reason.clone(), deliberate: false });
                    }
                }
            }
        }
    };

    // 取消也可能发生在某个 `send_cancellable` 里（对端停读、写缓冲填满时）：统一按「用户中断」
    // 正常收尾，与上面的 `cancel` 分支语义一致。
    let result = match result {
        Err(RemoteError::Cancelled) => {
            "用户中断".clone_into(&mut reason);
            Ok(SessionEnd {
                reason: reason.clone(),
                deliberate: true,
            })
        }
        other => other,
    };

    // 4) 收尾：尽力通知对端（限时，见 `BYE_TIMEOUT`），并把在途传输标记为失败（UI 不能停在 99%）。
    //    这里**不必**回发 `abort_all` 的 `FileError`——`Bye` 马上就到，对端收尾时同样会清掉自己
    //    的在途传输。
    //
    //    `Bye` 走 close 通道（写任务里最高优先级、不理会取消）；随后丢弃普通队列的发送端，
    //    写任务在写出 `Bye` 后自然退出。给它一个上界：写任务可能正卡在停读的对端上，超时即
    //    abort（丢弃写半边 = 关闭 socket）。
    let _ = transfer.abort_all(&reason);
    queues.close(SessionFrame::control(SessionMessage::Bye {
        reason: reason.clone(),
    }));
    drop(queues);
    let mut writer_task = writer_task;
    let _ = timeout(BYE_TIMEOUT, &mut writer_task).await;
    writer_task.abort();
    // 解码任务持有 `sink`：必须先让它停下，`on_closed` 之后才不会有画面再投进来。
    // 关闭队列让解码任务的 `pop()` 返回 `None` 并退出（它会把已入队的载荷解完）。
    decode_queue.close();
    let mut decode_task = decode_task;
    let _ = timeout(BYE_TIMEOUT, &mut decode_task).await;
    decode_task.abort();
    sink.on_closed(reason);
    result
}

/// 画面解码任务：从 [`DecodeQueue`] 取原始载荷，串行解码后交给 [`FrameSink`]。
///
/// 独立成任务是为了把 CPU 密集的解码移出会话 `select!` 循环（见 `run_controller` 的说明）。
/// 取消一旦触发即退出；队列关闭（会话结束）时把已入队的载荷解完再退出，保证顺序不乱。
///
/// # 丢弃与恢复（**改动前先读这段**）
///
/// 控制器**可以**丢帧（解码跟不上时不可避免），但视频是时域编码：丢掉一个 P 帧会断掉参考链，
/// 后续引用它的帧在解码端永远解不出来。故本任务维护「参考链是否完好」这一状态：
///
/// - **参考链断了**（序号断层 / 解码失败 / 配置失败 / 补丁没有基底）：进入 `awaiting_keyframe`
///   并**丢弃后续非关键帧**，直到一张关键帧被取出。
/// - **关键帧一被取出就恢复**：IDR 是新的参考链起点，排在它之后的 P 帧可以按序解码，不必等
///   IDR 解完。旧实现在这里晚了一步（等解码成功才恢复），真机上 IDR 解码本身可能 445 ms，
///   期间到达的 P 帧全被丢 → 再断链 → 再请你一张 IDR。
/// - **不因丢包销毁解码器**：H.264 / VP9 解码器都能直接吃下一张 IDR 重新同步，重建实例只会
///   让恢复更慢（还要重新 configure）。只有**解码器自己坏了**（`decode` 返回错误、
///   `configure` 失败、`spawn_blocking` panic）才重建。
/// - **序号**只在「本任务实际看到的包」之间比较：主循环有意丢掉的包根本到不了这里，
///   因此丢弃不会再表现成断层（见 [`seq_is_gap`]）。
/// - **不在这里发请求**：本任务只把「链断了 / IDR 解出来了」经 [`DecodeNotice`] 告诉主循环，
///   由主循环的 [`KeyframeGate`] 统一去重、限频、重试。旧实现把限频状态放在这里，而本任务
///   正卡在几百毫秒的 `spawn_blocking` 里，重试 tick 也被一起挡住。
///
/// 被控端会周期性发关键帧作兜底，但**不能只靠它**——请求关键帧是断链后唯一的快速恢复路径。
///
/// # 解码耗时（阶段 3）
///
/// 每次 `decode` 的耗时写进 [`DecodeShared::decode_ms`]，由主循环每秒读一次填进 `QosFeedback`。
/// 它是被控端「降编码分辨率」的**唯一**依据（4K 解码实测 37–58 ms 是瓶颈，编码仅 7.9/7.7 ms
/// 不是）。
///
/// **语义随数据面而变化**：VP9 的 `decode()` 含 I444→RGBA 转换；Windows H.264 硬件路径现在
/// 只解码 + 打包 NV12，NV12→RGBA 的 CPU 转换被移到 GUI 线程（阶段 1b 会换成 GPU 着色器），
/// 因此 B1 过渡期 H.264 的 `decode_ms` **不含**颜色转换，读数会比旧版本低。QoS 阈值暂不改，
/// 待着色器落地、转换成本重新可见后再校准。
/// 按 codec 取解码器工厂：VP9 用内置实现，H.264 用注入实现（未注入则为 `None`）。
fn decoder_factory_for(
    codec: CodecId,
    h264_decoder_factory: Option<&Arc<dyn VideoDecoderFactory>>,
) -> Option<Arc<dyn VideoDecoderFactory>> {
    match codec {
        CodecId::Vp9 => Some(vp9_decoder_factory()),
        CodecId::H264 => h264_decoder_factory.cloned(),
    }
}

/// 解码任务 → 主循环的通知（进程内，**不上线**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeNotice {
    /// 参考链断了：主循环据此丢 P 帧（[`ChainGate`]），并按 [`KeyframeGate`] 请求 IDR。
    Broken {
        /// 出事的那台显示器（被控端只认自己正在采集的那台）。
        display: u32,
        /// 原因（只进管理面，不进线上协议）。
        reason: KeyframeReason,
    },
    /// 关键帧已成功解出：本次等待事件结束。
    Satisfied,
}

/// 告诉主循环「这台显示器的参考链断了」。
///
/// 通道无界且主循环常驻接收端，故不会阻塞也不会失败（`send` 只在对端被 drop 后返回 `Err`，
/// 而那时会话已经在收尾）。去重/限频在 [`KeyframeGate`] 里做，这里可以放心地每帧都发。
fn note_broken(tx: &mpsc::UnboundedSender<DecodeNotice>, display: u32, reason: KeyframeReason) {
    let _ = tx.send(DecodeNotice::Broken { display, reason });
}

/// 解码侧的断链上报：**队列里已经有关键帧时不报**。
///
/// 关键帧一到就恢复，这时再让主循环关闸门/再请一张 IDR 都是多余的——而且闸门的打开条件是
/// 「关键帧**入队**」，那张关键帧已经入过队了，关上去就会一直关着，直到再骗一张 IDR。
fn report_break(
    queue: &DecodeQueue,
    tx: &mpsc::UnboundedSender<DecodeNotice>,
    display: u32,
    reason: KeyframeReason,
) {
    if !queue.has_keyframe() {
        note_broken(tx, display, reason);
    }
}

/// 主循环的「参考链已断」闸门：自上一个关键帧**入队**以来，是否丢过参考链上的帧。
///
/// **清除点必须是「IDR 入队」而不是「IDR 解出」**。旧实现把清除放在解码任务里、IDR 解码
/// 成功之后，而真机上一次 IDR 解码本身就可能 445 ms（DXVA staging 回读停顿，卷宗 §2.1）：
/// 这段时间里到达的 2–5 张 P 帧全被丢掉 → 每张都让解码任务的序号出现断层 → 解码任务重置
/// 解码器并再请求一次 IDR → 一次事件换回 3 张 IDR（91 秒 27 张、39% 带宽）。
///
/// IDR 一旦**进入队列**，它之后的 P 帧在队列里天然排在它后面、可以按序解码，闸门必须立刻
/// 打开；只有「请求已发出、IDR 还没到」期间到达的 P 帧才必须丢。
///
/// 状态**只由主循环写**（解码侧只经 [`DecodeNotice::Broken`] 告知），因此不存在「解码任务
/// 在别的时刻把它清掉」的竞态——旧实现里两处共写同一个原子，解码成功一帧就会把主循环刚
/// 建好的断链状态抹掉。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ChainGate {
    broken: bool,
}

impl ChainGate {
    /// 成功入队一张关键帧：IDR 之后的 P 帧可以正常解码，闸门打开。
    fn on_keyframe_enqueued(&mut self) {
        self.broken = false;
    }

    /// 有意丢了一帧（或解码侧报告断链）：在下一张关键帧入队前，P 帧一律不放行。
    fn on_drop(&mut self) {
        self.broken = true;
    }

    /// 本包是否应被闸门挡下。关键帧永远放行。
    #[must_use]
    fn should_drop(self, keyframe: bool) -> bool {
        self.broken && !keyframe
    }
}

/// 关键帧请求闸门：**单飞 + 去重 + 超时重试**（主循环独占，纯决策 → 可单测）。
///
/// 不变量：**只要还在等 IDR，就一定有请求在路上**。
///
/// 旧实现把限频状态放在解码任务里（`last_kf_request` + 单个 `kf_pending` + `kf_tick`），而
/// 解码任务正卡在几百毫秒的 `spawn_blocking` 里，tick 被一起挡住；主循环侧另有三个互不
/// 相干的限频器（无帧看门狗 3 s、老化 400 ms、解码任务 400 ms），请求被其中任何一个吞掉都
/// 没有人补发——真机日志里 63 帧连续 `reason=awaiting`（4.8 s 无画面）就是这么来的。
///
/// 新模型只有一处状态：
/// - 每个「断链」事件只更新 `desired`（**按显示器/原因去重**），并且只在距上次发送已满
///   [`KEYFRAME_REQUEST_INTERVAL`] 时才真的发；
/// - 等待期间主循环的 `keyframe_tick` 按同一间隔重试，直到 IDR 解码成功（[`Self::satisfied`]）；
/// - 一次等待事件最多换回一张 IDR：重试周期（400 ms）**不小于**被控端的关键帧去重窗口
///   （`KEYFRAME_DEDUP_WINDOW = 350 ms`，见 `crates/remote/src/controlled.rs`），在途 IDR
///   期间到达的重试会被对端吸收。
#[derive(Debug, Default)]
struct KeyframeGate {
    /// 是否在等 IDR（一次「等待事件」）。
    waiting: bool,
    /// 本次等待的最近一次请求目标；重试沿用同一目标，不重复上报。
    desired: Option<(u32, KeyframeReason)>,
    /// 上一次真正发出的时刻。
    last_sent: Option<Instant>,
}

impl KeyframeGate {
    /// 断链事件：记录目标，并在窗口允许时立刻发一次。
    ///
    /// 返回 `Some` 表示调用方**现在就要**把这条 `RequestKeyframe` 发出去。
    fn on_break(
        &mut self,
        display: u32,
        reason: KeyframeReason,
        now: Instant,
    ) -> Option<(u32, KeyframeReason)> {
        self.waiting = true;
        self.desired = Some((display, reason));
        if self.due(now) {
            self.last_sent = Some(now);
            return self.desired;
        }
        None
    }

    /// 定时重试：仍在等待且已过一个窗口 → 再发一次；否则 `None`。
    fn on_tick(&mut self, now: Instant) -> Option<(u32, KeyframeReason)> {
        if !self.waiting || !self.due(now) {
            return None;
        }
        self.last_sent = Some(now);
        self.desired
    }

    /// IDR 已解出：本次等待事件结束（下一次断链会立刻重发）。
    fn satisfied(&mut self) {
        self.waiting = false;
        self.desired = None;
        self.last_sent = None;
    }

    #[must_use]
    fn is_waiting(&self) -> bool {
        self.waiting
    }

    fn due(&self, now: Instant) -> bool {
        self.last_sent
            .is_none_or(|sent| now.saturating_duration_since(sent) >= KEYFRAME_REQUEST_INTERVAL)
    }
}

/// 本包的序号是否构成「意料之外的断层」（解码任务用）。
///
/// **关键帧永远不算**：它本身就是新的参考链起点，序号跳跃是正常的（主循环丢掉一批 P 帧之后
/// IDR 必然跳号）。旧实现不区分这一点，于是每次丢包之后到达的 IDR 都被判成断层 → 销毁解码器
/// + 再请求一张 IDR，这是「一次事件换回 3 张 IDR」的另一半来源。
///
/// 主循环有意丢弃的包**根本不会进入解码队列**，所以本函数只在「网络/协议真的漏了一帧」或
/// 「解码任务自己丢过包却忘了推进期望值」时返回 `true`。
#[must_use]
fn seq_is_gap(expected: Option<u64>, seq: u64, keyframe: bool) -> bool {
    !keyframe && expected.is_some_and(|expected| seq != expected)
}

/// 发一条 `RequestKeyframe` 并上报原因。
async fn dispatch_keyframe_request(
    queues: &mut OutboundQueues,
    cancel: &CancellationToken,
    stats: &SessionStatsReporter,
    (display, reason): (u32, KeyframeReason),
) -> RemoteResult<()> {
    queues
        .enqueue_ctrl(
            cancel,
            SessionFrame::screen(SessionMessage::RequestKeyframe { display }),
        )
        .await?;
    stats.report(SessionStats::KeyframeRequest { reason });
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "序号断层/关键帧等待/解码器重建/切 codec 是一条不可分割的状态机，拆开反而要来回传状态"
)]
async fn decode_loop(
    queue: Arc<DecodeQueue>,
    notice_tx: mpsc::UnboundedSender<DecodeNotice>,
    sink: std::sync::Arc<dyn FrameSink>,
    cancel: CancellationToken,
    decode_shared: Arc<DecodeShared>,
    h264_decoder_factory: Option<Arc<dyn VideoDecoderFactory>>,
    stats: SessionStatsReporter,
) {
    let mut codec: Option<CodecId> = None;
    let mut decoder: Option<Box<dyn VideoDecoderApi>> = None;
    // 已上报给 UI 的解码器信息（A3；**只在变化时**发，避免每秒刷一条一模一样的读数）。
    let mut last_decoder_info: Option<DecoderInfo> = None;
    // 当前解码器已经 `configure` 过的编码尺寸；`None` = 新实例，首个包必须先 configure。
    // 尺寸来自 `VideoPacket`（即编码尺寸），**不是** `ScreenConfig`——后者不经过解码队列。
    let mut configured_dims: Option<(u32, u32)> = None;
    // 参考链已断，只等 IDR。**关键帧一被取出就清**（不必等它解完）。
    let mut awaiting_keyframe = false;
    // 进入等待的原因：补丁在没有基底时被跳过要沿用同一原因重发（见下方 `DecodeNotice`）。
    let mut awaiting_reason = KeyframeReason::DecodeError;
    // 下一个期望的帧序号（只在**本任务实际看到**的包之间比较；见 [`seq_is_gap`]）。
    let mut expected_seq: Option<u64> = None;
    // 上一次**解码完成**的时刻（`now_us()`）。真机的 300–505 ms 单帧停顿 18/18 次都出现在
    // 「距上一帧超过 600 ms 的空闲」之后，故把空闲间隔一并打进逐帧日志，现场才能对上号。
    let mut last_decode_done_us: u64 = 0;
    // 本任务只发「断链 / IDR 解出」通知；请求的去重、限频与重试全部在主循环的
    // [`KeyframeGate`] 里（本任务会卡在 `spawn_blocking` 中，放这里的 tick 会被一起挡住）。
    loop {
        let job = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            job = queue.pop() => match job {
                Some(job) => job,
                None => break,
            },
        };
        // `display` 会与 tracing 宏展开里引入的 `field::display` 撞名，故绑定名用 monitor。
        let (monitor, seq, packet_codec, flags, width, height, data, capture_ts_us, recv_us) =
            match job {
                DecodeJob::SelectCodec(selected) => {
                    // 换 codec：旧解码器的参考帧不可用，重置并等新 codec 的关键帧。
                    if codec != Some(selected) {
                        debug!(?selected, "切换解码器 codec");
                        codec = Some(selected);
                        decoder = None;
                        // **只建解码器、不 configure**：尺寸由首个 `VideoPacket` 给出。
                        configured_dims = None;
                        awaiting_keyframe = true;
                        expected_seq = None;
                        if let Some(factory) =
                            decoder_factory_for(selected, h264_decoder_factory.as_ref())
                        {
                            match factory.create() {
                                Ok(dec) => {
                                    report_decoder_info(
                                        &stats,
                                        &mut last_decoder_info,
                                        dec.as_ref(),
                                    );
                                    decoder = Some(dec);
                                }
                                // 创建失败不必终止：首个关键帧到达时会再试一次（下方 `is_none` 分支）。
                                Err(err) => warn!(error = %err, "解码器创建失败"),
                            }
                        } else {
                            warn!(?selected, "没有对应 codec 的解码器工厂");
                        }
                    }
                    continue;
                }
                // 补丁批：校验/解压后交给 `FrameSink::on_patch`。
                //
                // **顺序保证**：补丁与全帧都从本函数出队，所以 `on_patch` 必然排在前一个
                // `on_frame`（或前一批补丁）之后——UI 侧的持久合成帧因此可以盲目按矩形覆写。
                // 补丁**不占用** `expected_seq`（被控端在补丁模式下不产全帧，视频序号不前进），
                // 退出补丁模式后的第一个全帧仍与解码端的期望值连续。
                DecodeJob::Region {
                    display: monitor,
                    width,
                    height,
                    rects,
                    capture_ts_us,
                    recv_us,
                } => {
                    // 诊断：包到达 → 本批被取走的队列等待（补丁没有解码耗时）。
                    let recv_q_us = now_us().saturating_sub(recv_us);
                    if latency::enabled() {
                        latency::record(&decode_shared.latency, Stage::RecvQueue, recv_q_us);
                    }
                    // 空批 = 保活（主循环已过滤；这里兜底，避免未来的调用方把空批当一帧上屏）。
                    if rects.is_empty() {
                        continue;
                    }
                    // 丢弃分两类，**只有一类可以静默**：正在等 IDR 时关键帧请求已经在途，再请
                    // 一次只会浪费限频窗口；其余任何丢弃都必须换回一个关键帧（补丁丢一个，
                    // 画面就永久缺一块）。
                    //
                    // 因此 codec / 几何检查排在 `awaiting_keyframe` **之前**：几何不符意味着
                    // 我们的合成帧基底与对端以为的不是同一块缓冲，这正是必须换回全帧的理由，
                    // 即使此刻已经在等 IDR。
                    if codec != Some(CodecId::H264) {
                        debug!(monitor, ?codec, "非 H.264 会话收到补丁，丢弃并请求关键帧");
                        awaiting_reason = KeyframeReason::DecodeError;
                        report_break(&queue, &notice_tx, monitor, awaiting_reason);
                        continue;
                    }
                    if configured_dims != Some((width, height)) {
                        debug!(
                            monitor,
                            width,
                            height,
                            ?configured_dims,
                            "补丁尺寸与当前合成帧不符，丢弃并请求关键帧"
                        );
                        awaiting_keyframe = true;
                        awaiting_reason = KeyframeReason::DecodeError;
                        report_break(&queue, &notice_tx, monitor, awaiting_reason);
                        continue;
                    }
                    if awaiting_keyframe {
                        // 还没有可应用的全帧基底。**不能静默**：这里必须保证真的有一条
                        // `RequestKeyframe` 在途（或会在重试窗口内发出），否则画面会永久冻结。
                        // 主循环的 [`KeyframeGate`] 本来就在等待期间重试，这条通知只是把「画面
                        // 还在缺基底」这个事实持续告诉它（同一显示器/原因会被去重），这样即使
                        // 某次请求被限频或丢失，也不会没有人补发。
                        report_break(&queue, &notice_tx, monitor, awaiting_reason);
                        continue;
                    }
                    let mut patches = Vec::with_capacity(rects.len());
                    let mut failed = false;
                    for rect in &rects {
                        let Some(patch) = decode_region_rect(width, height, rect) else {
                            failed = true;
                            break;
                        };
                        patches.push(patch);
                    }
                    if failed {
                        // 整批丢弃：解压/校验只做了一半也没有可应用的部分，半批补丁同样会让
                        // 画面缺块。
                        debug!(
                            monitor,
                            width, height, "补丁矩形非法或解压失败，丢弃整批并请求关键帧"
                        );
                        awaiting_keyframe = true;
                        awaiting_reason = KeyframeReason::DecodeError;
                        report_break(&queue, &notice_tx, monitor, awaiting_reason);
                        continue;
                    }
                    let patch_bytes = patches.iter().fold(0_u64, |acc, rect| {
                        acc.saturating_add(u64::try_from(rect.nv12.len()).unwrap_or(u64::MAX))
                    });
                    sink.on_patch(DecodedPatch {
                        display: monitor,
                        width,
                        height,
                        rects: patches,
                        capture_ts_us,
                    });
                    decode_shared
                        .applied_patches
                        .fetch_add(1, Ordering::Relaxed);
                    // 补丁的端到端：采集完成 → 本端应用完成（与全帧同一套公式）。
                    let offset = decode_shared.clock_offset_us.load(Ordering::Relaxed);
                    let e2e_ms = elapsed_ms(now_us(), offset, capture_ts_us);
                    decode_shared.e2e_ms.store(e2e_ms, Ordering::Relaxed);
                    if latency::enabled() {
                        let e2e_micros = u64::from(e2e_ms).saturating_mul(1000);
                        latency::record(&decode_shared.latency, Stage::CaptureToDecode, e2e_micros);
                        if latency::frame_logs() || e2e_micros > latency::slow_threshold_us() {
                            info!(
                                target: "latency",
                                "controller_patch bytes={} recv_q_us={} capture_to_recv_ms={} e2e_ms={}",
                                patch_bytes,
                                recv_q_us,
                                elapsed_ms(recv_us, offset, capture_ts_us),
                                e2e_ms
                            );
                        }
                    }
                    continue;
                }
                DecodeJob::Packet {
                    display,
                    seq,
                    codec: packet_codec,
                    flags,
                    width,
                    height,
                    data,
                    capture_ts_us,
                    recv_us,
                } => (
                    display,
                    seq,
                    packet_codec,
                    flags,
                    width,
                    height,
                    data,
                    capture_ts_us,
                    recv_us,
                ),
            };
        // 诊断：包到达 → 解码任务取走（解码队列等待）。`recv_us == 0` 是测试构造的哨兵，
        // 不能拿「进程启动至今」去减，否则汇总会被一个巨大值污染。
        let recv_q_us = if latency::enabled() && recv_us != 0 {
            let us = now_us().saturating_sub(recv_us);
            latency::record(&decode_shared.latency, Stage::RecvQueue, us);
            us
        } else {
            0
        };
        // 诊断：载荷字节数（`data` 稍后会被 move 进 `spawn_blocking`）。
        let packet_bytes = u64::try_from(data.len()).unwrap_or(u64::MAX);
        // 兜底：正常情况下 `SelectCodec` 一定先到；万一对端漏发，用视频包自带的 codec。
        // 换解码器 ⇒ 旧参考帧作废，必须等关键帧。
        if codec.is_none() {
            codec = Some(packet_codec);
            decoder = None;
            configured_dims = None;
            awaiting_keyframe = true;
        }

        // 声明尺寸的纵深防御：真正的解码尺寸由码流决定（下方解码后还会再校验一次），但先挡掉
        // 明显非法的声明，避免把畸形值传下去。
        if width == 0 || height == 0 || width > MAX_FRAME_DIM || height > MAX_FRAME_DIM {
            warn!(monitor, width, height, "视频包声明的编码尺寸非法，丢弃");
            continue;
        }

        let keyframe = flags & VIDEO_FLAG_KEYFRAME != 0;
        // 序号断层 = 真的漏了包（主循环有意丢的包根本到不了这里）→ 参考链已断。
        //
        // **不销毁解码器**：H.264 / VP9 都能直接吃下一张 IDR 重新同步，重建实例只会让恢复
        // 更慢。旧实现在这里 `decoder = None`，于是一次丢帧就换一次重建 + 一次多余的 IDR。
        if seq_is_gap(expected_seq, seq, keyframe) {
            debug!(seq, ?expected_seq, "视频序号断层，等待关键帧后恢复");
            awaiting_keyframe = true;
            awaiting_reason = KeyframeReason::SeqGap;
            // 队列里已经有一张关键帧排着就不再上报：请求只会多花 200–400 KB 带宽
            // （见 [`report_break`]）。
            report_break(&queue, &notice_tx, monitor, awaiting_reason);
        }
        // 期望值对**每一个看到的包**推进（含下面被丢弃的那些）：本任务主动丢弃的包不应该在
        // 下一个包上再表现成一次「断层」。
        expected_seq = Some(seq.wrapping_add(1));

        // 关键帧**一被取出**就恢复：它是新的参考链起点，排在它之后的 P 帧在队列里天然排在
        // 它后面、可以按序解码。旧实现等解码成功才恢复，真机上 IDR 解码本身可能 445 ms，
        // 期间到达的 P 帧全被丢 → 再断链 → 再换一张 IDR。
        if keyframe {
            awaiting_keyframe = false;
        }
        if awaiting_keyframe && !keyframe {
            continue;
        }
        // 尚无解码器时必须从关键帧起步，否则 P 帧无从参考。
        if decoder.is_none() && !keyframe {
            awaiting_keyframe = true;
            awaiting_reason = KeyframeReason::NoDecoder;
            report_break(&queue, &notice_tx, monitor, awaiting_reason);
            continue;
        }
        if decoder.is_none() {
            let factory =
                codec.and_then(|codec| decoder_factory_for(codec, h264_decoder_factory.as_ref()));
            let Some(factory) = factory else {
                warn!(?codec, "没有对应 codec 的解码器工厂，丢弃本帧并等待关键帧");
                awaiting_keyframe = true;
                awaiting_reason = KeyframeReason::NoDecoder;
                report_break(&queue, &notice_tx, monitor, awaiting_reason);
                continue;
            };
            match factory.create() {
                Ok(dec) => {
                    report_decoder_info(&stats, &mut last_decoder_info, dec.as_ref());
                    decoder = Some(dec);
                    // 新实例尚未 configure：下面立即按本包的编码尺寸配置。
                    configured_dims = None;
                }
                Err(err) => {
                    warn!(error = %err, "解码器创建失败");
                    continue;
                }
            }
        }
        // 生命周期钩子：新解码器首次收到包、或编码尺寸变化时先 configure 再 decode。
        // 尺寸来源是**本包**的 `width`/`height`（编码尺寸）；`ScreenConfig` 不经过解码队列。
        if configured_dims != Some((width, height)) {
            let result = match decoder.as_mut() {
                Some(dec) => dec.configure(width, height),
                None => Err(RemoteError::Platform("解码器不存在".to_owned())),
            };
            match result {
                Ok(()) => {
                    configured_dims = Some((width, height));
                    // 硬件解码器可能到 configure 之后才知道真正的后端（DXVA 还是软件回退）。
                    if let Some(dec) = decoder.as_ref() {
                        report_decoder_info(&stats, &mut last_decoder_info, dec.as_ref());
                    }
                }
                Err(err) => {
                    // 与解码失败同款处理：配置失败的实例不可信，丢弃并等下一个关键帧。
                    warn!(error = %err, width, height, "解码器配置尺寸失败，重置解码器并请求关键帧");
                    decoder = None;
                    configured_dims = None;
                    awaiting_keyframe = true;
                    awaiting_reason = KeyframeReason::ConfigureFailed;
                    report_break(&queue, &notice_tx, monitor, awaiting_reason);
                    continue;
                }
            }
        }
        // 解码是同步的重活（1080p 单帧十几毫秒，4K 更久）：直接跑在 async worker 上会与
        // 网络读循环/心跳抢线程。只把**这一次** `decode` 调用移进 `spawn_blocking`；
        // 上面的状态机（序号断层/等 IDR/重建解码器）仍然原样跑在 runtime 上。
        //
        // 安全性：`VideoDecoderApi: Send`，且解码器是**独占**的（`Box<dyn ...>` 只有这里
        // 持有），所以 `take()` 进闭包、拿回来再放回 `decoder` 是安全的。MFT 的 COM 初始化
        // 是线程局部的（每次 `decode` 自己 `CoInitializeEx`），VP9 没有线程亲和——两者都能
        // 在任何阻塞线程上跑。
        //
        // `decoder == None` 的分支保持原样（本包之前已决定丢弃/等关键帧）。
        let Some(mut dec) = decoder.take() else {
            continue;
        };
        // 本次解码开始前的空闲间隔：上一帧解码完成 → 现在。`0` = 本次会话首个可解码帧。
        let idle = idle_ms(now_us(), last_decode_done_us);
        let blocking_result = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let result = dec.decode(&data);
            (dec, result, started.elapsed())
        })
        .await;
        let decode_done_us = now_us();
        match blocking_result {
            Ok((dec, Ok(Some(frame)), elapsed)) => {
                // 子阶段读数要在把解码器放回 `decoder` 之前读（`decoder` 另行持有它）。
                // 纯观测：只进日志，不参与任何决策。
                let phase = dec.last_decode_timings();
                decoder = Some(dec);
                let decode_ms = u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX);
                decode_shared.decode_ms.store(decode_ms, Ordering::Relaxed);
                awaiting_keyframe = false;
                // IDR 已**解出**：本次等待事件结束，主循环清空重试状态（下一次断链会立刻重发）。
                // 注意主循环的入队闸门比这里早一步打开（IDR 入队即打开），两者语义不同：
                // 这里管「还要不要发请求」，那边管「还要不要丢 P 帧」。
                if keyframe {
                    let _ = notice_tx.send(DecodeNotice::Satisfied);
                }
                // 端到端（D2）：采集完成 → 本端解码完成。偏差未知/时间戳缺失时给 0
                // （见 [`crate::clock::elapsed_ms`]）。
                let offset = decode_shared.clock_offset_us.load(Ordering::Relaxed);
                let e2e_ms = elapsed_ms(now_us(), offset, capture_ts_us);
                decode_shared.e2e_ms.store(e2e_ms, Ordering::Relaxed);
                if latency::enabled() {
                    let decode_micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
                    let e2e_micros = u64::from(e2e_ms).saturating_mul(1000);
                    latency::record(&decode_shared.latency, Stage::Decode, decode_micros);
                    latency::record(&decode_shared.latency, Stage::CaptureToDecode, e2e_micros);
                    if latency::frame_logs() || e2e_micros > latency::slow_threshold_us() {
                        info!(
                            target: "latency",
                            "controller seq={} idr={} bytes={} recv_q_us={} idle_ms={} dec_us={} capture_to_recv_ms={} e2e_ms={} backlog={}",
                            seq,
                            flags & VIDEO_FLAG_KEYFRAME != 0,
                            packet_bytes,
                            recv_q_us,
                            idle,
                            decode_micros,
                            elapsed_ms(recv_us, offset, capture_ts_us),
                            e2e_ms,
                            queue.backlog()
                        );
                    }
                    // 慢帧才打四段细分：空闲后的第一帧到底卡在 MFT 出帧、GPU 回读，
                    // 还是 CPU 转换，全在这一行里。默认实现（VP9）四项全 0。
                    if decode_micros > SLOW_DECODE_US {
                        info!(
                            target: "latency",
                            "controller_decode_slow seq={} idle_ms={} dec_us={} submit_us={} output_us={} readback_us={} color_us={}",
                            seq,
                            idle,
                            decode_micros,
                            phase.submit_us,
                            phase.output_us,
                            phase.readback_us,
                            phase.color_us
                        );
                    }
                }
                sink.on_frame(DecodedFrame {
                    display: monitor,
                    width: frame.width,
                    height: frame.height,
                    seq,
                    rgba: frame.rgba,
                    nv12: frame.nv12,
                    capture_ts_us,
                });
            }
            // 本批无输出（理论路径；`lag_in_frames = 1` 下每次都有）。
            Ok((dec, Ok(None), _elapsed)) => {
                decoder = Some(dec);
                awaiting_keyframe = false;
            }
            Ok((dec, Err(err), _elapsed)) => {
                // **解码器自己坏了**（不是「参考链断了」）：这个实例不可信，重建并等下一个
                // 关键帧。这是唯一需要重建解码器的路径——丢包/断层只需喂一张 IDR。
                drop(dec);
                warn!(error = %err, "画面解码失败，重置解码器并请求关键帧");
                decoder = None;
                configured_dims = None;
                awaiting_keyframe = true;
                awaiting_reason = KeyframeReason::DecodeError;
                report_break(&queue, &notice_tx, monitor, awaiting_reason);
            }
            Err(join_err) => {
                // `spawn_blocking` 里 panic：解码器已随闭包一起 drop，只能重建实例并等
                // 下一个关键帧。与解码失败同款恢复路径，但错误类型明确写进日志。
                let err = if join_err.is_panic() {
                    RemoteError::Codec("解码阻塞任务 panic".to_owned())
                } else {
                    RemoteError::Platform("解码阻塞任务被取消".to_owned())
                };
                warn!(error = %err, "画面解码失败，重置解码器并请求关键帧");
                decoder = None;
                configured_dims = None;
                awaiting_keyframe = true;
                awaiting_reason = KeyframeReason::DecodeError;
                report_break(&queue, &notice_tx, monitor, awaiting_reason);
            }
        }
        // 下一次「空闲间隔」的起点。失败/panic 也算一次解码尝试完成——它们同样结束了
        // 「解码器闲着」的状态，不计的话下一次正常解码会被报成超长空闲。
        last_decode_done_us = decode_done_us;
    }
}

/// 把解码器的后端信息上报给 UI，**只在变化时**发。
///
/// 解码任务持有解码器，UI 在另一个任务里——信息走既有的进程内 [`SessionStatsReporter`]，
/// 不新增线上变体（`SessionMessage` 里没有它，也不该有）。
fn report_decoder_info(
    stats: &SessionStatsReporter,
    last: &mut Option<DecoderInfo>,
    decoder: &dyn VideoDecoderApi,
) {
    let info = decoder.info();
    if last.as_ref() == Some(&info) {
        return;
    }
    stats.report(SessionStats::DecoderInfo {
        backend: info.backend.clone(),
        adapter: info.adapter.clone(),
        hardware: info.hardware,
    });
    *last = Some(info);
}

/// 两份**累计**可靠层快照求差，得到每秒增量（[`TransportStats`]）。
///
/// `saturating_sub`：计数器理论上单调，但会话重建/快照竞争下出现倒退时宁可给 0，也不 panic
/// 或回绕成天文数字。
fn transport_delta(prev: ReliableSnapshot, now: ReliableSnapshot) -> TransportStats {
    let delta = |now: u64, prev: u64| u32::try_from(now.saturating_sub(prev)).unwrap_or(u32::MAX);
    TransportStats {
        rto: delta(now.rto_timeouts, prev.rto_timeouts),
        fast_retransmit: delta(now.fast_retransmits, prev.fast_retransmits),
        out_of_order: delta(now.out_of_order_segments, prev.out_of_order_segments),
        duplicate: delta(now.duplicate_segments, prev.duplicate_segments),
        out_of_window: delta(now.out_of_window_drops, prev.out_of_window_drops),
    }
}

/// 校验并解压一个补丁矩形；任何非法输入返回 `None`（调用方据此丢弃整批）。
///
/// 校验项与 [`ScreenPatch`] / [`crate::screen::DecodedNv12::apply_patch`] 的契约一字不差：
/// 坐标与尺寸全部为偶数、非零、落在声明帧内，解压后长度精确等于 `w*h*3/2`。控制器先把关，
/// UI 侧才不会拿到半个能应用的矩形。
///
/// zstd 解压带 `expected` 上限：超限即报错，不会按对端声明的尺寸无界分配。
fn decode_region_rect(
    frame_width: u32,
    frame_height: u32,
    rect: &RegionRect,
) -> Option<ScreenPatch> {
    let (x, y, width, height) = (rect.x, rect.y, rect.width, rect.height);
    if width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || !x.is_multiple_of(2)
        || !y.is_multiple_of(2)
    {
        return None;
    }
    if x.checked_add(width)? > frame_width || y.checked_add(height)? > frame_height {
        return None;
    }
    let expected = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(3)?
        .checked_div(2)?;
    let nv12 = if rect.flags & REGION_FLAG_ZSTD != 0 {
        zstd::bulk::decompress(&rect.data, expected).ok()?
    } else {
        if rect.data.len() != expected {
            return None;
        }
        rect.data.clone()
    };
    if nv12.len() != expected {
        return None;
    }
    Some(ScreenPatch {
        x,
        y,
        width,
        height,
        nv12,
    })
}

/// 两次解码完成之间的空闲间隔（毫秒），**纯函数**便于单测。
///
/// `last_done_us == 0` 是「本会话还没解码过」的哨兵，返回 0（不能拿进程启动至今去减，
/// 否则首个可解码帧会被报成一个巨大的空闲）。时钟倒退同样饱和成 0：这里只描述
/// 「闲置了多久」，负值是毫无意义的噪声，不该在日志里变成天文数字。
fn idle_ms(now_us: u64, last_done_us: u64) -> u64 {
    if last_done_us == 0 {
        return 0;
    }
    now_us.saturating_sub(last_done_us) / 1000
}

/// 把 [`TransferHub::abort_all`] 返回的 `FileError` 依次投进出站队列。
///
/// **必须回发**：发送方读文件失败后不会再有任何 chunk 流动，对端既等不到 `FileDone` 也等不到
/// 错误，会一直挂在半途（UI 停在 99%）。`FileError` 属 file 类：队列满时进暂存区、下一拍重试，
/// 绝不阻塞主循环。
async fn send_file_errors(
    queues: &mut OutboundQueues,
    cancel: &CancellationToken,
    msgs: Vec<SessionMessage>,
) -> RemoteResult<()> {
    for msg in msgs {
        queues
            .dispatch_frame(cancel, SessionFrame::file(msg))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ack_mac;
    use crate::crypto::{derive_session_keys, Cipher, EphemeralKeyPair};
    use crate::error::RemoteResult;
    use crate::input_buffer::InputBuffer;
    use crate::screen::{DecodedFrame, DisplayInfo, FrameSink, InputEvent, KeyEvent};
    use crate::session::{
        plan_controller_handshake, ControllerHandshake, SessionCodec, SessionFrame, SessionMessage,
        LIVENESS_TIMEOUT,
    };
    use crate::transfer::{TransferEvent, TransferHub};
    use crate::transport::BoxDuplex;
    // `.send(..)` 只有测试里的 peer writer 用得到（lib 层所有出站都走队列，不再直接写 writer），
    // 故 `SinkExt` 只在测试模块引入。
    use futures_util::SinkExt as _;
    use rust_tunnel_common::protocol::RemoteMode;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{ReadHalf, WriteHalf};
    use tokio::sync::{mpsc, Notify};
    use tokio_util::codec::{FramedRead, FramedWrite};
    use tokio_util::sync::CancellationToken;

    /// 单条等待上限，避免测试挂死。
    const WAIT: Duration = Duration::from_secs(5);

    /// 取消 / 命令通道关闭**同时**就绪这一竞态的重复轮数（见用例 5）。
    const RACE_ROUNDS: usize = 16;

    type PeerRead = FramedRead<ReadHalf<BoxDuplex>, SessionCodec>;
    type PeerWrite = FramedWrite<WriteHalf<BoxDuplex>, SessionCodec>;

    /// 记录 `on_closed` 的假上报目标。
    #[derive(Default)]
    struct FakeSink {
        closed: Mutex<Option<String>>,
    }

    impl FrameSink for FakeSink {
        fn on_displays(&self, _displays: Vec<DisplayInfo>) {}
        fn on_frame(&self, _frame: DecodedFrame) {}
        fn on_closed(&self, reason: String) {
            if let Ok(mut guard) = self.closed.lock() {
                *guard = Some(reason);
            }
        }
    }

    /// 空闲的传输状态机（这些用例不测文件传输）。
    fn transfer_hub() -> (TransferHub, mpsc::UnboundedReceiver<TransferEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (TransferHub::new(None, std::env::temp_dir(), tx), rx)
    }

    /// 作为「被控端」完成握手：回一个合法的 `HelloAck`，返回装好加密的读写半边。
    ///
    /// 会话循环不发 `Hello`（由建立阶段流水线写出），故这里直接用 `plan` 里的控制器临时公钥
    /// 算 MAC，无需真的先读一帧。
    async fn controlled_peer_handshake(
        stream: BoxDuplex,
        plan: &ControllerHandshake,
        token: &str,
        session_id: &str,
    ) -> (PeerRead, PeerWrite) {
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = FramedRead::new(read_half, SessionCodec::new());
        let mut writer = FramedWrite::new(write_half, SessionCodec::new());

        let responder = EphemeralKeyPair::generate();
        let responder_pub = responder.public_bytes();
        let initiator_pub = plan.keypair.public_bytes();
        let mac = ack_mac(token, session_id, &responder_pub, &initiator_pub);
        writer
            .send(SessionFrame::control(SessionMessage::HelloAck {
                accepted: true,
                reason: None,
                displays: Vec::new(),
                x25519_pub: responder_pub.to_vec(),
                hmac_ack: mac,
            }))
            .await
            .expect("回 HelloAck 应成功");

        let shared = responder
            .diffie_hellman(&initiator_pub)
            .expect("ECDH 应成功");
        // 本端扮演被控端（响应方）：`initiator=false`。
        let keys = derive_session_keys(
            token,
            &shared,
            session_id,
            &initiator_pub,
            &responder_pub,
            false,
        );
        reader
            .decoder_mut()
            .set_cipher(Cipher::new(keys.recv).expect("密钥长度合法"));
        writer
            .encoder_mut()
            .set_cipher(Cipher::new(keys.send).expect("密钥长度合法"));
        (reader, writer)
    }

    /// 后台把对端读端排空（用于让收尾的 `Bye` 能落地）。
    fn spawn_drain(mut reader: PeerRead) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(item) = reader.next().await {
                if item.is_err() {
                    break;
                }
            }
        })
    }

    fn config(session_id: &str, token: &str) -> ControllerConfig {
        ControllerConfig {
            session_id: session_id.to_owned(),
            token: token.to_owned(),
            local_rd_id: "111111111".to_owned(),
            mode: RemoteMode::Control,
            stats: SessionStatsReporter::noop(),
            h264_decoder_factory: None,
            reliable: None,
        }
    }

    fn controller_task(
        stream: BoxDuplex,
        plan: ControllerHandshake,
        cmd_rx: mpsc::Receiver<ControllerCommand>,
        sink: Arc<dyn FrameSink>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<RemoteResult<SessionEnd>> {
        tokio::spawn(run_controller(
            stream,
            config("sess-1", "TOK"),
            plan,
            cmd_rx,
            transfer_hub().0,
            sink,
            cancel,
        ))
    }

    /// 1. 取消必须能打断「已阻塞在发送里」的会话循环（核心回归）。
    ///
    /// 对端**完全不读**，先灌一条 1 MiB 的命令把写缓冲填满，使循环卡在 `send_cancellable`；
    /// 此时 fire 取消，循环必须脱身并让整个 `run_controller` 返回。修复前该发送发生在 `select!`
    /// 的 handler 内，取消分支得不到轮询，循环会一直阻塞到测试超时。
    ///
    /// 注意**不能**在取消后再去读对端：读端一消费，被卡住的发送就能推进，连修复前的代码也会
    /// 「自行恢复」，测试就测不到取消了。故这里对端自始至终不读，收尾的 `Bye` 只能等满
    /// `BYE_TIMEOUT`（2s），返回时间以 3s 为上界。
    #[tokio::test]
    async fn cancel_interrupts_blocked_send() {
        // duplex 容量远小于要发的帧：写缓冲很快填满，`send` 必然挂起。
        let (a, b) = tokio::io::duplex(16 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        // 对端**完全不读**：读半边攥着不放（drop 掉会让控制器写拿到 broken pipe，就测不到阻塞了）。
        let (_peer_reader, _peer_writer) =
            controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

        let sink = Arc::new(FakeSink::default());
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let task = controller_task(Box::new(a), plan, cmd_rx, sink.clone(), cancel.clone());

        // 等握手进入主循环，再灌一条大命令把写缓冲填满。
        tokio::time::sleep(Duration::from_millis(100)).await;
        cmd_tx
            .send(ControllerCommand::Clipboard("x".repeat(1_000_000)))
            .await
            .expect("投递命令");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 此刻循环卡在 `send_cancellable` 里。fire 取消 → 必须脱身。
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("取消后应脱身并返回（修复前会永久阻塞在 handler 内的发送里）")
            .expect("任务不应 panic");
        assert!(result.is_ok(), "取消应正常收尾，实得 {result:?}");
    }

    /// 2. 取消后不能卡在收尾：即便对端**始终不读**，`Bye` 也会因限时而放弃，并在其后投递
    ///    `on_closed`（覆盖 `BYE_TIMEOUT`）。
    #[tokio::test]
    async fn cancel_does_not_block_teardown() {
        let (a, b) = tokio::io::duplex(16 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (_peer_reader, _peer_writer) =
            controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

        let sink = Arc::new(FakeSink::default());
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let task = controller_task(Box::new(a), plan, cmd_rx, sink.clone(), cancel.clone());

        tokio::time::sleep(Duration::from_millis(100)).await;
        cmd_tx
            .send(ControllerCommand::Clipboard("x".repeat(1_000_000)))
            .await
            .expect("投递命令");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 对端**始终不读**：收尾 `Bye` 发不出去，只能等 `BYE_TIMEOUT`；但 teardown 必须继续，
        // `on_closed` 必须照投——限时失效会在这里超时。
        cancel.cancel();
        let result = tokio::time::timeout(WAIT, task)
            .await
            .expect("取消后整个 run_controller 应在有限时间内返回（Bye 限时失效会出现此超时）")
            .expect("任务不应 panic");
        assert!(result.is_ok(), "取消应正常收尾，实得 {result:?}");
        assert_eq!(
            sink.closed.lock().unwrap().as_deref(),
            Some("用户中断"),
            "收尾原因应为用户中断，说明 on_closed 在 Bye 限时后仍被投递"
        );
    }

    /// 3. 命令通道关闭时循环必须退出，而不是空转（`recv()` 对已关闭通道每次都 `Ready(None)`）。
    #[tokio::test]
    async fn closed_command_channel_exits_loop() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (peer_reader, _peer_writer) =
            controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;
        let _drain = spawn_drain(peer_reader);

        let sink = Arc::new(FakeSink::default());
        let (cmd_tx, cmd_rx) = mpsc::channel::<ControllerCommand>(4);
        let cancel = CancellationToken::new();
        let task = controller_task(Box::new(a), plan, cmd_rx, sink.clone(), cancel);

        // 等进入主循环后 drop 命令发送端。
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(cmd_tx);

        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("命令通道关闭后循环应立即退出，而不是空转")
            .expect("任务不应 panic");
        assert!(
            result.is_ok(),
            "命令通道关闭应按正常结束处理，实得 {result:?}"
        );
        assert_eq!(
            sink.closed.lock().unwrap().as_deref(),
            Some("会话命令通道已关闭"),
            "收尾原因应指明命令通道关闭"
        );
    }

    /// 4. 会话一建立就发一次心跳，往返被算成延迟样本上报。
    ///
    /// UI 的「开窗即有读数」全靠这一点：心跳 interval 的起点写成 `now + HEARTBEAT_INTERVAL` 时，
    /// 首个样本要等满一个周期才出现，用户开窗看到的是空白。所以这里用 **300ms** 的上限锁住
    /// 「立即」——远小于 [`HEARTBEAT_INTERVAL`]（1s），写成延迟起点必然超时。
    #[tokio::test]
    async fn session_start_reports_rtt_immediately() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (mut peer_reader, mut peer_writer) =
            controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

        // 被控端：把收到的 `Heartbeat` 回一个 `HeartbeatAck`（其余帧不管）。
        let peer = tokio::spawn(async move {
            while let Some(Ok(frame)) = peer_reader.next().await {
                if matches!(frame.message, SessionMessage::Heartbeat { .. })
                    && peer_writer
                        .send(SessionFrame::control(SessionMessage::HeartbeatAck {
                            t2_us: 1,
                            t3_us: 2,
                        }))
                        .await
                        .is_err()
                {
                    break;
                }
            }
        });

        let sink = Arc::new(FakeSink::default());
        let (_cmd_tx, cmd_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let (stats_tx, mut stats_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_controller(
            Box::new(a),
            ControllerConfig {
                stats: SessionStatsReporter::new(stats_tx),
                ..config("sess-1", "TOK")
            },
            plan,
            cmd_rx,
            transfer_hub().0,
            sink,
            cancel.clone(),
        ));

        // 首个样本应是 RTT（心跳的起点取 `now`，首个 tick 立即到期）；解码侧读数要等 1s 的
        // QoS 窗口，故这里跳过非 RTT 事件，直到拿到 RTT 或超时。
        let rtt = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                match stats_rx.recv().await {
                    Some(SessionStats::Rtt(rtt)) => break rtt,
                    // 解码侧读数（每秒一条）先于 RTT 到达时跳过它。
                    Some(_) => {}
                    None => panic!("统计通道不该被关闭"),
                }
            }
        })
        .await
        .expect("会话建立后 300ms 内就该有延迟样本（首个 tick 立即到期）");
        assert!(
            rtt < Duration::from_millis(300),
            "内存管道 RTT 不该是 {rtt:?}"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, task).await;
        peer.abort();
    }

    /// 6. 控制器每秒发一条 `QosFeedback`（阶段 3 闭环的输入端；不新增协议变体）。
    ///
    /// 首个 QoS tick 在 `+1s`，故这里用 1.5s 的上限锁住「每秒一条」；写成「不发」必然超时。
    #[tokio::test]
    async fn controller_sends_qos_feedback_periodically() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (mut peer_reader, _peer_writer) =
            controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

        let sink = Arc::new(FakeSink::default());
        let (_cmd_tx, cmd_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let task = controller_task(Box::new(a), plan, cmd_rx, sink, cancel.clone());

        let saw = tokio::time::timeout(Duration::from_millis(1500), async {
            while let Some(Ok(frame)) = peer_reader.next().await {
                if matches!(frame.message, SessionMessage::QosFeedback { .. }) {
                    return true;
                }
            }
            false
        })
        .await
        .expect("1.5s 内应收到一条 QosFeedback（每秒一条）");
        assert!(saw, "被控端应收到 QosFeedback");

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, task).await;
    }

    /// 7. 对端不再回 `HeartbeatAck` 时，必须在 `LIVENESS_TIMEOUT` 附近以**非主动**方式结束。
    ///
    /// 中继停转发但 TCP 不断（半开/零窗口）时，对端不会给我们任何错误——没有这个判据，会话会
    /// 永久冻结而 UI 仍显示在线。用 `start_paused` 的虚拟时钟跳过真实的 4s 等待。
    #[tokio::test(start_paused = true)]
    async fn heartbeat_timeout_ends_session_as_link_failure() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (mut peer_reader, peer_writer) =
            controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

        // 对端只排空、**从不回 HeartbeatAck**。writer 必须攥在手里：drop 掉会让控制器读到
        // EOF，会话将以「对端已关闭连接」提前结束，测到的就不是超时判据了。
        let peer = tokio::spawn(async move {
            let _keep_writer = peer_writer;
            while let Some(Ok(_frame)) = peer_reader.next().await {}
        });

        let sink = Arc::new(FakeSink::default());
        let (_cmd_tx, cmd_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let task = controller_task(Box::new(a), plan, cmd_rx, sink.clone(), cancel);

        // 首个心跳立即发出，此后每 `HEARTBEAT_INTERVAL` 一拍；队首变老到 `LIVENESS_TIMEOUT`
        // 的那一拍判定超时。外层上限留足收尾余量，绝不能退化成「等 WAIT 超时」。
        let end = tokio::time::timeout(LIVENESS_TIMEOUT + Duration::from_secs(3), task)
            .await
            .expect("心跳超时后应结束会话，而不是永久冻结")
            .expect("任务不应 panic")
            .expect("心跳超时属于正常收尾（不是 IO 错误）");
        assert_eq!(end.reason, "对端心跳超时（链路可能已中断）");
        assert!(
            !end.deliberate,
            "心跳超时是链路故障，deliberate 必须为 false，否则不会触发自动重连"
        );
        assert_eq!(
            sink.closed.lock().unwrap().as_deref(),
            Some("对端心跳超时（链路可能已中断）"),
            "on_closed 也应收到同一原因"
        );
        peer.abort();
    }

    /// 8. 外部输入缓冲的整批事件按入队顺序写出，且连续鼠标移动只发最后一条。
    ///
    /// 这是「输入不丢 + 合并」的接线锁：只测 `InputBuffer` 的纯逻辑不够，还要证明会话循环
    /// 真的用 `drain()` 的返回值、按顺序写进 `SessionMessage::Input`。
    #[tokio::test]
    async fn buffered_input_is_written_in_order_and_merged() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (mut peer_reader, mut peer_writer) =
            controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

        // 被控端：应答心跳，并把收到的输入事件按线上顺序转发出来。
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<InputEvent>();
        let peer = tokio::spawn(async move {
            while let Some(Ok(frame)) = peer_reader.next().await {
                match frame.message {
                    SessionMessage::Heartbeat { .. } => {
                        if peer_writer
                            .send(SessionFrame::control(SessionMessage::HeartbeatAck {
                                t2_us: 1,
                                t3_us: 2,
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    SessionMessage::Input(event) => {
                        if seen_tx.send(event).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        let input = Arc::new(InputBuffer::new());
        // 连续两条移动合并成最后一条；抬起事件必须原样排队。
        input.push(InputEvent::MouseMove {
            x: 1,
            y: 0,
            display: 0,
        });
        input.push(InputEvent::MouseMove {
            x: 2,
            y: 0,
            display: 0,
        });
        let key_up = InputEvent::Key(KeyEvent {
            code: 13,
            pressed: false,
            text: None,
            modifiers: 0,
        });
        input.push(key_up.clone());

        let sink = Arc::new(FakeSink::default());
        let (_cmd_tx, cmd_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_controller_with_input(
            Box::new(a),
            config("sess-1", "TOK"),
            plan,
            cmd_rx,
            Arc::clone(&input),
            transfer_hub().0,
            sink,
            cancel.clone(),
        ));

        let first = tokio::time::timeout(WAIT, seen_rx.recv())
            .await
            .expect("首个输入事件应及时写出")
            .expect("输入转发通道不应关闭");
        let second = tokio::time::timeout(WAIT, seen_rx.recv())
            .await
            .expect("抬起事件应及时写出")
            .expect("输入转发通道不应关闭");
        assert_eq!(
            first,
            InputEvent::MouseMove {
                x: 2,
                y: 0,
                display: 0
            },
            "连续移动应只发最后一条"
        );
        assert_eq!(second, key_up, "抬起事件必须原样送达且排在移动之后");

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, task).await;
        peer.abort();
    }

    /// 5. 取消与命令发送端**同时**失效时，必须判定为「用户中断」，而不是「命令通道已关闭」。
    ///
    /// 这正是 `abort_active` 的真实时序：`conclude_session` 背靠背地 fire 取消、drop `cmd_tx`，
    /// 本循环下一次轮询时两个分支同时就绪。没有 `biased` 时随机轮询会有一半概率走
    /// `recv() == None` 分支，把一次「用户主动挂断」记成 `deliberate: false`——本端随即会去
    /// **自动重连用户刚刚亲手结束的会话**，发给对端的 `Bye.reason` 也跟着变成错的。
    ///
    /// 两个信号在测试任务里同步 fire（中间不让出执行权），故任务被轮询时二者必然同时就绪；
    /// 单轮的「漏判」概率约 1/2，重复 [`RACE_ROUNDS`] 次把漏网概率压到可忽略——真按随机轮询
    /// 跑的实现在这里必然失败。等待首个心跳（起点取 `now`，首个 tick 立即到期）作为「已进入
    /// 主循环」的确定信号，比 sleep 一个猜出来的时长可靠。
    #[tokio::test]
    async fn cancel_wins_over_closed_command_channel() {
        for round in 0..RACE_ROUNDS {
            let (a, b) = tokio::io::duplex(64 * 1024);
            let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
            let (mut peer_reader, _peer_writer) =
                controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

            // 被控端只观察首个心跳并继续排空（让收尾的 `Bye` 能落地）。
            let ready = Arc::new(Notify::new());
            let peer_ready = ready.clone();
            tokio::spawn(async move {
                while let Some(Ok(frame)) = peer_reader.next().await {
                    if matches!(frame.message, SessionMessage::Heartbeat { .. }) {
                        peer_ready.notify_one();
                    }
                }
            });

            let sink = Arc::new(FakeSink::default());
            let (cmd_tx, cmd_rx) = mpsc::channel::<ControllerCommand>(4);
            let cancel = CancellationToken::new();
            let task = controller_task(Box::new(a), plan, cmd_rx, sink.clone(), cancel.clone());

            tokio::time::timeout(WAIT, ready.notified())
                .await
                .expect("会话应进入主循环并发出首个心跳");

            // 同步 fire：轮询时取消与命令通道关闭同时就绪。
            cancel.cancel();
            drop(cmd_tx);

            let end = tokio::time::timeout(WAIT, task)
                .await
                .expect("取消后循环必须退出")
                .expect("任务不应 panic")
                .expect("取消属正常收尾");
            assert_eq!(
                end.reason, "用户中断",
                "第 {round} 轮：取消应压过命令通道关闭"
            );
            assert!(
                end.deliberate,
                "第 {round} 轮：用户主动中断必须是 deliberate，否则会触发自动重连"
            );
            assert_eq!(
                sink.closed.lock().unwrap().as_deref(),
                Some("用户中断"),
                "第 {round} 轮：on_closed 的原因也应与之一致"
            );
        }
    }

    /// 假解码器记录到的生命周期调用（按发生顺序）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum DecoderCall {
        /// `configure(width, height)`。
        Configure(u32, u32),
        /// `decode(..)`。
        Decode,
    }

    /// 记录创建次数与生命周期调用的假解码器工厂。
    #[derive(Debug)]
    struct RecordingDecoderFactory {
        codec: CodecId,
        created: Arc<std::sync::atomic::AtomicUsize>,
        calls: Arc<Mutex<Vec<DecoderCall>>>,
    }

    impl VideoDecoderFactory for RecordingDecoderFactory {
        fn codec(&self) -> CodecId {
            self.codec
        }

        fn create(&self) -> RemoteResult<Box<dyn VideoDecoderApi>> {
            self.created.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(RecordingDecoder {
                calls: Arc::clone(&self.calls),
            }))
        }
    }

    /// 假解码器：记录 `configure` / `decode` 顺序，固定返回一帧 2×2。
    struct RecordingDecoder {
        calls: Arc<Mutex<Vec<DecoderCall>>>,
    }

    impl VideoDecoderApi for RecordingDecoder {
        fn configure(&mut self, width: u32, height: u32) -> RemoteResult<()> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(DecoderCall::Configure(width, height));
            Ok(())
        }

        fn decode(
            &mut self,
            _data: &[u8],
        ) -> RemoteResult<Option<crate::video::DecodedVideoFrame>> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(DecoderCall::Decode);
            Ok(Some(crate::video::DecodedVideoFrame {
                width: 2,
                height: 2,
                rgba: vec![0u8; 2 * 2 * 4],
                nv12: None,
            }))
        }
    }

    /// 记录收到帧数的假上报目标。
    #[derive(Default)]
    struct CountingSink {
        frames: std::sync::atomic::AtomicUsize,
    }

    impl FrameSink for CountingSink {
        fn on_displays(&self, _displays: Vec<DisplayInfo>) {}
        fn on_frame(&self, _frame: DecodedFrame) {
            self.frames.fetch_add(1, Ordering::SeqCst);
        }
        fn on_closed(&self, _reason: String) {}
    }

    /// **协商不变量**：`CodecSelect{H264}` 必须让解码任务改用注入的 H.264 工厂建解码器。
    ///
    /// 破坏它会怎样：控制器仍然建 VP9 解码器，H.264 字节流每帧解码失败 → 无限请求关键帧，
    /// 画面永远黑屏。这里用记录创建次数的假工厂做机械断言。
    #[tokio::test]
    async fn codec_select_uses_injected_h264_decoder_factory() {
        let queue = Arc::new(DecodeQueue::new());
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<DecodeNotice>();
        let sink = Arc::new(CountingSink::default());
        let cancel = CancellationToken::new();
        let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory: Arc<dyn VideoDecoderFactory> = Arc::new(RecordingDecoderFactory {
            codec: CodecId::H264,
            created: Arc::clone(&created),
            calls: Arc::clone(&calls),
        });
        let task = tokio::spawn(decode_loop(
            Arc::clone(&queue),
            notice_tx,
            sink.clone(),
            cancel.clone(),
            Arc::new(DecodeShared::new()),
            Some(factory),
            SessionStatsReporter::noop(),
        ));

        queue.push(DecodeJob::SelectCodec(CodecId::H264));
        queue.push(DecodeJob::Packet {
            display: 0,
            seq: 1,
            codec: CodecId::H264,
            flags: VIDEO_FLAG_KEYFRAME,
            width: 2,
            height: 2,
            data: vec![1, 2, 3],
            capture_ts_us: 0,
            recv_us: 0,
        });

        // 等解码任务真的建了解码器并投出一帧。
        let decoded = tokio::time::timeout(WAIT, async {
            for _ in 0..200 {
                if sink.frames.load(Ordering::SeqCst) >= 1 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            false
        })
        .await
        .expect("解码任务应及时出帧");
        assert!(decoded, "应收到解码帧");
        assert_eq!(
            created.load(Ordering::SeqCst),
            1,
            "H.264 选择必须由注入的工厂创建解码器（且只建一次）"
        );
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[DecoderCall::Configure(2, 2), DecoderCall::Decode],
            "首个包必须先 configure 编码尺寸再 decode"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, task).await;
    }

    /// **解码器生命周期**：首个包先 `configure` 再 `decode`；尺寸不变不重复 configure；
    /// 尺寸变化再次 configure。
    ///
    /// 这是 H.264 硬件解码器的硬需求：MFT 必须在建/重建时知道 `MF_MT_FRAME_SIZE`，而尺寸只
    /// 存在于 `VideoPacket` 里（`ScreenConfig` 不经过解码队列）。VP9 用默认空实现，不受影响。
    #[tokio::test]
    async fn decoder_configure_runs_on_first_packet_and_on_size_change_only() {
        let queue = Arc::new(DecodeQueue::new());
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<DecodeNotice>();
        let sink = Arc::new(CountingSink::default());
        let cancel = CancellationToken::new();
        let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory: Arc<dyn VideoDecoderFactory> = Arc::new(RecordingDecoderFactory {
            codec: CodecId::H264,
            created: Arc::clone(&created),
            calls: Arc::clone(&calls),
        });
        let task = tokio::spawn(decode_loop(
            Arc::clone(&queue),
            notice_tx,
            sink.clone(),
            cancel.clone(),
            Arc::new(DecodeShared::new()),
            Some(factory),
            SessionStatsReporter::noop(),
        ));

        queue.push(DecodeJob::SelectCodec(CodecId::H264));

        let packet = |seq: u64, flags: u8, width: u32, height: u32| DecodeJob::Packet {
            display: 0,
            seq,
            codec: CodecId::H264,
            flags,
            width,
            height,
            data: vec![1, 2, 3],
            capture_ts_us: 0,
            recv_us: 0,
        };
        // 首包（关键帧）→ configure + decode。
        queue.push(packet(1, VIDEO_FLAG_KEYFRAME, 64, 48));
        // 同尺寸 → 只 decode，不重复 configure。
        queue.push(packet(2, 0, 64, 48));
        // 尺寸变化（关键帧）→ 再次 configure + decode。
        queue.push(packet(3, VIDEO_FLAG_KEYFRAME, 96, 64));

        let decoded = tokio::time::timeout(WAIT, async {
            for _ in 0..200 {
                if sink.frames.load(Ordering::SeqCst) >= 3 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            false
        })
        .await
        .expect("解码任务应及时出帧");
        assert!(decoded, "应收到 3 帧");

        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[
                DecoderCall::Configure(64, 48),
                DecoderCall::Decode,
                DecoderCall::Decode,
                DecoderCall::Configure(96, 64),
                DecoderCall::Decode,
            ],
            "首个包先 configure；同尺寸不重复；尺寸变化再 configure"
        );
        assert_eq!(created.load(Ordering::SeqCst), 1, "尺寸变化不应重建解码器");

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, task).await;
    }

    // ── 协议 v5 脏矩形补丁（控制器侧）────────────────────────────────

    /// 上报目标看到的事件（顺序即 [`FrameSink`] 的调用顺序）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum PatchEvent {
        /// 一帧全帧。
        Frame,
        /// 一批补丁。
        Patch(DecodedPatch),
    }

    /// 记录全帧/补丁及其**调用顺序**的假上报目标。
    #[derive(Default)]
    struct PatchSink {
        frames: std::sync::atomic::AtomicUsize,
        events: Mutex<Vec<PatchEvent>>,
    }

    impl FrameSink for PatchSink {
        fn on_displays(&self, _displays: Vec<DisplayInfo>) {}
        fn on_frame(&self, _frame: DecodedFrame) {
            self.frames.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(PatchEvent::Frame);
        }
        fn on_patch(&self, patch: DecodedPatch) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(PatchEvent::Patch(patch));
        }
        fn on_closed(&self, _reason: String) {}
    }

    impl PatchSink {
        fn frames(&self) -> usize {
            self.frames.load(Ordering::SeqCst)
        }

        /// 已上屏的补丁批（按到达顺序）。
        fn patches(&self) -> Vec<DecodedPatch> {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter_map(|event| match event {
                    PatchEvent::Patch(patch) => Some(patch.clone()),
                    PatchEvent::Frame => None,
                })
                .collect()
        }

        /// 取走最早一批尚未取走的补丁。
        fn take_next_patch(&self) -> Option<DecodedPatch> {
            let mut guard = self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let index = guard
                .iter()
                .position(|event| matches!(event, PatchEvent::Patch(_)))?;
            match guard.remove(index) {
                PatchEvent::Patch(patch) => Some(patch),
                PatchEvent::Frame => None,
            }
        }
    }

    /// 解码任务测试装置：H.264 假工厂 + 记录型 sink + 通知接收端。
    struct DecodeHarness {
        queue: Arc<DecodeQueue>,
        notices: mpsc::UnboundedReceiver<DecodeNotice>,
        sink: Arc<PatchSink>,
        created: Arc<std::sync::atomic::AtomicUsize>,
        cancel: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    impl DecodeHarness {
        fn spawn() -> Self {
            let queue = Arc::new(DecodeQueue::new());
            let (notice_tx, notices) = mpsc::unbounded_channel::<DecodeNotice>();
            let sink = Arc::new(PatchSink::default());
            let cancel = CancellationToken::new();
            let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let factory: Arc<dyn VideoDecoderFactory> = Arc::new(RecordingDecoderFactory {
                codec: CodecId::H264,
                created: Arc::clone(&created),
                calls: Arc::new(Mutex::new(Vec::new())),
            });
            let task = tokio::spawn(decode_loop(
                Arc::clone(&queue),
                notice_tx,
                sink.clone(),
                cancel.clone(),
                Arc::new(DecodeShared::new()),
                Some(factory),
                SessionStatsReporter::noop(),
            ));
            Self {
                queue,
                notices,
                sink,
                created,
                cancel,
                task,
            }
        }

        /// 走完「选 codec → 首帧关键帧」，让解码任务进入可应用补丁的状态。
        async fn prime(&self, width: u32, height: u32) {
            self.queue.push(DecodeJob::SelectCodec(CodecId::H264));
            self.queue.push(DecodeJob::Packet {
                display: 0,
                seq: 1,
                codec: CodecId::H264,
                flags: VIDEO_FLAG_KEYFRAME,
                width,
                height,
                data: vec![1, 2, 3],
                capture_ts_us: 0,
                recv_us: 0,
            });
            let decoded = tokio::time::timeout(WAIT, async {
                for _ in 0..200 {
                    if self.sink.frames() >= 1 {
                        return true;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                false
            })
            .await
            .expect("解码任务应及时出帧");
            assert!(decoded, "prime 应解出一个全帧");
        }

        fn region(&self, width: u32, height: u32, rects: Vec<RegionRect>) {
            self.queue.push(DecodeJob::Region {
                display: 0,
                width,
                height,
                rects,
                capture_ts_us: 0,
                recv_us: 0,
            });
        }

        /// 等一批补丁上屏（解码是异步的）。
        async fn next_patch(&self) -> DecodedPatch {
            tokio::time::timeout(WAIT, async {
                loop {
                    if let Some(patch) = self.sink.take_next_patch() {
                        return patch;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("应收到补丁批")
        }

        /// 等解码任务报告「参考链断了」；返回 `(显示器, 原因)`。
        async fn expect_keyframe(&mut self) -> (u32, KeyframeReason) {
            loop {
                match tokio::time::timeout(WAIT, self.notices.recv())
                    .await
                    .expect("补丁被丢弃时必须报告断链")
                    .expect("通知通道不应关闭")
                {
                    DecodeNotice::Broken { display, reason } => return (display, reason),
                    // 只要解码成功就会有一条 `Satisfied`，跳过它继续等断链通知。
                    DecodeNotice::Satisfied => {}
                }
            }
        }
    }

    impl Drop for DecodeHarness {
        fn drop(&mut self) {
            self.cancel.cancel();
            self.task.abort();
        }
    }

    /// 断言通知通道里没有「参考链断了」（`Satisfied` 忽略不计）。
    fn assert_no_break(notices: &mut mpsc::UnboundedReceiver<DecodeNotice>) {
        while let Ok(notice) = notices.try_recv() {
            assert!(
                !matches!(notice, DecodeNotice::Broken { .. }),
                "不应报告断链，实得 {notice:?}"
            );
        }
    }

    /// **解压正确性**：zstd 矩形与原始矩形都要原样变成 [`ScreenPatch`] 交给 sink。
    #[tokio::test]
    async fn region_update_delivers_zstd_and_raw_rects() {
        let mut harness = DecodeHarness::spawn();
        harness.prime(8, 8).await;

        let zstd_nv12 = vec![0x11_u8; 4 * 4 * 3 / 2];
        let zstd_data = zstd::bulk::compress(&zstd_nv12, 1).expect("压缩应成功");
        let raw_nv12 = vec![0x22_u8; 2 * 2 * 3 / 2];
        harness
            .region(
                8,
                8,
                vec![
                    RegionRect {
                        x: 0,
                        y: 0,
                        width: 4,
                        height: 4,
                        flags: REGION_FLAG_ZSTD,
                        data: zstd_data,
                    },
                    RegionRect {
                        x: 4,
                        y: 4,
                        width: 2,
                        height: 2,
                        flags: 0,
                        data: raw_nv12.clone(),
                    },
                ],
            );

        let patch = harness.next_patch().await;
        assert_eq!(patch.display, 0);
        assert_eq!((patch.width, patch.height), (8, 8));
        assert_eq!(
            patch.rects,
            vec![
                ScreenPatch {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                    nv12: zstd_nv12,
                },
                ScreenPatch {
                    x: 4,
                    y: 4,
                    width: 2,
                    height: 2,
                    nv12: raw_nv12,
                },
            ],
            "两种 flags 都应解成同一份紧包 NV12"
        );
        assert_no_break(&mut harness.notices);
    }

    /// **基底不存在**：首个全帧之前到达的补丁没有可应用的目标，必须丢弃并请求关键帧。
    #[tokio::test]
    async fn region_update_before_any_video_packet_drops_and_requests_keyframe() {
        let mut harness = DecodeHarness::spawn();
        // 只选 codec、不投视频包：`configured_dims` 仍为空。
        harness.queue.push(DecodeJob::SelectCodec(CodecId::H264));
        harness
            .region(
                8,
                8,
                vec![RegionRect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                    flags: 0,
                    data: vec![0_u8; 6],
                }],
            );

        assert_eq!(
            harness.expect_keyframe().await,
            (0, KeyframeReason::DecodeError)
        );
        assert!(
            harness.sink.patches().is_empty(),
            "没有全帧基底时补丁不得上屏"
        );
    }

    /// 起一个已 prime 的解码任务，投一批非法补丁，断言「丢弃 + 请求关键帧 + 零上屏」。
    async fn assert_bad_region_requests_keyframe(rect: RegionRect) {
        let mut harness = DecodeHarness::spawn();
        harness.prime(8, 8).await;
        harness.region(8, 8, vec![rect]);
        assert_eq!(
            harness.expect_keyframe().await,
            (0, KeyframeReason::DecodeError)
        );
        assert!(harness.sink.patches().is_empty(), "非法补丁不得上屏");
    }

    /// **载荷损坏/长度不符**：解压失败与原始长度错误都必须整批丢弃并请求关键帧。
    #[tokio::test]
    async fn corrupt_or_mismatched_region_payload_requests_keyframe() {
        // 不是合法 zstd 帧（魔数都不对）。
        assert_bad_region_requests_keyframe(RegionRect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
            flags: REGION_FLAG_ZSTD,
            data: vec![0xFF, 0x00, 0x13],
        })
        .await;
        // 未压缩但长度不等于 2*2*3/2 = 6。
        assert_bad_region_requests_keyframe(RegionRect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
            flags: 0,
            data: vec![0_u8; 5],
        })
        .await;
    }

    /// **几何非法**：奇数坐标/奇数尺寸/越界都必须拒绝（否则 UV 会错位或写出缓冲）。
    #[tokio::test]
    async fn invalid_region_geometry_requests_keyframe() {
        // 奇数 X。
        assert_bad_region_requests_keyframe(RegionRect {
            x: 1,
            y: 0,
            width: 2,
            height: 2,
            flags: 0,
            data: vec![0_u8; 6],
        })
        .await;
        // 奇数高度。
        assert_bad_region_requests_keyframe(RegionRect {
            x: 0,
            y: 0,
            width: 2,
            height: 3,
            flags: 0,
            data: vec![0_u8; 9],
        })
        .await;
        // 越界：x + width = 10 > 8。
        assert_bad_region_requests_keyframe(RegionRect {
            x: 6,
            y: 0,
            width: 4,
            height: 2,
            flags: 0,
            data: vec![0_u8; 12],
        })
        .await;
    }

    /// **顺序保证**：全帧先于它的补丁到达 sink（同一条解码队列 ⇒ 调用顺序即线上顺序）。
    ///
    /// 破坏它会怎样：UI 把补丁拼到一块还没建立的合成帧上（或拼到上一帧上），画面缺块。
    #[tokio::test]
    async fn patch_is_delivered_after_the_full_frame_it_builds_on() {
        let harness = DecodeHarness::spawn();
        harness.prime(8, 8).await;
        harness
            .region(
                8,
                8,
                vec![RegionRect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                    flags: 0,
                    data: vec![7_u8; 6],
                }],
            );
        // 等补丁上屏，但**不要取走**事件——下面要检查完整顺序。
        tokio::time::timeout(WAIT, async {
            loop {
                if !harness.sink.patches().is_empty() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("应收到补丁批");

        let events = harness
            .sink
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(events.len(), 2, "一次全帧 + 一批补丁只应产生两个事件");
        assert!(
            matches!(events.first(), Some(PatchEvent::Frame)),
            "首个事件必须是全帧"
        );
        assert!(
            matches!(events.get(1), Some(PatchEvent::Patch(_))),
            "补丁必须排在它的基底全帧之后"
        );
    }

    /// 造一条视频包（内容无所谓，解码器是假的）。
    fn packet(seq: u64, keyframe: bool) -> DecodeJob {
        DecodeJob::Packet {
            display: 0,
            seq,
            codec: CodecId::H264,
            flags: if keyframe { VIDEO_FLAG_KEYFRAME } else { 0 },
            width: 2,
            height: 2,
            data: vec![1, 2, 3],
            capture_ts_us: 0,
            recv_us: 0,
        }
    }

    /// 造一批补丁。
    fn patch_batch() -> DecodeJob {
        DecodeJob::Region {
            display: 0,
            width: 2,
            height: 2,
            rects: vec![RegionRect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
                flags: 0,
                data: vec![0_u8; 6],
            }],
            capture_ts_us: 0,
            recv_us: 0,
        }
    }

    /// **按参考链截断**：只保留最新关键帧及其之后的条目（`SelectCodec` 永远保留）。
    ///
    /// H.264/VP9 的参考链是线性的：`P5` 依赖 `P4` 依赖 `P3`……单独丢一帧会让排在它后面的
    /// 帧全部解出花屏。唯一安全的切点因此是关键帧。
    #[test]
    fn flush_keep_truncates_to_the_newest_keyframe() {
        use JobKind::{Codec, Frame, Keyframe, Patch};
        // 没有关键帧：一个都不留（整条链作废）。
        assert_eq!(flush_keep(&[Frame, Frame, Frame, Frame]), [false; 4]);
        // 关键帧在中间：丢掉它之前那段（它们排在关键帧之前，本来就会被跳过）。
        assert_eq!(
            flush_keep(&[Frame, Frame, Keyframe, Frame]),
            [false, false, true, true]
        );
        // 关键帧在队首：全都保留（参考链完好）。
        assert_eq!(
            flush_keep(&[Keyframe, Frame, Patch, Frame]),
            [true, true, true, true]
        );
        // 有多张关键帧：以**最新**那张为切点。
        assert_eq!(
            flush_keep(&[Keyframe, Frame, Keyframe, Frame]),
            [false, false, true, true]
        );
        // 控制项在任何位置都不丢。
        assert_eq!(flush_keep(&[Codec, Frame, Frame, Frame]), [true, false, false, false]);
        assert_eq!(flush_keep(&[]), Vec::<bool>::new());
    }

    /// `[P,P,P,P] + P5` → **整队清空且 P5 不入队**（它引用的链全被丢了，留下只会花屏）。
    #[test]
    fn queue_flush_discards_everything_when_no_keyframe_is_queued() {
        assert_eq!(DECODE_QUEUE, 4, "16 帧 @30fps 等于 533 ms 陈旧画面缓冲");
        let queue = DecodeQueue::new();
        for seq in 1..=u64::try_from(DECODE_QUEUE).unwrap() {
            assert!(matches!(queue.push(packet(seq, false)), PushOutcome::Queued));
        }
        match queue.push(packet(5, false)) {
            PushOutcome::Flushed(report) => {
                assert_eq!(
                    report,
                    FlushReport {
                        dropped_frames: 4,
                        dropped_patches: 0,
                        admitted: false,
                        anchored: false,
                    },
                    "没有关键帧基准时必须整队作废，且本包也一起丢"
                );
            }
            other => panic!("满队列应走截断路径，实得 {other:?}"),
        }
        assert_eq!(queue.backlog(), 0, "队列必须清空");
    }

    /// `[P,P,IDR,P] + P5` → 保留 `[IDR,P]` 并收下 P5（关键帧之后参考链完好，不花屏、不请 IDR）。
    #[test]
    fn queue_flush_keeps_frames_after_the_newest_keyframe() {
        let queue = DecodeQueue::new();
        queue.push(packet(1, false));
        queue.push(packet(2, false));
        queue.push(packet(3, true));
        queue.push(packet(4, false));
        match queue.push(packet(5, false)) {
            PushOutcome::Flushed(report) => {
                assert_eq!(
                    report,
                    FlushReport {
                        dropped_frames: 2,
                        dropped_patches: 0,
                        admitted: true,
                        anchored: true,
                    }
                );
            }
            other => panic!("满队列应走截断路径，实得 {other:?}"),
        }
        let seqs = queue.lock().iter().map(DecodeJob::seq).collect::<Vec<_>>();
        assert_eq!(seqs, vec![Some(3), Some(4), Some(5)], "保留 [IDR,P] + 本包");
    }

    /// `[P,P,P,P] + IDR` → 只留 IDR（它就是新基准，旧链整体作废）。
    #[test]
    fn queue_flush_on_keyframe_keeps_only_the_keyframe() {
        let queue = DecodeQueue::new();
        for seq in 1..=u64::try_from(DECODE_QUEUE).unwrap() {
            queue.push(packet(seq, false));
        }
        match queue.push(packet(9, true)) {
            PushOutcome::Flushed(report) => {
                assert_eq!(
                    report,
                    FlushReport {
                        dropped_frames: 4,
                        dropped_patches: 0,
                        admitted: true,
                        anchored: true,
                    }
                );
            }
            other => panic!("满队列 + 关键帧应走截断路径，实得 {other:?}"),
        }
        let seqs = queue.lock().iter().map(DecodeJob::seq).collect::<Vec<_>>();
        assert_eq!(seqs, vec![Some(9)], "关键帧永远进得去，且旧链整体作废");
    }

    /// 控制项**永不丢弃**：满队列里 `SelectCodec` 必须活过截断。
    #[test]
    fn queue_flush_never_drops_control_items() {
        let queue = DecodeQueue::new();
        queue.push(DecodeJob::SelectCodec(CodecId::H264));
        for seq in 1..=u64::try_from(DECODE_QUEUE - 1).unwrap() {
            queue.push(packet(seq, false));
        }
        // 没有关键帧 → 载荷清空、控制项保留、本包被拒。
        assert!(matches!(
            queue.push(packet(9, false)),
            PushOutcome::Flushed(FlushReport {
                admitted: false,
                anchored: false,
                ..
            })
        ));
        assert_eq!(queue.backlog(), 1, "只剩 SelectCodec");
        assert!(matches!(
            queue.lock().front().map(DecodeJob::kind),
            Some(JobKind::Codec)
        ));
    }

    /// 满队列 + 补丁 → **拒绝本批**（补丁是增量的，丢一批会在合成帧上永久缺块，
    /// 只能换一整帧重建）。
    #[test]
    fn queue_rejects_patches_when_full() {
        let queue = DecodeQueue::new();
        for seq in 1..=u64::try_from(DECODE_QUEUE).unwrap() {
            queue.push(packet(seq, false));
        }
        assert!(matches!(queue.push(patch_batch()), PushOutcome::Dropped));
        assert_eq!(queue.backlog(), u32::try_from(DECODE_QUEUE).unwrap());
    }

    /// 空闲间隔：未解码过给 0（哨兵）、正常取整到毫秒、时钟倒退饱和为 0。
    #[test]
    fn idle_ms_handles_sentinel_truncates_and_saturates() {
        // 本会话还没解码过：不能用「进程启动至今」当空闲。
        assert_eq!(idle_ms(10_000_000, 0), 0);
        // 正常：上一帧解码完成于 600 ms 前。
        assert_eq!(idle_ms(10_000_000, 9_400_000), 600);
        // 亚毫秒截断，与日志里其余毫秒读数一致。
        assert_eq!(idle_ms(10_000_000, 9_999_500), 0);
        // 时间倒退（理论上不该发生）：饱和为 0，不出现天文数字。
        assert_eq!(idle_ms(1_000, 2_000), 0);
    }

    /// **闸门的清除点是「IDR 入队」，不是「IDR 解出」**（本次修复的核心不变量）。
    ///
    /// 旧实现要等 IDR 解码成功才放行 P 帧，而真机上 IDR 解码本身可能 445 ms（DXVA 回读
    /// 停顿）：这段时间里到达的 P 帧全被丢 → 每张都造成序号断层 → 再请一张 IDR →
    /// 一次事件换回 3 张 IDR、63 帧连续 `reason=awaiting`。
    ///
    /// 破坏它会怎样：把 `on_keyframe_enqueued` 挪回「解码成功之后」，本用例立刻失败。
    #[test]
    fn chain_gate_opens_when_the_keyframe_is_enqueued_not_decoded() {
        let mut gate = ChainGate::default();
        // 正常：全放行。
        assert!(!gate.should_drop(false));
        assert!(!gate.should_drop(true));
        // 丢了一帧：P 帧被闸门挡下，关键帧仍放行（否则永远恢复不了）。
        gate.on_drop();
        assert!(gate.should_drop(false), "断链后 P 帧必须丢");
        assert!(!gate.should_drop(true), "关键帧永远放行");
        // **IDR 入队**：紧跟着的 P 帧必须放行（它们排在 IDR 后面，可以正常解码）。
        gate.on_keyframe_enqueued();
        assert!(
            !gate.should_drop(false),
            "IDR 一入队，后续 P 帧就必须放行——不能等它解码成功"
        );
    }

    /// 关键帧请求闸门：首个事件立刻发；窗口内的事件只去重不发；窗口过后按 tick 重试；
    /// IDR 解出后清空（下一次断链立刻重发）。
    #[test]
    fn keyframe_gate_sends_now_dedupes_then_retries_until_satisfied() {
        let t0 = Instant::now();
        let mut gate = KeyframeGate::default();
        assert!(!gate.is_waiting());
        assert_eq!(gate.on_tick(t0), None, "没在等就不该有请求");

        // 首个断链事件立刻发一次。
        assert_eq!(
            gate.on_break(0, KeyframeReason::SeqGap, t0),
            Some((0, KeyframeReason::SeqGap))
        );
        assert!(gate.is_waiting());

        // 窗口内的事件只更新目标（按显示器/原因去重），不发第二次。
        let half = t0 + KEYFRAME_REQUEST_INTERVAL / 2;
        assert_eq!(gate.on_break(1, KeyframeReason::QueueOverflow, half), None);
        assert_eq!(gate.on_tick(half), None, "重试必须等满一个窗口");

        // 窗口边界：重试沿用**最新**的目标（去重后的那一条）。
        let due = t0 + KEYFRAME_REQUEST_INTERVAL;
        assert_eq!(
            gate.on_tick(due),
            Some((1, KeyframeReason::QueueOverflow)),
            "只要还在等，就一定有请求在路上"
        );
        // 重试也受同一窗口约束。
        assert_eq!(gate.on_tick(due), None);

        // IDR 解出：清空；下一次断链立刻重发（不残留节流状态）。
        gate.satisfied();
        assert!(!gate.is_waiting());
        assert_eq!(gate.on_tick(due), None);
        assert_eq!(
            gate.on_break(0, KeyframeReason::DecodeError, due),
            Some((0, KeyframeReason::DecodeError))
        );
    }

    /// 序号语义：**关键帧永远不算断层**（它本身就是新参考链的起点）。
    ///
    /// 旧实现不区分，于是每次丢包之后到达的 IDR 都被判成断层 → 销毁解码器 + 再请求一张
    /// IDR；这是「一次事件换回 3 张 IDR」的另一半来源。
    #[test]
    fn seq_gap_ignores_keyframes_and_first_packet() {
        // 首个包（没有期望值）不算断层。
        assert!(!seq_is_gap(None, 7, false));
        // 连续的两个 P 帧：没有断层。
        assert!(!seq_is_gap(Some(3), 3, false));
        // 跳号：真断层。
        assert!(seq_is_gap(Some(3), 5, false));
        // 跳号但来的是关键帧：正常（主循环丢过一批 P 帧之后 IDR 必然跳号）。
        assert!(!seq_is_gap(Some(3), 5, true));
    }

    /// **解码器不因丢包而重建**：丢了几帧之后来一张 IDR，解码器实例应原样继续用。
    ///
    /// 破坏它会怎样：每次丢帧都销毁解码器 + 重新 `configure`，恢复更慢，且真机上每条重建
    /// 路径都伴随一次多余的 IDR 请求。
    #[tokio::test]
    async fn keyframe_after_intentional_drops_resumes_without_rebuilding_decoder() {
        let mut harness = DecodeHarness::spawn();
        harness.prime(8, 8).await;
        assert_eq!(harness.created.load(Ordering::SeqCst), 1);

        // 主循环有意丢掉了 seq 2..=5（那些包根本不会到这里），然后补一张 IDR seq=6。
        harness.queue.push(packet(6, true));
        tokio::time::timeout(WAIT, async {
            loop {
                if harness.sink.frames() >= 2 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("IDR 应正常解出");

        assert_eq!(
            harness.created.load(Ordering::SeqCst),
            1,
            "参考链断掉不该重建解码器：喂一张 IDR 就够"
        );
        assert_no_break(&mut harness.notices);
    }
}
