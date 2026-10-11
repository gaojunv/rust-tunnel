//! 被控端会话循环：握手鉴权 → 采集编码 → 注入输入 → 心跳。
//!
//! 只在 `enabled == true` 且收到合法 [`rust_tunnel_common::protocol::ControlMessage::RemoteSessionIncoming`]
//! 时才由 [`crate::manager::RemoteManager`] 启动。握手校验失败一律拒绝并记原因；会话结束/
//! 中断立即停循环、关连接。

use crate::auth::{ack_mac, PendingSessions};
use crate::crypto::{derive_session_keys, Cipher, EphemeralKeyPair};
use crate::error::{RemoteError, RemoteResult};
use crate::screen::{Clipboard, EncodeOptions, InputSink, RawFrame, ScreenSource};
use crate::session::{SessionCodec, SessionFrame, SessionMessage, HEARTBEAT_INTERVAL, SESSION_PROTO_VERSION};
use crate::tiles::TileDiffer;
use crate::transfer::{TransferHub, CHUNK_PAUSE};
use crate::transport::BoxDuplex;
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
    /// 屏幕编码参数（会话中可被控制器的 `SetQuality` 覆盖）。
    pub encode: EncodeOptions,
    /// 在途采集的看门狗时限（见 [`CAPTURE_WATCHDOG`]）。
    ///
    /// 做成字段而非直接读常量，是为了让测试用极短的值验证「采集 panic/挂死之后仍能恢复」，
    /// 生产路径统一传 [`CAPTURE_WATCHDOG`]。
    pub capture_watchdog: Duration,
}

