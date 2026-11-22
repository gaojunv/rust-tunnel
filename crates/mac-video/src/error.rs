//! 错误类型与 OSStatus 可读化。
//!
//! VideoToolbox / CoreVideo / CoreMedia 的失败几乎都是 `OSStatus`（`i32`）。把它原样
//! 塞进错误信息对现场排查毫无帮助（`-12908` 与 `-12909` 只差一个数字），所以这里把
//! 已知的错误码翻成中文说明，未知码保留数值与十六进制。
//!
//! 本模块**不依赖任何 Apple API**：映射表是纯数字到文本，因此在 Windows/Linux 上
//! 也能单测。`OSStatus` 在非 Apple 平台只是普通 `i32`，不影响编译。

use thiserror::Error;

/// 本 crate 的结果别名。
pub type MacVideoResult<T> = Result<T, MacVideoError>;

/// macOS 视频数据面错误。
#[derive(Debug, Error)]
pub enum MacVideoError {
    /// 配置/参数错误（不是系统调用失败）。
    #[error("配置: {0}")]
    Config(String),
    /// 编码错误。
    #[error("编码: {0}")]
    Encode(String),
    /// 解码错误。
    #[error("解码: {0}")]
    Decode(String),
    /// 采集错误（保留给上层采集路径，本 crate 自身不采集）。
    #[error("采集: {0}")]
    Capture(String),
    /// 某个系统调用返回了非 `noErr` 的 `OSStatus`。
    #[error("{operation} 失败: {message}")]
    OsStatus {
        /// 失败的系统调用名（例如 `VTCompressionSessionCreate`）。
        operation: &'static str,
        /// 原始 `OSStatus`。
        status: i32,
        /// [`describe_os_status`] 的可读说明。
        message: String,
    },
}

impl MacVideoError {
    /// 用系统调用名与原始 `OSStatus` 构造错误（自动附带可读说明）。
    #[must_use]
    pub fn os_status(operation: &'static str, status: i32) -> Self {
        Self::OsStatus {
            operation,
            status,
            message: describe_os_status(status),
        }
    }
}

