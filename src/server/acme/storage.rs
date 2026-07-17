use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::info;

/// Certificate storage manager
pub struct CertificateStorage {
    base_dir: PathBuf,
}

impl CertificateStorage {
    /// Create a new certificate storage
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Get the directory for a domain
    fn domain_dir(&self, domain: &str) -> PathBuf {
        self.base_dir.join(domain)
    }

    /// Initialize storage directory
    pub fn initialize(&self) -> Result<()> {
        if !self.base_dir.exists() {
            std::fs::create_dir_all(&self.base_dir)
                .context("Failed to create certificate storage directory")?;
        }
        Ok(())
    }

    /// Save certificate files for a domain
    pub fn save_certificate(
        &self,
        domain: &str,
        cert_pem: &str,
        key_pem: &str,
        chain_pem: Option<&str>,
    ) -> Result<()> {
        let dir = self.domain_dir(domain);
        if !dir.exists() {
            std::fs::create_dir_all(&dir).context("Failed to create domain directory")?;
        }

        // Write certificate
        std::fs::write(dir.join("cert.pem"), cert_pem).context("Failed to write certificate")?;

        // Write private key
        std::fs::write(dir.join("key.pem"), key_pem).context("Failed to write private key")?;

        // Write chain if provided
        if let Some(chain) = chain_pem {
            std::fs::write(dir.join("chain.pem"), chain)
                .context("Failed to write certificate chain")?;
        }

        info!("Saved certificate files for domain: {}", domain);
        Ok(())
    }

    /// Load certificate PEM for a domain
    pub fn load_certificate(&self, domain: &str) -> Result<Option<String>> {
        let cert_path = self.domain_dir(domain).join("cert.pem");
        if cert_path.exists() {
            let cert = std::fs::read_to_string(&cert_path).context("Failed to read certificate")?;
            Ok(Some(cert))
        } else {
            Ok(None)
        }
    }

    /// Load private key PEM for a domain
    pub fn load_private_key(&self, domain: &str) -> Result<Option<String>> {
        let key_path = self.domain_dir(domain).join("key.pem");
        if key_path.exists() {
            let key = std::fs::read_to_string(&key_path).context("Failed to read private key")?;
            Ok(Some(key))
        } else {
            Ok(None)
        }
    }

    /// Load certificate chain PEM for a domain
    pub fn load_chain(&self, domain: &str) -> Result<Option<String>> {
        let chain_path = self.domain_dir(domain).join("chain.pem");
        if chain_path.exists() {
            let chain =
                std::fs::read_to_string(&chain_path).context("Failed to read certificate chain")?;
            Ok(Some(chain))
        } else {
            Ok(None)
        }
    }

    /// Delete certificate files for a domain
    pub fn delete_certificate(&self, domain: &str) -> Result<()> {
        let dir = self.domain_dir(domain);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).context("Failed to delete certificate directory")?;
            info!("Deleted certificate files for domain: {}", domain);
        }
        Ok(())
    }

    /// Check if certificate exists for a domain
    pub fn has_certificate(&self, domain: &str) -> bool {
        self.domain_dir(domain).join("cert.pem").exists()
    }

    /// List all domains with certificates
    pub fn list_domains(&self) -> Result<Vec<String>> {
        let mut domains = Vec::new();

        if self.base_dir.exists() {
            for entry in std::fs::read_dir(&self.base_dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        if self.has_certificate(name) {
                            domains.push(name.to_string());
                        }
                    }
                }
            }
        }

        Ok(domains)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load_certificate() {
        let temp_dir = TempDir::new().unwrap();
        let storage = CertificateStorage::new(temp_dir.path());

        let cert = "-----BEGIN CERTIFICATE-----\ntest cert\n-----END CERTIFICATE-----";
        let key = "-----BEGIN PRIVATE KEY-----\ntest key\n-----END PRIVATE KEY-----";

        storage
            .save_certificate("example.com", cert, key, None)
            .unwrap();

        let loaded_cert = storage.load_certificate("example.com").unwrap();
        assert!(loaded_cert.is_some());
        assert_eq!(loaded_cert.unwrap(), cert);

        let loaded_key = storage.load_private_key("example.com").unwrap();
        assert!(loaded_key.is_some());
        assert_eq!(loaded_key.unwrap(), key);
    }

    #[test]
    fn test_delete_certificate() {
        let temp_dir = TempDir::new().unwrap();
        let storage = CertificateStorage::new(temp_dir.path());

        let cert = "-----BEGIN CERTIFICATE-----\ntest cert\n-----END CERTIFICATE-----";
        let key = "-----BEGIN PRIVATE KEY-----\ntest key\n-----END PRIVATE KEY-----";

        storage
            .save_certificate("example.com", cert, key, None)
            .unwrap();
        assert!(storage.has_certificate("example.com"));

        storage.delete_certificate("example.com").unwrap();
        assert!(!storage.has_certificate("example.com"));
    }

    #[test]
    fn test_list_domains() {
        let temp_dir = TempDir::new().unwrap();
        let storage = CertificateStorage::new(temp_dir.path());

        let cert = "-----BEGIN CERTIFICATE-----\ntest cert\n-----END CERTIFICATE-----";
        let key = "-----BEGIN PRIVATE KEY-----\ntest key\n-----END PRIVATE KEY-----";

        storage
            .save_certificate("example.com", cert, key, None)
            .unwrap();
        storage
            .save_certificate("test.com", cert, key, None)
            .unwrap();

        let domains = storage.list_domains().unwrap();
        assert_eq!(domains.len(), 2);
        assert!(domains.contains(&"example.com".to_string()));
        assert!(domains.contains(&"test.com".to_string()));
    }
}
