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
use tokio::time::{interval_at, sleep, timeout, Instant, MissedTickBehavior};
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// 等待 `Hello` 的超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
    let mut differ = TileDiffer::new();
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
                if let Err(err) = writer.send(SessionFrame::control(SessionMessage::Heartbeat)).await {
                    break Err(RemoteError::Io(err));
                }
            }
            () = &mut capture_tick, if capture => {
                capture_tick.as_mut().reset(Instant::now() + encode.capture_interval());
                match screen.capture(display_id) {
                    Ok(frame) => match differ.diff(&frame, &encode, force_keyframe) {
                        Ok(Some(result)) => {
                            force_keyframe = false;
                            seq = seq.wrapping_add(1);
                            // 关键帧与瓦片批次都走 Screen lane：控制器侧只有「整帧替换」和
                            // 「局部更新」两种消费方式，不需要靠 lane 区分。
                            let msg = match result.full {
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
                            };
                            if let Err(err) = writer.send(SessionFrame::screen(msg)).await {
                                break Err(RemoteError::Io(err));
                            }
                        }
                        Ok(None) => {} // 静止画面：零输出，只留心跳。
                        Err(err) => warn!(error = %err, "画面编码失败"),
                    },
                    Err(err) => warn!(error = %err, "屏幕采集失败"),
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
                        SessionMessage::Input(event) => {
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
                                differ.reset();
                                force_keyframe = true;
                            } else {
                                // `display` 会与 `tracing::display` 撞名，故用 target 命名。
                                warn!(target, "控制器请求了不存在的显示器");
                            }
                        }
                        SessionMessage::SetQuality { preset } => {
                            encode = preset.options();
                            capture_tick.as_mut().reset(Instant::now() + encode.capture_interval());
                            differ.reset();
                            force_keyframe = true;
                        }
                        SessionMessage::Clipboard { text } => {
                            if let Some(cb) = clipboard.as_ref() {
                                if let Err(err) = cb.set(&text) {
                                    warn!(error = %err, "写入剪贴板失败");
                                }
                            } else {
                                debug!("本端未安装剪贴板实现，忽略写入");
                            }
                        }
                        SessionMessage::ClipboardRequest => {
                            let reply = match clipboard.as_ref() {
                                Some(cb) => match cb.get() {
                                    Ok(text) => SessionMessage::Clipboard { text },
                                    Err(err) => {
                                        warn!(error = %err, "读取剪贴板失败");
                                        SessionMessage::Clipboard { text: String::new() }
                                    }
                                },
                                None => SessionMessage::Clipboard { text: String::new() },
                            };
                            if let Err(err) = writer.send(SessionFrame::control(reply)).await {
                                break Err(RemoteError::Io(err));
                            }
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
                                    warn!(error = %err, "文件落盘失败，中断该次传输");
                                    transfer.abort_all(&err.to_string());
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

    transfer.abort_all(&reason);
    let _ = writer
        .send(SessionFrame::control(SessionMessage::Bye { reason: reason.clone() }))
        .await;
    result
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
