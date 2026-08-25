// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! 证书与 ACME 自动续签：HTTP-01/DNS-01 challenge、DNS provider 集成、
//! 证书存储与热更新。

pub mod acme;
mod domain;
mod error;
mod sni_resolver;

pub use domain::wildcard_for;
pub use error::{AcmeError, AcmeResult};
pub use sni_resolver::SniCertResolver;
