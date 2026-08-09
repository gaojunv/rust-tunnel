use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::Database;
use crate::ServerConfig;

/// ACME configuration summary for API responses
#[derive(Debug, Clone)]
pub struct AcmeConfigInfo {
    pub enabled: bool,
    pub server_url: String,
    pub cert_dir: String,
}

/// Full ACME configuration for API access
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcmeFullConfig {
    pub enabled: bool,
    pub server_url: String,
    pub email: Option<String>,
    pub cert_dir: String,
    pub auto_renew: bool,
    pub renewal_check_interval: u64,
    pub renewal_days_before_expiry: u64,
    pub tos_agreed: bool,
}

impl Default for AcmeFullConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            email: None,
            cert_dir: "./data/certs".to_string(),
            auto_renew: true,
            renewal_check_interval: 24,
            renewal_days_before_expiry: 30,
            tos_agreed: false,
        }
    }
}

impl AcmeFullConfig {
    /// Resolve `AcmeFullConfig` for the current process.
    ///
    /// If `server_settings.acme_config` in the database contains valid JSON,
    /// return it verbatim — the CLI/TOML `server_config` values are ignored
    /// on this path. This makes DB the runtime source of truth: values set
    /// via `PUT /api/acme/config` (which writes back to DB) survive restart.
    ///
    /// If the row is absent, seed from `server_config` and persist. On this
    /// first-seed path, `tos_agreed` is inferred as `true` when
    /// `acme_certificates` already contains rows — upgrading from a
    /// CLI-only ACME deployment shouldn't force the operator to re-agree.
    ///
    /// If the row is present but malformed, or the DB read fails, run this
    /// process with a fresh seed but leave the DB row untouched — this
    /// preserves any state the operator may want to inspect/repair, and
    /// avoids letting a transient DB error wipe good persistent config.
    ///
    /// All DB failures are logged as warnings and never fatal.
    pub async fn load_or_seed(db: &Database, server_config: &ServerConfig) -> Self {
        // Tracks whether we should persist the seed back to DB.
        // Set to false in branches where the DB may hold state we
        // shouldn't overwrite:
        // - malformed row: preserve it so an operator can inspect/repair
        // - transient read failure: a valid row may still exist; don't
        //   clobber it with the CLI seed
        //
        // Set to true (default) only when we're confident the row is
        // absent (Ok(None)).
        let mut should_persist_seed = true;

        match db.load_server_setting("acme_config").await {
            Ok(Some(json)) => match serde_json::from_str::<Self>(&json) {
                Ok(cfg) => return cfg,
                Err(e) => {
                    warn!(
                            "acme_config in DB is malformed ({}), re-seeding from CLI/TOML without overwriting the bad row",
                            e
                        );
                    should_persist_seed = false;
                }
            },
            Ok(None) => {
                // Normal seed path — should_persist_seed stays true
            }
            Err(e) => {
                warn!(
                    "Failed to load ACME config from DB ({}), falling back to CLI/TOML seed for this process; leaving DB alone until the next successful startup",
                    e
                );
                should_persist_seed = false;
            }
        }

        let mut seed = Self {
            enabled: server_config.acme_enabled,
            server_url: server_config.acme_server_url.clone(),
            email: server_config.acme_email.clone(),
            cert_dir: server_config.acme_cert_dir.clone(),
            auto_renew: server_config.acme_auto_renew,
            renewal_check_interval: server_config.acme_renewal_check_interval,
            renewal_days_before_expiry: server_config.acme_renewal_days_before_expiry,
            tos_agreed: false,
        };

        // Legacy DB path: if certificates already exist in this DB, the
        // operator must have agreed to ToS in a previous version —
        // otherwise those certs couldn't have been issued. Carry that
        // state forward so they don't get re-prompted after upgrade.
        // Only runs on first seed (should_persist_seed == true); the
        // DB-has-value fast path returns before we get here.
        if should_persist_seed && !seed.tos_agreed {
            match db.load_acme_certificates().await {
                Ok(records) if !records.is_empty() => {
                    info!(
                        "Inferred tos_agreed=true from {} existing certificates",
                        records.len()
                    );
                    seed.tos_agreed = true;
                }
                Ok(_) => {
                    // No certs → keep tos_agreed=false
                }
                Err(e) => {
                    warn!(
                        "Failed to query acme_certificates for ToS inference: {}. \
                         Keeping tos_agreed=false; user can re-agree via UI.",
                        e
                    );
                }
            }
        }

        if should_persist_seed {
            match serde_json::to_string(&seed) {
                Ok(json) => {
                    if let Err(e) = db.save_server_setting("acme_config", &json).await {
                        warn!("Failed to persist ACME seed config to DB: {}", e);
                    }
                }
                Err(e) => {
                    warn!("Failed to serialize ACME seed config: {}", e);
                }
            }
        }

        seed
    }
}
