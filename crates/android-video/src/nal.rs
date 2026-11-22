//! NAL 层纯逻辑：长度前缀 → Annex B、访问单元拆分、参数集（SPS/PPS/VPS）提取。
//!
//! # 线上格式
//!
//! 与控制端数据面一致：一次 `VideoPacket.data` 是若干「**4 字节大端长度前缀 +
//! 负载**」块的拼接。负载有两种可能形态，本模块统一归一化成 MediaCodec 要求的
//! Annex B（`00 00 00 01` 起始码）：
//!
//! - **Annex B 访问单元**：项目里的 NVENC / MFT 编码器就是这个形态——一帧一个块，
//!   块内是带起始码的多个 NAL（IDR 还内联 SPS/PPS）。块内若已带起始码则原样保留。
//! - **单个裸 NAL**：另一种假设封装，负载首字节是 NAL 头。此时补一个 4 字节起始码。
//!
//! # 为什么用「是否已带起始码」判别而不是硬转
//!
//! 把 `4 字节长度` 无条件替换成 `00 00 00 01` 在真实码流上会产生
//! `00 00 00 01 00 00 00 01 SPS...` 这样的空 NAL，部分解码器会拒绝。判别起始码后
//! 两种形态都能得到干净的 Annex B。
//!
//! # 参数集
//!
//! MediaCodec 的 `csd-0` 是可选增强：我们的编码器保证 IDR 内联参数集（NVENC
//! `repeatSPSPPS=1`），所以即使不填 `csd-0` 也能解。本模块仍提供
//! [`parameter_sets`] 供首帧探测时填入，提升对窄实现的兼容性。

use rust_tunnel_remote::CodecId;

/// Annex B 4 字节起始码。
pub(crate) const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// 视频编码语法（本 crate 只关心 H.264 与 H.265）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Syntax {
    /// H.264 / AVC（`video/avc`）。
    H264,
    /// H.265 / HEVC（`video/hevc`）。
    H265,
}

impl Syntax {
    /// 对应的 MediaCodec MIME，带 NUL 结尾（直接作为 `*const c_char` 传给 FFI）。
    pub(crate) fn mime(self) -> &'static [u8] {
        match self {
            Self::H264 => b"video/avc\0",
            Self::H265 => b"video/hevc\0",
        }
    }

    /// 人类可读标签，用于日志与遥测。
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::H265 => "H.265/HEVC",
        }
    }

    /// 映射回引擎的 [`CodecId`]。
    pub(crate) fn codec_id(self) -> CodecId {
        match self {
            Self::H264 => CodecId::H264,
            Self::H265 => CodecId::H265,
        }
    }

    /// 从 [`CodecId`] 映射；VP9 不是本 crate 支持的语法（返回 `None`）。
    pub(crate) fn from_codec_id(codec: CodecId) -> Option<Self> {
        match codec {
            CodecId::H264 => Some(Self::H264),
            CodecId::H265 => Some(Self::H265),
            CodecId::Vp9 => None,
        }
    }

    /// 该 NAL 类型是否是参数集（H.264：SPS=7 / PPS=8；H.265：VPS=32 / SPS=33 / PPS=34）。
    fn is_parameter_set(self, nal_type: u8) -> bool {
        match self {
            Self::H264 => matches!(nal_type, 7 | 8),
            Self::H265 => matches!(nal_type, 32..=34),
        }
    }
}

/// 长度前缀解析失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NalError {
    /// 剩余字节不足以容纳 4 字节长度前缀。
    TruncatedPrefix {
        /// 出错处的偏移（相对 `data` 起点）。
        offset: usize,
    },
    /// 声明长度为 0（空块）。
    EmptyBlock {
        /// 出错处的偏移。
        offset: usize,
    },
    /// 声明长度越过缓冲区末尾。
    LengthOverflow {
        /// 块头偏移。
        offset: usize,
        /// 块头声明的负载长度。
        length: usize,
        /// 实际剩余可读字节数。
        remaining: usize,
    },
}

impl std::fmt::Display for NalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TruncatedPrefix { offset } => {
                write!(f, "长度前缀不完整：偏移 {offset} 剩余不足 4 字节")
            }
            Self::EmptyBlock { offset } => write!(f, "偏移 {offset} 处出现空块"),
            Self::LengthOverflow {
                offset,
                length,
                remaining,
            } => write!(
                f,
                "包长度越界：偏移 {offset} 声明 {length} 字节，实际剩余 {remaining}"
            ),
        }
    }
}

