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
/// 解码在独立任务里串行进行（1080p 单帧约十几毫秒）。队列兼作**抖动缓冲**：16 深可以在
/// 一次 GC/调度抖动或 30–60 fps 的短突发中不丢包，同时把最坏内存压在「16 × 单帧载荷」
/// 量级——H.264 1080p 单帧载荷通常几十到几百 KiB，最坏（接近无损的 VP9 4:4:4 关键帧）
/// 约数 MiB，故上限大致在几十 MiB 以内；超过这个深度只是在解码跟不上时多堆积过期画面。
///
/// 队列满即丢帧——丢弃会打断 VP9 的参考链，故解码任务用**序号断层**检测到它并请求关键帧
/// （见 [`decode_loop`]）。队列满时还会推进 `drop_epoch`，让队列里的陈旧 P 帧整体作废，
/// 解码端直接跳到最新包。
const DECODE_QUEUE: usize = 16;

/// 关键帧请求的最小间隔。
///
/// 序号断层/队列溢出/解码失败都可能每帧触发一次「请补 IDR」。不限频就会形成 IDR 风暴：
/// 每个 IDR 都是带宽尖峰，反而让解码队列更满、继续丢包。400 ms 内最多一次，既压住风暴，
/// 又保证等待中的请求会在该窗口内被重试（不会永久卡在等 IDR）。
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

/// 一条待解码的画面载荷（尚未解码）。
enum DecodeJob {
    /// 被控端选定 codec（`CodecSelect`）。解码任务据此换解码器；新 codec 的码流仍从关键帧起步。
    SelectCodec(CodecId),
    /// 一个视频包（长度前缀拼接的帧，codec 由 `CodecSelect` 决定）。
    Packet {
        /// 来源显示器编号。
        display: u32,
        /// 帧序号（被控端递增；用于检测「有包被丢弃」的断层）。
        seq: u64,
        /// 入队时的 `drop_epoch`。小于当前 epoch 的包说明队列曾溢出，已被更新的包取代。
        epoch: u32,
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
    },
}

/// 主循环与解码任务共享的原子读数/状态。
///
/// 都是 `Relaxed` 足够的观测值：写的是每帧热路径，读的是每秒一次或入队前的一次判断，
/// 不承担跨线程的同步顺序语义。打包成一个结构是为了避免 [`decode_loop`] 的参数爆炸。
struct DecodeShared {
    /// 最近一帧的解码耗时（毫秒）。
    decode_ms: Arc<AtomicU32>,
    /// 解码队列溢出代次（主循环 `fetch_add`，解码任务比较）。
    drop_epoch: Arc<AtomicU32>,
    /// 解码任务是否正在等 IDR（主循环据此在入队前丢 P 帧）。
    awaiting_keyframe: Arc<AtomicBool>,
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
}

