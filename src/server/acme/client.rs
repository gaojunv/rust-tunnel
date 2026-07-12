use super::storage::CertificateStorage;
use super::{AcmeState, CertificateMetadata, CertificateStatus};
use anyhow::{Context, Result};
use chrono::Utc;
use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
};
use rcgen::{Certificate as RcgenCert, CertificateParams, KeyPair};
use std::path::Path;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use x509_parser::prelude::*;

/// Challenge poll interval
const CHALLENGE_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Maximum time to wait for challenge validation
const CHALLENGE_POLL_TIMEOUT: Duration = Duration::from_secs(120);

/// ACME client for certificate management
pub struct AcmeClient {
    state: AcmeState,
    server_url: String,
    cert_dir: String,
    email: Option<String>,
    /// The ACME account (loaded or created during initialization)
    account: RwLock<Option<Account>>,
    /// Saved account credentials for persistence
    credentials: RwLock<Option<AccountCredentials>>,
}

impl AcmeClient {
    /// Create a new ACME client
    pub fn new(
        state: AcmeState,
        server_url: String,
        cert_dir: String,
        email: Option<String>,
    ) -> Self {
        Self {
            state,
            server_url,
            cert_dir,
            email,
            account: RwLock::new(None),
            credentials: RwLock::new(None),
        }
    }

    /// Initialize the ACME client
    ///
    /// Creates the certificate directory if needed, then either loads existing
    /// account credentials from disk or registers a new account with the ACME server.
    pub async fn initialize(&self) -> Result<()> {
        info!("Initializing ACME client with server: {}", self.server_url);

        // Create cert directory if it doesn't exist
        let cert_dir = Path::new(&self.cert_dir);
        if !cert_dir.exists() {
            std::fs::create_dir_all(cert_dir)
                .context("Failed to create certificate directory")?;
        }

        // Try to load existing account credentials
        let account_path = cert_dir.join("account.json");
        if account_path.exists() {
            let data = std::fs::read_to_string(&account_path)
                .context("Failed to read account credentials file")?;
            let creds: AccountCredentials =
                serde_json::from_str(&data).context("Failed to parse account credentials")?;

            // Re-parse credentials for from_credentials since AccountCredentials doesn't implement Clone
            let creds_for_restore: AccountCredentials =
                serde_json::from_str(&data).context("Failed to parse account credentials for restore")?;

            let account = Account::from_credentials(creds_for_restore)
                .await
                .context("Failed to restore ACME account from saved credentials")?;

            info!("Restored existing ACME account: {}", account.id());
            *self.account.write().await = Some(account);
            *self.credentials.write().await = Some(creds);
        } else {
            // Register new account with the ACME server
            let contact_email = self.email.as_deref().unwrap_or("noreply@example.com");
            let contact_entry = format!("mailto:{contact_email}");

            let new_account = NewAccount {
                contact: &[&contact_entry],
                terms_of_service_agreed: true,
                only_return_existing: false,
            };

            let (account, credentials) =
                Account::create(&new_account, &self.server_url, None)
                    .await
                    .context("Failed to register new ACME account")?;

            // Save credentials to disk
            let creds_json = serde_json::to_string_pretty(&credentials)
                .context("Failed to serialize account credentials")?;
            std::fs::write(&account_path, creds_json)
                .context("Failed to save account credentials to disk")?;

            info!("Registered new ACME account: {}", account.id());
            *self.account.write().await = Some(account);
            *self.credentials.write().await = Some(credentials);
        }

        Ok(())
    }

