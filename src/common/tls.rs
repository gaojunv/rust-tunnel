//! TLS utilities for rust-tunnel
//!
//! This module provides functions for:
//! 1. Generating self-signed certificates on the server
//! 2. Setting up TLS listeners for the server
//! 3. Setting up TLS connectors for the client (with auto-certificate fetching)

use std::fs;
use std::io::{BufReader, Write};
use std::path::Path;
use std::sync::Arc;

use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ServerConfig;
use rustls::{ClientConfig, RootCertStore};
use rustls_pemfile::certs;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{debug, info};

use crate::common::{TunnelError, TunnelResult};

/// Certificate and private key pair for TLS
pub struct TlsCertPair {
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

/// Generate a self-signed certificate for the server
/// The certificate is valid for 1 year and uses common name "rust-tunnel-server"
pub fn generate_self_signed_cert() -> Result<(Certificate, KeyPair), String> {
    let mut params = CertificateParams::new(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
        "rust-tunnel-server".to_string(),
    ])
    .map_err(|e| format!("Failed to create certificate params: {}", e))?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "rust-tunnel-server");
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(365);

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ED25519)
        .map_err(|e| format!("Failed to generate key pair: {}", e))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("Failed to generate certificate: {}", e))?;

    Ok((cert, key_pair))
}

/// Load certificate and key from files, or generate and save them if they don't exist
pub fn load_or_generate_cert(cert_path: &str, key_path: &str) -> TunnelResult<TlsCertPair> {
    let cert_path = Path::new(cert_path);
    let key_path = Path::new(key_path);

    // Try to load existing cert and key
    if cert_path.exists() && key_path.exists() {
        debug!("Loading existing TLS certificate from {:?}", cert_path);
        return load_cert_from_files(cert_path, key_path);
    }

    // Generate new certificate
    info!("Generating new self-signed TLS certificate...");
    let (cert, key_pair) = generate_self_signed_cert().map_err(TunnelError::Tls)?;

    // Ensure parent directories exist
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent).map_err(TunnelError::Io)?;
    }
    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent).map_err(TunnelError::Io)?;
    }

    // Save certificate and key
    let mut cert_file = fs::File::create(cert_path).map_err(TunnelError::Io)?;
    cert_file
        .write_all(cert.pem().as_bytes())
        .map_err(TunnelError::Io)?;

    let mut key_file = fs::File::create(key_path).map_err(TunnelError::Io)?;
    key_file
        .write_all(key_pair.serialize_pem().as_bytes())
        .map_err(TunnelError::Io)?;

    info!("TLS certificate saved to: {}", cert_path.display());
    info!("TLS private key saved to: {}", key_path.display());

    load_cert_from_files(cert_path, key_path)
}

/// Load certificate and key from PEM files
pub fn load_cert_from_files(cert_path: &Path, key_path: &Path) -> TunnelResult<TlsCertPair> {
    // Load certificates
    let cert_file = fs::File::open(cert_path).map_err(TunnelError::Io)?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<CertificateDer<'static>> = certs(&mut cert_reader)
        .collect::<Result<_, _>>()
        .map_err(|e| TunnelError::Tls(format!("Failed to load certificate: {}", e)))?;

    if certs.is_empty() {
        return Err(TunnelError::Tls("No certificates found in file".into()));
    }

    // Load private key (supports PKCS#8, PKCS#1 RSA, and SEC1 EC formats)
    let key_file = fs::File::open(key_path).map_err(TunnelError::Io)?;
    let mut key_reader = BufReader::new(key_file);
    let item = rustls_pemfile::read_one(&mut key_reader)
        .map_err(|e| TunnelError::Tls(format!("Failed to read private key: {}", e)))?
        .ok_or_else(|| TunnelError::Tls("No private key found in file".into()))?;

    let key = match item {
        rustls_pemfile::Item::Pkcs8Key(key) => PrivateKeyDer::Pkcs8(key),
        rustls_pemfile::Item::Pkcs1Key(key) => PrivateKeyDer::Pkcs1(key),
        rustls_pemfile::Item::Sec1Key(key) => PrivateKeyDer::Sec1(key),
        _ => {
            return Err(TunnelError::Tls(
                "Unsupported private key format in file".into(),
            ))
        }
    };

    Ok(TlsCertPair { certs, key })
}

