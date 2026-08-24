//! SNI-based certificate resolver for shared HTTPS listeners.

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::sync::Arc;

use crate::acme::CertificateManager;

/// 按 TLS ClientHello 的 SNI 从 [`CertificateManager`] 解析证书（含一层通配回退）。
pub struct SniCertResolver {
    manager: Arc<CertificateManager>,
}

impl SniCertResolver {
    #[must_use]
    pub fn new(manager: Arc<CertificateManager>) -> Self {
        Self { manager }
    }
}

impl std::fmt::Debug for SniCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SniCertResolver").finish()
    }
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = client_hello.server_name()?;
        // rustls already lowercases SNI before delivering it here, but
        // resolve_certified_key normalizes again defensively — cheap and
        // avoids surprises if the caller ever changes.
        let ck = self.manager.resolve_certified_key(sni);
        if ck.is_none() {
            tracing::debug!("SNI resolver: no cert for {}", sni);
        }
        ck
    }
}