    /// Request a new certificate for a domain
    pub async fn request_certificate(&self, domain: &str) -> Result<CertificateMetadata> {
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
                .context("ACME account not initialized. Call initialize() first.")?
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
        let order_params = NewOrder {
            identifiers: &[identifier],
        };

        let mut order = account
            .new_order(&order_params)
            .await
            .context("Failed to create ACME order")?;

        info!("Created ACME order for {}", domain);

        // Process authorizations and set up HTTP-01 challenges
        let authorizations = order
            .authorizations()
            .await
            .context("Failed to get order authorizations")?;

        for auth in &authorizations {
            // Find the HTTP-01 challenge
            let challenge = auth
                .challenges
                .iter()
                .find(|c| c.r#type == ChallengeType::Http01)
                .context(format!(
                    "No HTTP-01 challenge found for domain {}",
                    domain
                ))?;

            // Get the key authorization response
            let key_authorization = order.key_authorization(challenge);

            // Deploy the challenge token so the challenge server can serve it
            self.state
                .add_challenge(challenge.token.clone(), key_authorization.as_str().to_string())
                .await;

            info!(
                "Deployed challenge token for {}: {}",
                domain, challenge.token
            );

            // Save challenge to database
            if let Some(db) = self.state.db() {
                db.save_acme_challenge(
                    &challenge.token,
                    domain,
                    key_authorization.as_str(),
                    None,
                )
                .await?;
            }

            // Signal to the ACME server that the challenge is ready
            order
                .set_challenge_ready(&challenge.url)
                .await
                .context("Failed to set challenge ready")?;

            info!("Challenge ready for {}: {}", domain, challenge.token);
        }

        // Poll order status until Ready or Invalid
        let deadline = tokio::time::Instant::now() + CHALLENGE_POLL_TIMEOUT;
        let _order_status = loop {
            if tokio::time::Instant::now() >= deadline {
                // Clean up challenges on timeout
                for auth in &authorizations {
                    for challenge in &auth.challenges {
                        if challenge.r#type == ChallengeType::Http01 {
                            self.state.remove_challenge(&challenge.token).await;
                        }
                    }
                }
                anyhow::bail!(
                    "Challenge validation timed out for domain {} after {:?}",
                    domain,
                    CHALLENGE_POLL_TIMEOUT
                );
            }

            tokio::time::sleep(CHALLENGE_POLL_INTERVAL).await;

            let state = order.refresh().await.context("Failed to refresh order")?;
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
                    for auth in &authorizations {
                        for challenge in &auth.challenges {
                            if challenge.r#type == ChallengeType::Http01 {
                                self.state.remove_challenge(&challenge.token).await;
                            }
                        }
                    }

                    // Update database with error
                    if let Some(db) = self.state.db() {
                        db.update_acme_certificate_status(domain, "failed", Some(&error_detail))
                            .await?;
                    }

                    anyhow::bail!(
                        "ACME order became invalid for domain {}: {}",
                        domain,
                        error_detail
                    );
                }
                _ => {
                    // Still pending or processing, keep polling
                    continue;
                }
            }
        };

        // Generate CSR using rcgen
        let key_pair =
            KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256).context("Failed to generate key pair")?;
        let params = CertificateParams::new(vec![domain.to_string()]);
        let cert = RcgenCert::from_params(params).context("Failed to create certificate from params")?;
        let csr_der = cert
            .serialize_request_der()
            .context("Failed to serialize CSR")?;

        // Finalize the order with the CSR
        order
            .finalize(&csr_der)
            .await
            .context("Failed to finalize ACME order")?;

        info!("Finalized ACME order for {}", domain);

        // Download the certificate chain
        let cert_chain = order
            .certificate()
            .await
            .context("Failed to download certificate")?
            .context("Certificate not yet available after finalization")?;

        // Parse the certificate to get expiry date
        let expires_at = parse_certificate_expiry(&cert_chain)
            .context("Failed to parse issued certificate")?;

        // Split the certificate chain into cert and chain PEM
        let (cert_pem, chain_pem) = split_certificate_chain(&cert_chain);

        // Serialize the private key
        let key_pem = key_pair.serialize_pem();

        // Save certificate files via CertificateStorage
        let storage = CertificateStorage::new(&self.cert_dir);
        storage
            .initialize()
            .context("Failed to initialize certificate storage")?;
        storage
            .save_certificate(domain, &cert_pem, &key_pem, Some(&chain_pem))
            .context("Failed to save certificate files")?;

        // Update in-memory certificate cache
        {
            let mut certs = self.state.certificates.write().await;
            certs.insert(domain.to_string(), cert_chain.clone());
        }

        // Clean up challenge tokens
        for auth in &authorizations {
            for challenge in &auth.challenges {
                if challenge.r#type == ChallengeType::Http01 {
                    self.state.remove_challenge(&challenge.token).await;
                    // Also clean up from database
                    if let Some(db) = self.state.db() {
                        let _ = db.delete_acme_challenge(&challenge.token).await;
                    }
                }
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

    /// Get certificate metadata for a domain
    pub async fn get_certificate_metadata(
        &self,
        domain: &str,
    ) -> Result<Option<CertificateMetadata>> {
        if let Some(db) = self.state.db() {
            if let Some(record) = db.get_acme_certificate(domain).await? {
                return Ok(Some(CertificateMetadata {
                    domain: record.domain,
                    status: match record.status.as_str() {
                        "pending" => CertificateStatus::Pending,
                        "active" => CertificateStatus::Active,
                        "expired" => CertificateStatus::Expired,
                        "failed" => CertificateStatus::Failed,
                        _ => CertificateStatus::Pending,
                    },
                    issued_at: record.issued_at.map(|dt| dt.to_rfc3339()),
                    expires_at: record.expires_at.map(|dt| dt.to_rfc3339()),
                    auto_renew: record.auto_renew != 0,
                    error: record.error_message,
                }));
            }
        }
        Ok(None)
    }

    /// List all certificates
    pub async fn list_certificates(&self) -> Result<Vec<CertificateMetadata>> {
        let mut certificates = Vec::new();

        if let Some(db) = self.state.db() {
            let records = db.load_acme_certificates().await?;
            for record in records {
                certificates.push(CertificateMetadata {
                    domain: record.domain,
                    status: match record.status.as_str() {
                        "pending" => CertificateStatus::Pending,
                        "active" => CertificateStatus::Active,
                        "expired" => CertificateStatus::Expired,
                        "failed" => CertificateStatus::Failed,
                        _ => CertificateStatus::Pending,
                    },
                    issued_at: record.issued_at.map(|dt| dt.to_rfc3339()),
                    expires_at: record.expires_at.map(|dt| dt.to_rfc3339()),
                    auto_renew: record.auto_renew != 0,
                    error: record.error_message,
                });
            }
        }

        Ok(certificates)
    }

    /// Renew a certificate
    pub async fn renew_certificate(&self, domain: &str) -> Result<CertificateMetadata> {
        info!("Renewing certificate for domain: {}", domain);

        // Update renewal attempt timestamp
        if let Some(db) = self.state.db() {
            db.update_acme_certificate_renewal_attempt(domain).await?;
        }

        self.request_certificate(domain).await
    }

    /// Delete a certificate
    pub async fn delete_certificate(&self, domain: &str) -> Result<()> {
        info!("Deleting certificate for domain: {}", domain);

        if let Some(db) = self.state.db() {
            db.delete_acme_certificate(domain).await?;
        }

        // Also remove from in-memory cache
        {
            let mut certs = self.state.certificates.write().await;
            certs.remove(domain);
        }

        // Delete certificate files from disk
        let storage = CertificateStorage::new(&self.cert_dir);
        if let Err(e) = storage.delete_certificate(domain) {
            warn!("Failed to delete certificate files for {}: {}", domain, e);
        }

        Ok(())
    }
}

