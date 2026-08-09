use async_trait::async_trait;
use std::time::Duration;
use tracing::{debug, info, warn};

use super::{DnsChallengeSolver, DnsProviderConfig};

/// Aliyun DNS API base URL
const ALIYUN_DNS_API: &str = "https://alidns.aliyuncs.com/";

/// Parse a domain into the root domain and RR (host record) for Aliyun DNS API.
///
/// - `"test.example.com"` -> `("example.com", "test")`
/// - `"example.com"`      -> `("example.com", "@")`
/// - `"*.example.com"`    -> `("example.com", "*")`
fn parse_domain(domain: &str) -> anyhow::Result<(String, String)> {
    // Strip wildcard prefix if present
    let clean_domain = domain.strip_prefix("*.").unwrap_or(domain);

    let parts: Vec<&str> = clean_domain.split('.').collect();

    if parts.len() < 2 {
        return Err(anyhow::anyhow!("Invalid domain format: {}", domain));
    }

    // Root domain is the last two parts (e.g., "example.com")
    let main_domain = parts[parts.len() - 2..].join(".");

    // Determine the RR (host record)
    let rr = if domain.starts_with("*.") {
        "*".to_string()
    } else if parts.len() > 2 {
        parts[..parts.len() - 2].join(".")
    } else {
        "@".to_string()
    };

    Ok((main_domain, rr))
}

/// Aliyun DNS challenge solver
pub struct AliyunDnsSolver {
    access_key_id: String,
    access_key_secret: String,
    client: reqwest::Client,
}

impl AliyunDnsSolver {
    /// Create a new Aliyun DNS solver
    pub fn new(config: &DnsProviderConfig) -> Self {
        Self {
            access_key_id: config.api_key.clone(),
            access_key_secret: config.api_secret.clone().unwrap_or_default(),
            client: reqwest::Client::new(),
        }
    }

    /// Generate Aliyun API signature parameters using HMAC-SHA1
    fn sign_request(&self, params: &mut Vec<(String, String)>) {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        use sha1::Sha1;

        // Add common parameters
        params.push(("Format".to_string(), "JSON".to_string()));
        params.push(("Version".to_string(), "2015-01-09".to_string()));
        params.push(("AccessKeyId".to_string(), self.access_key_id.clone()));
        params.push(("SignatureMethod".to_string(), "HMAC-SHA1".to_string()));
        params.push((
            "Timestamp".to_string(),
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        ));
        params.push(("SignatureVersion".to_string(), "1.0".to_string()));
        params.push((
            "SignatureNonce".to_string(),
            uuid::Uuid::new_v4().to_string(),
        ));

        // Sort parameters by key
        params.sort_by(|a, b| a.0.cmp(&b.0));

        // Build canonical query string
        let query_string: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        // Build string to sign
        let string_to_sign = format!(
            "GET&{}&{}",
            urlencoding::encode("/"),
            urlencoding::encode(&query_string)
        );

        // Debug logging for signature calculation
        debug!("Canonical query string: {}", query_string);
        debug!("String to sign: {}", string_to_sign);

        // Compute HMAC-SHA1 signature
        type HmacSha1 = Hmac<Sha1>;
        let mut mac = HmacSha1::new_from_slice(format!("{}&", self.access_key_secret).as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(string_to_sign.as_bytes());
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        // Append signature
        params.push(("Signature".to_string(), signature));
    }

    /// Call the Aliyun DNS API with the given action and extra parameters
    async fn call_api(
        &self,
        action: &str,
        extra_params: Vec<(String, String)>,
    ) -> anyhow::Result<serde_json::Value> {
        let mut params = vec![("Action".to_string(), action.to_string())];
        params.extend(extra_params);

        self.sign_request(&mut params);

        let query_string: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("{}?{}", ALIYUN_DNS_API, query_string);

        debug!("Calling Aliyun API: action={}", action);
        debug!("Request URL: {}", url);

        let response = self.client.get(&url).send().await?;

        let status = response.status();
        let body: serde_json::Value = response.json().await?;

        if !status.is_success() {
            return Err(anyhow::anyhow!("Aliyun API error: {} - {}", status, body));
        }

        // Check for Aliyun business-level errors
        if let Some(error_code) = body.get("Code") {
            return Err(anyhow::anyhow!(
                "Aliyun API error: {} - {}",
                error_code,
                body.get("Message")
                    .unwrap_or(&serde_json::Value::String("Unknown error".to_string()))
            ));
        }

        Ok(body)
    }

    /// Find an existing TXT record by domain and value
    async fn find_txt_record(&self, domain: &str, value: &str) -> anyhow::Result<Option<String>> {
        let (main_domain, rr) = parse_domain(domain)?;

        let params = vec![
            ("DomainName".to_string(), main_domain.clone()),
            ("RR".to_string(), rr.clone()),
            ("Type".to_string(), "TXT".to_string()),
        ];

        let body = self.call_api("DescribeDomainRecords", params).await?;

        if let Some(records) = body.get("DomainRecords").and_then(|r| r.get("Record")) {
            if let Some(arr) = records.as_array() {
                for record in arr {
                    if let (Some(record_value), Some(record_id)) =
                        (record.get("Value"), record.get("RecordId"))
                    {
                        if record_value.as_str() == Some(value) {
                            if let Some(id) = record_id.as_str() {
                                return Ok(Some(id.to_string()));
                            }
                        }
                    }
                }
            }
        }

        Ok(None)
    }
}

#[async_trait]
impl DnsChallengeSolver for AliyunDnsSolver {
    async fn create_txt_record(&self, domain: &str, value: &str) -> anyhow::Result<()> {
        let (main_domain, rr) = parse_domain(domain)?;

        info!(
            "Creating Aliyun DNS TXT record: {}.{} = {}",
            rr, main_domain, value
        );

        let params = vec![
            ("DomainName".to_string(), main_domain),
            ("RR".to_string(), rr),
            ("Type".to_string(), "TXT".to_string()),
            ("Value".to_string(), value.to_string()),
            ("TTL".to_string(), "600".to_string()),
        ];

        let body = self.call_api("AddDomainRecord", params).await?;

        if let Some(record_id) = body.get("RecordId").and_then(|r| r.as_str()) {
            info!("Created Aliyun DNS TXT record, RecordId: {}", record_id);
        }

        Ok(())
    }

