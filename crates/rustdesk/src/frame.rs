//! RustDesk 线上帧格式（长度前缀）编解码。
//!
//! RustDesk 的控制消息不是定长 4 字节长度前缀，而是**可变长头部**：头部首字节的
//! 低 2 位编码「头部总字节数 - 1」，长度值整体左移 2 位存放（低 2 位让给头宽标记）。
//! 等价描述：
//!
//! | 载荷长度区间 | 头长 | 头部内容（小端） |
//! |---|---|---|
//! | `0 ..= 0x3F` | 1 | `len << 2` |
//! | `0x40 ..= 0x3FFF` | 2 | `(len << 2) \| 1` |
//! | `0x4000 ..= 0x3F_FFFF` | 3 | `(len << 2) \| 2`，低 3 字节小端 |
//! | `0x40_0000 ..= 0x3FFF_FFFF` | 4 | `(len << 2) \| 3`，小端 u32 |
//!
//! 解码方向：`head_len = (b0 & 0x3) + 1`，取前 `head_len` 字节按小端拼成整数后 `>> 2`
//! 即得载荷长度。
//!
//! 配对成功后（中继数据面）两端都切到 [`FrameCodec::set_raw`] 的**裸字节模式**：
//! 不再有任何头部，读到什么转发什么。

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

/// 四字节头部能表达的最大载荷长度（`0x3FFF_FFFF`，约 1 GiB）。
pub const MAX_FRAME_LENGTH: usize = 0x3FFF_FFFF;

/// 头部按声明长度预分配内存时的上限。
///
/// 对端可以声明一个巨大帧却迟迟不发载荷，若照声明值 `reserve` 会放大成内存放大攻击；
/// 因此预分配封顶，超出的部分随实际读取增长。
const MAX_PREALLOCATED_PAYLOAD_LEN: usize = 256 * 1024;

/// 帧编解码错误。
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum FrameError {
    /// 载荷超过 [`MAX_FRAME_LENGTH`]，无法用 4 字节头部表达。
    #[error("frame payload too large: {0} bytes (max {MAX_FRAME_LENGTH})")]
    TooLarge(usize),
    /// 头部声明的长度超过本端配置的上限。
    #[error("declared frame length {0} exceeds configured maximum {1}")]
    ExceedsMaximum(usize, usize),
}

/// 把长度左移 2 位得到头部数值（低 2 位留给头宽标记）。
///
/// 入参已由调用方确认不超过 [`MAX_FRAME_LENGTH`]，故 `u32` 一定装得下。
fn head_value(len: usize) -> Result<u32, FrameError> {
    u32::try_from(len)
        .ok()
        .and_then(|v| v.checked_shl(2))
        .ok_or(FrameError::TooLarge(len))
}

/// 收窄位宽，失败即视为长度非法（正常路径下不可能触发）。
fn narrow<T: TryFrom<u32>>(v: u32, len: usize) -> Result<T, FrameError> {
    T::try_from(v).map_err(|_| FrameError::TooLarge(len))
}

/// 把头部写入 `buf`，返回写入的头部字节数。
///
/// 单独抽出便于对四种头宽做边界单测，不依赖完整 codec 状态机。
///
/// # Errors
/// 载荷长度超过 [`MAX_FRAME_LENGTH`] 时返回 [`FrameError::TooLarge`]。
pub fn encode_header(len: usize, buf: &mut BytesMut) -> Result<usize, FrameError> {
    let h = head_value(len)?;
    if len <= 0x3F {
        // 低 2 位为 0 → 头长 1。
        buf.put_u8(narrow::<u8>(h, len)?);
        Ok(1)
    } else if len <= 0x3FFF {
        // 低 2 位为 1 → 头长 2。
        buf.put_u16_le(narrow::<u16>(h | 0x1, len)?);
        Ok(2)
    } else if len <= 0x3F_FFFF {
        // 低 2 位为 2 → 头长 3：低 16 位 + 第 3 字节。
        let h = h | 0x2;
        buf.put_u16_le(narrow::<u16>(h & 0xFFFF, len)?);
        buf.put_u8(narrow::<u8>(h >> 16, len)?);
        Ok(3)
    } else if len <= MAX_FRAME_LENGTH {
        // 低 2 位为 3 → 头长 4。
        buf.put_u32_le(h | 0x3);
        Ok(4)
    } else {
        Err(FrameError::TooLarge(len))
    }
}

/// 从 `buf` 头部解析载荷长度；字节不足一个完整头部时返回 `Ok(None)`。
///
/// 解析成功会把头部字节 `advance` 掉，并按（封顶后的）长度 `reserve` 载荷空间。
///
/// # Errors
/// 声明的长度超过 [`MAX_FRAME_LENGTH`]（头部本身已损坏）时返回 [`FrameError::TooLarge`]。
pub fn decode_header(buf: &mut BytesMut) -> Result<Option<usize>, FrameError> {
    if buf.is_empty() {
        return Ok(None);
    }
    let first = buf[0];
    let head_len = usize::from(first & 0x3) + 1;
    if buf.len() < head_len {
        return Ok(None);
    }
    let mut head = u32::from(first);
    for i in 1..head_len {
        head |= u32::from(buf[i]) << (8 * i);
    }
    let len = usize::try_from(head >> 2).unwrap_or(usize::MAX);
    if len > MAX_FRAME_LENGTH {
        return Err(FrameError::TooLarge(len));
    }
    buf.advance(head_len);
    buf.reserve(
        len.saturating_sub(buf.len())
            .min(MAX_PREALLOCATED_PAYLOAD_LEN),
    );
    Ok(Some(len))
}

