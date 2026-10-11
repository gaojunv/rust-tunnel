//! 远程控制引擎的错误类型。

/// 远程控制引擎的统一错误。
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// 底层 IO 错误。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// 会话帧编解码错误（长度越界、lane 非法、bincode 解码失败等）。
    #[error("session codec error: {0}")]
    Codec(String),
    /// 会话握手被拒（版本不符、对端未接受等）。
    #[error("session handshake rejected: {0}")]
    Handshake(String),
    /// 会话鉴权失败（token 错误、过期、重放）。
    #[error("session authentication failed: {0}")]
    Auth(String),
    /// 屏幕编码/解码错误。
    #[error("screen codec error: {0}")]
    Encode(String),
    /// 平台侧错误（截屏、输入注入由上层实现返回）。
    #[error("platform error: {0}")]
    Platform(String),
    /// 会合/中继信令错误。
    #[error("signaling error: {0}")]
    Signaling(String),
    /// 会话尚未建立。
    #[error("session not established: {0}")]
    NotEstablished(String),
    /// 操作被取消（UI 一键中断或控制通道断开）。
    #[error("operation cancelled")]
    Cancelled,
    /// 远程控制不可用（服务端未启用 RustDesk，或被控端未开启）。
    #[error("remote control unavailable: {0}")]
    Unavailable(String),
}

/// 本 crate 的 `Result` 别名。
pub type RemoteResult<T> = Result<T, RemoteError>;