/// Create a server TLS config for the control channel
pub fn create_server_config(cert_pair: TlsCertPair) -> TunnelResult<Arc<ServerConfig>> {
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_pair.certs, cert_pair.key)
        .map_err(|e| TunnelError::Tls(format!("Failed to create server TLS config: {}", e)))?;

    Ok(Arc::new(config))
}

/// Create a client TLS config (accepts any cert - TOFU style)
/// This is for clients that auto-accept the server's certificate
pub fn create_insecure_client_config() -> TunnelResult<Arc<ClientConfig>> {
    // Create a client config that accepts any certificate (TOFU - Trust On First Use)
    // In production, you should verify certificates properly
    #[derive(Debug)]
    struct NoCertificateVerifier;

    impl rustls::client::danger::ServerCertVerifier for NoCertificateVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            // Accept any certificate (TOFU mode)
            // This is intentional for auto-certificate acceptance
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::RSA_PKCS1_SHA384,
                rustls::SignatureScheme::RSA_PKCS1_SHA512,
                rustls::SignatureScheme::RSA_PSS_SHA256,
                rustls::SignatureScheme::RSA_PSS_SHA384,
                rustls::SignatureScheme::RSA_PSS_SHA512,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            ]
        }
    }

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertificateVerifier))
        .with_no_client_auth();

    Ok(Arc::new(config))
}

/// Create a secure client TLS config with root certificates
/// Use this when you have proper CA certificates
pub fn create_secure_client_config() -> TunnelResult<Arc<ClientConfig>> {
    let mut root_store = RootCertStore::empty();

    // Add system root certificates
    for cert in rustls_native_certs::load_native_certs()
        .map_err(|e| TunnelError::Tls(format!("Failed to load system certificates: {}", e)))?
    {
        root_store
            .add(cert)
            .map_err(|e| TunnelError::Tls(format!("Failed to add root certificate: {}", e)))?;
    }

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(Arc::new(config))
}

/// Establish a TLS connection to the server (insecure/TOFU mode)
/// This will automatically accept any server certificate (for self-signed certs)
pub async fn connect_tls_insecure(
    addr: &str,
    server_name: &str,
) -> TunnelResult<tokio_rustls::client::TlsStream<TcpStream>> {
    debug!("Connecting to {} with TLS (insecure mode)", addr);

    let stream = TcpStream::connect(addr).await.map_err(TunnelError::Io)?;

    let config = create_insecure_client_config()?;
    let connector = TlsConnector::from(config);

    // Convert to owned String to get 'static lifetime
    let server_name_owned = server_name.to_string();
    let server_name = ServerName::try_from(server_name_owned)
        .map_err(|_| TunnelError::Tls(format!("Invalid server name: {}", server_name)))?;

    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| TunnelError::Tls(format!("TLS handshake failed: {}", e)))?;

    debug!("TLS connection established successfully");
    Ok(tls_stream)
}