    async fn delete_txt_record(&self, domain: &str, value: &str) -> anyhow::Result<()> {
        let (_main_domain, _rr) = parse_domain(domain)?;

        // Find the existing record
        let record_id = match self.find_txt_record(domain, value).await? {
            Some(id) => id,
            None => {
                warn!(
                    "No matching Aliyun DNS TXT record found for domain {}",
                    domain
                );
                return Ok(());
            }
        };

        info!("Deleting Aliyun DNS TXT record: RecordId={}", record_id);

        let params = vec![("RecordId".to_string(), record_id)];

        let _body = self.call_api("DeleteDomainRecord", params).await?;

        info!("Deleted Aliyun DNS TXT record successfully");
        Ok(())
    }

    async fn wait_for_propagation(
        &self,
        domain: &str,
        value: &str,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        use trust_dns_resolver::config::{
            NameServerConfig, Protocol, ResolverConfig, ResolverOpts,
        };
        use trust_dns_resolver::TokioAsyncResolver;

        // Build resolver with Google DNS + Alibaba DNS
        let mut config = ResolverConfig::new();
        config.add_name_server(NameServerConfig::new(
            "8.8.8.8:53".parse().unwrap(),
            Protocol::Udp,
        ));
        config.add_name_server(NameServerConfig::new(
            "223.5.5.5:53".parse().unwrap(),
            Protocol::Udp,
        ));
        let resolver = TokioAsyncResolver::tokio(config, ResolverOpts::default());

        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = Duration::from_secs(5);

        info!(
            "Waiting for DNS propagation of TXT record for {} (timeout: {:?})",
            domain, timeout
        );

        loop {
            // Check deadline BEFORE sleeping
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("DNS propagation timeout for {} after {:?}", domain, timeout);
            }

            match resolver.txt_lookup(domain).await {
                Ok(lookup) => {
                    let mut found_count = 0;
                    for txt in lookup.iter() {
                        found_count += 1;
                        let txt_str = txt.to_string();
                        if txt_str == value {
                            info!("DNS TXT record confirmed for {}: {}", domain, value);
                            return Ok(());
                        }
                    }
                    debug!(
                        "TXT record not yet propagated for {} (found {} records, none match)",
                        domain, found_count
                    );
                }
                Err(e) => {
                    debug!("DNS lookup failed for {} (may be NXDOMAIN): {}", domain, e);
                }
            }

            // Sleep for the lesser of poll_interval and remaining time
            let sleep_time = remaining.min(poll_interval);
            tokio::time::sleep(sleep_time).await;
        }
    }

