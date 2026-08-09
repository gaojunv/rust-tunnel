//! PTY 服务公共常量。

/// agent PTY 回环服务的默认端口（客户端在 45631 监听，服务端经隧道连接）。
/// 两端共享的协议常量，从 `rust-tunnel-client::pty` 上移至此。
pub const DEFAULT_PTY_PORT: u16 = 45631;
