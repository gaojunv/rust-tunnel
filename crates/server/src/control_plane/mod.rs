//! 控制面模块：客户端注册、隧道流、端口信息与 ACME 配置。

/// ACME 配置与持久化。
pub mod acme_config;
/// 在线客户端注册表。
pub mod client_registry;
/// 端口信息与 Trojan 运行时状态。
pub mod port_info;
/// 控制通道 TCP/TLS 服务端。
pub mod server;
/// 全局 ServerState 聚合。
pub mod state;
/// 隧道字节流（AsyncRead/Write）。
pub mod tunnel_stream;

pub use acme_config::*;
pub use port_info::*;
pub use server::run_server;
pub use state::ControlMessageSender;
pub use state::ServerState;
