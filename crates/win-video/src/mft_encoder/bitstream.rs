//! H.264 码流小工具：AVCC↔Annex B、NAL 类型、SPS/PPS 提取。
//!
//! Media Foundation 的编码器 MFT 有的输出 Annex B，有的输出 AVCC（每个 NAL
//! 前一个 4 字节大端长度）。remote 数据面要求 **Annex B**，所以这里做格式归一化，
//! 并解析 NAL 类型来判断 IDR / SPS / PPS。

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
/// 在只有一帧时这样输出）。
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
        "无法识别的 H.264 码流格式（前 8 字节: {preview}）"
    )))
}

/// 列出 Annex B 码流中每个 NAL 的类型。
fn nal_types(annex_b: &[u8]) -> Vec<u8> {
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
            types.push(annex_b[start] & 0x1F);
        }
    }
    types
}

/// 码流里是否有 IDR 切片（NAL type 5）。
pub(super) fn has_idr(annex_b: &[u8]) -> bool {
    nal_types(annex_b).contains(&5)
}

/// 码流里是否同时有 SPS（7）与 PPS（8）。
pub(super) fn has_parameter_sets(annex_b: &[u8]) -> bool {
    let types = nal_types(annex_b);
    types.contains(&7) && types.contains(&8)
}

/// 提取 Annex B 中的 SPS/PPS（带起始码），供关键帧前置。
pub(super) fn parameter_sets(annex_b: &[u8]) -> Vec<u8> {
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
        let nal_type = annex_b[nal_start] & 0x1F;
        if nal_type == 7 || nal_type == 8 {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&annex_b[nal_start..end]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_is_passed_through() {
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0xCC,
        ];
        let normalized = normalize_to_annex_b(&data).unwrap_or_default();
        assert_eq!(normalized, data);
        assert!(has_parameter_sets(&normalized));
        assert!(has_idr(&normalized));
    }

    #[test]
    fn avcc_is_converted_to_annex_b() {
        let data = [
            0, 0, 0, 2, 0x67, 0xAA, 0, 0, 0, 2, 0x68, 0xBB, 0, 0, 0, 2, 0x65, 0xCC,
        ];
        let normalized = normalize_to_annex_b(&data).unwrap_or_default();
        assert!(is_annex_b(&normalized));
        assert!(has_parameter_sets(&normalized));
        assert!(has_idr(&normalized));
    }

    #[test]
    fn bare_nal_is_wrapped() {
        let normalized = normalize_to_annex_b(&[0x65, 0x01, 0x02]).unwrap_or_default();
        assert_eq!(normalized, [0, 0, 0, 1, 0x65, 0x01, 0x02]);
        assert!(has_idr(&normalized));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(normalize_to_annex_b(&[0xFF, 0xFF, 0xFF, 0xFF]).is_err());
    }
}
