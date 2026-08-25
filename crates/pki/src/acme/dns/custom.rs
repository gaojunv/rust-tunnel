use crate::error::AcmeResult;
use async_trait::async_trait;
use std::time::Duration;

use super::{DnsChallengeSolver, DnsProviderConfig};

/// Custom DNS challenge solver (user-provided webhook)
pub struct CustomDnsSolver {
    #[allow(dead_code)]
    config: DnsProviderConfig,
}

impl CustomDnsSolver {
    #[must_use]
    pub fn new(config: &DnsProviderConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }
}

#[async_trait]
impl DnsChallengeSolver for CustomDnsSolver {
    async fn create_txt_record(&self, _domain: &str, _value: &str) -> AcmeResult<()> {
        todo!("Custom DNS TXT record creation via webhook")
    }

    async fn delete_txt_record(&self, _domain: &str, _value: &str) -> AcmeResult<()> {
        todo!("Custom DNS TXT record deletion via webhook")
    }

    async fn wait_for_propagation(
        &self,
        _domain: &str,
        _value: &str,
        _timeout: Duration,
    ) -> AcmeResult<()> {
        todo!("Custom DNS propagation check")
    }

    fn provider_name(&self) -> &'static str {
        "custom"
    }
}
