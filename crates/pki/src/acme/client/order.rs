use super::super::dns::DnsChallengeSolver;
use super::super::storage::CertificateStorage;
use super::cert_utils;
use super::AcmeClient;
use super::{CertificateMetadata, CertificateStatus};
use crate::error::{AcmeError, AcmeResult};
use base64::Engine;
use chrono::Utc;
use instant_acme::{ChallengeType, Identifier, NewOrder, OrderStatus, RetryPolicy};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Challenge poll interval
const CHALLENGE_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Maximum time to wait for challenge validation
const CHALLENGE_POLL_TIMEOUT: Duration = Duration::from_mins(2);

impl AcmeClient {
    /// 通过 HTTP-01 挑战为域名申请证书。
    ///
    /// # Errors
    ///
    /// 当 ACME 账户未初始化、ACME 订单创建/挑战失败、挑战验证超时、CSR 生成或证书下载/解析/落盘失败时返回错误。
    #[allow(
        clippy::too_many_lines,
        reason = "ACME 订单全流程顺序编排：建单、部署 HTTP-01 挑战、轮询校验、CSR 终结与落盘，共享大量局部状态，拆分会降低可读性"
    )]
    pub async fn request_certificate(&self, domain: &str) -> AcmeResult<CertificateMetadata> {
        info!("Requesting certificate for domain: {}", domain);

        // Check if we already have a valid certificate
        if let Some(cert) = self.get_certificate_metadata(domain).await? {
            if cert.status == CertificateStatus::Active {
                info!("Certificate already exists and is active for {}", domain);
                return Ok(cert);
            }
        }

        // Ensure we have an ACME account
        let account = {
            let guard = self.account.read().await;
            guard
                .as_ref()
                .ok_or_else(|| {
                    AcmeError::msg("ACME account not initialized. Call initialize() first.")
                })?
                .clone()
        };

        // Create certificate metadata with pending status
        let metadata = CertificateMetadata {
            domain: domain.to_string(),
            status: CertificateStatus::Pending,
            issued_at: None,
            expires_at: None,
            auto_renew: true,
            error: None,
        };

        // Save pending status to database
        if let Some(db) = self.state.db() {
            db.save_acme_certificate(
                domain,
                &metadata.status.to_string(),
                None,
                None,
                None,
                None,
                None,
                true,
            )
            .await?;
        }

        // Create ACME order
        let identifier = Identifier::Dns(domain.to_string());
        let identifiers = [identifier];
        let order_params = NewOrder::new(&identifiers);

        let mut order = account
            .new_order(&order_params)
            .await
            .map_err(AcmeError::protocol("Failed to create ACME order"))?;

        info!("Created ACME order for {}", domain);

        // Process authorizations and set up HTTP-01 challenges
        let mut challenge_tokens: Vec<String> = Vec::new();
        let mut authorizations = order.authorizations();
        while let Some(auth_result) = authorizations.next().await {
            let mut auth_handle =
                auth_result.map_err(AcmeError::protocol("Failed to get order authorization"))?;

            // Find the HTTP-01 challenge
            let mut challenge_handle =
                auth_handle
                    .challenge(ChallengeType::Http01)
                    .ok_or_else(|| {
                        AcmeError::msgf(format!("No HTTP-01 challenge found for domain {domain}"))
                    })?;

            // Get the key authorization response
            let key_authorization = challenge_handle.key_authorization();

            // Deploy the challenge token so the challenge server can serve it
            self.state
                .add_challenge(
                    challenge_handle.token.clone(),
                    key_authorization.as_str().to_string(),
                )
                .await;

            info!(
                "Deployed challenge token for {}: {}",
                domain, challenge_handle.token
            );

            // Save challenge to database
            if let Some(db) = self.state.db() {
                db.save_acme_challenge(
                    &challenge_handle.token,
                    domain,
                    key_authorization.as_str(),
                    None,
                )
                .await?;
            }

            challenge_tokens.push(challenge_handle.token.clone());

            // Signal to the ACME server that the challenge is ready
            challenge_handle
                .set_ready()
                .await
                .map_err(AcmeError::protocol("Failed to set challenge ready"))?;

            info!("Challenge ready for {}: {}", domain, challenge_handle.token);
        }

        // Poll order status until Ready or Invalid
        let deadline = tokio::time::Instant::now() + CHALLENGE_POLL_TIMEOUT;
        let _order_status = loop {
            if tokio::time::Instant::now() >= deadline {
                // Clean up challenges on timeout
                for token in &challenge_tokens {
                    self.state.remove_challenge(token).await;
                }
                return Err(AcmeError::msgf(format!(
                    "Challenge validation timed out for domain {domain} after {CHALLENGE_POLL_TIMEOUT:?}"
                )));
            }

            tokio::time::sleep(CHALLENGE_POLL_INTERVAL).await;

            let state = order
                .refresh()
                .await
                .map_err(AcmeError::protocol("Failed to refresh order"))?;
            let status = state.status;

            match status {
                OrderStatus::Ready => {
                    info!("Order ready for finalization: {}", domain);
                    break status;
                }
                OrderStatus::Invalid => {
                    let error_detail = state
                        .error
                        .as_ref()
                        .map(|e| e.detail.clone().unwrap_or_default())
                        .unwrap_or_default();

                    // Clean up challenges
                    for token in &challenge_tokens {
                        self.state.remove_challenge(token).await;
                    }

                    // Update database with error
                    if let Some(db) = self.state.db() {
                        db.update_acme_certificate_status(domain, "failed", Some(&error_detail))
                            .await?;
                    }

                    return Err(AcmeError::msgf(format!(
                        "ACME order became invalid for domain {domain}: {error_detail}"
                    )));
                }
                OrderStatus::Pending | OrderStatus::Processing | OrderStatus::Valid => {}
            }
        };

        // Generate CSR using rcgen
        let key_pair = rcgen::KeyPair::generate().map_err(AcmeError::GenerateKeyPair)?;
        let mut params = rcgen::CertificateParams::new(vec![domain.to_string()])
            .map_err(|e| AcmeError::CertParams(e.to_string()))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        let csr = params
            .serialize_request(&key_pair)
            .map_err(|e| AcmeError::SerializeCsr(e.to_string()))?;

        // Finalize the order with the CSR
        order
            .finalize_csr(csr.der())
            .await
            .map_err(AcmeError::protocol("Failed to finalize ACME order"))?;

        info!("Finalized ACME order for {}", domain);

        // Poll for the certificate - after finalization, the server may take time
        // to issue the certificate. poll_certificate handles the retry logic.
        let retry_policy = RetryPolicy::new()
            .initial_delay(Duration::from_secs(2))
            .timeout(Duration::from_mins(2));
        let cert_chain =
            order
                .poll_certificate(&retry_policy)
                .await
                .map_err(AcmeError::protocol(
                    "Failed to download certificate after finalization",
                ))?;

        // Parse the certificate to get expiry date
        let expires_at = cert_utils::parse_certificate_expiry(&cert_chain)
            .map_err(AcmeError::wrap("Failed to parse issued certificate"))?;

        // Split the certificate chain into cert and chain PEM
        let (cert_pem, chain_pem) = cert_utils::split_certificate_chain(&cert_chain);

        // Serialize the private key
        let key_pem = key_pair.serialize_pem();

        // Save certificate files via CertificateStorage
        let storage = CertificateStorage::new(&self.cert_dir);
        storage
            .initialize()
            .map_err(AcmeError::wrap("Failed to initialize certificate storage"))?;
        storage
            .save_certificate(domain, &cert_pem, &key_pem, Some(&chain_pem))
            .map_err(AcmeError::wrap("Failed to save certificate files"))?;

        // Update in-memory certificate cache
        {
            let mut certs = self.state.certificates.write().await;
            certs.insert(domain.to_string(), cert_chain.clone());
        }

        // Clean up challenge tokens
        for token in &challenge_tokens {
            self.state.remove_challenge(token).await;
            // Also clean up from database
            if let Some(db) = self.state.db() {
                let _ = db.delete_acme_challenge(token).await;
            }
        }

        let now = Utc::now();

        // Update database with active certificate
        if let Some(db) = self.state.db() {
            db.save_acme_certificate(
                domain,
                "active",
                Some(&cert_pem),
                Some(&key_pem),
                Some(&chain_pem),
                Some(now),
                Some(expires_at),
                true,
            )
            .await?;
        }

        info!(
            "Certificate issued for {} (expires: {})",
            domain, expires_at
        );

        // Build and return metadata
        let metadata = CertificateMetadata {
            domain: domain.to_string(),
            status: CertificateStatus::Active,
            issued_at: Some(now.to_rfc3339()),
            expires_at: Some(expires_at.to_rfc3339()),
            auto_renew: true,
            error: None,
        };

        Ok(metadata)
    }

    /// 使用 DNS-01 挑战申请证书
    ///
    /// 适用于：
    /// - 无法开放 80 端口的场景
    /// - 申请通配符证书（*.example.com）
    ///
    /// # Errors
    ///
    /// 当 ACME 账户未初始化、ACME 订单/DNS TXT 创建或传播失败、挑战验证超时、CSR 生成或证书落盘/解析失败时返回错误。
    #[allow(
        clippy::too_many_lines,
        reason = "DNS-01 订单全流程顺序编排：建单、创建/等待 DNS TXT 传播、轮询校验、CSR 终结与落盘，共享大量状态，拆分会降低可读性"
    )]
    pub async fn request_certificate_with_dns(
        &self,
        domain: &str,
        dns_solver: Arc<dyn DnsChallengeSolver>,
    ) -> AcmeResult<CertificateMetadata> {
        info!("Requesting certificate for domain with DNS-01: {}", domain);

        // Check if we already have a valid certificate
        if let Some(cert) = self.get_certificate_metadata(domain).await? {
            if cert.status == CertificateStatus::Active {
                info!("Certificate already exists and is active for {}", domain);
                return Ok(cert);
            }
        }

        // Ensure we have an ACME account
        let account = {
            let guard = self.account.read().await;
            guard
                .as_ref()
                .ok_or_else(|| {
                    AcmeError::msg("ACME account not initialized. Call initialize() first.")
                })?
                .clone()
        };

        // Create certificate metadata with pending status
        let metadata = CertificateMetadata {
            domain: domain.to_string(),
            status: CertificateStatus::Pending,
            issued_at: None,
            expires_at: None,
            auto_renew: true,
            error: None,
        };

        // Save pending status to database
        if let Some(db) = self.state.db() {
            db.save_acme_certificate(
                domain,
                &metadata.status.to_string(),
                None,
                None,
                None,
                None,
                None,
                true,
            )
            .await?;
        }

        // Create ACME order
        let identifier = Identifier::Dns(domain.to_string());
        let identifiers = [identifier];
        let order_params = NewOrder::new(&identifiers);

        let mut order = account
            .new_order(&order_params)
            .await
            .map_err(AcmeError::protocol("Failed to create ACME order"))?;

        info!("Created ACME order for {}", domain);

        // Process authorizations and set up DNS-01 challenges
        let mut challenge_tokens: Vec<String> = Vec::new();
        let mut txt_records_to_cleanup: Vec<(String, String)> = Vec::new();
        let mut authorizations = order.authorizations();

        while let Some(auth_result) = authorizations.next().await {
            let mut auth_handle =
                auth_result.map_err(AcmeError::protocol("Failed to get order authorization"))?;

            // Find the DNS-01 challenge
            let mut challenge_handle =
                auth_handle.challenge(ChallengeType::Dns01).ok_or_else(|| {
                    AcmeError::msgf(format!("No DNS-01 challenge found for domain {domain}"))
                })?;

            // Get the key authorization
            let key_authorization = challenge_handle.key_authorization();

            // Calculate DNS TXT record value: base64url(sha256(key_authorization))
            let mut hasher = Sha256::new();
            hasher.update(key_authorization.as_str().as_bytes());
            let hash = hasher.finalize();
            let txt_value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);

            info!(
                "DNS-01 challenge for {}: token={}, txt_value={}",
                domain, challenge_handle.token, txt_value
            );

            // Create DNS TXT record
            // For wildcard domains like *.example.com, the ACME challenge
            // domain should be _acme-challenge.example.com (without the *)
            let base_domain = domain.strip_prefix("*.").unwrap_or(domain);
            let acme_domain = format!("_acme-challenge.{base_domain}");
            dns_solver
                .create_txt_record(&acme_domain, &txt_value)
                .await
                .map_err(AcmeError::wrap("Failed to create DNS TXT record"))?;

            txt_records_to_cleanup.push((acme_domain.clone(), txt_value.clone()));

            // Wait for DNS propagation
            dns_solver
                .wait_for_propagation(&acme_domain, &txt_value, Duration::from_mins(10))
                .await
                .map_err(AcmeError::wrap("DNS propagation timeout"))?;

            // Save challenge to database
            if let Some(db) = self.state.db() {
                db.save_acme_challenge(
                    &challenge_handle.token,
                    domain,
                    key_authorization.as_str(),
                    None,
                )
                .await?;
            }

            challenge_tokens.push(challenge_handle.token.clone());

            // Signal to the ACME server that the challenge is ready
            challenge_handle
                .set_ready()
                .await
                .map_err(AcmeError::protocol("Failed to set challenge ready"))?;

            info!(
                "DNS-01 challenge ready for {}: {}",
                domain, challenge_handle.token
            );
        }

        // Poll order status until Ready or Invalid
        let deadline = tokio::time::Instant::now() + CHALLENGE_POLL_TIMEOUT;
        let _order_status = loop {
            if tokio::time::Instant::now() >= deadline {
                // Clean up challenges on timeout
                for token in &challenge_tokens {
                    self.state.remove_challenge(token).await;
                }
                for (acme_domain, txt_value) in &txt_records_to_cleanup {
                    let _ = dns_solver.delete_txt_record(acme_domain, txt_value).await;
                }
                return Err(AcmeError::msgf(format!(
                    "Challenge validation timed out for domain {domain} after {CHALLENGE_POLL_TIMEOUT:?}"
                )));
            }

            tokio::time::sleep(CHALLENGE_POLL_INTERVAL).await;

            let state = order
                .refresh()
                .await
                .map_err(AcmeError::protocol("Failed to refresh order"))?;
            let status = state.status;

            match status {
                OrderStatus::Ready => {
                    info!("Order ready for finalization: {}", domain);
                    break status;
                }
                OrderStatus::Invalid => {
                    let error_detail = state
                        .error
                        .as_ref()
                        .map(|e| e.detail.clone().unwrap_or_default())
                        .unwrap_or_default();

                    // Clean up
                    for token in &challenge_tokens {
                        self.state.remove_challenge(token).await;
                    }
                    for (acme_domain, txt_value) in &txt_records_to_cleanup {
                        let _ = dns_solver.delete_txt_record(acme_domain, txt_value).await;
                    }

                    if let Some(db) = self.state.db() {
                        db.update_acme_certificate_status(domain, "failed", Some(&error_detail))
                            .await?;
                    }

                    return Err(AcmeError::msgf(format!(
                        "ACME order became invalid for domain {domain}: {error_detail}"
                    )));
                }
                OrderStatus::Pending | OrderStatus::Processing | OrderStatus::Valid => {}
            }
        };

        // Generate CSR using rcgen
        let key_pair = rcgen::KeyPair::generate().map_err(AcmeError::GenerateKeyPair)?;
        let mut params = rcgen::CertificateParams::new(vec![domain.to_string()])
            .map_err(|e| AcmeError::CertParams(e.to_string()))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        let csr = params
            .serialize_request(&key_pair)
            .map_err(|e| AcmeError::SerializeCsr(e.to_string()))?;

        // Finalize the order with the CSR
        order
            .finalize_csr(csr.der())
            .await
            .map_err(AcmeError::protocol("Failed to finalize ACME order"))?;

        info!("Finalized ACME order for {}", domain);

        // Poll for the certificate - after finalization, the server may take time
        // to issue the certificate. poll_certificate handles the retry logic.
        let retry_policy = RetryPolicy::new()
            .initial_delay(Duration::from_secs(2))
            .timeout(Duration::from_mins(2));
        let cert_chain =
            order
                .poll_certificate(&retry_policy)
                .await
                .map_err(AcmeError::protocol(
                    "Failed to download certificate after finalization",
                ))?;

        // Parse the certificate to get expiry date
        let expires_at = cert_utils::parse_certificate_expiry(&cert_chain)
            .map_err(AcmeError::wrap("Failed to parse issued certificate"))?;

        // Split the certificate chain into cert and chain PEM
        let (cert_pem, chain_pem) = cert_utils::split_certificate_chain(&cert_chain);

        // Serialize the private key
        let key_pem = key_pair.serialize_pem();

        // Save certificate files via CertificateStorage
        let storage = CertificateStorage::new(&self.cert_dir);
        storage
            .initialize()
            .map_err(AcmeError::wrap("Failed to initialize certificate storage"))?;
        storage
            .save_certificate(domain, &cert_pem, &key_pem, Some(&chain_pem))
            .map_err(AcmeError::wrap("Failed to save certificate files"))?;

        // Update in-memory certificate cache
        {
            let mut certs = self.state.certificates.write().await;
            certs.insert(domain.to_string(), cert_chain.clone());
        }

        // Clean up challenge tokens and DNS records
        for token in &challenge_tokens {
            self.state.remove_challenge(token).await;
            if let Some(db) = self.state.db() {
                let _ = db.delete_acme_challenge(token).await;
            }
        }
        for (acme_domain, txt_value) in &txt_records_to_cleanup {
            if let Err(e) = dns_solver.delete_txt_record(acme_domain, txt_value).await {
                warn!("Failed to clean up DNS TXT record: {}", e);
            }
        }

        let now = Utc::now();

        // Update database with active certificate
        if let Some(db) = self.state.db() {
            db.save_acme_certificate(
                domain,
                "active",
                Some(&cert_pem),
                Some(&key_pem),
                Some(&chain_pem),
                Some(now),
                Some(expires_at),
                true,
            )
            .await?;
        }

        info!(
            "Certificate issued for {} via DNS-01 (expires: {})",
            domain, expires_at
        );

        let metadata = CertificateMetadata {
            domain: domain.to_string(),
            status: CertificateStatus::Active,
            issued_at: Some(now.to_rfc3339()),
            expires_at: Some(expires_at.to_rfc3339()),
            auto_renew: true,
            error: None,
        };

        Ok(metadata)
    }
}