/// 跑一次被控端会话，直到取消、对端关闭或出错。
///
/// # Errors
/// 握手被拒（版本不符、鉴权失败）、编解码/IO 失败时返回 [`RemoteError`]；正常结束返回 `Ok(())`。
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
) -> RemoteResult<()> {
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

    let capture = config.mode != RemoteMode::FileTransfer;
    let inject = config.mode == RemoteMode::Control;
    let mut encode = config.encode;
    let mut display_id = displays
        .iter()
        .find(|d| d.primary)
        .or_else(|| displays.first())
        .map_or(0, |d| d.id);

    // 3) 主循环。
    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 出站文件块：只在有在途传输时启用。
    let mut file_tick = interval_at(Instant::now(), CHUNK_PAUSE);
    file_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // 采集节拍用「可重置的 sleep」而不是固定 interval：画质档位会改 `max_fps`，档位一变
    // 节拍要立刻跟着变（`interval_at` 的周期改不了，只能重建）。
    let mut capture_tick = Box::pin(sleep(encode.capture_interval()));
    // 差分器在采集期间被**移进**阻塞任务（采集与整帧/瓦片编码都阻塞，见 `run_capture`），
    // 故用 `Option` 装、采集回来时归还。采集在途时若收到「重置基准」请求（切显示器/切档位），
    // `differ` 不在手上，先记进 `pending_differ_reset`，等它随结果回来再补做。
    let mut differ: Option<TileDiffer> = Some(TileDiffer::new());
    let mut pending_differ_reset = false;
    // 在途采集的结果经通道回传。`capture_deadline` 非空即代表有一次采集在途，保证同一时刻最多
    // 一次采集（并发两次会把阻塞线程池堆满）。循环自己保留 `capture_tx`，故 `recv()` 永不返回
    // `None`。
    let (capture_tx, mut capture_rx) = mpsc::channel::<CaptureOutcome>(1);
    // 在途采集的截止时刻（看门狗，见 `CAPTURE_WATCHDOG`）；`None` = 当前没有采集在途。
    let mut capture_deadline: Option<Instant> = None;
    // 采集代次：被看门狗判定为「已丢失」的那次采集，其阻塞任务仍可能**稍后**才把结果投进通道
    // （panic 被吞掉后由后续代码路径、或挂住之后终于返回）。带上代次才能把这种过期结果丢掉，
    // 否则会用旧基准发出错误的增量瓦片。
    let mut capture_generation: u64 = 0;
    // 连续超时次数：见 `MAX_CAPTURE_TIMEOUTS`。
    let mut capture_timeouts: u32 = 0;
    // 首帧（或切显示器/切档位之后）必须先发整帧：差分器没有基准就没有变化可言。
    let mut force_keyframe = true;
    let mut seq: u64 = 0;
    let mut reason = String::from("会话结束");
    let result = loop {
        tokio::select! {
            () = cancel.cancelled() => {
                "用户中断".clone_into(&mut reason);
                break Ok(());
            }
            _ = heartbeat.tick() => {
                if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::Heartbeat)).await {
                    break Err(err);
                }
            }
            outcome = capture_rx.recv(), if capture_deadline.is_some() => {
                match outcome {
                    // 过期结果：看门狗已判定该次采集丢失并重建了基准，这一帧与当前基准不匹配，
                    // 必须丢弃（发出错误的增量瓦片比丢一帧更糟）。此时**不能**清 `capture_deadline`
                    // ——在途的是另一次（更晚）的采集。
                    Some(CaptureOutcome { generation, .. }) if generation != capture_generation => {
                        debug!(generation, "丢弃过期的采集结果");
                    }
                    Some(CaptureOutcome { message, differ: mut restored, .. }) => {
                        capture_deadline = None;
                        capture_timeouts = 0;
                        // `capture_tx` 常驻于循环 → 只要还有发送端就不会 `None`。
                        if pending_differ_reset {
                            restored.reset();
                            pending_differ_reset = false;
                        }
                        differ = Some(restored);
                        if let Some(msg) = message {
                            // `run_capture` 用同一基值（`seq + 1`）算帧序号；同一时刻只有一次采集在途。
                            seq = seq.wrapping_add(1);
                            if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::screen(msg)).await {
                                break Err(err);
                            }
                        }
                    }
                    // 防御性兜底（`capture_tx` 常驻，理论上不会 `None`）：解除在途状态。
                    None => capture_deadline = None,
                }
            }
            // 看门狗：采集 panic/挂死时结果永不回传，见 `CAPTURE_WATCHDOG`。到点即判定该次采集
            // 丢失：放弃它（`differ` 已随闭包一起丢失，置 `None` 让下一轮采集新建基准）并强制整帧，
            // 画面因此会在下一拍恢复；连续多次才终止会话（见 `MAX_CAPTURE_TIMEOUTS`）。
            () = sleep(capture_deadline.unwrap_or_else(Instant::now).saturating_duration_since(Instant::now())), if capture_deadline.is_some() => {
                capture_deadline = None;
                differ = None;
                force_keyframe = true;
                capture_timeouts = capture_timeouts.saturating_add(1);
                if capture_timeouts >= MAX_CAPTURE_TIMEOUTS {
                    warn!(count = capture_timeouts, "采集连续超时，判定采集侧已失效，结束会话");
                    break Err(RemoteError::Platform(format!(
                        "屏幕采集连续 {capture_timeouts} 次超时（每次超过 {CAPTURE_WATCHDOG:?}）"
                    )));
                }
                warn!(count = capture_timeouts, "在途采集超过看门狗时限，判定已丢失并重建差分基线");
            }
            () = &mut capture_tick, if capture => {
                capture_tick.as_mut().reset(Instant::now() + encode.capture_interval());
                // 串行采集：上一次还没回来就跳过本轮，避免把阻塞线程池堆满。
                if capture_deadline.is_none() {
                    // 基准丢失时（看门狗放弃过一次采集）新建差分器：它没有基准，故必须整帧。
                    if differ.is_none() {
                        differ = Some(TileDiffer::new());
                        force_keyframe = true;
                    }
                    if let Some(differ) = differ.take() {
                        capture_generation = capture_generation.wrapping_add(1);
                        capture_deadline = Some(Instant::now() + config.capture_watchdog);
                        // 采集开始时消费掉「强制关键帧」：`diff(force_keyframe=true)` 必产出整帧
                        // （关键帧路径传 `prev_hash=None`），故与「产出后再清标志」等价；期间若切了
                        // 档位/显示器，那一侧会把它重新置真，让下一轮再发关键帧。
                        let force = force_keyframe;
                        force_keyframe = false;
                        let screen = Arc::clone(&screen);
                        let encode_now = encode;
                        let display_now = display_id;
                        let seq_now = seq.wrapping_add(1);
                        let generation_now = capture_generation;
                        let tx = capture_tx.clone();
                        // 采集 + 编码是阻塞调用，整体挪出 async worker（见 `run_capture`）。
                        tokio::task::spawn_blocking(move || {
                            run_capture(
                                &*screen,
                                CaptureJob {
                                    display: display_now,
                                    encode: encode_now,
                                    force_keyframe: force,
                                    seq: seq_now,
                                    generation: generation_now,
                                },
                                differ,
                                &tx,
                            );
                        });
                    }
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
                            // 切换采集目标后差分基准作废：分辨率/内容都变了，必须重发整帧。
                            if displays.iter().any(|d| d.id == target) {
                                display_id = target;
                                reset_differ(&mut differ, &mut pending_differ_reset);
                                force_keyframe = true;
                            } else {
                                // `display` 会与 `tracing::display` 撞名，故用 target 命名。
                                warn!(target, "控制器请求了不存在的显示器");
                            }
                        }
                        SessionMessage::SetQuality { preset } => {
                            encode = preset.options();
                            capture_tick.as_mut().reset(Instant::now() + encode.capture_interval());
                            reset_differ(&mut differ, &mut pending_differ_reset);
                            force_keyframe = true;
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
                            if let Err(err) =
                                send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::Clipboard { text })).await
                            {
                                break Err(err);
                            }
                        }
                        SessionMessage::Heartbeat => {
                            if let Err(err) = send_cancellable(&mut writer, &cancel, SessionFrame::control(SessionMessage::HeartbeatAck)).await {
                                break Err(err);
                            }
                        }
                        SessionMessage::HeartbeatAck => {}
                        SessionMessage::Bye { reason: r } => {
                            reason = r;
                            break Ok(());
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
                                    warn!(error = %err, "文件落盘失败，中断该次传输");
                                    if let Err(send_err) = send_file_errors(&mut writer, &cancel, transfer.abort_all(&err.to_string())).await {
                                        break Err(send_err);
                                    }
                                }
                            }
                        }
                        other => debug!(?other, "被控端收到未处理的消息"),
                    },
                    Some(Err(err)) => break Err(RemoteError::Codec(err.to_string())),
                    None => break Ok(()),
                }
            }
        }
    };

    // 取消也可能发生在某个 `send_cancellable` 里（对端停读、写缓冲填满时）：统一按「用户中断」
    // 正常收尾，与上面的 `cancel` 分支语义一致。
    let result = match result {
        Err(RemoteError::Cancelled) => {
            "用户中断".clone_into(&mut reason);
            Ok(())
        }
        other => other,
    };

    // 会话正在收尾：在途传输一并作废。这里**不必**回发 `abort_all` 的 `FileError`——`Bye` 马上
    // 就到，对端收尾时同样会清掉自己的在途传输，不会挂在半途。
    let _ = transfer.abort_all(&reason);
    let _ = timeout(
        BYE_TIMEOUT,
        writer.send(SessionFrame::control(SessionMessage::Bye { reason: reason.clone() })),
    )
    .await;
    result
}

