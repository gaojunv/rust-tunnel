//! SNI-based certificate resolver for shared HTTPS listeners.

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::sync::Arc;

use crate::acme::CertificateManager;

/// Compute the one-level wildcard pattern for a domain.
///
/// - `foo.example.com` -> `Some("*.example.com")`
/// - `foo.bar.example.com` -> `Some("*.bar.example.com")` (only one level up)
/// - `example.com` -> `None` (would produce `*.top`, refused)
#[must_use]
pub fn wildcard_for(domain: &str) -> Option<String> {
    let (_, rest) = domain.split_once('.')?;
    if rest.contains('.') {
        Some(format!("*.{rest}"))
    } else {
        None
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_for_three_labels() {
        assert_eq!(
            wildcard_for("foo.example.com"),
            Some("*.example.com".to_string())
        );
    }

    #[test]
    fn wildcard_for_four_labels_one_level() {
        assert_eq!(
            wildcard_for("foo.bar.example.com"),
            Some("*.bar.example.com".to_string())
        );
    }

    #[test]
    fn wildcard_for_two_labels_refused() {
        assert_eq!(wildcard_for("example.com"), None);
    }

    #[test]
    fn wildcard_for_single_label() {
        assert_eq!(wildcard_for("localhost"), None);
    }

    #[test]
    fn wildcard_for_trailing_dot() {
        // 严格串处理：不特殊处理 trailing dot
        assert_eq!(
            wildcard_for("foo.example.com."),
            Some("*.example.com.".to_string())
        );
    }
}
