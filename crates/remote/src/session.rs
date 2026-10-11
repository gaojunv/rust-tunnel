//! 会话数据面协议：帧编解码 + 握手消息 + 保活不变量。
//!
//! # 帧格式
//!
//! ```text
//! ┌──────────────┬────────┬───────────────────────────────┐
//! │ u32 大端长度  │ u8 lane│ 载荷（bincode / AEAD 密文）      │
//! └──────────────┴────────┴───────────────────────────────┘
//! ```
//!
//! 长度字段 = `1 (lane) + 载荷长度`。载荷在未加密时为 [`SessionMessage`] 的 bincode 编码，
//! 装上 [`crate::crypto::Cipher`] 后为「密文 ‖ tag」。`lane` 保持明文，为阶段 2 的多路复用
//! 留出「不解密即可路由」的能力。
//!
//! 上限 [`MAX_SESSION_FRAME`]（16 MiB）：**不复用**控制通道的 1 MiB 上限，因为整屏 JPEG
//! 轻易超过 1 MiB。
//!
//! # 保活
//!
//! [`HEARTBEAT_INTERVAL`] = 10s，硬性小于 hbbr 中继的 `idle_timeout`（30s，见
//! `crates/rustdesk/src/relay.rs`）。**静止画面也必须发心跳**，否则中继按空闲断开。

use crate::auth::handshake_mac;
use crate::crypto::{random_nonce, Cipher, EphemeralKeyPair};
use crate::error::{RemoteError, RemoteResult};
use crate::screen::{DisplayInfo, InputEvent};
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::io;
use std::time::Duration;
use tokio_util::codec::{Decoder, Encoder};

/// 会话协议版本；不匹配的 `Hello` 一律拒绝。
pub const SESSION_PROTO_VERSION: u16 = 1;

/// 单帧载荷上限（16 MiB）。
pub const MAX_SESSION_FRAME: usize = 16 * 1024 * 1024;

/// 心跳间隔。**必须小于 hbbr 的 30s idle_timeout**（由单测断言）。
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// 长度前缀字节数。
const LENGTH_PREFIX: usize = 4;

/// 多路复用通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Lane {
    /// 控制（握手、心跳、显示列表、结束）。
    Control = 0,
    /// 画面。
    Screen = 1,
    /// 文件（阶段 2）。
    File = 2,
}

impl Lane {
    /// 取线上字节值。
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// 从线上字节值解析；未知值返回 `None`。
    #[must_use]
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Control),
            1 => Some(Self::Screen),
            2 => Some(Self::File),
            _ => None,
        }
    }
}

/// 会话消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionMessage {
    /// 控制器发起握手。
    Hello {
        /// 会话 ID（服务端签发）。
        session_id: String,
        /// 协议版本。
        proto_version: u16,
        /// 控制器 RustDesk ID。
        controller_rd_id: String,
        /// 随机 nonce（防重放）。
        nonce: Vec<u8>,
        /// `HMAC-SHA256(token, session_id ‖ controller_rd_id ‖ nonce)`。
        hmac: Vec<u8>,
        /// 控制器的 X25519 临时公钥（32 字节）。
        x25519_pub: Vec<u8>,
    },
    /// 被控端回应握手。
    HelloAck {
        /// 是否接受。
        accepted: bool,
        /// 拒绝原因；接受时为 `None`。
        reason: Option<String>,
        /// 被控端显示器列表。
        displays: Vec<DisplayInfo>,
        /// 被控端的 X25519 临时公钥（32 字节）。
        x25519_pub: Vec<u8>,
        /// `HMAC-SHA256(token, session_id ‖ responder_pub ‖ initiator_pub)`，证明被控端持有 token
        /// 并绑定自己的临时公钥（防中继方替换 `x25519_pub`）。
        hmac_ack: Vec<u8>,
    },
    /// 一帧画面（JPEG）。
    Video {
        /// 来源显示器编号。
        display: u32,
        /// 编码宽度。
        width: u32,
        /// 编码高度。
        height: u32,
        /// 帧序号（被控端递增）。
        seq: u64,
        /// JPEG 字节。
        jpeg: Vec<u8>,
    },
    /// 一条输入事件。
    Input(InputEvent),
    /// 显示器列表（会话中变更时下发）。
    DisplayList(Vec<DisplayInfo>),
    /// 心跳。
    Heartbeat,
    /// 心跳应答。
    HeartbeatAck,
    /// 会话结束。
    Bye {
        /// 结束原因。
        reason: String,
    },
}

/// 一个带 `lane` 的会话帧。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionFrame {
    /// 多路复用通道。
    pub lane: Lane,
    /// 消息体。
    pub message: SessionMessage,
}

