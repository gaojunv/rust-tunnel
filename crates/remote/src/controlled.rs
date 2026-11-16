//! 被控端会话循环：握手鉴权 → 采集编码 → 注入输入 → 心跳。
//!
//! 只在 `enabled == true` 且收到合法 [`rust_tunnel_common::protocol::ControlMessage::RemoteSessionIncoming`]
//! 时才由 [`crate::manager::RemoteManager`] 启动。握手校验失败一律拒绝并记原因；会话结束/
//! 中断立即停循环、关连接。
//!
//! # 编码流水线（阶段 2）
//!
//! `select!` 循环只保留 cancel / heartbeat / 输入 / 文件 / 控制消息。画面走一条三阶段流水线：
//!
//! ```text
//! 采集(spawn_blocking) --有界(1)--> 编码任务(spawn_blocking) --有界(SCREEN_QUEUE)--> 发送(独占 writer)
//! ```
//!
//! # 出站三档队列（阶段 10）
//!
//! 写半边由**独立写任务**独占（[`writer_loop`]），主循环只投递帧。队列按优先级分成四档，
//! 写任务用 `select! { biased; ... }` 固定按 **close → ctrl → screen → file** 取帧：
//!
//! - **close**：收尾 `Bye`，无界通道，最高优先，不理会取消令牌；
//! - **ctrl**（深度 [`CTRL_QUEUE`]）：心跳/输入/QoS/剪贴板/文件协议小消息，可取消地等、绝不丢；
//! - **screen**（深度 [`SCREEN_QUEUE`]）：`VideoPacket`/`RegionUpdate`/`ScreenConfig`，可取消地等、
//!   绝不丢（时域参考链不能断）；
//! - **file**（深度 [`FILE_QUEUE`]）：只有 `FileChunk` 及其收尾（`FileDone`/`FileError`）。主循环
//!   **只在有容量时**用 `try_send` 投递，满了就留在 `file_staging` 下一拍重试——文件分块的生产
//!   因此**永不阻塞主循环**（否则采集会停拍、输入/心跳会停摆）。
//!
//! 分类由纯函数 [`outbound_class`] 决定，新增 `SessionMessage` 变体时编译器会强制在这里归类。
//!
//! **丢弃策略是这个管线里最关键的正确性决定**：时域编码的参考链是串起来的，丢掉一个**已编码**
//! 的 P 帧，后续引用它的帧在解码端永远解不出来（花屏且不自愈，只能等下一个 IDR）。所以背压的
//! 出口只能放在**编码之前**——输出队列满就跳过这一帧（等价于降帧率，语义合法），绝不在编码之后
//! 丢弃。**不要把这条改成「编码后丢弃」，那不是优化，是花屏。**
//!
//! 但「静止就不发」不能是绝对的。变化检测是**点采样**网格、存在盲区；编码前的背压出口、
//! 采集看门狗作废结果（此时 `changed` 水位已经推进）也会让「变化确实发生了」却一帧都没
//! 发出去。故静止时仍按 [`UNCHANGED_RESEND_INTERVAL`]（250 ms）补编一帧**普通 P 帧**作为
//! 保活——它只绕过「无变化就跳过」这一道闸门，**不会带出 IDR**。少了它，用户看到的是
//! 「远端静止一段时间后，第一次变化要反复几次才出现」。
//!
//! # QoS 闭环（阶段 3）
//!
//! 主循环持有 [`RateController`]（AIMD）：每收到一条控制器的 `QosFeedback` 就决策一次，把新的
//! **生效值**（[`EncodeRates`]）交给下一次编码任务。码率/帧率变化由编码任务**在线重配置**
//! （vendor 补丁 6），**不重建、不插 IDR**；只有降编码分辨率（几何变化）才重建 + 强制 IDR。
//! 详见 [`crate::qos`] 的模块文档。
//!
//! # 编解码协商（阶段 5）
//!
//! 握手加密完成后，控制器先发 `CodecOffer` 声明自己能解码的 codec。被控端**等它到了才开始
//! 采集**（否则首帧可能用了对端解不了的 codec），按「本端有 H.264 编码工厂 && 对端支持 H.264」
//! 选 codec，回一条 `CodecSelect`，再用对应工厂创建编码器。等待有 [`CODEC_OFFER_TIMEOUT`]
//! 上限：超时只记 `warn` 并回退 VP9，绝不永久卡住。版本硬相等已经把老对端挡在握手阶段，
//! 故这里没有额外的兼容分支。
//!
//! # 脏矩形补丁模式（阶段 8，Windows H.264）
//!
//! 桌面静止、只有小面积变化（打字、光标、菜单）时，整帧编码是浪费：一帧 IDR/P 帧要覆盖整个
//! 画面，而实际变了的可能只有几十分之一。协商到 **H.264** 且平台采集源支持时
//! （[`ScreenSource::supports_patches`]），被控端可在连续多次低活跃采集后切到**补丁模式**：
//! 不再走编码器，而是把脏矩形的紧包 NV12（zstd level 1 压缩，压不下去就存原始）装进
//! [`SessionMessage::RegionUpdate`] 直接发出。控制器维护一块持久 NV12 帧缓冲，逐块覆写。
//!
//! 三条硬约束：
//! - **顺序**：补丁必须排在那块全帧基准**之后**。进入模式前先强制一个基准 IDR，且只在该 IDR
//!   **真正投进出站队列**（编码任务回报 `base_frame_sent`）之后才切模式；两者走**同一条画面
//!   队列**（`ScreenConfig` 也在其中），故顺序天然成立。发送补丁走 [`OutboundQueues::enqueue_screen`]
//!   （可取消地等、绝不丢弃）。
//! - **退出即全帧**：任何让参考/几何失效的事件（`RequestKeyframe` / `SelectDisplay` /
//!   `SetQuality`、补丁采集失败、活跃度过高、矩形过多/过大）都立刻回到全帧并强制 IDR。
//! - **QoS 闭环在补丁模式下暂停**：控制器此时不解码视频，`decode_ms`/`recv_kbps` 是陈旧值，
//!   喂给 AIMD 只会制造假拥塞。回到全帧后的下一条 `QosFeedback` 自然恢复闭环。

use crate::auth::{ack_mac, PendingSessions};
use crate::codec::{vp9_encoder_factory, VideoEncoderApi, VideoEncoderFactory};
use crate::crypto::{derive_session_keys, Cipher, EphemeralKeyPair};
use crate::error::{RemoteError, RemoteResult};
use crate::qos::{Feedback, QosAction, RateController};
use crate::screen::{
    native_bitrate_ceiling, CaptureInfo, CapturedPatches, Clipboard, DisplayInfo, EncodeOptions,
    EncodeRates, EncoderInfo, InputSink, RawFrame, ScreenSource,
};
use crate::session::{
    trusted_rtt, CodecId, HostActivity, HostStats, RegionRect, SessionCodec, SessionEnd, SessionFrame,
    SessionMessage, HEARTBEAT_INTERVAL, LIVENESS_TIMEOUT, MAX_REGION_RECTS, REGION_FLAG_ZSTD,
    SESSION_PROTO_VERSION, VIDEO_FLAG_CONFIG, VIDEO_FLAG_KEYFRAME,
};
use crate::stats::{SessionStats, SessionStatsReporter};
use crate::transfer::{TransferHub, CHUNK_PAUSE};
use crate::transport::BoxDuplex;
use crate::video::{encode_dimensions, EncodedVideo, MAX_ENCODE_WIDTH, NATIVE_MAX_ENCODE_WIDTH};
use futures_util::{SinkExt as _, StreamExt as _};
use rust_tunnel_common::protocol::RemoteMode;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval_at, sleep, timeout, Instant, MissedTickBehavior, Sleep};
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// 等待 `Hello` 的超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 等待控制器 `CodecOffer` 的超时。
///
/// 控制器在握手完成后**立刻**发 offer，正常路径下这个等待几乎不耗时。超时是防御性的：
/// 对端实现异常（或将来某个控制器忘了发）时不能永久卡住，回退 VP9 继续会话。版本硬相等已经
/// 把老对端挡在握手阶段，所以这里不需要兼容分支，只需要「不永久挂起」。
const CODEC_OFFER_TIMEOUT: Duration = Duration::from_secs(5);

/// 收尾 `Bye` 的发送上限。对端可能已停读（写缓冲满），不加限时会让 teardown 永久卡住；`Bye`
/// 只是尽力通知，超时丢弃即可。
const BYE_TIMEOUT: Duration = Duration::from_secs(2);

/// 控制队列深度。
///
/// 控制帧只有几十字节，32 深足以吸收突发，同时把写侧被卡住时的等待时间压在很小量级。
pub(crate) const CTRL_QUEUE: usize = 32;

/// 画面队列深度。
///
/// 画面帧是大头（可达 MiB 级），队列过深会在写侧卡住时把内存顶上去；过浅则正常突发下容易丢帧。
/// 4 足以吸收一次采集与心跳的抖动，同时把最坏内存压在「4 × 单帧」量级。
pub(crate) const SCREEN_QUEUE: usize = 4;

/// 文件队列深度。
///
/// 只放 `FileChunk`（[`crate::transfer::CHUNK_SIZE`] = 16 KiB）及其收尾，2 块 ≈ 32 KiB 在途。
/// 主循环只在有空位时取下一块，队列容量就是这个方向唯一的背压。
pub(crate) const FILE_QUEUE: usize = 2;

/// 编码侧读数的上报间隔（见 [`EncodeStatsWindow`]）。
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// 显示器枚举的复查间隔。
///
/// 窗口枚举在 Windows 上是 `EnumDisplayMonitors`，有可测开销（要进内核问桌面拓扑），所以
/// 不复用采集节拍、也不做每帧检查。热插拔/改分辨率在 5 秒内可见，对用户是可接受的延迟；
/// 当前采集的显示器消失时也不会长期冻结画面（见主循环的 `display_tick` 分支）。
const DISPLAY_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// 在途心跳时刻队列的上限（1s 探测、10s 可信上限 ⇒ 8 个足够覆盖任何可信延迟）。
const HEARTBEAT_SENT_CAP: usize = 8;

/// 出站写队列的优先级分类（**控制器与被控端共用**）。
///
/// 分类是纯函数 [`outbound_class`] 的唯一职责：新增 [`SessionMessage`] 变体时，那个 `match`
/// 会因未覆盖而编译失败——避免新消息悄悄落进错误的队列。丢/不丢、能不能 `.await`、会不会
/// 打断参考链，全由类别决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutboundClass {
    /// 收尾 `Bye`：最高优先级，走独立无界通道，不理会取消令牌。
    Close,
    /// 小而敏感的控制帧：可取消地等空位，**绝不丢**。
    Control,
    /// 画面数据（`VideoPacket` / `RegionUpdate` / `ScreenConfig`）：可取消地等空位，**绝不丢**。
    Screen,
    /// 文件数据块与收尾（`FileChunk` / `FileDone` / `FileError`）：只用 `try_send`，满则暂存。
    File,
}

/// 把一条会话消息归入出站队列类别（纯函数，机械锁见单测）。
///
/// 两个容易归错的点：
/// - `ScreenConfig` 必须与 `VideoPacket` 同队列（它要排在第一个画面包之前）；
/// - `FileDone`/`FileError` 必须排在所有 `FileChunk` 之后，故与数据块同走 file 队列；
///   其余文件协议小消息（列目录/请求/受理）没有数据面顺序约束，走控制队列。
#[must_use]
pub(crate) fn outbound_class(msg: &SessionMessage) -> OutboundClass {
    match msg {
        SessionMessage::Bye { .. } => OutboundClass::Close,
        SessionMessage::VideoPacket { .. }
        | SessionMessage::RegionUpdate { .. }
        | SessionMessage::ScreenConfig { .. } => OutboundClass::Screen,
        SessionMessage::FileChunk { .. }
        | SessionMessage::FileDone { .. }
        | SessionMessage::FileError { .. } => OutboundClass::File,
        SessionMessage::Hello { .. }
        | SessionMessage::HelloAck { .. }
        | SessionMessage::Input(_)
        | SessionMessage::SelectDisplay { .. }
        | SessionMessage::SetQuality { .. }
        | SessionMessage::Clipboard { .. }
        | SessionMessage::ClipboardRequest
        | SessionMessage::DisplayList(_)
        | SessionMessage::Heartbeat { .. }
        | SessionMessage::HeartbeatAck { .. }
        | SessionMessage::FileListRequest { .. }
        | SessionMessage::FileList { .. }
        | SessionMessage::FileDownloadRequest { .. }
        | SessionMessage::FileUploadRequest { .. }
        | SessionMessage::FileAccept { .. }
        | SessionMessage::RequestKeyframe { .. }
        | SessionMessage::QosFeedback { .. }
        | SessionMessage::CodecOffer { .. }
        | SessionMessage::CodecSelect { .. }
        | SessionMessage::SetFrameRate { .. }
        | SessionMessage::LockScreen
        // 被控端每秒一条的整机遥测：小结构、时延不敏感，但**绝不能丢**，否则控制器的角标
        // 会停在旧读数上——与其它控制帧同档。
        | SessionMessage::HostStats(_) => OutboundClass::Control,
    }
}

/// 出站写侧的四个发送端（会话主循环持有）。
///
/// `file_staging` 是文件类消息的**暂存区**：file 队列满时消息留在这里、下一拍重试，主循环
/// 因此永远不必为文件数据 `.await`。它保持 FIFO，所以 `FileDone`/`FileError` 不会越过尚未
/// 投递的 `FileChunk`。
pub(crate) struct OutboundQueues {
    ctrl: mpsc::Sender<SessionFrame>,
    screen: mpsc::Sender<SessionFrame>,
    file: mpsc::Sender<SessionFrame>,
    close: mpsc::UnboundedSender<SessionFrame>,
    file_staging: VecDeque<SessionMessage>,
}

/// [`OutboundQueues`] 的另一半：四个接收端，整体移交给 [`writer_loop`]。
pub(crate) struct OutboundReceivers {
    ctrl: mpsc::Receiver<SessionFrame>,
    screen: mpsc::Receiver<SessionFrame>,
    file: mpsc::Receiver<SessionFrame>,
    close: mpsc::UnboundedReceiver<SessionFrame>,
}

impl OutboundQueues {
    /// 建立四个通道。返回的接收端必须移交给 [`writer_loop`]。
    #[must_use]
    pub(crate) fn new() -> (Self, OutboundReceivers) {
        let (ctrl_tx, ctrl_rx) = mpsc::channel(CTRL_QUEUE);
        let (screen_tx, screen_rx) = mpsc::channel(SCREEN_QUEUE);
        let (file_tx, file_rx) = mpsc::channel(FILE_QUEUE);
        let (close_tx, close_rx) = mpsc::unbounded_channel();
        (
            Self {
                ctrl: ctrl_tx,
                screen: screen_tx,
                file: file_tx,
                close: close_tx,
                file_staging: VecDeque::new(),
            },
            OutboundReceivers {
                ctrl: ctrl_rx,
                screen: screen_rx,
                file: file_rx,
                close: close_rx,
            },
        )
    }

    /// 画面发送端的克隆（编码任务需要独立持有）。
    pub(crate) fn screen_sender(&self) -> mpsc::Sender<SessionFrame> {
        self.screen.clone()
    }

    /// 当前画面队列里的在途帧数（QoS 的 `local_backlog` 读数）。
    #[must_use]
    pub(crate) fn screen_backlog(&self) -> usize {
        SCREEN_QUEUE.saturating_sub(self.screen.capacity())
    }

    /// 文件队列是否还有空位（主循环据此决定要不要再读一块）。
    #[must_use]
    pub(crate) fn file_has_room(&self) -> bool {
        self.file.capacity() > 0
    }

    /// 是否有文件类消息在暂存区等重试。
    #[must_use]
    pub(crate) fn has_staged_file(&self) -> bool {
        !self.file_staging.is_empty()
    }

    /// 收尾帧走最高优先级的无界通道（不阻塞、不理会取消）。
    pub(crate) fn close(&self, frame: SessionFrame) {
        // 写任务已退出时丢弃即可：会话已在收尾，没有别的接收者。
        let _ = self.close.send(frame);
    }

    /// 按 [`outbound_class`] 的分类投递一条出站帧。
    ///
    /// # Errors
    /// 取消令牌触发（ctrl/screen 等待空位时）或写任务已结束（通道关闭）时返回错误。
    pub(crate) async fn dispatch_frame(
        &mut self,
        cancel: &CancellationToken,
        frame: SessionFrame,
    ) -> RemoteResult<()> {
        match outbound_class(&frame.message) {
            OutboundClass::Close => {
                self.close(frame);
                Ok(())
            }
            OutboundClass::Control => self.enqueue_ctrl(cancel, frame).await,
            OutboundClass::Screen => self.enqueue_screen(cancel, frame).await,
            OutboundClass::File => self.try_enqueue_file(frame.message),
        }
    }

    /// 投进控制队列（可取消地等空位，**绝不丢**）。
    ///
    /// 帧的 `lane` 由调用方决定：`QosFeedback`/`RequestKeyframe` 在线上仍是 `Screen` lane，
    /// 但内容小、时延敏感，所以走控制队列。
    ///
    /// # Errors
    /// 取消令牌触发或写任务已结束时返回错误。
    pub(crate) async fn enqueue_ctrl(
        &self,
        cancel: &CancellationToken,
        frame: SessionFrame,
    ) -> RemoteResult<()> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(RemoteError::Cancelled),
            res = self.ctrl.send(frame) => res.map_err(|_| link_closed()),
        }
    }

    /// 投进画面队列（可取消地等空位，**绝不丢**；时域参考链不能断）。
    ///
    /// # Errors
    /// 取消令牌触发或写任务已结束时返回错误。
    pub(crate) async fn enqueue_screen(
        &self,
        cancel: &CancellationToken,
        frame: SessionFrame,
    ) -> RemoteResult<()> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(RemoteError::Cancelled),
            res = self.screen.send(frame) => res.map_err(|_| link_closed()),
        }
    }

    /// **非阻塞**地把一条文件类消息投进文件队列（满了就暂存，绝不 `.await`）。
    ///
    /// # Errors
    /// 写任务已结束（通道关闭）时返回错误。
    pub(crate) fn try_enqueue_file(&mut self, msg: SessionMessage) -> RemoteResult<()> {
        self.file_staging.push_back(msg);
        self.drain_file()
    }

    /// 把暂存区里的文件消息按序补投（队满即停，保持 FIFO）。
    ///
    /// # Errors
    /// 写任务已结束（通道关闭）时返回错误，暂存区一并清空。
    pub(crate) fn drain_file(&mut self) -> RemoteResult<()> {
        while let Some(msg) = self.file_staging.pop_front() {
            match self.file.try_send(SessionFrame::file(msg)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(frame)) => {
                    self.file_staging.push_front(frame.message);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.file_staging.clear();
                    return Err(link_closed());
                }
            }
        }
        Ok(())
    }
}

/// 在途采集的看门狗时限。
///
/// 采集与编码跑在 `spawn_blocking` 里（见 [`run_capture`]），结果经通道回传。可阻塞任务若
/// **panic**（`spawn_blocking` 的 `JoinHandle` 被丢弃，panic 被吞掉）或**永久挂住**（Wayland
/// 的 xcap portal 无响应），结果就永远不会回传，采集在途标记会一直亮着、差分器也随之丢失，
/// 表现为「会话还活着、输入与心跳都正常，但画面永久冻结且毫无报错」——比修复前（在循环里同步
/// 采集，卡住会让心跳停摆，hbbr 的 30s 空闲超时把会话踢掉）更难诊断。
///
/// 故给在途采集设一个远大于正常采集耗时的看门狗：正常 1080p 整帧编码是几十毫秒量级，即使
/// Wayland 路径「可能数秒」也远小于这个值。
pub const CAPTURE_WATCHDOG: Duration = Duration::from_secs(10);

/// 连续多少次采集超时后判定采集侧已彻底失效，结束会话。
///
/// 必须有上限：真正「永久挂住」的采集会让每次重试都再挂一个阻塞线程，无上限重试最终会把
/// `spawn_blocking` 的线程池（默认 512）填满，连累剪贴板与文件 IO。有上限时，一次偶发卡顿
/// （portal 正忙）只丢一帧并重建基线，连续卡死才终止会话、把原因交给 UI 显示。
const MAX_CAPTURE_TIMEOUTS: u32 = 3;

/// 静止画面下**保活补编**的最大间隔。
///
/// 「无变化就跳过」的闸门有几处真实盲区，任何一处都会让一次已发生的变化**一帧都没发出去**：
/// - 采集层的像素级检测是**点采样**网格（见 `win-video` 的 `capture::change`），小面积变化
///   可能整个落在采样点之间；
/// - 变化确实检测到了，但出站队列当时是满的，编码前的背压出口把这一帧跳过了；
/// - 采集结果被看门狗判定丢失时，`changed` 的水位已经推进到「已看过」，同一画面不会再被
///   当成新变化。
///
/// 所以静止时也要定期补编一帧：**普通 P 帧、不请求 IDR**，把「变化发生 → 控制器看到画面」
/// 的延迟上界压到 250 ms。代价可忽略：QSV/NVENC 实测静止 P 帧约 3 KB，4 fps ≈ 12 KB/s
/// ≈ 100 kbps（1080p；这一帧同时会喂给控制端的解码器，不会额外触发关键帧）。
pub const UNCHANGED_RESEND_INTERVAL: Duration = Duration::from_millis(250);

/// 进入补丁模式的**变化面积**门槛（千分比，3%）：低于它才算「安静」。
///
/// 与 [`PATCH_EXIT_PERMILLE`] 分开是**迟滞**：进入要求连续多次低活跃，退出只要一次高活跃。
/// 共用同一个值会在门槛附近来回抖，每次抖动都要一次基准 IDR（带宽尖峰），正好抵消补丁的收益。
const PATCH_ENTER_PERMILLE: u16 = 30;

/// 退出补丁模式的**变化面积**门槛（千分比，8%）：超过它立刻回到全帧。
const PATCH_EXIT_PERMILLE: u16 = 80;

/// 连续多少个**低活跃**采集才请求进入补丁模式（迟滞）。
const PATCH_ENTER_TICKS: u32 = 3;

/// 补丁模式下空补丁（保活）的最大间隔。
///
/// 控制器侧有「长时间无帧」看门狗；补丁模式下没有视频帧，空 `RegionUpdate` 就是它的心跳。
const PATCH_KEEPALIVE: Duration = Duration::from_millis(250);

/// 补丁模式的最大采集间隔（≈15 fps）。
///
/// 补丁本身很小（几 KB），帧率没必要跟着画质档位到 30/60：静桌面上的小变化以 15 fps 呈现
/// 已经足够跟手，再快只是重复发送「没有变化」。
const PATCH_MAX_INTERVAL: Duration = Duration::from_millis(66);

/// 脏矩形 NV12 的 zstd 压缩级别。
///
/// level 1 的编码速度远快于采集节拍，单块几 KB，收益却很明显（桌面文本/色块高度可压）。
const PATCH_ZSTD_LEVEL: i32 = 1;

/// 单条 `RegionUpdate` 压缩后**总字节**上限（64 KiB）。
///
/// 超过它说明脏矩形过多/过大，补丁已经不比全帧划算，且会给控制端的解压/合成带来尖峰——
/// 直接回全帧并强制 IDR。普通 UI 变化（一行文本/一个菜单）压缩后只有几 KB 到几十 KB；
/// 64 KiB 已经是很宽松的单批上限。
const PATCH_MAX_TOTAL_BYTES: usize = 64 * 1024;

/// 补丁出码的**窗口长度**（带宽保护）。
const PATCH_BANDWIDTH_WINDOW: Duration = Duration::from_secs(1);

/// 补丁出码允许超过当前生效码率的倍数。
///
/// 补丁模式下 QoS 闭环是暂停的（控制器未解码视频，反馈是陈旧值），所以必须自己兜住带宽：
/// 窗口内补丁出码超过 `有效码率 × 本倍数` 就回全帧。2× 给短促的 UI 突发留余量，又不会让
/// 「一个对话框 + 光标闪烁」把链路打满。
const PATCH_BANDWIDTH_FACTOR: u32 = 2;

/// 补丁窗口的码率地板（kbps）：档位目标很低时也允许一点突发。
const PATCH_MIN_BUDGET_KBPS: u32 = 500;

/// 补丁窗口的码率**上限**（kbps）。
///
/// 补丁模式可能在 QoS 的内容上限收紧之前就进入（连续 3 拍低活跃只要几百毫秒，而面积上限
/// 要 2 个 1s 的反馈 tick）。若直接拿 `rate.current()` 的 6/10 Mbps 当预算，补丁窗口的
/// 兜底就形同虚设。2 Mbps 对「UI 小面积变化」足够宽裕，超过它就说明该走全帧了。
const PATCH_MAX_BUDGET_KBPS: u32 = 2000;

/// 等基准 IDR 真正写出的上限。
///
/// 出站队列被背压卡住时基准帧可能迟迟发不出去；不能无限等（否则画面在补丁模式入口处停住）。
/// 超时就放弃本次进入，留在全帧视频路径，下次连续安静再试。
const PATCH_BASE_TIMEOUT: Duration = Duration::from_secs(1);

/// 画面编码模式（见模块文档的「脏矩形补丁模式」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenMode {
    /// 全帧视频（编码器出帧）。
    Video,
    /// 脏矩形补丁（不出视频帧，只发 `RegionUpdate`）。
    Patches,
}

/// 补丁模式的状态机（纯逻辑，便于单测锁定进入/退出判据）。
///
/// 时刻用 [`std::time::Instant`]（不是 tokio 的）：它只在本端做「过了多久」的比较，
/// 不参与任何定时器，测试因此可以直接构造时刻、不必推进虚拟时钟。
struct PatchMode {
    /// 当前模式。
    mode: ScreenMode,
    /// 连续「低活跃采集」的次数。
    enter_streak: u32,
    /// 已请求基准关键帧、正在等它真正写出。
    enter_pending: bool,
    /// 等待基准关键帧的截止时刻。
    enter_deadline: Option<std::time::Instant>,
    /// 上一次真正发出 `RegionUpdate` 的时刻（空补丁保活用）。
    last_patch_sent: Option<std::time::Instant>,
    /// 补丁出码统计窗口的起点（进入补丁模式时重置）。
    patch_window_start: std::time::Instant,
    /// 本窗口内已发出的补丁字节数。
    patch_window_bytes: u64,
}

