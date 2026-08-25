// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! rust-tunnel 代理协议实现：Shadowsocks、Trojan、反向代理（HTTP/TCP 路由、
//! SNI 分发、上游转发）、连接监听编排。
//!
//! 与 `rust-tunnel-server` 的依赖通过 trait 倒置消除：
//! - [`PortRegistry`]：端口注册/连接计数（server 侧 `ServerState` 实现）
//! - [`TunnelOpener`]：隧道打开（server 侧 `ClientRegistry` 实现）

pub mod listener;
pub mod reverse_proxy;
pub mod shadowsocks;
pub mod trojan;

mod port_registry;
mod tunnel_opener;

pub use port_registry::{PortInfo, PortRegistry, PortType};
pub use tunnel_opener::TunnelOpener;
