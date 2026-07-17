use async_trait::async_trait;
use rustls::pki_types::CertificateDer;
use rustls::server::ServerConfig;
use std::sync::Arc;

/// Certificate entry containing PEM-encoded certificate data
#[derive(Debug, Clone)]
pub struct CertEntry {
    pub cert_pem: String,
    pub key_pem: String,
    pub chain_pem: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub source: CertSource,
}

/// Source of the certificate
#[derive(Debug, Clone, PartialEq)]
pub enum CertSource {
    Acme,
    SelfSigned,
    Manual,
}

/// Provider trait for obtaining TLS certificates
#[async_trait]
pub trait CertificateProvider: Send + Sync {
    /// Get certificate entry for a domain
    async fn get_certificate(&self, domain: &str) -> Option<CertEntry>;

    /// Get rustls ServerConfig for a domain
    async fn get_tls_server_config(&self, domain: &str) -> Option<Arc<ServerConfig>>;
}

/// Helper function to create ServerConfig from CertEntry
pub fn create_server_config_from_entry(entry: &CertEntry) -> anyhow::Result<Arc<ServerConfig>> {
    use rustls_pemfile::certs;

    // Parse certificate chain
    let cert_chain: Vec<CertificateDer<'static>> = certs(&mut entry.cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Failed to parse certificate: {}", e))?;

    // Parse private key
    let key_der = rustls_pemfile::private_key(&mut entry.key_pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("Failed to parse private key: {}", e))?
        .ok_or_else(|| anyhow::anyhow!("No private key found"))?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key_der)
        .map_err(|e| anyhow::anyhow!("Failed to create server config: {}", e))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

/// Certificate coverage result for a domain lookup.
///
/// Returned by `CertificateManager::find_covering_cert` to indicate
/// whether a domain is covered by an exact-match or wildcard certificate.
#[derive(Debug, Clone, PartialEq)]
pub enum CertCoverage {
    /// Exact-match certificate found for the queried domain.
    Exact,
    /// A wildcard certificate covers the queried domain.
    /// Contains the wildcard pattern (e.g. `*.example.com`).
    Wildcard(String),
}

/// Build a rustls `CertifiedKey` from a `CertEntry`.
///
/// # Errors
///
/// Returns an error if the PEM data cannot be parsed or if the signing key
/// is unsupported by the crypto provider.
pub fn build_certified_key(
    entry: &CertEntry,
) -> anyhow::Result<std::sync::Arc<rustls::sign::CertifiedKey>> {
    use rustls::pki_types::CertificateDer;
    use rustls::sign::CertifiedKey;
    use rustls_pemfile::certs;
    use std::sync::Arc;

    let cert_chain: Vec<CertificateDer<'static>> = certs(&mut entry.cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Failed to parse certificate: {}", e))?;

    let key_der = rustls_pemfile::private_key(&mut entry.key_pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("Failed to parse private key: {}", e))?
        .ok_or_else(|| anyhow::anyhow!("No private key found"))?;

    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key_der)
        .map_err(|e| anyhow::anyhow!("Unsupported key type: {}", e))?;

    Ok(Arc::new(CertifiedKey::new(cert_chain, signing_key)))
}

#[cfg(test)]
mod provider_tests {
    use super::*;

    // 生成一份自签证书 PEM 用于测试
    fn gen_self_signed_pem() -> (String, String) {
        use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let params = CertificateParams::new(vec!["test.local".to_string()]).unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        (cert.pem(), key_pair.serialize_pem())
    }

    #[test]
    fn test_build_certified_key_valid_pem() {
        let (cert_pem, key_pem) = gen_self_signed_pem();
        let entry = CertEntry {
            cert_pem,
            key_pem,
            chain_pem: None,
            expires_at: None,
            source: CertSource::Manual,
        };
        let ck = build_certified_key(&entry).expect("should build CertifiedKey");
        assert!(!ck.cert.is_empty());
    }

    #[test]
    fn test_build_certified_key_invalid_pem() {
        let entry = CertEntry {
            cert_pem: "not a cert".to_string(),
            key_pem: "not a key".to_string(),
            chain_pem: None,
            expires_at: None,
            source: CertSource::Manual,
        };
        assert!(build_certified_key(&entry).is_err());
    }
}