impl PatchMode {
    fn new() -> Self {
        Self {
            mode: ScreenMode::Video,
            enter_streak: 0,
            enter_pending: false,
            enter_deadline: None,
            last_patch_sent: None,
            patch_window_start: std::time::Instant::now(),
            patch_window_bytes: 0,
        }
    }

    /// 记录一批补丁的**出码字节**，并在窗口到点时判断是否超预算。
    ///
    /// 返回 `false` 表示这一批不该发：要么上一窗口的补丁码率超过 `budget_kbps`，要么单批
    /// 本身就超过一个窗口的预算（后者防止一个巨大的补丁批把链路瞬时打满）。补丁模式下
    /// QoS 闭环是暂停的，这里是唯一的带宽兜底。
    fn admit_patch_bytes(
        &mut self,
        bytes: u64,
        now: std::time::Instant,
        budget_kbps: u32,
    ) -> bool {
        let elapsed = now.duration_since(self.patch_window_start);
        if elapsed >= PATCH_BANDWIDTH_WINDOW {
            let kbps = kbps_from_bytes(self.patch_window_bytes, elapsed);
            if kbps > budget_kbps {
                return false;
            }
            self.patch_window_start = now;
            self.patch_window_bytes = 0;
        }
        let budget_bytes = u64::from(budget_kbps).saturating_mul(1000) / 8;
        if bytes > budget_bytes {
            return false;
        }
        self.patch_window_bytes = self.patch_window_bytes.saturating_add(bytes);
        true
    }

    /// 当前是否处于补丁模式。
    fn is_patches(&self) -> bool {
        self.mode == ScreenMode::Patches
    }

    /// 用一次**视频**采集的变化面积推进进入判据。
    ///
    /// 返回 `true` 表示本帧必须作为**基准 IDR**：调用方据此置 `force_keyframe` 并清
    /// `base_frame_sent`（见模块文档的「顺序」约束）。
    fn observe_video(&mut self, changed_permille: u16, now: std::time::Instant) -> bool {
        // 已经在等基准帧：不重复触发（否则每帧都会重置截止时刻）。
        if self.enter_pending {
            return false;
        }
        if changed_permille <= PATCH_ENTER_PERMILLE {
            self.enter_streak = self.enter_streak.saturating_add(1);
        } else {
            self.enter_streak = 0;
        }
        if self.enter_streak < PATCH_ENTER_TICKS {
            return false;
        }
        self.enter_pending = true;
        self.enter_deadline = Some(now + PATCH_BASE_TIMEOUT);
        true
    }

    /// 在采集节拍上推进「等基准帧」的状态。
    ///
    /// `Some(true)` = 已进入补丁模式；`Some(false)` = 超时放弃；`None` = 继续等（仍在视频模式）。
    fn poll_entry(&mut self, base_sent: bool, now: std::time::Instant) -> Option<bool> {
        if !self.enter_pending {
            return None;
        }
        if base_sent {
            self.mode = ScreenMode::Patches;
            self.enter_pending = false;
            self.enter_deadline = None;
            // 基准帧刚发出，保活与带宽窗口都从这里起算。
            self.last_patch_sent = Some(now);
            self.patch_window_start = now;
            self.patch_window_bytes = 0;
            Some(true)
        } else if self.enter_deadline.is_some_and(|deadline| now >= deadline) {
            self.enter_pending = false;
            self.enter_deadline = None;
            self.enter_streak = 0;
            Some(false)
        } else {
            None
        }
    }

    /// 退出补丁模式并清掉进入判据的历史（几何/参考已失效，需要重新积累）。
    fn exit(&mut self) {
        self.mode = ScreenMode::Video;
        self.enter_streak = 0;
        self.enter_pending = false;
        self.enter_deadline = None;
    }
}

/// 当前模式下的采集节拍：补丁模式吃 [`PATCH_MAX_INTERVAL`] 的地板。
fn capture_tick_period(mode: ScreenMode, encode: &EncodeRates) -> Duration {
    match mode {
        ScreenMode::Video => encode.capture_interval(),
        ScreenMode::Patches => encode.capture_interval().max(PATCH_MAX_INTERVAL),
    }
}

/// 显示列表的**比较签名**：按 `id` 排序后的 `(id, width, height)`。
///
/// 只取这三个字段：`name`/`primary` 变化不影响采集（GUI 列表刷新本身需要整份 `DisplayList`，
/// 但那由调用方在签名变化时另行处理）；`id` 排序让枚举顺序波动不算变化——平台返回顺序
/// 不保证稳定，按原顺序比较会每拍都误判。
#[must_use]
fn display_signature(displays: &[DisplayInfo]) -> Vec<(u32, u32, u32)> {
    let mut signature: Vec<(u32, u32, u32)> =
        displays.iter().map(|d| (d.id, d.width, d.height)).collect();
    signature.sort_unstable_by_key(|entry| entry.0);
    signature
}

/// 显示尺寸 → **最终编码尺寸**，与视频路径的计算顺序完全一致。
///
/// 平台采集源会先按 `max_width` 封顶（Windows 的 `capture_gpu_frame` /
/// `capture_hardware` 用 `encode_dimensions(.., 1.0, ..)`），编码循环随后再施加 `rates.scale`。
/// 补丁模式的目标帧缓冲必须落在**同一套**结果上，否则 4K + 非 1.0 缩放时两边会差一档
/// （视频 960×540、补丁 1920×1080），控制端就会因尺寸不符拒绝所有补丁。
///
/// `max_width` 必须是**本会话协商后的**有效上限（见 [`effective_max_encode_width`]）：
/// 默认 1920，硬件 H.264 + 原生分辨率时 3840。
fn encode_target_dims(
    display_width: u32,
    display_height: u32,
    scale: f32,
    max_width: u32,
) -> (u32, u32) {
    let (capped_width, capped_height) =
        encode_dimensions(display_width, display_height, 1.0, max_width);
    encode_dimensions(capped_width, capped_height, scale, max_width)
}

/// 本会话的有效编码宽度上限（像素）。
///
/// **只有硬件 H.264 + 用户显式开启原生分辨率**才放宽到
/// [`NATIVE_MAX_ENCODE_WIDTH`]（3840）；其余一律 [`MAX_ENCODE_WIDTH`]（1920）——
/// 包括「开了选项但对端只有 VP9」：VP9 软件解码 4K 在控制器侧仍是 37–58 ms 一帧。
#[must_use]
fn effective_max_encode_width(codec: CodecId, native_resolution: bool) -> u32 {
    if native_resolution && codec == CodecId::H264 {
        NATIVE_MAX_ENCODE_WIDTH
    } else {
        MAX_ENCODE_WIDTH
    }
}

/// 原生分辨率会话把档位码率上限翻倍（帧率、缩放不动），见 [`native_bitrate_ceiling`]。
fn session_encode_options(options: EncodeOptions, native_active: bool) -> EncodeOptions {
    if native_active {
        EncodeOptions {
            target_bitrate_kbps: native_bitrate_ceiling(options.target_bitrate_kbps),
            ..options
        }
    } else {
        options
    }
}

/// 退出补丁模式：回到全帧视频、强制关键帧，并让采集节拍重新对齐到视频档位。
fn exit_patch_mode(
    mode: &mut PatchMode,
    force_keyframe: &mut bool,
    capture_tick: &mut Pin<Box<Sleep>>,
    interval: Duration,
) {
    mode.exit();
    *force_keyframe = true;
    capture_tick.as_mut().reset(Instant::now() + interval);
}

/// 把一批采集到的脏矩形装成一条 `RegionUpdate`。
///
/// 每个矩形独立压缩：压缩后**确实更小**才用 [`REGION_FLAG_ZSTD`]，否则存原始 NV12（极小块
/// 的 zstd 帧头开销比省下的字节还多）。总字节超过 [`PATCH_MAX_TOTAL_BYTES`] 或矩形数超过
/// [`MAX_REGION_RECTS`] 时返回 `None`：调用方据此回到全帧。
///
/// `capture_ts_us` 是本批采集完成的时刻，原样写进消息供控制器算端到端延迟。
fn build_region_update(
    compressor: &mut zstd::bulk::Compressor<'static>,
    display: u32,
    patches: &CapturedPatches,
    capture_ts_us: u64,
) -> Option<SessionMessage> {
    if patches.rects.len() > MAX_REGION_RECTS {
        return None;
    }
    let mut rects = Vec::with_capacity(patches.rects.len());
    let mut total = 0usize;
    for rect in &patches.rects {
        let compressed = compressor
            .compress(&rect.nv12)
            .ok()
            .filter(|data| data.len() < rect.nv12.len());
        let (flags, data) = match compressed {
            Some(data) => (REGION_FLAG_ZSTD, data),
            None => (0u8, rect.nv12.clone()),
        };
        total = total.checked_add(data.len())?;
        if total > PATCH_MAX_TOTAL_BYTES {
            return None;
        }
        rects.push(RegionRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            flags,
            data,
        });
    }
    Some(SessionMessage::RegionUpdate {
        display,
        width: patches.width,
        height: patches.height,
        rects,
        capture_ts_us,
    })
}

/// 一条 `RegionUpdate` 的压缩后总字节数（带宽窗口统计用）。
fn region_update_bytes(msg: &SessionMessage) -> u64 {
    match msg {
        SessionMessage::RegionUpdate { rects, .. } => rects.iter().fold(0_u64, |acc, rect| {
            acc.saturating_add(u64::try_from(rect.data.len()).unwrap_or(u64::MAX))
        }),
        _ => 0,
    }
}

/// 被控端会话配置。
#[derive(Debug, Clone)]
pub struct ControlledConfig {
    /// 会话 ID。
    pub session_id: String,
    /// 本端（被控端）RustDesk ID。
    pub local_rd_id: String,
    /// 会话模式。
    pub mode: RemoteMode,
    /// 挂起会话表（握手鉴权用）。
    pub pending: Arc<PendingSessions>,
    /// 屏幕编码参数**上限**（会话中可被控制器的 `SetQuality` 覆盖）。
    ///
    /// 自阶段 3 起它是档位**上限**，不是实际生效值——实际值由 QoS 闭环
    /// （[`crate::qos::RateController`]）在上限之下浮动，见 [`crate::screen::EncodeRates`]。
    pub encode: EncodeOptions,
    /// 是否允许**原生分辨率**（4K）编码（默认 `false`）。
    ///
    /// 仅在协商到**硬件 H.264** 时生效：编码宽度上限从 [`MAX_ENCODE_WIDTH`]（1920）
    /// 放宽到 [`NATIVE_MAX_ENCODE_WIDTH`]（3840），档位码率经
    /// [`crate::screen::native_bitrate_ceiling`] 翻倍（封顶 30 Mbps）；VP9 路径不受影响。
    /// 这一项只在**会话开始**时读取，运行中改它只影响下一次会话。
    pub native_resolution: bool,
    /// 在途采集的看门狗时限（见 [`CAPTURE_WATCHDOG`]）。
    ///
    /// 做成字段而非直接读常量，是为了让测试用极短的值验证「采集 panic/挂死之后仍能恢复」，
    /// 生产路径统一传 [`CAPTURE_WATCHDOG`]。
    pub capture_watchdog: Duration,
    /// 静止画面下保活补编的最大间隔（见 [`UNCHANGED_RESEND_INTERVAL`]）。
    ///
    /// 做成字段而非直接读常量，是为了让测试用 `Duration::ZERO` 锁住「保活补编不会被
    /// 去重闸门吃掉」；生产路径统一传 [`UNCHANGED_RESEND_INTERVAL`]。
    pub unchanged_resend: Duration,
    /// 统计数据上报句柄（延迟读数）；无管理端时用 [`SessionStatsReporter::noop`]。
    pub stats: SessionStatsReporter,
    /// 可选的 H.264 编码器工厂（通常由 `crates/win-video` 注入）。
    ///
    /// `None` = 本端只能编码 VP9，协商时一律选 VP9。**与控制器 offer 的交集**才是最终选择：
    /// 只有本字段为 `Some` **且** `CodecOffer.h264 == true` 时才选 H.264。
    pub h264_encoder_factory: Option<Arc<dyn VideoEncoderFactory>>,
    /// 会话结束后是否**自动锁定本机屏幕**。
    ///
    /// 由收尾守卫（[`AutoLockOnExit`]）在会话结束时**实时读取**，故用户在会话进行中打开设置
    /// 也立即生效。纯文件传输会话不创建守卫，因此不触发。
    pub lock_on_disconnect: Arc<AtomicBool>,
}

/// 会话收尾时按用户偏好锁定屏幕。
///
/// 用 `Drop` 而不是在函数末尾显式调用：主循环有多个退出路径（取消/对端关闭/IO 错误），
/// 守卫保证**所有**路径都会走到；它只在握手鉴权成功、且会话真的共享了画面之后才创建。
struct AutoLockOnExit {
    /// 平台注入目标（锁屏是 [`InputSink::lock_screen`]）。
    input: Arc<dyn InputSink>,
    /// 用户偏好（原子位实时读取）。
    enabled: Arc<AtomicBool>,
}

impl Drop for AutoLockOnExit {
    fn drop(&mut self) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        match self.input.lock_screen() {
            Ok(()) => info!("会话结束，已自动锁定屏幕"),
            Err(err) => warn!(error = %err, "会话结束自动锁屏失败"),
        }
    }
}

/// 被控端读半边类型（协商与主循环共用）。
type SessionReader = FramedRead<tokio::io::ReadHalf<BoxDuplex>, SessionCodec>;

/// 协商阶段等待 offer 的结果。
enum CodecOfferOutcome {
    /// 收到了控制器的能力声明。
    Offer {
        /// 控制器是否支持 H.264 解码。
        h264: bool,
        /// 控制器是否支持 VP9 解码。
        vp9: bool,
    },
    /// 等待超时，调用方回退 VP9。
    Timeout,
    /// 对端在协商完成前关闭连接（按非主动结束处理）。
    Closed,
}

/// 纯函数：根据 offer 与本端编码能力选 codec。
///
/// 规则（任务约定）：**本端有 H.264 编码工厂且控制器声明支持 H.264** 才选 H.264，否则 VP9。
/// 超时（`None`）同样回退 VP9。
#[must_use]
fn choose_codec(offer: Option<(bool, bool)>, has_h264_encoder: bool) -> CodecId {
    match offer {
        Some((true, _)) if has_h264_encoder => CodecId::H264,
        // 控制器永远声明 `vp9: true`（VP9 解码是内置实现）。即使它声明 false，VP9 仍是唯一
        // 双方都可能具备的实现，发出去让对端显式报错好过静默卡死。
        _ => CodecId::Vp9,
    }
}

/// 读出控制器的 `CodecOffer`（见 [`CodecOfferOutcome`]）。
///
/// 协商发生在加密装上之后、会话主循环之前。窗口内收到的 `Heartbeat` 要正常应答——控制器的
/// 首个心跳 tick 立即到期，可能在 offer 之后紧跟着到达。其它消息在这个极短窗口里没有意义，
/// 只记日志忽略（真实控制器在握手后**立刻**发 offer，TCP 保序保证它排在帧与输入之前）。
async fn read_codec_offer(
    reader: &mut SessionReader,
    queues: &OutboundQueues,
    cancel: &CancellationToken,
    offer_timeout: Duration,
) -> RemoteResult<CodecOfferOutcome> {
    let deadline = sleep(offer_timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(RemoteError::Cancelled),
            () = &mut deadline => return Ok(CodecOfferOutcome::Timeout),
            incoming = reader.next() => match incoming {
                Some(Ok(frame)) => match frame.message {
                    SessionMessage::CodecOffer { h264, vp9 } => {
                        return Ok(CodecOfferOutcome::Offer { h264, vp9 });
                    }
                    SessionMessage::Heartbeat { .. } => {
                        // t2 = 收到时刻、t3 = 发出时刻（都是本端单调时钟）。两者之间只隔一条
                        // 入队，故差值通常只有微秒级——它反映的是本端处理时延。
                        queues
                            .enqueue_ctrl(
                                cancel,
                                SessionFrame::control(SessionMessage::HeartbeatAck {
                                    t2_us: crate::clock::now_us(),
                                    t3_us: crate::clock::now_us(),
                                }),
                            )
                            .await?;
                    }
                    other => debug!(?other, "编解码协商阶段收到未处理消息，忽略"),
                },
                Some(Err(err)) => return Err(RemoteError::Codec(err.to_string())),
                None => return Ok(CodecOfferOutcome::Closed),
            }
        }
    }
}

/// 组装被控端每秒上报的整机遥测（[`HostStats`]）。
///
/// # 数据来源（**不要**在这里读任何会阻塞的东西）
///
/// - 身份：`std::env::consts::OS`（[`ControlledConfig`] 没有平台字段）+ 编码器快照 +
///   [`ScreenSource::capture_info`]；
/// - 遥测：编码任务每秒写入的 [`EncodeSnapshot`]；
/// - 模式：`patch_mode`（补丁状态机）、`activity`（QoS 的内容档）、`scale`（当前生效缩放）；
/// - C5：主循环累计的采集失败 + `CaptureInfo` 三个**累计计数**的每秒差分。
///
/// 上一份 [`CaptureInfo`] 由 `last_info` 持有，本函数会就地更新它——差分必须恰好每个上报周期
/// 做一次，重复调用会丢事件。
///
/// `scale_permille` 的浮点→整数转换在这里是一处有意的截断：缩放比例恒在
/// `[min_scale, 1.0]`，乘 1000 后远在 `u16` 范围内。
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "缩放比例由 QoS 决定且恒在 [min_scale, 1.0]，乘 1000 后落在 u16 内"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "组装的都是会话主循环已持有的本端状态（采集源/快照/速率/显示器/累计计数），\
              打包成结构体反而要在每秒心跳拍多构造一次"
)]
fn build_host_stats(
    screen: &Arc<dyn ScreenSource>,
    snapshot: &std::sync::Mutex<Option<EncodeSnapshot>>,
    encode: &EncodeRates,
    rate: &RateController,
    displays: &[DisplayInfo],
    display_id: u32,
    patch_mode: bool,
    native_resolution: bool,
    capture_failures: u32,
    last_info: &mut Option<CaptureInfo>,
) -> HostStats {
    let snap = snapshot
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_default();
    let capture = screen.capture_info();
    // 三个累计计数 → 每秒增量：首份快照（无上一份）没有基线，给 0 而不是「开机以来的全部」。
    let (capture_fallbacks, capture_open_fallbacks, no_new_frame) =
        last_info.replace(capture.clone()).map_or((0, 0, 0), |prev| {
            (
                u32::try_from(capture.fallbacks.saturating_sub(prev.fallbacks))
                    .unwrap_or(u32::MAX),
                u32::try_from(capture.open_fallbacks.saturating_sub(prev.open_fallbacks))
                    .unwrap_or(u32::MAX),
                u32::try_from(capture.no_new_frame.saturating_sub(prev.no_new_frame))
                    .unwrap_or(u32::MAX),
            )
        });
    let (display_width, display_height) = displays
        .iter()
        .find(|d| d.id == display_id)
        .map_or((0, 0), |d| (d.width, d.height));
    let (encode_width, encode_height) = snap.encode_dims.unwrap_or((0, 0));
    let activity = match rate.activity() {
        crate::qos::Activity::Ui => HostActivity::Ui,
        crate::qos::Activity::Mixed => HostActivity::Mixed,
        crate::qos::Activity::Active => HostActivity::Active,
    };
    HostStats {
        platform: std::env::consts::OS.to_owned(),
        encoder_backend: snap.encoder_info.backend.clone(),
        encoder_name: snap.encoder_info.name.clone(),
        encoder_adapter: snap.encoder_info.adapter.clone(),
        capture_backend: capture.backend.clone(),
        capture_adapter: capture.adapter.clone(),
        gpu_capture: capture.gpu_capture,
        gpu_encode: snap.encoder_info.gpu_input,
        gpu_disabled: capture.gpu_disabled,
        gpu_disabled_reason: capture.gpu_disabled_reason.clone(),
        native_resolution,
        encode_width,
        encode_height,
        display_width,
        display_height,
        capture_us: snap.capture_us,
        encode_ms: snap.encode_ms,
        fps: snap.fps,
        bitrate_kbps: snap.bitrate_kbps,
        sent_kbps: snap.sent_kbps,
        skipped: snap.skipped,
        keyframes: snap.keyframes,
        idr_kbps: snap.idr_kbps,
        idr_bytes: snap.idr_bytes,
        p_kbps: snap.p_kbps,
        changed_permille: snap.changed_permille,
        scale_permille: (encode.scale * 1000.0).round().clamp(0.0, f32::from(u16::MAX)) as u16,
        activity,
        patch_mode,
        out_queue_depth: snap.out_queue_depth,
        out_queue_skips: snap.out_queue_skips,
        encoder_rebuilds: snap.encoder_rebuilds,
        forced_idrs: snap.forced_idrs,
        capture_failures,
        capture_fallbacks,
        capture_open_fallbacks,
        no_new_frame,
    }
}