/// 一次后台采集的结果：要发的消息（静止画面或采集/编码失败时为 `None`）+ 归还的差分器。
struct CaptureOutcome {
    /// 采集代次（见循环里的 `capture_generation`）：用于丢弃被看门狗判定为丢失的过期结果。
    generation: u64,
    message: Option<SessionMessage>,
    differ: TileDiffer,
}

/// 一次采集任务的入参。成组传递是为了让 [`run_capture`] 的签名保持可读（8 个裸参数会触发
/// `clippy::too_many_arguments`）。
///
/// `Copy`：全是标量/小结构，采集线程按值拿走一份即可（也让 clippy 不再要求改成引用）。
#[derive(Clone, Copy)]
struct CaptureJob {
    display: u32,
    encode: EncodeOptions,
    force_keyframe: bool,
    seq: u64,
    generation: u64,
}

/// 在阻塞线程里采集一帧并做差分编码。
///
/// `ScreenSource::capture`（xcap）与整帧/瓦片 JPEG 编码都是 CPU/IO 阻塞调用：1080p 整帧几十
/// 毫秒，Wayland 的 xcap portal 路径可能数秒。跑在 async worker 上会饿死同一 worker 上的控制
/// 通道读循环与心跳，故整体移进 `spawn_blocking`。`differ` 随闭包移入、随结果移出，既保证采集
/// 串行（同一时刻只有一份基准状态），也避免每帧新建差分器。
///
/// 本函数**可能 panic**（平台实现 bug）：`spawn_blocking` 的 `JoinHandle` 被丢弃后 panic 会被
/// 吞掉、结果永不回传，由调用方的看门狗兜底（见 [`CAPTURE_WATCHDOG`]）。
fn run_capture(
    screen: &dyn ScreenSource,
    job: CaptureJob,
    mut differ: TileDiffer,
    tx: &mpsc::Sender<CaptureOutcome>,
) {
    let CaptureJob {
        display,
        encode,
        force_keyframe,
        seq,
        generation,
    } = job;
    let message = match screen.capture(display) {
        Ok(frame) => match differ.diff(&frame, &encode, force_keyframe) {
            Ok(Some(result)) => Some(match result.full {
                // 关键帧与瓦片批次都走 Screen lane：控制器侧只有「整帧替换」和「局部更新」
                // 两种消费方式，不需要靠 lane 区分。
                Some(full) => SessionMessage::Video {
                    display: full.display,
                    width: full.width,
                    height: full.height,
                    seq,
                    jpeg: full.jpeg,
                },
                None => SessionMessage::VideoTiles {
                    display: frame.display,
                    width: frame.width,
                    height: frame.height,
                    seq,
                    tiles: result.tiles,
                },
            }),
            Ok(None) => None, // 静止画面：零输出，只留心跳。
            Err(err) => {
                warn!(error = %err, "画面编码失败");
                None
            }
        },
        Err(err) => {
            warn!(error = %err, "屏幕采集失败");
            None
        }
    };
    // 接收端随会话循环存活；会话已结束时发送失败是正常的，忽略。
    let _ = tx.blocking_send(CaptureOutcome {
        generation,
        message,
        differ,
    });
}

