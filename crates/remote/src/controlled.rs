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
//! 采集(spawn_blocking) --有界(1)--> 编码任务(spawn_blocking) --有界(WRITER_QUEUE)--> 发送(独占 writer)
//! ```
//!
//! **丢弃策略是这个管线里最关键的正确性决定**：时域编码的参考链是串起来的，丢掉一个**已编码**
//! 的 P 帧，后续引用它的帧在解码端永远解不出来（花屏且不自愈，只能等下一个 IDR）。所以背压的
//! 出口只能放在**编码之前**——输出队列满就跳过这一帧（等价于降帧率，语义合法），绝不在编码之后
//! 丢弃。**不要把这条改成「编码后丢弃」，那不是优化，是花屏。**
//!
//! # QoS 闭环（阶段 3）
//!
//! 主循环持有 [`RateController`]（AIMD）：每收到一条控制器的 `QosFeedback` 就决策一次，把新的
//! **生效值**（[`EncodeRates`]）交给下一次编码任务。码率/帧率变化由编码任务**在线重配置**
//! （vendor 补丁 6），**不重建、不插 IDR**；只有降编码分辨率（几何变化）才重建 + 强制 IDR。
//! 详见 [`crate::qos`] 的模块文档。

use crate::auth::{ack_mac, PendingSessions};
use crate::crypto::{derive_session_keys, Cipher, EphemeralKeyPair};
use crate::error::{RemoteError, RemoteResult};
use crate::qos::{Feedback, RateController};
use crate::screen::{Clipboard, EncodeOptions, EncodeRates, InputSink, RawFrame, ScreenSource};
use crate::session::{
    trusted_rtt, CodecId, SessionCodec, SessionEnd, SessionFrame, SessionMessage, HEARTBEAT_INTERVAL,
    SESSION_PROTO_VERSION, VIDEO_FLAG_CONFIG, VIDEO_FLAG_KEYFRAME,
};
use crate::stats::{SessionStats, SessionStatsReporter};
use crate::transfer::{TransferHub, CHUNK_PAUSE};
use crate::transport::BoxDuplex;
use crate::video::{encode_dimensions, VideoEncoder, MAX_ENCODE_WIDTH};
use futures_util::{SinkExt as _, StreamExt as _};
use rust_tunnel_common::protocol::RemoteMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval_at, sleep, timeout, Instant, MissedTickBehavior};
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// 等待 `Hello` 的超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 收尾 `Bye` 的发送上限。对端可能已停读（写缓冲满），不加限时会让 teardown 永久卡住；`Bye`
/// 只是尽力通知，超时丢弃即可。
const BYE_TIMEOUT: Duration = Duration::from_secs(2);

/// 出站帧队列深度。
///
/// 画面帧是大头（可达 MiB 级），队列过深会在写侧卡住时把内存顶上去；过浅则正常突发下容易丢帧。
/// 4 足以吸收一次采集与心跳的抖动，同时把最坏内存压在「4 × 单帧」量级。
const WRITER_QUEUE: usize = 4;

/// 编码侧读数的上报间隔（见 [`EncodeStatsWindow`]）。
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// 投给出站写任务的一条消息。
enum WriterMsg {
    /// 普通出站帧：写侧被取消或出错即停止后续发送。
    Frame(SessionFrame),
    /// 收尾帧（`Bye`）：**不再理会取消令牌**，限时尽力写出后结束写任务。
    ///
    /// 取消正是最常见的结束原因，若让取消令牌把它一并掐掉，对端就只能看到「连接被断开」，
    /// 把一次用户主动中断记成 `deliberate: false`（进而触发自动重连）。
    Close(SessionFrame),
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
    /// 在途采集的看门狗时限（见 [`CAPTURE_WATCHDOG`]）。
    ///
    /// 做成字段而非直接读常量，是为了让测试用极短的值验证「采集 panic/挂死之后仍能恢复」，
    /// 生产路径统一传 [`CAPTURE_WATCHDOG`]。
    pub capture_watchdog: Duration,
    /// 统计数据上报句柄（延迟读数）；无管理端时用 [`SessionStatsReporter::noop`]。
    pub stats: SessionStatsReporter,
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

