//! 码流小工具：AVCC↔Annex B、NAL 类型、参数集提取（H.264 / H.265）。
//!
//! Media Foundation 的编码器 MFT 有的输出 Annex B，有的输出 AVCC（每个 NAL
//! 前一个 4 字节大端长度）。remote 数据面要求 **Annex B**，所以这里做格式归一化，
//! 并解析 NAL 类型来判断关键帧 / 参数集。
//!
//! **Annex B / AVCC 的容器语法与编码标准无关**，两条路径共用；只有「NAL 头的
//! 类型字段」不同：
//!
//! - H.264：`b0 & 0x1F`，IDR = 5，SPS = 7，PPS = 8；
//! - H.265：`(b0 >> 1) & 0x3F`，关键帧 = IRAP `16..=23`（BLA/IDR/CRA），
//!   VPS = 32，SPS = 33，PPS = 34。

use crate::codec::CodecKind;
use crate::WinVideoError;

/// 判断是否已经是 Annex B（以起始码开头）。
fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

/// 把 AVCC（4 字节大端长度前缀的 NAL 序列）转成 Annex B。
fn avcc_to_annex_b(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len().saturating_add(16));
    let mut offset = 0usize;
    while offset < data.len() {
        let header = data.get(offset..offset.checked_add(4)?)?;
        let length = usize::try_from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]))
        .ok()?;
        if length == 0 {
            return None;
        }
        let start = offset.checked_add(4)?;
        let end = start.checked_add(length)?;
        let nal = data.get(start..end)?;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        offset = end;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 把编码器输出归一化成 Annex B。
///
/// 支持三种情况：已经是 Annex B、AVCC、以及没有前缀的单个裸 NAL（部分 MFT
/// 在只有一帧时这样输出）。容器语法与编码标准无关，故本函数不接收 codec。
/// 裸 NAL 的合法性判定仍按 H.264 头（`forbidden_zero_bit` + `nal_unit_type`）——
/// HEVC MFT 实测都输出 Annex B / AVCC，不会走到这条回退。
///
/// # Errors
/// 既不是 Annex B 也无法按 AVCC/裸 NAL 解析时返回 [`WinVideoError::Encode`]。
pub(super) fn normalize_to_annex_b(data: &[u8]) -> Result<Vec<u8>, WinVideoError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if is_annex_b(data) {
        return Ok(data.to_vec());
    }
    if let Some(annex_b) = avcc_to_annex_b(data) {
        return Ok(annex_b);
    }
    // 裸 NAL：forbidden_zero_bit = 0，nal_unit_type 合法。
    let first = data[0];
    let nal_type = first & 0x1F;
    if first & 0x80 == 0 && (1..=12).contains(&nal_type) {
        let mut out = Vec::with_capacity(data.len().saturating_add(4));
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(data);
        return Ok(out);
    }
    let preview = data
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ");
    Err(WinVideoError::Encode(format!(
        "无法识别的 Annex B / AVCC 码流格式（前 8 字节: {preview}）"
    )))
}

/// 从 NAL 的第一个字节取出 `nal_unit_type`（按编码标准的位布局）。
fn nal_header_type(first: u8, codec: CodecKind) -> u8 {
    if codec.is_hevc() {
        // HEVC：forbidden_zero_bit(1) | nal_unit_type(6) | layer_id(6) | tid+1(3)。
        (first >> 1) & 0x3F
    } else {
        // H.264：forbidden_zero_bit(1) | nal_ref_idc(2) | nal_unit_type(5)。
        first & 0x1F
    }
}

