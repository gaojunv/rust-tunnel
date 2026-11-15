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

/// 控制器请求关键帧的原因（**进程内观测值，不上线**）。
///
/// 关键帧请求本身走既有 [`crate::session::SessionMessage::RequestKeyframe`]（线上只有
/// `display` 一个字段）；这里只是把「为什么请」拆开送进管理面，用于判断画面卡顿的真因：
/// - [`Self::SeqGap`] / [`Self::QueueOverflow`] → 本端解码队列在丢帧（链路/解码跟不上）；
/// - [`Self::DecodeError`] / [`Self::ConfigureFailed`] → 解码器/码流本身有问题；
/// - [`Self::NoDecoder`] → 工厂或 codec 协商缺失；
/// - [`Self::StalledNoFrame`] → 对端长时间不出帧（静止桌面的正常情况也会命中）；
/// - [`Self::Manual`] → 由人/上层显式发起（目前没有调用点，保留语义位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeReason {
    /// 帧序号断层：会话循环丢过包，参考链已断。
    SeqGap,
    /// 解码队列溢出：主循环推进了 `drop_epoch`，队列里的帧整体作废。
    QueueOverflow,
    /// 解码器返回错误（码流损坏/缺参考帧）。
    DecodeError,
    /// 没有可用的解码器（工厂缺失、codec 未知，或尚未创建成功）。
    NoDecoder,
    /// 解码器 `configure(width, height)` 失败。
    ConfigureFailed,
    /// 长时间未收到画面帧（[`crate::controller`] 的 `NO_FRAME_TIMEOUT` 看门狗）。
    StalledNoFrame,
    /// 人工/上层显式请求。
    Manual,
}

impl KeyframeReason {
    /// 变体总数（供定长计数器数组使用）。
    pub const COUNT: usize = 7;

    /// 稳定的数组下标（**只追加、不改顺序**，否则历史计数会错位）。
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::SeqGap => 0,
            Self::QueueOverflow => 1,
            Self::DecodeError => 2,
            Self::NoDecoder => 3,
            Self::ConfigureFailed => 4,
            Self::StalledNoFrame => 5,
            Self::Manual => 6,
        }
    }

    /// 面向 UI/日志的中文标签。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::SeqGap => "序号断层",
            Self::QueueOverflow => "队列溢出",
            Self::DecodeError => "解码失败",
            Self::NoDecoder => "无解码器",
            Self::ConfigureFailed => "配置失败",
            Self::StalledNoFrame => "长时间无帧",
            Self::Manual => "手动",
        }
    }
}

/// 会话循环 → 管理器的统计数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStats {
    /// 一次心跳往返测得的 RTT（复用既有 `Heartbeat` → `HeartbeatAck`，不新增数据面变体）。
    Rtt(Duration),
    /// 被控端**编码侧**读数（每秒一次，由编码任务上报）。
    Encode {
        /// 最近一秒的平均单帧编码耗时（毫秒）。
        encode_ms: u32,
        /// 最近一秒的平均单帧**采集**耗时（微秒，与编码帧一起统计）。
        ///
        /// 微秒而不是毫秒：GPU 采集常在 1 ms 以内，毫秒取整会恒为 0。
        capture_us: u32,
        /// 最近一秒实测编码帧率（帧/秒，即实际出帧速度，非目标值）。
        fps: u32,
        /// 当前**生效**的目标码率（kbps，闭环在上限之下浮动后的值）。
        bitrate_kbps: u32,
        /// 本上报窗口内因「无变化」而跳过的帧数（不编码、不发送）。
        skipped: u32,
        /// 本上报窗口内的**关键帧**数（`VideoPacket.keyframe` 为真）。
        keyframes: u32,
        /// 本窗口关键帧出码码率（kbps）。与 `p_kbps` 拆开后才看得出「字节到底花在 IDR
        /// 还是普通帧」，这是定位带宽尖峰的观测面。
        idr_kbps: u32,
        /// 本窗口关键帧的**累计出码字节数**（与 `idr_kbps` 同一窗口）。
        ///
        /// `idr_kbps` 是速率，单独看无法判断「一个关键帧到底多大」——同样的 kbps 可能来自
        /// 很多小 IDR，也可能来自少量巨大 IDR。UI 据此显示「关键帧平均 x KB」。
        idr_bytes: u64,
        /// 本窗口普通帧出码码率（kbps）。
        p_kbps: u32,
        /// 本窗口编码帧的**平均变化面积**（千分比，0..=1000）。
        ///
        /// 来自采集层的像素级变化检测：<5% 通常是 UI 操作，>20% 是运动/视频。它同时是
        /// 内容自适应上限的输入（见 `qos::RateController::observe_activity`），这里只做展示。
        changed_permille: u32,
    },
    /// 控制器**解码侧**读数（每秒一次，与 `QosFeedback` 同一节拍）。
    Decode {
        /// 最近一帧的解码耗时（毫秒）。
        decode_ms: u32,
        /// 最近一秒的接收码率（kbps）。
        recv_kbps: u32,
        /// 本窗口收到的 `VideoPacket` 数（含随后被解码队列丢弃的）。
        packets: u32,
        /// 本窗口因解码队列满而丢弃的包数（含等待 IDR 期间主动丢弃的 P 帧）。
        dropped: u32,
        /// 本窗口**已应用**到合成帧的补丁批数（协议 v5 脏矩形增量）。
        ///
        /// 统计的是解码侧真正交给 `FrameSink::on_patch` 的批次：因等待关键帧/几何不符被
        /// 丢弃的补丁不计入。补丁模式下全帧几乎为零，这一项是「增量通道在工作」的唯一读数。
        patches: u32,
    },
    /// 控制器发出的一次关键帧请求及原因（每秒可能多次；管理面按原因累计计数）。
    KeyframeRequest {
        /// 请求原因。
        reason: KeyframeReason,
    },
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
