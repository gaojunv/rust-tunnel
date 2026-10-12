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
use crate::screen::{DisplayInfo, FileEntry, InputEvent, QualityPreset, TilePatch};
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::io;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::codec::{Decoder, Encoder};

/// 会话协议版本；不匹配的 `Hello` 一律拒绝。
pub const SESSION_PROTO_VERSION: u16 = 1;

/// 单帧载荷上限（16 MiB）。
pub const MAX_SESSION_FRAME: usize = 16 * 1024 * 1024;

/// 心跳间隔。**必须小于 hbbr 的 30s idle_timeout**（由单测断言）。
///
/// # 为什么延迟读数最快也要 10s 一次
///
/// 会话循环用这条心跳的**往返**（`Heartbeat` → `HeartbeatAck`）测延迟，因此最快每
/// [`HEARTBEAT_INTERVAL`] 才有一个样本。更快的探测需要新增数据面变体（`SessionMessage`
/// 的硬相等版本校验会让混合版本的一端 bincode 解码失败而直接断链），代价远大于收益，故有意不加快。
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// 由「发出时刻」与「收到应答时刻」算一次可信的 RTT 样本。
///
/// 丢弃 `>= HEARTBEAT_INTERVAL` 的样本：心跳是**零载荷**的固定往返，正常 RTT 必然远小于间隔；
/// 一旦读数达到间隔量级，只可能是把一个迟到的 `HeartbeatAck` 与另一次（或已作废的）`Heartbeat`
/// 配了对，或对端/链路卡了整整一个周期——两者都不可信，报给 UI 只会误导。抽成纯函数以便单测。
#[must_use]
pub fn trusted_rtt(sent: Instant, now: Instant) -> Option<Duration> {
    let rtt = now.saturating_duration_since(sent);
    (rtt < HEARTBEAT_INTERVAL).then_some(rtt)
}