    fn provider_name(&self) -> &str {
        "aliyun"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_domain_subdomain() {
        let (domain, rr) = parse_domain("test.example.com").unwrap();
        assert_eq!(domain, "example.com");
        assert_eq!(rr, "test");
    }

    #[test]
    fn test_parse_domain_bare() {
        let (domain, rr) = parse_domain("example.com").unwrap();
        assert_eq!(domain, "example.com");
        assert_eq!(rr, "@");
    }

    #[test]
    fn test_parse_domain_wildcard() {
        let (domain, rr) = parse_domain("*.example.com").unwrap();
        assert_eq!(domain, "example.com");
        assert_eq!(rr, "*");
    }

    #[test]
    fn test_parse_domain_deep_subdomain() {
        let (domain, rr) = parse_domain("a.b.example.com").unwrap();
        assert_eq!(domain, "example.com");
        assert_eq!(rr, "a.b");
    }

    #[test]
    fn test_parse_domain_invalid() {
        assert!(parse_domain("com").is_err());
    }

    #[test]
    fn test_parse_acme_challenge_domain() {
        // For wildcard domain *.example.com, the ACME challenge domain
        // should be _acme-challenge.example.com
        let domain = "_acme-challenge.example.com";
        let (main_domain, rr) = parse_domain(domain).unwrap();
        assert_eq!(main_domain, "example.com");
        assert_eq!(rr, "_acme-challenge");
    }

    #[test]
    fn test_dns_txt_value_calculation() {
        // DNS-01 challenge TXT record value calculation:
        // key_authorization = token + "." + thumbprint(account_key)
        // txt_value = base64url(sha256(key_authorization))

        use base64::Engine;
        use sha2::{Digest, Sha256};

        let key_authorization = "test_token.test_thumbprint";
        let mut hasher = Sha256::new();
        hasher.update(key_authorization.as_bytes());
        let hash = hasher.finalize();
        let txt_value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);

        // Verify the result is non-empty
        assert!(!txt_value.is_empty());
        // Verify it's a valid base64url string
        assert!(txt_value
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
    }
}

#[cfg(test)]
mod aliyun_tests {
    //! End-to-end integration tests for ACME DNS-01 with Aliyun DNS.
    //!
    //! These tests make real API calls to Let's Encrypt staging and Aliyun DNS.
    //! Run with: `cargo test -- --ignored acme_dns01_e2e`

    #[cfg(test)]
    mod e2e_tests {
        use crate::acme::client::AcmeClient;
        use crate::acme::dns::aliyun::AliyunDnsSolver;
        use crate::acme::dns::{DnsChallengeSolver, DnsProvider, DnsProviderConfig};
        use crate::acme::storage::CertificateStorage;
        use crate::acme::{AcmeState, CertificateStatus};
        use std::sync::Arc;
        use x509_parser::prelude::*;

        const ACME_STAGING_URL: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";
        const TEST_EMAIL: &str = "test@example.com";
        const TEST_DOMAIN: &str = "*.example.com";
        const ALIYUN_AK: &str = "LTAI5tEXAMPLEAK00000000000000";
        const ALIYUN_SK: &str = "EXAMPLESK000000000000000000000000";

        fn make_dns_config() -> DnsProviderConfig {
            DnsProviderConfig {
                provider: DnsProvider::Aliyun,
                api_key: ALIYUN_AK.to_string(),
                api_secret: Some(ALIYUN_SK.to_string()),
                zone_id: None,
            }
        }

