//! 控制器侧会话循环：握手 → 收画面 → 回传输入 → 心跳。
//!
//! 传输建立阶段已把 `Hello` 帧与中继的 `RequestRelay` 帧合并写出（见 [`crate::transport`]），
//! 因此这里**只等 `HelloAck`**。收到后派生会话密钥、装上 [`crate::crypto::Cipher`]，
//! 之后的帧全部加密。

use crate::auth::verify_ack_mac;
use crate::crypto::{derive_session_keys, Cipher};
use crate::error::{RemoteError, RemoteResult};
use crate::screen::{DecodedFrame, FrameSink, InputEvent, QualityPreset};
use crate::session::{
    trusted_rtt, ControllerHandshake, SessionCodec, SessionEnd, SessionFrame, SessionMessage,
    HEARTBEAT_INTERVAL, VIDEO_FLAG_KEYFRAME,
};
use crate::stats::{SessionStats, SessionStatsReporter};
use crate::transfer::{TransferCommand, TransferHub, CHUNK_PAUSE};
use crate::transport::BoxDuplex;
use crate::video::{VideoDecoder, MAX_FRAME_DIM};
use futures_util::{SinkExt as _, StreamExt as _};
use rust_tunnel_common::protocol::RemoteMode;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// 等待 `HelloAck` 的超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 收尾 `Bye` 的发送上限。对端可能已停读（写缓冲满），不加限时会让 teardown 永久卡住、`on_closed`
/// 永不投递；`Bye` 只是尽力通知，超时丢弃即可。
const BYE_TIMEOUT: Duration = Duration::from_secs(2);

/// 待解码载荷队列深度。
///
/// 解码在独立任务里串行进行（1080p 单帧约十几毫秒）。队列太深会在解码跟不上时堆积内存，太浅
/// 则正常抖动就丢帧；8 兼作**抖动缓冲**，把最坏内存压在「8 × 单帧载荷」量级。队列满即丢帧——
/// 丢弃会打断 VP9 的参考链，故解码任务用**序号断层**检测到它并请求关键帧（见 [`decode_loop`]）。
const DECODE_QUEUE: usize = 8;

/// QoS 反馈间隔（秒级）。每秒向被控端发一条 `QosFeedback`（含 RTT / 收码率 / 解码耗时 / 积压），
/// 驱动被控端的 AIMD 闭环（见 [`crate::qos`]）。同时按同一节拍把解码侧读数经进程内
/// [`SessionStatsReporter`] 送到 UI。
const QOS_INTERVAL: Duration = Duration::from_secs(1);

/// 「长时间无帧」的判据（超过它即请求关键帧并告警）。
///
/// 静止桌面**合法地**不产帧（被控端 memcmp 去重后完全不喂编码器），所以这不是错误检测，而是
/// **卡死检测**：对端采集/编码挂住、或中继半死时，画面会永久冻结且毫无报错。取 3s——足够区分
/// 「静止」与「卡死」，代价是被控端在静止桌面上每 3s 补一个 IDR（1080p 静止帧 CBR 下仅 ~97 B，
/// 可忽略）。请求本身也按同一窗口限频，避免刷屏。
const NO_FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// 一条待解码的画面载荷（尚未 VP9 解码）。
enum DecodeJob {
    /// 一个视频包（长度前缀拼接的 VP9 帧）。
    Packet {
        /// 来源显示器编号。
        display: u32,
        /// 帧序号（被控端递增；用于检测「有包被丢弃」的断层）。
        seq: u64,
        /// 标志位（[`VIDEO_FLAG_KEYFRAME`] 等）。
        flags: u8,
        /// 声明的**编码**宽度。
        width: u32,
        /// 声明的**编码**高度。
        height: u32,
        /// VP9 帧数据。
        data: Vec<u8>,
    },
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
    /// 把本机剪贴板文本发给被控端。
    Clipboard(String),
    /// 请求被控端把它的剪贴板发过来。
    ClipboardRequest,
    /// 文件传输操作。
    File(TransferCommand),
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
}