impl SessionFrame {
    /// 构造一个 `Control` 通道帧。
    #[must_use]
    pub fn control(message: SessionMessage) -> Self {
        Self {
            lane: Lane::Control,
            message,
        }
    }

    /// 构造一个 `Screen` 通道帧。
    #[must_use]
    pub fn screen(message: SessionMessage) -> Self {
        Self {
            lane: Lane::Screen,
            message,
        }
    }
}

/// 会话帧编解码器。
///
/// 未装 [`Cipher`] 时收发明文载荷；[`SessionCodec::set_cipher`] 之后收发 AEAD 密文载荷。
/// 握手帧必须明文收发，因此加密在 `HelloAck` 之后才装上。
#[derive(Debug)]
pub struct SessionCodec {
    /// 单帧长度上限。
    max_frame: usize,
    /// 载荷加密器；`None` 为明文。
    cipher: Option<Cipher>,
}

impl SessionCodec {
    /// 构造一个明文编解码器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_frame: MAX_SESSION_FRAME,
            cipher: None,
        }
    }

    /// 装上加密器（握手完成后调用）。
    pub fn set_cipher(&mut self, cipher: Cipher) {
        self.cipher = Some(cipher);
    }

    /// 当前是否已加密。
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// 当前单帧上限。
    #[must_use]
    pub fn max_frame(&self) -> usize {
        self.max_frame
    }

    /// 收紧单帧上限（对端不可信时限制内存）。
    pub fn set_max_frame(&mut self, max_frame: usize) {
        self.max_frame = max_frame;
    }

    /// 把一个 [`SessionFrame`] 编码成完整线上字节（含长度前缀），便于流水线写出。
    ///
    /// # Errors
    /// bincode 编码失败或载荷超过上限时返回 [`RemoteError::Codec`]。
    pub fn encode_frame_bytes(&mut self, frame: &SessionFrame) -> RemoteResult<Vec<u8>> {
        let mut buf = BytesMut::new();
        self.encode(frame.clone(), &mut buf)
            .map_err(|err| RemoteError::Codec(err.to_string()))?;
        Ok(buf.to_vec())
    }
}

impl Default for SessionCodec {
    fn default() -> Self {
        Self::new()
    }
}

/// 构造一个 IO 层的非法数据错误。
fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl Decoder for SessionCodec {
    type Item = SessionFrame;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<SessionFrame>, io::Error> {
        if src.len() < LENGTH_PREFIX {
            return Ok(None);
        }
        let len_u32 = u32::from_be_bytes(
            src[..LENGTH_PREFIX]
                .try_into()
                .map_err(|_| invalid("长度前缀读取失败"))?,
        );
        let len = usize::try_from(len_u32).map_err(|_| invalid("长度前缀溢出"))?;
        if len < 1 {
            return Err(invalid("会话帧长度非法（缺少 lane 字节）"));
        }
        if len > self.max_frame {
            return Err(invalid(format!(
                "会话帧长度 {len} 超过上限 {}",
                self.max_frame
            )));
        }
        if src.len() < LENGTH_PREFIX + len {
            // 半包：等待更多数据。
            src.reserve(LENGTH_PREFIX + len - src.len());
            return Ok(None);
        }
        src.advance(LENGTH_PREFIX);
        let mut body = src.split_to(len);
        let lane = Lane::from_u8(body[0]).ok_or_else(|| invalid("未知 lane 值"))?;
        body.advance(1);

        let plaintext = match self.cipher.as_mut() {
            Some(cipher) => cipher
                .open(&body)
                .map_err(|err| invalid(err.to_string()))?,
            None => body.to_vec(),
        };
        let message: SessionMessage = bincode::deserialize(&plaintext)
            .map_err(|err| invalid(format!("bincode 解码失败: {err}")))?;
        Ok(Some(SessionFrame { lane, message }))
    }
}

impl Encoder<SessionFrame> for SessionCodec {
    type Error = io::Error;

    fn encode(&mut self, item: SessionFrame, dst: &mut BytesMut) -> Result<(), io::Error> {
        let payload = bincode::serialize(&item.message)
            .map_err(|err| invalid(format!("bincode 编码失败: {err}")))?;
        let payload = match self.cipher.as_mut() {
            Some(cipher) => cipher.seal(&payload).map_err(|err| invalid(err.to_string()))?,
            None => payload,
        };
        let len = payload
            .len()
            .checked_add(1)
            .ok_or_else(|| invalid("载荷长度溢出"))?;
        if len > self.max_frame {
            return Err(invalid(format!("会话帧长度 {len} 超过上限 {}", self.max_frame)));
        }
        let len_u32 = u32::try_from(len).map_err(|_| invalid("会话帧长度超出 u32"))?;
        dst.reserve(LENGTH_PREFIX + len);
        dst.put_u32(len_u32);
        dst.put_u8(item.lane.as_u8());
        dst.put_slice(&payload);
        Ok(())
    }
}

