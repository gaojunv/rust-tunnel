use async_trait::async_trait;
use std::time::Duration;

use super::{DnsChallengeSolver, DnsProviderConfig};

/// Aliyun DNS challenge solver
pub struct AliyunDnsSolver {
    #[allow(dead_code)]
    config: DnsProviderConfig,
}

impl AliyunDnsSolver {
    pub fn new(config: &DnsProviderConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }
}

#[async_trait]
impl DnsChallengeSolver for AliyunDnsSolver {
    async fn create_txt_record(&self, _domain: &str, _value: &str) -> anyhow::Result<()> {
        todo!("Aliyun DNS TXT record creation")
    }

    async fn delete_txt_record(&self, _domain: &str, _value: &str) -> anyhow::Result<()> {
        todo!("Aliyun DNS TXT record deletion")
    }

    async fn wait_for_propagation(
        &self,
        _domain: &str,
        _value: &str,
        _timeout: Duration,
    ) -> anyhow::Result<()> {
        todo!("Aliyun DNS propagation check")
    }

    fn provider_name(&self) -> &str {
        "aliyun"
    }
}
