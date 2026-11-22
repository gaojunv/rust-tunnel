//! Annex B / AVCC 码流容器工具（纯逻辑，任意平台可单测）。
//!
//! VideoToolbox 的**压缩会话输出 AVCC**（每个 NAL 前一个 4 字节大端长度），而 remote
//! 数据面要求 **Annex B**（起始码 `00 00 01` / `00 00 00 01`）；解码侧反过来：会话要求
//! 的 `CMSampleBuffer` 是 AVCC，而线上收到的是 Annex B。本模块负责两个方向的归一化，
//! 以及参数集/关键帧的提取。
//!
//! 容器语法与编码标准无关；只有「NAL 头里的类型字段」不同，那部分由
//! [`CodecKind`](crate::CodecKind) 提供。

use crate::codec::CodecKind;
use crate::error::{MacVideoError, MacVideoResult};

/// Annex B 起始码的固定前缀（3 字节部分）。
const START_CODE: [u8; 3] = [0, 0, 1];

/// 是否以 Annex B 起始码开头。
#[must_use]
pub fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&START_CODE) || data.starts_with(&[0, 0, 0, 1])
}

/// 如果 `data` 以「4 字节大端长度前缀」开头且前缀正好覆盖剩余字节，返回前缀后的载荷；
/// 否则返回 `None`（调用方按旧的裸 Annex B 处理）。
///
/// 线上契约与 `win-video` 一致：一帧 = `u32 BE payload_len` + Annex B 码流。
/// **必须显式剥前缀**再扫描起始码：裸 Annex B 的前缀 `00 00 00 01` 在 [`nal_units`]
/// 里会被当成普通字节扫描，而长度前缀本身可能包含 `00 00 01`（例如长度
/// `0x00000105`），不剥就会误判成起始码。
#[must_use]
pub fn length_prefixed_payload(data: &[u8]) -> Option<&[u8]> {
    let header = data.get(..4)?;
    let length = usize::try_from(u32::from_be_bytes([
        header[0], header[1], header[2], header[3],
    ]))
    .ok()?;
    (length == data.len().checked_sub(4)?).then(|| data.get(4..).unwrap_or_default())
}

/// 扫描出每个起始码之后的 NAL 起点（索引指向 NAL 的首字节）。
///
/// 4 字节起始码 `00 00 00 01` 会被识别成「偏移 1 处的 3 字节起始码」，多出的那个 `0`
/// 落在**上一个 NAL 的末尾**，由 [`nal_units`] 统一裁掉。
fn nal_starts(data: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut index = 0usize;
    while index + START_CODE.len() <= data.len() {
        if data.get(index..index + 3) == Some(START_CODE.as_slice()) {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }
    starts
}

/// 把 Annex B 码流拆成 NAL 载荷（不含起始码）。
///
/// 末尾连续的 `0` 会被裁掉：4 字节起始码多出的那个 `0` 不属于 NAL 数据，而合法的
/// RBSP 以 `rbsp_trailing_bits`（末位为 1）结束，末字节不可能是 `0`。
///
/// 起始码之前的前导字节（如果有）被忽略；输入里没有任何起始码时返回空列表。
#[must_use]
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let starts = nal_starts(data);
    let mut units = Vec::with_capacity(starts.len());
    for (position, &start) in starts.iter().enumerate() {
        let raw_end = starts
            .get(position + 1)
            .map_or(data.len(), |next| next.saturating_sub(START_CODE.len()));
        let mut end = raw_end.min(data.len());
        while end > start && data.get(end.saturating_sub(1)) == Some(&0) {
            end -= 1;
        }
        if end > start {
            if let Some(unit) = data.get(start..end) {
                units.push(unit);
            }
        }
    }
    units
}

/// 取 NAL 的 `nal_unit_type`（空 NAL 返回 `None`）。
#[must_use]
pub fn nal_type(codec: CodecKind, nal: &[u8]) -> Option<u8> {
    nal.first().map(|&first| codec.nal_type_of(first))
}