/// 跑一次被控端会话，直到取消、对端关闭或出错。
///
/// # Errors
/// 握手被拒（版本不符、鉴权失败）、编解码/IO 失败时返回 [`RemoteError`]；正常结束返回
/// `Ok(`[`SessionEnd`]`)`，由调用方据此判定是否需要自动重连。
#[expect(
    clippy::too_many_lines,
    reason = "握手 + 采集 + 注入 + 心跳的单会话循环，拆开反而丢失控制流全貌"
)]
pub async fn run_controlled(
    duplex: BoxDuplex,
    config: ControlledConfig,
    screen: Arc<dyn ScreenSource>,
    input: Arc<dyn InputSink>,
    clipboard: Option<Arc<dyn Clipboard>>,
    mut transfer: TransferHub,
    cancel: CancellationToken,
) -> RemoteResult<SessionEnd> {
    let (read_half, write_half) = tokio::io::split(duplex);
    let mut reader = FramedRead::new(read_half, SessionCodec::new());
    let mut writer = FramedWrite::new(write_half, SessionCodec::new());

    // 会持续更新（显示器热插拔/分辨率变化），故必须是 `mut`。
    let mut displays = screen.displays();

    // 1) 读 Hello 并鉴权（明文）。
    let hello = match timeout(HANDSHAKE_TIMEOUT, reader.next()).await {
        Ok(Some(Ok(frame))) => frame,
        Ok(Some(Err(err))) => return Err(RemoteError::Codec(err.to_string())),
        Ok(None) => return Err(RemoteError::Handshake("对端在握手前关闭连接".to_owned())),
        Err(_) => return Err(RemoteError::Handshake("等待 Hello 超时".to_owned())),
    };
    let SessionMessage::Hello {
        session_id,
        proto_version,
        controller_rd_id,
        nonce,
        hmac,
        x25519_pub,
    } = hello.message
    else {
        return Err(RemoteError::Handshake(format!("期望 Hello，实得 {:?}", hello.message)));
    };

    // 版本与 session 一致性检查；不通过则明文拒绝。
    if proto_version != SESSION_PROTO_VERSION || session_id != config.session_id {
        let reason = format!("握手参数不匹配（proto={proto_version}, session={session_id}）");
        reject_handshake(&mut writer, &reason).await;
        return Err(RemoteError::Handshake(reason));
    }

    // 鉴权：常数时间比对 HMAC；失败/过期/重放一律拒绝并记原因。
    // 拿住返回的 `PendingSession`——它的 token 还要进 KDF 做密钥交换认证。
    let pending = match config
        .pending
        .authorize(&session_id, &controller_rd_id, &nonce, &x25519_pub, &hmac)
    {
        Ok(pending) => pending,
        Err(err) => {
            let reason = format!("鉴权失败: {err}");
            warn!(session = %session_id, %reason, "被控端拒绝会话");
            reject_handshake(&mut writer, &reason).await;
            return Err(RemoteError::Auth(reason));
        }
    };

    // 2) 生成临时密钥并回 HelloAck（明文），随后装上加密。
    let keypair = EphemeralKeyPair::generate();
    let responder_pub = keypair.public_bytes().to_vec();
    // 用 token 对自己的临时公钥做 MAC：控制器据此拒绝被中继方替换过的 `x25519_pub`。
    let hmac_ack = ack_mac(&pending.token, &config.session_id, &responder_pub, &x25519_pub);
    let ack = SessionFrame::control(SessionMessage::HelloAck {
        accepted: true,
        reason: None,
        displays: displays.clone(),
        x25519_pub: responder_pub,
        hmac_ack,
    });
    writer.send(ack).await?;

    let shared = keypair.diffie_hellman(&x25519_pub)?;
    let initiator_pub: [u8; 32] = x25519_pub
        .as_slice()
        .try_into()
        .map_err(|_| RemoteError::Auth("Hello 的 X25519 公钥长度非法".to_owned()))?;
    let keys = derive_session_keys(
        &pending.token,
        &shared,
        &config.session_id,
        &initiator_pub,
        &keypair.public_bytes(),
        false,
    );
    reader.decoder_mut().set_cipher(Cipher::new(keys.recv)?);
    writer.encoder_mut().set_cipher(Cipher::new(keys.send)?);
    debug!(session = %config.session_id, "被控端会话已加密");

    // 出站写任务：独占写半边。会话 `select!` 循环只把帧投进分档队列，**不再在分支里 await 写**
    // ——否则写侧被背压卡住时（对端停读、中继停走），输入/心跳分支完全得不到轮询，形成队头阻塞。
    // 协商阶段的 `HeartbeatAck` / `CodecSelect` 也走这些队列，故先把它起来。
    let (mut queues, receivers) = OutboundQueues::new();
    let writer_task = tokio::spawn(writer_loop(writer, receivers, cancel.clone()));

    // 2.5) 编解码协商：**必须先拿到 `CodecOffer` 才能开始采集/编码**——否则首帧可能用了控制器
    // 解不了的 codec，只能靠后来请求关键帧重来。控制器在握手后立刻发它；超时回退 VP9（
    // 见 [`CODEC_OFFER_TIMEOUT`]），对端在协商前关连接按正常结束处理。
    let outcome = match read_codec_offer(&mut reader, &queues, &cancel, CODEC_OFFER_TIMEOUT).await {
        Ok(outcome) => outcome,
        Err(RemoteError::Cancelled) => {
            return Ok(SessionEnd {
                reason: "用户中断".to_owned(),
                deliberate: true,
            });
        }
        Err(err) => return Err(err),
    };
    let offer = match outcome {
        CodecOfferOutcome::Offer { h264, vp9 } => Some((h264, vp9)),
        CodecOfferOutcome::Timeout => {
            warn!(
                timeout = ?CODEC_OFFER_TIMEOUT,
                "等待 CodecOffer 超时，回退 VP9"
            );
            None
        }
        CodecOfferOutcome::Closed => {
            return Ok(SessionEnd {
                reason: "对端在编解码协商前关闭连接".to_owned(),
                deliberate: false,
            });
        }
    };
    let codec = choose_codec(offer, config.h264_encoder_factory.is_some());
    let encoder_factory: Arc<dyn VideoEncoderFactory> = if codec == CodecId::H264 {
        // `choose_codec` 只在工厂存在时返回 H264；这里仍是防御性检查，不 panic。
        config.h264_encoder_factory.clone().ok_or_else(|| {
            RemoteError::Platform("协商选择了 H.264，但本端没有编码器工厂".to_owned())
        })?
    } else {
        vp9_encoder_factory()
    };
    queues
        .enqueue_ctrl(&cancel, SessionFrame::control(SessionMessage::CodecSelect { codec }))
        .await?;
    info!("编解码协商完成（被控端） codec={codec:?}");

    // 本会话的**有效编码宽度上限**：只有硬件 H.264 + 用户显式开启原生分辨率才放宽到 3840
    // （见 [`effective_max_encode_width`]）。采集侧也按它预缩放——采集实现必须与编码循环用
    // 同一个值，否则补丁/全帧两条路径的尺寸会差一档。
    let max_encode_width = effective_max_encode_width(codec, config.native_resolution);
    let native_active = max_encode_width > MAX_ENCODE_WIDTH;
    screen.set_max_encode_width(max_encode_width);
    info!("编码宽度上限（被控端） max_width={max_encode_width} native={native_active}");

    // 编码任务：独占编码器，从有界队列取原始帧、去重、编码，再把 `VideoPacket` 投给出站队列。
    // **绝不丢弃已编码的帧**（见模块文档的流水线说明）：输出队列满时跳过的是**编码之前**的这一帧。
    //
    // `sent_kbps` 是编码任务每秒写入的**实际出码速率**（按窗口字节数算），由 QoS 闭环与控制器
    // 回报的 `recv_kbps` 相比得出投递率。它只在进程内传递，**不进** `QosFeedback`（线上字段不变）。
    let sent_kbps = Arc::new(AtomicU32::new(0));
    // 补丁模式的「基准全帧已写出」信号：由编码任务在关键帧**投进出站队列之后**置位，
    // 会话循环只在看到它之后才切补丁模式（顺序不变量，见模块文档）。
    let base_frame_sent = Arc::new(AtomicBool::new(false));
    // 编码任务每秒写入的完整快照（std `Mutex`，每秒只读/写一次）。主循环据此组装
    // `HostStats`——编码侧的真实读数（耗时、fps、出码、队列深度、重建/强制 IDR 次数）只在
    // 编码任务里，而 `SessionStats::Encode` 那条通道不带这些字段。
    let encode_snapshot: Arc<std::sync::Mutex<Option<EncodeSnapshot>>> =
        Arc::new(std::sync::Mutex::new(None));
    let (encode_tx, encode_rx) = mpsc::channel::<EncodeJob>(1);
    let encode_task = tokio::spawn(encode_loop(
        encode_rx,
        queues.screen_sender(),
        cancel.clone(),
        config.stats.clone(),
        codec,
        encoder_factory,
        config.unchanged_resend,
        Arc::clone(&sent_kbps),
        Arc::clone(&base_frame_sent),
        max_encode_width,
        Arc::clone(&encode_snapshot),
    ));

    let capture = config.mode != RemoteMode::FileTransfer;
    let inject = config.mode == RemoteMode::Control;
    // 会话收尾自动锁屏：只在本会话**真的共享了画面**时创建。位置在握手鉴权与编解码协商
    // 之后，因此版本不匹配/鉴权失败的连接绝不会锁屏；纯文件传输会话也不锁。
    let _auto_lock = capture.then(|| AutoLockOnExit {
        input: Arc::clone(&input),
        enabled: Arc::clone(&config.lock_on_disconnect),
    });
    // GPU 采集只在**协商到 H.264** 且平台采集源支持时启用。计算一次即可：codec 在
    // 会话内不变，平台能力也不会在会话中翻转。H.264 编码器在 GPU 输入不可用时由
    // 适配器自行回退 CPU（`RawFrame::gpu == None` 走原来的 rgba 路径）。
    let gpu_capture = codec == CodecId::H264 && screen.supports_gpu_capture();
    // 补丁模式只在 **H.264 + 平台采集源支持** 时可用：VP9 的解码输出是 RGBA，控制器需要
    // 另一套持久帧缓冲（见 [`ScreenSource::supports_patches`]）。压缩器跨拍复用（level 1，
    // 单块几 KB），构造失败就整体禁用它。
    let mut patch_compressor = if codec == CodecId::H264 && screen.supports_patches() {
        match zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL) {
            Ok(compressor) => Some(compressor),
            Err(err) => {
                warn!(error = %err, "zstd 压缩器创建失败，禁用脏矩形补丁模式");
                None
            }
        }
    } else {
        None
    };
    let patch_capable = patch_compressor.is_some();
    // 编码参数自阶段 3 起分两层：`config.encode` 是档位**上限**，`encode` 是闭环**当前生效值**
    // （起步 = 上限）。`SetQuality` 换上限，`QosFeedback` 动生效值。
    //
    // 原生分辨率会话把上限**翻倍**（封顶 30 Mbps）：4K 桌面文字在 10 Mbps 下明显不够。
    // 只改上限——QoS 仍可在其下下调，且不触碰帧率/缩放。
    let mut rate = RateController::new(session_encode_options(config.encode, native_active));
    let mut encode = rate.current();
    let mut display_id = displays
        .iter()
        .find(|d| d.primary)
        .or_else(|| displays.first())
        .map_or(0, |d| d.id);

    // 3) 主循环。
    // 起点取 `now` 而非 `now + HEARTBEAT_INTERVAL`：**首个 tick 因此立即到期**，会话一建立就发
    // 一次心跳，被控端横幅开屏即有延迟读数（与控制器侧同款手法，见 `controller.rs`）。
    let mut heartbeat = interval_at(Instant::now(), HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 出站文件块：只在有在途传输时启用。
    let mut file_tick = interval_at(Instant::now(), CHUNK_PAUSE);
    file_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 显示器热插拔/分辨率变化复查。首拍延后一个周期：会话开始时刚刚枚举过一次。
    let mut display_tick = interval_at(
        Instant::now() + DISPLAY_RECHECK_INTERVAL,
        DISPLAY_RECHECK_INTERVAL,
    );
    display_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // 采集节拍用「可重置的 sleep」而不是固定 interval：画质档位会改 `max_fps`，档位一变
    // 节拍要立刻跟着变（`interval_at` 的周期改不了，只能重建）。
    let mut capture_tick = Box::pin(sleep(encode.capture_interval()));
    // 在途采集的结果经通道回传。`capture_deadline` 非空即代表有一次采集在途，保证同一时刻最多
    // 一次采集（并发两次会把阻塞线程池堆满）。循环自己保留 `capture_tx`，故 `recv()` 永不返回
    // `None`。
    let (capture_tx, mut capture_rx) = mpsc::channel::<CaptureOutcome>(1);
    // 在途采集的截止时刻（看门狗，见 [`CAPTURE_WATCHDOG`]）；`None` = 当前没有采集在途。
    let mut capture_deadline: Option<Instant> = None;
    // 采集代次：被看门狗判定为「已丢失」的那次采集，其阻塞任务仍可能**稍后**才把结果投进通道
    // （panic 被吞掉后由后续代码路径、或挂住之后终于返回）。带上代次才能把这种过期结果丢掉，
    // 否则会喂给编码器一帧陈旧画面（虽然无害，但白费一次编码）。
    let mut capture_generation: u64 = 0;
    // 连续超时次数：见 [`MAX_CAPTURE_TIMEOUTS`]。
    let mut capture_timeouts: u32 = 0;
    // 首帧（或切显示器/切档位/收到 RequestKeyframe 之后）必须先发关键帧：解码侧要有可重建的参考。
    let mut force_keyframe = true;
    // 脏矩形补丁模式的状态机（见模块文档）。`patch_capable == false` 时它永远停在 `Video`。
    let mut screen_mode = PatchMode::new();
    // 最近一帧采集报告的**变化面积**（千分比），供 `QosFeedback` 里的内容自适应使用
    // （`RateController::observe_activity`）。初值 1000 = 面积未知/满帧，保守地不收紧上限。
    let mut last_changed_permille: u16 = 1000;
    // C5：采集调用返回错误（`CapturePayload::Frame(None)`）的每秒增量，由主循环累计、
    // 心跳拍清零。
    let mut capture_failures: u32 = 0;
    // `CaptureInfo` 的三个计数是**累计值**，主循环保存上一份求差后才是每秒增量。
    let mut last_capture_info: Option<CaptureInfo> = None;
    // 控制器显式选择的帧率上限（30 / 60）。`None` = 尚未下发，沿用画质档位默认。
    // 记住它是为了「换画质档位不覆盖用户选择」：`SetQuality` 换完档位后要重新应用它。
    let mut selected_fps: Option<u8> = None;
    // 上一次 QoS 日志的签名（动作 + 码率 + 帧率 + 缩放）。稳定的 Hold 不逐秒刷屏；只有动作
    // 或生效值变化才记一条 info（GUI 日志层会丢 tracing 字段，故全部内联进消息文本）。
    // **码率必须进签名**：降级阶段通常连续多次 Decrease、只有码率在变，漏掉它就会整段降码率
    // 过程只剩第一条日志。缩放用 `to_bits()` 比较：判「是否变化」只需逐位相等。
    let mut last_qos_log: Option<(QosAction, u32, u8, u32)> = None;
    // RTT 样本：发 `Heartbeat` 时把时刻压入队列，收到 `HeartbeatAck` 时从队首弹出配对。
    // **复用既有心跳往返**，不新增数据面变体（见 `HEARTBEAT_INTERVAL`）。
    //
    // 用队列而不是单个 `Option`：1s 探测在高 RTT 链路上会出现多个心跳同时在途，单个槽位会把
    // 迟到的 ack 与更新的心跳配错对，算出一个假的低 RTT。队列按 FIFO 配对，最坏只是样本旧
    // 一个周期；上限 8 防止对端完全不应答时无限增长。
    let mut heartbeat_sent: VecDeque<Instant> = VecDeque::new();
    // 最近一次收到 `HeartbeatAck` 的时刻：仅用于超时告警（不上线、不进协议）。
    let mut last_ack_at: Option<Instant> = None;
    let mut reason = String::from("会话结束");
    let result: RemoteResult<SessionEnd> = loop {
        tokio::select! {
            () = cancel.cancelled() => {
                "用户中断".clone_into(&mut reason);
                break Ok(SessionEnd { reason: reason.clone(), deliberate: true });
            }
            _ = heartbeat.tick() => {
                // 存活检测：队首是最老的未应答心跳。超过 `LIVENESS_TIMEOUT` 仍没配对，说明链路
                // 已经断了（中继停转发但 TCP 未断时对端不会给我们任何错误，会话会「永久冻结」
                // 而 UI 仍显示在线）。必须在容量上限的 `pop_front` 之前判（理由同控制器侧）。
                // 队列空（还没发过心跳）时不判。
                if heartbeat_sent.front().is_some_and(|sent| sent.elapsed() >= LIVENESS_TIMEOUT) {
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
                heartbeat_sent.push_back(Instant::now());
                if let Err(err) = queues
                    .enqueue_ctrl(
                        &cancel,
                        SessionFrame::control(SessionMessage::Heartbeat {
                            t1_us: crate::clock::now_us(),
                        }),
                    )
                    .await
                {
                    break Err(err);
                }
                // 同一拍（1s）向控制器上报整机遥测。这里也把同一份数据喂给本机 UI
                // （被控端横幅/状态页要显示「采集 x ms · 编码 y ms」）。
                //
                // 读的都是共享槽/原子/本端变量，**不做任何阻塞调用**：`screen.capture_info()`
                // 由平台实现保证常数时间（只读几个 `AtomicU64` 与已缓存的适配器名）。
                let host = build_host_stats(
                    &screen,
                    &encode_snapshot,
                    &encode,
                    &rate,
                    &displays,
                    display_id,
                    screen_mode.is_patches(),
                    native_active,
                    capture_failures,
                    &mut last_capture_info,
                );
                config.stats.report(SessionStats::Host(Box::new(host.clone())));
                if let Err(err) = queues
                    .enqueue_ctrl(&cancel, SessionFrame::control(SessionMessage::HostStats(host)))
                    .await
                {
                    break Err(err);
                }
                // C5 的每秒增量上报完即清零，下一窗口重新累计。
                capture_failures = 0;
            }
            outcome = capture_rx.recv(), if capture_deadline.is_some() => {
                match outcome {
                    // 过期结果：看门狗已判定该次采集丢失。丢掉它（在途的是另一次更晚的采集，
                    // **不能**清 `capture_deadline`）。
                    Some(CaptureOutcome { generation, .. }) if generation != capture_generation => {
                        debug!(generation, "丢弃过期的采集结果");
                    }
                    Some(CaptureOutcome { payload, capture_us, capture_ts_us, .. }) => {
                        capture_deadline = None;
                        capture_timeouts = 0;
                        match payload {
                            CapturePayload::Frame(frame) => {
                                if let Some(frame) = frame {
                                    debug!(capture_us, "采集一帧");
                                    last_changed_permille = frame.changed_permille;
                                    // 进入判据只喂**视频**采集：连续多次低活跃才值得付补丁模式的
                                    // 固定开销。命中时本帧必须充当**基准 IDR**（见模块文档的顺序约束）。
                                    //
                                    // 必须排除已在补丁模式的情况：切换瞬间可能还有一次在途的
                                    // 视频采集结果，不排除的话它会把计数攒到阈值、在补丁模式里
                                    // 又发一次强制 IDR。
                                    if patch_capable
                                        && !screen_mode.is_patches()
                                        && screen_mode.observe_video(
                                            frame.changed_permille,
                                            std::time::Instant::now(),
                                        )
                                    {
                                        force_keyframe = true;
                                        base_frame_sent.store(false, Ordering::Relaxed);
                                    }
                                    let job = EncodeJob {
                                        display: display_id,
                                        rates: encode,
                                        force_keyframe,
                                        capture_us,
                                        capture_ts_us,
                                        frame,
                                    };
                                    // 有界(1) 队列满 = 编码/发送侧还没消化完上一帧 → **编码之前**跳过这一
                                    // 帧（等价降帧率）。关键帧请求保留到真正投递成功的那一次。
                                    match encode_tx.try_send(job) {
                                        Ok(()) => force_keyframe = false,
                                        Err(mpsc::error::TrySendError::Full(_)) => {
                                            debug!("编码队列已满，跳过本轮采集（编码前丢帧，不破坏参考链）");
                                        }
                                        Err(mpsc::error::TrySendError::Closed(_)) => break Err(link_closed()),
                                    }
                                } else {
                                    // C5：采集调用返回错误（`run_capture` 已记日志）。只计数——
                                    // 会话继续，下一拍照常采集。
                                    capture_failures = capture_failures.saturating_add(1);
                                }
                            }
                            CapturePayload::Patches(patches) => {
                                if !screen_mode.is_patches() {
                                    // 切模式/放弃之后才回来的在途补丁采集：丢掉即可。
                                    // `capture_deadline` 已在上面清掉，下一拍照常采集。
                                    debug!(capture_us, "丢弃补丁模式之外的补丁采集结果");
                                } else if let Some(patches) = patches {
                                    // 目标帧缓冲尺寸必须与当前编码尺寸一致：显示器分辨率或缩放变了
                                    // （`SetQuality` / QoS 降分辨率）时控制器那块持久缓冲已作废。
                                    let (want_w, want_h) = displays
                                        .iter()
                                        .find(|d| d.id == display_id)
                                        .map_or((0, 0), |d| {
                                            encode_target_dims(
                                                d.width,
                                                d.height,
                                                encode.scale,
                                                max_encode_width,
                                            )
                                        });
                                    if patches.width != want_w || patches.height != want_h {
                                        warn!(
                                            got_w = patches.width,
                                            got_h = patches.height,
                                            want_w,
                                            want_h,
                                            "补丁尺寸与编码尺寸不符，回到全帧"
                                        );
                                        exit_patch_mode(
                                            &mut screen_mode,
                                            &mut force_keyframe,
                                            &mut capture_tick,
                                            encode.capture_interval(),
                                        );
                                    } else if patches.changed_permille > PATCH_EXIT_PERMILLE {
                                        debug!(
                                            changed_permille = patches.changed_permille,
                                            "画面活跃度回升，退出补丁模式"
                                        );
                                        exit_patch_mode(
                                            &mut screen_mode,
                                            &mut force_keyframe,
                                            &mut capture_tick,
                                            encode.capture_interval(),
                                        );
                                    } else if patches.rects.len() > MAX_REGION_RECTS {
                                        warn!(rects = patches.rects.len(), "脏矩形过多，退出补丁模式");
                                        exit_patch_mode(
                                            &mut screen_mode,
                                            &mut force_keyframe,
                                            &mut capture_tick,
                                            encode.capture_interval(),
                                        );
                                    } else {
                                        last_changed_permille = patches.changed_permille;
                                        // 空矩形 = 本拍没有变化：只在保活间隔到点时发一条空
                                        // `RegionUpdate`（控制器侧的「长时间无帧」看门狗靠它）。
                                        let keepalive_due = screen_mode
                                            .last_patch_sent
                                            .is_none_or(|at| at.elapsed() >= PATCH_KEEPALIVE);
                                        if patches.rects.is_empty() && !keepalive_due {
                                            debug!(capture_us, "补丁无变化且未到保活间隔，跳过");
                                        } else {
                                            let built = patch_compressor.as_mut().and_then(|compressor| {
                                                build_region_update(
                                                    compressor,
                                                    display_id,
                                                    &patches,
                                                    capture_ts_us,
                                                )
                                            });
                                            if let Some(msg) = built {
                                                // 带宽兜底：补丁模式下 QoS 闭环暂停，若一个窗口的
                                                // 补丁出码超过「有效码率 × 2」就回全帧——H.264 整帧
                                                // 在同样字节下至少是正常码流，继续发补丁只会更糟。
                                                let bytes = region_update_bytes(&msg);
                                                let budget_kbps = rate
                                                    .current()
                                                    .bitrate_kbps
                                                    .max(PATCH_MIN_BUDGET_KBPS)
                                                    .saturating_mul(PATCH_BANDWIDTH_FACTOR)
                                                    .min(PATCH_MAX_BUDGET_KBPS);
                                                if screen_mode.admit_patch_bytes(
                                                    bytes,
                                                    std::time::Instant::now(),
                                                    budget_kbps,
                                                ) {
                                                    debug!(
                                                        capture_us,
                                                        rects = patches.rects.len(),
                                                        bytes,
                                                        "发送脏矩形补丁"
                                                    );
                                                    // 补丁很小，但**绝不丢弃**（丢一个会让画面永久缺
                                                    // 一块），故走画面队列的可取消等待。
                                                    if let Err(err) = queues
                                                        .enqueue_screen(&cancel, SessionFrame::screen(msg))
                                                        .await
                                                    {
                                                        break Err(err);
                                                    }
                                                    screen_mode.last_patch_sent =
                                                        Some(std::time::Instant::now());
                                                } else {
                                                    warn!(
                                                        bytes,
                                                        budget_kbps,
                                                        "补丁出码超过窗口预算，退出补丁模式"
                                                    );
                                                    exit_patch_mode(
                                                        &mut screen_mode,
                                                        &mut force_keyframe,
                                                        &mut capture_tick,
                                                        encode.capture_interval(),
                                                    );
                                                }
                                            } else {
                                                warn!(
                                                    capture_us,
                                                    rects = patches.rects.len(),
                                                    "补丁压缩后超出字节预算，退出补丁模式"
                                                );
                                                exit_patch_mode(
                                                    &mut screen_mode,
                                                    &mut force_keyframe,
                                                    &mut capture_tick,
                                                    encode.capture_interval(),
                                                );
                                            }
                                        }
                                    }
                                } else {
                                    // 补丁采集返回 None：本拍不适合补丁（活跃度/矩形数超限、
                                    // 检测不可用……），立刻回全帧并强制 IDR。
                                    debug!("补丁采集未产出补丁，返回全帧视频");
                                    exit_patch_mode(
                                        &mut screen_mode,
                                        &mut force_keyframe,
                                        &mut capture_tick,
                                        encode.capture_interval(),
                                    );
                                }
                            }
                        }
                    }
                    // 防御性兜底（`capture_tx` 常驻，理论上不会 `None`）：解除在途状态。
                    None => capture_deadline = None,
                }
            }
            // 看门狗：采集 panic/挂死时结果永不回传，见 [`CAPTURE_WATCHDOG`]。到点即判定该次采集
            // 丢失：放弃它并继续下一拍（**不必**强制关键帧——丢的是**未编码**的采集，编码器的参考
            // 链完好无损）；连续多次才终止会话（见 [`MAX_CAPTURE_TIMEOUTS`]）。
            () = sleep(capture_deadline.unwrap_or_else(Instant::now).saturating_duration_since(Instant::now())), if capture_deadline.is_some() => {
                capture_deadline = None;
                capture_timeouts = capture_timeouts.saturating_add(1);
                if capture_timeouts >= MAX_CAPTURE_TIMEOUTS {
                    warn!(count = capture_timeouts, "采集连续超时，判定采集侧已失效，结束会话");
                    break Err(RemoteError::Platform(format!(
                        "屏幕采集连续 {capture_timeouts} 次超时（每次超过 {CAPTURE_WATCHDOG:?}）"
                    )));
                }
                warn!(count = capture_timeouts, "在途采集超过看门狗时限，判定已丢失");
            }
            () = &mut capture_tick, if capture => {
                // 进入判据：基准关键帧**真正写出**之后才切模式（顺序不变量，见模块文档）。
                match screen_mode.poll_entry(
                    base_frame_sent.load(Ordering::Relaxed),
                    std::time::Instant::now(),
                ) {
                    Some(true) => info!("进入脏矩形补丁模式"),
                    Some(false) => debug!("等待基准关键帧超时，放弃进入补丁模式"),
                    None => {}
                }
                capture_tick.as_mut().reset(
                    Instant::now() + capture_tick_period(screen_mode.mode, &encode),
                );
                // 串行采集：上一次还没回来就跳过本轮，避免把阻塞线程池堆满。
                if capture_deadline.is_none() {
                    capture_generation = capture_generation.wrapping_add(1);
                    capture_deadline = Some(Instant::now() + config.capture_watchdog);
                    let screen = Arc::clone(&screen);
                    let display_now = display_id;
                    let generation_now = capture_generation;
                    let request = match screen_mode.mode {
                        ScreenMode::Patches => CaptureRequest::Patches {
                            scale: encode.scale,
                            max_width: max_encode_width,
                        },
                        ScreenMode::Video => CaptureRequest::Frame { gpu: gpu_capture },
                    };
                    let tx = capture_tx.clone();
                    // 采集是阻塞调用（xcap 的 portal 路径可能数秒），整体挪出 async worker。
                    tokio::task::spawn_blocking(move || {
                        run_capture(&*screen, display_now, generation_now, request, &tx);
                    });
                }
            }
            // 显示器热插拔 / 分辨率变化复查。
            //
            // 会话开始时只枚举了一次，之后不复查会有两个后果：控制器的显示器列表永远陈旧；
            // 当前采集的显示器被拔掉后 `capture` 只 `warn!` 后继续，画面永久冻结而会话
            // 看起来还活着。5 秒的延迟对热插拔是可接受的。
            //
            // 枚举直接内联：Windows 的 `EnumDisplayMonitors` 是毫秒级，远小于一次采集（视
            // 平台可能几十毫秒到数秒），不值得为它再引一条 `spawn_blocking` 通道与在途标志
            // ——那反而让分支状态机多一个可出错点。
            _ = display_tick.tick() => {
                let fresh = screen.displays();
                if fresh.is_empty() {
                    // 空列表一律视为**一次枚举失败**（拔掉唯一显示器、锁屏/切桌面瞬间等）：
                    // 不应用。否则会把控制器列表清空、并把采集目标改成一块不存在的屏。
                    debug!("显示器枚举返回空列表，保留旧列表");
                } else if display_signature(&fresh) != display_signature(&displays) {
                    let old_current = displays.iter().find(|d| d.id == display_id).cloned();
                    let count = fresh.len();
                    displays = fresh;
                    // 控制队列（可取消地等空位、绝不丢）：GUI 据此刷新显示器下拉框。
                    if let Err(err) = queues
                        .enqueue_ctrl(
                            &cancel,
                            SessionFrame::control(SessionMessage::DisplayList(displays.clone())),
                        )
                        .await
                    {
                        break Err(err);
                    }
                    let new_current = displays.iter().find(|d| d.id == display_id);
                    match (old_current, new_current) {
                        // 当前采集的显示器被拔掉：改选 primary（没有则第一个），并立刻出帧。
                        (_, None) => {
                            if let Some(target) = displays
                                .iter()
                                .find(|d| d.primary)
                                .or_else(|| displays.first())
                            {
                                let new_id = target.id;
                                info!(
                                    "当前显示器 {display_id} 已消失，切换到显示器 {new_id}（共 {count} 台）"
                                );
                                display_id = new_id;
                            }
                            // 几何/来源都变了：补丁模式的持久缓冲与编码器尺寸都要重建。
                            screen_mode.exit();
                            force_keyframe = true;
                            capture_tick.as_mut().reset(Instant::now());
                        }
                        // 当前显示器还在但分辨率变了：同上（控制器那块合成帧已作废）。
                        (Some(old), Some(new))
                            if (old.width, old.height) != (new.width, new.height) =>
                        {
                            info!(
                                "显示器 {display_id} 分辨率变化 {}x{} -> {}x{}，重建编码器并强制关键帧",
                                old.width, old.height, new.width, new.height
                            );
                            screen_mode.exit();
                            force_keyframe = true;
                            capture_tick.as_mut().reset(Instant::now());
                        }
                        // 其它变化（拔插了别的显示器、primary 变了）：列表已刷新，当前采集目标
                        // 不受影响，参考链与几何都没变，**不**插 IDR。
                        (_, Some(_)) => {
                            info!("显示器列表变化（当前显示器 {display_id} 未变，共 {count} 台）");
                        }
                    }
                }
            }
            // 文件块生产**永不阻塞主循环**：先补投暂存区，再在 file 队列确实有空位时才读下一块。
            // `try_enqueue_file` 只做 `try_send`，满了就留在暂存区等下一拍；绝不 `.await` 空位。
            _ = file_tick.tick(), if transfer.has_outgoing() || queues.has_staged_file() => {
                if let Err(err) = queues.drain_file() {
                    break Err(err);
                }
                if queues.file_has_room() && transfer.has_outgoing() {
                    match transfer.next_outbound().await {
                        Ok(Some(msg)) => {
                            if let Err(err) = queues.dispatch_frame(&cancel, SessionFrame::file(msg)).await {
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
                        SessionMessage::Input(event) => {
                            // `InputSink::apply` 是 enigo 注入，快操作（微秒级），留在 async worker
                            // 上即可——为它付一次 `spawn_blocking` 的调度开销并不划算。
                            if inject {
                                if let Err(err) = input.apply(event) {
                                    warn!(error = %err, "输入注入失败");
                                }
                            }
                        }
                        SessionMessage::LockScreen => {
                            // 仅完全控制模式：仅观看/仅文件传输不该改变远端机器状态。
                            if inject {
                                match input.lock_screen() {
                                    Ok(()) => info!("控制端请求锁定屏幕，已执行"),
                                    Err(err) => warn!(error = %err, "控制端请求锁定屏幕失败"),
                                }
                            } else {
                                debug!("非完全控制模式，忽略锁屏请求");
                            }
                        }
                        SessionMessage::SelectDisplay { display: target } => {
                            // 切换采集目标后参考画面作废：分辨率/内容都变了，必须重发关键帧。
                            if displays.iter().any(|d| d.id == target) {
                                display_id = target;
                                // 几何变了：补丁模式（以及正在等的进入）一律作废，控制器那块
                                // 持久帧缓冲的尺寸已经不对了。
                                screen_mode.exit();
                                force_keyframe = true;
                                capture_tick.as_mut().reset(Instant::now() + encode.capture_interval());
                            } else {
                                // `display` 会与 `tracing::display` 撞名，故用 target 命名。
                                warn!(target, "控制器请求了不存在的显示器");
                            }
                        }
                        SessionMessage::SetQuality { preset } => {
                            // 档位是**上限**：换上限并把闭环重置到新上限（用户显式意图，不带降级历史）。
                            // 原生分辨率会话同样翻倍（见 `session_encode_options`）。
                            rate.set_ceiling(session_encode_options(
                                preset.options(),
                                native_active,
                            ));
                            // 用户显式选的帧率优先级高于档位默认值：换画质不应把它覆盖回去。
                            if let Some(fps) = selected_fps {
                                rate.set_frame_rate_ceiling(fps);
                            }
                            encode = rate.current();
                            // 档位可能改缩放 → 几何可能变化：补丁模式的持久缓冲尺寸随之作废，
                            // 必须退出（并靠下面的 `force_keyframe` 重发基准）。
                            screen_mode.exit();
                            capture_tick.as_mut().reset(Instant::now() + encode.capture_interval());
                            force_keyframe = true;
                        }
                        SessionMessage::SetFrameRate { fps } => {
                            // 帧率是**上限**，且只动采集节拍与编码器的 fps（在线重配置，**不重建**）。
                            selected_fps = Some(fps);
                            rate.set_frame_rate_ceiling(fps);
                            encode = rate.current();
                            // 补丁模式可以留着（帧率只影响空补丁的保活节拍），但节拍要按新值重排。
                            capture_tick.as_mut()
                                .reset(Instant::now() + capture_tick_period(screen_mode.mode, &encode));
                            // **不置 `force_keyframe`**：纯帧率变化只动采集节拍与编码器的在线 fps，
                            // 参考链不变；插 IDR 会多一个带宽尖峰（阶段 2/3 一直在避免的）。
                            // `SetQuality` 需要关键帧是因为它可能改变编码分辨率（几何变化）。
                            info!("帧率上限已切换（被控端） fps={}", encode.fps);
                        }
                        SessionMessage::QosFeedback { rtt_ms, recv_kbps, decode_ms, backlog }
                            if screen_mode.is_patches() =>
                        {
                            // **补丁模式下闭环暂停**：控制器此刻不解码视频，`decode_ms` /
                            // `recv_kbps` 都是陈旧值（甚至是 0），喂给 AIMD 只会制造假拥塞/假饱和，
                            // 把码率帧率压低，等回到全帧时画面先糊一截。回到全帧后的下一条反馈
                            // 自然恢复闭环（AIMD 状态本身没被污染）。
                            debug!(
                                rtt_ms,
                                recv_kbps,
                                decode_ms,
                                backlog,
                                "补丁模式下忽略 QoS 反馈（控制器未解码视频）"
                            );
                        }
                        SessionMessage::QosFeedback { rtt_ms, recv_kbps, decode_ms, backlog } => {
                            // 控制器的每秒反馈 + 本端出站队列积压 → 一次 AIMD 决策。
                            // **码率/帧率走在线重配置（不重建、不插 IDR）**，只有降分辨率才置
                            // `force_keyframe`（几何变化，见 [`crate::qos`]）。
                            //
                            // 先喂**本端**的变化面积：它只收紧/放开生效值上限，不改 AIMD 状态、
                            // 不置关键帧（见 `RateController::observe_activity`）。
                            rate.observe_activity(last_changed_permille);
                            let local_backlog =
                                u32::try_from(queues.screen_backlog()).unwrap_or(u32::MAX);
                            // 本窗口**实际发出**的码率（编码任务每秒写入）。与控制器回报的
                            // `recv_kbps` 一起算投递率，识别「RTT 还没抬升但链路已在丢包」。
                            let sent_kbps_now = sent_kbps.load(Ordering::Relaxed);
                            let decision = rate.on_feedback(
                                Feedback { rtt_ms, recv_kbps, decode_ms, backlog },
                                local_backlog,
                                sent_kbps_now,
                            );
                            if decision.rates.fps != encode.fps {
                                // 帧率变了：采集节拍要立刻跟着变（与 `SetQuality` 同款）。
                                capture_tick.as_mut().reset(Instant::now() + decision.rates.capture_interval());
                            }
                            encode = decision.rates;
                            if decision.force_keyframe {
                                force_keyframe = true;
                            }
                            let signature = (
                                decision.action,
                                encode.bitrate_kbps,
                                encode.fps,
                                encode.scale.to_bits(),
                            );
                            if last_qos_log != Some(signature) {
                                last_qos_log = Some(signature);
                                // 投递率也内联进消息文本：GUI 日志层会丢掉 tracing 字段，只保留文本。
                                let delivery_ratio = if sent_kbps_now > 0 {
                                    f64::from(recv_kbps) / f64::from(sent_kbps_now)
                                } else {
                                    0.0
                                };
                                info!(
                                    "QoS 调整 action={:?} rtt={}ms rtt_smooth={:?} baseline={:?} recv_kbps={} sent_kbps={} delivery_ratio={:.2} decode_ms={} backlog={} local_backlog={} bitrate_kbps={} fps={} scale={:.3} changed_permille={} cap_kbps={:?} cap_fps={:?}",
                                    decision.action,
                                    rtt_ms,
                                    rate.smoothed_rtt_ms(),
                                    rate.baseline_rtt_ms(),
                                    recv_kbps,
                                    sent_kbps_now,
                                    delivery_ratio,
                                    decode_ms,
                                    backlog,
                                    local_backlog,
                                    encode.bitrate_kbps,
                                    encode.fps,
                                    encode.scale,
                                    last_changed_permille,
                                    rate.content_cap_kbps(),
                                    rate.content_cap_fps(),
                                );
                            }
                        }
                        SessionMessage::RequestKeyframe { display } => {
                            // 控制器解码失败/丢弃了参考帧时的恢复请求。补丁模式下的「参考帧」就是
                            // 控制器那块持久缓冲：请求到达即说明它已不可信，必须回全帧重发 IDR。
                            if screen_mode.is_patches() {
                                exit_patch_mode(
                                    &mut screen_mode,
                                    &mut force_keyframe,
                                    &mut capture_tick,
                                    encode.capture_interval(),
                                );
                            }
                            if display == display_id {
                                force_keyframe = true;
                            }
                        }
                        SessionMessage::Clipboard { text } => {
                            if let Some(cb) = clipboard.clone() {
                                // `Clipboard::set` 在 X11 下别的进程持有 selection 时可长时间阻塞：
                                // 移出 async worker，否则会饿死同一 worker 上的控制通道读循环与心跳。
                                // 同时必须对取消敏感，见 `blocking_or_cancel`。
                                match blocking_or_cancel(&cancel, move || cb.set(&text)).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(err)) => warn!(error = %err, "写入剪贴板失败"),
                                    // 取消：交循环出口统一归一成「用户中断」（与发送路径一致）。
                                    Err(RemoteError::Cancelled) => break Err(RemoteError::Cancelled),
                                    Err(err) => warn!(error = %err, "写入剪贴板任务失败"),
                                }
                            } else {
                                debug!("本端未安装剪贴板实现，忽略写入");
                            }
                        }
                        SessionMessage::ClipboardRequest => {
                            // 同上：`Clipboard::get` 也可能无限阻塞，放阻塞线程并让它可被取消。
                            let text = match clipboard.clone() {
                                Some(cb) => match blocking_or_cancel(&cancel, move || cb.get()).await {
                                    Ok(Ok(text)) => text,
                                    Ok(Err(err)) => {
                                        warn!(error = %err, "读取剪贴板失败");
                                        String::new()
                                    }
                                    Err(RemoteError::Cancelled) => break Err(RemoteError::Cancelled),
                                    Err(err) => {
                                        warn!(error = %err, "读取剪贴板任务失败");
                                        String::new()
                                    }
                                },
                                None => String::new(),
                            };
                            if let Err(err) = queues
                                .enqueue_ctrl(&cancel, SessionFrame::control(SessionMessage::Clipboard { text }))
                                .await
                            {
                                break Err(err);
                            }
                        }
                        SessionMessage::Heartbeat { .. } => {
                            // t2 = 收到时刻、t3 = 发出时刻（本端单调时钟）。控制器用 t1/t2/t3/t4
                            // 估计两端时钟偏差，进而算端到端延迟。
                            let t2_us = crate::clock::now_us();
                            let ack = SessionMessage::HeartbeatAck {
                                t2_us,
                                t3_us: crate::clock::now_us(),
                            };
                            if let Err(err) = queues
                                .enqueue_ctrl(&cancel, SessionFrame::control(ack))
                                .await
                            {
                                break Err(err);
                            }
                        }
                        SessionMessage::HeartbeatAck { .. } => {
                            let now = Instant::now();
                            last_ack_at = Some(now);
                            if let Some(sent) = heartbeat_sent.pop_front() {
                                if let Some(rtt) = trusted_rtt(sent, now) {
                                    config.stats.report(SessionStats::Rtt(rtt));
                                }
                            }
                        }
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
                                    // 按分类投递：`FileError` 与数据块同队列（顺序在已有 chunk 之后），
                                    // 其余文件协议小消息走控制队列。
                                    let mut write_err = None;
                                    for reply in replies {
                                        if let Err(err) =
                                            queues.dispatch_frame(&cancel, SessionFrame::file(reply)).await
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
                                    warn!(error = %err, "文件落盘失败，中断该次传输");
                                    if let Err(send_err) =
                                        send_file_errors(&mut queues, &cancel, transfer.abort_all(&err.to_string())).await
                                    {
                                        break Err(send_err);
                                    }
                                }
                            }
                        }
                        other => debug!(?other, "被控端收到未处理的消息"),
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

    // 会话正在收尾：在途传输一并作废。这里**不必**回发 `abort_all` 的 `FileError`——`Bye` 马上
    // 就到，对端收尾时同样会清掉自己的在途传输，不会挂在半途。
    let _ = transfer.abort_all(&reason);
    // 收尾帧走独立的无界 close 通道：即便取消已触发、普通队列全满，写任务仍会限时尽力写出
    // `Bye`（见 `writer_loop`），使对端把它记成「主动结束」而不是「连接断开」。
    queues.close(SessionFrame::control(SessionMessage::Bye {
        reason: reason.clone(),
    }));
    // 编码任务先停：它可能正阻塞在画面队列的 `send` 上（对端停读）。关掉它的输入队列并限时等待，
    // 超时则中止——它的 `send` 在写任务退出后会拿到 `Closed`，正常收场。
    drop(encode_tx);
    let mut encode_task = encode_task;
    let _ = timeout(BYE_TIMEOUT, &mut encode_task).await;
    encode_task.abort();
    // 丢弃三个普通队列的发送端：写任务在取消后只等 close，这些队列是否还有残留帧都不再重要。
    drop(queues);
    // 写任务自己的 `Bye` 发送也限时（`BYE_TIMEOUT`），这里再给一个上界；超时则中止它——写任务
    // 仍卡在停读的对端上时，只有丢弃写半边（= 关闭 socket）才能避免把连接泄漏到 TCP 超时。
    let mut writer_task = writer_task;
    let _ = timeout(BYE_TIMEOUT, &mut writer_task).await;
    writer_task.abort();
    result
}

/// 出站写任务：独占写半边，按**四档优先级**从队列取帧写线。
///
/// 优先级固定为 close → ctrl → screen → file（`select!` 的 `biased`）。close 是收尾 `Bye`
/// 专用通道，**不理会取消令牌**、限时尽力写出；其余三档在取消后不再发送。
///
/// 取消语义：普通帧的发送同时等取消令牌（[`send_cancellable`]），取消一旦触发就丢弃后续普通帧、
/// 只等收尾帧——这样「取消能立刻打断被背压卡住的发送」与「收尾 `Bye` 仍能尽力发出」两件事同时成立。
pub(crate) async fn writer_loop(
    mut writer: FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    mut rx: OutboundReceivers,
    cancel: CancellationToken,
) {
    // 取消后不再写普通帧，只等收尾帧。
    let mut cancelled = false;
    loop {
        tokio::select! {
            // biased：顺序即优先级。close 永远先于任何普通帧。
            biased;
            Some(frame) = rx.close.recv() => {
                // 收尾帧不理会取消令牌（取消正是最常见的结束原因），限时尽力写出。
                let _ = timeout(BYE_TIMEOUT, writer.send(frame)).await;
                break;
            }
            Some(frame) = rx.ctrl.recv(), if !cancelled => {
                if !write_ordinary(&mut writer, &cancel, frame, &mut cancelled).await {
                    break;
                }
            }
            Some(frame) = rx.screen.recv(), if !cancelled => {
                if !write_ordinary(&mut writer, &cancel, frame, &mut cancelled).await {
                    break;
                }
            }
            Some(frame) = rx.file.recv(), if !cancelled => {
                if !write_ordinary(&mut writer, &cancel, frame, &mut cancelled).await {
                    break;
                }
            }
            // 所有通道都已关闭（或普通通道在取消后被禁用）：任务收场。
            else => break,
        }
    }
}

/// 写一条普通帧；返回 `false` 表示写侧出错、写任务应结束。
///
/// 取消触发时把 `cancelled` 置位并返回 `true`（继续循环只为等 close 通道里的 `Bye`）。
async fn write_ordinary(
    writer: &mut FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    cancel: &CancellationToken,
    frame: SessionFrame,
    cancelled: &mut bool,
) -> bool {
    match send_cancellable(writer, cancel, frame).await {
        Ok(()) => true,
        Err(RemoteError::Cancelled) => {
            *cancelled = true;
            true
        }
        Err(err) => {
            debug!(error = %err, "出站写失败，写任务结束");
            false
        }
    }
}

/// 出站写任务已结束（链路断开）时的会话级错误。
pub(crate) fn link_closed() -> RemoteError {
    RemoteError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "出站写任务已结束",
    ))
}

/// 后台采集任务要做的采集类型。
#[derive(Debug, Clone, Copy)]
enum CaptureRequest {
    /// 全帧视频采集（CPU 或 GPU）。
    Frame {
        /// 是否走 GPU 采集。
        gpu: bool,
    },
    /// 脏矩形补丁采集（编码尺寸坐标系，紧包 NV12）。
    Patches {
        /// 编码缩放比例（语义同 [`crate::video::encode_dimensions`]）。
        scale: f32,
        /// 编码宽度封顶。
        max_width: u32,
    },
}

/// 一次后台采集的产物。
enum CapturePayload {
    /// 全帧原始帧；失败为 `None`。
    Frame(Option<RawFrame>),
    /// 脏矩形补丁；失败或「本拍不适合补丁」为 `None`。
    Patches(Option<CapturedPatches>),
}

/// 一次后台采集的结果。
struct CaptureOutcome {
    /// 采集代次（见循环里的 `capture_generation`）：用于丢弃被看门狗判定为丢失的过期结果。
    generation: u64,
    /// 采集产物（全帧或补丁）。
    payload: CapturePayload,
    /// 本次采集耗时（微秒；采集失败时也记实际耗时）。**测量**用，不参与控制决策。
    ///
    /// 用微秒而不是毫秒：GPU 采集常在 1 ms 以内，毫秒取整会恒为 0（现场看到的就是这个）。
    capture_us: u64,
    /// 采集**完成**的时刻（本端单调时钟，微秒；[`crate::clock::now_us`]）。
    ///
    /// 它随画面帧一路上线（`VideoPacket` / `RegionUpdate` 的 `capture_ts_us`），控制器据此算
    /// 「采集 → 解码完成」的端到端延迟（D2）。失败时仍填（该帧本来就不会上线，只求代码统一）。
    capture_ts_us: u64,
}

/// 交给编码任务的一帧。
///
/// `display`/`rates`/`force_keyframe` 由会话循环带上：编码任务据此判断是否需要**重建**编码器
/// （只有几何变化才重建）与是否强制关键帧（首帧、切显示器、切档位、`RequestKeyframe`、降分辨率）。
/// **码率/帧率不触发重建**——它们在编码任务里在线重配置（见 [`encode_loop`]）。
struct EncodeJob {
    /// 来源显示器编号。
    display: u32,
    /// 当前**生效**编码参数（≤ 档位上限）。
    rates: EncodeRates,
    /// 本帧是否强制关键帧。
    force_keyframe: bool,
    /// 本帧的采集耗时（微秒），只进统计、不参与控制决策。
    capture_us: u64,
    /// 本帧采集完成的时刻（本端单调时钟，微秒），原样写进 `VideoPacket.capture_ts_us`。
    capture_ts_us: u64,
    /// 原始帧。
    frame: RawFrame,
}

/// 在阻塞线程里采集一帧（全帧视频或脏矩形补丁）。
///
/// `ScreenSource::capture`（xcap）是 CPU/IO 阻塞调用：1080p 几十毫秒，Wayland 的 xcap portal
/// 路径可能数秒。跑在 async worker 上会饿死同一 worker 上的控制通道读循环与心跳，故整体移进
/// `spawn_blocking`。
///
/// 本函数**可能 panic**（平台实现 bug）：`spawn_blocking` 的 `JoinHandle` 被丢弃后 panic 会被
/// 吞掉、结果永不回传，由调用方的看门狗兜底（见 [`CAPTURE_WATCHDOG`]）。
fn run_capture(
    screen: &dyn ScreenSource,
    display: u32,
    generation: u64,
    request: CaptureRequest,
    tx: &mpsc::Sender<CaptureOutcome>,
) {
    let started = Instant::now();
    let payload = match request {
        CaptureRequest::Frame { gpu } => {
            let captured = if gpu {
                screen.capture_gpu(display)
            } else {
                screen.capture(display)
            };
            CapturePayload::Frame(match captured {
                Ok(frame) => Some(frame),
                Err(err) => {
                    warn!(error = %err, "屏幕采集失败");
                    None
                }
            })
        }
        CaptureRequest::Patches { scale, max_width } => {
            CapturePayload::Patches(match screen.capture_patches(display, scale, max_width) {
                Ok(patches) => patches,
                Err(err) => {
                    warn!(error = %err, "脏矩形补丁采集失败");
                    None
                }
            })
        }
    };
    let capture_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    // 采集**返回之后**立刻打时间戳：它是控制器的端到端延迟（采集→解码完成）的起点。
    // 失败路径也打（结果不会上线，只是让所有分支形状一致）。
    let capture_ts_us = crate::clock::now_us();
    // 接收端随会话循环存活；会话已结束时发送失败是正常的，忽略。
    let _ = tx.blocking_send(CaptureOutcome {
        generation,
        payload,
        capture_us,
        capture_ts_us,
    });
}

/// 编码任务每秒写给会话循环的**完整快照**（C2/C3/C5 与遥测的唯一来源）。
///
/// `SessionStats::Encode` 只带一部分读数（且是进程内通道）；主循环组装 `HostStats` 还需要
/// 出站队列深度、编码器重建/强制 IDR 次数、编码器平台信息。这些只在编码任务里，故每秒经
/// 这个共享槽交出去一次。**不进线上结构**，只是进程内传递。
#[derive(Debug, Clone, Default)]
struct EncodeSnapshot {
    /// 最近一秒平均单帧编码耗时（毫秒）。
    encode_ms: u32,
    /// 最近一秒平均单帧采集耗时（微秒）。
    capture_us: u32,
    /// 最近一秒实测编码帧率。
    fps: u32,
    /// 当前生效目标码率（kbps）。
    bitrate_kbps: u32,
    /// 最近一秒实测出码速率（kbps）。
    sent_kbps: u32,
    /// 最近一秒因无变化跳过的帧数。
    skipped: u32,
    /// 最近一秒关键帧数。
    keyframes: u32,
    /// 最近一秒关键帧出码码率（kbps）。
    idr_kbps: u32,
    /// 最近一秒关键帧出码字节数。
    idr_bytes: u64,
    /// 最近一秒普通帧出码码率（kbps）。
    p_kbps: u32,
    /// 最近一秒平均变化面积（千分比）。
    changed_permille: u32,
    /// 最近一秒编码器重建次数。
    encoder_rebuilds: u32,
    /// 最近一秒因 `force` 产出的关键帧数。
    forced_idrs: u32,
    /// 最近一秒因出站队列满而跳过的帧数。
    out_queue_skips: u32,
    /// 采样时刻的出站画面队列深度（帧）。
    out_queue_depth: u32,
    /// 当前编码尺寸（宽, 高）。
    encode_dims: Option<(u32, u32)>,
    /// 当前编码器的平台信息（后端/名称/适配器/零拷贝）。
    encoder_info: EncoderInfo,
}

/// 编码侧读数累加器：按 [`STATS_INTERVAL`] 汇总一次，经进程内 [`SessionStatsReporter`] 送到 UI。
///
/// **不进热路径的锁**：只在编码任务里累加几个整数，到点发一条无界通道消息（接收端缺席即丢弃）
/// 并往共享槽写一份完整快照（`std::sync::Mutex`，每秒一次）。
struct EncodeStatsWindow {
    /// 本窗口起点。
    start: Instant,
    /// 本窗口编码成功的帧数。
    frames: u32,
    /// 本窗口出码字节数。
    bytes: u64,
    /// 本窗口编码耗时合计（毫秒）。
    encode_ms: u64,
    /// 本窗口采集耗时合计（微秒，与被编码的帧一起计）。
    capture_us: u64,
    /// 本窗口因「无变化」被跳过的帧数（不含背压/去重之外的其它丢弃）。
    skipped: u32,
    /// 本窗口内的**关键帧**数（`VideoPacket.keyframe` 为真）。
    keyframes: u32,
    /// 本窗口内关键帧的出码字节数（用于把「关键帧码率」与「普通帧码率」拆开）。
    idr_bytes: u64,
    /// 本窗口内普通帧的出码字节数。
    p_bytes: u64,
    /// 本窗口内各编码帧的 `changed_permille` 累加（用于算窗口平均变化面积）。
    changed_permille_sum: u64,
    /// 本窗口编码器重建次数（C3）。
    encoder_rebuilds: u32,
    /// 本窗口因 `force` 真正产出的关键帧数（C3）。
    forced_idrs: u32,
    /// 本窗口因出站队列满而跳过编码的次数（C2 背压）。
    out_queue_skips: u32,
    /// 最近一次采样到的出站队列深度（C2；瞬时量，不是窗口累计）。
    out_queue_depth: u32,
    /// 最近一次采样到的编码尺寸（几何；主循环据此填 `HostStats.encode_width/height`）。
    encode_dims: Option<(u32, u32)>,
    /// 当前编码器的平台信息（只在创建成功时更新）。
    encoder_info: EncoderInfo,
}

impl EncodeStatsWindow {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            frames: 0,
            bytes: 0,
            encode_ms: 0,
            capture_us: 0,
            skipped: 0,
            keyframes: 0,
            idr_bytes: 0,
            p_bytes: 0,
            changed_permille_sum: 0,
            encoder_rebuilds: 0,
            forced_idrs: 0,
            out_queue_skips: 0,
            out_queue_depth: 0,
            encode_dims: None,
            encoder_info: EncoderInfo::default(),
        }
    }

    /// 记一帧的产出与耗时（采集 + 编码），并按下「关键帧 / 普通帧」拆解字节。
    fn record(
        &mut self,
        bytes: usize,
        encode_ms: u64,
        capture_us: u64,
        keyframe: bool,
        changed_permille: u16,
    ) {
        self.frames = self.frames.saturating_add(1);
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.bytes = self.bytes.saturating_add(bytes);
        if keyframe {
            self.keyframes = self.keyframes.saturating_add(1);
            self.idr_bytes = self.idr_bytes.saturating_add(bytes);
        } else {
            self.p_bytes = self.p_bytes.saturating_add(bytes);
        }
        self.changed_permille_sum = self
            .changed_permille_sum
            .saturating_add(u64::from(changed_permille));
        self.encode_ms = self.encode_ms.saturating_add(encode_ms);
        self.capture_us = self.capture_us.saturating_add(capture_us);
    }

    /// 记一帧因「采集层判定无变化」而被跳过。
    fn record_skip(&mut self) {
        self.skipped = self.skipped.saturating_add(1);
    }

    /// 到点则上报一次并重置窗口。`bitrate_kbps` 是**当前生效的目标码率**（闭环值，非实测出码率）；
    /// `sent_kbps` 是共享的实测出码速率槽，供 QoS 闭环算投递率。
    ///
    /// **重置之前**先把完整快照写进共享槽（`snapshot`）：会话主循环在同一拍（心跳 tick）读它组
    /// 装 `HostStats`。两者都在 ~1s 节拍上，读写顺序不影响正确性——最坏只是多显示/少显示一拍。
    fn maybe_report(
        &mut self,
        stats: &SessionStatsReporter,
        bitrate_kbps: u32,
        sent_kbps: &AtomicU32,
        snapshot: &std::sync::Mutex<Option<EncodeSnapshot>>,
    ) {
        let elapsed = self.start.elapsed();
        if elapsed < STATS_INTERVAL {
            return;
        }
        // 实测出码速率：窗口字节数 / 窗口时长。先算再重置窗口。
        let sent_kbps_value = kbps_from_bytes(self.bytes, elapsed);
        sent_kbps.store(sent_kbps_value, Ordering::Relaxed);
        let secs = elapsed.as_secs_f64().max(f64::EPSILON);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "帧率为有界非负值，取整后落在 u32 内"
        )]
        let fps = (f64::from(self.frames) / secs).round() as u32;
        let encode_ms = if self.frames == 0 {
            0
        } else {
            u32::try_from(self.encode_ms / u64::from(self.frames)).unwrap_or(u32::MAX)
        };
        let capture_us = if self.frames == 0 {
            0
        } else {
            u32::try_from(self.capture_us / u64::from(self.frames)).unwrap_or(u32::MAX)
        };
        // 出码拆解：关键帧与普通帧分开算码率。单位换算是整数：kbps = bytes × 8 / 毫秒
        // （`bytes × 8 / (secs × 1000) × 1000`），避免浮点在 pedantic 下的 cast 告警。
        let idr_kbps = kbps_from_bytes(self.idr_bytes, elapsed);
        let p_kbps = kbps_from_bytes(self.p_bytes, elapsed);
        let changed_permille = if self.frames == 0 {
            0
        } else {
            u32::try_from(self.changed_permille_sum / u64::from(self.frames))
                .unwrap_or(u32::MAX)
        };
        stats.report(SessionStats::Encode {
            encode_ms,
            capture_us,
            fps,
            bitrate_kbps,
            skipped: self.skipped,
            keyframes: self.keyframes,
            idr_kbps,
            idr_bytes: self.idr_bytes,
            p_kbps,
            changed_permille,
        });
        // 完整快照（含只有编码任务知道的字段）：写进共享槽供主循环组装 `HostStats`。
        if let Ok(mut slot) = snapshot.lock() {
            *slot = Some(EncodeSnapshot {
                encode_ms,
                capture_us,
                fps,
                bitrate_kbps,
                sent_kbps: sent_kbps_value,
                skipped: self.skipped,
                keyframes: self.keyframes,
                idr_kbps,
                idr_bytes: self.idr_bytes,
                p_kbps,
                changed_permille,
                encoder_rebuilds: self.encoder_rebuilds,
                forced_idrs: self.forced_idrs,
                out_queue_skips: self.out_queue_skips,
                out_queue_depth: self.out_queue_depth,
                encode_dims: self.encode_dims,
                encoder_info: self.encoder_info.clone(),
            });
        }
        self.start = Instant::now();
        self.frames = 0;
        self.bytes = 0;
        self.encode_ms = 0;
        self.capture_us = 0;
        self.skipped = 0;
        self.keyframes = 0;
        self.idr_bytes = 0;
        self.p_bytes = 0;
        self.changed_permille_sum = 0;
        self.encoder_rebuilds = 0;
        self.forced_idrs = 0;
        self.out_queue_skips = 0;
    }
}

