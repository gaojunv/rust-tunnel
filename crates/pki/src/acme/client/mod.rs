pub mod cert_utils;
pub mod challenge_flow;
pub mod order;
pub mod renewal;

pub use renewal::start_renewal_task;

use super::storage::CertificateStorage;
use super::{AcmeState, CertificateMetadata, CertificateStatus};
use crate::error::{AcmeError, AcmeResult};
use instant_acme::{Account, AccountCredentials};
use std::path::Path;
use tokio::sync::RwLock;
use tracing::{info, warn};

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
    pub async fn initialize(&self) -> AcmeResult<()> {
        info!("Initializing ACME client with server: {}", self.server_url);

        // Create cert directory if it doesn't exist
        let cert_dir = Path::new(&self.cert_dir);
        if !cert_dir.exists() {
            std::fs::create_dir_all(cert_dir)
                .map_err(AcmeError::storage("Failed to create certificate directory"))?;
        }

        // Try to load existing account credentials
        let account_path = cert_dir.join("account.json");
        if account_path.exists() {
            let data = std::fs::read_to_string(&account_path)
                .map_err(AcmeError::storage("Failed to read account credentials file"))?;
            let creds: AccountCredentials = serde_json::from_str(&data)
                .map_err(|source| AcmeError::AccountSerde {
                    context: "Failed to parse account credentials",
                    source,
                })?;

            // Re-parse credentials for from_credentials since AccountCredentials doesn't implement Clone
            let creds_for_restore: AccountCredentials = serde_json::from_str(&data).map_err(
                |source| AcmeError::AccountSerde {
                    context: "Failed to parse account credentials for restore",
                    source,
                },
            )?;

            let account = Account::builder()?
                .from_credentials(creds_for_restore)
                .await
                .map_err(AcmeError::protocol(
                    "Failed to restore ACME account from saved credentials",
                ))?;

            info!("Restored existing ACME account: {}", account.id());
            *self.account.write().await = Some(account);
            *self.credentials.write().await = Some(creds);
        } else {
            // Register new account with the ACME server
            let contact_email = self.email.as_deref().unwrap_or("noreply@example.com");
            let contact_entry = format!("mailto:{contact_email}");

            let (account, credentials) = Account::builder()?
                .create(
                    &instant_acme::NewAccount {
                        contact: &[&contact_entry],
                        terms_of_service_agreed: true,
                        only_return_existing: false,
                    },
                    self.server_url.clone(),
                    None,
                )
                .await
                .map_err(AcmeError::protocol("Failed to register new ACME account"))?;

            // Save credentials to disk
            let creds_json = serde_json::to_string_pretty(&credentials).map_err(|source| {
                AcmeError::AccountSerde {
                    context: "Failed to serialize account credentials",
                    source,
                }
            })?;
            std::fs::write(&account_path, creds_json)
                .map_err(AcmeError::storage("Failed to save account credentials to disk"))?;

            info!("Registered new ACME account: {}", account.id());
            *self.account.write().await = Some(account);
            *self.credentials.write().await = Some(credentials);
        }

        Ok(())
    }

    /// Get certificate metadata for a domain
    pub async fn get_certificate_metadata(
        &self,
        domain: &str,
    ) -> AcmeResult<Option<CertificateMetadata>> {
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
    pub async fn list_certificates(&self) -> AcmeResult<Vec<CertificateMetadata>> {
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
    pub async fn renew_certificate(&self, domain: &str) -> AcmeResult<CertificateMetadata> {
        info!("Renewing certificate for domain: {}", domain);

        // Update renewal attempt timestamp
        if let Some(db) = self.state.db() {
            db.update_acme_certificate_renewal_attempt(domain).await?;
        }

        self.request_certificate(domain).await
    }

    /// Delete a certificate
    pub async fn delete_certificate(&self, domain: &str) -> AcmeResult<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme::AcmeState;
    use rust_tunnel_persistence::Database;

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
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("ACME account not initialized"));
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
                .save_acme_certificate("example.com", "pending", None, None, None, None, None, true)
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
        let metadata = client.request_certificate("example.com").await.unwrap();
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
                .save_acme_certificate("example.com", "active", None, None, None, None, None, true)
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
        let (leaf, chain) = cert_utils::split_certificate_chain(cert);
        assert_eq!(leaf, cert);
        assert!(chain.is_empty());
    }

    #[test]
    fn test_split_certificate_chain_multiple() {
        let chain_pem = "-----BEGIN CERTIFICATE-----\nMIIB...leaf...\n-----END CERTIFICATE-----\n\
                         -----BEGIN CERTIFICATE-----\nMIIB...intermediate...\n-----END CERTIFICATE-----\n\
                         -----BEGIN CERTIFICATE-----\nMIIB...root...\n-----END CERTIFICATE-----\n";
        let (leaf, chain) = cert_utils::split_certificate_chain(chain_pem);
        assert!(leaf.contains("leaf"));
        assert!(chain.contains("intermediate"));
        assert!(chain.contains("root"));
        assert!(!chain.contains("leaf"));
    }

    #[test]
    fn test_split_certificate_chain_empty() {
        let (leaf, chain) = cert_utils::split_certificate_chain("");
        assert!(leaf.is_empty());
        assert!(chain.is_empty());
    }
}
