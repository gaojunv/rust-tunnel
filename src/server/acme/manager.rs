use super::client::AcmeClient;
use super::provider::{
    create_server_config_from_entry, CertEntry, CertSource, CertificateProvider,
};
use super::storage::CertificateStorage;
use anyhow::{Context, Result};
use async_trait::async_trait;
use rustls::server::ServerConfig;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tracing::{error, info, warn};
use x509_parser::prelude::*;

/// Certificate events for notification
#[derive(Debug, Clone)]
pub enum CertEvent {
    /// A new certificate was issued
    Issued {
        /// The domain the certificate was issued for
        domain: String,
    },
    /// A certificate was renewed
    Renewed {
        /// The domain the certificate was renewed for
        domain: String,
    },
    /// A certificate expired
    Expired {
        /// The domain whose certificate expired
        domain: String,
    },
    /// An error occurred with a certificate
    Error {
        /// The domain the error is associated with
        domain: String,
        /// The error message
        error: String,
    },
}

/// Cached certificate entry with pre-built TLS config
struct CachedCert {
    entry: CertEntry,
    tls_config: Arc<ServerConfig>,
}

/// Central certificate manager implementing [`CertificateProvider`]
///
/// Manages TLS certificates with in-memory caching, disk persistence via
/// `CertificateStorage`, optional ACME automation, and event broadcasting.
pub struct CertificateManager {
    /// ACME client for certificate operations (optional)
    acme_client: RwLock<Option<Arc<AcmeClient>>>,
    /// In-memory certificate cache
    cache: RwLock<HashMap<String, CachedCert>>,
    /// Disk storage
    storage: CertificateStorage,
    /// Event broadcast sender
    event_tx: broadcast::Sender<CertEvent>,
}

impl CertificateManager {
    /// Create a new certificate manager
    ///
    /// Initializes the manager with the given certificate directory.
    /// Certificates are not loaded from disk until [`load_from_storage`] is called.
    #[must_use]
    pub fn new(cert_dir: &str) -> Self {
        let storage = CertificateStorage::new(cert_dir);
        let (event_tx, _) = broadcast::channel(64);

        Self {
            acme_client: RwLock::new(None),
            cache: RwLock::new(HashMap::new()),
            storage,
            event_tx,
        }
    }

    /// Set the ACME client for automated certificate operations
    pub async fn set_acme_client(&self, client: Arc<AcmeClient>) {
        *self.acme_client.write().await = Some(client);
    }

    /// Subscribe to certificate events
    pub fn subscribe(&self) -> broadcast::Receiver<CertEvent> {
        self.event_tx.subscribe()
    }

    /// Broadcast a certificate event to all subscribers
    fn broadcast_event(&self, event: CertEvent) {
        // Ignore send errors (no active subscribers)
        let _ = self.event_tx.send(event);
    }

    /// Load certificates from disk storage into the in-memory cache
    ///
    /// Iterates over all domains in the certificate directory, loads their
    /// certificate files, and populates the cache with parsed entries and
    /// pre-built TLS configurations.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage directory cannot be initialized or read.
    pub async fn load_from_storage(&self) -> Result<()> {
        self.storage.initialize()?;
        let domains = self.storage.list_domains()?;
        info!("Loading {} certificates from storage", domains.len());

        let mut entries = Vec::new();
        for domain in &domains {
            match self.build_cached_cert(domain) {
                Ok(cached) => {
                    info!("Loaded certificate for domain: {}", domain);
                    entries.push((domain.clone(), cached));
                }
                Err(e) => {
                    warn!(
                        "Failed to load certificate for domain {}: {}",
                        domain, e
                    );
                }
            }
        }

        let mut cache = self.cache.write().await;
        for (domain, cached) in entries {
            cache.insert(domain, cached);
        }

        Ok(())
    }

    /// Build a `CachedCert` from disk storage for a given domain
    fn build_cached_cert(&self, domain: &str) -> Result<CachedCert> {
        let cert_pem = self
            .storage
            .load_certificate(domain)?
            .context("Certificate PEM not found")?;

        let key_pem = self
            .storage
            .load_private_key(domain)?
            .context("Private key PEM not found")?;

        let chain_pem = self.storage.load_chain(domain)?;
        let expires_at = parse_cert_expiry(&cert_pem).ok();

        let entry = CertEntry {
            cert_pem,
            key_pem,
            chain_pem,
            expires_at,
            source: CertSource::Acme,
        };

        let tls_config = create_server_config_from_entry(&entry)
            .context("Failed to create TLS server config")?;

        Ok(CachedCert { entry, tls_config })
    }