/// 把窗口内的字节数换算成 kbps（整数运算，按窗口实际时长归一）。
fn kbps_from_bytes(bytes: u64, elapsed: Duration) -> u32 {
    let millis = u64::try_from(elapsed.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    u32::try_from(bytes.saturating_mul(8) / millis).unwrap_or(u32::MAX)
}

/// 编码任务：独占编码器，去重 → （在线重配置）→ 编码 → 投递。
///
/// # 丢弃策略（**改动前先读模块文档**）
///
/// - **静止画面默认不喂编码器**：CPU 路径与上一帧原始 RGBA 逐字节相同、GPU 路径由采集层
///   的**像素级**变化检测报告「内容没变」（[`RawFrame::changed`] 为 `false`）时直接跳过——
///   不转换、不编码、不发送。这是带宽趋零的唯一来源：GPU 路径只看「有没有新合成帧」是不够的
///   （WGC 在静止桌面上仍按刷新节奏产帧），所以平台采集源会在拿到新帧后再做一次
///   320×180 下采样 + 回读比较（见 `win-video` 的 `capture::change`）。
/// - **静止时也不是永远不发**：距上次真正编码超过 `unchanged_resend` 就补编一帧。检测有盲区
///   （点采样），编码前的背压出口与采集看门狗也会让「已检测到的变化」一帧都没发出去；
///   没有这条保活，用户看到的是「远端静止一段时间后，第一次变化要反复几次才出现」。
///   **补编只绕过「无变化就跳过」这一道闸门，`force` 语义不变，因此绝不会带出 IDR。**
/// - **输出队列满 → 在编码之前跳过这一帧**（等价降帧率）。**绝不**先编码再丢弃：时域编码的
///   参考链是串起来的，丢掉一个已编码的 P 帧会让后续引用它的帧在解码端永远解不出来。
///
/// # 重建 vs 在线重配置（**本阶段最容易踩的坑**）
///
/// 只有**几何**（编码宽高，即 `rates.scale` 推出的尺寸）变化才重建编码器并重发 `ScreenConfig`。
/// **码率 / 帧率变化走 [`VideoEncoder::set_bitrate`] / [`VideoEncoder::set_fps`] 在线重配置，
/// 不重建、不插 IDR**。把码率放进重建键会让 AIMD 每秒插一个关键帧，正好把阶段 2 消掉的带宽
/// 尖峰引回来（见 `vendor/shiguredo_libvpx` 的补丁 6 与 [`crate::qos`]）。
#[expect(
    clippy::too_many_lines,
    reason = "去重 → 几何重建 → 在线重配置 → 编码 → 投递是一条不可分割的流水线，拆成多个函数反而要来回传编码器与基准状态"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "编码卡片的输入（任务队列/出站队列/取消/统计/codec/工厂/保活间隔/出码速率槽/基准帧信号）都是它独占持有的依赖，打包成结构体反而多一层间接"
)]
async fn encode_loop(
    mut rx: mpsc::Receiver<EncodeJob>,
    out: mpsc::Sender<SessionFrame>,
    cancel: CancellationToken,
    stats: SessionStatsReporter,
    codec: CodecId,
    encoder_factory: Arc<dyn VideoEncoderFactory>,
    unchanged_resend: Duration,
    sent_kbps: Arc<AtomicU32>,
    base_frame_sent: Arc<AtomicBool>,
    // 本会话的编码宽度上限（1920 或原生 3840，见 [`effective_max_encode_width`]）。
    // 与平台采集侧用的是**同一个值**，否则补丁尺寸与全帧尺寸会对不上。
    max_encode_width: u32,
    // 每秒写一次的完整快照槽（主循环组装 `HostStats` 用）。
    encode_snapshot: Arc<std::sync::Mutex<Option<EncodeSnapshot>>>,
) {
    let mut encoder: Option<Box<dyn VideoEncoderApi>> = None;
    // 当前编码器的**几何**（宽, 高）。**只有它变化才重建**——码率/帧率不在这里。
    let mut encoder_dims: Option<(u32, u32)> = None;
    // 上一帧的原始 RGBA（去重基准）。保留它是去重的定义：memcmp 远快于密码学哈希。
    let mut prev_rgba: Option<Vec<u8>> = None;
    // 已下发的 `ScreenConfig` 尺寸；变化时重发。
    let mut sent_dims: Option<(u32, u32)> = None;
    // 因出站队列满而被跳过的那一帧若曾要求关键帧，这里记下来，**下一帧编码时补上**。
    //
    // 少了它就有个真实的坑：控制器进入 `awaiting_keyframe` 后只请求一次 IDR，若那一帧恰好因
    // 队列满被跳过，控制器就要一直等到 `KEYFRAME_INTERVAL` 的周期性兜底（数百帧）才恢复——
    // 用户看到的是「画面卡住十几秒」。关键帧请求不能因为一次背压丢弃而蒸发。
    let mut pending_keyframe = false;
    let mut seq: u64 = 0;
    let mut window = EncodeStatsWindow::new();
    // 上一次**真正编码**（含空包）的时刻：静止保活补编的水位（见 [`UNCHANGED_RESEND_INTERVAL`]）。
    let mut last_encode: Option<Instant> = None;

    while let Some(job) = rx.recv().await {
        if cancel.is_cancelled() {
            break;
        }
        let EncodeJob {
            display,
            rates,
            force_keyframe,
            capture_us,
            capture_ts_us,
            frame,
        } = job;
        // 本帧的变化面积（千分比）：进统计窗口的平均，并随每帧上报落进日志/UI。
        let changed_permille = frame.changed_permille;
        // 出站队列当前深度（C2）：主循环的 `HostStats` 每秒读一次快照。
        window.out_queue_depth = u32::try_from(out.max_capacity().saturating_sub(out.capacity()))
            .unwrap_or(u32::MAX);
        // 每秒上报一次编码侧读数（静止画面也会经过这里——采集每拍都投递任务，只是被去重跳过）。
        // 快照写入发生在窗口重置**之前**，因此主循环读到的是刚刚结束的那一秒。
        window.maybe_report(&stats, rates.bitrate_kbps, &sent_kbps, &encode_snapshot);

        // GPU 帧没有 CPU 像素：不能做 memcmp 去重（也绝不能拿空 `rgba` 去比较）。
        let is_gpu = frame.gpu.is_some();
        // 变化判据分两条路：
        // - GPU：采集层已经做过**像素级**变化检测（`RawFrame::changed`），不做 CPU 读回比较。
        //   检测器不可用时采集层保守给 `true`，这里不需要额外兜底。
        // - CPU：沿用逐字节 memcmp（与上一帧相同即视为静止）。
        let unchanged = if is_gpu {
            !frame.changed
        } else {
            prev_rgba.as_deref() == Some(frame.rgba.as_slice())
        };
        let (width, height) =
            encode_dimensions(frame.width, frame.height, rates.scale, max_encode_width);
        // 当前几何（供 `HostStats.encode_width/height`）。
        window.encode_dims = Some((width, height));
        // **几何变化才重建**：码率/帧率在下方在线重配置。
        let geometry_changed = encoder_dims != Some((width, height));
        if geometry_changed {
            if sent_dims != Some((width, height)) {
                let config = SessionMessage::ScreenConfig {
                    display,
                    codec,
                    width,
                    height,
                };
                if !send_video(&out, &cancel, config).await {
                    break;
                }
                sent_dims = Some((width, height));
            }
            encoder = None;
            prev_rgba = None;
            encoder_dims = Some((width, height));
        }
        let force = force_keyframe || geometry_changed || pending_keyframe;

        // 静止保活：距上次真正编码超过 `unchanged_resend` 就补一帧。它只绕过下面的
        // 「无变化就跳过」闸门——**不**参与 `force` 的计算，所以补编永远是普通 P 帧，
        // 不会像 `force` 那样带出 IDR（QoS 每秒动一次就插 IDR 是专门修掉的回归）。
        let resend_due = last_encode.is_none_or(|at| at.elapsed() >= unchanged_resend);

        // 静止画面：**默认**不喂编码器（不转换、不编码、不发送），但距上次编码超过
        // `unchanged_resend` 时必须补编一帧（保活，见 [`UNCHANGED_RESEND_INTERVAL`]）——
        // 「永久不发」会让检测盲区/背压丢弃变成画面上永久冻结的一帧。
        // `force`（首帧/切显示器/切档位/RequestKeyframe/几何变化）为真时必须照常编码，
        // 否则控制器 `NO_FRAME_TIMEOUT` 要的 IDR 会被这里吞掉。
        if !force && unchanged && !resend_due {
            window.record_skip();
            continue;
        }
        // 背压出口在**编码之前**：出站队列满就跳过这一帧。绝不能改成「编码后丢弃」。
        // 被跳过的关键帧请求必须留下（`pending_keyframe`），否则控制器的恢复请求会蒸发。
        if out.capacity() == 0 {
            pending_keyframe |= force;
            window.out_queue_skips = window.out_queue_skips.saturating_add(1);
            debug!("出站队列已满，跳过本轮编码（编码前丢帧，不破坏参考链）");
            continue;
        }
        if encoder.is_none() {
            match encoder_factory.create(width, height, rates.bitrate_kbps, rates.fps) {
                Ok(enc) => {
                    // C3：每次**成功创建**都算一次重建（含首建；几何未变时的首建也是从无到有）。
                    window.encoder_rebuilds = window.encoder_rebuilds.saturating_add(1);
                    window.encoder_info = enc.info();
                    encoder = Some(enc);
                }
                Err(err) => {
                    warn!(error = %err, "编码器创建失败，跳过本帧");
                    pending_keyframe |= force;
                    continue;
                }
            }
        }
        let Some(enc) = encoder.take() else {
            continue;
        };
        let started = Instant::now();
        // 编码是阻塞调用，移出 async worker；编码器与整帧（CPU 的 `rgba` 或 GPU 纹理）
        // 随闭包移入、随结果移出（独占持有）。码率/帧率的在线重配置也在这里做
        // （`reconfig_and_encode`），与编码同一次阻塞调用。
        let joined = tokio::task::spawn_blocking(move || {
            let mut enc = enc;
            let result = reconfig_and_encode(enc.as_mut(), &rates, &frame, force);
            (enc, result, frame)
        })
        .await;
        let (enc, result, frame) = match joined {
            Ok(value) => value,
            Err(err) => {
                warn!(error = %err, "编码任务失败");
                encoder = None;
                prev_rgba = None;
                pending_keyframe |= force;
                continue;
            }
        };
        let encode_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(packet) => {
                encoder = Some(enc);
                // 任何一次成功的编码都推进保活水位（**包括空包**）：否则编码器持续吐空包时
                // 会变成每拍重试，保活间隔形同虚设。
                last_encode = Some(Instant::now());
                // GPU 帧没有 CPU 像素可留作去重基准；置 `None` 让下一帧照常编码。
                prev_rgba = if is_gpu { None } else { Some(frame.rgba) };
                // `force` 为真时本帧必是 IDR，请求已被满足。
                pending_keyframe = false;
                if packet.is_empty() {
                    continue;
                }
                seq = seq.wrapping_add(1);
                let keyframe = packet.keyframe;
                if force && keyframe {
                    // C3：因 `force`（首帧/切显示器/切档位/关键帧请求/几何变化/降分辨率）而
                    // 真正产出的 IDR。`force` 为真的那一拍可能被上面的背压出口跳过，
                    // 所以必须以 packet 实际是不是关键帧为准。
                    window.forced_idrs = window.forced_idrs.saturating_add(1);
                }
                window.record(
                    packet.data.len(),
                    encode_ms,
                    capture_us,
                    keyframe,
                    changed_permille,
                );
                let mut flags = 0u8;
                if geometry_changed {
                    flags |= VIDEO_FLAG_CONFIG;
                }
                if keyframe {
                    flags |= VIDEO_FLAG_KEYFRAME;
                }
                let msg = SessionMessage::VideoPacket {
                    display,
                    seq,
                    codec,
                    flags,
                    width,
                    height,
                    data: packet.data,
                    capture_ts_us,
                };
                if !send_video(&out, &cancel, msg).await {
                    break;
                }
                if keyframe {
                    // 通知会话循环：这块**全帧基准**已经进入出站队列。补丁走同一条 FIFO 队列，
                    // 因此之后的 `RegionUpdate` 必然排在它后面（顺序不变量，见模块文档）。
                    base_frame_sent.store(true, Ordering::Relaxed);
                }
            }
            Err(err) => {
                // 编码/重配置失败后编码器内部状态不可信：丢弃它，下一帧重建（几何未变，但
                // `pending_keyframe` 会强制关键帧）。
                warn!(error = %err, "画面编码失败，重建编码器");
                encoder = None;
                prev_rgba = None;
                sent_dims = None;
                pending_keyframe |= force;
            }
        }
    }
}

