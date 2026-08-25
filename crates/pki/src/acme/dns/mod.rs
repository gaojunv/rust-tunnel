pub mod aliyun;
pub mod cloudflare;
pub mod custom;
pub mod tencent;

use crate::error::AcmeResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// DNS challenge solver trait
#[async_trait]
pub trait DnsChallengeSolver: Send + Sync {
    /// Create a TXT record for DNS-01 challenge
    async fn create_txt_record(&self, domain: &str, value: &str) -> AcmeResult<()>;

    /// Delete a TXT record after challenge completion
    async fn delete_txt_record(&self, domain: &str, value: &str) -> AcmeResult<()>;

    /// Wait for DNS propagation
    async fn wait_for_propagation(
        &self,
        domain: &str,
        value: &str,
        timeout: Duration,
    ) -> AcmeResult<()>;

    /// Get the provider name
    fn provider_name(&self) -> &str;
}

/// DNS provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsProviderConfig {
    pub provider: DnsProvider,
    pub api_key: String,
    pub api_secret: Option<String>,
    pub zone_id: Option<String>,
}

/// Supported DNS providers
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DnsProvider {
    Cloudflare,
    Aliyun,
    TencentCloud,
    Custom,
}

/// Registry of DNS providers
pub struct DnsProviderRegistry {
    providers: Vec<Box<dyn DnsChallengeSolver>>,
}

impl DnsProviderRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn register(&mut self, provider: Box<dyn DnsChallengeSolver>) {
        self.providers.push(provider);
    }

    #[must_use]
    pub fn get_provider(&self, name: &str) -> Option<&dyn DnsChallengeSolver> {
        self.providers
            .iter()
            .find(|p| p.provider_name() == name)
            .map(std::convert::AsRef::as_ref)
    }

    #[must_use]
    pub fn list_providers(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.provider_name()).collect()
    }
}

impl Default for DnsProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