    /// Add a certificate manually
    ///
    /// Saves the certificate to disk and adds it to the in-memory cache.
    /// Broadcasts a [`CertEvent::Issued`] event on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the TLS server config cannot be created from the
    /// certificate entry, or if saving to disk fails.
    pub async fn add_certificate(&self, domain: &str, entry: CertEntry) -> Result<()> {
        let tls_config = create_server_config_from_entry(&entry)
            .context("Failed to create TLS server config")?;

        // Save to disk
        self.storage.save_certificate(
            domain,
            &entry.cert_pem,
            &entry.key_pem,
            entry.chain_pem.as_deref(),
        )?;

        // Add to cache
        let cached = CachedCert { entry, tls_config };
        self.cache
            .write()
            .await
            .insert(domain.to_string(), cached);

        self.broadcast_event(CertEvent::Issued {
            domain: domain.to_string(),
        });

        info!("Added certificate for domain: {}", domain);
        Ok(())
    }

    /// Remove a certificate
    ///
    /// Removes the certificate from both the in-memory cache and disk storage.
    /// Broadcasts a [`CertEvent::Expired`] event on success.
    ///
    /// # Errors
    ///
    /// Returns an error if deleting the certificate from disk fails.
    pub async fn remove_certificate(&self, domain: &str) -> Result<()> {
        self.cache.write().await.remove(domain);
        self.storage.delete_certificate(domain)?;

        self.broadcast_event(CertEvent::Expired {
            domain: domain.to_string(),
        });

        info!("Removed certificate for domain: {}", domain);
        Ok(())
    }

    /// Request a certificate via ACME
    ///
    /// Delegates to the configured [`AcmeClient`] to obtain a new certificate,
    /// then loads the issued certificate from disk into the cache.
    /// Returns the [`CertEntry`] for the newly issued certificate.
    ///
    /// # Errors
    ///
    /// Returns an error if the ACME client is not configured, the ACME request
    /// fails, or the issued certificate cannot be loaded from disk.
    pub async fn request_acme_certificate(&self, domain: &str) -> Result<CertEntry> {
        let client = {
            let guard = self.acme_client.read().await;
            guard
                .as_ref()
                .context("ACME client not configured")?
                .clone()
        };

        // Request certificate via ACME (saves to disk internally)
        client
            .request_certificate(domain)
            .await
            .context("ACME certificate request failed")?;

        // Load the issued certificate from disk
        let cached = self
            .build_cached_cert(domain)
            .context("Failed to load issued certificate from storage")?;

        let entry = cached.entry.clone();
        self.cache
            .write()
            .await
            .insert(domain.to_string(), cached);

        self.broadcast_event(CertEvent::Issued {
            domain: domain.to_string(),
        });

        info!("Issued ACME certificate for domain: {}", domain);
        Ok(entry)
    }

    /// Start the background certificate renewal task
    ///
    /// Periodically checks all cached certificates and renews those expiring
    /// within `days_before_expiry` days. Broadcasts [`CertEvent::Renewed`] on
    /// success and [`CertEvent::Error`] on failure.
    pub fn start_renewal_task(
        self: Arc<Self>,
        interval_hours: u64,
        days_before_expiry: u64,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(interval_hours * 3600));

            loop {
                interval.tick().await;
                info!("Checking certificates for renewal...");

                let domains_needing_renewal: Vec<String> = {
                    let cache = self.cache.read().await;
                    let now = chrono::Utc::now();
                    let threshold = now + chrono::Duration::days(days_before_expiry.cast_signed());

                    cache
                        .iter()
                        .filter_map(|(domain, cached)| {
                            match cached.entry.expires_at {
                                Some(expires) if expires <= threshold => Some(domain.clone()),
                                _ => None,
                            }
                        })
                        .collect()
                };

                for domain in &domains_needing_renewal {
                    info!("Renewing certificate for domain: {}", domain);
                    match self.request_acme_certificate(domain).await {
                        Ok(_) => {
                            self.broadcast_event(CertEvent::Renewed {
                                domain: domain.clone(),
                            });
                            info!("Successfully renewed certificate for {}", domain);
                        }
                        Err(e) => {
                            error!("Failed to renew certificate for {}: {}", domain, e);
                            self.broadcast_event(CertEvent::Error {
                                domain: domain.clone(),
                                error: e.to_string(),
                            });
                        }
                    }
                }
            }
        })
    }
}

#[async_trait]
impl CertificateProvider for CertificateManager {
    async fn get_certificate(&self, domain: &str) -> Option<CertEntry> {
        let cache = self.cache.read().await;
        cache.get(domain).map(|cached| cached.entry.clone())
    }

    async fn get_tls_server_config(&self, domain: &str) -> Option<Arc<ServerConfig>> {
        let cache = self.cache.read().await;
        cache.get(domain).map(|cached| cached.tls_config.clone())
    }
}

