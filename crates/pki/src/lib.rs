//! 证书与 ACME 自动续签：HTTP-01/DNS-01 challenge、DNS provider 集成、
//! 证书存储与热更新。

pub mod acme;
mod domain;
mod error;
mod sni_resolver;

pub use domain::wildcard_for;
pub use error::{AcmeError, AcmeResult};
pub use sni_resolver::SniCertResolver;