/// 把 `OSStatus` 翻成可读文本。
///
/// 覆盖 VideoToolbox 的 `-12900..=-12919` / `-12210..=-12214` 与 CoreVideo 的
/// `kCVReturn*`（`-6660..=-6680`）；其余码保留十进制与十六进制——`OSStatus` 也可能是
/// 四字符码（例如 `'!pri'`），十六进制能让人一眼认出。
#[must_use]
pub fn describe_os_status(status: i32) -> String {
    let name = match status {
        0 => Some("noErr"),
        -12900 => Some("kVTPropertyNotSupportedErr（属性不被支持）"),
        -12901 => Some("kVTPropertyReadOnlyErr（属性只读）"),
        -12902 => Some("kVTParameterErr（参数非法）"),
        -12903 => Some("kVTInvalidSessionErr（会话已失效）"),
        -12904 => Some("kVTAllocationFailedErr（分配失败）"),
        -12905 => Some("kVTPixelTransferNotSupportedErr（不支持该像素转换）"),
        -12906 => Some("kVTCouldNotFindVideoDecoderErr（找不到视频解码器）"),
        -12907 => Some("kVTCouldNotCreateInstanceErr（无法创建编解码实例）"),
        -12908 => Some("kVTCouldNotFindVideoEncoderErr（找不到视频编码器）"),
        -12909 => Some("kVTVideoDecoderBadDataErr（码流数据非法）"),
        -12910 => Some("kVTVideoDecoderUnsupportedDataFormatErr（不支持的码流格式）"),
        -12911 => Some("kVTVideoDecoderMalfunctionErr（解码器故障）"),
        -12912 => Some("kVTVideoEncoderMalfunctionErr（编码器故障）"),
        -12913 => Some("kVTVideoDecoderNotAvailableNowErr（解码器当前不可用）"),
        -12914 => Some("kVTImageRotationNotSupportedErr（不支持旋转）"),
        -12915 => Some("kVTVideoEncoderNotAvailableNowErr（编码器当前不可用）"),
        -12916 => Some("kVTFormatDescriptionChangeNotSupportedErr（不支持格式描述变更）"),
        -12917 => Some("kVTInsufficientSourceColorDataErr（源颜色信息不足）"),
        -12918 => Some("kVTCouldNotCreateColorCorrectionDataErr（无法创建校色数据）"),
        -12919 => Some("kVTColorSyncTransformConvertFailedErr（ColorSync 转换失败）"),
        -12210 => Some("kVTVideoDecoderAuthorizationErr（解码器未授权）"),
        -12211 => Some("kVTVideoEncoderAuthorizationErr（编码器未授权）"),
        -12212 => Some("kVTColorCorrectionPixelTransferFailedErr（校色像素传输失败）"),
        -12213 => Some("kVTMultiPassStorageIdentifierMismatchErr（多趟存储标识不匹配）"),
        -12214 => Some("kVTMultiPassStorageInvalidErr（多趟存储非法）"),
        -6660 => Some("kCVReturnInvalidArgument（参数非法）"),
        -6661 => Some("kCVReturnAllocationFailed（分配失败）"),
        -6662 => Some("kCVReturnUnsupported（不支持）"),
        -6663 => Some("kCVReturnInvalidDisplay（显示非法）"),
        -6664 => Some("kCVReturnDisplayLinkAlreadyRunning（显示链接已在运行）"),
        -6665 => Some("kCVReturnDisplayLinkNotRunning（显示链接未运行）"),
        -6680 => Some("kCVReturnInvalidPixelFormat（像素格式非法）"),
        -6681 => Some("kCVReturnInvalidSize（尺寸非法）"),
        -6682 => Some("kCVReturnInvalidPixelBufferAttributes（像素缓冲属性非法）"),
        -6683 => Some("kCVReturnPixelBufferNotOpenGLCompatible（像素缓冲与 OpenGL 不兼容）"),
        -6684 => Some("kCVReturnPixelBufferNotMetalCompatible（像素缓冲与 Metal 不兼容）"),
        -6689 => Some("kCVReturnWouldExceedAllocationThreshold（超过分配阈值）"),
        -6690 => Some("kCVReturnPoolAllocationFailed（像素缓冲池分配失败）"),
        -6691 => Some("kCVReturnInvalidPoolAttributes（像素缓冲池属性非法）"),
        _ => None,
    };
    match name {
        Some(text) => format!("{status}（{text}）"),
        None => format!(
            "{status}（0x{:08X}）",
            u32::from_ne_bytes(status.to_ne_bytes())
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// `noErr` 与常见 VT 错误码都能翻出中文说明。
    #[test]
    fn known_status_codes_are_described() {
        assert!(describe_os_status(0).contains("noErr"));
        assert!(describe_os_status(-12908).contains("找不到视频编码器"));
        assert!(describe_os_status(-12903).contains("会话已失效"));
        assert!(describe_os_status(-6661).contains("分配失败"));
    }

    /// 未知码保留十进制与十六进制，便于对照四字符码。
    #[test]
    fn unknown_status_codes_keep_hex() {
        let text = describe_os_status(-12345);
        assert!(text.contains("-12345"), "{text}");
        assert!(text.contains("0x"), "{text}");
    }

    /// [`MacVideoError::os_status`] 同时保留系统调用名与原始码。
    #[test]
    fn os_status_error_carries_operation_and_status() {
        let error = MacVideoError::os_status("VTCompressionSessionCreate", -12908);
        let text = error.to_string();
        assert!(text.contains("VTCompressionSessionCreate"), "{text}");
        assert!(text.contains("-12908"), "{text}");
        match error {
            MacVideoError::OsStatus { status, .. } => assert_eq!(status, -12908),
            other => panic!("期望 OsStatus，得到 {other:?}"),
        }
    }
}
