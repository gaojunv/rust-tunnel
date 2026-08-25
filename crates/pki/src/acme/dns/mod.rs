//! DNS-01 挑战与 DNS 提供商集成：solver trait、配置与注册表。

/// 阿里云 DNS 解析。
pub mod aliyun;
/// Cloudflare DNS 解析。
pub mod cloudflare;
/// 自定义 webhook DNS 解析。
pub mod custom;
/// 腾讯云 DNS 解析。
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
    /// DNS 提供商类型。
    pub provider: DnsProvider,
    /// API 密钥。
    pub api_key: String,
    /// API 密钥的 secret，部分提供商需要。
    pub api_secret: Option<String>,
    /// 域名 zone 标识，部分提供商需要。
    pub zone_id: Option<String>,
}

/// Supported DNS providers
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DnsProvider {
    /// Cloudflare。
    Cloudflare,
    /// 阿里云。
    Aliyun,
    /// 腾讯云。
    TencentCloud,
    /// 自定义 webhook。
    Custom,
}

/// Registry of DNS providers
pub struct DnsProviderRegistry {
    providers: Vec<Box<dyn DnsChallengeSolver>>,
}

impl DnsProviderRegistry {
    /// 创建空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// 注册一个 DNS 提供商实现。
    pub fn register(&mut self, provider: Box<dyn DnsChallengeSolver>) {
        self.providers.push(provider);
    }

    /// 按名称取提供商，未找到返回 None。
    #[must_use]
    pub fn get_provider(&self, name: &str) -> Option<&dyn DnsChallengeSolver> {
        self.providers
            .iter()
            .find(|p| p.provider_name() == name)
            .map(std::convert::AsRef::as_ref)
    }

    /// 列出已注册提供商名称。
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