/// 先在线重配置码率/帧率（若与编码器当前值不同），再编码。
///
/// **绝不重建编码器**：重建会强制 IDR，闭环每秒动一次就是每秒一个关键帧。几何不变时
/// `set_bitrate` / `set_fps` 走 vendor 补丁 6（`vpx_codec_enc_config_set`）。
///
/// # Errors
/// libvpx 拒绝重配置或编码失败时返回 [`RemoteError::Encode`]（调用方丢弃编码器、下一帧重建）。
fn reconfig_and_encode(
    enc: &mut dyn VideoEncoderApi,
    rates: &EncodeRates,
    frame: &RawFrame,
    force: bool,
) -> RemoteResult<EncodedVideo> {
    if enc.bitrate_kbps() != rates.bitrate_kbps {
        enc.set_bitrate(rates.bitrate_kbps)?;
    }
    if enc.fps() != u32::from(rates.fps) {
        enc.set_fps(rates.fps)?;
    }
    // CPU 帧走 trait 默认实现（= `encode`）；GPU 帧由平台实现覆盖。
    enc.encode_frame(frame, force)
}

/// 把一帧画面消息投给出站写任务，**绝不丢弃**。
///
/// 画面帧一旦编码就不能丢（参考链），故这里 `send().await` 直到画面队列腾空；背压由编码任务
/// 之前的「跳过采集」出口消化（见 [`encode_loop`]）。返回 `false` 表示会话正在收尾
/// （取消触发或写任务已结束），编码任务据此退出。
async fn send_video(out: &mpsc::Sender<SessionFrame>, cancel: &CancellationToken, msg: SessionMessage) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        res = out.send(SessionFrame::screen(msg)) => res.is_ok(),
    }
}

/// 可取消的发送（由出站写任务 [`writer_loop`] 使用）。
///
/// 对端停读（中继停走、TCP 零窗口）会把写缓冲填满，`writer.send` 随即永久挂起。写任务必须能
/// 被取消信号打断，否则用户点「中断」后写任务会一直挂到 TCP 超时，会话收尾被永久拖住。故每次
/// 发送都同时等取消令牌，取消优先（`biased`）。
///
/// 发送被中途取消是安全的：会话正要结束，半截帧不会被对端当成完整帧（长度前缀 + bincode
/// 自校验），且此后不会再发新帧（写任务转入「只等收尾帧」）。
///
/// # Errors
/// 取消令牌触发时返回 [`RemoteError::Cancelled`]；发送失败时透传底层 IO 错误。
pub(crate) async fn send_cancellable(
    writer: &mut FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    cancel: &CancellationToken,
    frame: SessionFrame,
) -> RemoteResult<()> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(RemoteError::Cancelled),
        res = writer.send(frame) => res.map_err(RemoteError::Io),
    }
}

/// 把 [`TransferHub::abort_all`] 返回的 `FileError` 依次投进出站队列。
///
/// **必须回发**：发送方读文件失败后不会再有任何 chunk 流动，对端既等不到 `FileDone` 也等不到
/// 错误，会一直挂在半途（UI 停在 99%）。`FileError` 属 file 类，按分类只会暂存、不阻塞主循环。
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

/// 在阻塞线程上跑一次平台调用，但**对取消敏感**。
///
/// 与 [`send_cancellable`] 同一个道理：`select!` 的 handler 里直接 `spawn_blocking(..).await`
/// 会让循环看不见取消信号——`Clipboard::get`/`set` 在 X11 下别的进程持有 selection 时可长时间
/// 阻塞，用户点「中断」后循环照样卡在这里，`Bye` 与 `on_closed` 一并推迟到阻塞调用返回。
///
/// 丢弃 `JoinHandle` 不会终止那个阻塞任务，但它会自行跑完（只是没人等它），不再拖住会话循环。
///
/// # Errors
/// 取消令牌触发时返回 [`RemoteError::Cancelled`]；阻塞任务 panic（`JoinError`）时返回
/// [`RemoteError::Platform`]。闭包自身的错误包在内层 `Result` 里由调用方解释。
async fn blocking_or_cancel<T, F>(cancel: &CancellationToken, f: F) -> RemoteResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(RemoteError::Cancelled),
        res = tokio::task::spawn_blocking(f) => {
            res.map_err(|err| RemoteError::Platform(format!("阻塞任务失败：{err}")))
        }
    }
}

/// 明文回一条拒绝的 `HelloAck`。
async fn reject_handshake<W>(writer: &mut FramedWrite<W, SessionCodec>, reason: &str)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let ack = SessionFrame::control(SessionMessage::HelloAck {
        accepted: false,
        reason: Some(reason.to_owned()),
        displays: Vec::new(),
        x25519_pub: Vec::new(),
        // 拒绝路径无临时公钥可绑定，MAC 留空。
        hmac_ack: Vec::new(),
    });
    if writer.send(ack).await.is_err() {
        debug!("拒绝应答发送失败");
    }
}

/// 一个用于测试/兜底的采集源：无显示器。
#[derive(Debug, Default)]
pub struct EmptyScreenSource;

impl ScreenSource for EmptyScreenSource {
    fn displays(&self) -> Vec<crate::screen::DisplayInfo> {
        Vec::new()
    }

    fn capture(&self, _display: u32) -> RemoteResult<RawFrame> {
        Err(RemoteError::Platform("无可用显示器".to_owned()))
    }
}

/// 一个什么都不做的输入注入目标。
#[derive(Debug, Default)]
pub struct NullInputSink;