/// Parse certificate expiry from PEM-encoded certificate data
fn parse_cert_expiry(cert_pem: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .context("Failed to parse certificate PEM")?;

    let (_, cert) =
        X509Certificate::from_der(&pem.contents).context("Failed to parse certificate DER")?;

    let not_after = cert.validity.not_after.to_datetime();
    let ts = not_after.unix_timestamp();
    let naive = chrono::DateTime::from_timestamp(ts, 0)
        .context("Failed to create DateTime from timestamp")?;

    Ok(naive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cert_event_clone() {
        let event = CertEvent::Issued {
            domain: "example.com".to_string(),
        };
        let cloned = event.clone();
        match cloned {
            CertEvent::Issued { domain } => assert_eq!(domain, "example.com"),
            _ => panic!("Unexpected event variant"),
        }
    }

    #[test]
    fn test_cert_event_debug() {
        let issued = CertEvent::Issued {
            domain: "a.com".to_string(),
        };
        let renewed = CertEvent::Renewed {
            domain: "b.com".to_string(),
        };
        let expired = CertEvent::Expired {
            domain: "c.com".to_string(),
        };
        let err = CertEvent::Error {
            domain: "d.com".to_string(),
            error: "test error".to_string(),
        };

        assert!(!format!("{issued:?}").is_empty());
        assert!(!format!("{renewed:?}").is_empty());
        assert!(!format!("{expired:?}").is_empty());
        assert!(!format!("{err:?}").is_empty());
    }

    #[test]
    fn test_manager_new() {
        let manager = CertificateManager::new("/tmp/test-certs");
        let cache = manager.cache.try_read().unwrap();
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn test_subscribe_and_receive_event() {
        let manager = CertificateManager::new("/tmp/test-certs");
        let mut rx = manager.subscribe();

        manager.broadcast_event(CertEvent::Issued {
            domain: "test.com".to_string(),
        });

        let event = rx.recv().await.unwrap();
        match event {
            CertEvent::Issued { domain } => assert_eq!(domain, "test.com"),
            _ => panic!("Unexpected event type"),
        }
    }

    #[tokio::test]
    async fn test_subscribe_multiple_events() {
        let manager = CertificateManager::new("/tmp/test-certs");
        let mut rx = manager.subscribe();

        manager.broadcast_event(CertEvent::Issued {
            domain: "a.com".to_string(),
        });
        manager.broadcast_event(CertEvent::Renewed {
            domain: "b.com".to_string(),
        });

        let e1 = rx.recv().await.unwrap();
        let e2 = rx.recv().await.unwrap();

        match e1 {
            CertEvent::Issued { domain } => assert_eq!(domain, "a.com"),
            _ => panic!("Expected Issued event"),
        }
        match e2 {
            CertEvent::Renewed { domain } => assert_eq!(domain, "b.com"),
            _ => panic!("Expected Renewed event"),
        }
    }

    #[tokio::test]
    async fn test_get_nonexistent_certificate() {
        let manager = CertificateManager::new("/tmp/test-certs");
        let result = manager.get_certificate("nonexistent.com").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_tls_config_nonexistent() {
        let manager = CertificateManager::new("/tmp/test-certs");
        let result = manager.get_tls_server_config("nonexistent.com").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_add_certificate_invalid_pem() {
        let manager = CertificateManager::new("/tmp/test-certs");

        let entry = CertEntry {
            cert_pem: "not a real cert".to_string(),
            key_pem: "not a real key".to_string(),
            chain_pem: None,
            expires_at: None,
            source: CertSource::Manual,
        };

        let result = manager.add_certificate("test.com", entry).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_remove_certificate_nonexistent() {
        let manager = CertificateManager::new("/tmp/test-certs");
        let result = manager.remove_certificate("nonexistent.com").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_load_from_storage_empty_dir() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let manager = CertificateManager::new(
            temp_dir.path().to_str().unwrap(),
        );

        manager.load_from_storage().await.unwrap();

        let cache = manager.cache.read().await;
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn test_load_from_storage_invalid_cert() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cert_dir = temp_dir.path().to_str().unwrap().to_string();

        // Save invalid cert files to disk
        let storage = CertificateStorage::new(&cert_dir);
        storage.initialize().unwrap();
        storage
            .save_certificate(
                "bad.example.com",
                "not-a-cert",
                "not-a-key",
                None,
            )
            .unwrap();

        let manager = CertificateManager::new(&cert_dir);
        // Should succeed (logs warning for invalid cert, continues)
        manager.load_from_storage().await.unwrap();

        let cache = manager.cache.read().await;
        assert!(cache.is_empty());
    }

    #[test]
    fn test_parse_cert_expiry_invalid() {
        let result = parse_cert_expiry("not a real cert");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_cert_expiry_empty() {
        let result = parse_cert_expiry("");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_request_acme_no_client() {
        let manager = CertificateManager::new("/tmp/test-certs");
        let result = manager.request_acme_certificate("example.com").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("ACME client not configured"));
    }

    #[tokio::test]
    async fn test_set_acme_client() {
        let manager = CertificateManager::new("/tmp/test-certs");
        let state = crate::server::acme::AcmeState::new();
        let client = Arc::new(AcmeClient::new(
            state,
            "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            "/tmp/test-acme-certs".to_string(),
            None,
        ));

        manager.set_acme_client(client).await;

        // Verify the client was set
        let guard = manager.acme_client.read().await;
        assert!(guard.is_some());
    }

    #[tokio::test]
    async fn test_broadcast_without_subscribers() {
        let manager = CertificateManager::new("/tmp/test-certs");
        // Should not panic when there are no subscribers
        manager.broadcast_event(CertEvent::Issued {
            domain: "test.com".to_string(),
        });
    }
}