/// 列出 Annex B 码流中每个 NAL 的类型。
fn nal_types(annex_b: &[u8], codec: CodecKind) -> Vec<u8> {
    let mut starts = Vec::new();
    let mut index = 0usize;
    while index + 3 <= annex_b.len() {
        if annex_b[index] == 0 && annex_b[index + 1] == 0 && annex_b[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }
    let mut types = Vec::with_capacity(starts.len());
    for (position, &start) in starts.iter().enumerate() {
        let raw_end = starts
            .get(position + 1)
            .map_or(annex_b.len(), |next| next.saturating_sub(3));
        // 4 字节起始码多出的那个 0 会落在上一个 NAL 末尾；RBSP 尾部不可能以
        // 连续 0 结束，因此去掉尾部零是安全的。
        let mut end = raw_end;
        while end > start && annex_b[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            types.push(nal_header_type(annex_b[start], codec));
        }
    }
    types
}

/// 码流里是否有关键帧切片。
///
/// H.264 看 IDR（type 5）；H.265 看 IRAP（`16..=23`，含 BLA/IDR/CRA——编码器在
/// 强制关键帧时通常给 IDR，但接受全部 IRAP 才不会漏判）。
pub(super) fn has_keyframe(annex_b: &[u8], codec: CodecKind) -> bool {
    let types = nal_types(annex_b, codec);
    if codec.is_hevc() {
        types.iter().any(|&value| (16..=23).contains(&value))
    } else {
        types.contains(&5)
    }
}

/// 码流里是否带完整参数集。
///
/// H.264 需要 SPS(7) + PPS(8)；H.265 需要 VPS(32) + SPS(33) + PPS(34)。
pub(super) fn has_parameter_sets(annex_b: &[u8], codec: CodecKind) -> bool {
    let types = nal_types(annex_b, codec);
    if codec.is_hevc() {
        types.contains(&32) && types.contains(&33) && types.contains(&34)
    } else {
        types.contains(&7) && types.contains(&8)
    }
}

/// 提取 Annex B 中的参数集（带起始码），供关键帧前置。
pub(super) fn parameter_sets(annex_b: &[u8], codec: CodecKind) -> Vec<u8> {
    let mut starts = Vec::new();
    let mut index = 0usize;
    while index + 3 <= annex_b.len() {
        if annex_b[index] == 0 && annex_b[index + 1] == 0 && annex_b[index + 2] == 1 {
            starts.push(index);
            index += 3;
        } else {
            index += 1;
        }
    }
    let mut out = Vec::new();
    for (position, &start_code) in starts.iter().enumerate() {
        let nal_start = start_code + 3;
        let raw_end = starts.get(position + 1).map_or(annex_b.len(), |next| *next);
        let mut end = raw_end;
        while end > nal_start && annex_b[end - 1] == 0 {
            end -= 1;
        }
        if end <= nal_start {
            continue;
        }
        let nal_type = nal_header_type(annex_b[nal_start], codec);
        let wanted = if codec.is_hevc() {
            (32..=34).contains(&nal_type)
        } else {
            nal_type == 7 || nal_type == 8
        };
        if wanted {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&annex_b[nal_start..end]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::CodecKind;

    #[test]
    fn annex_b_is_passed_through() {
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0xCC,
        ];
        let normalized = normalize_to_annex_b(&data).unwrap_or_default();
        assert_eq!(normalized, data);
        assert!(has_parameter_sets(&normalized, CodecKind::H264));
        assert!(has_keyframe(&normalized, CodecKind::H264));
    }

    #[test]
    fn avcc_is_converted_to_annex_b() {
        let data = [
            0, 0, 0, 2, 0x67, 0xAA, 0, 0, 0, 2, 0x68, 0xBB, 0, 0, 0, 2, 0x65, 0xCC,
        ];
        let normalized = normalize_to_annex_b(&data).unwrap_or_default();
        assert!(is_annex_b(&normalized));
        assert!(has_parameter_sets(&normalized, CodecKind::H264));
        assert!(has_keyframe(&normalized, CodecKind::H264));
    }

    #[test]
    fn bare_nal_is_wrapped() {
        let normalized = normalize_to_annex_b(&[0x65, 0x01, 0x02]).unwrap_or_default();
        assert_eq!(normalized, [0, 0, 0, 1, 0x65, 0x01, 0x02]);
        assert!(has_keyframe(&normalized, CodecKind::H264));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(normalize_to_annex_b(&[0xFF, 0xFF, 0xFF, 0xFF]).is_err());
    }

    /// HEVC 的 `nal_unit_type` 在 `(b0 >> 1) & 0x3F`：构造 VPS/SPS/PPS + IDR_W_RADL
    /// 的 Annex B 码流，验证参数集与关键帧判定。
    ///
    /// 关键点：**IDR 不能漏判**——补丁模式的基准帧必须阻塞回读，漏判会让画面
    /// 停在旧基准上。
    #[test]
    fn hevc_parameter_sets_and_irap_are_detected() {
        // b0 = type << 1 | 1（nuh_temporal_id_plus1 = 1）。
        let hevc_nal = |nal_type: u8| [(nal_type << 1) | 1, 0x01];
        let mut data = Vec::new();
        for nal_type in [32u8, 33, 34, 19] {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(&hevc_nal(nal_type));
        }
        let normalized = normalize_to_annex_b(&data).unwrap_or_default();
        assert!(
            has_parameter_sets(&normalized, CodecKind::H265),
            "VPS+SPS+PPS 必须被识别"
        );
        assert!(
            has_keyframe(&normalized, CodecKind::H265),
            "IDR_W_RADL（19）必须被识别为关键帧"
        );
        // 参数集应按 VPS、SPS、PPS 顺序原样提取：每段 = 4 字节起始码 + 2 字节 NAL。
        let sets = parameter_sets(&normalized, CodecKind::H265);
        assert_eq!(sets.len(), 6 * 3, "三个参数集各 6 字节（起始码 + NAL）");
        assert_eq!(&sets[..4], &[0, 0, 0, 1], "首段应带起始码");
    }

    /// IRAP 的完整区间 `16..=23` 都算关键帧；非 IRAP 的 0..=15 不算。
    #[test]
    fn hevc_all_irap_types_are_keyframes() {
        for nal_type in 16u8..=23 {
            let data = [0, 0, 0, 1, (nal_type << 1) | 1, 0x01];
            assert!(
                has_keyframe(&data, CodecKind::H265),
                "IRAP {nal_type} 应算关键帧"
            );
        }
        for nal_type in [0u8, 1, 9, 15, 24, 32, 33, 34] {
            let data = [0, 0, 0, 1, (nal_type << 1) | 1, 0x01];
            assert!(
                !has_keyframe(&data, CodecKind::H265),
                "{nal_type} 不是 IRAP，不应算关键帧"
            );
        }
    }

    /// HEVC 缺 VPS 时参数集不完整（IDR 需要三者齐全）。
    #[test]
    fn hevc_parameter_sets_require_vps() {
        let data = [
            0, 0, 0, 1, (33 << 1) | 1, 0x01, 0, 0, 0, 1, (34 << 1) | 1, 0x01,
        ];
        assert!(!has_parameter_sets(&data, CodecKind::H265), "缺 VPS 不算完整");
        let with_vps = [
            0, 0, 0, 1, (32 << 1) | 1, 0x01, 0, 0, 0, 1, (33 << 1) | 1, 0x01, 0, 0, 0, 1,
            (34 << 1) | 1, 0x01,
        ];
        assert!(has_parameter_sets(&with_vps, CodecKind::H265));
    }

    /// H.264 的判定不受 HEVC 位布局影响（同一条 slice 字节不会互相误判）。
    #[test]
    fn h264_and_hevc_classification_do_not_cross_talk() {
        // H.264 IDR slice：0x65 → type 5；按 HEVC 解则是 0x32 = 50，不是 IRAP。
        let h264_idr = [0, 0, 0, 1, 0x65, 0xAA];
        assert!(has_keyframe(&h264_idr, CodecKind::H264));
        assert!(!has_keyframe(&h264_idr, CodecKind::H265));
        // HEVC IDR_W_RADL：type 19 → b0 = 0x27；按 H.264 解是 7（SPS），不是 IDR。
        let hevc_idr = [0, 0, 0, 1, (19 << 1) | 1, 0x01];
        assert!(has_keyframe(&hevc_idr, CodecKind::H265));
        assert!(!has_keyframe(&hevc_idr, CodecKind::H264));
    }
}
