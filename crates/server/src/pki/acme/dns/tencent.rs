use async_trait::async_trait;
use std::time::Duration;

use super::{DnsChallengeSolver, DnsProviderConfig};

/// Tencent Cloud DNS challenge solver
pub struct TencentDnsSolver {
    #[allow(dead_code)]
    config: DnsProviderConfig,
}

impl TencentDnsSolver {
    pub fn new(config: &DnsProviderConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }
}

#[async_trait]
impl DnsChallengeSolver for TencentDnsSolver {
    async fn create_txt_record(&self, _domain: &str, _value: &str) -> anyhow::Result<()> {
        todo!("Tencent DNS TXT record creation")
    }

    async fn delete_txt_record(&self, _domain: &str, _value: &str) -> anyhow::Result<()> {
        todo!("Tencent DNS TXT record deletion")
    }

    async fn wait_for_propagation(
        &self,
        _domain: &str,
        _value: &str,
        _timeout: Duration,
    ) -> anyhow::Result<()> {
        todo!("Tencent DNS propagation check")
    }

    fn provider_name(&self) -> &str {
        "tencentcloud"
    }
}
