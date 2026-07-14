//! End-to-end integration tests for ACME DNS-01 with Aliyun DNS.
//!
//! These tests make real API calls to Let's Encrypt staging and Aliyun DNS.
//! Run with: `cargo test -- --ignored acme_dns01_e2e`

#[cfg(test)]
mod e2e_tests {
    use crate::server::acme::client::AcmeClient;
    use crate::server::acme::dns::aliyun::AliyunDnsSolver;
    use crate::server::acme::dns::{DnsChallengeSolver, DnsProvider, DnsProviderConfig};
    use crate::server::acme::storage::CertificateStorage;
    use crate::server::acme::{AcmeState, CertificateStatus};
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
        let (_, cert) = X509Certificate::from_der(&pem.contents)
            .expect("Failed to parse cert DER");

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
            dns_names.iter().any(|n| n == "*.example.com" || n == "example.com"),
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
        println!(
            "✅ ACME DNS-01 e2e test passed for domain: {}",
            TEST_DOMAIN
        );
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
        assert!(account_path.exists(), "account.json should be saved after first init");

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
