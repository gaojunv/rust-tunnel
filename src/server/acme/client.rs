use super::{AcmeState, CertificateMetadata, CertificateStatus};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tracing::{error, info, warn};

/// ACME client for certificate management
pub struct AcmeClient {
    state: AcmeState,
    server_url: String,
    cert_dir: String,
    email: Option<String>,
}

impl AcmeClient {
    /// Create a new ACME client
    pub fn new(state: AcmeState, server_url: String, cert_dir: String, email: Option<String>) -> Self {
        Self {
            state,
            server_url,
            cert_dir,
            email,
        }
    }

    /// Initialize the ACME client
    pub async fn initialize(&self) -> Result<()> {
        info!("Initializing ACME client with server: {}", self.server_url);

        // Create cert directory if it doesn't exist
        let cert_dir = Path::new(&self.cert_dir);
        if !cert_dir.exists() {
            std::fs::create_dir_all(cert_dir)
                .context("Failed to create certificate directory")?;
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

        // Create certificate metadata
        let metadata = CertificateMetadata {
            domain: domain.to_string(),
            status: CertificateStatus::Pending,
            issued_at: None,
            expires_at: None,
            auto_renew: true,
            error: None,
        };

        // Save to database
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

        // TODO: Implement actual ACME protocol
        // For now, return pending status
        warn!("ACME certificate request not yet implemented");

        Ok(metadata)
    }

    /// Get certificate metadata for a domain
    pub async fn get_certificate_metadata(&self, domain: &str) -> Result<Option<CertificateMetadata>> {
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

        // TODO: Implement actual renewal
        self.request_certificate(domain).await
    }

    /// Delete a certificate
    pub async fn delete_certificate(&self, domain: &str) -> Result<()> {
        info!("Deleting certificate for domain: {}", domain);

        if let Some(db) = self.state.db() {
            db.delete_acme_certificate(domain).await?;
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
    async fn test_request_certificate_pending() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        let metadata = client
            .request_certificate("example.com")
            .await
            .unwrap();
        assert_eq!(metadata.domain, "example.com");
        assert_eq!(metadata.status, CertificateStatus::Pending);
        assert!(metadata.auto_renew);
        assert!(metadata.issued_at.is_none());
        assert!(metadata.expires_at.is_none());
        assert!(metadata.error.is_none());
    }

    #[tokio::test]
    async fn test_request_certificate_with_db_persists() {
        let db = Database::new(":memory:").await.unwrap();
        let state = AcmeState::with_db(db);
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        // Request a certificate
        let metadata = client
            .request_certificate("example.com")
            .await
            .unwrap();
        assert_eq!(metadata.status, CertificateStatus::Pending);

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
    async fn test_request_certificate_no_db() {
        let state = AcmeState::new();
        let client = AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "./data/certs".to_string(),
            Some("test@example.com".to_string()),
        );

        let metadata = client
            .request_certificate("example.com")
            .await
            .unwrap();
        assert_eq!(metadata.domain, "example.com");
        assert_eq!(metadata.status, CertificateStatus::Pending);
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

        let metadata = client
            .renew_certificate("example.com")
            .await
            .unwrap();
        assert_eq!(metadata.domain, "example.com");
        assert_eq!(metadata.status, CertificateStatus::Pending);
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

        // Request certificates for two different domains
        client.request_certificate("a.example.com").await.unwrap();
        client.request_certificate("b.example.com").await.unwrap();

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

        // Create a certificate
        client.request_certificate("example.com").await.unwrap();
        let certs = client.list_certificates().await.unwrap();
        assert_eq!(certs.len(), 1);

        // Delete it
        client.delete_certificate("example.com").await.unwrap();

        // Verify it's gone
        let certs = client.list_certificates().await.unwrap();
        assert!(certs.is_empty());
    }
}
