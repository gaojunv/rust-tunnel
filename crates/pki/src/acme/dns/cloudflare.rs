use crate::error::AcmeResult;
use async_trait::async_trait;
use std::time::Duration;

use super::{DnsChallengeSolver, DnsProviderConfig};

/// Cloudflare DNS challenge solver
pub struct CloudflareDnsSolver {
    #[allow(dead_code)]
    config: DnsProviderConfig,
}

impl CloudflareDnsSolver {
    pub fn new(config: &DnsProviderConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }
}

#[async_trait]
impl DnsChallengeSolver for CloudflareDnsSolver {
    async fn create_txt_record(&self, _domain: &str, _value: &str) -> AcmeResult<()> {
        todo!("Cloudflare DNS TXT record creation")
    }

    async fn delete_txt_record(&self, _domain: &str, _value: &str) -> AcmeResult<()> {
        todo!("Cloudflare DNS TXT record deletion")
    }

    async fn wait_for_propagation(
        &self,
        _domain: &str,
        _value: &str,
        _timeout: Duration,
    ) -> AcmeResult<()> {
        todo!("Cloudflare DNS propagation check")
    }

    fn provider_name(&self) -> &str {
        "cloudflare"
    }
}