/// 把 AVCC（4 字节大端长度前缀的 NAL 序列）转成 Annex B。
///
/// 每个 NAL 统一加 **4 字节**起始码（`00 00 00 01`），避免解码端还要处理混合起始码。
///
/// # Errors
/// 长度前缀缺失、长度为零、或声明的长度超出剩余字节时返回
/// [`MacVideoError::Decode`]（这是码流损坏，不是配置问题）。
pub fn avcc_to_annex_b(data: &[u8]) -> MacVideoResult<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(data.len().saturating_add(16));
    let mut offset = 0usize;
    while offset < data.len() {
        let header = data.get(offset..offset.saturating_add(4)).ok_or_else(|| {
            MacVideoError::Decode(format!("AVCC 长度前缀不完整（偏移 {offset}）"))
        })?;
        let length = usize::try_from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]))
        .map_err(|_| MacVideoError::Decode("AVCC NAL 长度溢出 usize".to_owned()))?;
        if length == 0 {
            return Err(MacVideoError::Decode(format!(
                "AVCC NAL 长度为零（偏移 {offset}）"
            )));
        }
        let start = offset.saturating_add(4);
        let end = start.saturating_add(length);
        let nal = data.get(start..end).ok_or_else(|| {
            MacVideoError::Decode(format!(
                "AVCC NAL 长度越界：声明 {length} 字节，剩余 {}",
                data.len().saturating_sub(start)
            ))
        })?;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        offset = end;
    }
    Ok(out)
}

/// 把 Annex B 转成 AVCC（每个 NAL 前一个 4 字节大端长度）。
///
/// 输入里没有起始码时返回空 `Vec`（与 [`nal_units`] 一致）。
///
/// # Errors
/// 单个 NAL 超过 `u32::MAX` 字节（不可能出现，但显式拒绝而不是静默截断）。
pub fn annex_b_to_avcc(data: &[u8]) -> MacVideoResult<Vec<u8>> {
    let units = nal_units(data);
    let mut out = Vec::with_capacity(data.len());
    for unit in units {
        let length = u32::try_from(unit.len())
            .map_err(|_| MacVideoError::Decode("NAL 超过 4 GiB，无法写入 AVCC".to_owned()))?;
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(unit);
    }
    Ok(out)
}

/// 码流里是否有关键帧切片。
#[must_use]
pub fn has_keyframe(codec: CodecKind, annex_b: &[u8]) -> bool {
    nal_units(annex_b)
        .iter()
        .any(|unit| nal_type(codec, unit).is_some_and(|nal| codec.is_keyframe_nal(nal)))
}

/// 码流里是否带**完整**参数集（H.264 需要 SPS+PPS；H.265 需要 VPS+SPS+PPS）。
#[must_use]
pub fn has_parameter_sets(codec: CodecKind, annex_b: &[u8]) -> bool {
    let units = nal_units(annex_b);
    let present: Vec<u8> = units
        .iter()
        .filter_map(|unit| nal_type(codec, unit))
        .filter(|&nal| codec.is_parameter_set(nal))
        .collect();
    if codec.is_hevc() {
        [32u8, 33, 34].iter().all(|want| present.contains(want))
    } else {
        [7u8, 8].iter().all(|want| present.contains(want))
    }
}

/// 取出码流里的参数集 NAL 载荷（**不含**起始码）。
///
/// 顺序按它们在码流里出现的顺序（正常是 VPS→SPS→PPS 或 SPS→PPS），这正是
/// `CMVideoFormatDescriptionCreateFromH264ParameterSets` /
/// `...HEVCParameterSets` 期望的入参顺序。
#[must_use]
pub fn parameter_sets(codec: CodecKind, annex_b: &[u8]) -> Vec<&[u8]> {
    nal_units(annex_b)
        .into_iter()
        .filter(|unit| nal_type(codec, unit).is_some_and(|nal| codec.is_parameter_set(nal)))
        .collect()
}

/// 把码流里的参数集按 Annex B 形式拼成一串（每段带 4 字节起始码）。
///
/// 空结果表示码流里没有参数集（编码器可能在关键帧外单独下发）。
#[must_use]
pub fn parameter_set_blob(codec: CodecKind, annex_b: &[u8]) -> Vec<u8> {
    let sets = parameter_sets(codec, annex_b);
    let mut out = Vec::new();
    for set in sets {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(set);
    }
    out
}