/// Start the certificate renewal background task
pub fn start_renewal_task(
    client: AcmeClient,
    check_interval_hours: u64,
    days_before_expiry: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(check_interval_hours * 3600));

        loop {
            interval.tick().await;

            info!("Checking for certificates needing renewal...");

            if let Some(db) = client.state.db() {
                match db
                    .load_acme_certificates_needing_renewal(days_before_expiry as i64)
                    .await
                {
                    Ok(certs) => {
                        for cert in certs {
                            info!("Renewing certificate for domain: {}", cert.domain);
                            if let Err(e) = client.renew_certificate(&cert.domain).await {
                                error!(
                                    "Failed to renew certificate for {}: {}",
                                    cert.domain, e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to load certificates for renewal: {}", e);
                    }
                }
            }
        }
    })
}

/// Parse a PEM certificate chain and extract the expiry date of the first (leaf) certificate
fn parse_certificate_expiry(cert_chain_pem: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    // Parse the PEM data
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_chain_pem.as_bytes())
        .context("Failed to parse certificate PEM")?;

    // Parse the DER-encoded certificate
    let (_, cert) =
        X509Certificate::from_der(&pem.contents).context("Failed to parse certificate DER")?;

    // Extract the expiry date (x509-parser returns time::OffsetDateTime, convert to chrono)
    let not_after = cert.validity.not_after.to_datetime();
    let ts = not_after.unix_timestamp();
    let naive = chrono::DateTime::from_timestamp(ts, 0)
        .context("Failed to create DateTime from timestamp")?;

    Ok(naive)
}

