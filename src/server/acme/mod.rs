pub mod challenge;
pub mod client;
pub mod dns;
pub mod manager;
pub mod provider;
pub mod storage;

pub use manager::{CertEvent, CertificateManager};
pub use provider::{CertCoverage, CertEntry, CertSource, CertificateProvider};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::server::db::Database;

/// ACME certificate status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CertificateStatus {
    Pending,
    Active,
    Expired,
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
    pub domain: String,
    pub status: CertificateStatus,
    pub issued_at: Option<String>,
    pub expires_at: Option<String>,
    pub auto_renew: bool,
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
    pub fn new() -> Self {
        Self {
            challenges: Arc::new(RwLock::new(HashMap::new())),
            certificates: Arc::new(RwLock::new(HashMap::new())),
            db: None,
        }
    }

    /// Create a new ACME state with database
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
    pub fn db(&self) -> Option<&Database> {
        self.db.as_ref()
    }
}

impl Default for AcmeState {
    fn default() -> Self {
        Self::new()
    }
}