impl std::error::Error for NalError {}

/// 把一个 `VideoPacket.data` 拆成若干 Annex B 访问单元（每个长度前缀块一个）。
///
/// 空输入返回空 `Vec`。任意块非法时返回 [`NalError`]。
///
/// # Errors
/// 长度前缀不完整、空块或长度越界时返回错误。
pub(crate) fn access_units(data: &[u8]) -> Result<Vec<Vec<u8>>, NalError> {
    let mut units = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let remaining = data.len() - offset;
        if remaining < 4 {
            return Err(NalError::TruncatedPrefix { offset });
        }
        let header = &data[offset..offset + 4];
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        if length == 0 {
            return Err(NalError::EmptyBlock { offset });
        }
        let length_usize = usize::try_from(length).map_err(|_| NalError::LengthOverflow {
            offset,
            length: usize::MAX,
            remaining,
        })?;
        let start = offset + 4;
        let end = start
            .checked_add(length_usize)
            .ok_or(NalError::LengthOverflow {
                offset,
                length: length_usize,
                remaining: data.len() - start,
            })?;
        if end > data.len() {
            return Err(NalError::LengthOverflow {
                offset,
                length: length_usize,
                remaining: data.len() - start,
            });
        }
        units.push(normalize_block(&data[start..end]));
        offset = end;
    }
    Ok(units)
}

/// 把一个块负载归一化成 Annex B（已带起始码则原样保留，否则补 4 字节起始码）。
fn normalize_block(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    if payload.starts_with(&START_CODE) {
        out.extend_from_slice(payload);
    } else if payload.starts_with(&[0, 0, 1]) {
        // 3 字节起始码统一升级成 4 字节。
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&payload[3..]);
    } else {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(payload);
    }
    out
}

/// 扫出 Annex B 缓冲区里的全部 NAL 单元（去掉起始码与尾部零字节）。
fn split_annex_b(annex_b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut index = 0usize;
    let mut nal_start: Option<usize> = None;
    while index + 3 <= annex_b.len() {
        if annex_b[index] == 0 && annex_b[index + 1] == 0 && annex_b[index + 2] == 1 {
            if let Some(start) = nal_start {
                push_trimmed(&mut out, annex_b, start, index);
            }
            index += 3;
            nal_start = Some(index);
            continue;
        }
        index += 1;
    }
    if let Some(start) = nal_start {
        push_trimmed(&mut out, annex_b, start, annex_b.len());
    }
    out
}

/// 把 `[start, end)` 之间的 NAL 去掉尾部零字节后压入 `out`（空片段跳过）。
fn push_trimmed<'a>(out: &mut Vec<&'a [u8]>, annex_b: &'a [u8], start: usize, end: usize) {
    let mut end = end;
    // 下一个 4 字节起始码的前导 0 会被算进本 NAL；尾部零字节也一并剥掉。
    while end > start && annex_b[end - 1] == 0 {
        end -= 1;
    }
    if end > start {
        out.push(&annex_b[start..end]);
    }
}

/// 取 NAL 头里的类型字段（H.264 取低 5 位；H.265 是 `(b0 >> 1) & 0x3F`）。
fn nal_type(nal: &[u8], syntax: Syntax) -> Option<u8> {
    let first = *nal.first()?;
    Some(match syntax {
        Syntax::H264 => first & 0x1F,
        Syntax::H265 => (first >> 1) & 0x3F,
    })
}

/// 该 Annex B 缓冲区是否含参数集（H.264 SPS/PPS、H.265 VPS/SPS/PPS）。
pub(crate) fn has_parameter_sets(annex_b: &[u8], syntax: Syntax) -> bool {
    split_annex_b(annex_b)
        .into_iter()
        .filter_map(|nal| nal_type(nal, syntax))
        .any(|ty| syntax.is_parameter_set(ty))
}

/// 抽出 Annex B 缓冲区里的全部参数集 NAL，按出现顺序拼成带 4 字节起始码的 Annex B。
///
/// 没有任何参数集时返回空 `Vec`（调用方据此跳过 `csd-0`）。
pub(crate) fn parameter_sets(annex_b: &[u8], syntax: Syntax) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in split_annex_b(annex_b) {
        if nal_type(nal, syntax).is_some_and(|ty| syntax.is_parameter_set(ty)) {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::pedantic,
    reason = "测试代码使用断言与 unwrap 更直观"
)]
mod tests {
    use super::*;

