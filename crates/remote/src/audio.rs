//! 音频数据面的平台抽象：采集源与播放目标。
//!
//! 引擎（本 crate）只负责把 [`AudioSource`] 产出的编码包经 [`Lane::Audio`]
//! （[`crate::session::Lane::Audio`]）送到对端，并把收到的包按**到达顺序**交给
//! [`AudioSink`]。编解码、抖动缓冲、设备枚举与热插拔都在平台 crate
//! （Windows 上是 `rust-tunnel-win-audio`）——本 crate 保持零平台依赖。
//!
//! # 契约
//!
//! - [`AudioSource::read_packet`] 会**阻塞**（20 ms 一帧），只能在 `spawn_blocking`
//!   里调用；`stop` 标志是它唯一的取消入口，实现必须每 5–20 ms 检查一次。
//! - [`AudioSink::on_packet`] 在引擎的运行时线程上调用，**必须非阻塞**
//!   （实现通常只是 `try_send` 到播放线程）。丢弃由播放侧用 PLC 抹平。
//! - 音频的启停由控制端的 [`crate::session::SessionMessage::SetAudioEnabled`] 驱动，
//!   被控端**绝不会**在未收到该消息前采集。

use crate::error::RemoteResult;
use crate::session::{AudioCodec, SessionMessage};
use std::sync::atomic::AtomicBool;

/// 本端音频采集能力与格式（被控端 → 控制端的 [`SessionMessage::AudioStatus`] 的来源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioState {
    /// 采集是否已启动。
    pub enabled: bool,
    /// 本端是否具备采集能力。
    pub available: bool,
    /// 不可用/被停止的原因（中文）。
    pub reason: Option<String>,
    /// 编解码器。
    pub codec: AudioCodec,
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 声道数。
    pub channels: u8,
    /// 目标码率（kbps）。
    pub bitrate_kbps: u16,
}

impl AudioState {
    /// 本端没有音频能力时的状态（控制端据此把扬声器按钮置灰并显示原因）。
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            available: false,
            reason: Some(reason.into()),
            codec: AudioCodec::Opus,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 96,
        }
    }

    /// 组装成线上消息（`Control` lane）。
    #[must_use]
    pub fn to_message(&self) -> SessionMessage {
        SessionMessage::AudioStatus {
            enabled: self.enabled,
            available: self.available,
            reason: self.reason.clone(),
            codec: self.codec,
            sample_rate: self.sample_rate,
            channels: self.channels,
            bitrate_kbps: self.bitrate_kbps,
        }
    }
}

/// 一段编码后的音频（一帧 20 ms）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAudio {
    /// 不透明编码数据（Opus 包）。
    pub data: Vec<u8>,
    /// 采集完成时刻（被控端单调时钟，微秒）。
    pub capture_ts_us: u64,
}

/// 控制器收到的一段编码音频（带线上序号）。
///
/// 序号可能不连续：被控端在音频队列满时会丢**最新**包。播放侧据此知道有断层，用 PLC 抹平。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedAudio {
    /// 线上序号（被控端递增）。
    pub seq: u64,
    /// 采集完成时刻（被控端单调时钟，微秒）。
    pub capture_ts_us: u64,
    /// 不透明编码数据（Opus 包）。
    pub data: Vec<u8>,
}

/// 被控端音频采集源（平台实现）。
///
/// 实现负责「设备打开 → 20 ms 分块 → 编码」的整条链路；引擎只消费编码后的包。
/// 设备失效应在 `read_packet` 内部按退避重建，只有**放弃**时才返回错误（引擎会把
/// 状态置为不可用并继续会话）。
pub trait AudioSource: Send + Sync + 'static {
    /// 当前能力与格式。引擎在会话开始、启停切换时读取。
    fn capability(&self) -> AudioState;

    /// 打开设备并启动采集（由阻塞任务调用）。
    ///
    /// # Errors
    /// 设备不存在、权限不足或初始化失败时返回平台错误。
    fn start(&self) -> RemoteResult<()>;

    /// 阻塞读取下一段编码音频。
    ///
    /// 返回 `Ok(None)` 表示在实现内部等待窗口内没有数据（调用方应检查 `stop`）。
    /// `stop` 置位后实现应尽快返回 `Ok(None)`。
    ///
    /// # Errors
    /// 设备不可恢复地失效或编码失败时返回错误。
    fn read_packet(&self, stop: &AtomicBool) -> RemoteResult<Option<EncodedAudio>>;

    /// 停止采集并释放设备。
    fn stop(&self);
}

/// 控制端音频播放目标（UI/平台实现）。
///
/// **`on_packet` 在引擎运行时线程上调用，必须非阻塞**。
pub trait AudioSink: Send + Sync + 'static {
    /// 收到一段编码音频（按到达顺序；序号可能不连续——发送侧队列满会丢新包）。
    fn on_packet(&self, packet: ReceivedAudio);

    /// 对端音频状态变化（可用性/启停/原因）。
    fn on_status(&self, state: &AudioState);
}
