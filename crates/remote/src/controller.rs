//! 控制器侧会话循环：握手 → 收画面 → 回传输入 → 心跳。
//!
//! 传输建立阶段已把 `Hello` 帧与中继的 `RequestRelay` 帧合并写出（见 [`crate::transport`]），
//! 因此这里**只等 `HelloAck`**。收到后派生会话密钥、装上 [`crate::crypto::Cipher`]，
//! 之后的帧全部加密。

use crate::auth::verify_ack_mac;
use crate::crypto::{derive_session_keys, Cipher};
use crate::error::{RemoteError, RemoteResult};
use crate::screen::{decode_frame, FrameSink, InputEvent};
use crate::session::{ControllerHandshake, SessionCodec, SessionFrame, SessionMessage, HEARTBEAT_INTERVAL};
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
/// `input_rx` 是 UI 经 [`crate::manager::RemoteManager::send_input`] 投递的输入事件；
/// `sink` 接收已解码画面（[`FrameSink::on_frame`] 拿到的是 RGBA，不在 UI 线程解码）。
///
/// # Errors
/// 握手被拒、编解码失败、IO 失败时返回 [`RemoteError`]；正常结束（收到 `Bye` / 对端关闭 /
/// 取消）返回 `Ok(())`。
#[expect(
    clippy::too_many_lines,
    reason = "握手校验 + 画面/输入/心跳的单会话循环，拆开反而丢失控制流全貌"
)]
pub async fn run_controller(
    duplex: BoxDuplex,
    config: ControllerConfig,
    handshake: ControllerHandshake,
    mut input_rx: mpsc::Receiver<InputEvent>,
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
            maybe_input = input_rx.recv() => {
                if let Some(event) = maybe_input {
                    if let Err(err) = writer.send(SessionFrame::control(SessionMessage::Input(event))).await {
                        break Err(RemoteError::Io(err));
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
                        SessionMessage::DisplayList(list) => sink.on_displays(list),
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
                        other => debug!(?other, "控制器收到未处理的消息"),
                    },
                    Some(Err(err)) => break Err(RemoteError::Codec(err.to_string())),
                    None => break Ok(()),
                }
            }
        }
    };

    // 4) 收尾：尽力通知对端。
    let _ = writer
        .send(SessionFrame::control(SessionMessage::Bye { reason: reason.clone() }))
        .await;
    sink.on_closed(reason);
    result
}
