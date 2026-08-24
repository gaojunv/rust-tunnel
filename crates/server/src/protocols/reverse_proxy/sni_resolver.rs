//! SNI-based certificate resolver for shared HTTPS listeners.
//!
//! `SniCertResolver` 与 `wildcard_for` 已下移至 `rust-tunnel-pki`（证书关注点）；
//! 此处 re-export 保持 `crate::reverse_proxy::sni_resolver::*` 路径兼容。

pub use rust_tunnel_pki::{wildcard_for, SniCertResolver};