/// 跑一次控制器会话，直到取消、对端关闭或出错。
///
/// `cmd_rx` 是 UI 经 [`crate::manager::RemoteManager`] 投递的命令流；`sink` 接收已解码画面
/// （[`FrameSink::on_frame`] 拿到的是 RGBA，不在 UI 线程解码）。
///
/// # Errors
/// 握手被拒、编解码失败、IO 失败时返回 [`RemoteError`]；正常结束（收到 `Bye` / 对端关闭 /
/// 取消）返回 `Ok(`[`SessionEnd`]`)`，由调用方据此判定是否需要自动重连。
#[expect(
    clippy::too_many_lines,
    reason = "握手校验 + 画面/输入/剪贴板/文件/心跳的单会话循环，拆开反而丢失控制流全貌"
)]
pub async fn run_controller(
    duplex: BoxDuplex,
    config: ControllerConfig,
    handshake: ControllerHandshake,
    mut cmd_rx: mpsc::Receiver<ControllerCommand>,
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

    // 解码任务：VP9 解码是 CPU 密集的**同步**调用，放在会话 `select!` 的分支里执行会让输入转发
    // 被它占住。故把解码挪到独立任务，循环只把原始载荷投进有界队列，输入/心跳照常得到轮询。
    //
    // 解码任务会占住运行时的**一个** worker 线程（解码期间不同让）。这是 CPU 密集工作的必然代价，
    // 且比原先占住会话循环本身好得多。
    let (decode_tx, decode_rx) = mpsc::channel::<DecodeJob>(DECODE_QUEUE);
    // 解码任务 → 循环的关键帧请求通道（无界：只承载「重置后请对端补一个 IDR」这种低频控制）。
    // 循环自己保留 `keyframe_tx`，故 `recv()` 永不返回 `None`（否则会 100% 空转）。
    let (keyframe_tx, mut keyframe_rx) = mpsc::unbounded_channel::<u32>();
    // 最近一帧的解码耗时（毫秒），由解码任务写、主循环读（填进 `QosFeedback`）。原子而非锁：
    // 写的是热路径（每帧一次），读的是每秒一次，两者几乎不重叠，`Relaxed` 足够。
    let decode_ms_shared = Arc::new(AtomicU32::new(0));
    let decode_task = tokio::spawn(decode_loop(
        decode_rx,
        keyframe_tx.clone(),
        std::sync::Arc::clone(&sink),
        cancel.clone(),
        Arc::clone(&decode_ms_shared),
    ));

    // 3) 主循环。
    // 起点取 `now` 而非 `now + HEARTBEAT_INTERVAL`：**首个 tick 因此立即到期**（起点已过去），
    // 会话一建立就发一次心跳，用户开窗即有读数，不必空等一个 [`HEARTBEAT_INTERVAL`]。
    let mut heartbeat = interval_at(Instant::now(), HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 出站文件块：只在有在途传输时启用（`has_outgoing` 为假时该分支不被轮询，不产生空转）。
    let mut file_tick = interval_at(Instant::now(), CHUNK_PAUSE);
    file_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // QoS 反馈节拍：每秒一条 `QosFeedback`（驱动被控端 AIMD）+ 一次解码侧读数上报。
    // 起点取 `now + QOS_INTERVAL`：第一个样本要有一整秒的统计窗口才有意义。
    let mut qos_tick = interval_at(Instant::now() + QOS_INTERVAL, QOS_INTERVAL);
    qos_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // RTT 样本：发 `Heartbeat` 时记时刻，收到 `HeartbeatAck` 时配对算 RTT。**复用既有心跳往返**，
    // 不新增数据面变体（新增 `SessionMessage` 变体会让混合版本的一端直接断链，见 `HEARTBEAT_INTERVAL`）。
    // 首个样本由上面那个「立即到期的 tick」给出，故这里先置空。
    let mut pending_heartbeat: Option<Instant> = None;
    // 最近一次可信 RTT（毫秒）：填进 `QosFeedback`。无样本时为 0（闭环此时按「未知」处理，
    // 不会误判为健康——`0 < fast_threshold` 但队列非空时仍可能判拥塞）。
    let mut last_rtt_ms: Option<u32> = None;
    // QoS 窗口内收到的画面字节数（算收码率）。
    let mut recv_bytes: u64 = 0;
    let mut qos_window_start = Instant::now();
    // 最近一次收到画面帧的时刻（「长时间无帧」看门狗用）。
    let mut last_frame_at = Instant::now();
    // 最近一次请求关键帧的时刻（限频，避免静止桌面上刷屏）。
    let mut last_keyframe_request_at: Option<Instant> = None;
    // 自最近一次收到画面帧以来已发出的关键帧请求数：0 = 这次是首次请求（静止桌面的常规情况，
    // 只记 `debug`），≥1 = 上次请求没换回帧（对端可能真挂住，升到 `warn`）。收到帧即清零。
    let mut stall_requests: u32 = 0;
    // 当前观看的显示器：请求关键帧要指名道姓（被控端只认自己正在采集的那台）。
    // 初值与 UI 的选择器一致：优先主显示器，否则第一台。
    let mut selected_display = displays
        .iter()
        .find(|d| d.primary)
        .or_else(|| displays.first())
        .map_or(0, |d| d.id);
    let mut reason = String::from("会话结束");

    let result: RemoteResult<SessionEnd> = loop {
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
            _ = heartbeat.tick() => {
                // 覆盖旧样本：上一个 ack 迟迟不来时，下一次心跳的往返才是与当前时刻相关的读数。
                pending_heartbeat = Some(Instant::now());
                if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::Heartbeat)).await {
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
                let decode_ms = decode_ms_shared.load(Ordering::Relaxed);
                let backlog = u32::try_from(DECODE_QUEUE.saturating_sub(decode_tx.capacity())).unwrap_or(u32::MAX);
                let feedback = SessionMessage::QosFeedback {
                    rtt_ms: last_rtt_ms.unwrap_or(0),
                    recv_kbps,
                    decode_ms,
                    backlog,
                };
                if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::screen(feedback)).await {
                    break Err(err);
                }
                // 同节拍把解码侧读数经进程内通道送到 UI（不落线）。
                config.stats.report(SessionStats::Decode { decode_ms, recv_kbps });
                recv_bytes = 0;
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
                        if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::screen(request)).await {
                            break Err(err);
                        }
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
                        if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::Input(event))).await {
                            break Err(err);
                        }
                    }
                    ControllerCommand::SelectDisplay(display) => {
                        if let Err(err) =
                            send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::SelectDisplay { display })).await
                        {
                            break Err(err);
                        }
                        // 记住当前观看的显示器：`RequestKeyframe` 要指名道姓（被控端只认自己正在
                        // 采集的那台）。
                        selected_display = display;
                    }
                    ControllerCommand::SetQuality(preset) => {
                        if let Err(err) =
                            send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::SetQuality { preset })).await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::Clipboard(text) => {
                        if let Err(err) =
                            send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::Clipboard { text })).await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::ClipboardRequest => {
                        if let Err(err) =
                            send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::ClipboardRequest)).await
                        {
                            break Err(err);
                        }
                    }
                    ControllerCommand::File(cmd) => match transfer.on_command(cmd).await {
                        Ok(Some(msg)) => {
                            if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::file(msg)).await {
                                break Err(err);
                            }
                        }
                        Ok(None) => {}
                        Err(err) => warn!(error = %err, "文件操作发起失败"),
                    },
                    ControllerCommand::Ping => {
                        // 已有在途样本时不打扰：否则会覆盖它、让那次往返的读数作废。
                        if pending_heartbeat.is_none() {
                            pending_heartbeat = Some(Instant::now());
                            if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::Heartbeat)).await {
                                break Err(err);
                            }
                        }
                    }
                }
            }
            // 解码任务请求关键帧（解码失败 / 丢弃了参考帧）：转达给被控端，让它补一个 IDR。
            // `keyframe_tx` 常驻于本函数 → `recv()` 不会返回 `None`，本分支只在真的有请求时触发。
            Some(display) = keyframe_rx.recv() => {
                if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::screen(SessionMessage::RequestKeyframe { display })).await {
                    break Err(err);
                }
            }
            _ = file_tick.tick(), if transfer.has_outgoing() => {
                match transfer.next_outbound().await {
                    Ok(Some(msg)) => {
                        if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::file(msg)).await {
                            break Err(err);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(error = %err, "文件读取失败，中断该次传输");
                        // 回发 `FileError`：发送方不再有 chunk 流动，对端的 `Incoming` 会一直挂着。
                        if let Err(send_err) = send_file_errors(&mut writer, &cancel, transfer.abort_all(&err.to_string())).await {
                            break Err(send_err);
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
                            flags,
                            width,
                            height,
                            data,
                            ..
                        } => {
                            // 只投递原始载荷；解码在独立任务里做（见 `decode_loop`）。
                            // 同时记收码率窗口与「最近收帧时刻」（QoS 反馈 / 无帧看门狗用）。
                            recv_bytes = recv_bytes.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                            last_frame_at = Instant::now();
                            // 帧到了：看门狗若是「首次请求」路径，计数回到 0，下次仍只记 `debug`。
                            stall_requests = 0;
                            if !enqueue_decode(&decode_tx, DecodeJob::Packet {
                                display,
                                seq,
                                flags,
                                width,
                                height,
                                data,
                            }) {
                                break Err(RemoteError::Platform("画面解码任务已结束".to_owned()));
                            }
                        }
                        // `display` 会与 tracing 宏展开里引入的 `field::display` 撞名，故绑定名用 monitor。
                        SessionMessage::ScreenConfig { display: monitor, codec, width, height } => {
                            // 编码尺寸/codec 的通告。解码器本身能在关键帧处跟随分辨率变化
                            // （由 `video` 的单测锁定），故这里只记录、不重建。
                            debug!(monitor, ?codec, width, height, "收到屏幕配置");
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
                        SessionMessage::Heartbeat => {
                            if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::HeartbeatAck)).await {
                                break Err(err);
                            }
                        }
                        SessionMessage::HeartbeatAck => {
                            if let Some(sent) = pending_heartbeat.take() {
                                if let Some(rtt) = trusted_rtt(sent, Instant::now()) {
                                    // 存一份毫秒读数：`QosFeedback` 要用它填 `rtt_ms`。
                                    last_rtt_ms = Some(u32::try_from(rtt.as_millis()).unwrap_or(u32::MAX));
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
                                    let mut write_err = None;
                                    for reply in replies {
                                        if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::file(reply)).await {
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
                                    if let Err(send_err) = send_file_errors(&mut writer, &cancel, transfer.abort_all(&err.to_string())).await {
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
    let _ = transfer.abort_all(&reason);
    let _ = timeout(
        BYE_TIMEOUT,
        writer.send(SessionFrame::control(SessionMessage::Bye { reason: reason.clone() })),
    )
    .await;
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
/// - `awaiting_keyframe` 期间**丢弃所有非关键帧**，只等 IDR；收到 IDR 后恢复。
///
/// 被控端会周期性发关键帧作兜底，但**不能只靠它**——请求关键帧是丢帧后唯一的快速恢复路径。
///
/// # 解码耗时（阶段 3）
///
/// 每次 `decode` 的耗时（含 I444→RGBA 转换）写进 `decode_ms_shared`，由主循环每秒读一次填进
/// `QosFeedback`。它是被控端「降编码分辨率」的**唯一**依据（4K 解码实测 37–58 ms 是瓶颈，
/// 编码仅 7.9/7.7 ms 不是）。
async fn decode_loop(
    mut rx: mpsc::Receiver<DecodeJob>,
    keyframe_tx: mpsc::UnboundedSender<u32>,
    sink: std::sync::Arc<dyn FrameSink>,
    cancel: CancellationToken,
    decode_ms_shared: Arc<AtomicU32>,
) {
    let mut decoder: Option<VideoDecoder> = None;
    // 参考链已断，只等 IDR。
    let mut awaiting_keyframe = false;
    // 下一个期望的帧序号（被控端按发送顺序递增；断层 = 中间有包被丢弃）。
    let mut expected_seq: Option<u64> = None;
    loop {
        let job = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            job = rx.recv() => match job {
                Some(job) => job,
                None => break,
            },
        };
        // `display` 会与 tracing 宏展开里引入的 `field::display` 撞名，故绑定名用 monitor。
        let DecodeJob::Packet {
            display: monitor,
            seq,
            flags,
            width,
            height,
            data,
        } = job;

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
            awaiting_keyframe = true;
            let _ = keyframe_tx.send(monitor);
        }
        expected_seq = Some(seq.wrapping_add(1));

        if awaiting_keyframe && !keyframe {
            continue;
        }
        // 尚无解码器时必须从关键帧起步，否则 P 帧无从参考。
        if decoder.is_none() && !keyframe {
            awaiting_keyframe = true;
            let _ = keyframe_tx.send(monitor);
            continue;
        }
        if decoder.is_none() {
            match VideoDecoder::new() {
                Ok(dec) => decoder = Some(dec),
                Err(err) => {
                    warn!(error = %err, "解码器创建失败");
                    continue;
                }
            }
        }
        let Some(dec) = decoder.as_mut() else {
            continue;
        };
        let started = Instant::now();
        match dec.decode(&data) {
            Ok(Some(frame)) => {
                let decode_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
                decode_ms_shared.store(decode_ms, Ordering::Relaxed);
                awaiting_keyframe = false;
                sink.on_frame(DecodedFrame {
                    display: monitor,
                    width: frame.width,
                    height: frame.height,
                    seq,
                    rgba: frame.rgba,
                });
            }
            // 本批无输出（理论路径；`lag_in_frames = 1` 下每次都有）。
            Ok(None) => awaiting_keyframe = false,
            Err(err) => {
                warn!(error = %err, "画面解码失败，重置解码器并请求关键帧");
                decoder = None;
                awaiting_keyframe = true;
                let _ = keyframe_tx.send(monitor);
            }
        }
    }
}

/// 投递一条待解码载荷；返回 `false` 表示解码任务已结束。
///
/// 队列满时**丢弃**该批而不是 `await`：在 `select!` 分支里等解码腾出位置会重新把循环卡住（正是
/// 本次要修的问题）。丢包会打断 VP9 的参考链，故解码任务用**序号断层**检测到它、重置解码器并
/// 请求关键帧（见 [`decode_loop`]）——丢帧本身安全，静默花屏才不安全。
fn enqueue_decode(tx: &mpsc::Sender<DecodeJob>, job: DecodeJob) -> bool {
    match tx.try_send(job) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            debug!("解码队列已满，丢弃一批画面（解码侧将按序号断层请求关键帧）");
            true
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

/// 可取消的发送：`select!` 的 handler 里直接 `.await` 会让循环看不见取消信号。
///
/// tokio 的 `select!` 在 handler 求值期间**不再轮询其它分支**，所以对端停读（中继停走、TCP
/// 零窗口）把写缓冲填满后，`writer.send` 会永久挂起，`cancel.cancelled()` 分支再也得不到轮询
/// ——会话不结束、不回 `RemoteSessionEnd`、socket 与任务一起泄漏到 TCP 超时。故每次发送都同时
/// 等取消令牌，取消优先（`biased`）。
///
/// 发送被中途取消是安全的：会话正要结束，半截帧不会被对端当成完整帧（长度前缀 + bincode
/// 自校验），且此后不会再发新帧。
///
/// # Errors
/// 取消令牌触发时返回 [`RemoteError::Cancelled`]；发送失败时透传底层 IO 错误。
async fn send_cancellable(
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

/// 把 [`TransferHub::abort_all`] 返回的 `FileError` 依次写出。
///
/// **必须回发**：发送方读文件失败后不会再有任何 chunk 流动，对端既等不到 `FileDone` 也等不到
/// 错误，会一直挂在半途（UI 停在 99%）。写失败按会话级错误处理，与其它发送一致。
async fn send_file_errors(
    writer: &mut FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    cancel: &CancellationToken,
    msgs: Vec<SessionMessage>,
) -> RemoteResult<()> {
    for msg in msgs {
        send_cancellable(writer, cancel, SessionFrame::file(msg)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ack_mac;
    use crate::crypto::{derive_session_keys, Cipher, EphemeralKeyPair};
    use crate::error::RemoteResult;
    use crate::screen::{DecodedFrame, DisplayInfo, FrameSink};
    use crate::session::{plan_controller_handshake, ControllerHandshake, SessionCodec, SessionFrame, SessionMessage};
    use crate::transfer::{TransferEvent, TransferHub};
    use crate::transport::BoxDuplex;
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
    /// 首个样本要等满一个周期（10s）才出现，用户开窗看到的是空白。所以这里用 **1s** 的上限锁住
    /// 「立即」——远小于 [`HEARTBEAT_INTERVAL`]，写成延迟起点必然超时。
    #[tokio::test]
    async fn session_start_reports_rtt_immediately() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let (mut peer_reader, mut peer_writer) = controlled_peer_handshake(Box::new(b), &plan, "TOK", "sess-1").await;

        // 被控端：把收到的 `Heartbeat` 回一个 `HeartbeatAck`（其余帧不管）。
        let peer = tokio::spawn(async move {
            while let Some(Ok(frame)) = peer_reader.next().await {
                if matches!(frame.message, SessionMessage::Heartbeat)
                    && peer_writer
                        .send(SessionFrame::control(SessionMessage::HeartbeatAck))
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
        let rtt = tokio::time::timeout(Duration::from_secs(1), async {
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
        .expect("会话建立后 1s 内就该有延迟样本（首个 tick 立即到期）");
        assert!(rtt < Duration::from_secs(1), "内存管道 RTT 不该是 {rtt:?}");

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
                    if matches!(frame.message, SessionMessage::Heartbeat) {
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
}
