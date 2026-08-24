use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::Database;
use crate::config::ServerConfig;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::test_helpers::in_memory_db;

    #[tokio::test]
    async fn default_seed_on_empty_db_matches_server_config_defaults() {
        let db = in_memory_db().await;
        let cfg = ServerConfig::default();
        let out = AcmeFullConfig::load_or_seed(&db, &cfg).await;

        let expected = AcmeFullConfig::default();
        assert!(!out.enabled);
        assert_eq!(out.server_url, expected.server_url);
        assert_eq!(out.email, expected.email);
        assert_eq!(out.cert_dir, expected.cert_dir);
        assert_eq!(out.auto_renew, expected.auto_renew);
        assert_eq!(out.renewal_check_interval, expected.renewal_check_interval);
        assert_eq!(
            out.renewal_days_before_expiry,
            expected.renewal_days_before_expiry
        );
        assert!(!out.tos_agreed);

        // 种子已持久化到 DB
        let json = db
            .load_server_setting("acme_config")
            .await
            .unwrap()
            .expect("acme_config row should exist after seed");
        let stored: AcmeFullConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(stored.server_url, expected.server_url);
        assert_eq!(stored.cert_dir, expected.cert_dir);
        assert!(!stored.tos_agreed);
    }

    #[tokio::test]
    async fn non_default_config_is_preserved_and_db_is_source_of_truth() {
        let db = in_memory_db().await;

        let first_cfg = ServerConfig {
            acme_enabled: true,
            acme_server_url: "https://custom.example/acme".to_string(),
            acme_email: Some("ops@example.test".to_string()),
            acme_cert_dir: "/tmp/custom-certs".to_string(),
            acme_auto_renew: false,
            acme_renewal_check_interval: 12,
            acme_renewal_days_before_expiry: 7,
            ..Default::default()
        };
        let first = AcmeFullConfig::load_or_seed(&db, &first_cfg).await;
        assert!(first.enabled);
        assert_eq!(first.server_url, "https://custom.example/acme");
        assert_eq!(first.email.as_deref(), Some("ops@example.test"));
        assert_eq!(first.cert_dir, "/tmp/custom-certs");
        assert!(!first.auto_renew);
        assert_eq!(first.renewal_check_interval, 12);
        assert_eq!(first.renewal_days_before_expiry, 7);

        // 第二次调用传入完全不同的 ServerConfig，DB 已有值时应被忽略
        let second_cfg = ServerConfig {
            acme_enabled: false,
            acme_server_url: "https://other.example/acme".to_string(),
            acme_email: Some("other@example.test".to_string()),
            acme_cert_dir: "/other".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 99,
            acme_renewal_days_before_expiry: 99,
            ..Default::default()
        };
        let second = AcmeFullConfig::load_or_seed(&db, &second_cfg).await;
        assert_eq!(second.server_url, first.server_url);
        assert_eq!(second.cert_dir, first.cert_dir);
        assert_eq!(second.email, first.email);
        assert_eq!(second.auto_renew, first.auto_renew);
        assert_eq!(second.renewal_check_interval, first.renewal_check_interval);
        assert_eq!(
            second.renewal_days_before_expiry,
            first.renewal_days_before_expiry
        );
        assert_eq!(second.enabled, first.enabled);
    }

    #[tokio::test]
    async fn roundtrip_seed_to_reload_is_identical() {
        let db = in_memory_db().await;

        let cfg = ServerConfig {
            acme_enabled: true,
            acme_server_url: "https://roundtrip.example/acme".to_string(),
            acme_email: Some("rt@example.test".to_string()),
            acme_cert_dir: "/tmp/rt-certs".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 6,
            acme_renewal_days_before_expiry: 20,
            ..Default::default()
        };

        let seeded = AcmeFullConfig::load_or_seed(&db, &cfg).await;

        // DB 中的 JSON 与种子返回值一致
        let json = db
            .load_server_setting("acme_config")
            .await
            .unwrap()
            .unwrap();
        let from_db: AcmeFullConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(from_db.server_url, seeded.server_url);
        assert_eq!(from_db.cert_dir, seeded.cert_dir);
        assert_eq!(from_db.email, seeded.email);
        assert_eq!(from_db.auto_renew, seeded.auto_renew);
        assert_eq!(
            from_db.renewal_check_interval,
            seeded.renewal_check_interval
        );
        assert_eq!(
            from_db.renewal_days_before_expiry,
            seeded.renewal_days_before_expiry
        );
        assert_eq!(from_db.tos_agreed, seeded.tos_agreed);
        assert_eq!(from_db.enabled, seeded.enabled);

        // 再次 load_or_seed 走 DB 命中分支，返回值与首次种子完全一致
        let reloaded = AcmeFullConfig::load_or_seed(&db, &ServerConfig::default()).await;
        assert_eq!(reloaded.server_url, seeded.server_url);
        assert_eq!(reloaded.cert_dir, seeded.cert_dir);
        assert_eq!(reloaded.email, seeded.email);
        assert_eq!(reloaded.enabled, seeded.enabled);
        assert_eq!(reloaded.auto_renew, seeded.auto_renew);
        assert_eq!(
            reloaded.renewal_check_interval,
            seeded.renewal_check_interval
        );
        assert_eq!(
            reloaded.renewal_days_before_expiry,
            seeded.renewal_days_before_expiry
        );
        assert_eq!(reloaded.tos_agreed, seeded.tos_agreed);
    }
}
