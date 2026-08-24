// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub use rust_tunnel_pki as pki;
// 测试辅助设施（仅 cfg(test) 编译）
#[cfg(test)]
pub mod test_helpers;
pub use mgmt::api;
pub use pki::acme;
// agent 工作台已拆分为独立 crate；`agent` 路径保持兼容 re-export
pub use rust_tunnel_agent as agent;
pub mod config;
pub mod control_plane;
// 持久化层已拆分为独立 crate；`db` 路径保持兼容 re-export
pub use rust_tunnel_persistence as db;
// LLM 网关已拆分为独立 crate；`llm` 路径保持兼容 re-export
pub use rust_tunnel_llm as llm;
pub mod net;
pub use net::dns;
pub use net::mesh;
pub use rust_tunnel_protocols::listener;
pub mod mgmt;
pub use mgmt::auth;
pub use mgmt::dynamic_config;
pub use mgmt::logs;
pub use mgmt::stats;
pub use protocols::reverse_proxy;
pub use protocols::shadowsocks;
pub use protocols::trojan;
pub use rust_tunnel_protocols as protocols;
// trojan_runtime 已移回 server 侧（不属于 protocols 关注点）
pub mod trojan_runtime;

// 受控 re-export（替代 config::* 扁平化）：仅装配层与外部调用方需要的根符号
pub use config::ServerConfig;
pub use db::Database;
pub use net::dns::registry::DnsRegistry;
pub use net::dns::DnsServer;
