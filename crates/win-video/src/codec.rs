//! 数据面支持的视频编码标准。
//!
//! Windows 硬件数据面固定 **4:2:0 8-bit（NV12）、BT.709 limited range**，编码标准
//! 只有两种：
//!
//! - [`CodecKind::H264`]：NVENC H.264 / Intel-AMD 硬件 H.264 编码 MFT，
//!   `MSH264DecoderMFT` 解码；
//! - [`CodecKind::H265`]：NVENC HEVC / 硬件 HEVC 编码 MFT，`MSH265DecoderMFT`
//!   解码（依赖 Microsoft Store 的 HEVC Video Extensions）。
//!
//! 两种标准的像素格式、颜色矩阵与渲染链路完全一致，差异只在码流层（NAL 语法、
//! 参数集类型、关键帧判定）。本类型只做「标准」这个维度上的分发，不携带任何
//! 平台资源。

/// 数据面支持的视频编码标准。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CodecKind {
    /// H.264 / AVC（Main/High profile，4:2:0 8-bit）。
    #[default]
    H264,
    /// H.265 / HEVC（Main profile，4:2:0 8-bit）。
    H265,
}

impl CodecKind {
    /// 供日志 / 探针 / UI 展示的稳定短名称。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::H265 => "H.265",
        }
    }

    /// 是否是 HEVC。
    #[must_use]
    pub const fn is_hevc(self) -> bool {
        matches!(self, Self::H265)
    }

    /// 从命令行/配置文本解析编码标准（`h264` / `h265` / `hevc`，大小写无关）。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("h264") || text.eq_ignore_ascii_case("avc") {
            Some(Self::H264)
        } else if text.eq_ignore_ascii_case("h265") || text.eq_ignore_ascii_case("hevc") {
            Some(Self::H265)
        } else {
            None
        }
    }
}

/// 真机探针的 `--codec` 选择（`h264` / `hevc` / `both`）。
///
/// 生产代码不用它——引擎按协商结果选一个 codec，探针则常需要同一台机器上
/// 两种后端各跑一遍做对照。默认 [`CodecSelection::H264`]，保持 H.264 探针的
/// 旧行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodecSelection {
    /// 只跑 H.264（默认）。
    #[default]
    H264,
    /// 只跑 H.265 / HEVC。
    H265,
    /// 两种都跑（先 H.264，再 H.265）。
    Both,
}

impl CodecSelection {
    /// 解析 `--codec` 的取值（大小写无关，兼容 `h265` / `hevc`）。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("both") {
            Some(Self::Both)
        } else {
            CodecKind::parse(text).map(|codec| match codec {
                CodecKind::H264 => Self::H264,
                CodecKind::H265 => Self::H265,
            })
        }
    }

    /// 本次要跑的编码标准（按顺序）。
    #[must_use]
    pub const fn kinds(self) -> &'static [CodecKind] {
        match self {
            Self::H264 => &[CodecKind::H264],
            Self::H265 => &[CodecKind::H265],
            Self::Both => &[CodecKind::H264, CodecKind::H265],
        }
    }

    /// 从进程参数里读 `--codec <值>` / `--codec=<值>`；缺省为 [`Self::H264`]。
    ///
    /// 无法识别的值会打印一行警告并回落到默认值，而不是让整条探针失败——
    /// 探针是排障工具，参数笔误不应该掩盖其他更有价值的输出。
    #[must_use]
    pub fn from_args() -> Self {
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let value = if let Some(value) = arg.strip_prefix("--codec=") {
                Some(value.to_owned())
            } else if arg == "--codec" {
                args.next()
            } else {
                None
            };
            if let Some(value) = value {
                return Self::parse(&value).unwrap_or_else(|| {
                    eprintln!("无法识别的 --codec 取值 {value:?}，按 h264 运行");
                    Self::default()
                });
            }
        }
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{CodecKind, CodecSelection};

    #[test]
    fn codec_kind_labels_and_hevc_flag() {
        assert_eq!(CodecKind::H264.label(), "H.264");
        assert_eq!(CodecKind::H265.label(), "H.265");
        assert!(!CodecKind::H264.is_hevc());
        assert!(CodecKind::H265.is_hevc());
        assert_eq!(CodecKind::default(), CodecKind::H264);
    }

    #[test]
    fn codec_kind_parsing_is_case_insensitive() {
        assert_eq!(CodecKind::parse("h264"), Some(CodecKind::H264));
        assert_eq!(CodecKind::parse("H264"), Some(CodecKind::H264));
        assert_eq!(CodecKind::parse("avc"), Some(CodecKind::H264));
        assert_eq!(CodecKind::parse(" h265 "), Some(CodecKind::H265));
        assert_eq!(CodecKind::parse("hevc"), Some(CodecKind::H265));
        assert_eq!(CodecKind::parse("HEVC"), Some(CodecKind::H265));
        assert_eq!(CodecKind::parse("vp9"), None);
        assert_eq!(CodecKind::parse(""), None);
    }

    #[test]
    fn codec_selection_expands_to_kinds() {
        assert_eq!(CodecSelection::default(), CodecSelection::H264);
        assert_eq!(CodecSelection::H264.kinds(), &[CodecKind::H264]);
        assert_eq!(CodecSelection::H265.kinds(), &[CodecKind::H265]);
        assert_eq!(
            CodecSelection::Both.kinds(),
            &[CodecKind::H264, CodecKind::H265]
        );
        assert_eq!(CodecSelection::parse("both"), Some(CodecSelection::Both));
        assert_eq!(CodecSelection::parse("hevc"), Some(CodecSelection::H265));
        assert_eq!(CodecSelection::parse("nope"), None);
    }
}
