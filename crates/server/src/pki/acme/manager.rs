use super::client::AcmeClient;
use super::provider::{
    create_server_config_from_entry, CertEntry, CertSource, CertificateProvider,
};
use super::storage::CertificateStorage;
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use rustls::server::ResolvesServerCert;
use rustls::server::ServerConfig;
use rustls::sign::CertifiedKey;
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
    /// Lock-free snapshot of domain -> `CertifiedKey` for synchronous SNI resolution.
    /// Written together with `cache` on every add/remove/renew.
    pub(crate) certified_key_cache: ArcSwap<HashMap<String, Arc<CertifiedKey>>>,
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
            certified_key_cache: ArcSwap::from_pointee(HashMap::new()),
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
                    warn!("Failed to load certificate for domain {}: {}", domain, e);
                }
            }
        }

        let mut cache = self.cache.write().await;
        for (domain, cached) in entries {
            cache.insert(domain, cached);
        }
        let mut new_map: HashMap<String, Arc<CertifiedKey>> = HashMap::new();
        for (domain, cached) in cache.iter() {
            match super::provider::build_certified_key(&cached.entry) {
                Ok(ck) => {
                    new_map.insert(domain.clone(), ck);
                }
                Err(e) => warn!("Failed to build CertifiedKey for {}: {}", domain, e),
            }
        }
        self.certified_key_cache.store(Arc::new(new_map));
        drop(cache);

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
        // Normalize to lowercase so lookups by SNI (which rustls lowercases
        // before delivering to the resolver) always match. Storage on disk
        // uses the same normalized key.
        let domain = domain.to_ascii_lowercase();
        let tls_config = create_server_config_from_entry(&entry)
            .context("Failed to create TLS server config")?;
        let certified_key =
            super::provider::build_certified_key(&entry).context("Failed to build CertifiedKey")?;

        // Save to disk
        self.storage.save_certificate(
            &domain,
            &entry.cert_pem,
            &entry.key_pem,
            entry.chain_pem.as_deref(),
        )?;

        let cached = CachedCert { entry, tls_config };
        {
            // Hold the cache write lock for BOTH updates so external readers
            // (find_covering_cert async, resolve_certified_key sync) never
            // observe a state where one map has the domain and the other doesn't.
            let mut cache = self.cache.write().await;
            cache.insert(domain.clone(), cached);
            let old_snap = self.certified_key_cache.load();
            let mut new_map = (**old_snap).clone();
            new_map.insert(domain.clone(), certified_key);
            self.certified_key_cache.store(Arc::new(new_map));
        }

        self.broadcast_event(CertEvent::Issued {
            domain: domain.clone(),
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
        let domain = domain.to_ascii_lowercase();
        {
            let mut cache = self.cache.write().await;
            cache.remove(&domain);
            let old_snap = self.certified_key_cache.load();
            let mut new_map = (**old_snap).clone();
            new_map.remove(&domain);
            self.certified_key_cache.store(Arc::new(new_map));
        }
        self.storage.delete_certificate(&domain)?;

        self.broadcast_event(CertEvent::Expired {
            domain: domain.clone(),
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

        // Load the issued certificate from disk into cache + broadcast Issued
        self.load_issued_certificate(domain).await
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
            let mut interval = tokio::time::interval(Duration::from_secs(interval_hours * 3600));

            loop {
                interval.tick().await;
                info!("Checking certificates for renewal...");

                let domains_needing_renewal: Vec<String> = {
                    let cache = self.cache.read().await;
                    let now = chrono::Utc::now();
                    let threshold = now + chrono::Duration::days(days_before_expiry.cast_signed());

                    cache
                        .iter()
                        .filter_map(|(domain, cached)| match cached.entry.expires_at {
                            Some(expires) if expires <= threshold => Some(domain.clone()),
                            _ => None,
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

    /// Resolve a `CertifiedKey` for a given SNI value.
    ///
    /// 1. Exact match: look up the SNI directly.
    /// 2. Wildcard fallback: strip the leftmost label and look up `*.<rest>`.
    ///    Only one level of fallback (RFC 6125-style), and only if the
    ///    remaining suffix still contains a dot.
    ///
    /// Domain comparison is case-insensitive per RFC 6125 §6.4.1 — inputs
    /// are ASCII-lowercased before lookup. Callers do not need to
    /// pre-normalize.
    ///
    /// Synchronous — safe to call from `ResolvesServerCert::resolve`.
    #[must_use]
    pub fn resolve_certified_key(&self, sni: &str) -> Option<Arc<CertifiedKey>> {
        let sni = sni.to_ascii_lowercase();
        let map = self.certified_key_cache.load();
        if let Some(k) = map.get(&sni) {
            return Some(k.clone());
        }
        let wildcard = crate::reverse_proxy::sni_resolver::wildcard_for(&sni)?;
        map.get(&wildcard).cloned()
    }

    /// Check whether an existing certificate covers `domain`.
    ///
    /// Returns `Some(CertCoverage::Exact)` if an exact-match certificate exists,
    /// `Some(CertCoverage::Wildcard(pattern))` if a one-level wildcard covers it,
    /// or `None` otherwise. Comparison is case-insensitive.
    pub async fn find_covering_cert(&self, domain: &str) -> Option<super::provider::CertCoverage> {
        let domain = domain.to_ascii_lowercase();
        let cache = self.cache.read().await;
        if cache.contains_key(&domain) {
            return Some(super::provider::CertCoverage::Exact);
        }
        let wildcard = crate::reverse_proxy::sni_resolver::wildcard_for(&domain)?;
        if cache.contains_key(&wildcard) {
            return Some(super::provider::CertCoverage::Wildcard(wildcard));
        }
        None
    }

    /// 获取覆盖 `domain` 的 TLS `ServerConfig`：先精确匹配，未命中回退一层通配。
    ///
    /// 返回 `(config, coverage)`，`coverage` 标明命中的是精确证书还是通配符证书，
    /// 供调用方标注证书来源（如 Trojan 的 `cert_source`）。比较不区分大小写。
    pub async fn get_tls_server_config_covering(
        &self,
        domain: &str,
    ) -> Option<(Arc<ServerConfig>, super::provider::CertCoverage)> {
        let coverage = self.find_covering_cert(domain).await?;
        let key = match &coverage {
            super::provider::CertCoverage::Exact => domain.to_ascii_lowercase(),
            super::provider::CertCoverage::Wildcard(pattern) => pattern.clone(),
        };
        let cache = self.cache.read().await;
        cache.get(&key).map(|c| (c.tls_config.clone(), coverage))
    }

    /// 把磁盘上新签发的证书加载进内存缓存并广播 [`CertEvent::Issued`]。
    ///
    /// ACME 客户端（`AcmeClient`）签发成功后只写磁盘；调用本方法后证书
    /// 无需重启即可被 SNI resolver / Trojan 等消费者使用。
    ///
    /// # Errors
    ///
    /// Returns an error if the certificate cannot be loaded from disk or parsed.
    pub async fn load_issued_certificate(&self, domain: &str) -> Result<CertEntry> {
        // 磁盘读取沿用传入域名（ACME 客户端按原样写盘）；
        // 缓存 key 与事件 domain 小写归一，与 add_certificate /
        // find_covering_cert / resolve_certified_key 的不变量一致，
        // 否则大小写混合域名签发后写入成功却永远查不到。
        let cached = self
            .build_cached_cert(domain)
            .context("Failed to load issued certificate from storage")?;
        let certified_key = super::provider::build_certified_key(&cached.entry)
            .context("Failed to build CertifiedKey for issued cert")?;
        let entry = cached.entry.clone();
        let domain = domain.to_ascii_lowercase();
        {
            let mut cache = self.cache.write().await;
            cache.insert(domain.clone(), cached);
            let old_snap = self.certified_key_cache.load();
            let mut new_map = (**old_snap).clone();
            new_map.insert(domain.clone(), certified_key);
            self.certified_key_cache.store(Arc::new(new_map));
        }

        self.broadcast_event(CertEvent::Issued {
            domain: domain.clone(),
        });

        info!("Loaded issued certificate for domain: {}", domain);
        Ok(entry)
    }

    /// Build a resolver for use with `rustls::ServerConfig::with_cert_resolver`.
    #[must_use]
    pub fn sni_resolver(self: Arc<Self>) -> Arc<dyn ResolvesServerCert> {
        Arc::new(crate::reverse_proxy::sni_resolver::SniCertResolver::new(
            self,
        ))
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
        let manager = CertificateManager::new(temp_dir.path().to_str().unwrap());

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
            .save_certificate("bad.example.com", "not-a-cert", "not-a-key", None)
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
        let state = crate::acme::AcmeState::new();
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

    #[tokio::test]
    async fn test_find_covering_cert_exact() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(CertificateManager::new(temp_dir.path().to_str().unwrap()));

        let (cert_pem, key_pem) = gen_self_signed_pem_for("foo.example.com");
        let entry = CertEntry {
            cert_pem,
            key_pem,
            chain_pem: None,
            expires_at: None,
            source: CertSource::Manual,
        };
        manager
            .add_certificate("foo.example.com", entry)
            .await
            .unwrap();

        let cov = manager.find_covering_cert("foo.example.com").await;
        assert_eq!(cov, Some(super::super::provider::CertCoverage::Exact));
    }

    #[tokio::test]
    async fn test_find_covering_cert_wildcard() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(CertificateManager::new(temp_dir.path().to_str().unwrap()));

        let (cert_pem, key_pem) = gen_self_signed_pem_for("*.example.com");
        let entry = CertEntry {
            cert_pem,
            key_pem,
            chain_pem: None,
            expires_at: None,
            source: CertSource::Manual,
        };
        manager
            .add_certificate("*.example.com", entry)
            .await
            .unwrap();

        let cov = manager.find_covering_cert("api.example.com").await;
        assert_eq!(
            cov,
            Some(super::super::provider::CertCoverage::Wildcard(
                "*.example.com".to_string()
            ))
        );
    }

    #[tokio::test]
    async fn test_find_covering_cert_none() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(CertificateManager::new(temp_dir.path().to_str().unwrap()));
        assert!(manager
            .find_covering_cert("nope.example.com")
            .await
            .is_none());
    }

    #[test]
    fn test_resolve_certified_key_populated() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(CertificateManager::new(temp_dir.path().to_str().unwrap()));

        let (cert_pem, key_pem) = gen_self_signed_pem_for("foo.example.com");
        let entry = CertEntry {
            cert_pem,
            key_pem,
            chain_pem: None,
            expires_at: None,
            source: CertSource::Manual,
        };
        let ck = super::super::provider::build_certified_key(&entry).unwrap();
        let mut map = HashMap::new();
        map.insert("*.example.com".to_string(), ck);
        manager.certified_key_cache.store(Arc::new(map));

        // SNI = foo.example.com should hit *.example.com via wildcard fallback
        assert!(manager.resolve_certified_key("foo.example.com").is_some());
        // Bare apex has no wildcard derivation
        assert!(manager.resolve_certified_key("example.com").is_none());
    }

    fn gen_self_signed_pem_for(domain: &str) -> (String, String) {
        use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let params = CertificateParams::new(vec![domain.to_string()]).unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        (cert.pem(), key_pair.serialize_pem())
    }

    /// 造一张自签名证书并加入 manager。
    async fn add_test_cert(mgr: &CertificateManager, domain: &str) {
        use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
        let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let params = CertificateParams::new(vec![domain.to_string()]).unwrap();
        let cert = params.self_signed(&kp).unwrap();
        mgr.add_certificate(
            domain,
            super::CertEntry {
                cert_pem: cert.pem(),
                key_pem: kp.serialize_pem(),
                chain_pem: None,
                expires_at: None,
                source: super::CertSource::Manual,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn covering_config_exact_match() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = CertificateManager::new(temp_dir.path().to_str().unwrap());
        add_test_cert(&mgr, "trojan.example.com").await;

        let (cfg, coverage) = mgr
            .get_tls_server_config_covering("trojan.example.com")
            .await
            .expect("exact match should hit");
        assert_eq!(coverage, crate::acme::provider::CertCoverage::Exact);
        assert_eq!(
            cfg.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[tokio::test]
    async fn covering_config_wildcard_fallback() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = CertificateManager::new(temp_dir.path().to_str().unwrap());
        add_test_cert(&mgr, "*.example.com").await;

        let (_, coverage) = mgr
            .get_tls_server_config_covering("trojan.example.com")
            .await
            .expect("wildcard should cover");
        assert_eq!(
            coverage,
            crate::acme::provider::CertCoverage::Wildcard("*.example.com".to_string())
        );

        // 不相关的域名不命中
        assert!(mgr
            .get_tls_server_config_covering("other.com")
            .await
            .is_none());
        // 两层子域不被一层通配覆盖（wildcard_for 规则）
        assert!(mgr
            .get_tls_server_config_covering("a.b.example.com")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn load_issued_certificate_populates_cache_and_broadcasts() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = CertificateManager::new(temp_dir.path().to_str().unwrap());
        let mut rx = mgr.subscribe();

        // 模拟 ACME 客户端只写磁盘的结果
        use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
        let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let params = CertificateParams::new(vec!["new.example.com".to_string()]).unwrap();
        let cert = params.self_signed(&kp).unwrap();
        mgr.storage
            .save_certificate("new.example.com", &cert.pem(), &kp.serialize_pem(), None)
            .unwrap();

        mgr.load_issued_certificate("new.example.com")
            .await
            .unwrap();

        // 内存缓存立即可用（CertificateProvider 已随 use super::* 引入）
        assert!(mgr.get_tls_server_config("new.example.com").await.is_some());
        assert!(mgr.resolve_certified_key("new.example.com").is_some());

        // 广播了 Issued 事件（tests mod 已 use super::*，CertEvent 直接在作用域内）
        match rx.try_recv() {
            Ok(CertEvent::Issued { domain }) => {
                assert_eq!(domain, "new.example.com")
            }
            _ => panic!("expected Issued event"),
        }
    }

    /// 大小写混合域名签发后：缓存 key 小写归一，find_covering_cert /
    /// resolve_certified_key（查找一律 lowercase）能命中，事件 domain 同为小写。
    #[tokio::test]
    async fn load_issued_certificate_normalizes_mixed_case_domain() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = CertificateManager::new(temp_dir.path().to_str().unwrap());
        let mut rx = mgr.subscribe();

        // ACME 客户端按传入域名原样写盘
        use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
        let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let params = CertificateParams::new(vec!["Mixed.Example.com".to_string()]).unwrap();
        let cert = params.self_signed(&kp).unwrap();
        mgr.storage
            .save_certificate("Mixed.Example.com", &cert.pem(), &kp.serialize_pem(), None)
            .unwrap();

        mgr.load_issued_certificate("Mixed.Example.com")
            .await
            .unwrap();

        assert_eq!(
            mgr.find_covering_cert("mixed.example.com").await,
            Some(crate::acme::provider::CertCoverage::Exact)
        );
        assert!(mgr.resolve_certified_key("mixed.example.com").is_some());
        match rx.try_recv() {
            Ok(CertEvent::Issued { domain }) => {
                assert_eq!(domain, "mixed.example.com")
            }
            _ => panic!("expected Issued event"),
        }
    }
}
