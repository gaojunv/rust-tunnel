//! ACME 证书管理：状态、元数据与共享状态，子模块负责签发、挑战、存储与 DNS。

/// ACME 挑战处理模块。
pub mod challenge;
/// ACME 客户端实现模块。
pub mod client;
/// DNS-01 挑战与 DNS 提供商集成。
pub mod dns;
/// 证书管理器与事件。
pub mod manager;
/// 证书提供与覆盖判定。
pub mod provider;
/// 证书持久化存储。
pub mod storage;

pub use manager::{CertEvent, CertificateManager};
pub use provider::{CertCoverage, CertEntry, CertSource, CertificateProvider};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use rust_tunnel_persistence::Database;

/// ACME certificate status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CertificateStatus {
    /// 待签发/验证中。
    Pending,
    /// 已签发且在有效期内。
    Active,
    /// 已过期。
    Expired,
    /// 签发失败。
    Failed,
}

impl std::fmt::Display for CertificateStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Active => write!(f, "active"),
            Self::Expired => write!(f, "expired"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// ACME certificate metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertificateMetadata {
    /// 证书域名。
    pub domain: String,
    /// 证书状态。
    pub status: CertificateStatus,
    /// 签发时间（RFC3339）。
    pub issued_at: Option<String>,
    /// 过期时间（RFC3339）。
    pub expires_at: Option<String>,
    /// 是否自动续签。
    pub auto_renew: bool,
    /// 失败原因，无失败为 None。
    pub error: Option<String>,
}

/// ACME state shared across the application
#[derive(Clone)]
pub struct AcmeState {
    /// Active challenges (token -> authorization)
    pub challenges: Arc<RwLock<HashMap<String, String>>>,
    /// Certificate cache (domain -> cert_pem)
    pub certificates: Arc<RwLock<HashMap<String, String>>>,
    /// Database reference
    db: Option<Database>,
}

impl AcmeState {
    /// Create a new ACME state without database
    #[must_use]
    pub fn new() -> Self {
        Self {
            challenges: Arc::new(RwLock::new(HashMap::new())),
            certificates: Arc::new(RwLock::new(HashMap::new())),
            db: None,
        }
    }

    /// Create a new ACME state with database
    #[must_use]
    pub fn with_db(db: Database) -> Self {
        Self {
            challenges: Arc::new(RwLock::new(HashMap::new())),
            certificates: Arc::new(RwLock::new(HashMap::new())),
            db: Some(db),
        }
    }

    /// Add a challenge
    pub async fn add_challenge(&self, token: String, authorization: String) {
        let mut challenges = self.challenges.write().await;
        challenges.insert(token, authorization);
    }

    /// Get a challenge authorization
    pub async fn get_challenge(&self, token: &str) -> Option<String> {
        let challenges = self.challenges.read().await;
        challenges.get(token).cloned()
    }

    /// Remove a challenge
    pub async fn remove_challenge(&self, token: &str) {
        let mut challenges = self.challenges.write().await;
        challenges.remove(token);
    }

    /// Get database reference
    #[must_use]
    pub fn db(&self) -> Option<&Database> {
        self.db.as_ref()
    }
}

impl Default for AcmeState {
    fn default() -> Self {
        Self::new()
    }
}
