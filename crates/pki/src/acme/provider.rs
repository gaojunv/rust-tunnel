use crate::error::{AcmeError, AcmeResult};
use async_trait::async_trait;
use rustls::pki_types::CertificateDer;
use rustls::server::ServerConfig;
use std::sync::Arc;

/// Certificate entry containing PEM-encoded certificate data
#[derive(Debug, Clone)]
pub struct CertEntry {
    /// 叶子证书 PEM。
    pub cert_pem: String,
    /// 私钥 PEM。
    pub key_pem: String,
    /// 中间证书链 PEM，无则为 None。
    pub chain_pem: Option<String>,
    /// 过期时间，无则为 None。
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 证书来源。
    pub source: CertSource,
}

/// Source of the certificate
#[derive(Debug, Clone, PartialEq)]
pub enum CertSource {
    /// ACME 签发。
    Acme,
    /// 自签名。
    SelfSigned,
    /// 手动上传。
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

/// 合并叶子证书与中间证书链 PEM。
///
/// ACME 签发时叶子证书存 cert.pem、中间证书存 chain.pem，
/// 构建 TLS 配置时必须拼接完整链，否则只发叶子证书，
/// 严格校验的客户端无法构建到根 CA 的链路（握手失败）。
fn full_chain_pem(entry: &CertEntry) -> String {
    match &entry.chain_pem {
        Some(chain) if !chain.trim().is_empty() => {
            let mut pem = entry.cert_pem.clone();
            if !pem.ends_with('\n') {
                pem.push('\n');
            }
            pem.push_str(chain);
            pem
        }
        _ => entry.cert_pem.clone(),
    }
}

/// 从证书条目创建 rustls `ServerConfig`。
///
/// # Errors
///
/// 当证书链/私钥 PEM 解析失败或 `ServerConfig` 构建失败时返回错误。
pub fn create_server_config_from_entry(entry: &CertEntry) -> AcmeResult<Arc<ServerConfig>> {
    use rustls_pemfile::certs;

    // Parse certificate chain（叶子 + 中间证书）
    let cert_chain: Vec<CertificateDer<'static>> = certs(&mut full_chain_pem(entry).as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AcmeError::ParseCertificate(e.to_string()))?;

    // Parse private key
    let key_der = rustls_pemfile::private_key(&mut entry.key_pem.as_bytes())
        .map_err(|e| AcmeError::ParsePrivateKey(e.to_string()))?
        .ok_or(AcmeError::NoPrivateKey)?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key_der)
        .map_err(|e| AcmeError::ServerConfig(e.to_string()))?;
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
) -> AcmeResult<std::sync::Arc<rustls::sign::CertifiedKey>> {
    use rustls::pki_types::CertificateDer;
    use rustls::sign::CertifiedKey;
    use rustls_pemfile::certs;
    use std::sync::Arc;

    let cert_chain: Vec<CertificateDer<'static>> = certs(&mut full_chain_pem(entry).as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AcmeError::ParseCertificate(e.to_string()))?;

    let key_der = rustls_pemfile::private_key(&mut entry.key_pem.as_bytes())
        .map_err(|e| AcmeError::ParsePrivateKey(e.to_string()))?
        .ok_or(AcmeError::NoPrivateKey)?;

    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key_der)
        .map_err(|e| AcmeError::UnsupportedKeyType(e.to_string()))?;

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

    // 回归：ACME 签发时叶子存 cert.pem、中间证书存 chain.pem，
    // 构建 TLS 配置必须拼接完整链，否则客户端无法验证到根 CA
    #[test]
    fn test_build_certified_key_includes_chain_pem() {
        let (leaf_pem, key_pem) = gen_self_signed_pem();
        let (intermediate_pem, _) = gen_self_signed_pem();
        let entry = CertEntry {
            cert_pem: leaf_pem,
            key_pem,
            chain_pem: Some(intermediate_pem),
            expires_at: None,
            source: CertSource::Acme,
        };
        let ck = build_certified_key(&entry).expect("should build CertifiedKey");
        assert_eq!(ck.cert.len(), 2, "应包含叶子证书 + 中间证书");
    }

    #[test]
    fn test_create_server_config_includes_chain_pem() {
        let (leaf_pem, key_pem) = gen_self_signed_pem();
        let (intermediate_pem, _) = gen_self_signed_pem();
        let entry = CertEntry {
            cert_pem: leaf_pem,
            key_pem,
            chain_pem: Some(intermediate_pem),
            expires_at: None,
            source: CertSource::Acme,
        };
        // ServerConfig 不暴露证书列表，通过合并函数间接验证
        let pem = full_chain_pem(&entry);
        let count = rustls_pemfile::certs(&mut pem.as_bytes()).count();
        assert_eq!(count, 2, "合并后应包含 2 张证书");
        assert!(create_server_config_from_entry(&entry).is_ok());
    }

    #[test]
    fn test_full_chain_pem_ignores_empty_chain() {
        let entry = CertEntry {
            cert_pem: "leaf\n".to_string(),
            key_pem: String::new(),
            chain_pem: Some("   \n".to_string()),
            expires_at: None,
            source: CertSource::Manual,
        };
        assert_eq!(full_chain_pem(&entry), "leaf\n");
    }
}