        /// 完整 ACME DNS-01 通配符证书申请流程
        ///
        /// 测试步骤：
        /// 1. 创建 AcmeClient（staging 环境）
        /// 2. 初始化 ACME 账号（注册或恢复）
        /// 3. 创建 AliyunDnsSolver
        /// 4. 申请 *.example.com 通配符证书
        /// 5. 验证证书元数据
        /// 6. 验证证书文件存在且可解析
        /// 7. 验证 account.json 已保存
        #[tokio::test]
        #[ignore]
        async fn acme_dns01_wildcard_e2e() {
            // 1. 创建临时目录
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            let cert_dir = temp_dir.path().to_str().unwrap().to_string();

            // 2. 创建并初始化 AcmeClient
            let state = AcmeState::new();
            let client = AcmeClient::new(
                state.clone(),
                ACME_STAGING_URL.to_string(),
                cert_dir.clone(),
                Some(TEST_EMAIL.to_string()),
            );
            client
                .initialize()
                .await
                .expect("Failed to initialize ACME client");

            // 3. 创建 AliyunDnsSolver
            let dns_config = make_dns_config();
            let solver: Arc<dyn DnsChallengeSolver> = Arc::new(AliyunDnsSolver::new(&dns_config));

            // 4. 申请通配符证书
            let metadata = client
                .request_certificate_with_dns(TEST_DOMAIN, solver)
                .await
                .expect("Failed to request certificate");

            // 5. 验证元数据
            assert_eq!(metadata.domain, TEST_DOMAIN);
            assert_eq!(metadata.status, CertificateStatus::Active);
            assert!(metadata.issued_at.is_some(), "issued_at should be set");
            assert!(metadata.expires_at.is_some(), "expires_at should be set");
            assert!(metadata.auto_renew, "auto_renew should be true");
            assert!(metadata.error.is_none(), "error should be None");

            // 6. 验证证书文件
            let storage = CertificateStorage::new(&cert_dir);
            assert!(
                storage.has_certificate(TEST_DOMAIN),
                "Certificate files should exist on disk"
            );

            // 验证 cert.pem
            let cert_pem = storage
                .load_certificate(TEST_DOMAIN)
                .expect("Failed to load cert")
                .expect("cert.pem should exist");
            assert!(
                cert_pem.contains("BEGIN CERTIFICATE"),
                "cert.pem should be valid PEM"
            );

            // 解析证书验证域名和有效期
            let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
                .expect("Failed to parse cert PEM");
            let (_, cert) =
                X509Certificate::from_der(&pem.contents).expect("Failed to parse cert DER");

            // 验证证书 SAN 包含通配符域名
            let san = cert
                .subject_alternative_name()
                .expect("Failed to get SAN extension")
                .expect("SAN extension should exist");
            let dns_names: Vec<String> = san
                .value
                .general_names
                .iter()
                .filter_map(|name| match name {
                    x509_parser::extensions::GeneralName::DNSName(dns) => Some(dns.to_string()),
                    _ => None,
                })
                .collect();
            assert!(
                dns_names
                    .iter()
                    .any(|n| n == "*.example.com" || n == "example.com"),
                "Certificate SAN should contain *.example.com or example.com, got: {:?}",
                dns_names
            );

            // 验证证书未过期
            let not_after = cert.validity.not_after.to_datetime();
            assert!(
                not_after.unix_timestamp() > chrono::Utc::now().timestamp(),
                "Certificate should not be expired"
            );

            // 验证 key.pem
            let key_pem = storage
                .load_private_key(TEST_DOMAIN)
                .expect("Failed to load key")
                .expect("key.pem should exist");
            assert!(
                key_pem.contains("PRIVATE KEY"),
                "key.pem should contain a private key"
            );

            // 验证 chain.pem
            let chain_pem = storage
                .load_chain(TEST_DOMAIN)
                .expect("Failed to load chain");
            assert!(chain_pem.is_some(), "chain.pem should exist");

            // 7. 验证 account.json 已保存
            let account_path = std::path::Path::new(&cert_dir).join("account.json");
            assert!(account_path.exists(), "account.json should be saved");

            // 清理由 TempDir drop 自动处理
            println!("✅ ACME DNS-01 e2e test passed for domain: {}", TEST_DOMAIN);
        }

        /// 测试阿里云 DNS TXT 记录的创建和删除
        ///
        /// 这是一个更轻量的测试，只验证 DNS 操作，不涉及 ACME。
        #[tokio::test]
        #[ignore]
        async fn aliyun_dns_txt_record_lifecycle() {
            let dns_config = make_dns_config();
            let solver = AliyunDnsSolver::new(&dns_config);

            let domain = "_acme-challenge.test-e2e.example.com";
            let value = "test-txt-value-e2e";

            // 创建 TXT 记录
            solver
                .create_txt_record(domain, value)
                .await
                .expect("Failed to create TXT record");

            // 等待传播
            solver
                .wait_for_propagation(domain, value, std::time::Duration::from_secs(60))
                .await
                .expect("DNS propagation failed");

            // 删除 TXT 记录
            solver
                .delete_txt_record(domain, value)
                .await
                .expect("Failed to delete TXT record");

            println!("✅ Aliyun DNS TXT record lifecycle test passed");
        }

        /// 测试 AcmeClient 账号初始化和恢复
        #[tokio::test]
        #[ignore]
        async fn acme_account_init_and_restore() {
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
            let cert_dir = temp_dir.path().to_str().unwrap().to_string();

            // 第一次初始化：注册新账号
            let state1 = AcmeState::new();
            let client1 = AcmeClient::new(
                state1,
                ACME_STAGING_URL.to_string(),
                cert_dir.clone(),
                Some(TEST_EMAIL.to_string()),
            );
            client1
                .initialize()
                .await
                .expect("Failed to initialize ACME client (first time)");

            // 验证 account.json 已保存
            let account_path = std::path::Path::new(&cert_dir).join("account.json");
            assert!(
                account_path.exists(),
                "account.json should be saved after first init"
            );

            // 第二次初始化：从文件恢复账号
            let state2 = AcmeState::new();
            let client2 = AcmeClient::new(
                state2,
                ACME_STAGING_URL.to_string(),
                cert_dir.clone(),
                Some(TEST_EMAIL.to_string()),
            );
            client2
                .initialize()
                .await
                .expect("Failed to restore ACME client (second time)");

            println!("✅ ACME account init and restore test passed");
        }
    }
}
