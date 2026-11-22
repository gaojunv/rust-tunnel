//! 编码标准（H.264 / H.265）的**纯逻辑**判定：NAL 头类型、关键帧、参数集。
//!
//! 与 `crates/win-video/src/codec.rs` 同口径：本模块**不依赖任何 Apple API**，可在
//! 任意平台单测。VideoToolbox 层（[`crate::vt_encoder`] / [`crate::vt_decoder`]）只
//! 用这里的结果决定「是不是关键帧」「要不要重建格式描述」。
//!
//! # 两种标准的位布局
//!
//! - H.264：`nal_unit_type` 在 `b0 & 0x1F`；IDR = 5，SPS = 7，PPS = 8；
//! - H.265：`nal_unit_type` 在 `(b0 >> 1) & 0x3F`；关键帧 = IRAP `16..=23`
//!   （BLA/IDR/CRA），VPS = 32，SPS = 33，PPS = 34。

/// 本 crate 支持的编码标准。
///
/// 与 remote 的 `CodecId` 刻意分开：本 crate 不依赖 `rust-tunnel-remote`（否则会把
/// 会话协议与 libvpx 拖进硬件层）。`client-gui` 的适配器负责两者互转。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodecKind {
    /// H.264 / AVC。
    H264,
    /// H.265 / HEVC。
    H265,
}

impl CodecKind {
    /// 人类可读的标签（日志/遥测用）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::H265 => "H.265",
        }
    }

    /// 是否 HEVC。
    #[must_use]
    pub const fn is_hevc(self) -> bool {
        matches!(self, Self::H265)
    }

    /// CoreMedia 的 `CMVideoCodecType`（`FourCharCode`）。
    ///
    /// 常量来自 `CMFormatDescription.h`：`kCMVideoCodecType_H264 = 'avc1'`、
    /// `kCMVideoCodecType_HEVC = 'hvc1'`。
    #[must_use]
    pub const fn codec_type(self) -> u32 {
        match self {
            // 'avc1'
            Self::H264 => 0x6176_6331,
            // 'hvc1'
            Self::H265 => 0x6876_6331,
        }
    }

    /// 从 NAL 首字节取出 `nal_unit_type`（按编码标准的位布局）。
    #[must_use]
    pub const fn nal_type_of(self, first: u8) -> u8 {
        if self.is_hevc() {
            // forbidden_zero_bit(1) | nal_unit_type(6) | layer_id(6) | tid+1(3)
            (first >> 1) & 0x3F
        } else {
            // forbidden_zero_bit(1) | nal_ref_idc(2) | nal_unit_type(5)
            first & 0x1F
        }
    }

    /// 该 NAL 类型是否表示**关键帧切片**。
    ///
    /// H.264 只认 IDR（5）；H.265 接受全部 IRAP（`16..=23`）——编码器强制关键帧时通常
    /// 给 IDR，但接受 BLA/CRA 才不会漏判。
    #[must_use]
    pub const fn is_keyframe_nal(self, nal_type: u8) -> bool {
        if self.is_hevc() {
            nal_type >= 16 && nal_type <= 23
        } else {
            nal_type == 5
        }
    }

    /// 该 NAL 类型是否是**参数集**。
    ///
    /// H.264：SPS(7) / PPS(8)；H.265：VPS(32) / SPS(33) / PPS(34)。
    #[must_use]
    pub const fn is_parameter_set(self, nal_type: u8) -> bool {
        if self.is_hevc() {
            nal_type >= 32 && nal_type <= 34
        } else {
            nal_type == 7 || nal_type == 8
        }
    }

    /// 参数集在 `CMVideoFormatDescription` 里的**必需**数量。
    ///
    /// H.264 需要 SPS + PPS 两段，H.265 需要 VPS + SPS + PPS 三段——少一段
    /// `CMVideoFormatDescriptionCreateFrom*ParameterSets` 就会失败。
    #[must_use]
    pub const fn parameter_set_count(self) -> usize {
        if self.is_hevc() {
            3
        } else {
            2
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 两种标准的四字符码与标签。
    #[test]
    fn codec_identity() {
        assert_eq!(CodecKind::H264.label(), "H.264");
        assert_eq!(CodecKind::H265.label(), "H.265");
        assert!(!CodecKind::H264.is_hevc());
        assert!(CodecKind::H265.is_hevc());
        assert_eq!(CodecKind::H264.codec_type(), u32::from_be_bytes(*b"avc1"));
        assert_eq!(CodecKind::H265.codec_type(), u32::from_be_bytes(*b"hvc1"));
    }

    /// NAL 头解析：H.264 的 IDR 0x65 → 5；HEVC 的 IDR_W_RADL 0x27 → 19。
    #[test]
    fn nal_header_layout() {
        assert_eq!(CodecKind::H264.nal_type_of(0x65), 5);
        assert_eq!(CodecKind::H264.nal_type_of(0x67), 7);
        assert_eq!(CodecKind::H265.nal_type_of(0x27), 19);
        assert_eq!(CodecKind::H265.nal_type_of(0x40), 32);
    }

    /// 关键帧判定：IDR(5) / IRAP(16..=23)，其余都不算。
    #[test]
    fn keyframe_detection() {
        assert!(CodecKind::H264.is_keyframe_nal(5));
        assert!(!CodecKind::H264.is_keyframe_nal(1));
        for nal in 16u8..=23 {
            assert!(CodecKind::H265.is_keyframe_nal(nal), "IRAP {nal}");
        }
        for nal in [0u8, 1, 9, 15, 24, 32, 33, 34] {
            assert!(!CodecKind::H265.is_keyframe_nal(nal), "{nal} 不是 IRAP");
        }
    }

    /// 参数集判定与必需数量。
    #[test]
    fn parameter_sets() {
        assert!(CodecKind::H264.is_parameter_set(7));
        assert!(CodecKind::H264.is_parameter_set(8));
        assert!(!CodecKind::H264.is_parameter_set(5));
        for nal in [32u8, 33, 34] {
            assert!(CodecKind::H265.is_parameter_set(nal), "{nal}");
        }
        assert!(!CodecKind::H265.is_parameter_set(31));
        assert_eq!(CodecKind::H264.parameter_set_count(), 2);
        assert_eq!(CodecKind::H265.parameter_set_count(), 3);
    }
}