    /// 造一个「4 字节长度前缀 + 负载」块。
    fn block(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn prepends_start_code_to_bare_nal() {
        // 裸 NAL（首字节 0x67 = SPS）。
        let data = block(&[0x67, 0x42, 0x00, 0x1E]);
        let units = access_units(&data).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0], vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E]);
    }

    #[test]
    fn preserves_existing_four_byte_start_code() {
        let payload = [0, 0, 0, 1, 0x67, 0x42];
        let data = block(&payload);
        let units = access_units(&data).unwrap();
        assert_eq!(units, vec![vec![0, 0, 0, 1, 0x67, 0x42]]);
    }

    #[test]
    fn upgrades_three_byte_start_code_to_four() {
        let payload = [0, 0, 1, 0x68, 0xCE];
        let data = block(&payload);
        let units = access_units(&data).unwrap();
        assert_eq!(units, vec![vec![0, 0, 0, 1, 0x68, 0xCE]]);
    }

    #[test]
    fn splits_multiple_blocks_into_multiple_units() {
        let mut data = block(&[0, 0, 0, 1, 0x67]); // SPS
        data.extend_from_slice(&block(&[0, 0, 0, 1, 0x68])); // PPS
        let units = access_units(&data).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0], vec![0, 0, 0, 1, 0x67]);
        assert_eq!(units[1], vec![0, 0, 0, 1, 0x68]);
    }

    #[test]
    fn empty_input_yields_no_units() {
        assert!(access_units(&[]).unwrap().is_empty());
    }

    #[test]
    fn truncated_prefix_is_rejected() {
        assert_eq!(
            access_units(&[0, 0]),
            Err(NalError::TruncatedPrefix { offset: 0 })
        );
    }

    #[test]
    fn empty_block_is_rejected() {
        assert_eq!(
            access_units(&[0, 0, 0, 0]),
            Err(NalError::EmptyBlock { offset: 0 })
        );
    }

    #[test]
    fn length_overflow_is_rejected() {
        // 声明 10 字节，实际只有 2 字节。
        let data = [0, 0, 0, 10, 0xAA, 0xBB];
        match access_units(&data) {
            Err(NalError::LengthOverflow {
                length, remaining, ..
            }) => {
                assert_eq!(length, 10);
                assert_eq!(remaining, 2);
            }
            other => panic!("期望 LengthOverflow，得到 {other:?}"),
        }
    }

    #[test]
    fn detects_h264_sps_and_pps() {
        let annex_b = [
            0, 0, 0, 1, 0x67, 0x42, // SPS (type 7)
            0, 0, 0, 1, 0x68, 0xCE, // PPS (type 8)
            0, 0, 0, 1, 0x65, 0x88, // IDR (type 5)
        ];
        assert!(has_parameter_sets(&annex_b, Syntax::H264));
        let ps = parameter_sets(&annex_b, Syntax::H264);
        assert_eq!(ps, vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE]);
    }

    #[test]
    fn h264_slice_without_parameter_sets_is_detected_as_absent() {
        let annex_b = [0, 0, 0, 1, 0x65, 0x88]; // 只有 IDR
        assert!(!has_parameter_sets(&annex_b, Syntax::H264));
        assert!(parameter_sets(&annex_b, Syntax::H264).is_empty());
    }

    #[test]
    fn detects_h265_vps_sps_pps() {
        // H.265 NAL 头两字节；类型在 b0 的 bits 1..=6。构造 32/33/34。
        let vps = 32u8 << 1; // 0x40
        let sps = 33u8 << 1; // 0x42
        let pps = 34u8 << 1; // 0x44
        let mut annex_b = Vec::new();
        for header in [vps, sps, pps] {
            annex_b.extend_from_slice(&[0, 0, 0, 1, header, 0x01]);
        }
        assert!(has_parameter_sets(&annex_b, Syntax::H265));
        let ps = parameter_sets(&annex_b, Syntax::H265);
        assert_eq!(ps.len(), 3 * 6, "三个参数集各 4 字节起始码 + 2 字节负载");
    }

    #[test]
    fn h265_slice_without_parameter_sets_is_absent() {
        // IDR_W_RADL = 19 → 19 << 1 = 0x26。
        let annex_b = [0, 0, 0, 1, 19u8 << 1, 0x01];
        assert!(!has_parameter_sets(&annex_b, Syntax::H265));
    }
}
