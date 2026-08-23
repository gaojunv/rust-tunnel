// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod pki;
pub use mgmt::api;
pub use pki::acme;
pub mod agent;
pub mod config;
pub mod control_plane;
pub use control_plane as control;
pub use control_plane::client_registry;
pub use control_plane::tunnel_stream;
pub use persistence::db;
pub mod llm;
pub mod net;
pub mod persistence;
pub use net::dns;
pub use net::listener;
pub use net::mesh;
pub mod mgmt;
pub use mgmt::auth;
pub use mgmt::dynamic_config;
pub use mgmt::logs;
pub use mgmt::stats;
pub mod protocols;
pub use protocols::reverse_proxy;
pub use protocols::shadowsocks;
pub use protocols::trojan;
pub use protocols::trojan_runtime;

pub use config::*;
pub use db::Database;
pub use net::dns::registry::DnsRegistry;
pub use net::dns::DnsServer;
