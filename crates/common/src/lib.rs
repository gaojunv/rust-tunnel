// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! rust-tunnel 共用库：协议、加解密、TLS、日志、mesh 类型与基础设施。

/// 加解密模块（API Key 对称加密等）。
pub mod crypto;
/// 统一错误类型与结果别名。
pub mod error;
/// 统一出站 HTTP 客户端工厂。
pub mod http_client;
/// 日志初始化。
pub mod logging;
/// Mesh 网络类型定义。
pub mod mesh_types;
/// 控制通道协议定义。
pub mod protocol;
/// PTY 常量。
pub mod pty;
/// STUN 协议解析与构造。
pub mod stun;
/// TLS 证书与连接工具。
pub mod tls;

pub use error::*;
pub use logging::*;
pub use mesh_types::*;
pub use protocol::*;
pub use pty::DEFAULT_PTY_PORT;
pub use stun::*;
pub use tls::*;