/// 重置差分基准。差分器若正被采集任务持有（采集在途），当场重置不了，先记下、等它随结果回来再
/// 补做（`force_keyframe` 已同时置真，下一帧本就必是整帧，这里只是让基准语义保持明确）。
fn reset_differ(differ: &mut Option<TileDiffer>, pending: &mut bool) {
    match differ.as_mut() {
        Some(differ) => differ.reset(),
        None => *pending = true,
    }
}

/// 可取消的发送：`select!` 的 handler 里直接 `.await` 会让循环看不见取消信号。
///
/// tokio 的 `select!` 在 handler 求值期间**不再轮询其它分支**，所以对端停读（中继停走、TCP
/// 零窗口）把写缓冲填满后，`writer.send` 会永久挂起，`cancel.cancelled()` 分支再也得不到轮询
/// ——会话不结束、任务与 socket 一起泄漏到 TCP 超时。故每次发送都同时等取消令牌，取消优先
/// （`biased`）。
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
        Clipboard, DisplayInfo, EncodeOptions, InputEvent, InputSink, KeyEvent, MouseButton, RawFrame, ScreenSource,
    };
    use crate::session::{plan_controller_handshake, ControllerHandshake, SessionCodec, SessionFrame, SessionMessage};
    use crate::transfer::{TransferEvent, TransferHub};
    use crate::transport::BoxDuplex;
    use rust_tunnel_common::protocol::RemoteMode;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};
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

    /// 大分辨率**噪声**帧：JPEG 远大于小容量 duplex，首帧发送必然阻塞。
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

    /// 取消必须能打断被控端「已阻塞在发送里」的会话循环（`send_cancellable` 的回归护栏）。
    #[tokio::test]
    async fn cancel_interrupts_blocked_send_on_controlled() {
        // duplex 容量远小于大帧 JPEG：首帧发送即填满写缓冲并挂起。
        let (a, b) = tokio::io::duplex(16 * 1024);
        let plan = plan_controller_handshake("sess-1", "TOK", "111111111").expect("造握手材料");

        let (input, _input_rx) = RecordingInput::with_notify();
        let cancel = CancellationToken::new();
        let ctld = tokio::spawn(run_controlled(
            Box::new(b),
            config(pending_with("TOK"), RemoteMode::Control),
            Arc::new(NoiseScreen),
            input,
            None,
            transfer_hub().0,
            cancel.clone(),
        ));

        // 对端此时不读，让大帧发送阻塞住。**不能**在取消后再读对端：读端一消费，被卡住的发送
        // 就能推进、连修复前的代码也会「自行恢复」，测试就测不到取消了。故对端自始至终不读，
        // 收尾的 `Bye` 只能等满 `BYE_TIMEOUT`（2s），返回时间以 3s 为上界。
        let (_peer_reader, _peer_writer) = controller_peer_handshake(Box::new(a), &plan, "TOK", "sess-1").await;
        tokio::time::sleep(Duration::from_millis(300)).await;

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