/// 控制器侧握手所需的预计算材料。
///
/// 在建立传输**之前**构造：`hello_frame` 会与中继的 `RequestRelay` 帧写在同一次 write
/// （流水线），配对一建立对端立刻拿到 `Hello`，省一个 RTT。会话循环随后只等 `HelloAck`。
pub struct ControllerHandshake {
    /// 控制器临时密钥对（收到 `HelloAck` 后用于 ECDH）。
    pub keypair: EphemeralKeyPair,
    /// 本次握手的随机 nonce。
    pub nonce: Vec<u8>,
    /// 服务端下发的一次性 token：既用于校验 `HelloAck` 的 MAC，也作为密钥派生的认证输入。
    pub token: String,
    /// 预编码的 `Hello` 帧线上字节。
    pub hello_frame: Vec<u8>,
}

impl std::fmt::Debug for ControllerHandshake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerHandshake")
            .field("nonce_len", &self.nonce.len())
            .field("hello_frame_len", &self.hello_frame.len())
            .finish_non_exhaustive()
    }
}

/// 构造控制器握手材料（生成临时密钥、nonce、MAC 与 `Hello` 帧）。
///
/// # Errors
/// `Hello` 帧编码失败时返回 [`RemoteError::Codec`]。
pub fn plan_controller_handshake(
    session_id: &str,
    token: &str,
    controller_rd_id: &str,
) -> RemoteResult<ControllerHandshake> {
    let keypair = EphemeralKeyPair::generate();
    let nonce = random_nonce().to_vec();
    let x25519_pub = keypair.public_bytes().to_vec();
    // MAC 覆盖临时公钥：否则中继方可替换它而不被发现。
    let hmac = handshake_mac(token, session_id, controller_rd_id, &nonce, &x25519_pub);
    let message = SessionMessage::Hello {
        session_id: session_id.to_owned(),
        proto_version: SESSION_PROTO_VERSION,
        controller_rd_id: controller_rd_id.to_owned(),
        nonce: nonce.clone(),
        hmac,
        x25519_pub,
    };
    let mut codec = SessionCodec::new();
    let hello_frame = codec.encode_frame_bytes(&SessionFrame::control(message))?;
    Ok(ControllerHandshake {
        keypair,
        nonce,
        token: token.to_owned(),
        hello_frame,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{MouseButton, KeyEvent};

    fn codec_roundtrip(message: SessionMessage, lane: Lane) -> SessionFrame {
        let mut enc = SessionCodec::new();
        let mut wire = BytesMut::new();
        enc.encode(SessionFrame { lane, message }, &mut wire).unwrap();

        let mut dec = SessionCodec::new();
        dec.decode(&mut wire).unwrap().expect("应解出一帧")
    }

    #[test]
    fn heartbeat_interval_is_below_relay_idle_timeout() {
        // hbbr 的 idle_timeout 是 30s（relay.rs::RelayConfig::default）。
        assert!(
            HEARTBEAT_INTERVAL < Duration::from_secs(30),
            "心跳间隔必须严格小于中继空闲超时，否则静止会话会被断开"
        );
    }

    #[test]
    fn messages_roundtrip() {
        let cases = vec![
            (
                SessionMessage::Hello {
                    session_id: "s1".into(),
                    proto_version: SESSION_PROTO_VERSION,
                    controller_rd_id: "123456789".into(),
                    nonce: vec![1, 2, 3],
                    hmac: vec![9; 32],
                    x25519_pub: vec![7; 32],
                },
                Lane::Control,
            ),
            (
                SessionMessage::HelloAck {
                    accepted: true,
                    reason: None,
                    displays: vec![DisplayInfo {
                        id: 0,
                        width: 1920,
                        height: 1080,
                        name: "main".into(),
                        primary: true,
                    }],
                    x25519_pub: vec![8; 32],
                    hmac_ack: vec![9; 32],
                },
                Lane::Control,
            ),
            (
                SessionMessage::Video {
                    display: 0,
                    width: 640,
                    height: 480,
                    seq: 42,
                    jpeg: vec![0xFF, 0xD8, 0xFF],
                },
                Lane::Screen,
            ),
            (
                SessionMessage::Input(InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                }),
                Lane::Control,
            ),
            (SessionMessage::Heartbeat, Lane::Control),
            (SessionMessage::HeartbeatAck, Lane::Control),
            (
                SessionMessage::Bye {
                    reason: "done".into(),
                },
                Lane::Control,
            ),
        ];
        for (message, lane) in cases {
            let got = codec_roundtrip(message.clone(), lane);
            assert_eq!(got.lane, lane, "lane 应保持");
            assert_eq!(got.message, message, "消息应往返一致");
        }
    }

    #[test]
    fn key_event_roundtrips_through_input() {
        let ev = InputEvent::Key(KeyEvent {
            code: 65,
            pressed: false,
            text: Some("A".into()),
            modifiers: 0b001,
        });
        let got = codec_roundtrip(SessionMessage::Input(ev.clone()), Lane::Control);
        assert_eq!(got.message, SessionMessage::Input(ev));
    }

    #[test]
    fn oversize_frame_is_rejected_on_encode_and_decode() {
        // 编码侧：载荷超过上限。
        let mut enc = SessionCodec::new();
        enc.set_max_frame(8);
        let mut wire = BytesMut::new();
        let big = SessionMessage::Video {
            display: 0,
            width: 1,
            height: 1,
            seq: 0,
            jpeg: vec![0u8; 64],
        };
        assert!(enc.encode(SessionFrame::screen(big), &mut wire).is_err());

        // 解码侧：伪造一个声明超大长度的头。
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32(u32::try_from(MAX_SESSION_FRAME + 1).unwrap());
        buf.put_u8(0);
        assert!(dec.decode(&mut buf).is_err(), "超限长度应被拒绝");
    }

    #[test]
    fn zero_length_frame_is_rejected() {
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32(0);
        assert!(dec.decode(&mut buf).is_err(), "长度为 0 缺少 lane，应拒绝");
    }

    #[test]
    fn invalid_lane_is_rejected() {
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        // 长度 2：1 字节 lane + 1 字节垃圾载荷。
        buf.put_u32(2);
        buf.put_u8(0xFF);
        buf.put_u8(0x00);
        assert!(dec.decode(&mut buf).is_err(), "未知 lane 应拒绝");
    }

    #[test]
    fn malformed_payload_is_rejected() {
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        // 合法 lane、非法 bincode 载荷。
        buf.put_u32(4);
        buf.put_u8(0);
        buf.put_slice(&[0xFF, 0xFF, 0xFF]);
        assert!(dec.decode(&mut buf).is_err(), "非法 bincode 应拒绝");
    }

    #[test]
    fn split_delivery_reassembles() {
        let mut enc = SessionCodec::new();
        let mut wire = BytesMut::new();
        enc.encode(
            SessionFrame::control(SessionMessage::Heartbeat),
            &mut wire,
        )
        .unwrap();
        let tail = wire.split_off(2);

        let mut dec = SessionCodec::new();
        assert!(dec.decode(&mut wire).unwrap().is_none(), "半包应返回 None");
        wire.extend_from_slice(&tail);
        assert_eq!(
            dec.decode(&mut wire).unwrap().unwrap().message,
            SessionMessage::Heartbeat
        );
    }

    #[test]
    fn encrypted_frames_roundtrip_and_differ_from_plaintext() {
        let key = [5u8; 32];
        let mut enc = SessionCodec::new();
        enc.set_cipher(Cipher::new(key).unwrap());
        assert!(enc.is_encrypted());
        let mut wire = BytesMut::new();
        enc.encode(
            SessionFrame::control(SessionMessage::Bye { reason: "x".into() }),
            &mut wire,
        )
        .unwrap();

        // 明文编解码器读密文应失败（证明真的加密了）。
        let mut plain = SessionCodec::new();
        let mut copy = wire.clone();
        assert!(plain.decode(&mut copy).is_err());

        // 正确密钥可解。
        let mut dec = SessionCodec::new();
        dec.set_cipher(Cipher::new(key).unwrap());
        let got = dec.decode(&mut wire).unwrap().unwrap();
        assert_eq!(got.message, SessionMessage::Bye { reason: "x".into() });
    }

    #[test]
    fn plan_controller_handshake_encodes_a_hello_frame() {
        let plan = plan_controller_handshake("sess-1", "tok", "123456789").unwrap();
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::from(&plan.hello_frame[..]);
        let frame = dec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.lane, Lane::Control);
        match frame.message {
            SessionMessage::Hello {
                session_id,
                proto_version,
                controller_rd_id,
                x25519_pub,
                ..
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(proto_version, SESSION_PROTO_VERSION);
                assert_eq!(controller_rd_id, "123456789");
                assert_eq!(x25519_pub.len(), 32);
            }
            other => panic!("应为 Hello，实得 {other:?}"),
        }
    }
}