/// 一次会话的结束信息：会话循环的返回值，也是管理器判定的唯一依据。
///
/// 放在本模块（而非 `manager`）是因为 `controller` / `controlled` 两个循环都要**返回**它；
/// 若定义在 `manager` 就成了「循环依赖管理器、管理器依赖循环」的互相引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnd {
    /// 面向用户的中文原因描述。
    pub reason: String,
    /// 是否**主动**结束（本端用户中断，或收到对端 `Bye`）。
    /// `false` 表示连接是断的/失败的——这是断线自动重连的唯一判据。
    pub deliberate: bool,
}

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
    /// 一帧画面（JPEG，整帧）。
    ///
    /// 首帧与关键帧走这里；两次关键帧之间只发 [`SessionMessage::VideoTiles`]。
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
    /// 增量瓦片更新（只含变化区域）。
    ///
    /// 收到它的控制器**应当**做局部更新而不是整帧替换；忽略它也能工作，只是要等下一个关键帧
    /// （[`crate::tiles::KEYFRAME_INTERVAL_FRAMES`] 帧之内）才看到变化。
    VideoTiles {
        /// 来源显示器编号。
        display: u32,
        /// 整帧宽（瓦片坐标以此为参照）。
        width: u32,
        /// 整帧高。
        height: u32,
        /// 帧序号（被控端递增，与 `Video` 共用同一计数器）。
        seq: u64,
        /// 变化的瓦片。
        tiles: Vec<TilePatch>,
    },
    /// 一条输入事件。
    Input(InputEvent),
    /// 控制器切换观看的显示器（被控端据此换采集目标）。
    SelectDisplay {
        /// 目标显示器编号（取自 `HelloAck.displays`）。
        display: u32,
    },
    /// 控制器下发画质档位（被控端据此换编码参数并强制重发关键帧）。
    SetQuality {
        /// 目标档位。
        preset: QualityPreset,
    },
    /// 剪贴板文本（双向）。
    Clipboard {
        /// 文本内容。
        text: String,
    },
    /// 请求对端把剪贴板内容发过来（双向）。
    ClipboardRequest,
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
    // ── 文件传输（走 `File` lane；`id` 由发起方分配，两端各自独立编号空间）──
    /// 请求列目录（`path` 为共享根下的虚拟路径，`""` = 根）。
    FileListRequest {
        /// 请求 ID。
        id: u64,
        /// 虚拟路径。
        path: String,
    },
    /// 列目录结果。
    FileList {
        /// 请求 ID。
        id: u64,
        /// 回显虚拟路径。
        path: String,
        /// 目录项。
        entries: Vec<FileEntry>,
    },
    /// 请求下载（被控端 → 控制器方向的文件）。
    FileDownloadRequest {
        /// 传输 ID。
        id: u64,
        /// 虚拟路径。
        path: String,
    },
    /// 请求上传（控制器 → 被控端方向的文件）。
    FileUploadRequest {
        /// 传输 ID。
        id: u64,
        /// 虚拟路径（含目标文件名）。
        path: String,
        /// 声明字节数（被控端据此预检限额）。
        size: u64,
    },
    /// 传输受理/拒绝。
    FileAccept {
        /// 传输 ID。
        id: u64,
        /// 是否受理。
        accepted: bool,
        /// 拒绝原因；受理时为 `None`。
        reason: Option<String>,
        /// 受理方回填的实际字节数（下载时由被控端给出；上传时为 0）。
        size: u64,
    },
    /// 数据块（按序追加；单条 TCP 流不会乱序，故无需 offset）。
    FileChunk {
        /// 传输 ID。
        id: u64,
        /// 数据。
        data: Vec<u8>,
    },
    /// 传输完成。
    FileDone {
        /// 传输 ID。
        id: u64,
    },
    /// 传输失败/取消（任一端均可发）。
    FileError {
        /// 传输 ID。
        id: u64,
        /// 原因。
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

    /// 构造一个 `File` 通道帧。
    #[must_use]
    pub fn file(message: SessionMessage) -> Self {
        Self {
            lane: Lane::File,
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
    use crate::screen::{FileEntry, KeyEvent, MouseButton, QualityPreset, TilePatch};

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
    fn trusted_rtt_keeps_fast_samples_and_drops_slow_ones() {
        let base = Instant::now();
        // 正常往返：远小于间隔，应采信。
        assert_eq!(
            trusted_rtt(base, base + Duration::from_millis(5)),
            Some(Duration::from_millis(5))
        );
        // 恰好等于间隔、以及超过间隔：样本不可信，丢弃（只可能是丢包/错配）。
        assert_eq!(trusted_rtt(base, base + HEARTBEAT_INTERVAL), None);
        assert_eq!(
            trusted_rtt(base, base + HEARTBEAT_INTERVAL + Duration::from_millis(1)),
            None
        );
        // 时钟回拨/负差不应 panic，按 0 处理（仍是可信样本）。
        assert_eq!(trusted_rtt(base, base), Some(Duration::ZERO));
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

    /// 造一个显示器的元信息。
    fn display(id: u32) -> DisplayInfo {
        DisplayInfo {
            id,
            width: 1280,
            height: 720,
            name: format!("d{id}"),
            primary: id == 0,
        }
    }

    /// 造一个目录项。
    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            path: format!("/{name}"),
            is_dir: false,
            size: 4096,
            modified_ms: 1_700_000_000_000,
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "枚举全部变体平铺成一张表，拆开反而看不出覆盖面"
    )]
    fn all_session_variants_roundtrip() {
        // 锁的不变量：**每一个** `SessionMessage` 变体都能 bincode 编解码往返，且 lane 保持。
        //
        // 这是数据面的线上契约：`messages_roundtrip` 只覆盖了约 6 个变体，全部 `File*`、
        // `VideoTiles`、`SetQuality`、`SelectDisplay`、`Clipboard`、`ClipboardRequest`、
        // `DisplayList`、`HeartbeatAck` 从未被序列化过。某个变体的字段一旦漂移（改类型、
        // 调字段序），只有真机联调才会炸——这条用例把契约钉死在单测里。
        // 每个变体都带上非空与边界内容（空 tiles、空 entries、空 data、空字符串路径）。
        let cases: Vec<(SessionMessage, Lane)> = vec![
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
                    accepted: false,
                    reason: Some("拒绝".into()),
                    displays: vec![],
                    x25519_pub: vec![],
                    hmac_ack: vec![],
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
                SessionMessage::VideoTiles {
                    display: 1,
                    width: 192,
                    height: 128,
                    seq: 7,
                    tiles: vec![
                        TilePatch {
                            x: 0,
                            y: 0,
                            width: 64,
                            height: 64,
                            jpeg: vec![0xAB; 8],
                        },
                        TilePatch {
                            x: 128,
                            y: 64,
                            width: 64,
                            height: 64,
                            jpeg: vec![],
                        },
                    ],
                },
                Lane::Screen,
            ),
            // 边界：空瓦片批次（静止画面之外的空更新）。
            (
                SessionMessage::VideoTiles {
                    display: 0,
                    width: 64,
                    height: 64,
                    seq: 0,
                    tiles: vec![],
                },
                Lane::Screen,
            ),
            (
                SessionMessage::Input(InputEvent::MouseMove {
                    x: -5,
                    y: 100,
                    display: 2,
                }),
                Lane::Control,
            ),
            (
                SessionMessage::Input(InputEvent::MouseButton {
                    button: MouseButton::Middle,
                    pressed: false,
                }),
                Lane::Control,
            ),
            (
                SessionMessage::Input(InputEvent::Wheel { dx: -3, dy: 9 }),
                Lane::Control,
            ),
            (
                SessionMessage::Input(InputEvent::Key(KeyEvent {
                    code: 27,
                    pressed: true,
                    text: None,
                    modifiers: 0b1010,
                })),
                Lane::Control,
            ),
            (SessionMessage::SelectDisplay { display: 1 }, Lane::Control),
            (SessionMessage::SetQuality { preset: QualityPreset::Hd }, Lane::Control),
            (SessionMessage::Clipboard { text: "剪贴板文本".into() }, Lane::Control),
            // 边界：空剪贴板文本。
            (SessionMessage::Clipboard { text: String::new() }, Lane::Control),
            (SessionMessage::ClipboardRequest, Lane::Control),
            (SessionMessage::DisplayList(vec![display(0), display(1)]), Lane::Control),
            // 边界：显示器列表为空（无显示器）。
            (SessionMessage::DisplayList(vec![]), Lane::Control),
            (SessionMessage::Heartbeat, Lane::Control),
            (SessionMessage::HeartbeatAck, Lane::Control),
            // 边界：空结束原因。
            (SessionMessage::Bye { reason: String::new() }, Lane::Control),
            // 边界：根路径（`""` = 共享根）。
            (SessionMessage::FileListRequest { id: 1, path: String::new() }, Lane::File),
            (
                SessionMessage::FileList {
                    id: 1,
                    path: "/docs".into(),
                    entries: vec![entry("a.txt"), entry("b.bin")],
                },
                Lane::File,
            ),
            // 边界：空目录。
            (SessionMessage::FileList { id: 2, path: "/empty".into(), entries: vec![] }, Lane::File),
            (SessionMessage::FileDownloadRequest { id: 3, path: "/a.bin".into() }, Lane::File),
            // 边界：声明字节数为 u64 上限。
            (
                SessionMessage::FileUploadRequest {
                    id: 4,
                    path: "/big.bin".into(),
                    size: u64::MAX,
                },
                Lane::File,
            ),
            (
                SessionMessage::FileAccept {
                    id: 5,
                    accepted: true,
                    reason: None,
                    size: 200 * 1024,
                },
                Lane::File,
            ),
            (
                SessionMessage::FileAccept {
                    id: 6,
                    accepted: false,
                    reason: Some("被控端未开启文件共享".into()),
                    size: 0,
                },
                Lane::File,
            ),
            (SessionMessage::FileChunk { id: 7, data: vec![0xCD; 4096] }, Lane::File),
            // 边界：空数据块。
            (SessionMessage::FileChunk { id: 8, data: vec![] }, Lane::File),
            (SessionMessage::FileDone { id: 9 }, Lane::File),
            (SessionMessage::FileError { id: 10, reason: "用户取消".into() }, Lane::File),
        ];
        for (message, lane) in cases {
            let got = codec_roundtrip(message.clone(), lane);
            assert_eq!(got.lane, lane, "lane 应保持：{message:?}");
            assert_eq!(got.message, message, "消息应往返一致");
        }
    }

    #[test]
    fn unknown_session_variant_is_rejected() {
        // 锁的不变量：数据面收到**未知的 `SessionMessage` 判别式**（lane 合法、长度合法、
        // 但枚举序号越界）必须报错，既不能静默接受也不能 panic。
        //
        // 这是数据面的「版本门控等价物」：控制通道有 `client_supports_remote` 挡住老客户端，
        // 数据面没有——版本漂移最可能的表现就是发来一个本端不认识的新变体，必须干净地拒绝。
        // bincode 1.3 默认 fixint 编码：枚举判别式是 u32 小端（见 `bincode::serialize` 的
        // `DefaultOptions::new().with_fixint_encoding()`），故越界值用 u32 构造。
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        let payload = u32::MAX.to_le_bytes();
        // 长度 = 1 字节 lane + 载荷长度。
        buf.put_u32(u32::try_from(1 + payload.len()).unwrap());
        buf.put_u8(Lane::Control.as_u8());
        buf.put_slice(&payload);
        assert!(dec.decode(&mut buf).is_err(), "越界的枚举判别式必须被拒绝");
    }

    #[test]
    fn short_length_prefix_awaits_more_data() {
        // 锁的不变量：长度前缀不足 4 字节时必须返回 `Ok(None)`（半包，等待更多数据），
        // 而不是报错或 panic——读循环据此保留缓冲、下次读齐了再解。
        for n in 0..LENGTH_PREFIX {
            let mut dec = SessionCodec::new();
            let mut buf = BytesMut::from(&vec![0u8; n][..]);
            assert!(
                dec.decode(&mut buf).unwrap().is_none(),
                "长度前缀只有 {n} 字节时应为半包（Ok(None)）"
            );
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
