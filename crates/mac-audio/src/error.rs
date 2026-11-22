//! 错误类型。

use thiserror::Error;

/// 音频数据面的错误。
#[derive(Debug, Error)]
pub enum MacAudioError {
    /// 平台调用失败（Objective-C / CoreGraphics / 参数校验）。
    #[error("macOS 音频调用失败：{0}")]
    Platform(String),
    /// ScreenCaptureKit 返回的错误。
    #[error("ScreenCaptureKit 错误 {code}: {message}")]
    ScreenCaptureKit {
        /// `NSError.code`。
        code: i64,
        /// `NSError.localizedDescription`。
        message: String,
    },
    /// 缺少「屏幕录制」权限（ScreenCaptureKit 采集系统声音的前置条件）。
    #[error("缺少「屏幕录制」权限：打开 系统设置 → 隐私与安全性 → 屏幕录制，勾选本应用后重启再试")]
    ScreenRecordingPermission,
    /// AudioQueue 调用返回非 0 `OSStatus`。
    #[error("AudioQueue 错误 {status}: {context}")]
    AudioQueue {
        /// `OSStatus`（AudioToolbox 的错误码）。
        status: i32,
        /// 出错的那一步（中文）。
        context: &'static str,
    },
    /// Opus 编解码失败。
    #[error("Opus 错误 {code}: {message}")]
    Opus {
        /// libopus 返回的错误码。
        code: i32,
        /// `opus_strerror` 的可读描述。
        message: String,
    },
    /// 当前构建不支持（未启用 `opus` feature、或不是 macOS 目标）。
    #[error("当前构建不支持：{0}")]
    Unsupported(&'static str),
    /// 等待系统回调超时。
    #[error("等待超时：{0}")]
    Timeout(&'static str),
    /// 其它错误（参数非法、文件 IO 等）。
    #[error("{0}")]
    Message(String),
}

impl MacAudioError {
    /// 用 `OSStatus` 组装 [`MacAudioError::AudioQueue`]。
    #[must_use]
    pub const fn audio_queue(status: i32, context: &'static str) -> Self {
        Self::AudioQueue { status, context }
    }
}

/// 音频数据面的结果别名。
pub type MacAudioResult<T> = Result<T, MacAudioError>;