/// 在关键帧码流前拼接参数集（空参数集时原样返回）。
///
/// VideoToolbox 的 `CMVideoFormatDescription` 里已经带了参数集，解码端不一定需要
/// 内联参数集；但数据面是**整段码流直接落进 decoder**，而解码会话可能刚重建（新尺寸/
/// 新参数集），把参数集前置最稳妥。
#[must_use]
pub fn prepend_parameter_sets(sets: &[u8], annex_b: &[u8]) -> Vec<u8> {
    if sets.is_empty() || annex_b.is_empty() {
        return annex_b.to_vec();
    }
    let mut out = Vec::with_capacity(sets.len().saturating_add(annex_b.len()));
    out.extend_from_slice(sets);
    out.extend_from_slice(annex_b);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 3 字节与 4 字节起始码混排时，NAL 边界与载荷都要正确。
    #[test]
    fn mixed_start_codes_split_correctly() {
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, // 4 字节起始码
            0, 0, 1, 0x68, 0xBB, // 3 字节起始码
            0, 0, 0, 1, 0x65, 0xCC, 0xDD, // 4 字节起始码
        ];
        let units = nal_units(&data);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], [0x67, 0xAA]);
        assert_eq!(units[1], [0x68, 0xBB]);
        assert_eq!(units[2], [0x65, 0xCC, 0xDD]);
        assert!(is_annex_b(&data));
    }

    /// 空输入：既不 panic，也不产出 NAL。
    #[test]
    fn empty_input_is_harmless() {
        assert!(nal_units(&[]).is_empty());
        assert!(parameter_sets(CodecKind::H264, &[]).is_empty());
        assert!(!has_keyframe(CodecKind::H264, &[]));
        assert!(!has_parameter_sets(CodecKind::H264, &[]));
        assert_eq!(avcc_to_annex_b(&[]).unwrap_or_default(), Vec::<u8>::new());
        assert_eq!(annex_b_to_avcc(&[]).unwrap_or_default(), Vec::<u8>::new());
        assert!(!is_annex_b(&[]));
    }

    /// AVCC → Annex B → AVCC 往返后 NAL 载荷不变。
    #[test]
    fn avcc_annexb_roundtrip() {
        let avcc = [
            0, 0, 0, 2, 0x67, 0xAA, //
            0, 0, 0, 2, 0x68, 0xBB, //
            0, 0, 0, 3, 0x65, 0xCC, 0xDD,
        ];
        let annex_b = avcc_to_annex_b(&avcc).unwrap_or_default();
        assert!(is_annex_b(&annex_b));
        assert!(has_parameter_sets(CodecKind::H264, &annex_b));
        assert!(has_keyframe(CodecKind::H264, &annex_b));
        let roundtrip = annex_b_to_avcc(&annex_b).unwrap_or_default();
        assert_eq!(roundtrip, avcc);
    }

    /// 长度声明越界 / 前缀不完整 / 长度为零都必须报错，不能越界读。
    #[test]
    fn avcc_length_violations_are_rejected() {
        // 声明 8 字节，但只剩 2 字节。
        assert!(avcc_to_annex_b(&[0, 0, 0, 8, 0x67, 0xAA]).is_err());
        // 前缀不足 4 字节。
        assert!(avcc_to_annex_b(&[0, 0, 0]).is_err());
        // 长度为零。
        assert!(avcc_to_annex_b(&[0, 0, 0, 0]).is_err());
    }

    /// H.265 的 VPS/SPS/PPS 与 IRAP 都能被识别，顺序保持码流顺序。
    #[test]
    fn hevc_parameter_sets_and_irap() {
        let hevc_nal = |nal_type: u8| [(nal_type << 1) | 1, 0x01];
        let mut data = Vec::new();
        for nal in [32u8, 33, 34, 19] {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(&hevc_nal(nal));
        }
        assert!(has_parameter_sets(CodecKind::H265, &data));
        assert!(has_keyframe(CodecKind::H265, &data));
        let sets = parameter_sets(CodecKind::H265, &data);
        assert_eq!(sets.len(), 3);
        assert_eq!(nal_type(CodecKind::H265, sets[0]), Some(32));
        assert_eq!(nal_type(CodecKind::H265, sets[1]), Some(33));
        assert_eq!(nal_type(CodecKind::H265, sets[2]), Some(34));
        let blob = parameter_set_blob(CodecKind::H265, &data);
        assert_eq!(blob.len(), 3 * (4 + 2));
        assert_eq!(&blob[..4], &[0, 0, 0, 1]);
    }

    /// H.264 缺 PPS 时参数集不完整。
    #[test]
    fn h264_parameter_sets_require_pps() {
        let sps_only = [0, 0, 0, 1, 0x67, 0xAA];
        assert!(!has_parameter_sets(CodecKind::H264, &sps_only));
        let sps_pps = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB];
        assert!(has_parameter_sets(CodecKind::H264, &sps_pps));
    }

    /// 前置参数集只影响拼接结果，空参数集时逐字节原样返回。
    #[test]
    fn prepend_is_a_noop_for_empty_sets() {
        let frame = [0, 0, 0, 1, 0x65, 0xCC];
        assert_eq!(prepend_parameter_sets(&[], &frame), frame);
        let sets = [0, 0, 0, 1, 0x67, 0xAA];
        let joined = prepend_parameter_sets(&sets, &frame);
        assert_eq!(joined.len(), sets.len() + frame.len());
        assert_eq!(&joined[..sets.len()], &sets);
    }

    /// H.264 与 H.265 的判定互不串味。
    #[test]
    fn codec_classification_does_not_cross_talk() {
        let h264_idr = [0, 0, 0, 1, 0x65, 0xAA];
        assert!(has_keyframe(CodecKind::H264, &h264_idr));
        assert!(!has_keyframe(CodecKind::H265, &h264_idr));
        let hevc_idr = [0, 0, 0, 1, (19 << 1) | 1, 0x01];
        assert!(has_keyframe(CodecKind::H265, &hevc_idr));
        assert!(!has_keyframe(CodecKind::H264, &hevc_idr));
    }

    /// 4 字节大端长度前缀 + Annex B：剥出载荷。
    #[test]
    fn length_prefix_is_stripped() {
        let annex_b = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x65, 0xCC];
        let mut framed = u32::try_from(annex_b.len()).unwrap().to_be_bytes().to_vec();
        framed.extend_from_slice(&annex_b);
        assert_eq!(length_prefixed_payload(&framed), Some(annex_b.as_slice()));
    }

    /// 裸 Annex B 没有长度前缀，helper 必须返回 `None`（调用方按旧格式处理）。
    #[test]
    fn bare_annex_b_is_not_length_prefixed() {
        let annex_b = [0, 0, 0, 1, 0x67, 0xAA];
        assert_eq!(length_prefixed_payload(&annex_b), None);
    }

    /// 前缀声明的长度与实际剩余字节不符时不得剥离。
    #[test]
    fn mismatched_prefix_length_is_rejected() {
        let mut framed = 7_u32.to_be_bytes().to_vec();
        framed.extend_from_slice(&[0, 0, 0, 1, 0x67, 0xAA]);
        assert_eq!(length_prefixed_payload(&framed), None);
        let mut short = 0_u32.to_be_bytes().to_vec();
        short.extend_from_slice(&[1, 2, 3]);
        assert_eq!(length_prefixed_payload(&short), None);
    }

    /// 长度前缀本身含 `00 00 01`（0x00000105）时，裸扫描会误判起始码；
    /// helper 必须按长度剥掉，不能靠起始码猜边界。
    #[test]
    fn prefix_containing_start_code_is_not_mistaken_for_nal() {
        let payload_len = 0x0000_0105_usize;
        let mut payload = vec![0_u8; payload_len];
        payload[..4].copy_from_slice(&[0, 0, 0, 1]);
        payload[4] = 0x65;
        let mut framed = u32::try_from(payload_len).unwrap().to_be_bytes().to_vec();
        framed.extend_from_slice(&payload);
        // 前缀字节是 00 00 01 05，裸看确实像 3 字节起始码。
        assert!(is_annex_b(&framed));
        assert_eq!(length_prefixed_payload(&framed), Some(payload.as_slice()));
        assert_eq!(length_prefixed_payload(&framed).map(<[u8]>::len), Some(payload_len));
    }
}