/// Split a PEM certificate chain into the leaf certificate and the remaining chain
fn split_certificate_chain(cert_chain_pem: &str) -> (String, String) {
    let mut certs = Vec::new();
    let mut current_cert = String::new();
    let mut in_cert = false;

    for line in cert_chain_pem.lines() {
        if line.contains("-----BEGIN CERTIFICATE-----") {
            in_cert = true;
            current_cert.clear();
            current_cert.push_str(line);
            current_cert.push('\n');
        } else if line.contains("-----END CERTIFICATE-----") {
            current_cert.push_str(line);
            current_cert.push('\n');
            certs.push(current_cert.clone());
            current_cert.clear();
            in_cert = false;
        } else if in_cert {
            current_cert.push_str(line);
            current_cert.push('\n');
        }
    }

    match certs.len() {
        0 => (cert_chain_pem.to_string(), String::new()),
        1 => (certs[0].clone(), String::new()),
        _ => {
            let leaf = certs[0].clone();
            let chain: String = certs[1..].join("");
            (leaf, chain)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::acme::AcmeState;
    use crate::server::db::Database;

    #[tokio::test]
    async fn test_acme_client_new() {
        let state = AcmeState::new();
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        assert_eq!(
            client.server_url,
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(client.cert_dir, "./data/certs");
        assert_eq!(client.email, Some("test@example.com".to_string()));
    }

    #[tokio::test]
    async fn test_acme_client_new_no_email() {
        let state = AcmeState::new();
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            None,
        );

        assert!(client.email.is_none());
    }

    #[tokio::test]
    async fn test_list_certificates_empty() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        let certs = client.list_certificates().await.unwrap();
        assert!(certs.is_empty());
    }

    #[tokio::test]
    async fn test_list_certificates_no_db() {
        let state = AcmeState::new();
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        let certs = client.list_certificates().await.unwrap();
        assert!(certs.is_empty());
    }

    #[tokio::test]
    async fn test_get_certificate_metadata_not_found() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        let cert = client
            .get_certificate_metadata("example.com")
            .await
            .unwrap();
        assert!(cert.is_none());
    }

    #[tokio::test]
    async fn test_get_certificate_metadata_no_db() {
        let state = AcmeState::new();
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        let cert = client
            .get_certificate_metadata("example.com")
            .await
            .unwrap();
        assert!(cert.is_none());
    }

    #[tokio::test]
    async fn test_delete_certificate_not_found() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Should not error even if certificate doesn't exist
        let result = client.delete_certificate("example.com").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_delete_certificate_no_db() {
        let state = AcmeState::new();
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Should not error when no database is configured
        let result = client.delete_certificate("example.com").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_request_certificate_no_account() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Should fail because account is not initialized
        let result = client.request_certificate("example.com").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ACME account not initialized")
        );
    }

    #[tokio::test]
    async fn test_request_certificate_with_db_persists_pending() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Insert a pending certificate directly to test persistence
        if let Some(database) = client.state.db() {
            database
                .save_acme_certificate(
                    "example.com",
                    "pending",
                    None,
                    None,
                    None,
                    None,
                    None,
                    true,
                )
                .await
                .unwrap();
        }

        // Verify it was persisted in the database
        let retrieved = client
            .get_certificate_metadata("example.com")
            .await
            .unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.domain, "example.com");
        assert_eq!(retrieved.status, CertificateStatus::Pending);
        assert!(retrieved.auto_renew);
    }

    #[tokio::test]
    async fn test_request_certificate_already_active() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Insert an active certificate directly
        if let Some(database) = client.state.db() {
            database
                .save_acme_certificate(
                    "example.com",
                    "active",
                    Some("2026-01-01T00:00:00Z"),
                    Some("2026-12-31T23:59:59Z"),
                    None,
                    None,
                    None,
                    true,
                )
                .await
                .unwrap();
        }

        // Requesting the same domain should return the existing active cert
        let metadata = client
            .request_certificate("example.com")
            .await
            .unwrap();
        assert_eq!(metadata.domain, "example.com");
        assert_eq!(metadata.status, CertificateStatus::Active);
    }

    #[tokio::test]
    async fn test_request_certificate_no_db_no_account() {
        let state = AcmeState::new();
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Should fail because account is not initialized
        let result = client.request_certificate("example.com").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_certificate_status_display() {
        assert_eq!(CertificateStatus::Pending.to_string(), "pending");
        assert_eq!(CertificateStatus::Active.to_string(), "active");
        assert_eq!(CertificateStatus::Expired.to_string(), "expired");
        assert_eq!(CertificateStatus::Failed.to_string(), "failed");
    }

    #[tokio::test]
    async fn test_certificate_status_roundtrip() {
        // Verify that Display serialization can be parsed back
        let statuses = vec![
            CertificateStatus::Pending,
            CertificateStatus::Active,
            CertificateStatus::Expired,
            CertificateStatus::Failed,
        ];

        for status in statuses {
            let displayed = status.to_string();
            let parsed = match displayed.as_str() {
                "pending" => CertificateStatus::Pending,
                "active" => CertificateStatus::Active,
                "expired" => CertificateStatus::Expired,
                "failed" => CertificateStatus::Failed,
                _ => panic!("Unknown status: {displayed}"),
            };
            assert_eq!(status, parsed);
        }
    }

    #[tokio::test]
    async fn test_renew_certificate_no_db() {
        let state = AcmeState::new();
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Should fail because account is not initialized
        let result = client.renew_certificate("example.com").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_multiple_certificates() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Insert certificates directly to test listing
        if let Some(database) = client.state.db() {
            database
                .save_acme_certificate(
                    "a.example.com",
                    "active",
                    None,
                    None,
                    None,
                    None,
                    None,
                    true,
                )
                .await
                .unwrap();
            database
                .save_acme_certificate(
                    "b.example.com",
                    "active",
                    None,
                    None,
                    None,
                    None,
                    None,
                    true,
                )
                .await
                .unwrap();
        }

        let certs = client.list_certificates().await.unwrap();
        assert_eq!(certs.len(), 2);

        let domains: Vec<&str> = certs.iter().map(|c| c.domain.as_str()).collect();
        assert!(domains.contains(&"a.example.com"));
        assert!(domains.contains(&"b.example.com"));
    }

    #[tokio::test]
    async fn test_delete_existing_certificate() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Create a certificate record
        if let Some(database) = client.state.db() {
            database
                .save_acme_certificate(
                    "example.com",
                    "active",
                    None,
                    None,
                    None,
                    None,
                    None,
                    true,
                )
                .await
                .unwrap();
        }
        let certs = client.list_certificates().await.unwrap();
        assert_eq!(certs.len(), 1);

        // Delete it
        client.delete_certificate("example.com").await.unwrap();

        // Verify it's gone
        let certs = client.list_certificates().await.unwrap();
        assert!(certs.is_empty());
    }

    #[test]
    fn test_split_certificate_chain_single() {
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB...leaf...\n-----END CERTIFICATE-----\n";
        let (leaf, chain) = split_certificate_chain(cert);
        assert_eq!(leaf, cert);
        assert!(chain.is_empty());
    }

    #[test]
    fn test_split_certificate_chain_multiple() {
        let chain_pem = "-----BEGIN CERTIFICATE-----\nMIIB...leaf...\n-----END CERTIFICATE-----\n\
                         -----BEGIN CERTIFICATE-----\nMIIB...intermediate...\n-----END CERTIFICATE-----\n\
                         -----BEGIN CERTIFICATE-----\nMIIB...root...\n-----END CERTIFICATE-----\n";
        let (leaf, chain) = split_certificate_chain(chain_pem);
        assert!(leaf.contains("leaf"));
        assert!(chain.contains("intermediate"));
        assert!(chain.contains("root"));
        assert!(!chain.contains("leaf"));
    }

    #[test]
    fn test_split_certificate_chain_empty() {
        let (leaf, chain) = split_certificate_chain("");
        assert!(leaf.is_empty());
        assert!(chain.is_empty());
    }
}
