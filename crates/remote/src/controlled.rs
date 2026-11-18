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
//! # 输入触发采集
//!
//! 采集节拍是周期性的，而输入事件到达的时刻与节拍相位无关：拖动窗口时最多白等一个周期
//! （30 fps 下 33 ms，且逐次随机），这正是「拖动延迟忽大忽小」的来源。故每次输入注入后
//! 都把下一拍提前到「上一拍之后半个周期」（见 [`input_capture_target`]），交互期间有效
//! 帧率最多翻倍；静止时仍按档位节拍，采集层的像素级变化检测会跳过编码，不额外耗带宽。
//!
//! # QoS 闭环（阶段 3）
//!
//! 主循环持有 [`RateController`]（AIMD）：每收到一条控制器的 `QosFeedback` 就决策一次，把新的
//! **生效值**（[`EncodeRates`]）交给下一次编码任务。码率/帧率变化由编码任务**在线重配置**
//! （vendor 补丁 6），**不重建、不插 IDR**；只有降编码分辨率（几何变化）才重建 + 强制 IDR。
//! 变化面积**逐帧**喂给 [`RateController::promote_activity`]（只升档，用户一开始操作就放量），
//! 降档仍由每秒反馈的 `observe_activity` 按迟滞处理。详见 [`crate::qos`] 的模块文档。
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
//! 六条硬约束：
//! - **顺序**：补丁必须排在那块全帧基准**之后**。进入模式前先让一帧基准全帧**真正投进出站
//!   队列**（编码任务回报 `base_frame_sent`）之后才切模式；两者走**同一条画面队列**
//!   （`ScreenConfig` 也在其中），故顺序天然成立。发送补丁走 [`OutboundQueues::try_enqueue_screen_or_return`]
//!   （**非阻塞**：主循环绝不能在补丁投递上 `.await`，否则网络拥塞时输入/心跳一并被卡住）。
//! - **补丁先暂存，不因「队列瞬时满」退出**（[`PatchStaging`]）：画面队列只有
//!   [`SCREEN_QUEUE`] 两深，刚进补丁模式时排在前面的基准 IDR 往往还没写完（真机上一帧
//!   ~240 KB 的 IDR 要 190+ ms），第一条补丁必然投不进去。旧实现在这里直接退出补丁模式并
//!   强制 IDR，于是「进入 → 5 ms 后 abort → 再打一个 IDR」形成自激——真机 0.8 秒打出 4 个
//!   ~240 KB 的 IDR。现在整拍补丁整批进有界暂存区、每拍补投，只有暂存区**超龄
//!   （[`PATCH_STAGING_MAX_AGE`]）或超字节上限（[`PATCH_STAGING_MAX_BYTES`]）**才退出——那
//!   才是「写侧真的跟不上」。补丁模式期间**暂停 QoS 闭环**，写侧不会因降档突然变快，所以
//!   上限必须按「一帧 IDR 的写出时间」量级取，不能按一帧补丁的大小取。
//!   退出时：若有**半批**横条已经发出（同批更早的已投递、其余仍暂存），控制端合成缓冲缺块，
//!   必须强制一帧完整 IDR；整批未发的直接丢弃即可——退出后那一帧全帧会整体覆盖。
//! - **进补丁模式用 IDR，退出用 P 帧**：补丁只改控制器侧的合成帧缓冲，**不碰 H.264 参考链**
//!   （补丁模式期间编码器根本不编码），退出后的第一帧只需普通 P 帧。但**基准帧必须是 IDR**：
//!   补丁只覆盖变化矩形，静态区域会长期沿用这一帧，若它继承了一张糊的参考（典型场景：
//!   切浏览器标签导致整页重绘，那一帧被单帧预算压过），整页就再也不会自愈——补丁模式里
//!   编码器不跑，intra refresh 也停了。这里付一个关键帧换「基底一定是整幅重编的」。
//!   其余让参考链/几何失效的事件同样 IDR：会话首帧、`RequestKeyframe`、`SelectDisplay` /
//!   `SetQuality`、显示器分辨率变化、QoS 降分辨率、编码器重建。
//! - **超预算先拆批，不退出**：单条 `RegionUpdate` 的上限是 32 KiB，单个脏矩形编码后超限时
//!   按行拆成若干横条分多条消息发出（同一拍总量 256 KiB / 最多 8 条）。只有**真的装不下**
//!   （矩形数、单拍总量或条数超限）才回全帧。
//! - **退出即全帧，且退出一律带冷却**：补丁采集失败、活跃度过高、装不下等只回到全帧并
//!   **立刻抓帧**（不插 IDR）；几何真的变了（显示器/缩放）才连带强制 IDR。退出后
//!   [`PATCH_REENTRY_HOLDOFF`]（基础 5 s）内不重新进入，避免「退出 → 整帧 → 再进入 → 再整帧」
//!   的抖动正反馈；若上一次补丁模式**寿命太短**（[`PATCH_SHORT_SESSION`]），冷却按
//!   [`PATCH_REENTRY_HOLDOFF_MAX`] 封顶指数退避——反复抖动的场景正是「每 3 秒一张基准 IDR」
//!   的来源（见 [`PATCH_ENTER_TICKS`] 的真机数据）。退出原因经 [`PatchExitReason`] 记一条
//!   `info!`，现场日志不必靠 IDR 间隔反推。
//! - **QoS 闭环在补丁模式下暂停**：控制器此时不解码视频，`decode_ms`/`recv_kbps` 是陈旧值，
//!   喂给 AIMD 只会制造假拥塞。回到全帧后的下一条 `QosFeedback` 自然恢复闭环。

use crate::auth::{ack_mac, PendingSessions};
use crate::codec::{vp9_encoder_factory, VideoEncoderApi, VideoEncoderFactory};
use crate::crypto::{derive_session_keys, Cipher, EphemeralKeyPair};
use crate::error::{RemoteError, RemoteResult};
use crate::latency::{self, LatencyWindow, Stage};
use crate::qos::{Feedback, QosAction, RateController};
use crate::screen::{
    bitrate_permille_for_pixels, scale_bitrate, CaptureInfo, CapturedPatches, Clipboard,
    CursorShape, DisplayInfo, EncodeOptions, EncodeRates, EncoderInfo, InputSink, RawFrame,
    ScreenPatch, ScreenSource, NATIVE_BITRATE_FACTOR,
};
use crate::session::{
    trusted_rtt, CodecId, HostActivity, HostStats, RegionRect, SessionCodec, SessionEnd,
    SessionFrame, SessionMessage, HEARTBEAT_INTERVAL, LIVENESS_TIMEOUT, MAX_REGION_RECTS,
    REGION_FLAG_ZSTD, SESSION_PROTO_VERSION, VIDEO_FLAG_CONFIG, VIDEO_FLAG_KEYFRAME,
};
use crate::stats::{SessionStats, SessionStatsReporter};
use crate::transfer::{TransferHub, CHUNK_PAUSE};
use crate::transport::BoxDuplex;
use crate::video::{encode_dimensions, EncodedVideo, MAX_ENCODE_WIDTH, NATIVE_MAX_ENCODE_WIDTH};
use futures_util::{SinkExt as _, StreamExt as _};
use rust_tunnel_common::protocol::RemoteMode;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
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
/// 这是**编码之后、绝不可丢弃**的队列（见模块文档的丢弃策略），所以它的深度**直接等于链路
/// 上的常驻排队延迟**：`深度 / fps`。4 深在 30 fps 下就是 133 ms 的稳态排队——链路吞吐一旦
/// 接近编码输出码率（QoS 闭环的目标正是如此），队列会长期装满，用户按一次键要等它排空。
///
/// 2 深把这段压到 66 ms，同时仍能吸收一次采集/心跳的抖动。**不必担心丢帧**：背压出口在
/// **编码之前**（编码队列满就跳过这一帧，等价降帧率，语义合法），所以队列浅只是让「编码前
/// 丢帧」更早发生，而不是让已编码的 P 帧丢失（那会断参考链）。
pub(crate) const SCREEN_QUEUE: usize = 2;

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
        | SessionMessage::HostStats(_)
        // 远端光标形状：极小、**时延敏感**（拖过链接/输入框时用户立刻能看出图标没跟上），
        // 且丢一条就会让控制端一直显示旧光标直到下次变化——绝不能丢，与其它控制帧同档。
        | SessionMessage::CursorShape { .. } => OutboundClass::Control,
    }
}

/// 出站写侧的四个发送端（会话主循环持有）。
///
/// `file_staging` 是文件类消息的**暂存区**：file 队列满时消息留在这里、下一拍重试，主循环
/// 因此永远不必为文件数据 `.await`。它保持 FIFO，所以 `FileDone`/`FileError` 不会越过尚未
/// 投递的 `FileChunk`。
///
/// `screen_pacing_kbps` 与接收端共享同一个原子：主循环在 QoS 生效值变化时写，写任务发送大帧
/// 时读（见 [`pacing_plan`]）。用共享原子而不是加一条控制消息，是因为它只是「一个数字」，且
/// 写任务不能为了拿到最新值而去等队列。
pub(crate) struct OutboundQueues {
    ctrl: mpsc::Sender<SessionFrame>,
    screen: mpsc::Sender<SessionFrame>,
    file: mpsc::Sender<SessionFrame>,
    close: mpsc::UnboundedSender<SessionFrame>,
    file_staging: VecDeque<SessionMessage>,
    screen_pacing_kbps: Arc<AtomicU32>,
}

/// [`OutboundQueues`] 的另一半：四个接收端，整体移交给 [`writer_loop`]。
pub(crate) struct OutboundReceivers {
    ctrl: mpsc::Receiver<SessionFrame>,
    screen: mpsc::Receiver<SessionFrame>,
    file: mpsc::Receiver<SessionFrame>,
    close: mpsc::UnboundedReceiver<SessionFrame>,
    /// 画面发送 pacing 的目标码率（kbps，0 = 关闭；见 [`OutboundQueues::set_screen_pacing_kbps`]）。
    screen_pacing_kbps: Arc<AtomicU32>,
}

impl OutboundQueues {
    /// 建立四个通道。返回的接收端必须移交给 [`writer_loop`]。
    #[must_use]
    pub(crate) fn new() -> (Self, OutboundReceivers) {
        let (ctrl_tx, ctrl_rx) = mpsc::channel(CTRL_QUEUE);
        let (screen_tx, screen_rx) = mpsc::channel(SCREEN_QUEUE);
        let (file_tx, file_rx) = mpsc::channel(FILE_QUEUE);
        let (close_tx, close_rx) = mpsc::unbounded_channel();
        // 0 = 不 pace：控制器侧从不设置它（大帧 pacing 是被控端的发送侧策略）。
        let screen_pacing_kbps = Arc::new(AtomicU32::new(0));
        (
            Self {
                ctrl: ctrl_tx,
                screen: screen_tx,
                file: file_tx,
                close: close_tx,
                file_staging: VecDeque::new(),
                screen_pacing_kbps: Arc::clone(&screen_pacing_kbps),
            },
            OutboundReceivers {
                ctrl: ctrl_rx,
                screen: screen_rx,
                file: file_rx,
                close: close_rx,
                screen_pacing_kbps,
            },
        )
    }

    /// 设置画面大帧的分段 pacing 目标码率（kbps；0 = 关闭）。
    ///
    /// 只在被控端、且只在 `encode = rate.current()` 更新之后调用——它必须与编码器的在线码率
    /// 保持同一个值，否则 pacing 会按一个已经不存在的码率摊开。
    pub(crate) fn set_screen_pacing_kbps(&self, kbps: u32) {
        self.screen_pacing_kbps.store(kbps, Ordering::Relaxed);
    }