impl DecodeShared {
    fn new() -> Self {
        Self {
            decode_ms: Arc::new(AtomicU32::new(0)),
            drop_epoch: Arc::new(AtomicU32::new(0)),
            awaiting_keyframe: Arc::new(AtomicBool::new(false)),
            applied_patches: Arc::new(AtomicU32::new(0)),
            clock_offset_us: Arc::new(AtomicI64::new(UNKNOWN_OFFSET)),
            e2e_ms: Arc::new(AtomicU32::new(0)),
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
        return Err(RemoteError::Handshake(format!("期望 HelloAck，实得 {:?}", ack.message)));
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
    let writer_task = tokio::spawn(writer_loop(writer, receivers, cancel.clone()));

    // 解码任务：解码是 CPU 密集的**同步**调用，放在会话 `select!` 的分支里执行会让输入转发
    // 被它占住。故把解码挪到独立任务，循环只把原始载荷投进有界队列，输入/心跳照常得到轮询。
    //
    // 解码任务会占住运行时的**一个** worker 线程（解码期间不同让）。这是 CPU 密集工作的必然代价，
    // 且比原先占住会话循环本身好得多。
    let (decode_tx, decode_rx) = mpsc::channel::<DecodeJob>(DECODE_QUEUE);
    // 解码任务 → 循环的关键帧请求通道（无界：只承载「重置后请对端补一个 IDR」这种低频控制）。
    // 循环自己保留 `keyframe_tx`，故 `recv()` 永不返回 `None`（否则会 100% 空转）。
    // 载荷是 `(显示器, 原因)`：原因只进管理面（进程内 `SessionStats`），**不上线**。
    let (keyframe_tx, mut keyframe_rx) = mpsc::unbounded_channel::<(u32, KeyframeReason)>();
    // 主循环与解码任务共享的原子读数/状态（见 [`DecodeShared`]）：
    // - 解码耗时：由解码任务写、主循环读（填进 `QosFeedback`）；
    // - `drop_epoch`：队列溢出时推进，解码任务据此整体作废陈旧 P 帧；
    // - `awaiting_keyframe`：解码任务在等 IDR 时置位，主循环据此**在入队前**直接丢弃
    //   P 帧——等待期间队列里只留关键帧，IDR 一到就能立刻解码，不会被一堆过期 P 帧
    //   挤掉（打破「丢弃→请求 IDR→IDR 又被丢」的自锁）。
    let decode_shared = Arc::new(DecodeShared::new());
    let decode_task = tokio::spawn(decode_loop(
        decode_rx,
        keyframe_tx.clone(),
        std::sync::Arc::clone(&sink),
        cancel.clone(),
        Arc::clone(&decode_shared),
        config.h264_decoder_factory.clone(),
        config.stats.clone(),
    ));

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
    // 最近一次请求关键帧的时刻（限频，避免静止桌面上刷屏）。
    let mut last_keyframe_request_at: Option<Instant> = None;
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
                for event in input.drain() {
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
                let backlog = u32::try_from(DECODE_QUEUE.saturating_sub(decode_tx.capacity())).unwrap_or(u32::MAX);
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
            // 解码任务请求关键帧（解码失败 / 丢弃了参考帧）：转达给被控端，让它补一个 IDR。
            // `keyframe_tx` 常驻于本函数 → `recv()` 不会返回 `None`，本分支只在真的有请求时触发。
            // 原因只进管理面（进程内上报），线上 `RequestKeyframe` 仍只有 `display` 一个字段。
            Some((display, reason)) = keyframe_rx.recv() => {
                if let Err(err) = queues
                    .enqueue_ctrl(
                        &cancel,
                        SessionFrame::screen(SessionMessage::RequestKeyframe { display }),
                    )
                    .await
                {
                    break Err(err);
                }
                config.stats.report(SessionStats::KeyframeRequest { reason });
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
                            // 第一道闸门（5.6）：解码任务在等 IDR 时，P 帧一律不入队。
                            // 否则队列会被过期 P 帧塞满，IDR 反而被挤掉，形成「丢弃→请求 IDR
                            // →IDR 又被丢」的自锁。关键帧必须放行。
                            if decode_shared.awaiting_keyframe.load(Ordering::Relaxed)
                                && flags & VIDEO_FLAG_KEYFRAME == 0
                            {
                                window_dropped = window_dropped.saturating_add(1);
                                debug!("等待关键帧期间丢弃 P 帧（不让它占用解码队列）");
                                continue;
                            }
                            let epoch = decode_shared.drop_epoch.load(Ordering::Relaxed);
                            match enqueue_decode(&decode_tx, DecodeJob::Packet {
                                display,
                                seq,
                                epoch,
                                codec,
                                flags,
                                width,
                                height,
                                data,
                                capture_ts_us,
                            }) {
                                DecodeEnqueue::Queued => {}
                                // 第二道兜底：队列满时推进 epoch，让队列里的陈旧 P 帧
                                // 出队时被整体作废，解码端直接跳到最新包。
                                DecodeEnqueue::Dropped => {
                                    decode_shared.drop_epoch.fetch_add(1, Ordering::Relaxed);
                                    window_dropped = window_dropped.saturating_add(1);
                                }
                                DecodeEnqueue::Closed => {
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
                                let _ = keyframe_tx.send((monitor, KeyframeReason::DecodeError));
                                continue;
                            }
                            // 补丁走 `try_send`：它绝不能卡住主循环（输入/心跳照常轮询）。
                            match enqueue_decode(
                                &decode_tx,
                                DecodeJob::Region {
                                    display: monitor,
                                    width,
                                    height,
                                    rects,
                                    capture_ts_us,
                                },
                            ) {
                                DecodeEnqueue::Queued => {}
                                DecodeEnqueue::Dropped => {
                                    // 队列满时**不**推进 `drop_epoch`：补丁不是参考链上的帧，
                                    // 直接请一个全帧 IDR 即可——被控端收到后会退出补丁模式并
                                    // 发一整帧，合成帧随之重建。
                                    debug!(monitor, "解码队列已满，丢弃补丁批并请求关键帧");
                                    let _ = keyframe_tx
                                        .send((monitor, KeyframeReason::QueueOverflow));
                                }
                                DecodeEnqueue::Closed => {
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
                                if enqueue_decode(&decode_tx, DecodeJob::SelectCodec(codec))
                                    == DecodeEnqueue::Closed
                                {
                                    break Err(RemoteError::Platform(
                                        "画面解码任务已结束".to_owned(),
                                    ));
                                }
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
    drop(decode_tx);
    let mut decode_task = decode_task;
    let _ = timeout(BYE_TIMEOUT, &mut decode_task).await;
    decode_task.abort();
    sink.on_closed(reason);
    result
}

/// 画面解码任务：从有界队列取原始载荷，串行解码后交给 [`FrameSink`]。
///
/// 独立成任务是为了把 CPU 密集的解码移出会话 `select!` 循环（见 `run_controller` 的说明）。
/// 取消一旦触发即退出；队列关闭（会话结束）时把已入队的载荷解完再退出，保证顺序不乱。
///
/// # 丢弃与恢复（**改动前先读这段**）
///
/// 控制器**可以**丢帧（解码跟不上时不可避免），但 VP9 是时域编码：丢掉一个 P 帧会断掉参考链，
/// 后续引用它的帧在解码端永远解不出来。故本任务维护「参考链是否完好」这一状态：
///
/// - **序号断层**（`seq` 不等于预期）说明会话循环因队列满丢弃过包 → 重置解码器、进入
///   `awaiting_keyframe`，并请求对端补一个 IDR。
/// - **解码失败**（数据损坏/缺参考帧）同上。
/// - **队列溢出代次**（`epoch < drop_epoch`）说明主循环在 `try_send` 失败时已经推进了代次：
///   队列里排着的都是过期 P 帧，直接丢弃并等 IDR，避免「先解一堆陈旧帧再重置」。
/// - `awaiting_keyframe` 期间**丢弃所有非关键帧**，只等 IDR；收到 IDR 后恢复。主循环也通过
///   共享的 `awaiting_keyframe` 原子做**第一道闸门**：入队前就丢掉 P 帧，保证队列里只留
///   关键帧，IDR 不会被过期 P 帧挤掉。
///
/// 被控端会周期性发关键帧作兜底，但**不能只靠它**——请求关键帧是丢帧后唯一的快速恢复路径。
/// 所有请求都走 [`request_keyframe`] 的 400 ms 限频：断层/溢出风暴时最多每 400 ms 一个 IDR；
/// 同时挂起的请求由 `kf_tick` 重试，不会永久卡在等 IDR。
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

/// 发送一个关键帧请求，按 [`KEYFRAME_REQUEST_INTERVAL`] 限频。
///
/// 距上次请求已满一个窗口就立即发出并记录时刻；否则只把显示器记进 `pending`，由
/// [`decode_loop`] 的 400 ms tick 补发。这样断层/溢出风暴里 400 ms 内最多一个 IDR，
/// 又不会让请求永久丢失（tick 保证 400 ms 内必定重试）。
fn request_keyframe(
    tx: &mpsc::UnboundedSender<(u32, KeyframeReason)>,
    last: &mut Option<Instant>,
    pending: &mut Option<(u32, KeyframeReason)>,
    monitor: u32,
    reason: KeyframeReason,
) {
    let now = Instant::now();
    if last.is_none_or(|sent| now.duration_since(sent) >= KEYFRAME_REQUEST_INTERVAL) {
        let _ = tx.send((monitor, reason));
        *last = Some(now);
        *pending = None;
    } else {
        *pending = Some((monitor, reason));
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "序号断层/关键帧等待/解码器重建/切 codec 是一条不可分割的状态机，拆开反而要来回传状态"
)]
async fn decode_loop(
    mut rx: mpsc::Receiver<DecodeJob>,
    keyframe_tx: mpsc::UnboundedSender<(u32, KeyframeReason)>,
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
    // 参考链已断，只等 IDR。
    let mut awaiting_keyframe = false;
    // 下一个期望的帧序号（被控端按发送顺序递增；断层 = 中间有包被丢弃）。
    let mut expected_seq: Option<u64> = None;
    // 关键帧请求限频状态：上次真正发出的时刻、被限频挂起的显示器。
    let mut last_kf_request: Option<Instant> = None;
    let mut kf_pending: Option<(u32, KeyframeReason)> = None;
    // 定期检查挂起的请求：距上次请求已满一个窗口就发出去，保证不会永久等 IDR。
    let mut kf_tick = tokio::time::interval_at(
        Instant::now() + KEYFRAME_REQUEST_INTERVAL,
        KEYFRAME_REQUEST_INTERVAL,
    );
    kf_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let job = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            _ = kf_tick.tick() => {
                if awaiting_keyframe {
                    if let Some((monitor, reason)) = kf_pending.take() {
                        let _ = keyframe_tx.send((monitor, reason));
                        last_kf_request = Some(Instant::now());
                    }
                } else {
                    // 已不在等 IDR（例如收到了关键帧）：挂起的请求作废。
                    kf_pending = None;
                }
                continue;
            }
            job = rx.recv() => match job {
                Some(job) => job,
                None => break,
            },
        };
        // `display` 会与 tracing 宏展开里引入的 `field::display` 撞名，故绑定名用 monitor。
        let (monitor, seq, epoch, packet_codec, flags, width, height, data, capture_ts_us) = match job {
            DecodeJob::SelectCodec(selected) => {
                // 换 codec：旧解码器的参考帧不可用，重置并等新 codec 的关键帧。
                if codec != Some(selected) {
                    debug!(?selected, "切换解码器 codec");
                    codec = Some(selected);
                    decoder = None;
                    // **只建解码器、不 configure**：尺寸由首个 `VideoPacket` 给出。
                    configured_dims = None;
                    awaiting_keyframe = true;
                    decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
                    expected_seq = None;
                    if let Some(factory) =
                        decoder_factory_for(selected, h264_decoder_factory.as_ref())
                    {
                        match factory.create() {
                            Ok(dec) => {
                                report_decoder_info(&stats, &mut last_decoder_info, dec.as_ref());
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
            } => {
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
                    request_keyframe(
                        &keyframe_tx,
                        &mut last_kf_request,
                        &mut kf_pending,
                        monitor,
                        KeyframeReason::DecodeError,
                    );
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
                    decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
                    request_keyframe(
                        &keyframe_tx,
                        &mut last_kf_request,
                        &mut kf_pending,
                        monitor,
                        KeyframeReason::DecodeError,
                    );
                    continue;
                }
                if awaiting_keyframe {
                    // 还没有可应用的全帧基底；关键帧请求已经在途（见上）。
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
                    debug!(monitor, width, height, "补丁矩形非法或解压失败，丢弃整批并请求关键帧");
                    awaiting_keyframe = true;
                    decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
                    request_keyframe(
                        &keyframe_tx,
                        &mut last_kf_request,
                        &mut kf_pending,
                        monitor,
                        KeyframeReason::DecodeError,
                    );
                    continue;
                }
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
                decode_shared
                    .e2e_ms
                    .store(elapsed_ms(now_us(), offset, capture_ts_us), Ordering::Relaxed);
                continue;
            }
            DecodeJob::Packet {
                display,
                seq,
                epoch,
                codec,
                flags,
                width,
                height,
                data,
                capture_ts_us,
            } => (display, seq, epoch, codec, flags, width, height, data, capture_ts_us),
        };
        // 队列曾溢出（主循环推进了 epoch）：本包已被更新的包取代。丢掉它并重置参考链，
        // `expected_seq = None` 避免紧接着的下一个包再触发一次「断层」重置。
        if epoch < decode_shared.drop_epoch.load(Ordering::Relaxed) {
            debug!(seq, epoch, "解码队列曾溢出，丢弃陈旧视频包并等待关键帧");
            decoder = None;
            configured_dims = None;
            awaiting_keyframe = true;
            decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
            expected_seq = None;
            request_keyframe(
                &keyframe_tx,
                &mut last_kf_request,
                &mut kf_pending,
                monitor,
                KeyframeReason::QueueOverflow,
            );
            continue;
        }
        // 兜底：正常情况下 `SelectCodec` 一定先到；万一对端漏发，用视频包自带的 codec。
        if codec.is_none() {
            codec = Some(packet_codec);
            decoder = None;
            configured_dims = None;
            awaiting_keyframe = true;
            decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
        }

        // 声明尺寸的纵深防御：真正的解码尺寸由码流决定（下方解码后还会再校验一次），但先挡掉
        // 明显非法的声明，避免把畸形值传下去。
        if width == 0 || height == 0 || width > MAX_FRAME_DIM || height > MAX_FRAME_DIM {
            warn!(monitor, width, height, "视频包声明的编码尺寸非法，丢弃");
            continue;
        }

        let keyframe = flags & VIDEO_FLAG_KEYFRAME != 0;
        // 序号断层 = 会话循环丢弃过包 → 参考链已断，重置解码器并等关键帧。
        if expected_seq.is_some_and(|expected| seq != expected) {
            debug!(seq, ?expected_seq, "视频序号断层，重置解码器并等待关键帧");
            decoder = None;
            configured_dims = None;
            awaiting_keyframe = true;
            decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
            request_keyframe(
                &keyframe_tx,
                &mut last_kf_request,
                &mut kf_pending,
                monitor,
                KeyframeReason::SeqGap,
            );
        }
        expected_seq = Some(seq.wrapping_add(1));

        if awaiting_keyframe && !keyframe {
            continue;
        }
        // 尚无解码器时必须从关键帧起步，否则 P 帧无从参考。
        if decoder.is_none() && !keyframe {
            awaiting_keyframe = true;
            decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
            request_keyframe(
                &keyframe_tx,
                &mut last_kf_request,
                &mut kf_pending,
                monitor,
                KeyframeReason::NoDecoder,
            );
            continue;
        }
        if decoder.is_none() {
            let factory = codec
                .and_then(|codec| decoder_factory_for(codec, h264_decoder_factory.as_ref()));
            let Some(factory) = factory else {
                warn!(?codec, "没有对应 codec 的解码器工厂，丢弃本帧并等待关键帧");
                awaiting_keyframe = true;
                decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
                request_keyframe(
                    &keyframe_tx,
                    &mut last_kf_request,
                    &mut kf_pending,
                    monitor,
                    KeyframeReason::NoDecoder,
                );
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
                    decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
                    request_keyframe(
                        &keyframe_tx,
                        &mut last_kf_request,
                        &mut kf_pending,
                        monitor,
                        KeyframeReason::ConfigureFailed,
                    );
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
        let blocking_result = tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let result = dec.decode(&data);
            (dec, result, started.elapsed())
        })
        .await;
        match blocking_result {
            Ok((dec, Ok(Some(frame)), elapsed)) => {
                decoder = Some(dec);
                let decode_ms = u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX);
                decode_shared.decode_ms.store(decode_ms, Ordering::Relaxed);
                awaiting_keyframe = false;
                decode_shared.awaiting_keyframe.store(false, Ordering::Relaxed);
                // IDR 已解出：被限频挂起的请求没有意义了。
                kf_pending = None;
                // 端到端（D2）：采集完成 → 本端解码完成。偏差未知/时间戳缺失时给 0
                // （见 [`crate::clock::elapsed_ms`]）。
                let offset = decode_shared.clock_offset_us.load(Ordering::Relaxed);
                decode_shared
                    .e2e_ms
                    .store(elapsed_ms(now_us(), offset, capture_ts_us), Ordering::Relaxed);
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
                decode_shared.awaiting_keyframe.store(false, Ordering::Relaxed);
                kf_pending = None;
            }
            Ok((dec, Err(err), _elapsed)) => {
                // 解码失败：这个实例不可信，丢弃并等下一个关键帧（`dec` 在分支结束时 drop）。
                drop(dec);
                warn!(error = %err, "画面解码失败，重置解码器并请求关键帧");
                decoder = None;
                configured_dims = None;
                awaiting_keyframe = true;
                decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
                request_keyframe(
                    &keyframe_tx,
                    &mut last_kf_request,
                    &mut kf_pending,
                    monitor,
                    KeyframeReason::DecodeError,
                );
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
                decode_shared.awaiting_keyframe.store(true, Ordering::Relaxed);
                request_keyframe(
                    &keyframe_tx,
                    &mut last_kf_request,
                    &mut kf_pending,
                    monitor,
                    KeyframeReason::DecodeError,
                );
            }
        }
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

/// [`enqueue_decode`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeEnqueue {
    /// 已入队。
    Queued,
    /// 队列满，该包被丢弃。调用方应推进 `drop_epoch`，让队列里的陈旧包整体作废。
    Dropped,
    /// 解码任务已结束。
    Closed,
}

/// 投递一条待解码载荷。
///
/// 队列满时**丢弃**该批而不是 `await`：在 `select!` 分支里等解码腾出位置会重新把循环卡住（正是
/// 本次要修的问题）。丢包会打断 VP9 的参考链，故解码任务用**序号断层**检测到它、重置解码器并
/// 请求关键帧（见 [`decode_loop`]）——丢帧本身安全，静默花屏才不安全。
///
/// 主循环还会在 `Dropped` 时推进 `drop_epoch`：队列里排着的 P 帧都是过期画面，解码端应直接
/// 跳到最新包，而不是先把它们解完再重置。
fn enqueue_decode(tx: &mpsc::Sender<DecodeJob>, job: DecodeJob) -> DecodeEnqueue {
    match tx.try_send(job) {
        Ok(()) => DecodeEnqueue::Queued,
        Err(mpsc::error::TrySendError::Full(_)) => {
            debug!("解码队列已满，丢弃一批画面（解码侧将按序号断层请求关键帧）");
            DecodeEnqueue::Dropped
        }
        Err(mpsc::error::TrySendError::Closed(_)) => DecodeEnqueue::Closed,
    }
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

        let shared = responder.diffie_hellman(&initiator_pub).expect("ECDH 应成功");
        // 本端扮演被控端（响应方）：`initiator=false`。
        let keys = derive_session_keys(token, &shared, session_id, &initiator_pub, &responder_pub, false);
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
        let (_peer_reader, _peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

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
        let (_peer_reader, _peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

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
        let (peer_reader, _peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;
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
        assert!(result.is_ok(), "命令通道关闭应按正常结束处理，实得 {result:?}");
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
        let (mut peer_reader, mut peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

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
        assert!(rtt < Duration::from_millis(300), "内存管道 RTT 不该是 {rtt:?}");

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
        let (mut peer_reader, _peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

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
            let (mut peer_reader, _peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

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
            assert_eq!(end.reason, "用户中断", "第 {round} 轮：取消应压过命令通道关闭");
            assert!(end.deliberate, "第 {round} 轮：用户主动中断必须是 deliberate，否则会触发自动重连");
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

        fn decode(&mut self, _data: &[u8]) -> RemoteResult<Option<crate::video::DecodedVideoFrame>> {
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
        let (job_tx, job_rx) = mpsc::channel::<DecodeJob>(4);
        let (keyframe_tx, _keyframe_rx) = mpsc::unbounded_channel::<(u32, KeyframeReason)>();
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
            job_rx,
            keyframe_tx,
            sink.clone(),
            cancel.clone(),
            Arc::new(DecodeShared::new()),
            Some(factory),
            SessionStatsReporter::noop(),
        ));

        job_tx
            .send(DecodeJob::SelectCodec(CodecId::H264))
            .await
            .expect("投递 codec 选择");
        job_tx
            .send(DecodeJob::Packet {
                display: 0,
                seq: 1,
                epoch: 0,
                codec: CodecId::H264,
                flags: VIDEO_FLAG_KEYFRAME,
                width: 2,
                height: 2,
                data: vec![1, 2, 3],
                capture_ts_us: 0,
            })
            .await
            .expect("投递关键帧");

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
            &[
                DecoderCall::Configure(2, 2),
                DecoderCall::Decode
            ],
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
        let (job_tx, job_rx) = mpsc::channel::<DecodeJob>(8);
        let (keyframe_tx, _keyframe_rx) = mpsc::unbounded_channel::<(u32, KeyframeReason)>();
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
            job_rx,
            keyframe_tx,
            sink.clone(),
            cancel.clone(),
            Arc::new(DecodeShared::new()),
            Some(factory),
            SessionStatsReporter::noop(),
        ));

        job_tx
            .send(DecodeJob::SelectCodec(CodecId::H264))
            .await
            .expect("投递 codec 选择");

        let packet = |seq: u64, flags: u8, width: u32, height: u32| DecodeJob::Packet {
            display: 0,
            seq,
            epoch: 0,
            codec: CodecId::H264,
            flags,
            width,
            height,
            data: vec![1, 2, 3],
            capture_ts_us: 0,
        };
        // 首包（关键帧）→ configure + decode。
        job_tx
            .send(packet(1, VIDEO_FLAG_KEYFRAME, 64, 48))
            .await
            .expect("投递首包");
        // 同尺寸 → 只 decode，不重复 configure。
        job_tx
            .send(packet(2, 0, 64, 48))
            .await
            .expect("投递同尺寸包");
        // 尺寸变化（关键帧）→ 再次 configure + decode。
        job_tx
            .send(packet(3, VIDEO_FLAG_KEYFRAME, 96, 64))
            .await
            .expect("投递改尺寸包");

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

    /// 解码任务测试装置：H.264 假工厂 + 记录型 sink + 关键帧请求接收端。
    struct DecodeHarness {
        jobs: mpsc::Sender<DecodeJob>,
        keyframes: mpsc::UnboundedReceiver<(u32, KeyframeReason)>,
        sink: Arc<PatchSink>,
        cancel: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    impl DecodeHarness {
        fn spawn() -> Self {
            let (jobs, job_rx) = mpsc::channel::<DecodeJob>(8);
            let (keyframe_tx, keyframes) = mpsc::unbounded_channel::<(u32, KeyframeReason)>();
            let sink = Arc::new(PatchSink::default());
            let cancel = CancellationToken::new();
            let factory: Arc<dyn VideoDecoderFactory> = Arc::new(RecordingDecoderFactory {
                codec: CodecId::H264,
                created: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                calls: Arc::new(Mutex::new(Vec::new())),
            });
            let task = tokio::spawn(decode_loop(
                job_rx,
                keyframe_tx,
                sink.clone(),
                cancel.clone(),
                Arc::new(DecodeShared::new()),
                Some(factory),
                SessionStatsReporter::noop(),
            ));
            Self {
                jobs,
                keyframes,
                sink,
                cancel,
                task,
            }
        }

        /// 走完「选 codec → 首帧关键帧」，让解码任务进入可应用补丁的状态。
        async fn prime(&self, width: u32, height: u32) {
            self.jobs
                .send(DecodeJob::SelectCodec(CodecId::H264))
                .await
                .expect("投递 codec 选择");
            self.jobs
                .send(DecodeJob::Packet {
                    display: 0,
                    seq: 1,
                    epoch: 0,
                    codec: CodecId::H264,
                    flags: VIDEO_FLAG_KEYFRAME,
                    width,
                    height,
                    data: vec![1, 2, 3],
                    capture_ts_us: 0,
                })
                .await
                .expect("投递关键帧");
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

        async fn region(&self, width: u32, height: u32, rects: Vec<RegionRect>) {
            self.jobs
                .send(DecodeJob::Region {
                    display: 0,
                    width,
                    height,
                    rects,
                    capture_ts_us: 0,
                })
                .await
                .expect("投递补丁批");
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

        /// 等解码任务请求关键帧；返回 `(显示器, 原因)`。
        async fn expect_keyframe(&mut self) -> (u32, KeyframeReason) {
            tokio::time::timeout(WAIT, self.keyframes.recv())
                .await
                .expect("补丁被丢弃时必须请求关键帧")
                .expect("关键帧请求通道不应关闭")
        }
    }

    impl Drop for DecodeHarness {
        fn drop(&mut self) {
            self.cancel.cancel();
            self.task.abort();
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
            )
            .await;

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
        assert!(
            harness.keyframes.try_recv().is_err(),
            "合法补丁不该请求关键帧"
        );
    }

    /// **基底不存在**：首个全帧之前到达的补丁没有可应用的目标，必须丢弃并请求关键帧。
    #[tokio::test]
    async fn region_update_before_any_video_packet_drops_and_requests_keyframe() {
        let mut harness = DecodeHarness::spawn();
        // 只选 codec、不投视频包：`configured_dims` 仍为空。
        harness
            .jobs
            .send(DecodeJob::SelectCodec(CodecId::H264))
            .await
            .expect("投递 codec 选择");
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
            )
            .await;

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
        harness.region(8, 8, vec![rect]).await;
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
            )
            .await;
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
}
