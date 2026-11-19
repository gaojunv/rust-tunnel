//! 错误类型。

use thiserror::Error;

/// 音频数据面的错误。
#[derive(Debug, Error)]
pub enum AudioError {
    /// Windows API（COM / WASAPI）调用失败。
    #[error("WASAPI/COM 调用失败: {0}")]
    Windows(#[from] windows::core::Error),
    /// Opus 编解码失败。
    #[error("Opus 错误 {code}: {message}")]
    Opus {
        /// libopus 返回的错误码。
        code: i32,
        /// `opus_strerror` 的可读描述。
        message: String,
    },
    /// 当前构建目标没有内置 libopus（只有 `x86_64-pc-windows-gnu` 有预编译库）。
    #[error("当前构建目标未内置 libopus：{0}")]
    Unsupported(&'static str),
    /// 其它错误（参数非法、文件 IO 等）。
    #[error("{0}")]
    Message(String),
}

/// 音频数据面的结果别名。
pub type AudioResult<T> = Result<T, AudioError>;