    let displays = screen.displays();

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

    // 出站写任务：独占写半边。会话 `select!` 循环只把帧投进有界队列，**不再在分支里 await 写**
    // ——否则写侧被背压卡住时（对端停读、中继停走），输入/心跳分支完全得不到轮询，形成队头阻塞。
    let (tx, rx) = mpsc::channel::<WriterMsg>(WRITER_QUEUE);
    let writer_task = tokio::spawn(writer_loop(writer, rx, cancel.clone()));

    // 编码任务：独占编码器，从有界队列取原始帧、去重、编码，再把 `VideoPacket` 投给出站队列。
    // **绝不丢弃已编码的帧**（见模块文档的流水线说明）：输出队列满时跳过的是**编码之前**的这一帧。
    let (encode_tx, encode_rx) = mpsc::channel::<EncodeJob>(1);
    let encode_task = tokio::spawn(encode_loop(
        encode_rx,
        tx.clone(),
        cancel.clone(),
        config.stats.clone(),
    ));

    let capture = config.mode != RemoteMode::FileTransfer;
    let inject = config.mode == RemoteMode::Control;
    // 编码参数自阶段 3 起分两层：`config.encode` 是档位**上限**，`encode` 是闭环**当前生效值**
    // （起步 = 上限）。`SetQuality` 换上限，`QosFeedback` 动生效值。
    let mut rate = RateController::new(config.encode);
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
    // RTT 样本：发 `Heartbeat` 时记时刻，收到 `HeartbeatAck` 时配对算 RTT。**复用既有心跳往返**，
    // 不新增数据面变体（见 `HEARTBEAT_INTERVAL`）。
    let mut pending_heartbeat: Option<Instant> = None;
    let mut reason = String::from("会话结束");
    let result: RemoteResult<SessionEnd> = loop {
        tokio::select! {
            () = cancel.cancelled() => {
                "用户中断".clone_into(&mut reason);
                break Ok(SessionEnd { reason: reason.clone(), deliberate: true });
            }
            _ = heartbeat.tick() => {
                // 覆盖旧样本：上一个 ack 迟迟不来时，下一次心跳的往返才是与当前时刻相关的读数。
                pending_heartbeat = Some(Instant::now());
                if let Err(err) = enqueue(&tx, &cancel, SessionFrame::control(SessionMessage::Heartbeat)).await {
                    break Err(err);
                }
            }
            outcome = capture_rx.recv(), if capture_deadline.is_some() => {
                match outcome {
                    // 过期结果：看门狗已判定该次采集丢失。丢掉它（在途的是另一次更晚的采集，
                    // **不能**清 `capture_deadline`）。
                    Some(CaptureOutcome { generation, .. }) if generation != capture_generation => {
                        debug!(generation, "丢弃过期的采集结果");
                    }
                    Some(CaptureOutcome { frame, capture_ms, .. }) => {
                        capture_deadline = None;
                        capture_timeouts = 0;
                        if let Some(frame) = frame {
                            debug!(capture_ms, "采集一帧");
                            let job = EncodeJob { display: display_id, rates: encode, force_keyframe, frame };
                            // 有界(1) 队列满 = 编码/发送侧还没消化完上一帧 → **编码之前**跳过这一
                            // 帧（等价降帧率）。关键帧请求保留到真正投递成功的那一次。
                            match encode_tx.try_send(job) {
                                Ok(()) => force_keyframe = false,
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    debug!("编码队列已满，跳过本轮采集（编码前丢帧，不破坏参考链）");
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => break Err(link_closed()),
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
                capture_tick.as_mut().reset(Instant::now() + encode.capture_interval());
                // 串行采集：上一次还没回来就跳过本轮，避免把阻塞线程池堆满。
                if capture_deadline.is_none() {
                    capture_generation = capture_generation.wrapping_add(1);
                    capture_deadline = Some(Instant::now() + config.capture_watchdog);
                    let screen = Arc::clone(&screen);
                    let display_now = display_id;
                    let generation_now = capture_generation;
                    let tx = capture_tx.clone();
                    // 采集是阻塞调用（xcap 的 portal 路径可能数秒），整体挪出 async worker。
                    tokio::task::spawn_blocking(move || {
                        run_capture(&*screen, display_now, generation_now, &tx);
                    });
                }
            }
            _ = file_tick.tick(), if transfer.has_outgoing() => {
                match transfer.next_outbound().await {
                    Ok(Some(msg)) => {
                        if let Err(err) = enqueue(&tx, &cancel, SessionFrame::file(msg)).await {
                            break Err(err);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(error = %err, "文件读取失败，中断该次传输");
                        // 回发 `FileError`：发送方不再有 chunk 流动，对端的 `Incoming` 会一直挂着。
                        if let Err(send_err) = send_file_errors(&tx, &cancel, transfer.abort_all(&err.to_string())).await {
                            break Err(send_err);
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
                        SessionMessage::SelectDisplay { display: target } => {
                            // 切换采集目标后参考画面作废：分辨率/内容都变了，必须重发关键帧。
                            if displays.iter().any(|d| d.id == target) {
                                display_id = target;
                                force_keyframe = true;
                            } else {
                                // `display` 会与 `tracing::display` 撞名，故用 target 命名。
                                warn!(target, "控制器请求了不存在的显示器");
                            }
                        }
                        SessionMessage::SetQuality { preset } => {
                            // 档位是**上限**：换上限并把闭环重置到新上限（用户显式意图，不带降级历史）。
                            rate.set_ceiling(preset.options());
                            encode = rate.current();
                            capture_tick.as_mut().reset(Instant::now() + encode.capture_interval());
                            force_keyframe = true;
                        }
                        SessionMessage::QosFeedback { rtt_ms, recv_kbps, decode_ms, backlog } => {
                            // 控制器的每秒反馈 + 本端出站队列积压 → 一次 AIMD 决策。
                            // **码率/帧率走在线重配置（不重建、不插 IDR）**，只有降分辨率才置
                            // `force_keyframe`（几何变化，见 [`crate::qos`]）。
                            let local_backlog =
                                u32::try_from(WRITER_QUEUE.saturating_sub(tx.capacity())).unwrap_or(u32::MAX);
                            let decision = rate.on_feedback(
                                Feedback { rtt_ms, recv_kbps, decode_ms, backlog },
                                local_backlog,
                            );
                            if decision.rates.fps != encode.fps {
                                // 帧率变了：采集节拍要立刻跟着变（与 `SetQuality` 同款）。
                                capture_tick.as_mut().reset(Instant::now() + decision.rates.capture_interval());
                            }
                            encode = decision.rates;
                            if decision.force_keyframe {
                                force_keyframe = true;
                            }
                            debug!(
                                rtt_ms,
                                recv_kbps,
                                decode_ms,
                                backlog,
                                local_backlog,
                                action = ?decision.action,
                                bitrate_kbps = encode.bitrate_kbps,
                                fps = encode.fps,
                                scale = encode.scale,
                                "QoS 决策"
                            );
                        }
                        SessionMessage::RequestKeyframe { display } => {
                            // 控制器解码失败/丢弃了参考帧时的恢复请求。
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
                            if let Err(err) = enqueue(&tx, &cancel, SessionFrame::control(SessionMessage::Clipboard { text })).await {
                                break Err(err);
                            }
                        }
                        SessionMessage::Heartbeat => {
                            if let Err(err) = enqueue(&tx, &cancel, SessionFrame::control(SessionMessage::HeartbeatAck)).await {
                                break Err(err);
                            }
                        }
                        SessionMessage::HeartbeatAck => {
                            if let Some(sent) = pending_heartbeat.take() {
                                if let Some(rtt) = trusted_rtt(sent, Instant::now()) {
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
                                        if let Err(err) = enqueue(&tx, &cancel, SessionFrame::file(reply)).await {
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
                                    if let Err(send_err) = send_file_errors(&tx, &cancel, transfer.abort_all(&err.to_string())).await {
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
    // 收尾帧经同一队列交给写任务：即便取消已触发，写任务仍会限时尽力写出 `Bye`（见 `writer_loop`），
    // 使对端把它记成「主动结束」而不是「连接断开」。队列满时 `send` 会等写侧腾出位置，故同样限时。
    let _ = timeout(
        BYE_TIMEOUT,
        tx.send(WriterMsg::Close(SessionFrame::control(SessionMessage::Bye {
            reason: reason.clone(),
        }))),
    )
    .await;
    // 编码任务先停：它可能正阻塞在出站队列的 `send` 上（对端停读）。关掉它的输入队列并限时等待，
    // 超时则中止——它的 `send` 在写任务退出后会拿到 `Closed`，正常收场。
    drop(encode_tx);
    let mut encode_task = encode_task;
    let _ = timeout(BYE_TIMEOUT, &mut encode_task).await;
    encode_task.abort();
    drop(tx);
    // 写任务自己的 `Bye` 发送也限时（`BYE_TIMEOUT`），这里再给一个上界；超时则中止它——写任务
    // 仍卡在停读的对端上时，只有丢弃写半边（= 关闭 socket）才能避免把连接泄漏到 TCP 超时。
    let mut writer_task = writer_task;
    let _ = timeout(BYE_TIMEOUT, &mut writer_task).await;
    writer_task.abort();
    result
}

/// 出站写任务：独占写半边，从有界队列取帧写线。
///
/// 取消语义：普通帧的发送同时等取消令牌（[`send_cancellable`]），取消一旦触发就丢弃后续普通帧、
/// 只等收尾帧——这样「取消能立刻打断被背压卡住的发送」与「收尾 `Bye` 仍能尽力发出」两件事同时成立。
async fn writer_loop(
    mut writer: FramedWrite<tokio::io::WriteHalf<BoxDuplex>, SessionCodec>,
    mut rx: mpsc::Receiver<WriterMsg>,
    cancel: CancellationToken,
) {
    // 取消后不再写普通帧，只等收尾帧。
    let mut cancelled = false;
    while let Some(msg) = rx.recv().await {
        match msg {
            WriterMsg::Frame(frame) => {
                if cancelled {
                    continue;
                }
                match send_cancellable(&mut writer, &cancel, frame).await {
                    Ok(()) => {}
                    Err(RemoteError::Cancelled) => cancelled = true,
                    Err(err) => {
                        debug!(error = %err, "出站写失败，写任务结束");
                        break;
                    }
                }
            }
            WriterMsg::Close(frame) => {
                // 收尾帧不理会取消令牌（取消正是最常见的结束原因），限时尽力写出。
                let _ = timeout(BYE_TIMEOUT, writer.send(frame)).await;
                break;
            }
        }
    }
}

/// 投递一帧给出站写任务（**控制帧与文件帧**专用）。
///
/// # 为什么这里可以等、画面帧却不能走这条路
///
/// 队头阻塞的成因是**画面帧**：它曾是大块（MiB 级 JPEG），写侧被背压卡住时会把有界队列塞满。
/// 而**控制帧**只有几十字节、**文件帧**本身已由 [`CHUNK_PAUSE`] 节流，故它们**绝不可丢**：
/// 丢一条 `FileChunk` 会让接收侧静默少收一截字节（`TransferHub::on_done` 的字节数校验会把这种
/// 损坏变成显式错误，但正确做法是根本不丢），丢一条控制帧会让心跳/剪贴板应答凭空消失。队列被
/// 暂时塞满时**可取消地等**它腾空；取消令牌保证「用户点中断能立刻脱身」这一既有语义不受影响。
///
/// **画面帧不走这里**：它在编码任务里经 [`send_video`] 投递——那条路径**绝不丢弃**（见模块文档），
/// 背压出口在编码之前。
///
/// # Errors
/// 等待队列腾空时被取消返回 [`RemoteError::Cancelled`]；写任务已结束（链路断开）返回
/// [`RemoteError::Io`]。
async fn enqueue(
    tx: &mpsc::Sender<WriterMsg>,
    cancel: &CancellationToken,
    frame: SessionFrame,
) -> RemoteResult<()> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(RemoteError::Cancelled),
        res = tx.send(WriterMsg::Frame(frame)) => res.map_err(|_| link_closed()),
    }
}

/// 出站写任务已结束（链路断开）时的会话级错误。
fn link_closed() -> RemoteError {
    RemoteError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "出站写任务已结束",
    ))
}

/// 一次后台采集的结果：原始帧（编码在独立任务里做）。
struct CaptureOutcome {
    /// 采集代次（见循环里的 `capture_generation`）：用于丢弃被看门狗判定为丢失的过期结果。
    generation: u64,
    /// 采集到的原始帧；采集失败时为 `None`。
    frame: Option<RawFrame>,
    /// 本次采集耗时（毫秒；采集失败时也记实际耗时）。**测量**用，不参与控制决策。
    capture_ms: u64,
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
    /// 原始帧。
    frame: RawFrame,
}

/// 在阻塞线程里采集一帧。
///
/// `ScreenSource::capture`（xcap）是 CPU/IO 阻塞调用：1080p 几十毫秒，Wayland 的 xcap portal
/// 路径可能数秒。跑在 async worker 上会饿死同一 worker 上的控制通道读循环与心跳，故整体移进
/// `spawn_blocking`。
///
/// 本函数**可能 panic**（平台实现 bug）：`spawn_blocking` 的 `JoinHandle` 被丢弃后 panic 会被
/// 吞掉、结果永不回传，由调用方的看门狗兜底（见 [`CAPTURE_WATCHDOG`]）。
fn run_capture(screen: &dyn ScreenSource, display: u32, generation: u64, tx: &mpsc::Sender<CaptureOutcome>) {
    let started = Instant::now();
    let frame = match screen.capture(display) {
        Ok(frame) => Some(frame),
        Err(err) => {
            warn!(error = %err, "屏幕采集失败");
            None
        }
    };
    let capture_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // 接收端随会话循环存活；会话已结束时发送失败是正常的，忽略。
    let _ = tx.blocking_send(CaptureOutcome {
        generation,
        frame,
        capture_ms,
    });
}

/// 编码侧读数累加器：按 [`STATS_INTERVAL`] 汇总一次，经进程内 [`SessionStatsReporter`] 送到 UI。
///
/// **不进热路径的锁**：只在编码任务里累加几个整数，到点发一条无界通道消息（接收端缺席即丢弃）。
struct EncodeStatsWindow {
    /// 本窗口起点。
    start: Instant,
    /// 本窗口编码成功的帧数。
    frames: u32,
    /// 本窗口出码字节数。
    bytes: u64,
    /// 本窗口编码耗时合计（毫秒）。
    encode_ms: u64,
}

impl EncodeStatsWindow {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            frames: 0,
            bytes: 0,
            encode_ms: 0,
        }
    }

    /// 记一帧的产出与耗时。
    fn record(&mut self, bytes: usize, encode_ms: u64) {
        self.frames = self.frames.saturating_add(1);
        self.bytes = self.bytes.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        self.encode_ms = self.encode_ms.saturating_add(encode_ms);
    }

    /// 到点则上报一次并重置窗口。`bitrate_kbps` 是**当前生效的目标码率**（闭环值，非实测出码率）。
    fn maybe_report(&mut self, stats: &SessionStatsReporter, bitrate_kbps: u32) {
        let elapsed = self.start.elapsed();
        if elapsed < STATS_INTERVAL {
            return;
        }
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
        stats.report(SessionStats::Encode {
            encode_ms,
            fps,
            bitrate_kbps,
        });
        self.start = Instant::now();
        self.frames = 0;
        self.bytes = 0;
        self.encode_ms = 0;
    }
}

/// 编码任务：独占编码器，去重 → （在线重配置）→ 编码 → 投递。
///
/// # 丢弃策略（**改动前先读模块文档**）
///
/// - **静止画面完全不喂编码器**：与上一帧原始 RGBA 逐字节相同（且不强制关键帧）时直接跳过——
///   不转换、不编码、不发送。这是「带宽趋零」的唯一来源。
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
async fn encode_loop(
    mut rx: mpsc::Receiver<EncodeJob>,
    out: mpsc::Sender<WriterMsg>,
    cancel: CancellationToken,
    stats: SessionStatsReporter,
) {
    let mut encoder: Option<VideoEncoder> = None;
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

    while let Some(job) = rx.recv().await {
        if cancel.is_cancelled() {
            break;
        }
        let EncodeJob {
            display,
            rates,
            force_keyframe,
            frame,
        } = job;
        // 每秒上报一次编码侧读数（静止画面也会经过这里——采集每拍都投递任务，只是被去重跳过）。
        window.maybe_report(&stats, rates.bitrate_kbps);

        let (width, height) = encode_dimensions(frame.width, frame.height, rates.scale, MAX_ENCODE_WIDTH);
        // **几何变化才重建**：码率/帧率在下方在线重配置。
        let geometry_changed = encoder_dims != Some((width, height));
        if geometry_changed {
            if sent_dims != Some((width, height)) {
                let config = SessionMessage::ScreenConfig {
                    display,
                    codec: CodecId::Vp9,
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

        // 静止画面：完全不喂编码器（不转换、不编码、不发送）。
        if !force && prev_rgba.as_deref() == Some(frame.rgba.as_slice()) {
            continue;
        }
        // 背压出口在**编码之前**：出站队列满就跳过这一帧。绝不能改成「编码后丢弃」。
        // 被跳过的关键帧请求必须留下（`pending_keyframe`），否则控制器的恢复请求会蒸发。
        if out.capacity() == 0 {
            pending_keyframe |= force;
            debug!("出站队列已满，跳过本轮编码（编码前丢帧，不破坏参考链）");
            continue;
        }
        if encoder.is_none() {
            match VideoEncoder::new(width, height, rates.bitrate_kbps, rates.fps) {
                Ok(enc) => encoder = Some(enc),
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
        let (src_width, src_height) = (frame.width, frame.height);
        let rgba = frame.rgba;
        let started = Instant::now();
        // 编码是阻塞调用，移出 async worker；编码器随闭包移入、随结果移出（独占持有）。
        // 码率/帧率的在线重配置也在这里做（`reconfig_and_encode`），与编码同一次阻塞调用。
        let joined = tokio::task::spawn_blocking(move || {
            let mut enc = enc;
            let result = reconfig_and_encode(&mut enc, &rates, &rgba, src_width, src_height, force);
            (enc, result, rgba)
        })
        .await;
        let (enc, result, rgba) = match joined {
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
                prev_rgba = Some(rgba);
                // `force` 为真时本帧必是 IDR，请求已被满足。
                pending_keyframe = false;
                if packet.is_empty() {
                    continue;
                }
                seq = seq.wrapping_add(1);
                window.record(packet.data.len(), encode_ms);
                let mut flags = 0u8;
                if geometry_changed {
                    flags |= VIDEO_FLAG_CONFIG;
                }
                if packet.keyframe {
                    flags |= VIDEO_FLAG_KEYFRAME;
                }
                let msg = SessionMessage::VideoPacket {
                    display,
                    seq,
                    codec: CodecId::Vp9,
                    flags,
                    width,
                    height,
                    data: packet.data,
                };
                if !send_video(&out, &cancel, msg).await {
                    break;
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
    enc: &mut VideoEncoder,
    rates: &EncodeRates,
    rgba: &[u8],
    src_width: u32,
    src_height: u32,
    force: bool,
) -> RemoteResult<crate::video::EncodedVideo> {
    if enc.bitrate_kbps() != rates.bitrate_kbps {
        enc.set_bitrate(rates.bitrate_kbps)?;
    }
    if enc.fps() != u32::from(rates.fps) {
        enc.set_fps(rates.fps)?;
    }
    enc.encode(rgba, src_width, src_height, force)
}

/// 把一帧画面消息投给出站写任务，**绝不丢弃**。
///
/// 与 [`enqueue`] 的区别只在语义：画面帧一旦编码就不能丢（参考链），故这里 `send().await`
/// 直到队列腾空；背压由编码任务之前的「跳过采集」出口消化（见 [`encode_loop`]）。
/// 返回 `false` 表示会话正在收尾（取消触发或写任务已结束），编码任务据此退出。
async fn send_video(out: &mpsc::Sender<WriterMsg>, cancel: &CancellationToken, msg: SessionMessage) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        res = out.send(WriterMsg::Frame(SessionFrame::screen(msg))) => res.is_ok(),
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

/// 把 [`TransferHub::abort_all`] 返回的 `FileError` 依次投给出站写任务。
///
/// **必须回发**：发送方读文件失败后不会再有任何 chunk 流动，对端既等不到 `FileDone` 也等不到
/// 错误，会一直挂在半途（UI 停在 99%）。走 [`enqueue`] 的文件帧路径（满队列时可取消地等，不丢）。
async fn send_file_errors(
    tx: &mpsc::Sender<WriterMsg>,
    cancel: &CancellationToken,
    msgs: Vec<SessionMessage>,
) -> RemoteResult<()> {
    for msg in msgs {
        enqueue(tx, cancel, SessionFrame::file(msg)).await?;
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
        Clipboard, DisplayInfo, EncodeOptions, EncodeRates, InputEvent, InputSink, KeyEvent, MouseButton, RawFrame,
        ScreenSource,
    };
    use crate::session::{plan_controller_handshake, ControllerHandshake, SessionCodec, SessionFrame, SessionMessage};
    use crate::transfer::{TransferEvent, TransferHub};
    use crate::transport::BoxDuplex;
    use rust_tunnel_common::protocol::RemoteMode;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    }

    impl RecordingInput {
        fn with_notify() -> (Arc<Self>, mpsc::UnboundedReceiver<InputEvent>) {
            let (tx, rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    notify: Mutex::new(Some(tx)),
                }),
                rx,
            )
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
            capture_watchdog,
            stats: SessionStatsReporter::noop(),
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

    /// 作为「控制器」完成握手：写 `Hello`、读 `HelloAck`、装上加密，返回读写半边。
    async fn controller_peer_handshake(
        stream: BoxDuplex,
        plan: &ControllerHandshake,
        token: &str,
        session_id: &str,
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
        let (a, b) = tokio::io::duplex(64 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");
        let bad_hello = SessionMessage::Hello {
            session_id: "sess-1".to_owned(),
            proto_version: SESSION_PROTO_VERSION.wrapping_add(1),
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
        }
    }

    /// 一条出站消息是否为关键帧视频包。
    fn is_keyframe(msg: &WriterMsg) -> bool {
        matches!(
            msg,
            WriterMsg::Frame(SessionFrame {
                message: SessionMessage::VideoPacket { flags, .. },
                ..
            }) if flags & VIDEO_FLAG_KEYFRAME != 0
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
    async fn next_video(out_rx: &mut mpsc::Receiver<WriterMsg>) -> (u8, u32, u32) {
        loop {
            match out_rx.recv().await {
                Some(WriterMsg::Frame(SessionFrame {
                    message: SessionMessage::VideoPacket { flags, width, height, .. },
                    ..
                })) => return (flags, width, height),
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
        let (out_tx, mut out_rx) = mpsc::channel::<WriterMsg>(8);
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
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
        let (out_tx, mut out_rx) = mpsc::channel::<WriterMsg>(8);
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
        ));

        let hi = EncodeRates::ceiling(EncodeOptions::default());
        // scale 减半 → 编码尺寸减半（几何变化）。
        let half = EncodeRates { scale: 0.5, ..hi };

        job_tx
            .send(EncodeJob {
                display: 0,
                rates: hi,
                force_keyframe: true,
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
        let (out_tx, mut out_rx) = mpsc::channel::<WriterMsg>(2);
        // 供测试自己往出站队列里塞占位帧（模拟背压），故先克隆一份。
        let filler = out_tx.clone();
        let (job_tx, job_rx) = mpsc::channel::<EncodeJob>(JOB_CAP);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(encode_loop(
            job_rx,
            out_tx,
            cancel.clone(),
            SessionStatsReporter::noop(),
        ));
        let opts = EncodeRates::ceiling(EncodeOptions::default());
        let job = |frame: RawFrame, force: bool| EncodeJob {
            display: 0,
            rates: opts,
            force_keyframe: force,
            frame,
        };
        let placeholder = || WriterMsg::Frame(SessionFrame::control(SessionMessage::Heartbeat));

        // 首帧：先下发 `ScreenConfig`，随后是关键帧。
        job_tx.send(job(shaded(0, 64, 48, 10), true)).await.expect("投递首帧");
        assert!(
            matches!(
                out_rx.recv().await,
                Some(WriterMsg::Frame(SessionFrame { message: SessionMessage::ScreenConfig { .. }, .. }))
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
}
