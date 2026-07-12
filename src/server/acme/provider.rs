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

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key_der)
        .map_err(|e| anyhow::anyhow!("Failed to create server config: {}", e))?;

    Ok(Arc::new(config))
}