/// Establish a TLS connection to the server (secure mode with CA verification)
pub async fn connect_tls_secure(
    addr: &str,
    server_name: &str,
) -> TunnelResult<tokio_rustls::client::TlsStream<TcpStream>> {
    debug!("Connecting to {} with TLS (secure mode)", addr);

    let stream = TcpStream::connect(addr).await.map_err(TunnelError::Io)?;

    let config = create_secure_client_config()?;
    let connector = TlsConnector::from(config);

    // Convert to owned String to get 'static lifetime
    let server_name_owned = server_name.to_string();
    let server_name = ServerName::try_from(server_name_owned)
        .map_err(|_| TunnelError::Tls(format!("Invalid server name: {}", server_name)))?;

    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| TunnelError::Tls(format!("TLS handshake failed: {}", e)))?;

    debug!("TLS connection established successfully");
    Ok(tls_stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_generate_self_signed_cert() {
        let result = generate_self_signed_cert();
        assert!(
            result.is_ok(),
            "Failed to generate self-signed cert: {:?}",
            result.err()
        );

        let (cert, key_pair) = result.unwrap();

        // Verify the cert can be serialized to PEM
        let cert_pem = cert.pem();
        assert!(!cert_pem.is_empty(), "Failed to serialize cert to PEM");

        // Verify the key can be serialized to PEM
        let key_pem = key_pair.serialize_pem();
        assert!(!key_pem.is_empty(), "Key PEM is empty");
    }

    #[test]
    fn test_load_or_generate_cert_new_files() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("cert.pem");
        let key_path = tmp_dir.path().join("key.pem");

        let result = load_or_generate_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap());
        assert!(
            result.is_ok(),
            "Failed to generate cert: {:?}",
            result.err()
        );

        // Verify files were created
        assert!(cert_path.exists(), "Cert file not created");
        assert!(key_path.exists(), "Key file not created");

        let cert_pair = result.unwrap();
        assert!(!cert_pair.certs.is_empty(), "No certificates loaded");
    }

    #[test]
    fn test_load_or_generate_cert_existing_files() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("cert.pem");
        let key_path = tmp_dir.path().join("key.pem");

        // First call generates the files
        let result1 =
            load_or_generate_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap());
        assert!(result1.is_ok());

        // Second call should load existing files
        let result2 =
            load_or_generate_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap());
        assert!(
            result2.is_ok(),
            "Failed to load existing cert: {:?}",
            result2.err()
        );

        let cert_pair = result2.unwrap();
        assert!(
            !cert_pair.certs.is_empty(),
            "No certificates loaded from existing files"
        );
    }

    #[test]
    fn test_load_cert_from_nonexistent_files() {
        let result = load_cert_from_files(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_load_cert_from_empty_files() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("cert.pem");
        let key_path = tmp_dir.path().join("key.pem");

        // Create empty files
        fs::write(&cert_path, "").unwrap();
        fs::write(&key_path, "").unwrap();

        let result = load_cert_from_files(&cert_path, &key_path);
        assert!(result.is_err(), "Should fail loading empty cert files");
    }

    #[test]
    fn test_create_server_config() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("cert.pem");
        let key_path = tmp_dir.path().join("key.pem");

        let cert_pair =
            load_or_generate_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap()).unwrap();

        let result = create_server_config(cert_pair);
        assert!(
            result.is_ok(),
            "Failed to create server config: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_create_insecure_client_config() {
        let result = create_insecure_client_config();
        assert!(
            result.is_ok(),
            "Failed to create insecure client config: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_load_or_generate_cert_creates_parent_dirs() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("deep").join("nested").join("cert.pem");
        let key_path = tmp_dir.path().join("deep").join("nested").join("key.pem");

        let result = load_or_generate_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap());
        assert!(
            result.is_ok(),
            "Failed to generate cert with nested dirs: {:?}",
            result.err()
        );
        assert!(cert_path.exists(), "Cert file not created in nested dir");
        assert!(key_path.exists(), "Key file not created in nested dir");
    }

    #[test]
    fn test_tls_cert_pair_fields() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("cert.pem");
        let key_path = tmp_dir.path().join("key.pem");

        let cert_pair =
            load_or_generate_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap()).unwrap();

        // Should have at least one certificate
        assert!(!cert_pair.certs.is_empty());

        // Key should be valid (non-zero length when serialized)
        // PrivateKeyDer variants wrap Der<'a> which can be checked via secret_pkcs8_der/secret_pkcs1_der
        let key_bytes: &[u8] = match &cert_pair.key {
            PrivateKeyDer::Pkcs8(data) => data.secret_pkcs8_der(),
            PrivateKeyDer::Pkcs1(data) => data.secret_pkcs1_der(),
            PrivateKeyDer::Sec1(data) => data.secret_sec1_der(),
            _ => panic!("Unexpected key type"),
        };
        assert!(!key_bytes.is_empty(), "Key data is empty");
    }
}