/// 把一个载荷整帧（头部 + 载荷）追加到 `buf`。
///
/// # Errors
/// 载荷超过 [`MAX_FRAME_LENGTH`] 时返回 [`FrameError::TooLarge`]。
pub fn encode_frame(data: &[u8], buf: &mut BytesMut) -> Result<(), FrameError> {
    encode_header(data.len(), buf)?;
    buf.extend_from_slice(data);
    Ok(())
}

/// 把单个载荷编码为独立的新 [`BytesMut`]。
///
/// # Errors
/// 载荷超过 [`MAX_FRAME_LENGTH`] 时返回 [`FrameError::TooLarge`]。
pub fn frame(data: &[u8]) -> Result<BytesMut, FrameError> {
    let mut buf = BytesMut::with_capacity(data.len() + 4);
    encode_frame(data, &mut buf)?;
    Ok(buf)
}

/// 控制消息的帧编解码器。
///
/// 默认状态解析长度前缀；调用 [`FrameCodec::set_raw`] 后进入裸字节模式——此时每个
/// `decode` 直接把缓冲区现有内容整体交给上层，`encode` 也不写头部。中继数据面
/// 配对完成后必须切到裸模式，与上游行为一致。
#[derive(Debug, Clone, Copy)]
pub struct FrameCodec {
    state: DecodeState,
    raw: bool,
    max_packet_length: usize,
}

#[derive(Debug, Clone, Copy)]
enum DecodeState {
    Head,
    Data(usize),
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameCodec {
    /// 构造一个长度前缀模式的编解码器（长度上限为 [`MAX_FRAME_LENGTH`]）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: DecodeState::Head,
            raw: false,
            max_packet_length: MAX_FRAME_LENGTH,
        }
    }

    /// 切换到裸字节模式，不可逆。
    pub fn set_raw(&mut self) {
        self.raw = true;
    }

    /// 当前是否处于裸字节模式。
    #[must_use]
    pub fn is_raw(&self) -> bool {
        self.raw
    }

    /// 收紧可接受的单帧长度上限（用于对端不可信时限制内存）。
    pub fn set_max_packet_length(&mut self, n: usize) {
        self.max_packet_length = n;
    }
}

impl Decoder for FrameCodec {
    type Item = BytesMut;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<BytesMut>, io::Error> {
        if self.raw {
            return Ok(if src.is_empty() {
                None
            } else {
                let len = src.len();
                Some(src.split_to(len))
            });
        }
        let n = match self.state {
            DecodeState::Head => match decode_header(src).map_err(to_io)? {
                Some(n) => n,
                None => return Ok(None),
            },
            DecodeState::Data(n) => n,
        };
        // 先校验再改状态：超限时连接会被上层丢弃，不留半截状态。
        if n > self.max_packet_length {
            return Err(to_io(FrameError::ExceedsMaximum(n, self.max_packet_length)));
        }
        self.state = DecodeState::Data(n);
        if src.len() < n {
            return Ok(None);
        }
        self.state = DecodeState::Head;
        Ok(Some(src.split_to(n)))
    }
}

impl Encoder<Bytes> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, data: Bytes, buf: &mut BytesMut) -> Result<(), io::Error> {
        if self.raw {
            buf.reserve(data.len());
            buf.put(data);
            return Ok(());
        }
        encode_frame(&data[..], buf).map_err(to_io)
    }
}

