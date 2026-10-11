//! 控制器侧会话循环：握手 → 收画面 → 回传输入 → 心跳。
//!
//! 传输建立阶段已把 `Hello` 帧与中继的 `RequestRelay` 帧合并写出（见 [`crate::transport`]），
//! 因此这里**只等 `HelloAck`**。收到后派生会话密钥、装上 [`crate::crypto::Cipher`]，
//! 之后的帧全部加密。

use crate::auth::verify_ack_mac;
use crate::crypto::{derive_session_keys, Cipher};
use crate::error::{RemoteError, RemoteResult};
use crate::screen::{decode_frame, FrameSink, InputEvent, QualityPreset};
use crate::session::{ControllerHandshake, SessionCodec, SessionFrame, SessionMessage, HEARTBEAT_INTERVAL};
use crate::tiles::decode_tiles;
use crate::transfer::{TransferCommand, TransferHub, CHUNK_PAUSE};
use crate::transport::BoxDuplex;
use futures_util::{SinkExt as _, StreamExt as _};
use rust_tunnel_common::protocol::RemoteMode;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// 等待 `HelloAck` 的超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
}

/// 跑一次控制器会话，直到取消、对端关闭或出错。
///
/// `cmd_rx` 是 UI 经 [`crate::manager::RemoteManager`] 投递的命令流；`sink` 接收已解码画面
/// （[`FrameSink::on_frame`] / [`FrameSink::on_tiles`] 拿到的是 RGBA，不在 UI 线程解码）。
///
/// # Errors
/// 握手被拒、编解码失败、IO 失败时返回 [`RemoteError`]；正常结束（收到 `Bye` / 对端关闭 /
/// 取消）返回 `Ok(())`。
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
) -> RemoteResult<()> {
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

    sink.on_displays(displays);

    // 3) 主循环。
    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 出站文件块：只在有在途传输时启用（`has_outgoing` 为假时该分支不被轮询，不产生空转）。
    let mut file_tick = interval_at(Instant::now(), CHUNK_PAUSE);
    file_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut reason = String::from("会话结束");
    let result = loop {
        tokio::select! {
            () = cancel.cancelled() => {
                "用户中断".clone_into(&mut reason);
                break Ok(());
            }
            _ = heartbeat.tick() => {
                if let Err(err) = writer.send(SessionFrame::control(SessionMessage::Heartbeat)).await {
                    break Err(RemoteError::Io(err));
                }
            }
            maybe_cmd = cmd_rx.recv() => {
                let Some(cmd) = maybe_cmd else { continue };
                match cmd {
                    ControllerCommand::Input(event) => {
                        if let Err(err) = writer.send(SessionFrame::control(SessionMessage::Input(event))).await {
                            break Err(RemoteError::Io(err));
                        }
                    }
                    ControllerCommand::SelectDisplay(display) => {
                        if let Err(err) = writer
                            .send(SessionFrame::control(SessionMessage::SelectDisplay { display }))
                            .await
                        {
                            break Err(RemoteError::Io(err));
                        }
                    }
                    ControllerCommand::SetQuality(preset) => {
                        if let Err(err) = writer
                            .send(SessionFrame::control(SessionMessage::SetQuality { preset }))
                            .await
                        {
                            break Err(RemoteError::Io(err));
                        }
                    }
                    ControllerCommand::Clipboard(text) => {
                        if let Err(err) = writer
                            .send(SessionFrame::control(SessionMessage::Clipboard { text }))
                            .await
                        {
                            break Err(RemoteError::Io(err));
                        }
                    }
                    ControllerCommand::ClipboardRequest => {
                        if let Err(err) = writer
                            .send(SessionFrame::control(SessionMessage::ClipboardRequest))
                            .await
                        {
                            break Err(RemoteError::Io(err));
                        }
                    }
                    ControllerCommand::File(cmd) => match transfer.on_command(cmd).await {
                        Ok(Some(msg)) => {
                            if let Err(err) = writer.send(SessionFrame::file(msg)).await {
                                break Err(RemoteError::Io(err));
                            }
                        }
                        Ok(None) => {}
                        Err(err) => warn!(error = %err, "文件操作发起失败"),
                    },
                }
            }
            _ = file_tick.tick(), if transfer.has_outgoing() => {
                match transfer.next_outbound().await {
                    Ok(Some(msg)) => {
                        if let Err(err) = writer.send(SessionFrame::file(msg)).await {
                            break Err(RemoteError::Io(err));
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(error = %err, "文件读取失败，中断该次传输");
                        transfer.abort_all(&err.to_string());
                    }
                }
            }
            incoming = reader.next() => {
                match incoming {
                    Some(Ok(frame)) => match frame.message {
                        SessionMessage::Video { display, seq, jpeg, .. } => {
                            match decode_frame(display, seq, &jpeg) {
                                Ok(decoded) => sink.on_frame(decoded),
                                Err(err) => warn!(error = %err, "画面解码失败，跳过该帧"),
                            }
                        }
                        SessionMessage::VideoTiles { display, width, height, seq, tiles } => {
                            match decode_tiles(display, seq, width, height, &tiles) {
                                Ok(update) => sink.on_tiles(update),
                                Err(err) => warn!(error = %err, "瓦片解码失败，跳过该批"),
                            }
                        }
                        SessionMessage::DisplayList(list) => sink.on_displays(list),
                        SessionMessage::Clipboard { text } => sink.on_clipboard(text),
                        SessionMessage::ClipboardRequest => {
                            // 引擎里**没有**任何路径会让被控端发出该请求（它只负责应答），且控制器
                            // 侧根本没有 `Clipboard` 实现可应答。收到即说明对端实现异常，记一条日志。
                            debug!("控制器收到 ClipboardRequest，但控制器不实现剪贴板应答，忽略");
                        }
                        SessionMessage::Heartbeat => {
                            if let Err(err) = writer.send(SessionFrame::control(SessionMessage::HeartbeatAck)).await {
                                break Err(RemoteError::Io(err));
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
                                        if let Err(err) = writer.send(SessionFrame::file(reply)).await {
                                            write_err = Some(err);
                                            break;
                                        }
                                    }
                                    if let Some(err) = write_err {
                                        break Err(RemoteError::Io(err));
                                    }
                                }
                                Err(err) => {
                                    // 本地落盘失败：这次传输已不可信，报错并中断它，会话本身继续。
                                    warn!(error = %err, "文件接收失败，中断该次传输");
                                    transfer.abort_all(&err.to_string());
                                }
                            }
                        }
                        other => debug!(?other, "控制器收到未处理的消息"),
                    },
                    Some(Err(err)) => break Err(RemoteError::Codec(err.to_string())),
                    None => break Ok(()),
                }
            }
        }
    };

    // 4) 收尾：尽力通知对端，并把在途传输标记为失败（UI 不能停在 99%）。
    transfer.abort_all(&reason);
    let _ = writer
        .send(SessionFrame::control(SessionMessage::Bye { reason: reason.clone() }))
        .await;
    sink.on_closed(reason);
    result
}
