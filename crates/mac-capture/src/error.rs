//! 错误类型。
//!
//! 与 `mac-audio` / `mac-video` 的取舍一致：所有面向现场的失败都给**中文**说明，
//! `client-gui` 的适配器可以把 `to_string()` 直接当提示语用。本模块不依赖任何
//! Apple API，因此非 macOS 平台也能编译（那里只会用到 [`MacCaptureError::Unsupported`]）。

use thiserror::Error;

/// macOS 采集层的错误。
#[derive(Debug, Error)]
pub enum MacCaptureError {
    /// 平台调用失败（Objective-C / CoreGraphics / 参数校验）。
    #[error("macOS 采集调用失败：{0}")]
    Platform(String),
    /// ScreenCaptureKit 返回的错误。
    #[error("ScreenCaptureKit 错误 {code}: {message}")]
    ScreenCaptureKit {
        /// `NSError.code`。
        code: i64,
        /// `NSError.localizedDescription`。
        message: String,
    },
    /// 缺少「屏幕录制」权限（ScreenCaptureKit 的前置条件）。
    #[error("缺少「屏幕录制」权限：打开 系统设置 → 隐私与安全性 → 屏幕录制，勾选本应用后重启再试")]
    ScreenRecordingPermission,
    /// 等待系统回调 / 首帧超时。
    #[error("等待超时：{0}")]
    Timeout(&'static str),
    /// 采集流故障后的重建退避期内再次请求采集。
    #[error("采集流重建退避中（还剩 {remaining_ms} ms）：{reason}")]
    Backoff {
        /// 距离下一次允许重建的剩余毫秒数。
        remaining_ms: u64,
        /// 上一次失败的原因（中文）。
        reason: String,
    },
    /// 当前构建 / 平台不支持。
    #[error("当前构建不支持：{0}")]
    Unsupported(&'static str),
    /// 其它错误（参数非法、线程创建失败等）。
    #[error("{0}")]
    Message(String),
}

/// 采集层的结果别名。
pub type MacCaptureResult<T> = Result<T, MacCaptureError>;