impl InputSink for NullInputSink {
    fn mouse_move(&self, _x: i32, _y: i32, _display: u32) -> RemoteResult<()> {
        Ok(())
    }
    fn mouse_button(&self, _button: crate::screen::MouseButton, _pressed: bool) -> RemoteResult<()> {
        Ok(())
    }
    fn wheel(&self, _dx: i32, _dy: i32) -> RemoteResult<()> {
        Ok(())
    }
    fn key(&self, _event: crate::screen::KeyEvent) -> RemoteResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{PendingSession, PendingSessions};
    use crate::crypto::{derive_session_keys, Cipher};
    use crate::error::RemoteResult;
    use crate::screen::{
        CapturedPatches, Clipboard, DisplayInfo, EncodeOptions, EncodeRates, GpuFrame, InputEvent,
        InputSink, KeyEvent, MouseButton, RawFrame, ScreenPatch, ScreenSource,
    };
    use crate::session::{
        plan_controller_handshake, ControllerHandshake, SessionCodec, SessionFrame, SessionMessage,
        LIVENESS_TIMEOUT,
    };
    use crate::transfer::{TransferEvent, TransferHub};
    use crate::transport::BoxDuplex;
    use rust_tunnel_common::protocol::RemoteMode;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, ReadBuf, ReadHalf, WriteHalf};
    use tokio::sync::mpsc;
    use tokio_util::codec::{FramedRead, FramedWrite};
    use tokio_util::sync::CancellationToken;

    /// 单条等待上限，避免测试挂死。
    const WAIT: Duration = Duration::from_secs(5);

    type PeerRead = FramedRead<ReadHalf<BoxDuplex>, SessionCodec>;
    type PeerWrite = FramedWrite<WriteHalf<BoxDuplex>, SessionCodec>;

    /// 造一帧纯色 RGBA。
    fn solid(display: u32, w: u32, h: u32) -> RawFrame {
        RawFrame {
            display,
            width: w,
            height: h,
            rgba: [40u8, 80, 120, 255].repeat((w * h) as usize),
            gpu: None,
            changed: true,
            changed_permille: 1000,
        }
    }

    /// 采集开始即报信、随后阻塞 `delay` 的假采集源（模拟 Wayland portal 的慢采集）。
    struct SlowScreen {
        started: mpsc::UnboundedSender<()>,
        delay: Duration,
    }

    impl ScreenSource for SlowScreen {
        fn displays(&self) -> Vec<DisplayInfo> {
            vec![DisplayInfo {
                id: 0,
                width: 32,
                height: 24,
                name: "slow".into(),
                primary: true,
            }]
        }

        fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
            let _ = self.started.send(());
            std::thread::sleep(self.delay);
            Ok(solid(display, 32, 24))
        }
    }

    /// 大分辨率**噪声**帧：VP9 关键帧远大于小容量 duplex，首帧发送必然阻塞。
    struct NoiseScreen;

    impl ScreenSource for NoiseScreen {
        fn displays(&self) -> Vec<DisplayInfo> {
            vec![DisplayInfo {
                id: 0,
                width: 800,
                height: 600,
                name: "noise".into(),
                primary: true,
            }]
        }

        fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
            let (w, h) = (800u32, 600u32);
            // 确定性伪随机（LCG），避免引额外依赖。
            let mut x: u32 = 0x1234_5678;
            let mut rgba = Vec::with_capacity((w * h) as usize * 4);
            for _ in 0..(w * h) {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                rgba.push(u8::try_from(x >> 24).unwrap_or(0));
                rgba.push(u8::try_from((x >> 16) & 0xff).unwrap_or(0));
                rgba.push(u8::try_from((x >> 8) & 0xff).unwrap_or(0));
                rgba.push(255);
            }
            Ok(RawFrame {
                display,
                width: w,
                height: h,
                rgba,
                gpu: None,
                changed: true,
                changed_permille: 1000,
            })
        }
    }

    /// 首次采集 panic、之后正常的假采集源：模拟平台抓屏代码崩溃。
    struct PanicOnceScreen {
        calls: AtomicUsize,
    }

    impl PanicOnceScreen {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ScreenSource for PanicOnceScreen {
        fn displays(&self) -> Vec<DisplayInfo> {
            vec![DisplayInfo {
                id: 0,
                width: 8,
                height: 8,
                name: "panic-once".into(),
                primary: true,
            }]
        }

        fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
            // 第一次采集即 panic（模拟平台抓屏崩溃），之后正常出帧。
            assert!(
                self.calls.fetch_add(1, Ordering::SeqCst) != 0,
                "模拟抓屏崩溃"
            );
            Ok(solid(display, 8, 8))
        }
    }

    /// 每次采集都「慢过看门狗」的假采集源：模拟真正挂死的采集侧。
    struct HangingScreen {
        calls: AtomicUsize,
    }

    impl HangingScreen {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ScreenSource for HangingScreen {
        fn displays(&self) -> Vec<DisplayInfo> {
            vec![DisplayInfo {
                id: 0,
                width: 8,
                height: 8,
                name: "hanging".into(),
                primary: true,
            }]
        }

        fn capture(&self, _display: u32) -> RemoteResult<RawFrame> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(400));
            Err(RemoteError::Platform("挂死的采集".to_owned()))
        }
    }

    /// 显示器列表会变的假采集源：前 `stable_calls` 次返回旧列表，之后返回新列表。
    struct ShiftingScreen {
        calls: AtomicUsize,
        old: Vec<DisplayInfo>,
        new: Vec<DisplayInfo>,
        stable_calls: usize,
    }

    impl ScreenSource for ShiftingScreen {
        fn displays(&self) -> Vec<DisplayInfo> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.stable_calls {
                self.old.clone()
            } else {
                self.new.clone()
            }
        }

        fn capture(&self, _display: u32) -> RemoteResult<RawFrame> {
            Err(RemoteError::Platform("测试不采集".to_owned()))
        }
    }

    /// 读取剪贴板会长时间阻塞的假实现（X11 下别的进程持有 selection 的真实情形）。
    struct SlowClipboard {
        delay: Duration,
    }

    impl Clipboard for SlowClipboard {
        fn get(&self) -> RemoteResult<String> {
            std::thread::sleep(self.delay);
            Ok("慢".to_owned())
        }

        fn set(&self, _text: &str) -> RemoteResult<()> {
            Ok(())
        }
    }

    /// 记录注入事件（并经通道转发，供时序断言）。
    struct RecordingInput {
        notify: Mutex<Option<mpsc::UnboundedSender<InputEvent>>>,
        /// 锁屏调用次数（`Insert` 之外的 `InputSink::lock_screen`）。
        locks: AtomicU32,
    }

    impl RecordingInput {
        fn with_notify() -> (Arc<Self>, mpsc::UnboundedReceiver<InputEvent>) {
            let (tx, rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    notify: Mutex::new(Some(tx)),
                    locks: AtomicU32::new(0),
                }),
                rx,
            )
        }

        /// 本端被请求锁屏的次数。
        fn lock_count(&self) -> u32 {
            self.locks.load(Ordering::SeqCst)
        }

        fn record(&self, event: InputEvent) {
            if let Ok(guard) = self.notify.lock() {
                if let Some(tx) = guard.as_ref() {
                    let _ = tx.send(event);
                }
            }
        }
    }

    impl InputSink for RecordingInput {
        fn mouse_move(&self, x: i32, y: i32, display: u32) -> RemoteResult<()> {
            self.record(InputEvent::MouseMove { x, y, display });
            Ok(())
        }
        fn mouse_button(&self, button: MouseButton, pressed: bool) -> RemoteResult<()> {
            self.record(InputEvent::MouseButton { button, pressed });
            Ok(())
        }
        fn wheel(&self, dx: i32, dy: i32) -> RemoteResult<()> {
            self.record(InputEvent::Wheel { dx, dy });
            Ok(())
        }
        fn key(&self, event: KeyEvent) -> RemoteResult<()> {
            self.record(InputEvent::Key(event));
            Ok(())
        }
        fn lock_screen(&self) -> RemoteResult<()> {
            self.locks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// 空闲的传输状态机（这些用例不测文件传输）。
    fn transfer_hub() -> (TransferHub, mpsc::UnboundedReceiver<TransferEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (TransferHub::new(None, std::env::temp_dir(), tx), rx)
    }

    fn config(pending: Arc<PendingSessions>, mode: RemoteMode) -> ControlledConfig {
        config_with(pending, mode, CAPTURE_WATCHDOG)
    }

    /// 同 [`config`]，但可指定采集看门狗时限（看门狗用例需要极短的值，否则测试要等满 10s）。
    fn config_with(
        pending: Arc<PendingSessions>,
        mode: RemoteMode,
        capture_watchdog: Duration,
    ) -> ControlledConfig {
        ControlledConfig {
            session_id: "sess-1".to_owned(),
            local_rd_id: "222222222".to_owned(),
            mode,
            pending,
            encode: EncodeOptions::default(),
            native_resolution: false,
            capture_watchdog,
            unchanged_resend: UNCHANGED_RESEND_INTERVAL,
            stats: SessionStatsReporter::noop(),
            h264_encoder_factory: None,
            lock_on_disconnect: Arc::new(AtomicBool::new(false)),
        }
    }

    fn pending_with(token: &str) -> Arc<PendingSessions> {
        let pending = Arc::new(PendingSessions::new());
        pending.insert(PendingSession::new(
            "sess-1".to_owned(),
            token.to_owned(),
            "home-nas".to_owned(),
            RemoteMode::Control,
        ));
        pending
    }

    /// 协商规则：只有「本端有 H.264 编码器」且「控制器声明支持 H.264」才选 H.264。
    #[test]
    fn choose_codec_requires_both_sides_to_support_h264() {
        assert_eq!(choose_codec(Some((true, true)), true), CodecId::H264);
        // 本端没有编码器：即便对端支持也不能选。
        assert_eq!(choose_codec(Some((true, true)), false), CodecId::Vp9);
        // 对端不支持：本端有编码器也不选。
        assert_eq!(choose_codec(Some((false, true)), true), CodecId::Vp9);
        // 超时回退 VP9。
        assert_eq!(choose_codec(None, true), CodecId::Vp9);
        assert_eq!(choose_codec(None, false), CodecId::Vp9);
    }

    /// 显示器签名：与枚举顺序无关、空列表为空签名、尺寸/编号变化看得出来。
    #[test]
    fn display_signature_is_order_independent_and_tracks_geometry() {
        fn display(id: u32, width: u32, height: u32, name: &str, primary: bool) -> DisplayInfo {
            DisplayInfo {
                id,
                width,
                height,
                name: name.to_owned(),
                primary,
            }
        }
        let a = display(0, 1920, 1080, "内置", true);
        let b = display(1, 2560, 1440, "外接", false);

        // 枚举顺序不保证稳定：顺序不同但内容相同必须视为同一个签名。
        assert_eq!(
            display_signature(&[a.clone(), b.clone()]),
            display_signature(&[b.clone(), a.clone()])
        );
        // 空列表：空签名（调用方据此判定「枚举失败/没有显示器」）。
        assert!(display_signature(&[]).is_empty());
        // 尺寸变化必须改变签名（分辨率变化要重建编码器）。
        let resized = display(1, 1920, 1080, "外接", false);
        assert_ne!(
            display_signature(&[a.clone(), b.clone()]),
            display_signature(&[a.clone(), resized])
        );
        // 编号变化也必须改变签名（换了一块屏）。
        let renumbered = display(2, 2560, 1440, "外接", false);
        assert_ne!(
            display_signature(&[a.clone(), b.clone()]),
            display_signature(&[a.clone(), renumbered])
        );
        // name/primary 不参与签名：它们不影响采集几何，不该触发重建/IDR。
        let renamed = display(1, 2560, 1440, "改了个名", true);
        assert_eq!(
            display_signature(&[a, b]),
            display_signature(&[renamed, display(0, 1920, 1080, "x", false)])
        );
    }

    /// 协商读取器应返回控制器声明的能力，并在窗口内正常应答心跳。
    #[tokio::test]
    async fn codec_offer_is_read_and_heartbeat_answered_during_negotiation() {
        let (peer, us) = tokio::io::duplex(4096);
        let (read_half, _write_half) = tokio::io::split(Box::new(us) as BoxDuplex);
        let mut reader = FramedRead::new(read_half, SessionCodec::new());
        let mut peer_writer = FramedWrite::new(Box::new(peer) as BoxDuplex, SessionCodec::new());
        let (queues, mut rx) = OutboundQueues::new();
        let cancel = CancellationToken::new();

        // 控制器：先一条心跳（协商窗口内要应答），再发 offer。
        peer_writer
            .send(SessionFrame::control(SessionMessage::Heartbeat { t1_us: 0 }))
            .await
            .expect("发心跳");
        peer_writer
            .send(SessionFrame::control(SessionMessage::CodecOffer {
                h264: true,
                vp9: true,
            }))
            .await
            .expect("发 offer");

        let outcome = read_codec_offer(&mut reader, &queues, &cancel, WAIT)
            .await
            .expect("读 offer 应成功");
        match outcome {
            CodecOfferOutcome::Offer { h264, vp9 } => {
                assert!(h264 && vp9, "应原样带回控制器的能力声明");
            }
            _ => panic!("应收到 CodecOffer"),
        }
        // 心跳必须在协商阶段就被应答，否则控制器首个 RTT 样本会凭空消失。
        assert!(
            matches!(
                rx.ctrl.recv().await,
                Some(SessionFrame {
                    message: SessionMessage::HeartbeatAck { .. },
                    ..
                })
            ),
            "协商阶段收到 Heartbeat 必须回 HeartbeatAck"
        );
    }

    /// 对端不发 offer 时必须超时回退，而不是永久卡住。
    #[tokio::test]
    async fn codec_offer_timeout_is_reported() {
        let (peer, us) = tokio::io::duplex(1024);
        let (read_half, _write_half) = tokio::io::split(Box::new(us) as BoxDuplex);
        let mut reader = FramedRead::new(read_half, SessionCodec::new());
        let (queues, _rx) = OutboundQueues::new();
        let cancel = CancellationToken::new();

        let started = Instant::now();
        let outcome = read_codec_offer(&mut reader, &queues, &cancel, Duration::from_millis(30))
            .await
            .expect("超时不是错误");
        assert!(matches!(outcome, CodecOfferOutcome::Timeout), "应为 Timeout");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "超时路径应立即返回，实得 {:?}",
            started.elapsed()
        );
        // 对端保持存活但没有发任何消息。
        drop(peer);
    }

    /// 端到端：对端完成握手却**不发** `CodecOffer`，被控端必须超时回退 VP9 并继续会话。
    ///
    /// 用 `start_paused` 的虚拟时钟跳过真实的 5s 等待；断言的是「超时之后真的发出
    /// `CodecSelect{Vp9}`」这一接线，而不只是「一个纯函数返回了 Timeout」。
    #[tokio::test(start_paused = true)]
    async fn missing_codec_offer_falls_back_to_vp9_end_to_end() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config(pending_with("TOK"), RemoteMode::Control),
            Arc::new(EmptyScreenSource),
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        let (mut peer_reader, _peer_writer) =
            controller_peer_handshake_with(Box::new(a), &plan, "TOK", "sess-1", None).await;

        // 虚拟时钟会推进到 `CODEC_OFFER_TIMEOUT`；外层给一个远大于它的上限以免误判。
        let frame = tokio::time::timeout(Duration::from_secs(60), peer_reader.next())
            .await
            .expect("协商超时后应继续会话并发出 CodecSelect")
            .expect("流不应关闭")
            .expect("CodecSelect 应可解码");
        match frame.message {
            SessionMessage::CodecSelect { codec } => assert_eq!(codec, CodecId::Vp9),
            other => panic!("应收到 CodecSelect，实得 {other:?}"),
        }

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, ctld).await;
    }

    /// 显示器热插拔：会话中途列表变化时，被控端必须主动推 `DisplayList`（此前从未发送过）。
    ///
    /// 用 `start_paused` 跳过真实的 5s 复查间隔；控制器对端在循环里应答心跳，避免
    /// [`LIVENESS_TIMEOUT`] 先把会话结束掉。`FileTransfer` 模式不采集，排除画面路径的干扰。
    #[tokio::test(start_paused = true)]
    async fn display_hotplug_pushes_display_list_to_controller() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let screen = Arc::new(ShiftingScreen {
            calls: AtomicUsize::new(0),
            old: vec![DisplayInfo {
                id: 0,
                width: 1920,
                height: 1080,
                name: "内置".into(),
                primary: true,
            }],
            new: vec![
                DisplayInfo {
                    id: 0,
                    width: 1920,
                    height: 1080,
                    name: "内置".into(),
                    primary: false,
                },
                DisplayInfo {
                    id: 1,
                    width: 2560,
                    height: 1440,
                    name: "外接".into(),
                    primary: true,
                },
            ],
            stable_calls: 1,
        });
        let cancel = CancellationToken::new();
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config(pending_with("TOK"), RemoteMode::FileTransfer),
            screen,
            Arc::new(NullInputSink),
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        let (mut peer_reader, mut peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

        // 会话开始时的 `HelloAck` 已带旧列表；这里等中途那条 `DisplayList`。
        let displays = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match peer_reader.next().await {
                    Some(Ok(frame)) => match frame.message {
                        SessionMessage::Heartbeat { .. } => {
                            peer_writer
                                .send(SessionFrame::control(SessionMessage::HeartbeatAck {
                                    t2_us: 1,
                                    t3_us: 2,
                                }))
                                .await
                                .expect("回应心跳");
                        }
                        SessionMessage::DisplayList(list) => return list,
                        _ => {}
                    },
                    other => panic!("会话意外结束: {other:?}"),
                }
            }
        })
        .await
        .expect("5s 复查后应主动推送 DisplayList");

        assert_eq!(
            displays
                .iter()
                .map(|d| (d.id, d.width, d.height))
                .collect::<Vec<_>>(),
            vec![(0, 1920, 1080), (1, 2560, 1440)],
            "DisplayList 必须是新列表（含新插入的显示器）"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, ctld).await;
    }

    /// 被控端侧心跳存活检测：控制器只读帧、**从不回 `HeartbeatAck`** 时，被控端必须在
    /// [`LIVENESS_TIMEOUT`] 附近以 `deliberate: false` 结束（与控制器侧对称）。
    ///
    /// 中继停转发但 TCP 不断时，被控端同样收不到任何错误；没有这道判据，采集/编码会继续
    /// 空转、UI 仍显示在线。用 `start_paused` 跳过真实的 4s 等待。
    #[tokio::test(start_paused = true)]
    async fn heartbeat_timeout_ends_controlled_session_as_link_failure() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config(pending_with("TOK"), RemoteMode::Control),
            Arc::new(EmptyScreenSource),
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        let (mut peer_reader, peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

        // 控制器只排空、**从不回 HeartbeatAck**。writer 必须攥在手里：drop 掉会让被控端读到
        // EOF，会话将以「对端已关闭连接」提前结束，测到的就不是超时判据了。
        let peer = tokio::spawn(async move {
            let _keep_writer = peer_writer;
            while let Some(Ok(_frame)) = peer_reader.next().await {}
        });

        let end = tokio::time::timeout(LIVENESS_TIMEOUT + Duration::from_secs(3), ctld)
            .await
            .expect("心跳超时后应结束会话，而不是永久冻结")
            .expect("任务不应 panic")
            .expect("心跳超时属于正常收尾（不是 IO 错误）");
        assert_eq!(end.reason, "对端心跳超时（链路可能已中断）");
        assert!(
            !end.deliberate,
            "心跳超时是链路故障，deliberate 必须为 false"
        );
        peer.abort();
    }

    /// 作为「控制器」完成握手：写 `Hello`、读 `HelloAck`、装上加密，返回读写半边。
    async fn controller_peer_handshake(
        stream: BoxDuplex,
        plan: &ControllerHandshake,
        token: &str,
        session_id: &str,
    ) -> (PeerRead, PeerWrite) {
        controller_peer_handshake_with(stream, plan, token, session_id, Some((false, true))).await
    }

    /// 同 [`controller_peer_handshake`]，但可控制是否发送 `CodecOffer`
    /// （`None` 用于验证「对端不发 offer」的超时回退路径）。
    async fn controller_peer_handshake_with(
        stream: BoxDuplex,
        plan: &ControllerHandshake,
        token: &str,
        session_id: &str,
        offer: Option<(bool, bool)>,
    ) -> (PeerRead, PeerWrite) {
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = FramedRead::new(read_half, SessionCodec::new());
        let mut writer = FramedWrite::new(write_half, SessionCodec::new());

        // 会话循环先等 `Hello`（模拟建立阶段的流水线写出）。
        writer
            .get_mut()
            .write_all(&plan.hello_frame)
            .await
            .expect("写 Hello 应成功");
        writer.get_mut().flush().await.expect("flush Hello 应成功");

        let ack = reader.next().await.expect("应有 HelloAck").expect("HelloAck 可解码");
        let responder_pub = match ack.message {
            SessionMessage::HelloAck { x25519_pub, .. } => x25519_pub,
            other => panic!("期望 HelloAck，实得 {other:?}"),
        };
        let initiator_pub = plan.keypair.public_bytes();
        let responder_arr: [u8; 32] = responder_pub.as_slice().try_into().expect("公钥应为 32 字节");
        let shared = plan.keypair.diffie_hellman(&responder_pub).expect("ECDH 应成功");
        // 本端扮演控制器（发起方）：`initiator=true`。
        let keys = derive_session_keys(token, &shared, session_id, &initiator_pub, &responder_arr, true);
        reader
            .decoder_mut()
            .set_cipher(Cipher::new(keys.recv).expect("密钥长度合法"));
        writer
            .encoder_mut()
            .set_cipher(Cipher::new(keys.send).expect("密钥长度合法"));
        // 真实控制器在握手后立刻发 `CodecOffer`；协商窗口有超时，但测试不该为此等满 5s。
        // 默认声明「只支持 VP9」，需要测 H.264 的用例自行发能力更全的 offer。
        if let Some((h264, vp9)) = offer {
            writer
                .send(SessionFrame::control(SessionMessage::CodecOffer { h264, vp9 }))
                .await
                .expect("发送 CodecOffer 应成功");
        }
        (reader, writer)
    }

    /// 协议版本不匹配必须**明文干净拒绝**，而不是 bincode 解码失败导致的硬断链。
    ///
    /// 锁的不变量（见 `SESSION_PROTO_VERSION` 的文档与计划「硬约束 1」）：被控端要能先解出
    /// `Hello`、读到 `proto_version`、回一个 `HelloAck { accepted: false, reason }`。这条路径成立
    /// 的前提是 `Hello`/`HelloAck` 的字节形状**逐字节不变**（由 `session::tests` 的两条机械锁守着）
    /// ——改坏它们，混合版本就会退化成「连接被莫名断开」。
    ///
    /// 这里手工造一个 `proto_version` 不匹配的 `Hello` 喂给被控端：版本校验在鉴权**之前**，故
    /// `hmac` 填垃圾也无妨（校验根本走不到那一步）。
    #[tokio::test]
    async fn mismatched_proto_version_is_rejected_cleanly() {
        // 两个方向都要拒绝：v4（上一版）与 v6（未来版本）。v4↔v5 正是本次抬版本后真实会遇到的
        // 混合部署场景——两端都必须在握手阶段干净拒绝，而不是等 bincode 解码新变体时炸掉。
        assert_proto_version_rejected(SESSION_PROTO_VERSION.wrapping_sub(1)).await;
        assert_proto_version_rejected(SESSION_PROTO_VERSION.wrapping_add(1)).await;
    }

    /// 用给定的 `proto_version` 造 `Hello`，断言被控端回明文 `HelloAck{accepted:false}` 并以握手错误收场。
    async fn assert_proto_version_rejected(version: u16) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let bad_hello = SessionMessage::Hello {
            session_id: "sess-1".to_owned(),
            proto_version: version,
            controller_rd_id: "111111111".to_owned(),
            nonce: plan.nonce.clone(),
            hmac: vec![0u8; 32],
            x25519_pub: plan.keypair.public_bytes().to_vec(),
        };

        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config(pending_with("TOK"), RemoteMode::Control),
            Arc::new(EmptyScreenSource),
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));

        // 手工写 Hello（明文），随后读回明文 HelloAck。
        let (read_half, write_half) = tokio::io::split(Box::new(a));
        let mut reader = FramedRead::new(read_half, SessionCodec::new());
        let mut writer = FramedWrite::new(write_half, SessionCodec::new());
        let mut codec = SessionCodec::new();
        let hello_bytes = codec
            .encode_frame_bytes(&SessionFrame::control(bad_hello))
            .expect("编码 Hello");
        writer.get_mut().write_all(&hello_bytes).await.expect("写 Hello");
        writer.get_mut().flush().await.expect("flush Hello");

        // 关键断言①：对端回的是一个**明文可解**的 HelloAck（干净拒绝），不是解码失败。
        let ack = tokio::time::timeout(WAIT, reader.next())
            .await
            .expect("应收到 HelloAck（干净拒绝）")
            .expect("HelloAck 必须能明文解码")
            .expect("HelloAck 解码不应出错");
        match ack.message {
            SessionMessage::HelloAck { accepted, reason, .. } => {
                assert!(!accepted, "版本不匹配必须拒绝");
                let reason = reason.unwrap_or_default();
                assert!(reason.contains("proto"), "拒绝原因应点名版本，实得 {reason}");
            }
            other => panic!("期望 HelloAck，实得 {other:?}"),
        }

        // 关键断言②：被控端以握手错误收场（而不是 panic / IO 错误）。
        let result = tokio::time::timeout(WAIT, ctld)
            .await
            .expect("被控端应结束")
            .expect("任务不应 panic");
        assert!(
            matches!(result, Err(RemoteError::Handshake(_))),
            "应为握手错误，实得 {result:?}"
        );
    }

    /// 造一帧带明暗的 RGBA（内容随 `shade` 变化，用于制造「画面有变化」）。
    fn shaded(display: u32, w: u32, h: u32, shade: u8) -> RawFrame {
        let mut rgba = Vec::with_capacity((w * h) as usize * 4);
        for i in 0..(w * h) as usize {
            rgba.extend_from_slice(&[shade, u8::try_from(i % 251).unwrap_or(0), 40, 255]);
        }
        RawFrame {
            display,
            width: w,
            height: h,
            rgba,
            gpu: None,
            changed: true,
            changed_permille: 1000,
        }
    }

    /// 一条出站消息是否为关键帧视频包。
    fn is_keyframe(frame: &SessionFrame) -> bool {
        matches!(
            frame,
            SessionFrame {
                message: SessionMessage::VideoPacket { flags, .. },
                ..
            } if flags & VIDEO_FLAG_KEYFRAME != 0
        )
    }

    /// 轮询等待条件成立（有界；超时即断言失败）。
    async fn wait_for(mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(cond(), "等待条件超时");
    }

    /// 取出下一个视频包（跳过 `ScreenConfig` 等非画面帧），返回 `(flags, width, height)`。
    async fn next_video(out_rx: &mut mpsc::Receiver<SessionFrame>) -> (u8, u32, u32) {
        loop {
            match out_rx.recv().await {
                Some(SessionFrame {
                    message: SessionMessage::VideoPacket { flags, width, height, .. },
                    ..
                }) => return (flags, width, height),
                Some(_) => {}
                None => panic!("出站队列已关闭"),
            }
        }
    }

    /// **阶段 3 关键不变量**：码率/帧率变化必须**在线重配置**——不重建编码器、不插 IDR。
    ///
    /// 锁住本阶段最容易踩的坑：把码率放进「重建键」会让 AIMD 每秒插一个关键帧，正好把阶段 2
    /// 消掉的带宽尖峰引回来。几何不变时 `encode_loop` 只能走 `set_bitrate` / `set_fps`（补丁 6）。
    #[tokio::test]
    async fn rate_change_reconfigures_online_without_idr() {
        let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(8);
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
            CodecId::Vp9,
            vp9_encoder_factory(),
            UNCHANGED_RESEND_INTERVAL,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
        ));

        let hi = EncodeRates::ceiling(EncodeOptions::default());
        let lo = EncodeRates {
            bitrate_kbps: 500,
            fps: 8,
            ..hi
        };

        // 首帧：强制关键帧（建立参考链）。
        job_tx
            .send(EncodeJob {
                display: 0,
                rates: hi,
                force_keyframe: true,
                capture_us: 0,
                capture_ts_us: 0,
                frame: shaded(0, 64, 48, 10),
            })
            .await
            .expect("投递首帧");
        let (flags, w0, h0) = next_video(&mut out_rx).await;
        assert!(flags & VIDEO_FLAG_KEYFRAME != 0, "首帧应是关键帧");

        // 同几何、改码率与帧率、内容变化：必须是 P 帧（在线重配置，不插 IDR）。
        job_tx
            .send(EncodeJob {
                display: 0,
                rates: lo,
                force_keyframe: false,
                capture_us: 0,
                capture_ts_us: 0,
                frame: shaded(0, 64, 48, 40),
            })
            .await
            .expect("投递改码率帧");
        let (flags, w1, h1) = next_video(&mut out_rx).await;
        assert_eq!(
            flags & VIDEO_FLAG_KEYFRAME,
            0,
            "改码率/帧率不得插 IDR（否则闭环每秒一个关键帧）"
        );
        assert_eq!(
            flags & VIDEO_FLAG_CONFIG,
            0,
            "码率/帧率变化不是几何变化，不应带 CONFIG 标志"
        );
        assert_eq!((w1, h1), (w0, h0), "码率变化不应改变编码尺寸");

        drop(job_tx);
        let _ = tokio::time::timeout(WAIT, task).await.expect("编码任务应结束");
    }

    /// **阶段 3 关键不变量**：降编码分辨率（几何变化）必须**重建 + 重发 ScreenConfig + 强制 IDR**。
    ///
    /// 与上一条相对：这是唯一允许插 IDR 的路径（AIMD 的「大幅下调」）。
    #[tokio::test]
    async fn resolution_change_rebuilds_and_forces_keyframe() {
        let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(8);
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
            CodecId::Vp9,
            vp9_encoder_factory(),
            UNCHANGED_RESEND_INTERVAL,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
        ));

        let hi = EncodeRates::ceiling(EncodeOptions::default());
        // scale 减半 → 编码尺寸减半（几何变化）。
        let half = EncodeRates { scale: 0.5, ..hi };

        job_tx
            .send(EncodeJob {
                display: 0,
                rates: hi,
                force_keyframe: true,
                capture_us: 0,
                capture_ts_us: 0,
                frame: shaded(0, 64, 48, 10),
            })
            .await
            .expect("投递首帧");
        let (_, w0, h0) = next_video(&mut out_rx).await;
        assert_eq!((w0, h0), (64, 48));

        job_tx
            .send(EncodeJob {
                display: 0,
                rates: half,
                force_keyframe: false,
                capture_us: 0,
                capture_ts_us: 0,
                frame: shaded(0, 64, 48, 40),
            })
            .await
            .expect("投递降分辨率帧");
        // 先应收到新的 `ScreenConfig`（32×24），再是关键帧。
        let (flags, w1, h1) = next_video(&mut out_rx).await;
        assert_eq!((w1, h1), (32, 24), "降分辨率应改变编码尺寸");
        assert!(
            flags & VIDEO_FLAG_KEYFRAME != 0,
            "降分辨率（几何变化）必须强制关键帧"
        );

        drop(job_tx);
        let _ = tokio::time::timeout(WAIT, task).await.expect("编码任务应结束");
    }

    /// 假 H.264 编码器工厂：不产出真码流，只用于锁定「按协商 codec 建编码器」的接线。
    #[derive(Debug, Default)]
    struct FakeH264EncoderFactory;

    impl VideoEncoderFactory for FakeH264EncoderFactory {
        fn codec(&self) -> CodecId {
            CodecId::H264
        }

        fn create(
            &self,
            width: u32,
            height: u32,
            target_bitrate_kbps: u32,
            max_fps: u8,
        ) -> RemoteResult<Box<dyn VideoEncoderApi>> {
            Ok(Box::new(FakeH264Encoder {
                width,
                height,
                bitrate: target_bitrate_kbps,
                fps: u32::from(max_fps),
            }))
        }
    }

    /// 假 H.264 编码器：每帧固定吐 4 字节「码流」。
    struct FakeH264Encoder {
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
    }

    impl VideoEncoderApi for FakeH264Encoder {
        fn dimensions(&self) -> (u32, u32) {
            (self.width, self.height)
        }
        fn bitrate_kbps(&self) -> u32 {
            self.bitrate
        }
        fn fps(&self) -> u32 {
            self.fps
        }
        fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()> {
            self.bitrate = kbps;
            Ok(())
        }
        fn set_fps(&mut self, fps: u8) -> RemoteResult<()> {
            self.fps = u32::from(fps);
            Ok(())
        }
        fn encode(
            &mut self,
            _rgba: &[u8],
            _src_width: u32,
            _src_height: u32,
            force_keyframe: bool,
        ) -> RemoteResult<EncodedVideo> {
            Ok(EncodedVideo {
                data: vec![0, 0, 0, 1, 0x42],
                keyframe: force_keyframe,
            })
        }
    }

    /// **协商不变量**：编码任务必须用协商选定的 codec 工厂，并在 `ScreenConfig` /
    /// `VideoPacket` 里填**实际** codec（不是写死 VP9）。
    #[tokio::test]
    async fn encode_loop_uses_selected_codec_in_config_and_packets() {
        let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(8);
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
            CodecId::H264,
            Arc::new(FakeH264EncoderFactory),
            UNCHANGED_RESEND_INTERVAL,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
        ));

        job_tx
            .send(EncodeJob {
                display: 0,
                rates: EncodeRates::ceiling(EncodeOptions::default()),
                force_keyframe: true,
                capture_us: 0,
                capture_ts_us: 0,
                frame: shaded(0, 64, 48, 10),
            })
            .await
            .expect("投递首帧");

        // 首帧前必须先声明 `ScreenConfig`，且 codec 是协商选定的 H.264。
        match out_rx.recv().await {
            Some(SessionFrame {
                message: SessionMessage::ScreenConfig { codec, .. },
                ..
            }) => assert_eq!(codec, CodecId::H264, "ScreenConfig 必须填实际 codec"),
            _ => panic!("首个出站帧应为 ScreenConfig"),
        }
        match out_rx.recv().await {
            Some(SessionFrame {
                message: SessionMessage::VideoPacket { codec, flags, .. },
                ..
            }) => {
                assert_eq!(codec, CodecId::H264, "VideoPacket 必须填实际 codec");
                assert!(flags & VIDEO_FLAG_KEYFRAME != 0, "首帧应为关键帧");
            }
            _ => panic!("第二个出站帧应为关键帧"),
        }

        drop(job_tx);
        let _ = tokio::time::timeout(WAIT, task).await.expect("编码任务应结束");
    }

    /// 被背压跳过的那一帧若曾要求关键帧，请求**不能蒸发**：下一帧必须补上 IDR。
    ///
    /// 少了这条，控制器进入 `awaiting_keyframe` 后只请求一次 IDR，若那一帧恰好因出站队列满被跳过，
    /// 就要一直等到 `KEYFRAME_INTERVAL` 的周期性兜底（数百帧）才恢复——用户看到画面卡住十几秒。
    ///
    /// 构造是**确定性**的：出站队列容量 2，用 `Receiver::len()` / `Sender::capacity()` 作同步点
    /// ——编码任务从取到任务到下一次 `await` 之间的判断全是同步的（尺寸计算、去重、容量检查），
    /// 故这两个读数足以确定它的状态，不靠 sleep 猜时长。
    #[tokio::test]
    async fn skipped_forced_keyframe_is_retried_on_next_frame() {
        const JOB_CAP: usize = 4;
        let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(2);
        // 供测试自己往出站队列里塞占位帧（模拟背压），故先克隆一份。
        let filler = out_tx.clone();
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(JOB_CAP);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
            CodecId::Vp9,
            vp9_encoder_factory(),
            UNCHANGED_RESEND_INTERVAL,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
        ));
        let opts = EncodeRates::ceiling(EncodeOptions::default());
        let job = |frame: RawFrame, force: bool| EncodeJob {
            display: 0,
            rates: opts,
            force_keyframe: force,
            capture_us: 0,
            capture_ts_us: 0,
            frame,
        };
        let placeholder = || SessionFrame::control(SessionMessage::Heartbeat { t1_us: 0 });

        // 首帧：先下发 `ScreenConfig`，随后是关键帧。
        job_tx.send(job(shaded(0, 64, 48, 10), true)).await.expect("投递首帧");
        assert!(
            matches!(
                out_rx.recv().await,
                Some(SessionFrame { message: SessionMessage::ScreenConfig { .. }, .. })
            ),
            "首帧前应先下发 ScreenConfig"
        );
        let first = out_rx.recv().await.expect("应收到首帧");
        assert!(is_keyframe(&first), "首帧应是关键帧");

        // 用占位帧把出站队列塞满（容量 2）。
        filler.send(placeholder()).await.expect("占位 1");
        filler.send(placeholder()).await.expect("占位 2");
        assert_eq!(out_rx.len(), 2, "队列应已满");

        // 队列已满时投递一个**要求关键帧**的任务 → 编码任务只能在编码前跳过它。
        job_tx.send(job(shaded(0, 64, 48, 30), true)).await.expect("投递被跳过的帧");
        // 任务取走该任务并回到等待（跳过后立即 `continue` 到下一次 `recv`）→ 跳过已发生。
        wait_for(|| job_tx.capacity() == JOB_CAP).await;

        // 腾出位置后投递一个**不要求**关键帧的任务：它必须补上被跳过的那个 IDR。
        assert!(out_rx.recv().await.is_some(), "腾出一格");
        assert!(out_rx.recv().await.is_some(), "腾出另一格");
        job_tx.send(job(shaded(0, 64, 48, 40), false)).await.expect("投递下一帧");
        let retried = out_rx.recv().await.expect("应收到补发帧");
        assert!(
            is_keyframe(&retried),
            "被跳过的关键帧请求必须在下一帧补上（否则控制器会卡到周期性兜底）"
        );

        // 关掉任务队列让编码任务自然退出（不必等满 `WAIT`）。
        drop(job_tx);
        let _ = tokio::time::timeout(WAIT, task).await.expect("编码任务应结束");
    }

    /// GPU 帧的假平台载体（测试只关心「是不是 GPU 帧」，不关心纹理）。
    struct DummyGpuFrame;

    impl GpuFrame for DummyGpuFrame {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// **变化信号不变量**：GPU 帧在采集层报告「无变化」且不强制关键帧时必须跳过，
    /// 不能让 GPU 路径退回到「每拍都编码发送」；`force=true` 时仍必须产出 IDR。
    ///
    /// 这是现场「静态桌面带宽是两个数量级、帧率却不到 5」的直接回归锁：没有它，
    /// `frame.changed` 会被静默忽略，GPU 路径又回到每拍空编码。
    #[tokio::test]
    async fn unchanged_gpu_frame_is_skipped_unless_forced() {
        let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(8);
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
            CodecId::H264,
            Arc::new(FakeH264EncoderFactory),
            // 语义锁：间隔足够大时「无变化就跳过」必须保持原语义（本用例不断言保活）。
            Duration::from_secs(3600),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
        ));
        let rates = EncodeRates::ceiling(EncodeOptions::default());
        let gpu_job = |changed: bool, force: bool| EncodeJob {
            display: 0,
            rates,
            force_keyframe: force,
            capture_us: 0,
            capture_ts_us: 0,
            frame: RawFrame {
                display: 0,
                width: 64,
                height: 48,
                rgba: Vec::new(),
                gpu: Some(Arc::new(DummyGpuFrame)),
                changed,
                changed_permille: if changed { 1000 } else { 0 },
            },
        };

        // 首帧：强制关键帧，先拿到 `ScreenConfig` 再拿到画面包。
        job_tx.send(gpu_job(true, true)).await.expect("投递首帧");
        assert!(
            matches!(
                out_rx.recv().await,
                Some(SessionFrame { message: SessionMessage::ScreenConfig { .. }, .. })
            ),
            "首帧前应先下发 ScreenConfig"
        );
        assert!(is_keyframe(&out_rx.recv().await.expect("应收到首帧")));

        // 采集层说「没有新帧」且不强制：不转换、不编码、不发送。
        job_tx
            .send(gpu_job(false, false))
            .await
            .expect("投递静止 GPU 帧");
        wait_for(|| job_tx.capacity() == 4).await;
        assert!(
            out_rx.try_recv().is_err(),
            "无变化的 GPU 帧不应产出视频包（否则 GPU 路径又变成每拍空编码）"
        );

        // 有新帧：正常产出 P 帧。
        job_tx
            .send(gpu_job(true, false))
            .await
            .expect("投递变化 GPU 帧");
        let (flags, ..) = next_video(&mut out_rx).await;
        assert_eq!(flags & VIDEO_FLAG_KEYFRAME, 0, "有变化但未强制时应是 P 帧");

        // 无变化但强制（首帧/RequestKeyframe）：必须产出 IDR，不能被去重吞掉。
        job_tx
            .send(gpu_job(false, true))
            .await
            .expect("投递强制 GPU 帧");
        assert!(
            is_keyframe(&out_rx.recv().await.expect("应收到强制帧")),
            "force=true 时即使无变化也必须产出 IDR"
        );

        drop(job_tx);
        let _ = tokio::time::timeout(WAIT, task).await.expect("编码任务应结束");
    }

    /// **保活不变量**：`unchanged_resend = 0` 时，连续多帧 `changed == false` 也必须每帧
    /// 都编码发送，且都是普通 P 帧。
    ///
    /// 锁住两件事：① 保活补编不能被「无变化就跳过」的去重闸门吃掉（否则 250 ms 的延迟
    /// 上界只是纸面数字）；② 补编**不请求 IDR**——`force` 的语义不变，QoS 每秒动一次
    /// 就插 IDR 是之前专门修掉的回归。
    #[tokio::test]
    async fn unchanged_gpu_frames_are_resent_when_interval_is_zero() {
        let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(8);
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
            CodecId::H264,
            Arc::new(FakeH264EncoderFactory),
            Duration::ZERO,
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(false)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
        ));
        let rates = EncodeRates::ceiling(EncodeOptions::default());
        let gpu_job = |changed: bool, force: bool| EncodeJob {
            display: 0,
            rates,
            force_keyframe: force,
            capture_us: 0,
            capture_ts_us: 0,
            frame: RawFrame {
                display: 0,
                width: 64,
                height: 48,
                rgba: Vec::new(),
                gpu: Some(Arc::new(DummyGpuFrame)),
                changed,
                changed_permille: if changed { 1000 } else { 0 },
            },
        };

        // 首帧建立参考链。
        job_tx.send(gpu_job(true, true)).await.expect("投递首帧");
        assert!(
            matches!(
                out_rx.recv().await,
                Some(SessionFrame { message: SessionMessage::ScreenConfig { .. }, .. })
            ),
            "首帧前应先下发 ScreenConfig"
        );
        assert!(is_keyframe(&out_rx.recv().await.expect("应收到首帧")));

        // 连续三帧「无变化、未强制」：每一帧都必须被补编发送，且都不得是 IDR。
        for index in 0..3 {
            job_tx
                .send(gpu_job(false, false))
                .await
                .expect("投递静止 GPU 帧");
            let (flags, ..) = next_video(&mut out_rx).await;
            assert_eq!(
                flags & VIDEO_FLAG_KEYFRAME,
                0,
                "第 {index} 帧保活补编必须是普通 P 帧（不得带出 IDR）"
            );
        }

        drop(job_tx);
        let _ = tokio::time::timeout(WAIT, task).await.expect("编码任务应结束");
    }

    /// 慢采集不得饿死入站处理：采集在阻塞线程里跑，循环仍应及时处理控制器的输入。
    ///
    /// 修复前 `capture` 在会话 `select!` 的分支里**同步**执行，循环会卡在它上面直到 300ms 结束，
    /// 因此 200ms 窗口内收不到被注入的输入。
    #[tokio::test]
    async fn slow_capture_does_not_starve_inbound_processing() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");

        let (started_tx, mut started_rx) = mpsc::unbounded_channel::<()>();
        let screen = Arc::new(SlowScreen {
            started: started_tx,
            delay: Duration::from_millis(400),
        });
        let (input, mut input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();

        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config(pending_with("TOK"), RemoteMode::Control),
            screen,
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));

        let (mut peer_reader, mut peer_writer) = controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

        // 等一次采集开始（它随即在阻塞线程里 sleep 400ms）。
        tokio::time::timeout(WAIT, started_rx.recv())
            .await
            .expect("采集应开始")
            .expect("信号通道应存活");

        // 采集正阻塞：此刻发一条输入，它应在采集结束前（200ms 窗口内）被处理。
        peer_writer
            .send(SessionFrame::control(SessionMessage::Input(InputEvent::MouseMove {
                x: 1,
                y: 2,
                display: 0,
            })))
            .await
            .expect("发送输入应成功");
        let applied = tokio::time::timeout(Duration::from_millis(200), input_rx.recv()).await;
        assert!(
            applied.is_ok(),
            "采集阻塞期间循环仍应处理入站消息（修复前会被慢采集饿死）"
        );

        cancel.cancel();
        let drain = tokio::spawn(async move { while peer_reader.next().await.is_some() {} });
        let _ = tokio::time::timeout(WAIT, ctld).await.expect("被控端应结束");
        let _ = drain.await;
        drop(peer_writer);
    }

    /// 测试用 duplex 包装：统计写侧 `poll_write` 返回 `Pending` 的次数。
    ///
    /// 这是「发送确实被背压卡住」的**机械信号**：`Pending` 只在写缓冲已满时出现，等价于
    /// `send().await` 会挂起。用它替代「帧必然比容量大」的注释推理——若哪天帧小到填不满缓冲，
    /// 计数为 0，断言立即失败，而不是让用例在什么都没锁住的情况下静默通过。
    struct BackpressureProbe {
        inner: tokio::io::DuplexStream,
        blocked: Arc<AtomicUsize>,
    }

    impl AsyncRead for BackpressureProbe {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for BackpressureProbe {
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
            if poll.is_pending() {
                this.blocked.fetch_add(1, Ordering::SeqCst);
            }
            poll
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    /// 取消必须能打断被控端编码任务里**被背压卡住的发送**。
    ///
    /// 阶段 2 起画面发送不再发生在会话 `select!` 循环里，而在独立的编码任务中（[`encode_loop`]
    /// → [`send_video`]）。这条回归护栏的语义因此变成：编码任务阻塞在出站队列的 `send` 上时，
    /// 取消仍必须让整个 `run_controlled` 在有限时间内返回（`send_video` 对取消敏感）。
    ///
    /// 「发送被阻塞」由**构造 + 机械断言**共同保证，不靠注释推理：
    /// - duplex 容量只有 1024 字节，而噪声整帧的 VP9 关键帧实测 **861,138 字节**（800×600
    ///   噪声，见 [`NoiseScreen`]），远超容量；握手/心跳/ScreenConfig 等控制帧都在 1024 以内，
    ///   故第一个撞上背压的必是画面帧；
    /// - 对端自始至终不读（读端一消费，被卡住的发送就能推进，测试就测不到取消了）；
    /// - [`BackpressureProbe`] 直接观测写侧 `poll_write` 是否返回 `Pending`——**这就是「发送
    ///   已挂起」的信号**；帧若小到填不满缓冲，下面的断言会超时失败而非静默通过。
    ///
    /// 收尾的 `Bye` 只能等满 `BYE_TIMEOUT`（2s），返回时间以 3s 为上界。
    #[tokio::test]
    async fn cancel_interrupts_blocked_send_on_controlled() {
        let (a, b) = tokio::io::duplex(1024);
        // 包一层探针：写侧一旦撞上满缓冲（`poll_write` 返回 `Pending`）即计数。
        let blocked = Arc::new(AtomicUsize::new(0));
        let probe = BackpressureProbe {
            inner: b,
            blocked: Arc::clone(&blocked),
        };
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");

        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();
        let ctld = tokio::spawn(run_controlled(
            Box::new(probe),
            config(pending_with("TOK"), RemoteMode::Control),
            Arc::new(NoiseScreen),
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));

        let (_peer_reader, _peer_writer) = controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

        // 机械验证「发送确实被阻塞」：等写侧撞上背压。若帧填不满 1024 字节缓冲，这里会超时失败。
        tokio::time::timeout(WAIT, async {
            while blocked.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("写侧应撞上背压：首帧 VP9 关键帧必须填满 1024 字节缓冲，否则用例根本没覆盖到「取消打断被阻塞的发送」");

        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), ctld)
            .await
            .expect("取消后被控端应脱身并返回（修复前会永久阻塞在发送里）")
            .expect("任务不应 panic");
        assert!(result.is_ok(), "取消应正常收尾，实得 {result:?}");
    }

    /// 采集侧 panic 不得让画面永久冻结：看门狗到点即丢弃那次采集、重建基线并继续采集。
    ///
    /// 回归用例：把采集挪进阻塞线程后，「在途」状态只能靠结果回传解除，而 panic 后的结果永不
    /// 回传——若不设看门狗，后续采集会被**永久**跳过，会话却照旧活着（比挪走之前的「挂住即
    /// 结束会话」更糟：用户看到的是画面冻住、没有任何错误）。
    #[tokio::test]
    async fn panicking_capture_is_recovered_by_watchdog() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");

        let screen = Arc::new(PanicOnceScreen {
            calls: AtomicUsize::new(0),
        });
        let run_screen: Arc<dyn ScreenSource> = screen.clone();
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();

        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config_with(pending_with("TOK"), RemoteMode::Control, Duration::from_millis(100)),
            run_screen,
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        // 对端读半边必须活着（`_` 前缀的具名绑定不会被立即丢弃）：它一断，写失败会让会话先结束。
        let (_peer_reader, _peer_writer) = controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

        // panic 那次之后再有一次采集调用，即说明看门狗已解除在途状态、循环继续跑。
        let recovered = tokio::time::timeout(WAIT, async {
            while screen.calls() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            recovered.is_ok(),
            "采集 panic 后应由看门狗恢复（实得 {} 次采集）",
            screen.calls()
        );

        cancel.cancel();
        let result = tokio::time::timeout(WAIT, ctld)
            .await
            .expect("取消后应结束")
            .expect("任务不应 panic");
        assert!(result.is_ok(), "取消应正常收尾，实得 {result:?}");
    }

    /// 采集彻底挂死时必须有上限：连续 [`MAX_CAPTURE_TIMEOUTS`] 次超时即结束会话、把原因交给 UI，
    /// 而不是无限重试（每次重试都再挂一个阻塞线程，最终会填满 `spawn_blocking` 的线程池）。
    #[tokio::test]
    async fn hanging_capture_ends_session_after_max_timeouts() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");

        let screen = Arc::new(HangingScreen {
            calls: AtomicUsize::new(0),
        });
        let run_screen: Arc<dyn ScreenSource> = screen.clone();
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();

        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config_with(pending_with("TOK"), RemoteMode::Control, Duration::from_millis(50)),
            run_screen,
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        let (_peer_reader, _peer_writer) = controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

        let result = tokio::time::timeout(WAIT, ctld)
            .await
            .expect("挂死的采集应被看门狗终止（不得无限重试）")
            .expect("任务不应 panic");
        let err = result.expect_err("连续超时后应结束会话");
        assert!(err.to_string().contains("超时"), "错误应说明采集超时，实得 {err}");
        assert!(
            screen.calls() >= MAX_CAPTURE_TIMEOUTS as usize,
            "应重试到上限为止（实得 {} 次）",
            screen.calls()
        );
    }

    /// 读取剪贴板阻塞时必须能被取消打断：否则「本机剪贴板被别的进程占用」会让会话卡在收尾上，
    /// 取消按钮形同虚设（`send_cancellable` 管不到 handler 内部的阻塞 `.await`）。
    #[tokio::test]
    async fn cancel_interrupts_blocked_clipboard_read() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");

        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();
        // 采集源为空（本用例不关心画面），剪贴板读取故意慢过收尾窗口。
        let clipboard = Arc::new(SlowClipboard {
            delay: Duration::from_secs(5),
        });

        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config(pending_with("TOK"), RemoteMode::Control),
            Arc::new(EmptyScreenSource),
            input,
            Some(clipboard),
            transfer_hub().0,
            cancel.clone(),
        ));
        let (_peer_reader, mut peer_writer) = controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

        peer_writer
            .send(SessionFrame::control(SessionMessage::ClipboardRequest))
            .await
            .expect("发送剪贴板请求应成功");
        // 让循环走进那次阻塞读取（5s）之后再取消。
        tokio::time::sleep(Duration::from_millis(150)).await;

        let started = Instant::now();
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), ctld)
            .await
            .expect("取消应能打断阻塞的剪贴板读取（修复前会一直等到读取返回）")
            .expect("任务不应 panic");
        assert!(result.is_ok(), "取消应正常收尾，实得 {result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "收尾不应等满剪贴板读取（实得 {:?}）",
            started.elapsed()
        );
    }

    // ───────────────── 锁屏（协议 v7） ─────────────────

    /// 等被控端完成编解码协商（看到 `CodecSelect` 即说明 offer 已被处理）。
    async fn wait_codec_select(peer_reader: &mut PeerRead) {
        let frame = tokio::time::timeout(WAIT, peer_reader.next())
            .await
            .expect("协商完成前应收到 CodecSelect")
            .expect("流不应关闭")
            .expect("CodecSelect 应可解码");
        assert!(
            matches!(frame.message, SessionMessage::CodecSelect { .. }),
            "首帧应为 CodecSelect，实得 {:?}",
            frame.message
        );
    }

    /// 等被控端**主循环**真正跑起来（首个 `Heartbeat` 立即到期）。
    ///
    /// 收尾守卫在编解码协商之后、主循环之前创建，故看到主循环发出的帧就证明守卫已经建立——
    /// 比固定睡眠更可靠（不会在负载高的机器上把取消抢在守卫创建之前）。
    async fn wait_loop_started(peer_reader: &mut PeerRead) {
        loop {
            let frame = tokio::time::timeout(WAIT, peer_reader.next())
                .await
                .expect("主循环应发出首帧")
                .expect("流不应关闭")
                .expect("帧应可解码");
            if matches!(frame.message, SessionMessage::Heartbeat { .. }) {
                return;
            }
        }
    }

    /// 仅观看模式下 `LockScreen` 必须被忽略：它不该改变远端机器状态。
    #[tokio::test]
    async fn lock_screen_request_is_ignored_outside_control_mode() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config(pending_with("TOK"), RemoteMode::ViewOnly),
            Arc::new(EmptyScreenSource),
            Arc::clone(&input) as Arc<dyn InputSink>,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        let (mut peer_reader, mut peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;
        wait_codec_select(&mut peer_reader).await;

        peer_writer
            .send(SessionFrame::control(SessionMessage::LockScreen))
            .await
            .expect("发送锁屏请求应成功");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(input.lock_count(), 0, "仅观看模式不得执行锁屏");

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, ctld).await;
        drop(peer_writer);
    }

    /// 完全控制模式下 `LockScreen` 应执行一次。
    #[tokio::test]
    async fn lock_screen_request_locks_in_control_mode() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config(pending_with("TOK"), RemoteMode::Control),
            Arc::new(EmptyScreenSource),
            Arc::clone(&input) as Arc<dyn InputSink>,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        let (mut peer_reader, mut peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;
        wait_codec_select(&mut peer_reader).await;

        peer_writer
            .send(SessionFrame::control(SessionMessage::LockScreen))
            .await
            .expect("发送锁屏请求应成功");
        tokio::time::timeout(Duration::from_secs(2), async {
            while input.lock_count() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("完全控制模式下应执行锁屏");
        assert_eq!(input.lock_count(), 1, "一次请求只应锁屏一次");

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, ctld).await;
        drop(peer_writer);
    }

    /// 跑一场已建立（握手 + 协商完成）的会话，返回会话结束时累计的锁屏次数。
    ///
    /// 守住「守卫在下行路径上确实被创建」这一点：等待 `CodecSelect` 之后再多给一点时间，
    /// 让 `run_controlled` 走到创建守卫那一行（它在协商完成之后、主循环之前）。
    async fn auto_lock_count_on_session_end(lock_on_disconnect: bool) -> u32 {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();
        let mut config = config(pending_with("TOK"), RemoteMode::Control);
        config.lock_on_disconnect = Arc::new(AtomicBool::new(lock_on_disconnect));
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config,
            Arc::new(EmptyScreenSource),
            Arc::clone(&input) as Arc<dyn InputSink>,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        let (mut peer_reader, peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;
        wait_codec_select(&mut peer_reader).await;
        // 主循环起来 = 收尾守卫已创建；此刻取消，结束路径必然经过守卫的 `Drop`。
        wait_loop_started(&mut peer_reader).await;

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, ctld)
            .await
            .expect("取消后被控端应结束");
        drop(peer_writer);
        input.lock_count()
    }

    /// 会话结束（这里是取消路径，网络断开同样走主循环出口）后按偏好自动锁屏。
    #[tokio::test]
    async fn auto_lock_fires_when_an_established_session_ends() {
        assert_eq!(
            auto_lock_count_on_session_end(true).await,
            1,
            "开启偏好后，已建立会话结束应锁屏一次"
        );
    }

    /// 偏好关闭时不得锁屏。
    #[tokio::test]
    async fn auto_lock_does_not_fire_when_disabled() {
        assert_eq!(
            auto_lock_count_on_session_end(false).await,
            0,
            "偏好关闭时会话结束不得锁屏"
        );
    }

    // ───────────────── 脏矩形补丁模式（阶段 8） ─────────────────

    /// 造一段确定性伪随机 NV12（压不动，用于验证「压缩没有收益就存原始」）。
    fn pseudo_nv12(w: u32, h: u32) -> Vec<u8> {
        let w = usize::try_from(w).expect("宽");
        let h = usize::try_from(h).expect("高");
        let len = w * h * 3 / 2;
        let mut x: u32 = 0x1234_5678;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.push(u8::try_from(x >> 24).unwrap_or(0));
        }
        out
    }

    /// 补丁打包：坐标原样保留、可压的用 zstd（能解回原文）、压不动的存原始 NV12。
    #[test]
    fn build_region_update_compresses_rects_and_keeps_coordinates() {
        let mut compressor = zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL).expect("压缩器");
        let zero_nv12 = vec![0u8; 32 * 32 * 3 / 2];
        let tiny = vec![9u8, 8, 7, 6, 5, 4];
        let patches = CapturedPatches {
            display: 3,
            width: 64,
            height: 48,
            changed_permille: 12,
            rects: vec![
                ScreenPatch {
                    x: 2,
                    y: 4,
                    width: 32,
                    height: 32,
                    nv12: zero_nv12.clone(),
                },
                ScreenPatch {
                    x: 10,
                    y: 20,
                    width: 2,
                    height: 2,
                    nv12: tiny.clone(),
                },
            ],
        };
        let msg = build_region_update(&mut compressor, 3, &patches, 7_000).expect("应在预算内");
        let SessionMessage::RegionUpdate {
            display,
            width,
            height,
            rects,
            capture_ts_us,
        } = msg
        else {
            panic!("应为 RegionUpdate，实得 {msg:?}");
        };
        assert_eq!((display, width, height), (3, 64, 48), "目标帧缓冲尺寸应原样带回");
        assert_eq!(capture_ts_us, 7_000, "采集时刻应原样带回");
        assert_eq!(rects.len(), 2);
        // 坐标/尺寸原样保留（脏矩形坐标是编码尺寸坐标系）。
        assert_eq!(
            (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
            (2, 4, 32, 32)
        );
        assert_eq!(
            (rects[1].x, rects[1].y, rects[1].width, rects[1].height),
            (10, 20, 2, 2)
        );
        // 全零 NV12 高度可压：带 ZSTD 标志，且能解回原文。
        assert_eq!(
            rects[0].flags & REGION_FLAG_ZSTD,
            REGION_FLAG_ZSTD,
            "全零 NV12 应压得动"
        );
        let restored = zstd::bulk::decompress(&rects[0].data, zero_nv12.len()).expect("解压应成功");
        assert_eq!(restored, zero_nv12, "zstd 往返必须无损");
        // 2×2 的小块压不下去（zstd 帧头比数据还大）：存原始 NV12，flags 为 0。
        assert_eq!(rects[1].flags, 0);
        assert_eq!(rects[1].data, tiny);
    }

    /// 空矩形 = 保活：仍然产出一条 `RegionUpdate`（矩形列表为空）。
    #[test]
    fn build_region_update_keeps_empty_keepalive() {
        let mut compressor = zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL).expect("压缩器");
        let patches = CapturedPatches {
            display: 0,
            width: 2,
            height: 2,
            changed_permille: 0,
            rects: Vec::new(),
        };
        match build_region_update(&mut compressor, 0, &patches, 7_000).expect("空补丁也应构造") {
            SessionMessage::RegionUpdate {
                width,
                height,
                rects,
                ..
            } => {
                assert_eq!((width, height), (2, 2));
                assert!(rects.is_empty(), "保活补丁的矩形列表为空");
            }
            other => panic!("应为 RegionUpdate，实得 {other:?}"),
        }
    }

    /// 字节预算闸门：压缩后总字节超过 [`PATCH_MAX_TOTAL_BYTES`] 时必须放弃补丁。
    #[test]
    fn build_region_update_rejects_oversized_payload() {
        let mut compressor = zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL).expect("压缩器");
        // 单块不可压缩的 512×512 NV12 = 393216 字节，已超过 256 KiB 预算。
        let patches = CapturedPatches {
            display: 0,
            width: 512,
            height: 512,
            changed_permille: 300,
            rects: vec![ScreenPatch {
                x: 0,
                y: 0,
                width: 512,
                height: 512,
                nv12: pseudo_nv12(512, 512),
            }],
        };
        assert!(
            build_region_update(&mut compressor, 0, &patches, 7_000).is_none(),
            "超预算应放弃补丁（回到全帧）"
        );
    }

    /// 矩形数闸门：超过 [`MAX_REGION_RECTS`] 时必须放弃补丁。
    #[test]
    fn build_region_update_rejects_too_many_rects() {
        let mut compressor = zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL).expect("压缩器");
        let rects = (0..=MAX_REGION_RECTS)
            .map(|i| ScreenPatch {
                x: u32::try_from(i).expect("下标") * 2,
                y: 0,
                width: 2,
                height: 2,
                nv12: vec![1, 2, 3, 4, 5, 6],
            })
            .collect();
        let patches = CapturedPatches {
            display: 0,
            width: 128,
            height: 2,
            changed_permille: 50,
            rects,
        };
        assert!(
            build_region_update(&mut compressor, 0, &patches, 7_000).is_none(),
            "矩形数超上限应放弃补丁（回到全帧）"
        );
    }

    /// 进入判据必须**连续**低活跃；一帧高活跃把计数清零。
    #[test]
    fn patch_mode_entry_needs_consecutive_quiet_captures() {
        let mut mode = PatchMode::new();
        let now = Instant::now();
        assert!(!mode.observe_video(PATCH_ENTER_PERMILLE, now));
        assert!(!mode.observe_video(PATCH_ENTER_PERMILLE, now));
        // 一帧高活跃打断连续计数。
        assert!(!mode.observe_video(PATCH_ENTER_PERMILLE + 1, now));
        assert!(!mode.observe_video(PATCH_ENTER_PERMILLE, now));
        assert!(!mode.observe_video(PATCH_ENTER_PERMILLE, now));
        assert!(
            mode.observe_video(PATCH_ENTER_PERMILLE, now),
            "连续 {PATCH_ENTER_TICKS} 次低活跃才应请求基准 IDR"
        );
        assert!(mode.enter_pending);
        // 已在等待基准帧时不重复触发。
        assert!(!mode.observe_video(0, now));
        assert_eq!(mode.mode, ScreenMode::Video, "基准未发出前不得切模式");
    }

    /// 补丁带宽兜底：单批超一个窗口预算直接拒绝；窗口到点时按实际出码码率判定。
    #[test]
    fn patch_bandwidth_guard_rejects_over_budget_windows() {
        let t0 = Instant::now();
        let budget = 1000_u32; // kbps → 一个窗口 125_000 B
        let mut mode = PatchMode::new();
        mode.patch_window_start = t0;
        mode.patch_window_bytes = 0;
        // 单批就超过一个窗口的预算：拒绝（防止一个巨大补丁打满链路）。
        assert!(!mode.admit_patch_bytes(200_000, t0, budget));
        // 正常批次：接受并累计。
        assert!(mode.admit_patch_bytes(60_000, t0 + Duration::from_millis(10), budget));
        assert!(mode.admit_patch_bytes(60_000, t0 + Duration::from_millis(20), budget));
        // 窗口到点：120 KB / 1.01s ≈ 950 kbps < 1000 → 接受并重置窗口。
        let t1 = t0 + Duration::from_secs(1) + Duration::from_millis(10);
        assert!(mode.admit_patch_bytes(10_000, t1, budget));
        // 下一个窗口累计 190 KB / ~1.04s ≈ 1461 kbps > 1000 → 拒绝。
        assert!(mode.admit_patch_bytes(60_000, t1 + Duration::from_millis(10), budget));
        assert!(mode.admit_patch_bytes(60_000, t1 + Duration::from_millis(20), budget));
        assert!(mode.admit_patch_bytes(60_000, t1 + Duration::from_millis(30), budget));
        let t2 = t1 + Duration::from_secs(1) + Duration::from_millis(50);
        assert!(
            !mode.admit_patch_bytes(0, t2, budget),
            "上一窗口出码超过预算时必须回全帧"
        );
    }

    /// 补丁目标尺寸必须与视频路径的**双重换算**一致（先按 `max_width` 封顶，再乘 scale）。
    ///
    /// 这条锁住的是 4K + 非 1.0 缩放的错位：直接用「原始宽 × scale 再封顶」会得到
    /// 1920×1080，而视频路径得到 960×540，控制端会因尺寸不符拒绝所有补丁。
    #[test]
    fn encode_target_dims_matches_the_video_path_double_step() {
        assert_eq!(encode_target_dims(1920, 1080, 1.0, MAX_ENCODE_WIDTH), (1920, 1080));
        assert_eq!(encode_target_dims(1920, 1080, 0.5, MAX_ENCODE_WIDTH), (960, 540));
        // 4K：先封顶到 1920×1080，再乘 scale。
        assert_eq!(
            encode_target_dims(3840, 2160, 1.0, MAX_ENCODE_WIDTH),
            (1920, 1080)
        );
        assert_eq!(
            encode_target_dims(3840, 2160, 0.5, MAX_ENCODE_WIDTH),
            (960, 540)
        );
        assert_eq!(
            encode_target_dims(3840, 2160, 0.75, MAX_ENCODE_WIDTH),
            (1440, 810)
        );
        // 超宽屏（5120）：同样先封顶。
        assert_eq!(
            encode_target_dims(5120, 2880, 0.5, MAX_ENCODE_WIDTH),
            (960, 540)
        );
        // 原生分辨率：4K 原生保真；0.5 缩放仍得到 1920（与视频路径一致）。
        assert_eq!(
            encode_target_dims(3840, 2160, 1.0, NATIVE_MAX_ENCODE_WIDTH),
            (3840, 2160)
        );
        assert_eq!(
            encode_target_dims(3840, 2160, 0.5, NATIVE_MAX_ENCODE_WIDTH),
            (1920, 1080)
        );
    }

    /// 有效上限只对**硬件 H.264 + 显式开启**放宽；开了选项但只有 VP9 仍是 1920。
    ///
    /// 这是「原生分辨率」的完整真值表：VP9 软件解 4K 的成本不允许被这个开关带进来。
    #[test]
    fn effective_max_encode_width_table() {
        assert_eq!(effective_max_encode_width(CodecId::H264, true), 3840);
        assert_eq!(effective_max_encode_width(CodecId::Vp9, true), MAX_ENCODE_WIDTH);
        assert_eq!(effective_max_encode_width(CodecId::H264, false), MAX_ENCODE_WIDTH);
        assert_eq!(effective_max_encode_width(CodecId::Vp9, false), MAX_ENCODE_WIDTH);
        assert_eq!(NATIVE_MAX_ENCODE_WIDTH, 3840);
    }

    /// 原生分辨率只翻**码率上限**：帧率/缩放原样，封顶 30 Mbps。
    #[test]
    fn native_resolution_doubles_only_the_bitrate_ceiling() {
        let hd = EncodeOptions::default();
        let native = session_encode_options(hd, true);
        assert_eq!(native.target_bitrate_kbps, hd.target_bitrate_kbps * 2);
        assert_eq!(native.max_fps, hd.max_fps, "帧率不动");
        assert!((native.scale - hd.scale).abs() < f32::EPSILON, "缩放不动");
        // 非原生：逐字段原样（不悄悄改上限）。
        assert_eq!(session_encode_options(hd, false), hd);
        // 各档位都翻倍且不越过封顶。
        for preset in crate::screen::QualityPreset::all() {
            let doubled = session_encode_options(preset.options(), true);
            assert_eq!(
                doubled.target_bitrate_kbps,
                preset.options().target_bitrate_kbps * 2
            );
            assert!(doubled.target_bitrate_kbps <= crate::screen::NATIVE_MAX_BITRATE_KBPS);
        }
    }

    /// 顺序不变量：基准关键帧**真正写出**后才进入补丁模式；等不到就放弃并清零计数。
    #[test]
    fn patch_mode_enters_only_after_base_keyframe_and_abandons_on_timeout() {
        let t0 = Instant::now();
        let mut mode = PatchMode::new();
        for _ in 0..PATCH_ENTER_TICKS {
            mode.observe_video(0, t0);
        }
        assert_eq!(mode.poll_entry(false, t0 + Duration::from_millis(1)), None);
        assert_eq!(mode.mode, ScreenMode::Video, "基准未发出前不得切模式");
        assert_eq!(mode.poll_entry(true, t0 + Duration::from_millis(2)), Some(true));
        assert_eq!(mode.mode, ScreenMode::Patches);
        assert!(mode.last_patch_sent.is_some(), "进入时保活计时从此刻起算");

        // 超时路径：基准迟迟发不出去（例如出站队列被背压卡住）。
        let mut mode = PatchMode::new();
        for _ in 0..PATCH_ENTER_TICKS {
            mode.observe_video(0, t0);
        }
        assert_eq!(mode.poll_entry(false, t0 + PATCH_BASE_TIMEOUT), Some(false));
        assert_eq!(mode.mode, ScreenMode::Video);
        assert_eq!(mode.enter_streak, 0, "放弃后连续计数必须清零");
    }

    /// 支持补丁的假采集源：视频采集报低活跃，补丁采集返回一块固定脏矩形。
    struct PatchyScreen {
        patch_calls: AtomicUsize,
    }

    impl ScreenSource for PatchyScreen {
        fn displays(&self) -> Vec<DisplayInfo> {
            vec![DisplayInfo {
                id: 0,
                width: 32,
                height: 24,
                name: "patchy".into(),
                primary: true,
            }]
        }

        fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
            let mut frame = solid(display, 32, 24);
            frame.changed_permille = 8;
            Ok(frame)
        }

        fn supports_patches(&self) -> bool {
            true
        }

        fn capture_patches(
            &self,
            display: u32,
            scale: f32,
            max_width: u32,
        ) -> RemoteResult<Option<CapturedPatches>> {
            self.patch_calls.fetch_add(1, Ordering::SeqCst);
            let (width, height) = encode_dimensions(32, 24, scale, max_width);
            Ok(Some(CapturedPatches {
                display,
                width,
                height,
                changed_permille: 8,
                rects: vec![ScreenPatch {
                    x: 0,
                    y: 0,
                    width: 16,
                    height: 16,
                    nv12: vec![7u8; 16 * 16 * 3 / 2],
                }],
            }))
        }
    }

    /// 从对端读半边取下一条 `SessionMessage`（超时/断链即断言失败）。
    async fn next_message(reader: &mut PeerRead) -> SessionMessage {
        tokio::time::timeout(WAIT, reader.next())
            .await
            .expect("等消息超时")
            .expect("流不应关闭")
            .expect("消息应可解码")
            .message
    }

    /// 端到端：低活跃 + H.264 + 采集源支持补丁 → 先发基准关键帧，再发 `RegionUpdate`；
    /// 控制器 `RequestKeyframe` 后必须退回全帧并重发 IDR。
    #[tokio::test]
    async fn patch_mode_sends_region_update_after_base_keyframe_and_exits_on_request() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let screen = Arc::new(PatchyScreen {
            patch_calls: AtomicUsize::new(0),
        });
        let run_screen: Arc<dyn ScreenSource> = screen.clone();
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();

        let mut cfg = config(pending_with("TOK"), RemoteMode::Control);
        cfg.h264_encoder_factory = Some(Arc::new(FakeH264EncoderFactory));
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            cfg,
            run_screen,
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        // 声明支持 H.264，协商结果才会是 H.264（补丁模式的前提）。
        let (mut peer_reader, mut peer_writer) =
            controller_peer_handshake_with(Box::new(a), &plan, "TOK", "sess-1", Some((true, true))).await;

        // 读到第一块补丁为止，沿途统计关键帧：补丁之前必须已经有两个 IDR（会话首帧 + 进入基准）。
        let mut keyframes = 0u32;
        let (width, height, rects) = tokio::time::timeout(WAIT, async {
            loop {
                match next_message(&mut peer_reader).await {
                    SessionMessage::VideoPacket { flags, .. } => {
                        if flags & VIDEO_FLAG_KEYFRAME != 0 {
                            keyframes += 1;
                        }
                    }
                    SessionMessage::RegionUpdate {
                        width,
                        height,
                        rects,
                        ..
                    } => return (width, height, rects),
                    _ => {}
                }
            }
        })
        .await
        .expect("低活跃画面应进入补丁模式并发出 RegionUpdate");

        assert_eq!((width, height), (32, 24), "补丁坐标系是编码尺寸");
        assert!(!rects.is_empty(), "本拍有脏矩形");
        assert!(
            keyframes >= 2,
            "补丁之前必须已发出会话首帧与进入基准两个关键帧（实得 {keyframes}）"
        );
        assert!(
            screen.patch_calls.load(Ordering::SeqCst) >= 1,
            "应真的走补丁采集，而不是退回全帧"
        );

        // 控制器请求关键帧（参考丢失/队列溢出）→ 被控端退出补丁模式并重发全帧 IDR。
        peer_writer
            .send(SessionFrame::control(SessionMessage::RequestKeyframe {
                display: 0,
            }))
            .await
            .expect("发送关键帧请求应成功");
        let recovered = tokio::time::timeout(WAIT, async {
            loop {
                match next_message(&mut peer_reader).await {
                    SessionMessage::VideoPacket { flags, .. }
                        if flags & VIDEO_FLAG_KEYFRAME != 0 =>
                    {
                        return;
                    }
                    _ => {}
                }
            }
        })
        .await;
        assert!(
            recovered.is_ok(),
            "RequestKeyframe 后必须退出补丁模式并重发全帧 IDR"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, ctld).await.expect("被控端应结束");
    }

    // ───────────────── 出站分档队列（阶段 10） ─────────────────

    /// 分类机械锁：每一类代表消息都必须落在正确的队列上。
    ///
    /// 三处最容易归错的点（改 `outbound_class` 前先看这里）：
    /// - `ScreenConfig` 必须与 `VideoPacket` 同队列，否则首帧可能先于尺寸声明到达；
    /// - `FileDone`/`FileError` 必须与 `FileChunk` 同队列，否则收尾会越过尚未投递的块；
    /// - `QosFeedback`/`RequestKeyframe` 在线上是 `Screen` lane，但内容小、时延敏感，走控制队列。
    #[test]
    fn outbound_class_covers_sensitive_messages() {
        assert_eq!(
            outbound_class(&SessionMessage::Bye {
                reason: "结束".to_owned(),
            }),
            OutboundClass::Close
        );
        for msg in [
            SessionMessage::VideoPacket {
                display: 0,
                seq: 1,
                codec: CodecId::Vp9,
                flags: VIDEO_FLAG_KEYFRAME,
                width: 2,
                height: 2,
                data: vec![0],
                capture_ts_us: 0,
            },
            SessionMessage::RegionUpdate {
                display: 0,
                width: 2,
                height: 2,
                rects: Vec::new(),
                capture_ts_us: 0,
            },
            SessionMessage::ScreenConfig {
                display: 0,
                codec: CodecId::Vp9,
                width: 2,
                height: 2,
            },
        ] {
            assert_eq!(outbound_class(&msg), OutboundClass::Screen, "{msg:?}");
        }
        for msg in [
            SessionMessage::FileChunk {
                id: 1,
                data: vec![0],
            },
            SessionMessage::FileDone { id: 1 },
            SessionMessage::FileError {
                id: 1,
                reason: "失败".to_owned(),
            },
        ] {
            assert_eq!(outbound_class(&msg), OutboundClass::File, "{msg:?}");
        }
        for msg in [
            SessionMessage::Heartbeat { t1_us: 1 },
            SessionMessage::HeartbeatAck {
                t2_us: 2,
                t3_us: 3,
            },
            SessionMessage::Input(InputEvent::MouseMove {
                x: 1,
                y: 2,
                display: 0,
            }),
            SessionMessage::RequestKeyframe { display: 0 },
            SessionMessage::QosFeedback {
                rtt_ms: 1,
                recv_kbps: 2,
                decode_ms: 3,
                backlog: 4,
            },
            SessionMessage::FileAccept {
                id: 1,
                accepted: true,
                reason: None,
                size: 0,
            },
            SessionMessage::CodecSelect {
                codec: CodecId::Vp9,
            },
            SessionMessage::LockScreen,
            // v8：被控端每秒遥测。必须走控制队列：它小、时延不敏感，但**绝不能丢**——
            // 落进画面队列会与帧抢带宽，落进 file 队列则会被暂存/丢弃。
            SessionMessage::HostStats(HostStats::default()),
        ] {
            assert_eq!(outbound_class(&msg), OutboundClass::Control, "{msg:?}");
        }
    }

    /// file 队列满时，文件消息只暂存、不阻塞；控制帧必须立刻投得进去。
    ///
    /// 这是「文件传输永不阻塞主循环」的核心断言：`try_enqueue_file` 若在满队列上 `.await`，
    /// 采集会停拍、输入与心跳会停摆（中继停走时对端还会把会话判成超时）。
    #[tokio::test]
    async fn full_file_queue_does_not_block_control() {
        let (mut queues, mut rx) = OutboundQueues::new();
        // 填满 file 队列。
        for byte in [1_u8, 2] {
            queues
                .try_enqueue_file(SessionMessage::FileChunk {
                    id: 1,
                    data: vec![byte; 16],
                })
                .expect("初始队列有空位");
        }
        assert!(!queues.file_has_room(), "file 队列应已填满");
        // 再压一条：只能进暂存区，且不得阻塞。
        queues
            .try_enqueue_file(SessionMessage::FileChunk {
                id: 1,
                data: vec![3; 16],
            })
            .expect("满队列也只是暂存");
        assert!(queues.has_staged_file(), "满队列的块应留在暂存区");
        assert!(!queues.file_has_room(), "暂存不会偷偷扩容队列");

        // 关键断言：控制帧投递立即成功（timeout 只是防挂死的守卫，不是预期等待）。
        let cancel = CancellationToken::new();
        tokio::time::timeout(
            Duration::from_millis(50),
            queues.enqueue_ctrl(
                &cancel,
                SessionFrame::control(SessionMessage::Heartbeat { t1_us: 0 }),
            ),
        )
        .await
        .expect("控制帧投递不得被文件队列阻塞")
        .expect("控制队列有空位");

        // 已入队的块保持原序，暂存的那条不在其中。
        for byte in [1_u8, 2] {
            let frame = rx.file.recv().await.expect("file 队列应有帧");
            match frame.message {
                SessionMessage::FileChunk { data, .. } => assert_eq!(data[0], byte),
                other => panic!("期望 FileChunk，实得 {other:?}"),
            }
        }
    }

    /// 暂存区保持 FIFO：`FileDone` 绝不能越过尚未投递的 `FileChunk`。
    ///
    /// 越序意味着接收方在收到最后一块之前就收到「传输完成」，落盘的文件会被截断。
    #[tokio::test]
    async fn staged_file_messages_keep_fifo_order() {
        let (mut queues, mut rx) = OutboundQueues::new();
        let cancel = CancellationToken::new();
        // 先填满队列，让后面三条全部落进暂存区。
        for _ in 0..FILE_QUEUE {
            queues
                .try_enqueue_file(SessionMessage::FileChunk {
                    id: 7,
                    data: vec![0; 16],
                })
                .expect("填队列");
        }
        for msg in [
            SessionMessage::FileChunk {
                id: 7,
                data: vec![1; 16],
            },
            SessionMessage::FileChunk {
                id: 7,
                data: vec![2; 16],
            },
            SessionMessage::FileDone { id: 7 },
        ] {
            queues
                .dispatch_frame(&cancel, SessionFrame::file(msg))
                .await
                .expect("file 类只暂存、不阻塞");
        }
        assert!(queues.has_staged_file(), "三条都应暂存");

        // 收一块、补投一次：每读出一块就腾出一个空位，暂存区按 FIFO 补进去。
        let mut seen = Vec::new();
        for _ in 0..FILE_QUEUE + 3 {
            let frame = rx.file.recv().await.expect("file 队列应始终有帧");
            seen.push(frame.message);
            queues.drain_file().expect("写任务仍在，补投应成功");
        }

        // 前 FILE_QUEUE 条是填充块（首字节 0），随后必须依次是块 1、块 2、完成（0xFF 代表）。
        let order: Vec<u8> = seen
            .into_iter()
            .map(|msg| match msg {
                SessionMessage::FileChunk { data, .. } => data[0],
                SessionMessage::FileDone { .. } => u8::MAX,
                other => panic!("意外的文件消息 {other:?}"),
            })
            .collect();
        assert_eq!(
            &order[FILE_QUEUE..],
            &[1, 2, u8::MAX],
            "暂存区必须保持 FIFO：FileDone 不能越过尚未投递的块（实际顺序 {order:?}）"
        );
    }
}