fn to_io(err: FrameError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 头宽切换的四个边界值（含上界与下界）。
    const BOUNDARIES: &[usize] = &[
        0,
        1,
        0x3F,       // 1 字节头上界
        0x40,       // 2 字节头下界
        0x3FFF,     // 2 字节头上界
        0x4000,     // 3 字节头下界
        0x3F_FFFF,  // 3 字节头上界
        0x40_0000,  // 4 字节头下界
        0x3FFF_FFFF // 4 字节头上界
    ];

    #[test]
    fn header_bytes_match_upstream_layout() {
        // 逐字节断言，任何格式改动都会立刻暴露为互操作破坏。
        let cases: &[(usize, &[u8])] = &[
            (0, &[0x00]),
            (0x3F, &[0xFC]),
            (0x40, &[0x01, 0x01]),
            (0x3FFF, &[0xFD, 0xFF]),
            (0x4000, &[0x02, 0x00, 0x01]),
            (0x3F_FFFF, &[0xFE, 0xFF, 0xFF]),
            (0x40_0000, &[0x03, 0x00, 0x00, 0x01]),
            (0x3FFF_FFFF, &[0xFF, 0xFF, 0xFF, 0xFF]),
        ];
        for (len, expect) in cases {
            let mut buf = BytesMut::new();
            let written = encode_header(*len, &mut buf).unwrap();
            assert_eq!(written, expect.len(), "len={len} 头长不符");
            assert_eq!(&buf[..], *expect, "len={len} 头部字节不符");
        }
    }

    #[test]
    fn header_roundtrips_at_boundaries() {
        for &len in BOUNDARIES {
            let mut buf = BytesMut::new();
            let written = encode_header(len, &mut buf).unwrap();
            assert_eq!(buf.len(), written, "len={len} 头长与返回值不一致");
            assert_eq!(
                decode_header(&mut buf).unwrap(),
                Some(len),
                "len={len} 头部回读不一致"
            );
            assert!(buf.is_empty(), "len={len} 解码后应消耗掉头部");
        }
    }

    #[test]
    fn over_max_length_is_rejected() {
        let mut buf = BytesMut::new();
        assert!(matches!(
            encode_header(MAX_FRAME_LENGTH + 1, &mut buf),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn decode_header_waits_for_partial_input() {
        // 头声明为 2 字节宽度，只喂 1 字节时应返回 None 而不是误判长度。
        let mut buf = BytesMut::new();
        encode_header(0x40, &mut buf).unwrap();
        let mut partial = BytesMut::from(&buf[..1]);
        assert_eq!(decode_header(&mut partial).unwrap(), None);
        partial.extend_from_slice(&buf[1..]);
        assert_eq!(decode_header(&mut partial).unwrap(), Some(0x40));
    }

    #[test]
    fn codec_roundtrips_payloads() {
        let payloads: Vec<Vec<u8>> = vec![
            vec![],
            vec![1],
            vec![7; 0x3F],
            vec![7; 0x40],
            vec![7; 0x4000],
            vec![7; 0x40_0000],
        ];
        for payload in payloads {
            let mut enc = FrameCodec::new();
            let mut wire = BytesMut::new();
            enc.encode(Bytes::from(payload.clone()), &mut wire).unwrap();

            let mut dec = FrameCodec::new();
            let got = dec.decode(&mut wire).unwrap().expect("应解出一帧");
            assert_eq!(&got[..], &payload[..], "载荷 len={}", payload.len());
            assert!(
                wire.is_empty(),
                "载荷 len={} 解码后不应有残留",
                payload.len()
            );
        }
    }

    #[test]
    fn codec_handles_split_delivery() {
        // 同一帧分两次到达（模拟 TCP 半包）：第一次 None，补齐后才出载荷。
        let mut enc = FrameCodec::new();
        let mut wire = BytesMut::new();
        enc.encode(Bytes::from(vec![9u8; 100]), &mut wire).unwrap();

        let mut dec = FrameCodec::new();
        let mut partial = wire.split_to(1);
        assert!(dec.decode(&mut partial).unwrap().is_none());
        partial.extend_from_slice(&wire);
        let got = dec.decode(&mut partial).unwrap().expect("补齐后应解出");
        assert_eq!(got.len(), 100);
    }

    #[test]
    fn codec_handles_two_frames_in_one_read() {
        let mut enc = FrameCodec::new();
        let mut wire = BytesMut::new();
        enc.encode(Bytes::from_static(b"aa"), &mut wire).unwrap();
        enc.encode(Bytes::from_static(b"bbb"), &mut wire).unwrap();

        let mut dec = FrameCodec::new();
        assert_eq!(&dec.decode(&mut wire).unwrap().unwrap()[..], b"aa");
        assert_eq!(&dec.decode(&mut wire).unwrap().unwrap()[..], b"bbb");
        assert!(dec.decode(&mut wire).unwrap().is_none());
    }

    #[test]
    fn raw_mode_passes_bytes_through_untouched() {
        let mut codec = FrameCodec::new();
        codec.set_raw();
        assert!(codec.is_raw());

        // 裸模式下编解码都不带头部，内容原样。
        let mut wire = BytesMut::new();
        codec
            .encode(Bytes::from_static(b"hello"), &mut wire)
            .unwrap();
        assert_eq!(&wire[..], b"hello");

        let out = codec.decode(&mut wire).unwrap().unwrap();
        assert_eq!(&out[..], b"hello");
        assert!(wire.is_empty());
    }

    #[test]
    fn raw_mode_returns_whatever_is_available() {
        // 裸模式不做重组：来多少给多少，由上层（copy_bidirectional 语义）处理。
        let mut codec = FrameCodec::new();
        codec.set_raw();
        let mut wire = BytesMut::from(&b"partial"[..]);
        assert_eq!(&codec.decode(&mut wire).unwrap().unwrap()[..], b"partial");
    }

    #[test]
    fn max_packet_length_is_enforced() {
        let mut enc = FrameCodec::new();
        let mut wire = BytesMut::new();
        enc.encode(Bytes::from(vec![0u8; 5000]), &mut wire).unwrap();

        let mut dec = FrameCodec::new();
        dec.set_max_packet_length(1024);
        let err = dec.decode(&mut wire).expect_err("应因超限报错");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