    /// 当前 pacing 目标码率（kbps；0 = 关闭）。只在单测里读回验证，线上没有读者。
    #[cfg(test)]
    pub(crate) fn screen_pacing_kbps(&self) -> u32 {
        self.screen_pacing_kbps.load(Ordering::Relaxed)
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
    /// 只用于**编码任务**（它阻塞在一个独立任务里，阻塞它不会卡住输入/心跳）。会话 `select!`
    /// 主循环**绝不能**调用它——那里等一次队列，输入注入与采集节拍就一起停一次。
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

    /// **非阻塞**地把一条画面帧投进去（补丁路径专用）。
    ///
    /// 返回 `Ok(None)` = 已入队；`Ok(Some(frame))` = 队列已满，帧原样还回来——调用方必须
    /// 自己决定退让策略（补丁路径是放进暂存区等下一拍，见 [`PatchStaging`]）。这里**不等待、
    /// 不丢帧**。把帧还回来而不是只回一个布尔值，是为了让暂存区能原样放回（FIFO，且不必
    /// 为几 KB–32 KiB 的补丁重建一份）。
    ///
    /// # Errors
    /// 写任务已结束（通道关闭）时返回 [`link_closed`]。
    pub(crate) fn try_enqueue_screen_or_return(
        &self,
        frame: SessionFrame,
    ) -> RemoteResult<Option<SessionFrame>> {
        match self.screen.try_send(frame) {
            Ok(()) => Ok(None),
            Err(mpsc::error::TrySendError::Full(frame)) => Ok(Some(frame)),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(link_closed()),
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

/// 周期性强制关键帧的兜底间隔。
///
/// 正常情况下 IDR 只在参考链真的失效时才发（首帧、`RequestKeyframe`、切显示器/画质、几何
/// 变化）。但如果控制端**整段**收不到帧（丢包风暴、可靠层重传耗尽、控制器看门狗刚好在
/// 400 ms 限频窗口里没发出请求），恢复就只剩「控制器 3s 无帧看门狗 + 限频请求」这一条路，
/// 用户会看到一次以秒计的冻结。
///
/// 10 秒是**兜底**而不是节拍：静止画面下 P 帧保活（[`UNCHANGED_RESEND_INTERVAL`]）已经让
/// 控制端持续有画面，正常会话里这个定时器几乎不会命中；命中时也只多一个 150–400 KB 的 IDR
/// 尖峰（pacing 会把它摊开）。
const PERIODIC_KEYFRAME_INTERVAL: Duration = Duration::from_secs(10);

/// 大帧 pacing 的最小帧载荷（96 KiB）。
///
/// IDR 典型 150–400 KB，而普通 P 帧在静止/低活跃下只有几 KB。门槛定在这里，是为了让「真的
/// 会把链路堵住的帧」才付分段写的代价：交互帧本身就是延迟，给它们加 sleep 只会更慢。
const PACING_MIN_FRAME_BYTES: usize = 96 * 1024;

/// 大帧分段写的块大小（32 KiB）。
///
/// 6 Mbps 上一块约 43 ms，恰好是「让后面排队的小帧有机会被内核/中继推进一个块」的粒度；
/// 再大就失去交错的意义，再小则 write 次数与 TLS 记录开销都上去了。
const PACING_CHUNK_BYTES: usize = 32 * 1024;

/// 单个块间等待的绝对上限（100 ms）。
///
/// pacing 的目的是**削尖峰**，不是限速：码率读数偏低（刚降档、或读到的值还没跟上实际链路）
/// 时，按目标码率算出的块间等待可能长达数百毫秒，一帧就会被拖成秒级。封顶之后即使读数
/// 离谱，一帧最多也就多花「块数 × 100 ms」。
const PACING_MAX_GAP: Duration = Duration::from_millis(100);

/// 全部块间等待相对「该帧在目标码率下的自然串行时间」的上限倍数。
///
/// 与 [`PACING_MAX_GAP`] 是两道互补的闸门：前者管单个间隙，这条管总量（块数多时单个间隙
/// 虽小、累加也可能超过一帧的自然时间）。
const PACING_MAX_WAIT_FACTOR: u32 = 2;

/// 进入补丁模式的**变化面积**门槛（千分比，3%）：低于它才算「安静」。
///
/// 与 [`PATCH_EXIT_PERMILLE`] 分开是**迟滞**：进入要求连续多次低活跃，退出只要一次高活跃。
/// 共用同一个值会在门槛附近来回抖，每次抖动都要多一次全帧基准帧往返，正好抵消补丁的收益。
const PATCH_ENTER_PERMILLE: u16 = 30;

/// 退出补丁模式的**变化面积**门槛（千分比，8%）：超过它立刻回到全帧。
const PATCH_EXIT_PERMILLE: u16 = 80;

/// 连续多少个**低活跃**采集才请求进入补丁模式（迟滞）。
///
/// 15 拍在 30 fps 下约 **500 ms**（旧值 3 拍 ≈100 ms）。**为什么要多等这 0.4 秒**：每进入
/// 一次补丁模式都要先发一张**基准 IDR**，而真机数据（1080p30 H.264 / Intel QSV / Punch 路径
/// RTT 13 ms，同一会话 64 秒）是：进入 **21 次**（平均每 3 秒一次）、共 31 张 IDR、占全部
/// 字节 **43%**（平均 223 KB、最大 417 KB，P 帧平均只有 25 KB）。一张 417 KB 的 IDR 在
/// 10 Mbps 的 pacing 下要写 **338 ms**，期间后续帧在 2 深的画面队列里干等（`send_q` p99
/// 237 ms、max 1.34 s）。
///
/// 换句话说，旧门槛把「任何一次 100 ms 的短暂安静」都当成可以进入补丁模式的静止桌面，
/// 于是「进入 → 基准 IDR → 用户又动了一下 → 退出 → 整帧」反复循环。多等 0.4 秒的代价只是
/// 静止桌面多几帧 ~3 KB 的 P 帧（≈30–40 KB），换来的是少一张 223 KB 的 IDR——收益是数量级
/// 的。**不要用「把基准 IDR 改小」来解决这件事**：基准帧必须是整幅重编的 IDR（补丁只覆盖
/// 变化矩形，静态区域长期沿用这一帧，糊了就永不自我修复）。
const PATCH_ENTER_TICKS: u32 = 15;

/// 补丁模式下空补丁（保活）的最大间隔。
///
/// 控制器侧有「长时间无帧」看门狗；补丁模式下没有视频帧，空 `RegionUpdate` 就是它的心跳。
const PATCH_KEEPALIVE: Duration = Duration::from_millis(250);

/// 补丁模式的**固定**采集间隔（30 fps）。
///
/// 补丁本身很小（几 KB），带宽由 [`PatchMode::admit_patch_bytes`] 的窗口兜底，所以帧率不必
/// 跟着视频档位走。反过来，**不能**取 `max(生效帧率间隔, 33ms)`：进入补丁模式时内容上限通常
/// 已经收紧到 UI 档（15 fps → 66 ms），取 max 仍是 66 ms，补丁节拍与「用户操作 → 检测到大
/// 变化 → 退出补丁模式」的延迟都会被拖慢一倍，而这正是本模式要消除的顿挫。
const PATCH_MAX_INTERVAL: Duration = Duration::from_millis(33);

/// 脏矩形 NV12 的 zstd 压缩级别。
///
/// level 1 的编码速度远快于采集节拍，单块几 KB，收益却很明显（桌面文本/色块高度可压）。
const PATCH_ZSTD_LEVEL: i32 = 1;

/// 单条 `RegionUpdate` 的压缩后**总字节**上限（32 KiB）。
///
/// 单条越大，控制端的解压/合成尖峰越明显；取 32 KiB 之后，一个「面积不大但内容复杂」的
/// 脏矩形可以被**拆成若干横条**分多条消息发出，而不是像以前那样一旦超过整批上限就退回全帧。
/// 拆批的理由见 [`PATCH_MAX_TICK_BYTES`]。
const PATCH_MESSAGE_MAX_BYTES: usize = 32 * 1024;

/// 单拍（一次采集）最多发多少条 `RegionUpdate`。
///
/// 超过它说明这一拍的变化量已经大到补丁不比整帧划算，直接回全帧。
const PATCH_MAX_MESSAGES_PER_TICK: usize = 8;

/// 单拍（一次采集）补丁的压缩后**总字节**上限（256 KiB = 8 × 32 KiB）。
///
/// 它同时是「窗口预算」的天花板：补丁窗口兜底最多允许 `PATCH_MAX_BUDGET_KBPS`（2000 kbps）
/// × 1s ≈ 250 KB，更大的单拍批本来就会被 `PatchMode::admit_patch_bytes` 拒绝。
///
/// **为什么从旧的单批 64 KiB 抬到这里**：真机日志里「一个面积不大但内容复杂的脏矩形压缩后
/// 超过 64 KiB」会让补丁模式退出 → 发一整帧 → 0.2 s 后又进入（进入前还要再发一整帧基准帧）
/// → 再次超预算，形成 200–700 ms 的整帧循环，正是角标抖动与卡顿的来源。现在单条消息仍有
/// 32 KiB 上限（控制端不吃尖峰），单拍总量只约束「一次 UI 变化」的规模。
///
/// 拆出来的多条消息是同一条画面 FIFO 上的**独立帧**，控制器按到达顺序**逐条**合成，所以
/// 首块在约「32 KiB / 链路速率」后就能显示，不必等整批发完——这正是它能替代「退回全帧」
/// 的原因（整帧要等编码完成且整帧送达才可见）。
const PATCH_MAX_TICK_BYTES: usize = PATCH_MESSAGE_MAX_BYTES * PATCH_MAX_MESSAGES_PER_TICK;

/// 补丁暂存区允许滞留的**最长时间**（500 ms）。
///
/// 真机日志（Windows → Windows、QSV 1080p@30、会话码率 10 Mbps、路径 Punch）里，一帧
/// 232–246 KB 的 IDR 从入队到写完要 190+ ms（32 KiB/块、块间 26 ms 的 pacing）。画面队列只有
/// [`SCREEN_QUEUE`] 两深，所以「刚进补丁模式」的头几百毫秒里排在前面的基准 IDR 必然占满队列
/// ——这是**瞬态**，不是写侧跟不上。旧实现在第一条补丁投不进去时就退出补丁模式并强制 IDR，
/// 于是每次进入补丁模式都立刻产出又一个 ~240 KB 的 IDR（现场：0.8 秒 4 个）。
///
/// 500 ms ≈ 2.6 个这样的 IDR 写出时间，足以吸收「基准 IDR + 退出的 P 帧」这类正常排队；超过
/// 它就说明补丁真的在被持续丢弃，继续攒只会让控制端画面越来越旧，回全帧重来更划算。
const PATCH_STAGING_MAX_AGE: Duration = Duration::from_millis(500);

/// 补丁暂存区允许滞留的**最大字节数**（256 KiB = 一个满配补丁拍）。
///
/// 与 [`PATCH_STAGING_MAX_AGE`] 是两道互补的闸门：时间管「陈旧」，字节管「内存与迟到量」。
/// 取单拍上限（[`PATCH_MAX_TICK_BYTES`]）而不是更多拍：攒下两拍以上的补丁再发出去，控制端
/// 看到的就是几百毫秒前的画面，已经不如直接回一帧全帧。单拍本身不会超限，所以「空暂存区
/// 接一拍」永远成功——超限只可能来自「之前几拍一直没投出去」。
const PATCH_STAGING_MAX_BYTES: usize = PATCH_MAX_TICK_BYTES;

/// 暂存区里的一条画面消息（补丁或空保活）。
struct StagedScreen {
    /// 已装好的线上帧（`RegionUpdate`）。
    frame: SessionFrame,
    /// 这条消息的补丁载荷字节数（`rects` 的 `data` 总和；空保活为 0）。
    bytes: usize,
    /// 是否是所在补丁批的**首条**。
    ///
    /// 它只服务一件事：区分「整批还没发」与「半批已发出、其余仍滞留」。前者退出补丁模式后
    /// 直接丢弃是安全的（下一帧全帧整体覆盖控制端缓冲）；后者控制端合成缓冲**缺块**，
    /// 退出必须强制一帧完整 IDR 才能修复。
    batch_start: bool,
    /// 进入暂存区的时刻（滞留时长按**头部**这条算）。
    staged_at: std::time::Instant,
}

/// 补丁出站的有界暂存区（语义与 file 档的 `file_staging` 相同，见 [`OutboundQueues`]）。
///
/// 主循环绝不能在补丁投递上 `.await`（那会把输入注入/心跳/采集节拍一起卡住），而画面队列
/// 又只有 [`SCREEN_QUEUE`] 深。满队列时把**整拍**补丁先放这里、下一拍再 `try` 补投，
/// 是「不阻塞」与「补丁绝不丢」之间唯一成立的组合：
/// - 补丁丢了，控制端合成缓冲会永久缺一块（直到下一个 IDR），所以不能像视频帧那样在背压出口
///   直接放弃；
/// - 反过来，攒得太久说明写侧真的跟不上，这时退出补丁模式 + 全帧（必要时 IDR）比继续攒好。
///
/// 整批入区是**原子**的（先算总字节再决定），所以暂存区里不会出现「某批只进了一半」的中间态；
/// 半批只可能来自**投递**过程（`drain` 投到一半队满），由 [`PatchStaging::partial`] 识别。
#[derive(Default)]
struct PatchStaging {
    frames: VecDeque<StagedScreen>,
    bytes: usize,
}

impl PatchStaging {
    fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn bytes(&self) -> usize {
        self.bytes
    }

    /// 把**一整拍**补丁放进暂存区；返回 `false` 表示装不下（调用方应退出补丁模式）。
    ///
    /// 先按整批总量判定再入区，故不会出现「半批进暂存区」：要么整批都在，要么一条都没进。
    fn push_batch(&mut self, messages: Vec<SessionMessage>, now: std::time::Instant) -> bool {
        let batch_bytes = messages.iter().fold(0_usize, |acc, msg| {
            acc.saturating_add(usize::try_from(region_update_bytes(msg)).unwrap_or(usize::MAX))
        });
        if self.bytes.saturating_add(batch_bytes) > PATCH_STAGING_MAX_BYTES {
            return false;
        }
        for (index, msg) in messages.into_iter().enumerate() {
            let bytes = usize::try_from(region_update_bytes(&msg)).unwrap_or(usize::MAX);
            self.frames.push_back(StagedScreen {
                frame: SessionFrame::screen(msg),
                bytes,
                batch_start: index == 0,
                staged_at: now,
            });
        }
        self.bytes = self.bytes.saturating_add(batch_bytes);
        true
    }

    /// **非阻塞**地按 FIFO 补投；队满即停、剩余留到下一拍。返回本轮投出的条数。
    ///
    /// `try_send(frame, bytes)` 返回 `Ok(None)` = 已入队；`Ok(Some(frame))` = 队列已满，
    /// 帧原样还回来（保持 FIFO，绝不重建、绝不丢）。这里保持严格 FIFO：后面的消息绝不会
    /// 越过前面没投出去的，所以控制端收到的补丁顺序永远是采集顺序。
    ///
    /// # Errors
    /// 写任务已结束（链路断开）时返回错误，暂存区一并清空。
    fn drain<F>(&mut self, mut try_send: F) -> RemoteResult<usize>
    where
        F: FnMut(SessionFrame, u64) -> RemoteResult<Option<SessionFrame>>,
    {
        let mut sent = 0;
        while let Some(staged) = self.frames.pop_front() {
            let StagedScreen {
                frame,
                bytes,
                batch_start,
                staged_at,
            } = staged;
            match try_send(frame, u64::try_from(bytes).unwrap_or(u64::MAX)) {
                Ok(None) => {
                    self.bytes = self.bytes.saturating_sub(bytes);
                    sent += 1;
                }
                Ok(Some(frame)) => {
                    self.frames.push_front(StagedScreen {
                        frame,
                        bytes,
                        batch_start,
                        staged_at,
                    });
                    break;
                }
                Err(err) => {
                    self.clear();
                    return Err(err);
                }
            }
        }
        Ok(sent)
    }

    /// 暂存区头部是否是**半批**（同批更早的横条已经投出、其余仍滞留）。
    ///
    /// 头部的 `batch_start == false` 等价于「这一批的首条已经离开暂存区」，也就是控制端的
    /// 合成缓冲已经吃到了这批的一部分。整批都在暂存区时返回 `false`。
    fn partial(&self) -> bool {
        self.frames.front().is_some_and(|staged| !staged.batch_start)
    }

    /// 头部这条滞留是否已超龄（见 [`PATCH_STAGING_MAX_AGE`]）。
    fn over_age(&self, now: std::time::Instant) -> bool {
        self.frames.front().is_some_and(|staged| {
            now.saturating_duration_since(staged.staged_at) >= PATCH_STAGING_MAX_AGE
        })
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
    }
}

/// 退出补丁模式后的**基础重入冷却**（5 秒）。
///
/// 退出原因（超预算、关键帧请求、活跃度回升）往往下一拍就消失；没有冷却时连续
/// [`PATCH_ENTER_TICKS`] 个安静拍就会重新进入，而进入前必须把一整帧基准帧写进
/// 队列——「退出 → 全帧 → 再进入 → 再全帧」因此会变成正反馈。5 秒足够让一次突发过去，又
/// 不至于让静止桌面长时间停在整帧模式。
///
/// **为什么不只是一个固定值**：真机上「用户每隔几秒碰一下鼠标/键盘」的桌面会让补丁模式
/// 反复进入又立刻退出，每次固定 5 秒冷却仍会每 5 秒付一张基准 IDR。所以冷却按上一次补丁
/// 模式的**存活时长**指数退避（见 [`PatchMode::exit`]）：短命会话 5 → 10 → 20 秒，长命
/// 会话（≥ 2 × [`PATCH_SHORT_SESSION`]）归零回到 5 秒。
const PATCH_REENTRY_HOLDOFF: Duration = Duration::from_secs(5);

/// 重入冷却指数退避的**上限**（20 秒）。
///
/// 封顶的理由：退避只是「别让抖动正反馈」，不是想把补丁模式关掉。桌面真的安静下来之后，
/// 用户最迟 20 秒内应回到补丁模式（补丁态的 e2e 实测 5 ms），再长就会让静止桌面长期停在
/// 整帧编码的带宽开销上。
const PATCH_REENTRY_HOLDOFF_MAX: Duration = Duration::from_secs(20);

/// 「短命补丁会话」的判定阈值（2 秒）。
///
/// 存活短于它说明这次进入几乎立刻被打断（用户还在间歇操作，桌面并未真正静止），下一次
/// 冷却翻倍；存活达到它的 2 倍（4 秒）才说明桌面确实静下来了，退避归零。
const PATCH_SHORT_SESSION: Duration = Duration::from_secs(2);

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
/// 补丁模式可能在 QoS 的内容上限收紧之前就进入（连续 [`PATCH_ENTER_TICKS`] 拍低活跃
/// ≈500 ms，而面积上限要 2 个 1s 的反馈 tick）。若直接拿 `rate.current()` 的 6/10 Mbps
/// 当预算，补丁窗口的兜底就形同虚设。2 Mbps 对「UI 小面积变化」足够宽裕，超过它就说明该
/// 走全帧了。
const PATCH_MAX_BUDGET_KBPS: u32 = 2000;

/// 等基准帧真正写出的上限。
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

/// 退出补丁模式的原因（**只用于日志**，不参与任何判据）。
///
/// 真机日志（info 级）里原先只有「进入脏矩形补丁模式」一条，退出只有个别分支打 `debug!`，
/// 现场只能靠 IDR 间隔反推「为什么又退出了」。这些字符串是排查入口：哪一个闸门在频繁
/// 触发（`activity`？`staging_age`？`keyframe_request`？）直接决定该调哪个常量。
///
/// 取值稳定、简短，便于 grep/统计；`Display` 与 [`Self::as_str`] 同值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchExitReason {
    /// 画面活跃度回升（超过 [`PATCH_EXIT_PERMILLE`]）。
    Activity,
    /// 脏矩形数量超过 [`MAX_REGION_RECTS`]。
    Rects,
    /// 补丁出码超过窗口预算，或单拍装不进预算。
    Budget,
    /// 补丁暂存区已满（写侧持续跟不上）。
    StagingFull,
    /// 补丁在暂存区滞留超过 [`PATCH_STAGING_MAX_AGE`]。
    StagingAge,
    /// 补丁采集本拍未产出补丁（平台判定不适合补丁）。
    NoPatches,
    /// 补丁尺寸与当前编码尺寸不符（几何已变）。
    SizeMismatch,
    /// 控制器请求关键帧（它那块合成缓冲已不可信）。
    KeyframeRequest,
    /// 切显示器/改画质/显示器热插拔等几何或参考链重建。
    Reconfigure,
}

impl PatchExitReason {
    /// 日志里使用的稳定字符串。
    const fn as_str(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Rects => "rects",
            Self::Budget => "budget",
            Self::StagingFull => "staging_full",
            Self::StagingAge => "staging_age",
            Self::NoPatches => "no_patches",
            Self::SizeMismatch => "size_mismatch",
            Self::KeyframeRequest => "keyframe_request",
            Self::Reconfigure => "reconfigure",
        }
    }
}

impl std::fmt::Display for PatchExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// 已请求基准帧、正在等它真正写出。
    enter_pending: bool,
    /// 等待基准帧的截止时刻。
    enter_deadline: Option<std::time::Instant>,
    /// 上一次真正发出 `RegionUpdate` 的时刻（空补丁保活用）。
    last_patch_sent: Option<std::time::Instant>,
    /// 补丁出码统计窗口的起点（进入补丁模式时重置）。
    patch_window_start: std::time::Instant,
    /// 本窗口内已发出的补丁字节数。
    patch_window_bytes: u64,
    /// 重入冷却的截止时刻（见 [`PATCH_REENTRY_HOLDOFF`]）：`None` = 不在冷却期。
    holdoff_until: Option<std::time::Instant>,
    /// 本次补丁模式的进入时刻（算存活时长，驱动冷却退避）。`None` = 没进过/已退出。
    entered_at: Option<std::time::Instant>,
    /// 下一次退出后要用的重入冷却（基础 [`PATCH_REENTRY_HOLDOFF`]，短命会话翻倍，上限
    /// [`PATCH_REENTRY_HOLDOFF_MAX`]）。
    holdoff_backoff: Duration,
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
            holdoff_until: None,
            entered_at: None,
            holdoff_backoff: PATCH_REENTRY_HOLDOFF,
        }
    }

    /// 记录一批补丁的**出码字节**，并在窗口到点时判断是否超预算。
    ///
    /// 返回 `false` 表示这一批不该发：要么上一窗口的补丁码率超过 `budget_kbps`，要么单批
    /// 本身就超过一个窗口的预算（后者防止一个巨大的补丁批把链路瞬时打满）。补丁模式下
    /// QoS 闭环是暂停的，这里是唯一的带宽兜底。
    fn admit_patch_bytes(&mut self, bytes: u64, now: std::time::Instant, budget_kbps: u32) -> bool {
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
    /// 返回 `true` 表示本帧必须作为**基准全帧**：调用方据此置 `force_keyframe` / `force_encode`
    /// 并清 `base_frame_sent`（见模块文档的「顺序」约束）。
    ///
    /// 基准帧是 **IDR** 而不是 P 帧：补丁模式只覆盖变化矩形，剩下的静态区域会被长期沿用，
    /// 所以基底一旦是糊的（例如刚切过浏览器标签、整页重绘那帧被单帧预算压过），整页就会
    /// **一直**糊着不再自愈——补丁模式期间编码器根本不跑，intra refresh 也停了。进补丁模式
    /// 时付一个关键帧（RDP 换桌面时也是这么做的）换「基底一定是整幅重编的」，是这里的取舍。
    fn observe_video(&mut self, changed_permille: u16, now: std::time::Instant) -> bool {
        // 重入冷却：刚退出补丁模式的一段时间内连续安静也**不**重新进入。冷却期内把连续计数
        // 清零，冷却结束后重新从 0 开始累计（见 [`PATCH_REENTRY_HOLDOFF`]）。
        if let Some(until) = self.holdoff_until {
            if now < until {
                self.enter_streak = 0;
                return false;
            }
            self.holdoff_until = None;
        }
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
            // 基准帧刚发出，保活与带宽窗口都从这里起算；存活时长也从这里起算。
            self.last_patch_sent = Some(now);
            self.patch_window_start = now;
            self.patch_window_bytes = 0;
            self.entered_at = Some(now);
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
    ///
    /// `now` 由调用方传入（而不是内部取 `Instant::now()`）：单测要能直接构造时刻验证冷却，
    /// 不必真的等待 [`PATCH_REENTRY_HOLDOFF`]。
    ///
    /// 冷却按上一次补丁模式的**存活时长**指数退避（见 [`PATCH_SHORT_SESSION`]）：短命会话
    /// 翻倍（5 → 10 → 20 秒，[`PATCH_REENTRY_HOLDOFF_MAX`] 封顶），长命会话归零回基础值。
    /// 真机上「用户每隔几秒碰一下鼠标/键盘」的桌面正是「每 3 秒一张基准 IDR」的来源，固定
    /// 冷却挡不住这种抖动。
    ///
    /// 退出原因在这里统一记一条 `info!`（与「进入脏矩形补丁模式」对称）：调用点只负责给出
    /// [`PatchExitReason`]，不必各打各的日志——现场日志（info 级）里原先什么都看不到，只能
    /// 靠 IDR 间隔反推。只有**真的在补丁模式里**才记（几何重建等也会调用本函数，那时没有
    /// 「退出」可言）。
    fn exit(&mut self, reason: PatchExitReason, now: std::time::Instant) {
        let dwell = self
            .entered_at
            .take()
            .map(|at| now.saturating_duration_since(at));
        let was_patches = self.mode == ScreenMode::Patches;
        if let Some(dwell) = dwell {
            if dwell < PATCH_SHORT_SESSION {
                // 进去不到两秒就被打断：桌面并没真的静下来，下一次冷却翻倍。
                self.holdoff_backoff = self
                    .holdoff_backoff
                    .saturating_mul(2)
                    .min(PATCH_REENTRY_HOLDOFF_MAX);
            } else if dwell >= PATCH_SHORT_SESSION * 2 {
                // 撑过两倍阈值：这次进入确实有收益，退避归零。
                self.holdoff_backoff = PATCH_REENTRY_HOLDOFF;
            }
        }
        self.mode = ScreenMode::Video;
        self.enter_streak = 0;
        self.enter_pending = false;
        self.enter_deadline = None;
        let holdoff = self.holdoff_backoff;
        self.holdoff_until = Some(now + holdoff);
        if was_patches {
            let dwell_ms = dwell.map_or(0, |dwell| {
                u64::try_from(dwell.as_millis()).unwrap_or(u64::MAX)
            });
            info!(
                reason = reason.as_str(),
                dwell_ms,
                holdoff_ms = u64::try_from(holdoff.as_millis()).unwrap_or(u64::MAX),
                "退出脏矩形补丁模式"
            );
        }
    }
}

/// 当前模式下的采集节拍。
///
/// 视频路径跟随生效帧率；补丁路径**固定** [`PATCH_MAX_INTERVAL`]——补丁很小，带宽由
/// `admit_patch_bytes` 兜底，不该被视频侧的内容上限（UI 档 15 fps）拖慢进入/退出判据的检测。
fn capture_tick_period(mode: ScreenMode, encode: &EncodeRates) -> Duration {
    match mode {
        ScreenMode::Video => encode.capture_interval(),
        ScreenMode::Patches => PATCH_MAX_INTERVAL,
    }
}

/// 交互期间把下一拍采集**提前**到「上一拍开始后半个周期」（纯函数，便于单测）。
///
/// 采集是周期节拍，而输入事件到达的时刻与节拍相位无关：用户拖动窗口时，一次鼠标移动最
/// 多要等满一个采集周期（默认 30 fps 下 33 ms）才会被采到，而且等待时长逐次随机——这正是
/// 「拖动时延迟忽大忽小」的来源。输入生效后立刻把节拍拉近，采样相位的等待就从
/// `0..period` 降到 `0..period/2`，拖动/打字的第一帧明显更跟手。
///
/// **只提前到半个周期**：上限是「交互期间最多把有效帧率翻倍」，而不是让输入把采集推到
/// 不限速——那会在拖动时把编码器与带宽打满。编码器/出站队列本就有编码前丢帧出口，跟不上
/// 时会自然降速。补丁模式同样适用（打字时游标/输入框反馈更即时）。
fn input_capture_target(
    now: Instant,
    last_capture_at: Option<Instant>,
    period: Duration,
) -> Instant {
    let min_interval = period / 2;
    match last_capture_at {
        Some(last) => (last + min_interval).max(now),
        None => now,
    }
}

/// 输入到达时是否把下一拍**提前**：只在 `target` 早于既定计划时才返回它。
///
/// `next_capture_at` 必须是**与 `Sleep` 同步**的最新计划时刻（见 [`schedule_capture`]）。
/// 否则帧率下调后它会停在过去，`target < next_capture_at` 不成立，提前采集会被静默丢弃
/// ——恰好是「空闲后 QoS 降到 15 fps，用户重新开始拖窗」这个最需要它的时刻。
fn input_capture_pull(
    now: Instant,
    last_capture_at: Option<Instant>,
    period: Duration,
    next_capture_at: Instant,
) -> Option<Instant> {
    let target = input_capture_target(now, last_capture_at, period);
    (target < next_capture_at).then_some(target)
}

/// 重排采集节拍：**`next_capture_at` 与 `Sleep` 必须同时更新**。
///
/// 固定相位推进（tick 分支）与输入提前采集都读 `next_capture_at`。只 reset `Sleep` 会
/// 让两者不一致——例如 QoS 把帧率降到 15 后 `Sleep` 被推到 66 ms，而 `next_capture_at`
/// 仍停在旧的 16 ms，下一次输入提前采集就会被 `target < next_capture_at` 静默丢弃，
/// 第一帧仍要等满新周期。**所有**重排都必须走这里。
fn schedule_capture(
    capture_tick: &mut Pin<Box<Sleep>>,
    next_capture_at: &mut Instant,
    at: Instant,
) {
    *next_capture_at = at;
    capture_tick.as_mut().reset(at);
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

/// 1080p 的像素数：内容上限的面积基准（见 [`content_area_permille`]）。
const CONTENT_CAP_BASE_PIXELS: u64 = 1920 * 1080;

/// 编码面积相对 1080p 的倍数（千分比：1080p = 1000，4K = 4000）。
///
/// 内容上限（UI 1.2 Mbps / Mixed 2.5 Mbps）是在 1080p 上标定的。同一个**比例**的变化
/// 面积在 4K 上是四倍像素，按绝对码率收紧会让每像素比特掉到四分之一，文字就被量化糊
/// （见 [`crate::qos::RateController::set_content_area_permille`]）。这里用
/// **实际编码尺寸**（与视频路径同一套计算）算倍数，交给闭环等比放大。
#[must_use]
fn content_area_permille(frame_width: u32, frame_height: u32, scale: f32, max_width: u32) -> u32 {
    let (width, height) = encode_dimensions(frame_width, frame_height, scale, max_width);
    let pixels = u64::from(width) * u64::from(height);
    u32::try_from(pixels.saturating_mul(1000) / CONTENT_CAP_BASE_PIXELS).unwrap_or(u32::MAX)
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

/// 原生分辨率会话按**显示器面积**放大档位码率上限（帧率、缩放不动）。
///
/// `permille` 来自 [`bitrate_permille_for_pixels`]：1080p 显示器是 1000（原样），
/// 4K 是 4000。用实际像素算而不是「开关是否打开」，避免 1080p 显示器凭空拿到 4 倍。
fn session_encode_options(options: EncodeOptions, permille: u32) -> EncodeOptions {
    if permille > 1000 {
        EncodeOptions {
            target_bitrate_kbps: scale_bitrate(options.target_bitrate_kbps, permille),
            ..options
        }
    } else {
        options
    }
}

/// 某台显示器在原生分辨率会话里的码率倍数（千分比）；非原生会话恒 1000。
fn native_bitrate_permille(display: Option<&DisplayInfo>, native_active: bool) -> u32 {
    if !native_active {
        return 1000;
    }
    display.map_or(NATIVE_BITRATE_FACTOR * 1000, |display| {
        bitrate_permille_for_pixels(u64::from(display.width) * u64::from(display.height))
    })
}

/// 把逐帧变化面积喂给内容上限，并在帧率变化时立刻重排采集节拍。
///
/// 升档必须赶在**本帧**编码之前生效：调用方在更新 `last_changed_permille` 之后、构造
/// [`EncodeJob`] 之前调用本函数。
fn refresh_activity(
    rate: &mut RateController,
    encode: &mut EncodeRates,
    queues: &OutboundQueues,
    capture_tick: &mut Pin<Box<Sleep>>,
    next_capture_at: &mut Instant,
    mode: ScreenMode,
    changed_permille: u16,
) {
    rate.promote_activity(changed_permille);
    let next = rate.current();
    if next.fps != encode.fps {
        schedule_capture(
            capture_tick,
            next_capture_at,
            Instant::now() + capture_tick_period(mode, &next),
        );
    }
    *encode = next;
    // pacing 描述的是**链路速率**，取 AIMD 值而不是内容上限钳制后的生效码率：小面积变化的
    // P 帧本来就只有几 KB，按 1.2 Mbps 摊开没有任何带宽收益，只会给大帧（关键帧）白加延迟。
    queues.set_screen_pacing_kbps(rate.keyframe_bitrate_kbps());
}

/// 退出补丁模式：回到全帧视频，并让采集节拍**立刻**到期（先把用户操作的那一帧抓上来）。
///
/// **不在这里强制关键帧**：补丁不碰参考链，退出后的第一帧 P 帧就能被正确解码。
/// 调用方只在参考链/几何真的失效时（`RequestKeyframe`、切显示器、改缩放）额外置
/// `force_keyframe`。无条件插 IDR 会让每次切窗口/开文件夹都付出一个 150–400 KB 的尖峰。
///
/// 退出同时丢弃暂存区（见 [`PatchStaging`]）并开启重入冷却（[`PATCH_REENTRY_HOLDOFF`]，
/// 按存活时长指数退避），避免「退出 → 整帧 → 立刻再进入 → 再整帧」的抖动正反馈。暂存区
/// 必须在退出时清掉：里面是**旧**画面的补丁，若让它们排到退出后的全帧之后，控制端会把旧
/// 内容盖到新画面上。
///
/// 已发出的**半批**横条由调用方判断（[`PatchStaging::partial`]）并另行强制关键帧——那时
/// 控制端合成缓冲缺块，只有完整 IDR 能修复。
fn exit_patch_mode(
    mode: &mut PatchMode,
    staging: &mut PatchStaging,
    force_encode: &mut bool,
    last_changed_permille: &mut u16,
    capture_tick: &mut Pin<Box<Sleep>>,
    next_capture_at: &mut Instant,
    reason: PatchExitReason,
) {
    mode.exit(reason, std::time::Instant::now());
    staging.clear();
    *force_encode = true;
    // 面积未知/刚发生大变化：保守给满面积，别让下一次 1Hz 反馈拿补丁模式里的旧值
    // 把内容上限压回 UI 档（15 fps / 1.2 Mbps）。
    *last_changed_permille = 1000;
    // 立刻抓一帧，并把 `next_capture_at` 一起锚到现在；否则它仍停在未来，下一次输入
    // 提前采集会因为 `target < next_capture_at` 反过来把这个「立刻」推迟。
    schedule_capture(capture_tick, next_capture_at, Instant::now());
}

/// 关键帧请求的**去重窗口**（350 ms）。
///
/// 真机日志（Windows → Windows、QSV 1080p@30、10 Mbps、Punch）：74 秒会话 595 帧里 63 个
/// IDR，占 51% 字节、**75% 的写线时间**；其中 0.8 秒内打出 4 个 ~240 KB 的 IDR，
/// 246199 / 246117 字节几乎相同——同一幅画面被反复重编。控制器在 `awaiting_keyframe` 期间
/// 每 400 ms 重发一次请求，而被控端「有请求就置 `force_keyframe`」，于是每个请求都变成一个
/// 整帧尖峰；一帧 IDR 从编码到写完要 190+ ms，这段时间里到达的请求完全是在为**已经在路上**
/// 的关键帧重复付费。
///
/// 窗口取 350 ms 的依据是控制器 400 ms 的重发周期，它必须**严格小于**它：
/// ① 被吸收的请求不会让控制器永久卡在 `awaiting_keyframe`——下一次重发必然落在窗口之外，
///    一定会被满足；
/// ② 若那个 IDR 真的在链路上丢了，最坏也只是多等一个重发周期（≈400 ms），而不是永久抑制。
/// 取到 400 ms 以上就把「重发」也吞掉了，那就不再是去重。
const KEYFRAME_DEDUP_WINDOW: Duration = Duration::from_millis(350);

/// IDR 在途/刚写出的共享观测点（进程内，**不上线**）。
///
/// 编码任务在**承诺编码一个 IDR** 时入账，写任务在该帧**真正写完**时销账；会话循环据此
/// 吸收重复的关键帧请求（见 [`keyframe_request_is_redundant`]）。
///
/// 与 `base_frame_sent` 分开是刻意的：后者只服务补丁基准帧的**顺序**不变量（「基准帧已进入
/// 出站队列」），语义不能被这里的「IDR 是否已上线」污染；两者的事件点也不同（基准帧是 P 帧
/// 也会置位）。
#[derive(Debug, Default)]
pub(crate) struct IdrTracker {
    /// 已承诺、但还没写完到链路上的 IDR 数（正常只会是 0 或 1，用计数是为了容错/重叠）。
    pending: AtomicU32,
    /// 最近一次 IDR **写完**的时刻（[`crate::clock::now_us`] 的单调微秒；0 = 还没写过）。
    written_us: AtomicU64,
}

impl IdrTracker {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 编码任务：承诺一个 IDR。**在真正编码之前**登记，覆盖「已决定编码 IDR」到「出包」
    /// 那十几毫秒——否则这段时间到达的请求会再插一个 IDR。
    fn commit(&self) {
        self.pending.fetch_add(1, Ordering::Relaxed);
    }

    /// 销账一个 IDR（编码失败/被跳过，或写任务写完）。
    fn release(&self) {
        let _ = self
            .pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            });
    }

    /// 写任务：一个 IDR 已经写完（销账并推进「刚写出」时间戳）。
    fn on_idr_written(&self, now_us: u64) {
        self.release();
        self.written_us.store(now_us, Ordering::Relaxed);
    }

    /// 当前在途（已承诺、未写完）的 IDR 数。
    fn in_flight(&self) -> u32 {
        self.pending.load(Ordering::Relaxed)
    }

    /// 最近一次 IDR 写完的时刻（微秒；0 = 还没写过）。
    fn last_written_us(&self) -> u64 {
        self.written_us.load(Ordering::Relaxed)
    }
}

/// 编码任务里的「已承诺 IDR」在途登记（RAII）。
///
/// 承诺点在真正编码之前，而编码循环里有好几条 `continue`（出站队列满、编码器创建/编码失败、
/// 空包）。任何一条没有走到 [`IdrInFlight::handoff`] 的路径都必须销账，否则 `pending` 会
/// 永久卡在 >0，把后续所有关键帧请求都吸收掉。用 `Drop` 兜住比在每个 `continue` 前手写一遍
/// 可靠得多。
struct IdrInFlight {
    tracker: Arc<IdrTracker>,
    armed: bool,
}

impl IdrInFlight {
    fn begin(tracker: Arc<IdrTracker>) -> Self {
        tracker.commit();
        Self {
            tracker,
            armed: true,
        }
    }

    /// 已交给写任务：由它在写完时销账。
    fn handoff(mut self) {
        self.armed = false;
    }
}

impl Drop for IdrInFlight {
    fn drop(&mut self) {
        if self.armed {
            self.tracker.release();
        }
    }
}

/// 一次**外部触发**（控制器 `RequestKeyframe`、补丁半批退出）的关键帧请求是否应被吸收。
///
/// 三种情形都说明已经有一个 IDR 在路上或刚刚上过线，再打一个只是同一幅画面重编一遍：
/// - `force_keyframe` 已为真：上一次请求还没被编码任务消费；
/// - `in_flight > 0`：编码任务已承诺一个 IDR，它还在编码/画面队列/写线途中（真机上一帧
///   IDR 写完要 190+ ms，这段时间最长）；
/// - 最近一次 IDR 写出不超过 [`KEYFRAME_DEDUP_WINDOW`]。
///
/// **只用于外部请求**：周期性 IDR、切显示器/切画质、几何变化、编码器重建是「参考链/几何真的
/// 失效」，必须无条件生效，不走这里。
#[must_use]
fn keyframe_request_is_redundant(
    force_keyframe: bool,
    in_flight: u32,
    last_written_us: u64,
    now_us: u64,
) -> bool {
    if force_keyframe || in_flight > 0 {
        return true;
    }
    let window_us = u64::try_from(KEYFRAME_DEDUP_WINDOW.as_micros()).unwrap_or(u64::MAX);
    last_written_us != 0 && now_us.saturating_sub(last_written_us) < window_us
}

/// 把补丁暂存区按 FIFO 补投进画面队列（**非阻塞**，队满即停）。
///
/// 两条副作用只在**真正入队之后**发生，与旧实现按条投递时一致：
/// - `last_patch_sent` 是补丁保活（空 `RegionUpdate`）的水位，只有发出去才算数；
/// - `meter` 是 QoS 投递率口径，只有真正交给画面队列的字节才计入。
///
/// # Errors
/// 写任务已结束（链路断开）时返回错误。
fn drain_patch_staging(
    staging: &mut PatchStaging,
    queues: &OutboundQueues,
    mode: &mut PatchMode,
    meter: &ScreenSendMeter,
) -> RemoteResult<usize> {
    staging.drain(|frame, bytes| {
        match queues.try_enqueue_screen_or_return(frame)? {
            None => {
                mode.last_patch_sent = Some(std::time::Instant::now());
                meter.add_bytes(bytes);
                Ok(None)
            }
            // 队满：把帧原样还回去，暂存区保序留到下一拍。
            Some(frame) => Ok(Some(frame)),
        }
    })
}

/// 一段已编码（压缩或原始）的横向条块，坐标是**目标帧缓冲**坐标系。
struct EncodedRegion {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    flags: u8,
    data: Vec<u8>,
}

/// 压缩一段紧包 NV12：压缩后**确实更小**才用 [`REGION_FLAG_ZSTD`]，否则存原始（极小块的
/// zstd 帧头开销比省下的字节还多）。
fn encode_nv12(compressor: &mut zstd::bulk::Compressor<'static>, nv12: &[u8]) -> (u8, Vec<u8>) {
    match compressor
        .compress(nv12)
        .ok()
        .filter(|data| data.len() < nv12.len())
    {
        Some(data) => (REGION_FLAG_ZSTD, data),
        None => (0u8, nv12.to_vec()),
    }
}

/// 从矩形的紧包 NV12 里取一段**横向条带**：`[y0, y1)` 行（两个端点都是偶数）的 Y 平面 +
/// 对应的 UV 行。
///
/// 布局是 `Y(width*height) ‖ UV(width*height/2)`、无行填充（见 [`ScreenPatch`]），所以：
/// - Y：`y0*width .. y1*width`
/// - UV 每行 `width` 字节、共 `height/2` 行，起始偏移 `width*height + (y0/2)*width`
///
/// 输出长度固定为 `width*(y1-y0)*3/2`。源缓冲长度不符时返回 `None`（宁可回全帧，也不读越界）。
fn nv12_band(rect: &ScreenPatch, y0: u32, y1: u32) -> Option<Vec<u8>> {
    let width = usize::try_from(rect.width).ok()?;
    let height = usize::try_from(rect.height).ok()?;
    let expected = width.checked_mul(height)?.checked_mul(3)? / 2;
    if rect.nv12.len() != expected {
        return None;
    }
    let top = usize::try_from(y0).ok()?;
    let bottom = usize::try_from(y1).ok()?;
    let y_start = top.checked_mul(width)?;
    let y_end = bottom.checked_mul(width)?;
    let uv_base = width.checked_mul(height)?;
    let uv_start = uv_base.checked_add(top.checked_div(2)?.checked_mul(width)?)?;
    let uv_end = uv_base.checked_add(bottom.checked_div(2)?.checked_mul(width)?)?;
    let mut band = Vec::with_capacity((y_end - y_start) + (uv_end - uv_start));
    band.extend_from_slice(rect.nv12.get(y_start..y_end)?);
    band.extend_from_slice(rect.nv12.get(uv_start..uv_end)?);
    Some(band)
}

/// 编码矩形 `[y0, y1)` 这一段行；超出单条消息上限时按高度二分递归，直到每段都装得下。
///
/// 切点取 `(span / 2) & !1`：**两个半区的高度都是偶数**（`y0` 本身是偶数，故子段的起点也
/// 是偶数），且都至少 2 行。所有递归产出的条带按顺序首尾相接、**无缝且无重叠**地覆盖原矩形。
fn encode_region_range(
    compressor: &mut zstd::bulk::Compressor<'static>,
    rect: &ScreenPatch,
    y0: u32,
    y1: u32,
    max_bytes: usize,
    out: &mut Vec<EncodedRegion>,
) -> Option<()> {
    let (flags, data) = if y0 == 0 && y1 == rect.height {
        encode_nv12(compressor, &rect.nv12)
    } else {
        let band = nv12_band(rect, y0, y1)?;
        encode_nv12(compressor, &band)
    };
    if data.len() <= max_bytes {
        out.push(EncodedRegion {
            x: rect.x,
            y: rect.y.checked_add(y0)?,
            width: rect.width,
            height: y1 - y0,
            flags,
            data,
        });
        return Some(());
    }
    // 放不下：按高度二分。2 行仍超上限时无法再拆——宽 ≤ 3840 时 2 行 NV12 ≤ 11520 B，
    // 不可能发生；真发生说明输入几何异常，交给调用方回全帧。
    let span = y1 - y0;
    if span < 4 {
        return None;
    }
    let mid = y0 + ((span / 2) & !1);
    encode_region_range(compressor, rect, y0, mid, max_bytes, out)?;
    encode_region_range(compressor, rect, mid, y1, max_bytes, out)
}

/// 把一批采集到的脏矩形装成**任意条** `RegionUpdate`（生产路径，单条上限
/// [`PATCH_MESSAGE_MAX_BYTES`]）。
///
/// 拆批/装箱逻辑见 [`build_region_updates_with_cap`]。
fn build_region_updates(
    compressor: &mut zstd::bulk::Compressor<'static>,
    display: u32,
    patches: &CapturedPatches,
    capture_ts_us: u64,
) -> Option<Vec<SessionMessage>> {
    build_region_updates_with_cap(
        compressor,
        display,
        patches,
        capture_ts_us,
        PATCH_MESSAGE_MAX_BYTES,
    )
}

/// 把一批采集到的脏矩形装成**任意条** `RegionUpdate`。
///
/// 每个矩形独立压缩；单个矩形编码后超过 `message_max_bytes` 时按行拆成若干横条
/// （见 [`encode_region_range`]），再按顺序装箱：每条消息的总字节不超过
/// `message_max_bytes`、矩形数不超过 [`MAX_REGION_RECTS`]。
///
/// `message_max_bytes` 做成参数只为了单测能用很小的上限触发拆分（不必构造几十 KB 的
/// 随机数据）；生产路径见 [`build_region_updates`]。
///
/// 返回 `None` 只代表**真的装不下**：矩形数超过 [`MAX_REGION_RECTS`]、单拍总量超过
/// [`PATCH_MAX_TICK_BYTES`]、或消息条数超过 [`PATCH_MAX_MESSAGES_PER_TICK`]。调用方据此回
/// 全帧。空矩形列表仍然产出一条空消息（保活语义不变）。
///
/// `capture_ts_us` 是本批采集完成的时刻，原样写进每条消息供控制器算端到端延迟。
fn build_region_updates_with_cap(
    compressor: &mut zstd::bulk::Compressor<'static>,
    display: u32,
    patches: &CapturedPatches,
    capture_ts_us: u64,
    message_max_bytes: usize,
) -> Option<Vec<SessionMessage>> {
    if patches.rects.len() > MAX_REGION_RECTS {
        return None;
    }
    if patches.rects.is_empty() {
        return Some(vec![SessionMessage::RegionUpdate {
            display,
            width: patches.width,
            height: patches.height,
            rects: Vec::new(),
            capture_ts_us,
        }]);
    }

    // 1) 逐矩形编码；超限的按行拆成横条，直到每段都 ≤ 单条消息上限。
    let mut encoded = Vec::new();
    for rect in &patches.rects {
        // `ScreenPatch` 的契约是偶数几何；不合契约就回全帧，不硬拆（拆出来会错位）。
        if rect.width == 0
            || rect.height == 0
            || !rect.width.is_multiple_of(2)
            || !rect.height.is_multiple_of(2)
        {
            return None;
        }
        encode_region_range(
            compressor,
            rect,
            0,
            rect.height,
            message_max_bytes,
            &mut encoded,
        )?;
    }

    // 2) 按顺序装箱：尽量把条块塞进同一条消息，装不下就开下一条。
    let mut messages: Vec<SessionMessage> = Vec::new();
    let mut rects: Vec<RegionRect> = Vec::new();
    let mut message_bytes = 0usize;
    let mut tick_bytes = 0usize;
    for region in encoded {
        let len = region.data.len();
        if len > message_max_bytes {
            return None;
        }
        if !rects.is_empty()
            && (message_bytes.saturating_add(len) > message_max_bytes
                || rects.len() >= MAX_REGION_RECTS)
        {
            messages.push(SessionMessage::RegionUpdate {
                display,
                width: patches.width,
                height: patches.height,
                rects: std::mem::take(&mut rects),
                capture_ts_us,
            });
            if messages.len() > PATCH_MAX_MESSAGES_PER_TICK {
                return None;
            }
            message_bytes = 0;
        }
        message_bytes = message_bytes.saturating_add(len);
        tick_bytes = tick_bytes.saturating_add(len);
        if tick_bytes > PATCH_MAX_TICK_BYTES {
            return None;
        }
        rects.push(RegionRect {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
            flags: region.flags,
            data: region.data,
        });
    }
    if !rects.is_empty() {
        messages.push(SessionMessage::RegionUpdate {
            display,
            width: patches.width,
            height: patches.height,
            rects,
            capture_ts_us,
        });
    }
    if messages.len() > PATCH_MAX_MESSAGES_PER_TICK {
        return None;
    }
    Some(messages)
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

/// 计量窗口换算：把「累计字节差值」按**实际间隔**算成 kbps。
///
/// 抽成纯函数便于单测：`elapsed` 为 0（两次反馈落在同一毫秒/计时精度不足）时由
/// [`kbps_from_bytes`] 的 `max(1 ms)` 兜底，**绝不除零**。
fn window_kbps(prev_bytes: u64, now_bytes: u64, elapsed: Duration) -> u32 {
    kbps_from_bytes(now_bytes.saturating_sub(prev_bytes), elapsed)
}

/// 画面出站字节计量器（进程内，不上线）。
///
/// 只累计**真正交给画面队列**的载荷字节：视频是 `VideoPacket.data.len()`，补丁是各
/// `RegionRect.data.len()` 之和。它与编码任务的 [`EncodeStatsWindow`] **互不影响**——后者
/// 仍是「编码侧出码」，角标的「目标/实际码率」继续用它。
///
/// 速率**不在这里按固定节拍采样**：由会话主循环在每次收到 `QosFeedback` 时用累计差值与
/// 实际间隔现算（见 `run_controlled` 的反馈分支）。这样发出的窗口恰好是「上一条反馈 → 本
/// 条反馈」，与控制器 `recv_kbps` 的窗口只差一个单向延迟，不会因两个独立 1 s 计时器的相位
/// 差把突发流量算成 `0.2×` 或 `5×`。
struct ScreenSendMeter {
    /// 累计交给画面队列的载荷字节数（编码任务与会话主循环都累加）。
    bytes: AtomicU64,
}

impl ScreenSendMeter {
    fn new() -> Self {
        Self {
            bytes: AtomicU64::new(0),
        }
    }

    /// 记一笔真正交付给画面队列的载荷字节。
    fn add_bytes(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 累计字节数（主循环据此算窗口差值）。
    fn total_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
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
    /// [`crate::screen::native_bitrate_ceiling`] 按显示器面积放大（封顶 60 Mbps）；VP9 路径不受影响。
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
    let (capture_fallbacks, capture_open_fallbacks, no_new_frame) = last_info
        .replace(capture.clone())
        .map_or((0, 0, 0), |prev| {
            (
                u32::try_from(capture.fallbacks.saturating_sub(prev.fallbacks)).unwrap_or(u32::MAX),
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
        scale_permille: (encode.scale * 1000.0)
            .round()
            .clamp(0.0, f32::from(u16::MAX)) as u16,
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
        return Err(RemoteError::Handshake(format!(
            "期望 Hello，实得 {:?}",
            hello.message
        )));
    };

    // 版本与 session 一致性检查；不通过则明文拒绝。
    if proto_version != SESSION_PROTO_VERSION || session_id != config.session_id {
        let reason = format!("握手参数不匹配（proto={proto_version}, session={session_id}）");
        reject_handshake(&mut writer, &reason).await;
        return Err(RemoteError::Handshake(reason));
    }

    // 鉴权：常数时间比对 HMAC；失败/过期/重放一律拒绝并记原因。
    // 拿住返回的 `PendingSession`——它的 token 还要进 KDF 做密钥交换认证。
    let pending =
        match config
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
    let hmac_ack = ack_mac(
        &pending.token,
        &config.session_id,
        &responder_pub,
        &x25519_pub,
    );
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
    // IDR 在途/刚写出的共享观测点：编码任务登记、写任务销账、会话循环据此吸收重复的
    // 关键帧请求（见 [`keyframe_request_is_redundant`]）。只在本端参与调度，**不上线**。
    let idr_tracker = IdrTracker::new();
    // 诊断：主循环 / 编码任务 / 写任务共写一份延迟窗口，由主循环每秒取走并打一行汇总
    // （见 `crate::latency`）。未开延迟诊断时只有一次 `enabled()` 判断。
    let latency_window: Arc<std::sync::Mutex<LatencyWindow>> =
        Arc::new(std::sync::Mutex::new(LatencyWindow::new()));
    let writer_task = tokio::spawn(writer_loop(
        writer,
        receivers,
        cancel.clone(),
        Some(Arc::clone(&latency_window)),
        Some(Arc::clone(&idr_tracker)),
    ));

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
        .enqueue_ctrl(
            &cancel,
            SessionFrame::control(SessionMessage::CodecSelect { codec }),
        )
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
    // 画面出站计量器：编码任务（视频包）与会话主循环（补丁）都往里累加**真正交给画面队列**
    // 的字节。主循环在**每条 `QosFeedback`** 到来时用差值算出该窗口的 `sent_kbps`——窗口
    // 与控制器 `recv_kbps` 的窗口对齐（只差一个单向延迟），不受两个 1 s 计时器相位差影响。
    // 它只在进程内传递，**不进** `QosFeedback`（线上字段不变）。
    //
    // **不能**沿用编码任务的 `EncodeStatsWindow`：补丁模式下没有编码任务，那个值会冻结在
    // 进入补丁模式前的一秒，而 `recv_kbps` 是控制器当前窗口（含补丁），两者不可比——真机
    // 日志里 `sent_kbps=643 / recv=69` 与 `sent=57 / recv=1617` 都是这么来的。
    let screen_send_meter = Arc::new(ScreenSendMeter::new());
    // 补丁模式的「基准全帧已写出」信号：由编码任务在该帧**投进出站队列之后**置位，
    // 会话循环只在看到它之后才切补丁模式（顺序不变量，见模块文档）。基准帧是 P 帧，
    // 所以信号不能只看 `keyframe`（见 [`encode_loop`]）。
    let base_frame_sent = Arc::new(AtomicBool::new(false));
    // 最近一次**真正编码出关键帧**的时刻（本端单调时钟微秒，见 [`crate::clock::now_us`]）。
    // 由编码任务写、主循环读，用来做周期性 IDR 兜底（[`PERIODIC_KEYFRAME_INTERVAL`]）。
    // 初值 0 = 「还没有关键帧」：主循环据此跳过判断，等首帧 IDR 把值写上。
    let last_keyframe_us = Arc::new(AtomicU64::new(0));
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
        Arc::clone(&screen_send_meter),
        Arc::clone(&base_frame_sent),
        Arc::clone(&last_keyframe_us),
        max_encode_width,
        Arc::clone(&encode_snapshot),
        Arc::clone(&latency_window),
        Arc::clone(&idr_tracker),
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
    // 另一套持久帧缓冲（见 [`ScreenSource::supports_patches`]）。
    // **压缩在采集任务的 `spawn_blocking` 里做**（见 `run_capture` / `build_patch_batch`），
    // 不再占用会话循环——补丁模式下 zstd 曾同步阻塞循环 10–40 ms，直接拖慢采集节拍。
    let patch_capable = codec == CodecId::H264 && screen.supports_patches();
    // 编码参数自阶段 3 起分两层：`config.encode` 是档位**上限**，`encode` 是闭环**当前生效值**
    // （起步 = 上限）。`SetQuality` 换上限，`QosFeedback` 动生效值。
    //
    // 原生分辨率会话按**显示器面积**放大上限（4K = 4×，封顶 60 Mbps）：4K 桌面文字在
    // 10 Mbps 下明显不够。只改上限——QoS 仍可在其下下调，且不触碰帧率/缩放。
    // 用实际显示器像素而非开关状态：1080p 显示器拿不到任何放大。
    let initial_display = displays
        .iter()
        .find(|d| d.primary)
        .or_else(|| displays.first());
    let mut rate = RateController::new(session_encode_options(
        config.encode,
        native_bitrate_permille(initial_display, native_active),
    ));
    // 内容面积上限按**编码器类型**开关：VP9 是 CBR（静止画面也会把目标码率灌满，需要这层
    // 省带宽）；H.264 是峰值受限 VBR（没有残差时只发几 KB），上限只会压低 VBV 单帧预算，
    // 让画面一变就被量化糊——画质优先，关闭。
    rate.set_content_caps_enabled(codec == CodecId::Vp9);
    let mut encode = rate.current();
    // pacing 按**链路**码率（AIMD）摊开：内容上限压的是「这一帧值多少比特」，不是链路速率
    // （见 [`RateController::keyframe_bitrate_kbps`]）。
    queues.set_screen_pacing_kbps(rate.keyframe_bitrate_kbps());
    let mut display_id = initial_display.map_or(0, |d| d.id);

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
    // 采集的**固定相位**截止时刻：tick 触发时从上一拍的计划时刻推进周期，而不是从
    // `Instant::now()` 重新计时。否则循环里每拍的处理时间（输入注入、采集结果处理等）会被
    // 叠加进周期，33 ms 的节拍会漂成 ~45 ms（真机实测只有 22 fps）。
    let mut next_capture_at = Instant::now() + encode.capture_interval();
    // 采集节拍诊断（每秒遥测）：实际间隔、慢拍数、因上一拍未返回而跳过的次数、采集等待。
    let mut last_capture_tick: Option<Instant> = None;
    // 输入触发采集（见 [`input_capture_target`]）：采集在途时收到的输入只记一个标志，
    // 等结果回来再立刻补拍——同一时刻绝不并发第二次采集（会把阻塞线程池堆满）。
    let mut input_capture_pending = false;
    let mut tick_dt_sum_ms = 0_u64;
    let mut tick_dt_count = 0_u32;
    let mut tick_slow = 0_u32;
    let mut tick_spawn_skip = 0_u32;
    let mut capture_request_us: Option<u64> = None;
    let mut cap_wait_sum_ms = 0_u64;
    let mut cap_wait_count = 0_u32;
    let mut cap_wait_max_ms = 0_u64;
    let mut input_events = 0_u32;
    let mut display_max_ms = 0_u64;
    // 诊断（见 [`crate::latency`]）：最近一次**成功注入**输入的时刻（微秒）。每次注入覆盖，
    // 直到某一拍采集的变化面积 ≥ [`INPUT_CHANGE_PERMILLE`] 才清掉——它量的是「用户点了，
    // 画面多久才真的开始变」（远端应用重绘 + 我们的采集），不是「下一拍采集多久到」。
    let mut last_input_us: Option<u64> = None;
    // 诊断：尚未被采集消费的输入时刻（`InputToCapture` 用）。与 `last_input_us` **分开**：
    // 后者要留到「有明显变化」才清，前者只被第一拍采集消费。
    let mut pending_input_capture_us: Option<u64> = None;
    // 诊断开关**每帧读一次**（Relaxed 原子）：设置页运行中切换要立即生效，不能缓存成局部量。
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
    // 「必须真正编码一帧」与「必须是 IDR」是两件事：补丁模式的基准帧、退出补丁模式后的第一帧
    // 都只需 P 帧（参考链没断），但不能被「无变化就跳过」的闸门吞掉。
    let mut force_encode = false;
    // 脏矩形补丁模式的状态机（见模块文档）。`patch_capable == false` 时它永远停在 `Video`。
    let mut screen_mode = PatchMode::new();
    // 补丁出站的有界暂存区：画面队列瞬时满时整拍补丁先留在这里、下一拍补投（见
    // [`PatchStaging`]）。它是「主循环绝不 `.await` 画面队列」与「补丁绝不丢」同时成立的
    // 唯一办法；只有超龄/超字节上限（写侧真的跟不上）才退出补丁模式。
    let mut patch_staging = PatchStaging::new();
    // 最近一帧采集报告的**变化面积**（千分比）：逐帧喂 `RateController::promote_activity`
    // （升档立刻生效），每秒再随 `QosFeedback` 喂 `observe_activity`（带迟滞的降档）。
    // 初值 1000 = 面积未知/满帧，保守地不收紧上限。
    let mut last_changed_permille: u16 = 1000;
    // C5：采集调用返回错误（`CapturePayload::Frame(None)`）的每秒增量，由主循环累计、
    // 心跳拍清零。
    let mut capture_failures: u32 = 0;
    // `CaptureInfo` 的三个计数是**累计值**，主循环保存上一份求差后才是每秒增量。
    let mut last_capture_info: Option<CaptureInfo> = None;
    // 上一次发给控制器的光标形状。只在**变化时**下发；`None`（平台读不到）时既不发也不清
    // 它——一次读取失败不该让控制端的 I 形光标闪回箭头。
    let mut last_cursor: Option<CursorShape> = None;
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
    // QoS 投递率的出码窗口：**每条 `QosFeedback` 推进一次**（不是固定 1 s 定时器），窗口
    // 恰好是「上一条反馈 → 本条反馈」，与控制器 `recv_kbps` 的窗口只差一个单向延迟。
    // `meter_primed == false` 时还没有窗口（首条反馈按无样本处理）。
    // 推进发生在两个 `QosFeedback` 分支之前，因此补丁模式下被忽略的反馈也会推进窗口——
    // 否则退出补丁模式后的第一窗口会把整个补丁期间的字节都算进一秒。
    let mut meter_bytes = 0_u64;
    let mut meter_at = Instant::now();
    let mut meter_primed = false;
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
                // 周期 IDR 兜底（1 Hz 心跳拍顺带检查，不额外引定时器）：全帧视频下若距上个
                // 关键帧已超过 [`PERIODIC_KEYFRAME_INTERVAL`]，主动置一次 `force_keyframe`。
                // 它只兜「控制端整段没恢复」（丢包风暴、请求被限频吞掉）；正常会话里静止 P 帧
                // 保活已经让画面持续流动，这个分支几乎不会命中。
                //
                // `last_keyframe_us == 0` = 还没有关键帧（会话刚起）：跳过，等首帧 IDR 写值，
                // 否则会话一开始就会白插一个重复的 IDR。`force_keyframe` 已为真时也不必重复。
                let last_kf_us = last_keyframe_us.load(Ordering::Relaxed);
                let periodic_us = u64::try_from(PERIODIC_KEYFRAME_INTERVAL.as_micros())
                    .unwrap_or(u64::MAX);
                if screen_mode.mode == ScreenMode::Video
                    && !force_keyframe
                    && last_kf_us != 0
                    && crate::clock::now_us().saturating_sub(last_kf_us) >= periodic_us
                {
                    force_keyframe = true;
                    debug!(
                        age_ms = crate::clock::now_us().saturating_sub(last_kf_us) / 1000,
                        "距上个关键帧超过周期兜底间隔，强制刷新 IDR"
                    );
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
                // 诊断：每秒打印被控端真实遥测（**debug**；默认 info 下静默）。`fps` 是
                // **实测**编码帧率（不是目标值），配合 `capture_us` / `encode_ms` /
                // `out_queue_skips` 可判断掉帧发生在采集、编码还是发送背压。
                debug!(
                    fps = host.fps,
                    capture_us = host.capture_us,
                    encode_ms = host.encode_ms,
                    skipped = host.skipped,
                    keyframes = host.keyframes,
                    idr_kbps = host.idr_kbps,
                    p_kbps = host.p_kbps,
                    changed_permille = host.changed_permille,
                    out_queue_depth = host.out_queue_depth,
                    out_queue_skips = host.out_queue_skips,
                    encoder_rebuilds = host.encoder_rebuilds,
                    forced_idrs = host.forced_idrs,
                    patches = host.patch_mode,
                    activity = ?host.activity,
                    encoder_backend = %host.encoder_backend,
                    encoder_name = %host.encoder_name,
                    capture_backend = %host.capture_backend,
                    gpu_capture = host.gpu_capture,
                    gpu_encode = host.gpu_encode,
                    encode_width = host.encode_width,
                    encode_height = host.encode_height,
                    scale_permille = host.scale_permille,
                    tick_ms = if tick_dt_count > 0 {
                        tick_dt_sum_ms / u64::from(tick_dt_count)
                    } else {
                        0
                    },
                    tick_slow,
                    tick_spawn_skip,
                    cap_wait_ms = if cap_wait_count > 0 {
                        cap_wait_sum_ms / u64::from(cap_wait_count)
                    } else {
                        0
                    },
                    cap_wait_max_ms,
                    input_events,
                    display_max_ms,
                    "被控端每秒遥测"
                );
                // 诊断：每秒一行分阶段延迟汇总（**只在延迟诊断打开时**）。
                // 与上面的 `debug!` 分开：那条是既有遥测（debug 级），这条是给「切标签为什么
                // 慢」用的拆解（info 级、默认完全静默）。窗口由主循环/编码任务/写任务共写。
                if latency::enabled() {
                    if let Some(summary) = latency::take_summary(&latency_window) {
                        // 全部信息塞进 message：GUI 的日志面板 / 服务端日志只保留 `message`
                        // 字段（结构化字段会被丢掉），文件与 stdout 则两种都完整。
                        info!(
                            target: "latency",
                            "controlled fps={} patches={} {}",
                            host.fps,
                            host.patch_mode,
                            summary
                        );
                    }
                }
                // 本秒诊断计数已上报，清零后重新累计。
                tick_dt_sum_ms = 0;
                tick_dt_count = 0;
                tick_slow = 0;
                tick_spawn_skip = 0;
                cap_wait_sum_ms = 0;
                cap_wait_count = 0;
                cap_wait_max_ms = 0;
                input_events = 0;
                display_max_ms = 0;
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
                    Some(CaptureOutcome {
                        payload,
                        started_us,
                        capture_us,
                        capture_ts_us,
                        cursor,
                        ..
                    }) => {
                        capture_deadline = None;
                        capture_timeouts = 0;
                        // 采集在途期间有输入到达：结果一回来就立刻安排下一拍，不再等满一个
                        // 周期（见 [`input_capture_target`]）。这里用当前模式/生效帧率重算，
                        // 因为采集期间可能刚换过档位或进出过补丁模式。
                        if input_capture_pending {
                            input_capture_pending = false;
                            if let Some(target) = input_capture_pull(
                                Instant::now(),
                                last_capture_tick,
                                capture_tick_period(screen_mode.mode, &encode),
                                next_capture_at,
                            ) {
                                schedule_capture(&mut capture_tick, &mut next_capture_at, target);
                            }
                        }
                        // 远端光标形状：**只在变化时**发一条控制帧。`None` 表示平台不支持/读不到，
                        // 此时不发消息、也**不清** `last_cursor`（一次读取失败不该让控制端的光标
                        // 闪回箭头）。形状变化与鼠标是否移动无关，控制端据此更新图标。
                        if let Some(shape) = cursor {
                            if last_cursor != Some(shape) {
                                last_cursor = Some(shape);
                                if let Err(err) = queues
                                    .enqueue_ctrl(
                                        &cancel,
                                        SessionFrame::control(SessionMessage::CursorShape {
                                            shape,
                                        }),
                                    )
                                    .await
                                {
                                    break Err(err);
                                }
                            }
                        }
                        let mut capture_wait_us: Option<u64> = None;
                        if let Some(request_us) = capture_request_us.take() {
                            let wait_ms = capture_ts_us.saturating_sub(request_us) / 1000;
                            cap_wait_sum_ms = cap_wait_sum_ms.saturating_add(wait_ms);
                            cap_wait_count = cap_wait_count.saturating_add(1);
                            cap_wait_max_ms = cap_wait_max_ms.max(wait_ms);
                            let startup_us = started_us.saturating_sub(request_us);
                            capture_wait_us = Some(startup_us);
                            if latency::enabled() {
                                // 诊断拆分：派发 → 采集任务真正开始（阻塞线程池/调度等待）
                                // 与采集本身。两者之和就是上面的 `cap_wait`/`capture_us`。
                                latency::record(
                                    &latency_window,
                                    Stage::CaptureWait,
                                    startup_us,
                                );
                                latency::record(&latency_window, Stage::Capture, capture_us);
                            }
                        }
                        // 诊断：两条输入交互延迟（见 [`Stage::InputToCapture`] /
                        // [`Stage::InputToChange`]）。必须在 `match payload` 消费它之前算。
                        let mut input_to_capture_us = None;
                        let mut input_to_change_us = None;
                        if latency::enabled() {
                            let changed = match &payload {
                                CapturePayload::Frame(Some(frame)) => frame.changed_permille,
                                CapturePayload::Patches(Some(batch)) => batch.changed_permille,
                                _ => 0,
                            };
                            if let Some(input_us) = pending_input_capture_us {
                                if capture_ts_us >= input_us {
                                    let us = capture_ts_us.saturating_sub(input_us);
                                    latency::record(&latency_window, Stage::InputToCapture, us);
                                    input_to_capture_us = Some(us);
                                    pending_input_capture_us = None;
                                }
                            }
                            if let Some(input_us) = last_input_us {
                                // 只有**用户看得出来变了**的那一帧才算「画面开始变」；
                                // 悬停高亮之类的小重绘不消耗这个起点。
                                if changed >= INPUT_CHANGE_PERMILLE && capture_ts_us >= input_us {
                                    let us = capture_ts_us.saturating_sub(input_us);
                                    latency::record(&latency_window, Stage::InputToChange, us);
                                    input_to_change_us = Some(us);
                                    last_input_us = None;
                                }
                            }
                        }
                        match payload {
                            CapturePayload::Frame(frame) => {
                                if let Some(frame) = frame {
                                    debug!(capture_us, "采集一帧");
                                    last_changed_permille = frame.changed_permille;
                                    // 逐帧升档：用户一开始拖窗/切窗口就放量，不等下一次 1 Hz 反馈
                                    // （那正是「一开始操作先顿一下」的根因）。
                                    refresh_activity(
                                        &mut rate,
                                        &mut encode,
                                        &queues,
                                        &mut capture_tick,
                                        &mut next_capture_at,
                                        screen_mode.mode,
                                        last_changed_permille,
                                    );
                                    // 进入判据只喂**视频**采集：连续多次低活跃才值得付补丁模式的
                                    // 固定开销。命中时本帧必须充当**基准全帧**（见模块文档的顺序约束）。
                                    //
                                    // 必须排除已在补丁模式的情况：切换瞬间可能还有一次在途的
                                    // 视频采集结果，不排除的话它会把计数攒到阈值、在补丁模式里
                                    // 又发一次强制全帧。
                                    if patch_capable
                                        && !screen_mode.is_patches()
                                        && screen_mode.observe_video(
                                            frame.changed_permille,
                                            std::time::Instant::now(),
                                        )
                                    {
                                        // 基准帧必须是 **IDR**：补丁只覆盖变化矩形，静态区域
                                        // 沿用这一帧。若它继承了一张糊的参考（切标签/整页重绘
                                        // 那一帧被单帧预算压过），整页会长期糊住——补丁模式里
                                        // 编码器不跑，intra refresh 也停了，没有别的自愈路径。
                                        force_keyframe = true;
                                        force_encode = true;
                                        base_frame_sent.store(false, Ordering::Relaxed);
                                    }
                                    // 内容上限按**编码像素数**等比放大：1080p 的 1.2 Mbps 在 4K
                                    // 上只有四分之一的每像素比特，文字会被量化糊（见 `qos` 的
                                    // `set_content_area_permille`）。几何变化本来就要重建 + 关键帧，
                                    // 所以这里每帧设置一次没有额外代价（内部按值去重）。
                                    rate.set_content_area_permille(content_area_permille(
                                        frame.width,
                                        frame.height,
                                        encode.scale,
                                        max_encode_width,
                                    ));
                                    let job = EncodeJob {
                                        display: display_id,
                                        rates: encode,
                                        keyframe_bitrate_kbps: rate.keyframe_bitrate_kbps(),
                                        force_keyframe,
                                        force_encode,
                                        capture_us,
                                        capture_wait_us,
                                        capture_ts_us,
                                        input_to_capture_us,
                                        input_to_change_us,
                                        frame,
                                    };
                                    // 有界(1) 队列满 = 编码/发送侧还没消化完上一帧 → **编码之前**跳过这一
                                    // 帧（等价降帧率）。关键帧/强制编码请求保留到真正投递成功的那一次。
                                    match encode_tx.try_send(job) {
                                        Ok(()) => {
                                            force_keyframe = false;
                                            force_encode = false;
                                        }
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
                                } else if let Some(batch) = patches {
                                    let PatchBatch {
                                        width,
                                        height,
                                        changed_permille,
                                        rect_count,
                                        empty,
                                        messages,
                                        bytes,
                                    } = batch;
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
                                    if width != want_w || height != want_h {
                                        warn!(
                                            got_w = width,
                                            got_h = height,
                                            want_w,
                                            want_h,
                                            "补丁尺寸与编码尺寸不符，回到全帧"
                                        );
                                        // 尺寸不符 = 几何变化：参考链与控制器合成缓冲都作废，
                                        // 退出后必须重发 IDR。
                                        exit_patch_mode(
                                            &mut screen_mode,
                                            &mut patch_staging,
                                            &mut force_encode,
                                            &mut last_changed_permille,
                                            &mut capture_tick,
                                            &mut next_capture_at,
                                            PatchExitReason::SizeMismatch,
                                        );
                                        force_keyframe = true;
                                    } else if changed_permille > PATCH_EXIT_PERMILLE {
                                        debug!(changed_permille, "画面活跃度回升，退出补丁模式");
                                        // 只是画面变忙：参考链完好，退出后的第一帧 P 帧即可。
                                        exit_patch_mode(
                                            &mut screen_mode,
                                            &mut patch_staging,
                                            &mut force_encode,
                                            &mut last_changed_permille,
                                            &mut capture_tick,
                                            &mut next_capture_at,
                                            PatchExitReason::Activity,
                                        );
                                    } else if rect_count > MAX_REGION_RECTS {
                                        warn!(rects = rect_count, "脏矩形过多，退出补丁模式");
                                        exit_patch_mode(
                                            &mut screen_mode,
                                            &mut patch_staging,
                                            &mut force_encode,
                                            &mut last_changed_permille,
                                            &mut capture_tick,
                                            &mut next_capture_at,
                                            PatchExitReason::Rects,
                                        );
                                    } else {
                                        last_changed_permille = changed_permille;
                                        // 补丁模式下 QoS 闭环暂停，但逐帧升档仍然有效：只要补丁
                                        // 报告的面积跨过 Mixed/Active 边界，退出后的全帧帧率就
                                        // 已经放开（降档仍归 1 Hz 反馈的迟滞）。
                                        refresh_activity(
                                            &mut rate,
                                            &mut encode,
                                            &queues,
                                            &mut capture_tick,
                                            &mut next_capture_at,
                                            screen_mode.mode,
                                            last_changed_permille,
                                        );
                                        // 空矩形 = 本拍没有变化：只在保活间隔到点时发一条空
                                        // `RegionUpdate`（控制器侧的「长时间无帧」看门狗靠它）。
                                        let keepalive_due = screen_mode
                                            .last_patch_sent
                                            .is_none_or(|at| at.elapsed() >= PATCH_KEEPALIVE);
                                        if empty && !keepalive_due {
                                            debug!(capture_us, "补丁无变化且未到保活间隔，跳过");
                                        } else if let Some(messages) = messages {
                                            // 压缩已在采集任务里完成；这里只做带宽预算与入队。
                                            // 带宽兜底按**单拍总量**只判一次：补丁模式下 QoS
                                            // 闭环暂停，若一个窗口的补丁出码超过「有效码率 × 2」
                                            // 就回全帧——H.264 整帧在同样字节下至少是正常码流，
                                            // 继续发补丁只会更糟。
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
                                                let count = messages.len();
                                                // 整拍补丁**整批**进暂存区，而不是逐条 try 投递：
                                                // 队列满时不会留下「本批只发了一半」的中间态。
                                                //
                                                // 为什么不能像旧实现那样「投不进去就退出补丁模式」：
                                                // 画面队列只有 [`SCREEN_QUEUE`] 两深，刚进补丁模式
                                                // 时排在前面的基准 IDR 还在写（真机 ~240 KB / 190+
                                                // ms），第一条必然投不进去；退出又强制一个 IDR，于是
                                                // 「进入 → 5 ms 后 abort → 再打 IDR」自激。
                                                if patch_staging
                                                    .push_batch(messages, std::time::Instant::now())
                                                {
                                                    debug!(
                                                        capture_us,
                                                        messages = count,
                                                        rects = rect_count,
                                                        bytes,
                                                        staged_bytes = patch_staging.bytes(),
                                                        "补丁已进暂存区"
                                                    );
                                                    // 队列通常有空位：立刻补投一次，不必等下一拍
                                                    // （否则每条补丁白等最多一个采集周期）。
                                                    let drained = match drain_patch_staging(
                                                        &mut patch_staging,
                                                        &queues,
                                                        &mut screen_mode,
                                                        &screen_send_meter,
                                                    ) {
                                                        Ok(drained) => drained,
                                                        Err(err) => break Err(err),
                                                    };
                                                    if drained < count {
                                                        debug!(
                                                            drained,
                                                            pending = count - drained,
                                                            "画面队列暂无空位，补丁留在暂存区等下一拍"
                                                        );
                                                    }
                                                } else {
                                                    // 暂存区都装不下 = 写侧已经连续一整段没消化。
                                                    // 这**才是**真的跟不上：退回全帧（必要时 IDR）。
                                                    warn!(
                                                        bytes,
                                                        staged_bytes = patch_staging.bytes(),
                                                        rects = rect_count,
                                                        "补丁暂存区已满，写侧持续跟不上，退出补丁模式"
                                                    );
                                                    let partial = patch_staging.partial();
                                                    exit_patch_mode(
                                                        &mut screen_mode,
                                                        &mut patch_staging,
                                                        &mut force_encode,
                                                        &mut last_changed_permille,
                                                        &mut capture_tick,
                                                        &mut next_capture_at,
                                                        PatchExitReason::StagingFull,
                                                    );
                                                    // 半批已发出：控制端合成缓冲缺块，必须一帧完整
                                                    // IDR。它与控制器自己的 `RequestKeyframe` 走
                                                    // **同一道去重闸门**——同一 IDR 不在 0.8 秒里打
                                                    // 四遍。
                                                    if partial
                                                        && !keyframe_request_is_redundant(
                                                            force_keyframe,
                                                            idr_tracker.in_flight(),
                                                            idr_tracker.last_written_us(),
                                                            crate::clock::now_us(),
                                                        )
                                                    {
                                                        force_keyframe = true;
                                                    }
                                                }
                                            } else {
                                                warn!(
                                                    bytes,
                                                    budget_kbps,
                                                    "补丁出码超过窗口预算，退出补丁模式"
                                                );
                                                exit_patch_mode(
                                                    &mut screen_mode,
                                                    &mut patch_staging,
                                                    &mut force_encode,
                                                    &mut last_changed_permille,
                                                    &mut capture_tick,
                                                    &mut next_capture_at,
                                                    PatchExitReason::Budget,
                                                );
                                            }
                                        } else {
                                            warn!(
                                                capture_us,
                                                messages = 0,
                                                rects = rect_count,
                                                "补丁无法装入单拍预算，退出补丁模式"
                                            );
                                            exit_patch_mode(
                                                &mut screen_mode,
                                                &mut patch_staging,
                                                &mut force_encode,
                                                &mut last_changed_permille,
                                                &mut capture_tick,
                                                &mut next_capture_at,
                                                PatchExitReason::Budget,
                                            );
                                        }
                                    }
                                } else {
                                    // 补丁采集返回 None：本拍不适合补丁（活跃度/矩形数超限、
                                    // 检测不可用……），立刻回全帧。参考链没断，第一帧 P 帧即可。
                                    debug!("补丁采集未产出补丁，返回全帧视频");
                                    exit_patch_mode(
                                        &mut screen_mode,
                                        &mut patch_staging,
                                        &mut force_encode,
                                        &mut last_changed_permille,
                                        &mut capture_tick,
                                        &mut next_capture_at,
                                        PatchExitReason::NoPatches,
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
                capture_request_us = None;
                // 在途期间有输入到达：超时放弃的这一次不算「刚采过」，立刻补一拍（见
                // [`input_capture_target`]）。否则这个标志会挂到下一次正常采集的结果上，
                // 让交互延迟多等一个周期。
                if input_capture_pending {
                    input_capture_pending = false;
                    if let Some(target) = input_capture_pull(
                        Instant::now(),
                        last_capture_tick,
                        capture_tick_period(screen_mode.mode, &encode),
                        next_capture_at,
                    ) {
                        schedule_capture(&mut capture_tick, &mut next_capture_at, target);
                    }
                }
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
                // 进入判据：基准帧**真正写出**之后才切模式（顺序不变量，见模块文档）。
                match screen_mode.poll_entry(
                    base_frame_sent.load(Ordering::Relaxed),
                    std::time::Instant::now(),
                ) {
                    Some(true) => info!("进入脏矩形补丁模式"),
                    Some(false) => debug!("等待基准帧超时，放弃进入补丁模式"),
                    None => {}
                }
                // 补丁暂存区：每拍先按 FIFO 补投（非阻塞），再判滞留是否已超龄。
                //
                // 补投必须放在采集**派发之前**：排在前面的基准 IDR 一写完，补丁就在这一拍
                // 立刻上路，而不是等下一拍（最多白等一个采集周期）。超龄是唯一会触发退出的
                // 时间条件——它量的是「头部这条补丁等了多久」，而不是「队列满没满」：队列满
                // 本身是瞬态（一帧大 IDR 写完就腾空），退出并强制 IDR 只会把瞬态放大成自激。
                if screen_mode.is_patches() {
                    let drained = match drain_patch_staging(
                        &mut patch_staging,
                        &queues,
                        &mut screen_mode,
                        &screen_send_meter,
                    ) {
                        Ok(drained) => drained,
                        Err(err) => break Err(err),
                    };
                    if drained > 0 {
                        debug!(drained, "补丁暂存区补投");
                    }
                    if patch_staging.over_age(std::time::Instant::now()) {
                        warn!(
                            seconds = PATCH_STAGING_MAX_AGE.as_secs_f32(),
                            staged_bytes = patch_staging.bytes(),
                            "补丁滞留超过上限（写侧持续跟不上），退出补丁模式"
                        );
                        let partial = patch_staging.partial();
                        exit_patch_mode(
                            &mut screen_mode,
                            &mut patch_staging,
                            &mut force_encode,
                            &mut last_changed_permille,
                            &mut capture_tick,
                            &mut next_capture_at,
                            PatchExitReason::StagingAge,
                        );
                        // 半批横条已发出 → 控制端合成缓冲缺块，只能靠完整 IDR 修复；走与
                        // `RequestKeyframe` 相同的去重闸门（别为同一幅画面连打 IDR）。
                        if partial
                            && !keyframe_request_is_redundant(
                                force_keyframe,
                                idr_tracker.in_flight(),
                                idr_tracker.last_written_us(),
                                crate::clock::now_us(),
                            )
                        {
                            force_keyframe = true;
                        }
                    }
                }
                let period = capture_tick_period(screen_mode.mode, &encode);
                let tick_now = Instant::now();
                if let Some(last) = last_capture_tick.replace(tick_now) {
                    let dt_ms = u64::try_from(tick_now.duration_since(last).as_millis())
                        .unwrap_or(u64::MAX);
                    tick_dt_sum_ms = tick_dt_sum_ms.saturating_add(dt_ms);
                    tick_dt_count = tick_dt_count.saturating_add(1);
                    if dt_ms > 40 {
                        tick_slow = tick_slow.saturating_add(1);
                    }
                }
                // 固定相位：从上一拍的计划时刻推进。落后超过一整拍（循环被长任务占住）
                // 才重同步，避免追帧式突发。
                next_capture_at += period;
                if next_capture_at <= tick_now {
                    next_capture_at = tick_now + period;
                }
                let scheduled = next_capture_at;
                schedule_capture(&mut capture_tick, &mut next_capture_at, scheduled);
                // 串行采集：上一次还没回来就跳过本轮，避免把阻塞线程池堆满。
                if capture_deadline.is_none() {
                    capture_request_us = Some(crate::clock::now_us());
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
                } else {
                    tick_spawn_skip = tick_spawn_skip.saturating_add(1);
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
                let display_started = Instant::now();
                let fresh = screen.displays();
                display_max_ms = display_max_ms.max(
                    u64::try_from(display_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                );
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
                            screen_mode.exit(
                                PatchExitReason::Reconfigure,
                                std::time::Instant::now(),
                            );
                            patch_staging.clear();
                            force_keyframe = true;
                            schedule_capture(
                                &mut capture_tick,
                                &mut next_capture_at,
                                Instant::now(),
                            );
                        }
                        // 当前显示器还在但分辨率变了：同上（控制器那块合成帧已作废）。
                        (Some(old), Some(new))
                            if (old.width, old.height) != (new.width, new.height) =>
                        {
                            info!(
                                "显示器 {display_id} 分辨率变化 {}x{} -> {}x{}，重建编码器并强制关键帧",
                                old.width, old.height, new.width, new.height
                            );
                            screen_mode.exit(
                                PatchExitReason::Reconfigure,
                                std::time::Instant::now(),
                            );
                            patch_staging.clear();
                            force_keyframe = true;
                            schedule_capture(
                                &mut capture_tick,
                                &mut next_capture_at,
                                Instant::now(),
                            );
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
                // 出码窗口推进器：每收到一条 `QosFeedback` 推进一次（无论这条反馈会不会被
                // 补丁模式忽略），窗口因此恰好是「上一条反馈 → 本条反馈」，与控制器
                // `recv_kbps` 的窗口只差一个单向延迟。首条反馈没有窗口，返回 `None`。
                let mut advance_meter = |meter: &ScreenSendMeter| -> Option<u32> {
                    let now = Instant::now();
                    let total = meter.total_bytes();
                    let sample = meter_primed
                        .then(|| window_kbps(meter_bytes, total, now.duration_since(meter_at)));
                    meter_bytes = total;
                    meter_at = now;
                    meter_primed = true;
                    sample
                };
                match incoming {
                    Some(Ok(frame)) => match frame.message {
                        SessionMessage::Input(event) => {
                            input_events = input_events.saturating_add(1);
                            // `InputSink::apply` 是 enigo 注入，快操作（微秒级），留在 async worker
                            // 上即可——为它付一次 `spawn_blocking` 的调度开销并不划算。
                            if inject {
                                // 诊断：注入本身的耗时，以及「输入已生效」的时刻（两条输入
                                // 交互延迟测量的起点，见 [`crate::latency`]）。
                                let inject_started = std::time::Instant::now();
                                match input.apply(event) {
                                    Ok(()) if latency::enabled() => {
                                        let now = crate::clock::now_us();
                                        latency::record(
                                            &latency_window,
                                            Stage::Inject,
                                            u64::try_from(inject_started.elapsed().as_micros())
                                                .unwrap_or(u64::MAX),
                                        );
                                        last_input_us = Some(now);
                                        pending_input_capture_us = Some(now);
                                    }
                                    Ok(()) => {}
                                    Err(err) => warn!(error = %err, "输入注入失败"),
                                }
                            }
                            // 输入**会改变远端画面**（拖窗/悬停/滚动/打字）：主动把下一拍采集
                            // 拉近，不让用户白等一个周期节拍（见 [`input_capture_target`]）。
                            //
                            // 纯鼠标移动本身不进画面（采集帧不含光标，控制端用本机光标），
                            // 但它会触发悬停效果、拖动窗口、滚动条等——照样要立刻采一拍照。
                            if capture {
                                if capture_deadline.is_some() {
                                    // 采集在途：等结果回来时立刻补拍（见 capture_rx 分支）。
                                    input_capture_pending = true;
                                } else if let Some(target) = input_capture_pull(
                                    Instant::now(),
                                    last_capture_tick,
                                    capture_tick_period(screen_mode.mode, &encode),
                                    next_capture_at,
                                ) {
                                    schedule_capture(
                                        &mut capture_tick,
                                        &mut next_capture_at,
                                        target,
                                    );
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
                                screen_mode.exit(
                                    PatchExitReason::Reconfigure,
                                    std::time::Instant::now(),
                                );
                                patch_staging.clear();
                                force_keyframe = true;
                                let at = Instant::now() + encode.capture_interval();
                                schedule_capture(&mut capture_tick, &mut next_capture_at, at);
                            } else {
                                // `display` 会与 `tracing::display` 撞名，故用 target 命名。
                                warn!(target, "控制器请求了不存在的显示器");
                            }
                        }
                        SessionMessage::SetQuality { preset } => {
                            // 档位是**上限**：换上限并把闭环重置到新上限（用户显式意图，不带降级历史）。
                            // 原生分辨率会话按**当前显示器面积**放大（见 `session_encode_options`）。
                            rate.set_ceiling(session_encode_options(
                                preset.options(),
                                native_bitrate_permille(
                                    displays.iter().find(|d| d.id == display_id),
                                    native_active,
                                ),
                            ));
                            // 用户显式选的帧率优先级高于档位默认值：换画质不应把它覆盖回去。
                            if let Some(fps) = selected_fps {
                                rate.set_frame_rate_ceiling(fps);
                            }
                            encode = rate.current();
                            // pacing 跟随 AIMD 链路码率（大帧摊开的时间按它算）。
                            queues.set_screen_pacing_kbps(rate.keyframe_bitrate_kbps());
                            // 档位可能改缩放 → 几何可能变化：补丁模式的持久缓冲尺寸随之作废，
                            // 必须退出（并靠下面的 `force_keyframe` 重发基准）。
                            screen_mode.exit(
                                PatchExitReason::Reconfigure,
                                std::time::Instant::now(),
                            );
                            patch_staging.clear();
                            let at = Instant::now() + encode.capture_interval();
                            schedule_capture(&mut capture_tick, &mut next_capture_at, at);
                            force_keyframe = true;
                        }
                        SessionMessage::SetFrameRate { fps } => {
                            // 帧率是**上限**，且只动采集节拍与编码器的 fps（在线重配置，**不重建**）。
                            selected_fps = Some(fps);
                            rate.set_frame_rate_ceiling(fps);
                            encode = rate.current();
                            queues.set_screen_pacing_kbps(rate.keyframe_bitrate_kbps());
                            // 补丁模式可以留着（帧率只影响空补丁的保活节拍），但节拍要按新值重排。
                            let at = Instant::now() + capture_tick_period(screen_mode.mode, &encode);
                            schedule_capture(&mut capture_tick, &mut next_capture_at, at);
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
                            //
                            // 出码窗口仍要推进（丢弃返回值）：否则退出补丁模式后的首个窗口会把
                            // 整个补丁期间的字节都算进去，得到一个虚高的 `sent_kbps`。
                            let _ = advance_meter(&screen_send_meter);
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
                            // 不置关键帧（见 `RateController::observe_activity`）。升档已由每帧的
                            // `promote_activity` 抢先完成，这里主要负责**带迟滞的降档**。
                            rate.observe_activity(last_changed_permille);
                            let local_backlog =
                                u32::try_from(queues.screen_backlog()).unwrap_or(u32::MAX);
                            // 上一个窗口**实际交给画面队列**的码率（视频 + 补丁）。窗口由
                            // `advance_meter` 推进为「上一条反馈 → 本条反馈」，与控制器回报的
                            // `recv_kbps`（同一段窗口）只差一个单向延迟，可安全算投递率；
                            // 首条反馈没有窗口（`None`），不算投递率。
                            let sent_kbps_now = advance_meter(&screen_send_meter);
                            let decision = rate.on_feedback(
                                Feedback { rtt_ms, recv_kbps, decode_ms, backlog },
                                local_backlog,
                                sent_kbps_now,
                            );
                            if decision.rates.fps != encode.fps {
                                // 帧率变了：采集节拍要立刻跟着变（与 `SetQuality` 同款）。
                                let at = Instant::now() + decision.rates.capture_interval();
                                schedule_capture(&mut capture_tick, &mut next_capture_at, at);
                            }
                            encode = decision.rates;
                            queues.set_screen_pacing_kbps(rate.keyframe_bitrate_kbps());
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
                                let delivery_ratio = sent_kbps_now.filter(|sent| *sent > 0).map_or(0.0, |sent| {
                                    f64::from(recv_kbps) / f64::from(sent)
                                });
                                // 首条反馈尚无窗口时打印 `-`，别让读者把 0 当成真实出码。
                                let sent_label = sent_kbps_now.map_or_else(
                                    || "-".to_owned(),
                                    |sent| sent.to_string(),
                                );
                                info!(
                                    "QoS 调整 action={:?} rtt={}ms rtt_smooth={:?} baseline={:?} recv_kbps={} sent_kbps={} delivery_ratio={:.2} decode_ms={} backlog={} local_backlog={} bitrate_kbps={} fps={} scale={:.3} changed_permille={} cap_kbps={:?} cap_fps={:?}",
                                    decision.action,
                                    rtt_ms,
                                    rate.smoothed_rtt_ms(),
                                    rate.baseline_rtt_ms(),
                                    recv_kbps,
                                    sent_label,
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
                                    &mut patch_staging,
                                    &mut force_encode,
                                    &mut last_changed_permille,
                                    &mut capture_tick,
                                    &mut next_capture_at,
                                    PatchExitReason::KeyframeRequest,
                                );
                            }
                            // 「请求了就一定 IDR」（仅限请求的就是当前显示器）：这是控制器明确
                            // 报告参考帧不可信，与补丁模式自己退出时的 P 帧语义不同。
                            //
                            // 但**同一个 IDR 不连打**：控制器在 `awaiting_keyframe` 期间每 400 ms
                            // 重发一次，而一帧 IDR 从编码到写完要 190+ ms。真机日志里 0.8 秒 4 个
                            // ~240 KB 的 IDR（246199 / 246117 字节几乎相同）就是这么来的。已有
                            // IDR 在途、或刚写出不到 [`KEYFRAME_DEDUP_WINDOW`] 时直接吸收；
                            // 窗口（350 ms）严格小于重发周期（400 ms），所以被吸收的请求会在
                            // 下一次重发时得到满足——若那个 IDR 真的丢了，最多多等一个周期。
                            if display == display_id {
                                if keyframe_request_is_redundant(
                                    force_keyframe,
                                    idr_tracker.in_flight(),
                                    idr_tracker.last_written_us(),
                                    crate::clock::now_us(),
                                ) {
                                    debug!(
                                        in_flight = idr_tracker.in_flight(),
                                        "已有 IDR 在途或刚写出，吸收重复的关键帧请求"
                                    );
                                } else {
                                    force_keyframe = true;
                                }
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

/// 计算一帧的 pacing 方案：`(每块字节数, 块间等待)`；`None` = 走原来的整帧发送。
///
/// 抽成纯函数便于单测锁定两条上界：
/// - 单个间隙 ≤ [`PACING_MAX_GAP`]（码率读数离谱时不会把一帧拖成秒级）；
/// - 全部间隙之和 ≤ [`PACING_MAX_WAIT_FACTOR`] × 该帧自然串行时间。
///
/// `kbps == 0` 表示关闭（控制器侧永远如此）；小于 [`PACING_MIN_FRAME_BYTES`] 的帧返回 `None`
/// ——它们本身就是交互延迟，加 sleep 只会更慢。
#[expect(
    clippy::cast_precision_loss,
    reason = "帧长与码率都远小于 2^53，转 f64 只用于算时长，不损精度"
)]
fn pacing_plan(len: usize, kbps: u32) -> Option<(usize, Duration)> {
    if kbps == 0 || len < PACING_MIN_FRAME_BYTES {
        return None;
    }
    let chunk = len.min(PACING_CHUNK_BYTES);
    // 每块、整帧在目标码率下的自然串行时间（秒 = 字节 × 8 / (kbps × 1000)）。
    let chunk_natural = Duration::from_secs_f64((chunk as f64 * 8.0) / (f64::from(kbps) * 1000.0));
    let frame_natural = Duration::from_secs_f64((len as f64 * 8.0) / (f64::from(kbps) * 1000.0));
    // 块间等待的次数 = ceil(len / chunk) - 1（最后一块写完后不必等）。
    let gap_count = len.div_ceil(chunk).saturating_sub(1);
    if gap_count == 0 {
        return Some((chunk, Duration::ZERO));
    }
    let gap_count = u32::try_from(gap_count).unwrap_or(u32::MAX);
    let total_cap = frame_natural
        .checked_mul(PACING_MAX_WAIT_FACTOR)
        .unwrap_or(Duration::MAX);
    let sleep = chunk_natural.min(PACING_MAX_GAP).min(total_cap / gap_count);
    Some((chunk, sleep))
}

/// 出站写任务：独占写半边，按**四档优先级**从队列取帧写线。
///
/// 优先级固定为 close → ctrl → screen → file（`select!` 的 `biased`）。close 是收尾 `Bye`
/// 专用通道，**不理会取消令牌**、限时尽力写出；其余三档在取消后不再发送。
///
/// 取消语义：普通帧的发送同时等取消令牌（[`send_cancellable`]），取消一旦触发就丢弃后续普通帧、
/// 只等收尾帧——这样「取消能立刻打断被背压卡住的发送」与「收尾 `Bye` 仍能尽力发出」两件事同时成立。
///
/// # 大帧 pacing
///
/// screen 档里**编码后 ≥ [`PACING_MIN_FRAME_BYTES`] 的帧**（典型是 IDR）不整帧 `send`，而是
/// 编码成一段线上字节后**按 [`PACING_CHUNK_BYTES`] 分块写**，块间按目标码率 sleep
/// （[`pacing_plan`]）。理由：一帧 300 KB 在 6 Mbps 上是 400 ms 的串行化尖峰，它会把后面排队的
/// 输入回显帧一起堵住；摊开写之后抖动相位被打散，瓶颈处也不容易成片丢包。小帧、ctrl、file、
/// close 全部走原路径，行为不变。
pub(crate) async fn writer_loop(
    mut writer: FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    mut rx: OutboundReceivers,
    cancel: CancellationToken,
    // 诊断：与主循环/编码任务共享的延迟窗口；控制器侧传 `None`（它不写画面帧）。
    latency_window: Option<Arc<std::sync::Mutex<LatencyWindow>>>,
    // IDR 在途/刚写出观测点；控制器侧传 `None`（它不产生 IDR）。被控端在**写完**一帧
    // 关键帧时销账并置「刚写出」时间戳，会话循环据此吸收重复的关键帧请求。
    idr_tracker: Option<Arc<IdrTracker>>,
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
                let pacing_kbps = rx.screen_pacing_kbps.load(Ordering::Relaxed);
                // 诊断：`write` 阶段（含大帧 pacing 的分块 sleep）+ 采集完成 → 写任务取走。
                // 关闭时 `meta` 为 `None`，不多做任何工作。
                let trace = latency_window.is_some() && latency::enabled();
                let dequeued_us = if trace { crate::clock::now_us() } else { 0 };
                let meta = if trace { screen_frame_meta(&frame) } else { None };
                // 关键帧标记必须**无条件**取：延迟诊断关闭时 `meta` 是 `None`，但 IDR 销账
                // 与诊断无关（见 [`IdrTracker`]）。
                let wrote_idr = matches!(
                    &frame.message,
                    SessionMessage::VideoPacket { flags, .. } if flags & VIDEO_FLAG_KEYFRAME != 0
                );
                if !write_screen(&mut writer, &cancel, frame, &mut cancelled, pacing_kbps).await {
                    break;
                }
                // 只有**完整写完**才算「刚写出」：pacing 中途被取消的那一帧没有完整上线，
                // 会话也正在收尾，不需要再吸收任何请求。
                if wrote_idr && !cancelled {
                    if let Some(tracker) = &idr_tracker {
                        tracker.on_idr_written(crate::clock::now_us());
                    }
                }
                if let (Some(window), Some((seq, capture_ts_us, bytes, keyframe))) =
                    (latency_window.as_ref(), meta)
                {
                    let done_us = crate::clock::now_us();
                    latency::record(window, Stage::Write, done_us.saturating_sub(dequeued_us));
                    if capture_ts_us != 0 {
                        let send_q_us = dequeued_us.saturating_sub(capture_ts_us);
                        latency::record(window, Stage::SendQueue, send_q_us);
                        // 只有全帧有 `seq`：补丁批不逐帧打日志（一拍可能多条消息），
                        // 但它的 `write`/`send_q` 仍计入每秒汇总。
                        if seq != 0 {
                            let capture_to_write_done_us =
                                done_us.saturating_sub(capture_ts_us);
                            if latency::frame_logs()
                                || capture_to_write_done_us > latency::slow_threshold_us()
                            {
                                info!(
                                    target: "latency",
                                    "controlled_wire seq={} bytes={} idr={} send_q_us={} write_us={} capture_to_write_done_us={}",
                                    seq,
                                    bytes,
                                    keyframe,
                                    send_q_us,
                                    done_us.saturating_sub(dequeued_us),
                                    capture_to_write_done_us
                                );
                            }
                        }
                    }
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

/// 诊断：从一条画面帧里取出 `(seq, capture_ts_us, 字节数, 是否关键帧)`。
///
/// 补丁批没有视频序号（记 `0`），但同样带采集时刻与总字节数——它也要计入每秒汇总。
/// 非画面帧返回 `None`。
fn screen_frame_meta(frame: &SessionFrame) -> Option<(u64, u64, u64, bool)> {
    match &frame.message {
        SessionMessage::VideoPacket {
            seq,
            capture_ts_us,
            data,
            flags,
            ..
        } => Some((
            *seq,
            *capture_ts_us,
            u64::try_from(data.len()).unwrap_or(u64::MAX),
            flags & VIDEO_FLAG_KEYFRAME != 0,
        )),
        SessionMessage::RegionUpdate {
            capture_ts_us,
            rects,
            ..
        } => Some((
            0,
            *capture_ts_us,
            rects.iter().fold(0_u64, |acc, rect| {
                acc.saturating_add(u64::try_from(rect.data.len()).unwrap_or(u64::MAX))
            }),
            false,
        )),
        _ => None,
    }
}

/// 写一条**画面**帧：大帧按 [`pacing_plan`] 分块 pacing，其余与 [`write_ordinary`] 相同。
///
/// 只有 `VideoPacket` 可能达到 [`PACING_MIN_FRAME_BYTES`]（`RegionUpdate` 单条上限 32 KiB、
/// `ScreenConfig` 是几十字节），所以判据用载荷长度即可，不必先编码一遍再判断。
///
/// 返回值语义与 [`write_ordinary`] 一致：`false` = 写侧出错、写任务结束。
async fn write_screen(
    writer: &mut FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    cancel: &CancellationToken,
    frame: SessionFrame,
    cancelled: &mut bool,
    pacing_kbps: u32,
) -> bool {
    let payload_len = match &frame.message {
        SessionMessage::VideoPacket { data, .. } => data.len(),
        _ => 0,
    };
    let Some((chunk, gap)) = pacing_plan(payload_len, pacing_kbps) else {
        return write_ordinary(writer, cancel, frame, cancelled).await;
    };
    // 按值编码（不 clone 帧）：大帧每帧数 MB 级拷贝在这里是纯浪费。
    let bytes = match writer.encoder_mut().encode_frame_owned(frame) {
        Ok(bytes) => bytes,
        Err(err) => {
            // 与 `FramedWrite::send` 的编码失败同义：这条帧永远写不出去，写任务收场。
            debug!(error = %err, "画面帧编码失败，写任务结束");
            return false;
        }
    };
    write_paced(writer, cancel, &bytes, chunk, gap, cancelled).await
}

/// 把已经编码好的线上字节按 `chunk` 分块写，块间 `sleep(gap)`（可取消）。
///
/// # 前提：`FramedWrite` 的内部缓冲是空的
///
/// 本函数绕开 `FramedWrite::send` 直接写底层 `WriteHalf`。`send` 成功返回即代表内部
/// 编码缓冲已经 flush 干净（`SinkExt::send` = `start_send` + `poll_flush`），而本函数只由
/// [`writer_loop`] 在一帧接一帧写出时调用，所以进入时缓冲必为空——否则这里写下的字节会插到
/// 上一帧的**内部缓冲之前**，线上顺序被破坏。**不要**在别处调用它。
///
/// 每块写完都 flush 一次底层 writer：TLS 会把小块攒在记录缓冲里，不 flush 就失去了 pacing
/// 的意义（字节仍然迟一步才上线）。
///
/// 取消/IO 失败语义与 [`send_cancellable`] 相同：取消时可能留下**半截帧**，但此时会话正在
/// 收尾（此后最多再尽力发一条 `Bye`），对端会按长度前缀/自校验把这条残帧判为坏帧并断开。
async fn write_paced(
    writer: &mut FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    cancel: &CancellationToken,
    bytes: &[u8],
    chunk: usize,
    gap: Duration,
    cancelled: &mut bool,
) -> bool {
    let chunk = chunk.max(1);
    for (index, part) in bytes.chunks(chunk).enumerate() {
        // 第一块立刻写，块间才等：等待代表「上一块占用的链路时间」。
        if index > 0 && !gap.is_zero() {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    *cancelled = true;
                    return true;
                }
                () = sleep(gap) => {}
            }
        }
        let written = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(RemoteError::Cancelled),
            res = async {
                writer.get_mut().write_all(part).await?;
                writer.get_mut().flush().await
            } => res.map_err(RemoteError::Io),
        };
        match written {
            Ok(()) => {}
            Err(RemoteError::Cancelled) => {
                *cancelled = true;
                return true;
            }
            Err(err) => {
                debug!(error = %err, "分块写失败，写任务结束");
                return false;
            }
        }
    }
    true
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
    /// 脏矩形补丁（**已在采集任务里压缩好**）；采集失败或「本拍不适合补丁」为 `None`。
    Patches(Option<PatchBatch>),
}

/// 采集任务里**已经完成 zstd 压缩**的一拍补丁。
///
/// 会话循环只据此做尺寸/活跃度判断、带宽预算和入队——压缩本身发生在 `spawn_blocking`
/// 的采集任务里，不再阻塞事件循环（旧实现会同步卡住循环 10–40 ms）。
struct PatchBatch {
    /// 目标帧缓冲宽度（编码尺寸）。
    width: u32,
    /// 目标帧缓冲高度（编码尺寸）。
    height: u32,
    /// 本拍变化面积（千分比）。
    changed_permille: u16,
    /// 原始脏矩形数（判断是否超过 [`MAX_REGION_RECTS`]）。
    rect_count: usize,
    /// 本拍没有变化（空补丁；只有保活间隔到点才发送）。
    empty: bool,
    /// 压缩后的 `RegionUpdate` 消息；`None` = 装不进单拍预算（调用方退出补丁模式）。
    messages: Option<Vec<SessionMessage>>,
    /// 压缩后总字节数（`messages` 为 `Some` 时有效）。
    bytes: u64,
}

/// 一次后台采集的结果。
struct CaptureOutcome {
    /// 采集代次（见循环里的 `capture_generation`）：用于丢弃被看门狗判定为丢失的过期结果。
    generation: u64,
    /// 采集产物（全帧或补丁）。
    payload: CapturePayload,
    /// 阻塞采集任务**真正开始**的时刻（本端单调时钟，微秒）。
    ///
    /// 与 `capture_ts_us` 相减得采集本身耗时；与派发时刻相减得阻塞线程池/运行时调度等待
    /// （诊断用，见 [`crate::latency`]）。
    started_us: u64,
    /// 本次采集耗时（微秒；采集失败时也记实际耗时）。**测量**用，不参与控制决策。
    ///
    /// 用微秒而不是毫秒：GPU 采集常在 1 ms 以内，毫秒取整会恒为 0（现场看到的就是这个）。
    capture_us: u64,
    /// 采集**完成**的时刻（本端单调时钟，微秒；[`crate::clock::now_us`]）。
    ///
    /// 它随画面帧一路上线（`VideoPacket` / `RegionUpdate` 的 `capture_ts_us`），控制器据此算
    /// 「采集 → 解码完成」的端到端延迟（D2）。失败时仍填（该帧本来就不会上线，只求代码统一）。
    capture_ts_us: u64,
    /// 本拍采样到的远端光标形状（进程内传递，不上线）。
    ///
    /// `None` = 平台不支持/读取失败；`Some(Hidden)` 表示远端隐藏了光标。即使采集失败也采样，
    /// 光标形状不依赖画面是否成功。
    cursor: Option<CursorShape>,
}

/// 交给编码任务的一帧。
///
/// `display`/`rates`/`force_keyframe`/`force_encode` 由会话循环带上：编码任务据此判断是否
/// 需要**重建**编码器（只有几何变化才重建）、是否强制关键帧（首帧、切显示器、切档位、
/// `RequestKeyframe`、降分辨率）以及是否只需**真正编码一次**（补丁模式的基准/退出帧，P 帧即可）。
/// **码率/帧率不触发重建**——它们在编码任务里在线重配置（见 [`encode_loop`]）。
struct EncodeJob {
    /// 来源显示器编号。
    display: u32,
    /// 当前**生效**编码参数（≤ 档位上限）；P 帧用它。
    rates: EncodeRates,
    /// 关键帧（IDR）该用的目标码率（kbps）：AIMD 值，**不受内容面积上限钳制**
    /// （见 [`RateController::keyframe_bitrate_kbps`]）。
    ///
    /// 关键帧要编码整幅画面，面积上限那套「小改动用小预算」的逻辑会把它压到 1.2 Mbps
    /// （1080p 只剩几 KB），整屏文本直接糊掉；而静态画面没有残差，糊掉的
    /// 参考帧**永远不会自行恢复**。必须把这条与 P 帧的预算分开。
    keyframe_bitrate_kbps: u32,
    /// 本帧必须是 IDR（首帧/切显示器/切档位/`RequestKeyframe`/降分辨率/编码器重建）。
    force_keyframe: bool,
    /// 本帧必须真正编码一次（即使与上一帧相同），但**不要求** IDR。
    /// 补丁模式的基准帧用这个：参考链没断时插 IDR 只是一个带宽尖峰。
    force_encode: bool,
    /// 本帧的采集耗时（微秒），只进统计、不参与控制决策。
    capture_us: u64,
    /// 诊断：采集派发（tick）→ 采集任务真正开始的等待（微秒）；未测量时 `None`。
    /// **只进延迟日志**（见 [`crate::latency`]），不参与控制决策。
    capture_wait_us: Option<u64>,
    /// 本帧采集完成的时刻（本端单调时钟，微秒），原样写进 `VideoPacket.capture_ts_us`。
    capture_ts_us: u64,
    /// 诊断：本帧是「输入注入之后的第一拍采集」时，注入到采集完成的耗时（微秒）；
    /// 否则 `None`。**只进延迟日志**（见 [`crate::latency`]），不参与控制决策。
    input_to_capture_us: Option<u64>,
    /// 诊断：本帧是第一帧变化面积 ≥ [`INPUT_CHANGE_PERMILLE`] 的采集时，
    /// 最近一次输入注入到本帧采集完成的耗时（微秒）；否则 `None`。
    input_to_change_us: Option<u64>,
    /// 原始帧。
    frame: RawFrame,
}

/// 诊断：「输入 → 画面开始变」判据的变化面积门限（千分比）。
///
/// 50‰ 是「用户看得出来画面动了」的量级：悬停高亮、光标附近重绘通常低于它，
/// 点击标签/展开文件夹这种整块重绘远高于它。只用于 [`Stage::InputToChange`] 的采样。
const INPUT_CHANGE_PERMILLE: u16 = 50;

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
    // 诊断：采集任务真正开始的时刻（与派发时刻的差 = 阻塞线程池/运行时调度等待）。
    let started_us = crate::clock::now_us();
    let (payload, capture_us, capture_ts_us) = match request {
        CaptureRequest::Frame { gpu } => {
            let captured = if gpu {
                screen.capture_gpu(display)
            } else {
                screen.capture(display)
            };
            let capture_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            // 采集**返回之后**立刻打时间戳：它是控制器的端到端延迟（采集→解码完成）的起点。
            // 失败路径也打（结果不会上线，只是让所有分支形状一致）。
            let capture_ts_us = crate::clock::now_us();
            let payload = CapturePayload::Frame(match captured {
                Ok(frame) => Some(frame),
                Err(err) => {
                    warn!(error = %err, "屏幕采集失败");
                    None
                }
            });
            (payload, capture_us, capture_ts_us)
        }
        CaptureRequest::Patches { scale, max_width } => {
            let captured = screen.capture_patches(display, scale, max_width);
            let capture_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            let capture_ts_us = crate::clock::now_us();
            let batch = match captured {
                Ok(Some(patches)) => Some(build_patch_batch(display, &patches, capture_ts_us)),
                Ok(None) => None,
                Err(err) => {
                    warn!(error = %err, "脏矩形补丁采集失败");
                    None
                }
            };
            (CapturePayload::Patches(batch), capture_us, capture_ts_us)
        }
    };
    // 光标形状每拍采样一次（很便宜：`GetCursorInfo` 是常数时间）。**采集失败也采样**——
    // 形状与画面无关，一次采集错误不该让控制端的光标停住。`None` = 平台读不到。
    let cursor = screen.cursor_shape();
    // 接收端随会话循环存活；会话已结束时发送失败是正常的，忽略。
    let _ = tx.blocking_send(CaptureOutcome {
        generation,
        payload,
        started_us,
        capture_us,
        capture_ts_us,
        cursor,
    });
}

/// 在采集的 `spawn_blocking` 任务里压好这一拍补丁。
///
/// **为什么不在会话循环里压**：zstd level 1 对复杂桌面内容单拍可达 10–40 ms，同步执行会
/// 直接拖慢采集节拍（真机表现为采集 30 Hz 掉到 ~22 Hz、偶发顿挫）。采集任务本来就是阻塞
/// 上下文，把压缩放进来后循环只做预算判断与入队。
///
/// 压缩器每拍新建（zstd 上下文构造是微秒级；30 Hz 下开销可忽略），换取无需跨任务搬运
/// 可变状态的简单性。
fn build_patch_batch(display: u32, patches: &CapturedPatches, capture_ts_us: u64) -> PatchBatch {
    let mut compressor = match zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL) {
        Ok(compressor) => Some(compressor),
        Err(err) => {
            warn!(error = %err, "zstd 压缩器创建失败，本拍补丁作废");
            None
        }
    };
    let messages = compressor
        .as_mut()
        .and_then(|compressor| build_region_updates(compressor, display, patches, capture_ts_us));
    let bytes = messages.as_ref().map_or(0, |messages| {
        messages.iter().fold(0_u64, |acc, msg| {
            acc.saturating_add(region_update_bytes(msg))
        })
    });
    PatchBatch {
        width: patches.width,
        height: patches.height,
        changed_permille: patches.changed_permille,
        rect_count: patches.rects.len(),
        empty: patches.rects.is_empty(),
        messages,
        bytes,
    }
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
    /// 最近一秒因 `idr` 产出的关键帧数。
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
    /// 本窗口因 `idr` 真正产出的关键帧数（C3）。
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

    /// 到点则上报一次并重置窗口。`bitrate_kbps` 是**当前生效的目标码率**（闭环值，非实测出码率）。
    /// 实测出码速率只进 [`EncodeSnapshot`]（编码侧口径）；QoS 的投递率用 [`ScreenSendMeter`]。
    ///
    /// **重置之前**先把完整快照写进共享槽（`snapshot`）：会话主循环在同一拍（心跳 tick）读它组
    /// 装 `HostStats`。两者都在 ~1s 节拍上，读写顺序不影响正确性——最坏只是多显示/少显示一拍。
    fn maybe_report(
        &mut self,
        stats: &SessionStatsReporter,
        bitrate_kbps: u32,
        snapshot: &std::sync::Mutex<Option<EncodeSnapshot>>,
    ) {
        let elapsed = self.start.elapsed();
        if elapsed < STATS_INTERVAL {
            return;
        }
        // 实测出码速率：窗口字节数 / 窗口时长。先算再重置窗口。
        // 它只进 `EncodeSnapshot`（角标「实际码率」是编码侧口径）；QoS 的投递率改用
        // [`ScreenSendMeter`]——那里含补丁字节且不受补丁模式冻结影响。
        let sent_kbps_value = kbps_from_bytes(self.bytes, elapsed);
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
            u32::try_from(self.changed_permille_sum / u64::from(self.frames)).unwrap_or(u32::MAX)
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
///   **补编只绕过「无变化就跳过」这一道闸门，不参与 `idr` 的计算，因此绝不会带出 IDR。**
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
    send_meter: Arc<ScreenSendMeter>,
    base_frame_sent: Arc<AtomicBool>,
    // 最近一次真正编码出关键帧的时刻（微秒；0 = 还没有）。主循环据此做周期 IDR 兜底。
    last_keyframe_us: Arc<AtomicU64>,
    // 本会话的编码宽度上限（1920 或原生 3840，见 [`effective_max_encode_width`]）。
    // 与平台采集侧用的是**同一个值**，否则补丁尺寸与全帧尺寸会对不上。
    max_encode_width: u32,
    // 每秒写一次的完整快照槽（主循环组装 `HostStats` 用）。
    encode_snapshot: Arc<std::sync::Mutex<Option<EncodeSnapshot>>>,
    // 诊断：与主循环/写任务共享的延迟窗口（见 [`crate::latency`]）。
    latency_window: Arc<std::sync::Mutex<LatencyWindow>>,
    // IDR 在途/刚写出观测点：编码任务在承诺编码 IDR 时入账，写任务写完时销账
    // （见 [`IdrTracker`]）。主循环据此吸收重复的关键帧请求。
    idr_tracker: Arc<IdrTracker>,
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
    // 同上，但只要求「真正编码一次」（P 帧即可）：补丁模式的基准帧/退出帧被背压跳过时，
    // 请求同样不能蒸发——否则会话循环会一直等 `base_frame_sent`，直到 `PATCH_BASE_TIMEOUT`
    // 放弃进入（白白再等一秒），或者退出后画面停在补丁模式的最后一帧。
    let mut pending_encode = false;
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
            keyframe_bitrate_kbps,
            force_keyframe,
            force_encode,
            capture_us,
            capture_wait_us,
            capture_ts_us,
            input_to_capture_us,
            input_to_change_us,
            frame,
        } = job;
        // 诊断：采集完成 → 编码任务取走（编码队列深度 1 + 会话循环调度）。
        let enc_q_us = if latency::enabled() && capture_ts_us != 0 {
            let us = crate::clock::now_us().saturating_sub(capture_ts_us);
            latency::record(&latency_window, Stage::EncodeQueue, us);
            us
        } else {
            0
        };
        // 本帧的变化面积（千分比）：进统计窗口的平均，并随每帧上报落进日志/UI。
        let changed_permille = frame.changed_permille;
        // 出站队列当前深度（C2）：主循环的 `HostStats` 每秒读一次快照。
        window.out_queue_depth =
            u32::try_from(out.max_capacity().saturating_sub(out.capacity())).unwrap_or(u32::MAX);
        // 每秒上报一次编码侧读数（静止画面也会经过这里——采集每拍都投递任务，只是被去重跳过）。
        // 快照写入发生在窗口重置**之前**，因此主循环读到的是刚刚结束的那一秒。
        window.maybe_report(&stats, rates.bitrate_kbps, &encode_snapshot);

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
        // IDR 只在参考链/几何真的失效时才需要。
        let idr = force_keyframe || geometry_changed || pending_keyframe;
        // 「必须编码一次」可以只要 P 帧（补丁模式的基准/退出帧）。`pending_encode` 是上一帧
        // 因背压被跳过的同一请求，所以它也算「本帧被要求编码」。
        let forced_encode = force_encode || pending_encode;
        let encode_now = idr || forced_encode;

        // 静止保活：距上次真正编码超过 `unchanged_resend` 就补一帧。它只绕过下面的
        // 「无变化就跳过」闸门——**不**参与 `idr` 的计算，所以补编永远是普通 P 帧，
        // 不会带出 IDR（QoS 每秒动一次就插 IDR 是专门修掉的回归）。
        let resend_due = last_encode.is_none_or(|at| at.elapsed() >= unchanged_resend);

        // 静止画面：**默认**不喂编码器（不转换、不编码、不发送），但距上次编码超过
        // `unchanged_resend` 时必须补编一帧（保活，见 [`UNCHANGED_RESEND_INTERVAL`]）——
        // 「永久不发」会让检测盲区/背压丢弃变成画面上永久冻结的一帧。
        // `encode_now`（首帧/切显示器/切档位/RequestKeyframe/几何变化/补丁基准帧）为真时
        // 必须照常编码，否则控制器要的 IDR、或补丁模式要的基准帧会被这里吞掉。
        if !encode_now && unchanged && !resend_due {
            window.record_skip();
            continue;
        }
        // 背压出口在**编码之前**：出站队列满就跳过这一帧。绝不能改成「编码后丢弃」。
        // 被跳过的关键帧请求必须留下（`pending_keyframe`），否则控制器的恢复请求会蒸发；
        // 「只需编码一次」的请求同理（`pending_encode`）。
        if out.capacity() == 0 {
            pending_keyframe |= idr;
            pending_encode |= force_encode && !idr;
            window.out_queue_skips = window.out_queue_skips.saturating_add(1);
            debug!("出站队列已满，跳过本轮编码（编码前丢帧，不破坏参考链）");
            continue;
        }
        // 本帧真正的编码码率：关键帧用 AIMD 值（不被内容面积上限钳制），P 帧用生效值。
        //
        // 关键帧是**整幅画面**，用 1.2 Mbps 编 1080p 只有几 KB，文本必然糊；
        // 而静止画面的 P 帧没有残差，糊掉的参考帧不会自行恢复。见
        // [`RateController::keyframe_bitrate_kbps`]。
        let frame_rates = if idr {
            EncodeRates {
                bitrate_kbps: keyframe_bitrate_kbps.max(rates.bitrate_kbps),
                ..rates
            }
        } else {
            rates
        };
        if encoder.is_none() {
            // **创建时也用关键帧码率**：MFT 的峰值与 VBV 缓冲只在创建时设定（运行期改会
            // 插 IDR），用一个被面积上限压低的码率创建会把整场会话的峰值永久锁死。
            match encoder_factory.create(
                width,
                height,
                keyframe_bitrate_kbps.max(rates.bitrate_kbps),
                rates.fps,
            ) {
                Ok(enc) => {
                    // C3：每次**成功创建**都算一次重建（含首建；几何未变时的首建也是从无到有）。
                    window.encoder_rebuilds = window.encoder_rebuilds.saturating_add(1);
                    window.encoder_info = enc.info();
                    encoder = Some(enc);
                }
                Err(err) => {
                    warn!(error = %err, "编码器创建失败，跳过本帧");
                    pending_keyframe |= idr;
                    pending_encode |= force_encode && !idr;
                    continue;
                }
            }
        }
        let Some(enc) = encoder.take() else {
            continue;
        };
        // 承诺一个 IDR：从这一刻起到它**写完**，外部来的重复关键帧请求都会被吸收
        // （见 [`IdrTracker`] / [`keyframe_request_is_redundant`]）。登记点必须在真正编码
        // **之前**——否则「已决定编码 IDR」到「出包」那十几毫秒里到达的请求会再插一个。
        // RAII 保证编码器创建失败/编码出错/空包这些 `continue` 路径自动销账。
        let mut idr_in_flight = idr.then(|| IdrInFlight::begin(Arc::clone(&idr_tracker)));
        let started = Instant::now();
        // 编码是阻塞调用，移出 async worker；编码器与整帧（CPU 的 `rgba` 或 GPU 纹理）
        // 随闭包移入、随结果移出（独占持有）。码率/帧率的在线重配置也在这里做
        // （`reconfig_and_encode`），与编码同一次阻塞调用。
        let joined = tokio::task::spawn_blocking(move || {
            let mut enc = enc;
            let result = reconfig_and_encode(enc.as_mut(), &frame_rates, &frame, idr);
            (enc, result, frame)
        })
        .await;
        let (enc, result, frame) = match joined {
            Ok(value) => value,
            Err(err) => {
                warn!(error = %err, "编码任务失败");
                encoder = None;
                prev_rgba = None;
                pending_keyframe |= idr;
                pending_encode |= force_encode && !idr;
                continue;
            }
        };
        let encode_elapsed = started.elapsed();
        let encode_ms = u64::try_from(encode_elapsed.as_millis()).unwrap_or(u64::MAX);
        let encode_micros = u64::try_from(encode_elapsed.as_micros()).unwrap_or(u64::MAX);
        if latency::enabled() {
            latency::record(&latency_window, Stage::Encode, encode_micros);
        }
        match result {
            Ok(packet) => {
                encoder = Some(enc);
                // 任何一次成功的编码都推进保活水位（**包括空包**）：否则编码器持续吐空包时
                // 会变成每拍重试，保活间隔形同虚设。
                last_encode = Some(Instant::now());
                // GPU 帧没有 CPU 像素可留作去重基准；置 `None` 让下一帧照常编码。
                prev_rgba = if is_gpu { None } else { Some(frame.rgba) };
                // `idr` 为真时本帧必是 IDR、「必须编码一次」也已满足，两类请求一并清掉。
                pending_keyframe = false;
                pending_encode = false;
                if packet.is_empty() {
                    continue;
                }
                seq = seq.wrapping_add(1);
                let keyframe = packet.keyframe;
                if keyframe {
                    // 周期 IDR 兜底的水位（主循环读）。必须按**实际编码结果**写：`idr` 为真的
                    // 那一拍可能被背压出口跳过，只有 packet 真的是关键帧才算数。
                    last_keyframe_us.store(crate::clock::now_us(), Ordering::Relaxed);
                }
                if idr && keyframe {
                    // C3：因 `idr`（首帧/切显示器/切档位/关键帧请求/几何变化/降分辨率）而
                    // 真正产出的 IDR。`idr` 为真的那一拍可能被上面的背压出口跳过，
                    // 所以必须以 packet 实际是不是关键帧为准。
                    window.forced_idrs = window.forced_idrs.saturating_add(1);
                }
                let payload_bytes = packet.data.len();
                window.record(
                    payload_bytes,
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
                let out_started = Instant::now();
                if !send_video(&out, &cancel, msg).await {
                    break;
                }
                // 真的产出了关键帧并交给画面队列：销账交给写任务（它在**写完**时置
                // 「刚写出」时间戳）。编码器没照做（`keyframe == false`）时 `idr_in_flight`
                // 保持 armed，由 `Drop` 在本轮结束时销账。
                if keyframe {
                    if let Some(guard) = idr_in_flight.take() {
                        guard.handoff();
                    }
                }
                let out_q_us = u64::try_from(out_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                if latency::enabled() {
                    latency::record(&latency_window, Stage::OutQueue, out_q_us);
                    // 逐帧明细：`frame_logs()`（=2）下每帧一行；`Summary`（=1）下只打慢帧。
                    // 总耗时是**采集派发到进队列**这一段，不含写任务与网络（那两段由
                    // `controlled_wire` 与控制器侧的 `controller` 行给出）。
                    let total_us = capture_wait_us
                        .unwrap_or(0)
                        .saturating_add(capture_us)
                        .saturating_add(enc_q_us)
                        .saturating_add(encode_micros)
                        .saturating_add(out_q_us);
                    if latency::frame_logs() || total_us > latency::slow_threshold_us() {
                        info!(
                            target: "latency",
                            "controlled seq={} idr={} bytes={} changed_permille={} cap_wait_us={:?} cap_us={} enc_q_us={} enc_us={} out_q_us={} input_to_cap_us={:?} input_to_change_us={:?}",
                            seq,
                            keyframe,
                            payload_bytes,
                            changed_permille,
                            capture_wait_us,
                            capture_us,
                            enc_q_us,
                            encode_micros,
                            out_q_us,
                            input_to_capture_us,
                            input_to_change_us
                        );
                    }
                }
                // 真正进入画面队列之后才计入出码（QoS 投递率口径，见 [`ScreenSendMeter`]）。
                // 被背压挡下/取消的帧没有交付，绝不能计入。
                send_meter.add_bytes(u64::try_from(payload_bytes).unwrap_or(u64::MAX));
                if keyframe || forced_encode {
                    // 通知会话循环：这块**全帧基准**已经进入出站队列。补丁走同一条 FIFO 队列，
                    // 因此之后的 `RegionUpdate` 必然排在它后面（顺序不变量，见模块文档）。
                    //
                    // 基准帧是 **P 帧**（补丁不碰参考链），所以不能只看 `keyframe`：
                    // `forced_encode` 就是会话循环为「补丁模式基准帧」打的标记（含被背压跳过、
                    // 在后续帧上补发的那个请求——没有它，重试成功也不会置位，进入只能等超时）。
                    // 退出补丁模式的那一帧也带它，此时没有人在等这个信号，置位无副作用。
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
                pending_keyframe |= idr;
                pending_encode |= force_encode && !idr;
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
    // 诊断：把「在线重配置」与「真正编码」分开计时。正常 NVENC 1080p 的编码应在几毫秒；
    // 持续几十毫秒时，这个拆解能区分是 `nvEncReconfigureEncoder` 慢，还是 encode 本身
    // （CPU 输入回读/转换、GPU 上下文争用）。
    let reconfig_started = std::time::Instant::now();
    if enc.bitrate_kbps() != rates.bitrate_kbps {
        enc.set_bitrate(rates.bitrate_kbps)?;
    }
    if enc.fps() != u32::from(rates.fps) {
        enc.set_fps(rates.fps)?;
    }
    let reconfig_us = reconfig_started.elapsed().as_micros();
    let encode_started = std::time::Instant::now();
    // CPU 帧走 trait 默认实现（= `encode`）；GPU 帧由平台实现覆盖。
    let result = enc.encode_frame(frame, force);
    let encode_us = encode_started.elapsed().as_micros();
    if encode_us >= 20_000 || reconfig_us >= 20_000 {
        debug!(
            reconfig_us,
            encode_us,
            gpu_input = frame.gpu.is_some(),
            width = frame.width,
            height = frame.height,
            "慢帧编码拆解"
        );
    }
    result
}

/// 把一帧画面消息投给出站写任务，**绝不丢弃**。
///
/// 画面帧一旦编码就不能丢（参考链），故这里 `send().await` 直到画面队列腾空；背压由编码任务
/// 之前的「跳过采集」出口消化（见 [`encode_loop`]）。返回 `false` 表示会话正在收尾
/// （取消触发或写任务已结束），编码任务据此退出。
async fn send_video(
    out: &mpsc::Sender<SessionFrame>,
    cancel: &CancellationToken,
    msg: SessionMessage,
) -> bool {
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
    fn mouse_button(
        &self,
        _button: crate::screen::MouseButton,
        _pressed: bool,
    ) -> RemoteResult<()> {
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
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU16, AtomicU32, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
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
            .send(SessionFrame::control(SessionMessage::Heartbeat {
                t1_us: 0,
            }))
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
        assert!(
            matches!(outcome, CodecOfferOutcome::Timeout),
            "应为 Timeout"
        );
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

        let ack = reader
            .next()
            .await
            .expect("应有 HelloAck")
            .expect("HelloAck 可解码");
        let responder_pub = match ack.message {
            SessionMessage::HelloAck { x25519_pub, .. } => x25519_pub,
            other => panic!("期望 HelloAck，实得 {other:?}"),
        };
        let initiator_pub = plan.keypair.public_bytes();
        let responder_arr: [u8; 32] = responder_pub
            .as_slice()
            .try_into()
            .expect("公钥应为 32 字节");
        let shared = plan
            .keypair
            .diffie_hellman(&responder_pub)
            .expect("ECDH 应成功");
        // 本端扮演控制器（发起方）：`initiator=true`。
        let keys = derive_session_keys(
            token,
            &shared,
            session_id,
            &initiator_pub,
            &responder_arr,
            true,
        );
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
                .send(SessionFrame::control(SessionMessage::CodecOffer {
                    h264,
                    vp9,
                }))
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
        writer
            .get_mut()
            .write_all(&hello_bytes)
            .await
            .expect("写 Hello");
        writer.get_mut().flush().await.expect("flush Hello");

        // 关键断言①：对端回的是一个**明文可解**的 HelloAck（干净拒绝），不是解码失败。
        let ack = tokio::time::timeout(WAIT, reader.next())
            .await
            .expect("应收到 HelloAck（干净拒绝）")
            .expect("HelloAck 必须能明文解码")
            .expect("HelloAck 解码不应出错");
        match ack.message {
            SessionMessage::HelloAck {
                accepted, reason, ..
            } => {
                assert!(!accepted, "版本不匹配必须拒绝");
                let reason = reason.unwrap_or_default();
                assert!(
                    reason.contains("proto"),
                    "拒绝原因应点名版本，实得 {reason}"
                );
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
                    message:
                        SessionMessage::VideoPacket {
                            flags,
                            width,
                            height,
                            ..
                        },
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
            Arc::new(ScreenSendMeter::new()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(latency::LatencyWindow::new())),
            IdrTracker::new(),
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
                keyframe_bitrate_kbps: hi.bitrate_kbps,
                force_keyframe: true,
                force_encode: false,
                capture_us: 0,
                capture_wait_us: None,
                capture_ts_us: 0,
                input_to_capture_us: None,
                input_to_change_us: None,
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
                keyframe_bitrate_kbps: hi.bitrate_kbps,
                force_keyframe: false,
                force_encode: false,
                capture_us: 0,
                capture_wait_us: None,
                capture_ts_us: 0,
                input_to_capture_us: None,
                input_to_change_us: None,
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
        let _ = tokio::time::timeout(WAIT, task)
            .await
            .expect("编码任务应结束");
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
            Arc::new(ScreenSendMeter::new()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(latency::LatencyWindow::new())),
            IdrTracker::new(),
        ));

        let hi = EncodeRates::ceiling(EncodeOptions::default());
        // scale 减半 → 编码尺寸减半（几何变化）。
        let half = EncodeRates { scale: 0.5, ..hi };

        job_tx
            .send(EncodeJob {
                display: 0,
                rates: hi,
                keyframe_bitrate_kbps: hi.bitrate_kbps,
                force_keyframe: true,
                force_encode: false,
                capture_us: 0,
                capture_wait_us: None,
                capture_ts_us: 0,
                input_to_capture_us: None,
                input_to_change_us: None,
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
                keyframe_bitrate_kbps: hi.bitrate_kbps,
                force_keyframe: false,
                force_encode: false,
                capture_us: 0,
                capture_wait_us: None,
                capture_ts_us: 0,
                input_to_capture_us: None,
                input_to_change_us: None,
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
        let _ = tokio::time::timeout(WAIT, task)
            .await
            .expect("编码任务应结束");
    }

    /// 假的编码记录：每次 `encode` 压入 `(当时的编码码率, 是否关键帧)`。
    type EncodeLog = Arc<std::sync::Mutex<Vec<(u32, bool)>>>;

    /// 假 H.264 编码器工厂：不产出真码流，只用于锁定「按协商 codec 建编码器」的接线。
    #[derive(Debug, Default)]
    struct FakeH264EncoderFactory {
        /// 可选记录器。`None` = 不记录（大多数用例只关心产物形状）。
        log: Option<EncodeLog>,
    }

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
                log: self.log.clone(),
            }))
        }
    }

    /// 假 H.264 编码器：每帧固定吐 4 字节「码流」。
    struct FakeH264Encoder {
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        log: Option<EncodeLog>,
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
            if let Some(log) = &self.log {
                log.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((self.bitrate, force_keyframe));
            }
            Ok(EncodedVideo {
                data: vec![0, 0, 0, 1, 0x42],
                keyframe: force_keyframe,
            })
        }
    }

    /// **关键帧码率不随内容上限收紧**（回归锁）。
    ///
    /// 被控端在 UI 档会把 P 帧的生效码率压到 1.2 Mbps；关键帧必须改用作业携带的 AIMD
    /// 码率（这里 6 Mbps），否则 1080p 的 IDR 只有几 KB，整屏文本糊掉且静态画面不会自愈。
    /// 编码器的**创建**码率也必须是关键帧码率：MFT 的峰值与 VBV 只在创建时设定。
    #[tokio::test]
    async fn encode_loop_uses_keyframe_bitrate_for_idr() {
        let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(8);
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(4);
        let cancel = CancellationToken::new();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let factory = FakeH264EncoderFactory {
            log: Some(Arc::clone(&log)),
        };
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
            CodecId::H264,
            Arc::new(factory),
            UNCHANGED_RESEND_INTERVAL,
            Arc::new(ScreenSendMeter::new()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(latency::LatencyWindow::new())),
            IdrTracker::new(),
        ));
        // 内容上限把 P 帧压到 1.2 Mbps；关键帧仍应按 6 Mbps 编码。
        let capped = EncodeRates {
            bitrate_kbps: 1200,
            fps: 30,
            scale: 1.0,
        };
        let job = |force_keyframe: bool, salt: u8| EncodeJob {
            display: 0,
            rates: capped,
            keyframe_bitrate_kbps: 6000,
            force_keyframe,
            force_encode: false,
            capture_us: 0,
            capture_wait_us: None,
            capture_ts_us: 0,
            input_to_capture_us: None,
            input_to_change_us: None,
            frame: shaded(0, 64, 48, salt),
        };

        job_tx.send(job(true, 10)).await.expect("投递关键帧");
        let (flags, _, _) = next_video(&mut out_rx).await;
        assert!(flags & VIDEO_FLAG_KEYFRAME != 0);
        job_tx.send(job(false, 40)).await.expect("投递 P 帧");
        let (flags, _, _) = next_video(&mut out_rx).await;
        assert_eq!(flags & VIDEO_FLAG_KEYFRAME, 0);

        // 守卫的作用域必须结束在后面的 `await` 之前（clippy::await_holding_lock）。
        {
            let recorded = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                recorded.first().copied(),
                Some((6000, true)),
                "关键帧必须用 AIMD 码率编码（不是内容上限的 1200）"
            );
            assert_eq!(
                recorded.get(1).copied(),
                Some((1200, false)),
                "P 帧仍用内容上限收紧后的生效码率"
            );
        }

        drop(job_tx);
        let _ = tokio::time::timeout(WAIT, task)
            .await
            .expect("编码任务应结束");
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
            Arc::new(FakeH264EncoderFactory::default()),
            UNCHANGED_RESEND_INTERVAL,
            Arc::new(ScreenSendMeter::new()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(latency::LatencyWindow::new())),
            IdrTracker::new(),
        ));

        job_tx
            .send(EncodeJob {
                display: 0,
                rates: EncodeRates::ceiling(EncodeOptions::default()),
                keyframe_bitrate_kbps: EncodeOptions::default().target_bitrate_kbps,
                force_keyframe: true,
                force_encode: false,
                capture_us: 0,
                capture_wait_us: None,
                capture_ts_us: 0,
                input_to_capture_us: None,
                input_to_change_us: None,
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
        let _ = tokio::time::timeout(WAIT, task)
            .await
            .expect("编码任务应结束");
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
            Arc::new(ScreenSendMeter::new()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(latency::LatencyWindow::new())),
            IdrTracker::new(),
        ));
        let opts = EncodeRates::ceiling(EncodeOptions::default());
        let job = |frame: RawFrame, force: bool| EncodeJob {
            display: 0,
            rates: opts,
            keyframe_bitrate_kbps: opts.bitrate_kbps,
            force_keyframe: force,
            force_encode: false,
            capture_us: 0,
            capture_wait_us: None,
            capture_ts_us: 0,
            input_to_capture_us: None,
            input_to_change_us: None,
            frame,
        };
        let placeholder = || SessionFrame::control(SessionMessage::Heartbeat { t1_us: 0 });

        // 首帧：先下发 `ScreenConfig`，随后是关键帧。
        job_tx
            .send(job(shaded(0, 64, 48, 10), true))
            .await
            .expect("投递首帧");
        assert!(
            matches!(
                out_rx.recv().await,
                Some(SessionFrame {
                    message: SessionMessage::ScreenConfig { .. },
                    ..
                })
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
        job_tx
            .send(job(shaded(0, 64, 48, 30), true))
            .await
            .expect("投递被跳过的帧");
        // 任务取走该任务并回到等待（跳过后立即 `continue` 到下一次 `recv`）→ 跳过已发生。
        wait_for(|| job_tx.capacity() == JOB_CAP).await;

        // 腾出位置后投递一个**不要求**关键帧的任务：它必须补上被跳过的那个 IDR。
        assert!(out_rx.recv().await.is_some(), "腾出一格");
        assert!(out_rx.recv().await.is_some(), "腾出另一格");
        job_tx
            .send(job(shaded(0, 64, 48, 40), false))
            .await
            .expect("投递下一帧");
        let retried = out_rx.recv().await.expect("应收到补发帧");
        assert!(
            is_keyframe(&retried),
            "被跳过的关键帧请求必须在下一帧补上（否则控制器会卡到周期性兜底）"
        );

        // 关掉任务队列让编码任务自然退出（不必等满 `WAIT`）。
        drop(job_tx);
        let _ = tokio::time::timeout(WAIT, task)
            .await
            .expect("编码任务应结束");
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
            Arc::new(FakeH264EncoderFactory::default()),
            // 语义锁：间隔足够大时「无变化就跳过」必须保持原语义（本用例不断言保活）。
            Duration::from_secs(3600),
            Arc::new(ScreenSendMeter::new()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(latency::LatencyWindow::new())),
            IdrTracker::new(),
        ));
        let rates = EncodeRates::ceiling(EncodeOptions::default());
        let gpu_job = |changed: bool, force: bool| EncodeJob {
            display: 0,
            rates,
            keyframe_bitrate_kbps: rates.bitrate_kbps,
            force_keyframe: force,
            force_encode: false,
            capture_us: 0,
            capture_wait_us: None,
            capture_ts_us: 0,
            input_to_capture_us: None,
            input_to_change_us: None,
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
                Some(SessionFrame {
                    message: SessionMessage::ScreenConfig { .. },
                    ..
                })
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
        let _ = tokio::time::timeout(WAIT, task)
            .await
            .expect("编码任务应结束");
    }

    /// **`force_encode` 语义锁**：补丁模式的基准帧要求「必须编码一次」但**不是** IDR。
    ///
    /// 同一像素内容、采集层报告无变化时，它仍必须产出一个 `VideoPacket`（否则基准帧会被
    /// 「无变化就跳过」闸门吞掉，会话循环只能等到 `PATCH_BASE_TIMEOUT` 放弃进入），且
    /// `KEYFRAME` 位必须为 0（参考链没断，插 IDR 只是带宽尖峰）。
    #[tokio::test]
    async fn forced_encode_without_idr_emits_p_frame_even_when_unchanged() {
        let (out_tx, mut out_rx) = mpsc::channel::<SessionFrame>(8);
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
            CodecId::H264,
            Arc::new(FakeH264EncoderFactory::default()),
            // 保活间隔足够大：本用例只验证 `force_encode` 单独绕过无变化闸门。
            Duration::from_secs(3600),
            Arc::new(ScreenSendMeter::new()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(latency::LatencyWindow::new())),
            IdrTracker::new(),
        ));
        let rates = EncodeRates::ceiling(EncodeOptions::default());
        let gpu_job = |changed: bool, force_keyframe: bool, force_encode: bool| EncodeJob {
            display: 0,
            rates,
            keyframe_bitrate_kbps: rates.bitrate_kbps,
            force_keyframe,
            force_encode,
            capture_us: 0,
            capture_wait_us: None,
            capture_ts_us: 0,
            input_to_capture_us: None,
            input_to_change_us: None,
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

        // 首帧：建立参考链。
        job_tx
            .send(gpu_job(true, true, false))
            .await
            .expect("投递首帧");
        assert!(
            matches!(
                out_rx.recv().await,
                Some(SessionFrame {
                    message: SessionMessage::ScreenConfig { .. },
                    ..
                })
            ),
            "首帧前应先下发 ScreenConfig"
        );
        let (flags, ..) = next_video(&mut out_rx).await;
        assert!(flags & VIDEO_FLAG_KEYFRAME != 0, "首帧应是关键帧");

        // 同内容、无变化、不要求 IDR，但必须编码一次：正是补丁基准帧的语义。
        job_tx
            .send(gpu_job(false, false, true))
            .await
            .expect("投递基准帧");
        let (flags, ..) = next_video(&mut out_rx).await;
        assert_eq!(
            flags & VIDEO_FLAG_KEYFRAME,
            0,
            "force_encode 必须绕过去重闸门，但不得带出 IDR"
        );

        drop(job_tx);
        let _ = tokio::time::timeout(WAIT, task)
            .await
            .expect("编码任务应结束");
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
            Arc::new(FakeH264EncoderFactory::default()),
            Duration::ZERO,
            Arc::new(ScreenSendMeter::new()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            MAX_ENCODE_WIDTH,
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(latency::LatencyWindow::new())),
            IdrTracker::new(),
        ));
        let rates = EncodeRates::ceiling(EncodeOptions::default());
        let gpu_job = |changed: bool, force: bool| EncodeJob {
            display: 0,
            rates,
            keyframe_bitrate_kbps: rates.bitrate_kbps,
            force_keyframe: force,
            force_encode: false,
            capture_us: 0,
            capture_wait_us: None,
            capture_ts_us: 0,
            input_to_capture_us: None,
            input_to_change_us: None,
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
                Some(SessionFrame {
                    message: SessionMessage::ScreenConfig { .. },
                    ..
                })
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
        let _ = tokio::time::timeout(WAIT, task)
            .await
            .expect("编码任务应结束");
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

        let (mut peer_reader, mut peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

        // 等一次采集开始（它随即在阻塞线程里 sleep 400ms）。
        tokio::time::timeout(WAIT, started_rx.recv())
            .await
            .expect("采集应开始")
            .expect("信号通道应存活");

        // 采集正阻塞：此刻发一条输入，它应在采集结束前（200ms 窗口内）被处理。
        peer_writer
            .send(SessionFrame::control(SessionMessage::Input(
                InputEvent::MouseMove {
                    x: 1,
                    y: 2,
                    display: 0,
                },
            )))
            .await
            .expect("发送输入应成功");
        let applied = tokio::time::timeout(Duration::from_millis(200), input_rx.recv()).await;
        assert!(
            applied.is_ok(),
            "采集阻塞期间循环仍应处理入站消息（修复前会被慢采集饿死）"
        );

        cancel.cancel();
        let drain = tokio::spawn(async move { while peer_reader.next().await.is_some() {} });
        let _ = tokio::time::timeout(WAIT, ctld)
            .await
            .expect("被控端应结束");
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
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
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

        let (_peer_reader, _peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

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
            config_with(
                pending_with("TOK"),
                RemoteMode::Control,
                Duration::from_millis(100),
            ),
            run_screen,
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        // 对端读半边必须活着（`_` 前缀的具名绑定不会被立即丢弃）：它一断，写失败会让会话先结束。
        let (_peer_reader, _peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

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
            config_with(
                pending_with("TOK"),
                RemoteMode::Control,
                Duration::from_millis(50),
            ),
            run_screen,
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        let (_peer_reader, _peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

        let result = tokio::time::timeout(WAIT, ctld)
            .await
            .expect("挂死的采集应被看门狗终止（不得无限重试）")
            .expect("任务不应 panic");
        let err = result.expect_err("连续超时后应结束会话");
        assert!(
            err.to_string().contains("超时"),
            "错误应说明采集超时，实得 {err}"
        );
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
        let (_peer_reader, mut peer_writer) =
            controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;

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
    fn build_region_updates_compress_rects_and_keep_coordinates() {
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
        let messages =
            build_region_updates(&mut compressor, 3, &patches, 7_000).expect("应在预算内");
        assert_eq!(messages.len(), 1, "小补丁应装进同一条消息");
        let msg = messages.into_iter().next().expect("一条消息");
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
        assert_eq!(
            (display, width, height),
            (3, 64, 48),
            "目标帧缓冲尺寸应原样带回"
        );
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
    fn build_region_updates_keep_empty_keepalive() {
        let mut compressor = zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL).expect("压缩器");
        let patches = CapturedPatches {
            display: 0,
            width: 2,
            height: 2,
            changed_permille: 0,
            rects: Vec::new(),
        };
        let messages =
            build_region_updates(&mut compressor, 0, &patches, 7_000).expect("空补丁也应构造");
        assert_eq!(messages.len(), 1, "空补丁只产出一条保活消息");
        match &messages[0] {
            SessionMessage::RegionUpdate {
                width,
                height,
                rects,
                ..
            } => {
                assert_eq!((*width, *height), (2, 2));
                assert!(rects.is_empty(), "保活补丁的矩形列表为空");
            }
            other => panic!("应为 RegionUpdate，实得 {other:?}"),
        }
    }

    /// 单拍总量闸门：整块不可压缩的 512×512 NV12（393216 字节）即使拆批也装不进
    /// [`PATCH_MAX_TICK_BYTES`]/[`PATCH_MAX_MESSAGES_PER_TICK`] 时必须放弃补丁。
    #[test]
    fn build_region_updates_reject_payload_over_tick_budget() {
        let mut compressor = zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL).expect("压缩器");
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
            build_region_updates(&mut compressor, 0, &patches, 7_000).is_none(),
            "超过单拍总量/条数预算应放弃补丁（回到全帧）"
        );
    }

    /// 矩形数闸门：超过 [`MAX_REGION_RECTS`] 时必须放弃补丁。
    #[test]
    fn build_region_updates_reject_too_many_rects() {
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
            build_region_updates(&mut compressor, 0, &patches, 7_000).is_none(),
            "矩形数超上限应放弃补丁（回到全帧）"
        );
    }

    /// 把横条按坐标写回目标帧缓冲，并抽出原矩形区域用于逐字节比对。
    fn reassemble_rect(
        messages: &[SessionMessage],
        frame_width: u32,
        frame_height: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let fw = usize::try_from(frame_width).expect("宽");
        let fh = usize::try_from(frame_height).expect("高");
        let mut frame = vec![0u8; fw * fh * 3 / 2];
        for msg in messages {
            let SessionMessage::RegionUpdate { rects, .. } = msg else {
                panic!("应为 RegionUpdate，实得 {msg:?}");
            };
            for band in rects {
                let band_w = usize::try_from(band.width).expect("带宽");
                let band_h = usize::try_from(band.height).expect("带高");
                let expected = band_w * band_h * 3 / 2;
                let data = if band.flags & REGION_FLAG_ZSTD != 0 {
                    zstd::bulk::decompress(&band.data, expected).expect("补丁解压")
                } else {
                    band.data.clone()
                };
                assert_eq!(data.len(), expected, "横条载荷长度必须与几何一致");
                for row in 0..band_h {
                    let dst = (usize::try_from(band.y).expect("y") + row) * fw
                        + usize::try_from(band.x).expect("x");
                    let src = row * band_w;
                    frame[dst..dst + band_w].copy_from_slice(&data[src..src + band_w]);
                }
                let uv_base = fw * fh;
                for row in 0..band_h / 2 {
                    let dst = uv_base
                        + (usize::try_from(band.y).expect("y") / 2 + row) * fw
                        + usize::try_from(band.x).expect("x");
                    let src = band_w * band_h + row * band_w;
                    frame[dst..dst + band_w].copy_from_slice(&data[src..src + band_w]);
                }
            }
        }
        // 抽出原矩形（Y 平面 + UV 平面），布局与输入 `nv12` 完全一致。
        let rw = usize::try_from(width).expect("宽");
        let rh = usize::try_from(height).expect("高");
        let rx = usize::try_from(x).expect("x");
        let ry = usize::try_from(y).expect("y");
        let mut extracted = Vec::with_capacity(rw * rh * 3 / 2);
        for row in 0..rh {
            let start = (ry + row) * fw + rx;
            extracted.extend_from_slice(&frame[start..start + rw]);
        }
        let uv_base = fw * fh;
        for row in 0..rh / 2 {
            let start = uv_base + (ry / 2 + row) * fw + rx;
            extracted.extend_from_slice(&frame[start..start + rw]);
        }
        extracted
    }

    /// 拆批必须**无缝、无重叠、无损**：把横条拼回目标帧缓冲后与原矩形逐字节相等。
    #[test]
    fn split_patch_bands_reconstruct_original() {
        let mut compressor = zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL).expect("压缩器");
        let original = pseudo_nv12(16, 16);
        let patches = CapturedPatches {
            display: 0,
            width: 64,
            height: 64,
            changed_permille: 0,
            rects: vec![ScreenPatch {
                x: 4,
                y: 6,
                width: 16,
                height: 16,
                nv12: original.clone(),
            }],
        };
        // 很小的单条上限：16×16 的 384 B 载荷必然被拆成多条横条。
        let messages =
            build_region_updates_with_cap(&mut compressor, 0, &patches, 0, 100).expect("应能拆批");
        assert!(
            messages.len() >= 2,
            "小上限下必须拆成多条消息，实得 {}",
            messages.len()
        );
        let rebuilt = reassemble_rect(&messages, 64, 64, 4, 6, 16, 16);
        assert_eq!(rebuilt, original, "横条拼回后必须与原矩形逐字节相等");
    }

    /// 单个矩形压缩后超过单条上限时必须**拆批而不是丢弃**：≥2 条消息、每条 ≤ 上限。
    #[test]
    fn oversized_single_patch_is_split_not_dropped() {
        let mut compressor = zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL).expect("压缩器");
        // 256×128 的伪随机 NV12 = 49152 B，超过 32 KiB 单条上限。
        let patches = CapturedPatches {
            display: 0,
            width: 256,
            height: 128,
            changed_permille: 20,
            rects: vec![ScreenPatch {
                x: 0,
                y: 0,
                width: 256,
                height: 128,
                nv12: pseudo_nv12(256, 128),
            }],
        };
        let messages =
            build_region_updates(&mut compressor, 0, &patches, 7_000).expect("拆批后应装得下");
        assert!(
            messages.len() >= 2,
            "超过单条上限的矩形必须拆成多条消息，实得 {}",
            messages.len()
        );
        let mut total = 0usize;
        for msg in &messages {
            let bytes = usize::try_from(region_update_bytes(msg)).expect("字节数");
            assert!(
                bytes <= PATCH_MESSAGE_MAX_BYTES,
                "单条消息 {bytes} B 超过上限 {PATCH_MESSAGE_MAX_BYTES} B"
            );
            total += bytes;
        }
        let sum_of_rects: usize = messages
            .iter()
            .map(|msg| match msg {
                SessionMessage::RegionUpdate { rects, .. } => {
                    rects.iter().map(|rect| rect.data.len()).sum()
                }
                _ => 0,
            })
            .sum();
        assert_eq!(total, sum_of_rects, "总量必须等于各条消息之和");
        assert!(total <= PATCH_MAX_TICK_BYTES, "总字节应落在单拍预算内");
    }

    /// 拆出来的所有横条：偶数几何、x/width 与原矩形一致、按顺序无缝覆盖原高度。
    #[test]
    fn bands_have_even_geometry() {
        let mut compressor = zstd::bulk::Compressor::new(PATCH_ZSTD_LEVEL).expect("压缩器");
        let (x, y, width, height) = (8_u32, 10_u32, 16_u32, 16_u32);
        let patches = CapturedPatches {
            display: 0,
            width: 64,
            height: 64,
            changed_permille: 0,
            rects: vec![ScreenPatch {
                x,
                y,
                width,
                height,
                nv12: pseudo_nv12(width, height),
            }],
        };
        let messages =
            build_region_updates_with_cap(&mut compressor, 0, &patches, 0, 100).expect("应能拆批");
        let bands: Vec<&RegionRect> = messages
            .iter()
            .flat_map(|msg| match msg {
                SessionMessage::RegionUpdate { rects, .. } => rects.iter(),
                _ => panic!("应为 RegionUpdate"),
            })
            .collect();
        assert!(bands.len() >= 2, "小上限下应拆出多条横条");
        let mut expected_y = y;
        for band in &bands {
            assert_eq!(band.x, x, "横条的 x 必须与原矩形一致");
            assert_eq!(band.width, width, "横条的 width 必须与原矩形一致");
            assert!(
                band.y.is_multiple_of(2),
                "横条 y 必须是偶数，实得 {}",
                band.y
            );
            assert!(
                band.height.is_multiple_of(2),
                "横条 height 必须是偶数，实得 {}",
                band.height
            );
            assert!(band.height >= 2, "横条至少 2 行");
            assert_eq!(band.y, expected_y, "横条必须首尾相接（无缝、无重叠）");
            expected_y += band.height;
        }
        assert_eq!(expected_y, y + height, "横条必须恰好覆盖原矩形的高度");
    }

    /// 进入判据必须**连续**低活跃；一帧高活跃把计数清零。
    #[test]
    fn patch_mode_entry_needs_consecutive_quiet_captures() {
        let mut mode = PatchMode::new();
        let now = Instant::now();
        // 旧门槛（3 拍）必须**不再**足以进入：真机上它把每次 100 ms 的短暂安静都当成静止
        // 桌面，64 秒进入 21 次、IDR 占 43% 字节（见 [`PATCH_ENTER_TICKS`]）。
        for _ in 0..3 {
            assert!(
                !mode.observe_video(PATCH_ENTER_PERMILLE, now),
                "3 拍安静不该再触发进入（真实门槛是 {PATCH_ENTER_TICKS} 拍）"
            );
        }
        // 一帧高活跃打断连续计数。
        assert!(!mode.observe_video(PATCH_ENTER_PERMILLE + 1, now));
        for _ in 0..PATCH_ENTER_TICKS - 1 {
            assert!(!mode.observe_video(PATCH_ENTER_PERMILLE, now));
        }
        assert!(
            mode.observe_video(PATCH_ENTER_PERMILLE, now),
            "连续 {PATCH_ENTER_TICKS} 次低活跃才应请求基准帧"
        );
        assert!(mode.enter_pending);
        // 已在等待基准帧时不重复触发。
        assert!(!mode.observe_video(0, now));
        assert_eq!(mode.mode, ScreenMode::Video, "基准未发出前不得切模式");
    }

    /// 退出后的重入冷却：冷却期内连续安静也不进入，冷却结束才重新累计。
    #[test]
    fn patch_reentry_respects_holdoff() {
        let mut mode = PatchMode::new();
        let t0 = Instant::now();
        // 一次「长命」补丁会话（≥ 2 × 短会话阈值）：退避归零回基础冷却。
        mode.mode = ScreenMode::Patches;
        mode.entered_at = Some(t0);
        let exited = t0 + PATCH_SHORT_SESSION * 2;
        mode.exit(PatchExitReason::Activity, exited);
        assert_eq!(mode.holdoff_backoff, PATCH_REENTRY_HOLDOFF);
        assert!(mode.entered_at.is_none(), "退出后必须清掉存活计时");
        let holdoff = exited + PATCH_REENTRY_HOLDOFF;
        // 冷却期内喂再多安静拍也不得触发（`enter_streak` 每拍被清零）。
        for tick in 0..14_u32 {
            let at = exited + Duration::from_millis(333) * tick;
            assert!(at < holdoff, "构造的采样点必须落在冷却期内");
            assert!(
                !mode.observe_video(0, at),
                "冷却期内第 {tick} 拍不得进入补丁模式"
            );
        }
        // 冷却结束后需要重新凑满 PATCH_ENTER_TICKS 个安静拍。
        for tick in 0..PATCH_ENTER_TICKS - 1 {
            assert!(!mode.observe_video(0, holdoff + Duration::from_millis(33) * tick));
        }
        assert!(
            mode.observe_video(
                0,
                holdoff + Duration::from_millis(33) * (PATCH_ENTER_TICKS - 1)
            ),
            "冷却结束且连续 {PATCH_ENTER_TICKS} 拍安静后应允许重新进入"
        );
    }

    /// 重入冷却按上一次补丁模式的**存活时长**指数退避，并在长命会话后归零。
    ///
    /// 真机上「用户每隔几秒碰一下鼠标/键盘」会让补丁模式反复进入又立刻退出：固定冷却挡不住
    /// 这种抖动，每次都要为进入付一张基准 IDR（平均 223 KB）。短命会话 5 → 10 → 20 秒
    /// （[`PATCH_REENTRY_HOLDOFF_MAX`] 封顶），一旦撑过 2 × [`PATCH_SHORT_SESSION`] 就归零。
    #[test]
    fn patch_holdoff_backs_off_on_short_sessions_and_resets_on_long_ones() {
        assert_eq!(PATCH_REENTRY_HOLDOFF, Duration::from_secs(5));
        assert_eq!(PATCH_REENTRY_HOLDOFF_MAX, Duration::from_secs(20));
        assert_eq!(PATCH_SHORT_SESSION, Duration::from_secs(2));

        let mut mode = PatchMode::new();
        let t0 = Instant::now();
        // 存活 1.999 秒：算「短命」，冷却 5 → 10 → 20 → 封顶仍是 20。
        let short = PATCH_SHORT_SESSION
            .checked_sub(Duration::from_millis(1))
            .expect("短命阈值远大于 1 ms");
        for want in [
            Duration::from_secs(10),
            Duration::from_secs(20),
            PATCH_REENTRY_HOLDOFF_MAX,
        ] {
            mode.mode = ScreenMode::Patches;
            mode.entered_at = Some(t0);
            mode.exit(PatchExitReason::Activity, t0 + short);
            assert_eq!(mode.holdoff_backoff, want, "短命会话必须翻倍并封顶");
            assert_eq!(mode.holdoff_until, Some(t0 + short + want), "冷却按新退避设置");
        }
        // 存活落在 [2s, 4s)：既不算抖动、也还没证明「真静下来了」——退避保持不动。
        let borderline = (PATCH_SHORT_SESSION * 2)
            .checked_sub(Duration::from_millis(1))
            .expect("两倍阈值远大于 1 ms");
        mode.mode = ScreenMode::Patches;
        mode.entered_at = Some(t0);
        mode.exit(PatchExitReason::Activity, t0 + borderline);
        assert_eq!(
            mode.holdoff_backoff, PATCH_REENTRY_HOLDOFF_MAX,
            "中间时长不该让退避回落"
        );
        // 存活 ≥ 2 × 阈值：退避归零回基础冷却。
        mode.mode = ScreenMode::Patches;
        mode.entered_at = Some(t0);
        mode.exit(PatchExitReason::Activity, t0 + PATCH_SHORT_SESSION * 2);
        assert_eq!(mode.holdoff_backoff, PATCH_REENTRY_HOLDOFF);
        assert_eq!(
            mode.holdoff_until,
            Some(t0 + PATCH_SHORT_SESSION * 2 + PATCH_REENTRY_HOLDOFF)
        );
    }

    /// 退出原因字符串是现场排查入口：取值必须稳定（日志/统计靠它做 grep 与分桶）。
    #[test]
    fn patch_exit_reasons_have_stable_strings() {
        for (reason, want) in [
            (PatchExitReason::Activity, "activity"),
            (PatchExitReason::Rects, "rects"),
            (PatchExitReason::Budget, "budget"),
            (PatchExitReason::StagingFull, "staging_full"),
            (PatchExitReason::StagingAge, "staging_age"),
            (PatchExitReason::NoPatches, "no_patches"),
            (PatchExitReason::SizeMismatch, "size_mismatch"),
            (PatchExitReason::KeyframeRequest, "keyframe_request"),
            (PatchExitReason::Reconfigure, "reconfigure"),
        ] {
            assert_eq!(reason.as_str(), want);
            assert_eq!(reason.to_string(), want, "Display 必须与 as_str 同值");
        }
    }

    /// 出码窗口换算：累计字节差按**实际间隔**归一，间隔为 0 时不除零。
    #[test]
    fn window_kbps_converts_bytes_over_actual_elapsed() {
        // 1 秒内多出 125 000 B → 1000 kbps（125000 × 8 / 1000 ms）。
        assert_eq!(window_kbps(0, 125_000, Duration::from_secs(1)), 1000);
        // 500 ms 内多出 62 500 B → 同样 1000 kbps（按实际间隔归一，不是按固定 1 s）。
        assert_eq!(
            window_kbps(10_000, 72_500, Duration::from_millis(500)),
            1000
        );
        // 计数未增长 → 0。
        assert_eq!(window_kbps(4096, 4096, Duration::from_secs(1)), 0);
        // 极端：间隔为 0 不得 panic/除零（`kbps_from_bytes` 以 max(1 ms) 兜底）。
        assert_eq!(window_kbps(0, 0, Duration::ZERO), 0);
        assert!(window_kbps(0, 1000, Duration::ZERO) > 0);
        // 计数器回绕/异常：饱和到 0 而不是巨大值。
        assert_eq!(window_kbps(500, 100, Duration::from_secs(1)), 0);
        // 计量器本身：累加与读取口径一致。
        let meter = ScreenSendMeter::new();
        meter.add_bytes(4096);
        meter.add_bytes(1024);
        assert_eq!(meter.total_bytes(), 5120);
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
        assert_eq!(
            encode_target_dims(1920, 1080, 1.0, MAX_ENCODE_WIDTH),
            (1920, 1080)
        );
        assert_eq!(
            encode_target_dims(1920, 1080, 0.5, MAX_ENCODE_WIDTH),
            (960, 540)
        );
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
        assert_eq!(
            effective_max_encode_width(CodecId::Vp9, true),
            MAX_ENCODE_WIDTH
        );
        assert_eq!(
            effective_max_encode_width(CodecId::H264, false),
            MAX_ENCODE_WIDTH
        );
        assert_eq!(
            effective_max_encode_width(CodecId::Vp9, false),
            MAX_ENCODE_WIDTH
        );
        assert_eq!(NATIVE_MAX_ENCODE_WIDTH, 3840);
    }

    /// 原生分辨率只按**显示器面积**放大码率上限：帧率/缩放原样，封顶 60 Mbps。
    #[test]
    fn native_resolution_scales_only_the_bitrate_ceiling() {
        let hd = EncodeOptions::default();
        // 4K 显示器：4 倍。
        let four_k = session_encode_options(hd, 4000);
        assert_eq!(
            four_k.target_bitrate_kbps,
            hd.target_bitrate_kbps * crate::screen::NATIVE_BITRATE_FACTOR
        );
        assert_eq!(four_k.max_fps, hd.max_fps, "帧率不动");
        assert!((four_k.scale - hd.scale).abs() < f32::EPSILON, "缩放不动");
        // 1080p 显示器（1000‰）：逐字段原样（开了原生开关也不凭空加码率）。
        assert_eq!(session_encode_options(hd, 1000), hd);
        // 2.5K：按像素数 1.78 倍。
        let qhd = session_encode_options(hd, 1777);
        assert_eq!(qhd.target_bitrate_kbps, 6000 * 1777 / 1000);
        // 各档位放大后都不越过封顶。
        for preset in crate::screen::QualityPreset::all() {
            let scaled = session_encode_options(preset.options(), 4000);
            assert_eq!(
                scaled.target_bitrate_kbps,
                preset.options().target_bitrate_kbps * crate::screen::NATIVE_BITRATE_FACTOR
            );
            assert!(scaled.target_bitrate_kbps <= crate::screen::NATIVE_MAX_BITRATE_KBPS);
        }
        // 非原生（`native_active == false`）恒 1000。
        assert_eq!(native_bitrate_permille(None, false), 1000);
    }

    /// 内容上限的面积倍数：1080p = 1000‰，4K = 4000‰，与编码尺寸同口径。
    #[test]
    fn content_area_permille_follows_encode_dimensions() {
        // 1080p 源、不缩放。
        assert_eq!(
            content_area_permille(1920, 1080, 1.0, MAX_ENCODE_WIDTH),
            1000
        );
        // 4K 源在原生上限下不缩放。
        assert_eq!(
            content_area_permille(3840, 2160, 1.0, NATIVE_MAX_ENCODE_WIDTH),
            4000
        );
        // 4K 源被默认 1920 上限封顶 → 仍是 1080p 的面积。
        assert_eq!(
            content_area_permille(3840, 2160, 1.0, MAX_ENCODE_WIDTH),
            1000
        );
        // QoS 降分辨率后面积按比例下降，但 setter 会钳到 ≥1000。
        assert_eq!(
            content_area_permille(3840, 2160, 0.75, NATIVE_MAX_ENCODE_WIDTH),
            2250
        );
    }

    /// 顺序不变量：基准帧**真正写出**后才进入补丁模式；等不到就放弃并清零计数。
    #[test]
    fn patch_mode_enters_only_after_base_frame_and_abandons_on_timeout() {
        let t0 = Instant::now();
        let mut mode = PatchMode::new();
        for _ in 0..PATCH_ENTER_TICKS {
            mode.observe_video(0, t0);
        }
        assert_eq!(mode.poll_entry(false, t0 + Duration::from_millis(1)), None);
        assert_eq!(mode.mode, ScreenMode::Video, "基准未发出前不得切模式");
        assert_eq!(
            mode.poll_entry(true, t0 + Duration::from_millis(2)),
            Some(true)
        );
        assert_eq!(mode.mode, ScreenMode::Patches);
        assert!(mode.last_patch_sent.is_some(), "进入时保活计时从此刻起算");
        assert!(
            mode.entered_at.is_some(),
            "进入时必须记下存活计时（退出时据此决定冷却退避）"
        );

        // 超时路径：基准迟迟发不出去（例如出站队列被背压卡住）。
        let mut mode = PatchMode::new();
        for _ in 0..PATCH_ENTER_TICKS {
            mode.observe_video(0, t0);
        }
        assert_eq!(mode.poll_entry(false, t0 + PATCH_BASE_TIMEOUT), Some(false));
        assert_eq!(mode.mode, ScreenMode::Video);
        assert_eq!(mode.enter_streak, 0, "放弃后连续计数必须清零");
    }

    /// 造一条补丁消息：`seed` 写进 `data`，便于断言暂存区/投递的 FIFO 顺序。
    fn patch_message(seed: u8, bytes: usize) -> SessionMessage {
        SessionMessage::RegionUpdate {
            display: 0,
            width: 32,
            height: 24,
            rects: vec![RegionRect {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
                flags: 0,
                data: vec![seed; bytes],
            }],
            capture_ts_us: 0,
        }
    }

    /// 取回 [`patch_message`] 写进去的 `seed`。
    fn patch_seed(frame: &SessionFrame) -> u8 {
        match &frame.message {
            SessionMessage::RegionUpdate { rects, .. } => {
                rects.first().map_or(u8::MAX, |rect| rect.data[0])
            }
            other => panic!("期望 RegionUpdate，实得 {other:?}"),
        }
    }

    /// 暂存区**跨拍续投且严格 FIFO**：队满时一条都不丢，投出一部分后仍保持原序。
    ///
    /// 这是「补丁绝不丢」的核心断言：丢了控制端合成缓冲会永久缺一块（直到下一个 IDR）。
    #[test]
    fn patch_staging_drains_fifo_across_capture_ticks() {
        let t0 = Instant::now();
        let mut staging = PatchStaging::new();
        // 两拍：A 批 2 条、B 批 2 条，按采集顺序进区。
        assert!(staging.push_batch(vec![patch_message(1, 100), patch_message(2, 100)], t0));
        assert!(staging.push_batch(
            vec![patch_message(3, 100), patch_message(4, 100)],
            t0 + Duration::from_millis(33)
        ));
        assert_eq!(staging.bytes(), 400);

        // 第一拍：画面队列只剩 1 个空位 → 只投出 A 批首条，其余留在暂存区。
        let mut sent: Vec<u8> = Vec::new();
        let count = staging
            .drain(|frame, _bytes| {
                if !sent.is_empty() {
                    return Ok(Some(frame));
                }
                sent.push(patch_seed(&frame));
                Ok(None)
            })
            .expect("写任务仍在");
        assert_eq!(count, 1);
        assert_eq!(sent, vec![1]);
        assert_eq!(staging.bytes(), 300, "投出的字节必须从暂存区扣掉");
        assert!(
            staging.partial(),
            "A 批首条已发出、其余仍滞留 ⇒ 半批（退出时需要 IDR 修复）"
        );

        // 第二拍：多腾出三个空位 → 严格按 FIFO 续投，不越过、不重排。
        let mut sent: Vec<u8> = Vec::new();
        staging
            .drain(|frame, _bytes| {
                if sent.len() >= 3 {
                    return Ok(Some(frame));
                }
                sent.push(patch_seed(&frame));
                Ok(None)
            })
            .expect("写任务仍在");
        assert_eq!(sent, vec![2, 3, 4], "续投必须保持采集顺序");
        assert!(!staging.partial(), "整批都投出后不再是半批");
        assert!(staging.is_empty());
        assert_eq!(staging.bytes(), 0);
    }

    /// 暂存区满 → **整批拒收**（调用方据此退出补丁模式）；空暂存区总能接下一整拍。
    #[test]
    fn patch_staging_rejects_a_batch_when_full() {
        let t0 = Instant::now();
        let mut staging = PatchStaging::new();
        let half = PATCH_STAGING_MAX_BYTES / 2;
        assert!(staging.push_batch(vec![patch_message(1, half)], t0));
        assert!(staging.push_batch(vec![patch_message(2, half)], t0));
        assert_eq!(staging.bytes(), PATCH_STAGING_MAX_BYTES, "恰好顶到上限");
        // 再来一批（哪怕只有 1 字节）：装不下就整批拒收，暂存区原样不动。
        assert!(
            !staging.push_batch(vec![patch_message(3, 1)], t0),
            "超上限必须拒收，调用方据此退出补丁模式"
        );
        assert_eq!(staging.bytes(), PATCH_STAGING_MAX_BYTES, "拒收不得改动暂存区");
        // 空暂存区总能接下**一整拍**（单拍上限 = 暂存区上限）：否则补丁会永远进不来。
        staging.clear();
        assert!(staging.push_batch(vec![patch_message(4, PATCH_STAGING_MAX_BYTES)], t0));
    }

    /// 滞留判定按**头部**那条的入区时刻算：未到上限不退出，到点即退出。
    #[test]
    fn patch_staging_aborts_only_after_the_age_limit() {
        let t0 = Instant::now();
        let mut staging = PatchStaging::new();
        assert!(!staging.over_age(t0), "空暂存区不超龄");
        assert!(staging.push_batch(vec![patch_message(1, 10)], t0));
        assert!(
            !staging.over_age(t0 + PATCH_STAGING_MAX_AGE.saturating_sub(Duration::from_millis(1))),
            "差 1 ms 不该退出"
        );
        assert!(staging.over_age(t0 + PATCH_STAGING_MAX_AGE), "到点即超龄");

        // 头部投出后滞留时长从**新的头部**重新起算（它入区更晚）。
        let later = t0 + PATCH_STAGING_MAX_AGE / 2;
        assert!(staging.push_batch(vec![patch_message(2, 10)], later));
        let mut budget = 1_u32;
        staging
            .drain(|frame, _bytes| {
                if budget == 0 {
                    return Ok(Some(frame));
                }
                budget -= 1;
                Ok(None)
            })
            .expect("写任务仍在");
        assert!(
            !staging.over_age(t0 + PATCH_STAGING_MAX_AGE),
            "新头部只滞留了半个上限"
        );
        assert!(staging.over_age(later + PATCH_STAGING_MAX_AGE));
    }

    /// 去重闸门：IDR 在途、刚写出、或已有待发 IDR 时吸收；其余一律放行。
    #[test]
    fn keyframe_request_dedup_absorbs_in_flight_and_recent_ids() {
        let window_us = u64::try_from(KEYFRAME_DEDUP_WINDOW.as_micros()).expect("窗口是微秒级");
        let now = 10_000_000_u64;
        // 有 IDR 在途（编码/画面队列/写线途中）：吸收。
        assert!(keyframe_request_is_redundant(false, 1, 0, now));
        // 刚写出（窗口内）：吸收。
        assert!(keyframe_request_is_redundant(false, 0, now - window_us + 1, now));
        // 已经有一个待发的 IDR（force_keyframe 还没被编码任务消费）：吸收。
        assert!(keyframe_request_is_redundant(true, 0, 0, now));
        // 恰好到达窗口边界：不再吸收（控制器 400 ms 的重发必须能穿透）。
        assert!(!keyframe_request_is_redundant(false, 0, now - window_us, now));
        // 从没写过 IDR、也没有在途：必须放行，否则会话早期的请求会被饿死。
        assert!(!keyframe_request_is_redundant(false, 0, 0, now));
        // 安全前提写死在测试里：窗口必须**严格小于**控制器的 400 ms 重发周期。
        assert!(
            KEYFRAME_DEDUP_WINDOW < Duration::from_millis(400),
            "窗口不小于重发周期就会把「重发」也吞掉 = 永久抑制"
        );
    }

    /// IDR 在途计数：承诺/销账配对、写完记时间戳；RAII 守卫在没交接时自动销账。
    #[test]
    fn idr_tracker_balances_commit_release_and_records_write() {
        let tracker = IdrTracker::new();
        assert_eq!(tracker.in_flight(), 0);
        assert_eq!(tracker.last_written_us(), 0, "还没写过 IDR");

        {
            let guard = IdrInFlight::begin(Arc::clone(&tracker));
            assert_eq!(tracker.in_flight(), 1, "承诺编码就要立刻登记在途");
            // 没走到 `handoff` 就离开作用域（编码失败/空包）：必须自动销账，否则后续请求
            // 会被永久吸收。
            drop(guard);
        }
        assert_eq!(tracker.in_flight(), 0);

        let guard = IdrInFlight::begin(Arc::clone(&tracker));
        assert_eq!(tracker.in_flight(), 1);
        guard.handoff();
        assert_eq!(tracker.in_flight(), 1, "交接后由写任务销账，期间仍算在途");
        tracker.on_idr_written(42);
        assert_eq!(tracker.in_flight(), 0);
        assert_eq!(tracker.last_written_us(), 42);
        // 编码器自发插 IDR 时写任务也会销账一次：不得把计数减成负数。
        tracker.on_idr_written(99);
        assert_eq!(tracker.in_flight(), 0);
        assert_eq!(tracker.last_written_us(), 99);
    }

    /// 输入触发采集的提前量：最多提前到「上一拍之后半个周期」，且不早于当前时刻。
    ///
    /// 钉住三件事：① 还没采过时立刻采；② 刚采过时等半个周期（把相位等待从 `0..period`
    /// 压到 `0..period/2`）；③ 距上一拍已超过半个周期时立刻采（不能把节拍推回过去）。
    #[test]
    fn input_capture_target_pulls_at_most_half_a_period_early() {
        // 会话循环用的是 `tokio::time::Instant`（测试模块里的 `Instant` 是 std 的那个，
        // 供其它用例直接构造 `PatchMode` 的时刻），这里必须用同一个类型。
        let now = tokio::time::Instant::now();
        let period = Duration::from_millis(33);
        assert_eq!(input_capture_target(now, None, period), now, "首拍立刻采");

        let last = now;
        assert_eq!(
            input_capture_target(now + Duration::from_millis(2), Some(last), period),
            last + period / 2,
            "刚采过 → 只等到半个周期"
        );
        assert_eq!(
            input_capture_target(now + Duration::from_millis(20), Some(last), period),
            now + Duration::from_millis(20),
            "已超过半个周期 → 立刻采，不把目标时刻推到过去"
        );
    }

    /// 帧率被下调后，`next_capture_at` 必须与 `Sleep` 一起重排，否则输入提前采集会被丢弃。
    ///
    /// 复现修复前的失败序列：上一拍 T，30 fps 时计划时刻 T+16 ms；QoS 把帧率降到 15
    /// 并把下一拍重排到 T+16+66 ms。若计划时刻仍是陈旧的 T+16，输入到达时 `target`
    /// （T+33）不早于它，提前采集被静默丢弃；同步之后才成立。
    #[test]
    fn input_pull_requires_a_synchronized_capture_deadline() {
        let t0 = tokio::time::Instant::now();
        let period = Duration::from_millis(66);
        let last = t0;
        let now = t0 + Duration::from_millis(17);
        // 同一时刻、同一输入：陈旧的计划时刻让提前采集失效。
        assert_eq!(
            input_capture_pull(now, Some(last), period, t0 + Duration::from_millis(16)),
            None,
            "陈旧的 next_capture_at 会让提前采集被静默丢弃"
        );
        // `schedule_capture` 同步之后（T+16 重排到 T+16+66），提前采集正常生效。
        let scheduled = t0 + Duration::from_millis(16) + period;
        assert_eq!(
            input_capture_pull(now, Some(last), period, scheduled),
            Some(t0 + Duration::from_millis(33)),
            "同步后应提前到上一拍后半个周期"
        );
    }

    /// 支持补丁的假采集源：视频/补丁采集各自报告一个可调的活跃度，补丁采集返回一块固定脏矩形。
    ///
    /// 活跃度做成原子变量是为了在会话**运行中**模拟「用户突然开始拖窗」：把它推到
    /// [`PATCH_EXIT_PERMILLE`] 之上，下一次补丁采集就会走退出分支。
    struct PatchyScreen {
        patch_calls: AtomicUsize,
        /// 视频采集报告的变化面积（千分比）。
        video_permille: AtomicU16,
        /// 补丁采集报告的变化面积（千分比）。
        patch_permille: AtomicU16,
    }

    impl PatchyScreen {
        /// 静桌面：两个路径都远低于进入门槛。
        fn quiet() -> Self {
            Self {
                patch_calls: AtomicUsize::new(0),
                video_permille: AtomicU16::new(8),
                patch_permille: AtomicU16::new(8),
            }
        }
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
            frame.changed_permille = self.video_permille.load(Ordering::SeqCst);
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
                changed_permille: self.patch_permille.load(Ordering::SeqCst),
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

    /// 端到端：低活跃 + H.264 + 采集源支持补丁 → 先发基准帧（P 帧），再发 `RegionUpdate`；
    /// 控制器 `RequestKeyframe` 后必须退回全帧并重发 IDR。
    #[tokio::test]
    async fn patch_mode_sends_region_update_after_base_frame_and_exits_on_request() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let screen = Arc::new(PatchyScreen::quiet());
        let run_screen: Arc<dyn ScreenSource> = screen.clone();
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();

        let mut cfg = config(pending_with("TOK"), RemoteMode::Control);
        cfg.h264_encoder_factory = Some(Arc::new(FakeH264EncoderFactory::default()));
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
            controller_peer_handshake_with(Box::new(a), &plan, "TOK", "sess-1", Some((true, true)))
                .await;

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
            keyframes >= 1,
            "补丁之前至少已发出会话首帧；进入基准帧是 P 帧（补丁不碰参考链），不再额外插 IDR（实得 {keyframes}）"
        );
        assert!(
            screen.patch_calls.load(Ordering::SeqCst) >= 1,
            "应真的走补丁采集，而不是退回全帧"
        );

        // 控制器请求关键帧（参考丢失/队列溢出）→ 被控端退出补丁模式并重发全帧 IDR。
        //
        // 先等过**去重窗口**：刚进补丁模式时基准 IDR 是刚刚写出的，窗口内到达的请求会被
        // 吸收（真机上控制器在 `awaiting_keyframe` 期间每 400 ms 重发，重发必然落在窗口
        // 之外）。这里等过窗口，验证的正是「重发会被满足」这条路径。
        tokio::time::sleep(KEYFRAME_DEDUP_WINDOW + Duration::from_millis(50)).await;
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
        let _ = tokio::time::timeout(WAIT, ctld)
            .await
            .expect("被控端应结束");
    }

    /// 补丁模式的**基准帧必须是 IDR**，而「活跃度回升」退出后的**第一帧仍是 P 帧**。
    ///
    /// 基准帧不碰参考链，但它会成为补丁模式下静态区域的**长期基底**：若它继承了一张糊的
    /// 参考（切浏览器标签 → 整页重绘那帧被单帧预算压过），整页就再也不会自愈——补丁模式里
    /// 编码器不跑、intra refresh 也停了。所以进模式时付一个关键帧（RDP 换桌面时同样这么做），
    /// 退出时仍用 P 帧（参考链没断，插 IDR 只是白加一个带宽尖峰）。
    #[tokio::test]
    async fn patch_mode_base_is_an_idr_and_activity_exit_uses_a_p_frame() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let screen = Arc::new(PatchyScreen::quiet());
        let run_screen: Arc<dyn ScreenSource> = screen.clone();
        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();

        let mut cfg = config(pending_with("TOK"), RemoteMode::Control);
        cfg.h264_encoder_factory = Some(Arc::new(FakeH264EncoderFactory::default()));
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            cfg,
            run_screen,
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));
        // 声明支持 H.264：补丁模式的前提（VP9 的解码输出不维护持久 NV12 帧缓冲）。
        let (mut peer_reader, _peer_writer) =
            controller_peer_handshake_with(Box::new(a), &plan, "TOK", "sess-1", Some((true, true)))
                .await;

        // 读到第一块补丁为止：会话首帧一个 IDR + 进入基准帧一个 IDR（共 2 个）。
        let keyframes = tokio::time::timeout(WAIT, async {
            let mut keyframes = 0u32;
            loop {
                match next_message(&mut peer_reader).await {
                    SessionMessage::VideoPacket { flags, .. } => {
                        if flags & VIDEO_FLAG_KEYFRAME != 0 {
                            keyframes += 1;
                        }
                    }
                    SessionMessage::RegionUpdate { .. } => return keyframes,
                    _ => {}
                }
            }
        })
        .await
        .expect("低活跃画面应进入补丁模式并发出 RegionUpdate");
        assert_eq!(
            keyframes, 2,
            "补丁之前应有会话首帧 + 进入基准帧两个 IDR（实得 {keyframes}）"
        );

        // 模拟用户开始拖窗：下一次补丁采集报高活跃 → 退出补丁模式并立刻抓一帧全帧。
        screen.patch_permille.store(1000, Ordering::SeqCst);
        screen.video_permille.store(1000, Ordering::SeqCst);
        let exit_flags = tokio::time::timeout(WAIT, async {
            loop {
                if let SessionMessage::VideoPacket { flags, .. } =
                    next_message(&mut peer_reader).await
                {
                    return flags;
                }
            }
        })
        .await
        .expect("退出补丁模式后应发出全帧视频包");
        assert_eq!(
            exit_flags & VIDEO_FLAG_KEYFRAME,
            0,
            "活跃度回升退出补丁模式后的第一帧必须是 P 帧（参考链没断，不该插 IDR）"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(WAIT, ctld)
            .await
            .expect("被控端应结束");
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
            SessionMessage::HeartbeatAck { t2_us: 2, t3_us: 3 },
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

    /// 补丁投递必须**非阻塞**：画面队列满时立即把帧还回来（不是等空位、也不是丢帧）。
    ///
    /// 主循环的补丁路径只能用它（P1-1）：退化成 `.await` 的话，网络拥塞时输入注入、心跳、
    /// 采集节拍会被一起卡住。把帧还回来则让补丁暂存区能原样放回、保持 FIFO。
    #[tokio::test]
    async fn try_enqueue_screen_returns_the_frame_when_full() {
        let (queues, rx) = OutboundQueues::new();
        let probe = || {
            SessionFrame::screen(SessionMessage::ScreenConfig {
                display: 0,
                codec: CodecId::Vp9,
                width: 16,
                height: 16,
            })
        };
        for index in 0..SCREEN_QUEUE {
            assert!(
                queues
                    .try_enqueue_screen_or_return(probe())
                    .expect("写任务仍在")
                    .is_none(),
                "第 {index} 条应入队"
            );
        }
        // 函数是同步的：满队列只可能立刻把帧还回来，不存在「等空位」这条路径。
        match queues
            .try_enqueue_screen_or_return(probe())
            .expect("满不是错误")
        {
            Some(frame) => assert!(matches!(
                frame.message,
                SessionMessage::ScreenConfig { .. }
            )),
            None => panic!("队列已满时帧必须被还回来"),
        }
        // 写任务结束（接收端全部丢弃）≠ 队列满：必须报链路关闭，否则调用方会把它当成
        // 「只是忙」而无限循环。
        drop(rx);
        assert!(
            queues.try_enqueue_screen_or_return(probe()).is_err(),
            "写任务已结束时必须返回错误"
        );

        // pacing 默认关闭（控制器侧从不设置），设置后读写一致。
        assert_eq!(queues.screen_pacing_kbps(), 0, "默认不 pace");
        queues.set_screen_pacing_kbps(6000);
        assert_eq!(queues.screen_pacing_kbps(), 6000);
    }

    /// pacing 方案的两条上界：关闭/小帧不 pace；大帧的块数与块间等待都受控。
    #[test]
    fn pacing_plan_bounds_chunk_and_gap() {
        // 关闭（控制器侧）与小于门槛的帧：完全走原路径。
        assert!(pacing_plan(300 * 1024, 0).is_none(), "0 kbps = 关闭");
        assert!(
            pacing_plan(PACING_MIN_FRAME_BYTES - 1, 6000).is_none(),
            "小帧不 pace"
        );
        assert!(
            pacing_plan(PACING_MIN_FRAME_BYTES, 6000).is_some(),
            "门槛本身要 pace"
        );

        // 300 KiB @ 6 Mbps：32 KiB 一块（10 块），块间 ≈ 43.7 ms。
        let (chunk, gap) = pacing_plan(300 * 1024, 6000).expect("大帧应 pace");
        assert_eq!(chunk, PACING_CHUNK_BYTES, "块大小封顶 32 KiB");
        assert!(
            (Duration::from_millis(40)..=Duration::from_millis(50)).contains(&gap),
            "6 Mbps 上一块 32 KiB 约 43.7 ms，实得 {gap:?}"
        );
        let gap_count = u32::try_from((300 * 1024_usize).div_ceil(chunk) - 1).expect("块数很少");
        let frame_natural = Duration::from_secs_f64((300.0 * 1024.0 * 8.0) / 6_000_000.0);
        assert!(
            gap * gap_count <= frame_natural * PACING_MAX_WAIT_FACTOR,
            "总等待不得超过该帧自然串行时间的 2 倍"
        );

        // 低码率读数：单个间隙被 [`PACING_MAX_GAP`] 封顶（32 KiB @ 300 kbps 的自然时间约
        // 874 ms），一帧绝不会因此被拖成秒级。
        let (_, slow_gap) = pacing_plan(300 * 1024, 300).expect("大帧应 pace");
        assert_eq!(slow_gap, PACING_MAX_GAP, "低码率下用单个间隙上限");
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
