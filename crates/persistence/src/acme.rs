use chrono::{DateTime, Utc};

use super::records::{AcmeCertificateRecord, AcmeChallengeRecord};
use super::Database;

impl Database {
    // ============================================================
    // ACME Certificate methods
    // ============================================================

    /// Save or update an ACME certificate
    #[allow(clippy::too_many_arguments)]
    pub async fn save_acme_certificate(
        &self,
        domain: &str,
        status: &str,
        cert_pem: Option<&str>,
        key_pem: Option<&str>,
        chain_pem: Option<&str>,
        issued_at: Option<DateTime<Utc>>,
        expires_at: Option<DateTime<Utc>>,
        auto_renew: bool,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r"
            INSERT INTO acme_certificates (domain, status, cert_pem, key_pem, chain_pem,
                issued_at, expires_at, auto_renew, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(domain) DO UPDATE SET
                status = excluded.status,
                cert_pem = excluded.cert_pem,
                key_pem = excluded.key_pem,
                chain_pem = excluded.chain_pem,
                issued_at = excluded.issued_at,
                expires_at = excluded.expires_at,
                auto_renew = excluded.auto_renew
            ",
        )
        .bind(domain)
        .bind(status)
        .bind(cert_pem)
        .bind(key_pem)
        .bind(chain_pem)
        .bind(issued_at)
        .bind(expires_at)
        .bind(i32::from(auto_renew))
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all ACME certificates
    pub async fn load_acme_certificates(&self) -> Result<Vec<AcmeCertificateRecord>, sqlx::Error> {
        sqlx::query_as::<_, AcmeCertificateRecord>(
            r"
            SELECT domain, status, cert_pem, key_pem, chain_pem,
                   issued_at, expires_at, auto_renew, last_renewal_attempt,
                   error_message, created_at
            FROM acme_certificates
            ORDER BY created_at
            ",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Get an ACME certificate by domain
    pub async fn get_acme_certificate(
        &self,
        domain: &str,
    ) -> Result<Option<AcmeCertificateRecord>, sqlx::Error> {
        sqlx::query_as::<_, AcmeCertificateRecord>(
            r"
            SELECT domain, status, cert_pem, key_pem, chain_pem,
                   issued_at, expires_at, auto_renew, last_renewal_attempt,
                   error_message, created_at
            FROM acme_certificates
            WHERE domain = ?
            ",
        )
        .bind(domain)
        .fetch_optional(&self.pool)
        .await
    }

    /// Update ACME certificate status
    pub async fn update_acme_certificate_status(
        &self,
        domain: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            UPDATE acme_certificates
            SET status = ?, error_message = ?
            WHERE domain = ?
            ",
        )
        .bind(status)
        .bind(error_message)
        .bind(domain)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update ACME certificate renewal attempt
    pub async fn update_acme_certificate_renewal_attempt(
        &self,
        domain: &str,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r"
            UPDATE acme_certificates
            SET last_renewal_attempt = ?
            WHERE domain = ?
            ",
        )
        .bind(now)
        .bind(domain)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete an ACME certificate
    pub async fn delete_acme_certificate(&self, domain: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM acme_certificates WHERE domain = ?")
            .bind(domain)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Save an ACME challenge
    pub async fn save_acme_challenge(
        &self,
        token: &str,
        domain: &str,
        authorization: &str,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r"
            INSERT INTO acme_challenges (token, domain, authorization, status, created_at, expires_at)
            VALUES (?, ?, ?, 'pending', ?, ?)
            ON CONFLICT(token) DO UPDATE SET
                domain = excluded.domain,
                authorization = excluded.authorization,
                status = 'pending',
                expires_at = excluded.expires_at
            ",
        )
        .bind(token)
        .bind(domain)
        .bind(authorization)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get an ACME challenge by token
    pub async fn get_acme_challenge(
        &self,
        token: &str,
    ) -> Result<Option<AcmeChallengeRecord>, sqlx::Error> {
        sqlx::query_as::<_, AcmeChallengeRecord>(
            r"
            SELECT token, domain, authorization, status, created_at, expires_at
            FROM acme_challenges
            WHERE token = ?
            ",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await
    }

    /// Update ACME challenge status
    pub async fn update_acme_challenge_status(
        &self,
        token: &str,
        status: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            UPDATE acme_challenges
            SET status = ?
            WHERE token = ?
            ",
        )
        .bind(status)
        .bind(token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete expired ACME challenges
    pub async fn cleanup_expired_acme_challenges(&self) -> Result<u64, sqlx::Error> {
        let now = Utc::now();
        let result = sqlx::query(
            r"
            DELETE FROM acme_challenges
            WHERE expires_at IS NOT NULL AND expires_at < ?
            ",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Delete an ACME challenge
    pub async fn delete_acme_challenge(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM acme_challenges WHERE token = ?")
            .bind(token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load ACME certificates that need renewal
    pub async fn load_acme_certificates_needing_renewal(
        &self,
        days_before_expiry: i64,
    ) -> Result<Vec<AcmeCertificateRecord>, sqlx::Error> {
        let cutoff = Utc::now() + chrono::Duration::days(days_before_expiry);
        sqlx::query_as::<_, AcmeCertificateRecord>(
            r"
            SELECT domain, status, cert_pem, key_pem, chain_pem,
                   issued_at, expires_at, auto_renew, last_renewal_attempt,
                   error_message, created_at
            FROM acme_certificates
            WHERE status = 'active'
              AND auto_renew = 1
              AND expires_at IS NOT NULL
              AND expires_at <= ?
            ",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn in_memory_db() -> Database {
        Database::new(":memory:").await.expect("in-memory db")
    }

    #[tokio::test]
    async fn certificate_crud_save_get_update_delete() {
        let db = in_memory_db().await;

        // initially empty
        assert!(db.load_acme_certificates().await.unwrap().is_empty());
        assert!(db
            .get_acme_certificate("example.com")
            .await
            .unwrap()
            .is_none());

        let issued = Utc::now() - Duration::days(10);
        let expires = Utc::now() + Duration::days(80);

        // save → get
        db.save_acme_certificate(
            "example.com",
            "pending",
            Some("cert-pem"),
            Some("key-pem"),
            Some("chain-pem"),
            Some(issued),
            Some(expires),
            true,
        )
        .await
        .unwrap();

        let rec = db
            .get_acme_certificate("example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.domain, "example.com");
        assert_eq!(rec.status, "pending");
        assert_eq!(rec.cert_pem.as_deref(), Some("cert-pem"));
        assert_eq!(rec.key_pem.as_deref(), Some("key-pem"));
        assert_eq!(rec.chain_pem.as_deref(), Some("chain-pem"));
        assert_eq!(rec.auto_renew, 1);
        assert!(rec.error_message.is_none());
        assert!(rec.last_renewal_attempt.is_none());

        // load_all returns one, ordered by created_at
        let all = db.load_acme_certificates().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].domain, "example.com");

        // upsert: same domain overwrites via ON CONFLICT
        let new_expires = Utc::now() + Duration::days(90);
        db.save_acme_certificate(
            "example.com",
            "active",
            Some("cert-pem-v2"),
            Some("key-pem-v2"),
            None,
            Some(issued),
            Some(new_expires),
            false,
        )
        .await
        .unwrap();
        let rec2 = db
            .get_acme_certificate("example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec2.status, "active");
        assert_eq!(rec2.cert_pem.as_deref(), Some("cert-pem-v2"));
        assert_eq!(rec2.chain_pem, None);
        assert_eq!(rec2.auto_renew, 0);

        // update status + error_message
        db.update_acme_certificate_status("example.com", "failed", Some("dns timeout"))
            .await
            .unwrap();
        let rec3 = db
            .get_acme_certificate("example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec3.status, "failed");
        assert_eq!(rec3.error_message.as_deref(), Some("dns timeout"));

        // clear error_message
        db.update_acme_certificate_status("example.com", "active", None)
            .await
            .unwrap();
        let rec4 = db
            .get_acme_certificate("example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec4.status, "active");
        assert!(rec4.error_message.is_none());

        // update renewal attempt sets timestamp
        assert!(rec4.last_renewal_attempt.is_none());
        db.update_acme_certificate_renewal_attempt("example.com")
            .await
            .unwrap();
        let rec5 = db
            .get_acme_certificate("example.com")
            .await
            .unwrap()
            .unwrap();
        assert!(rec5.last_renewal_attempt.is_some());

        // delete
        db.delete_acme_certificate("example.com").await.unwrap();
        assert!(db
            .get_acme_certificate("example.com")
            .await
            .unwrap()
            .is_none());
        assert!(db.load_acme_certificates().await.unwrap().is_empty());

        // delete non-existent is no-op
        db.delete_acme_certificate("no-such.example.com")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn certificate_save_with_null_fields_and_load_order() {
        let db = in_memory_db().await;

        db.save_acme_certificate(
            "a.example.com",
            "pending",
            None,
            None,
            None,
            None,
            None,
            true,
        )
        .await
        .unwrap();
        // small delay to ensure created_at ordering is deterministic
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        db.save_acme_certificate(
            "b.example.com",
            "pending",
            None,
            None,
            None,
            None,
            None,
            true,
        )
        .await
        .unwrap();

        let all = db.load_acme_certificates().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].domain, "a.example.com");
        assert_eq!(all[1].domain, "b.example.com");
        for r in &all {
            assert!(r.cert_pem.is_none());
            assert!(r.expires_at.is_none());
        }
    }

    #[tokio::test]
    async fn challenge_crud_save_get_update_delete() {
        let db = in_memory_db().await;

        assert!(db
            .get_acme_challenge("tok-missing")
            .await
            .unwrap()
            .is_none());

        let expires = Utc::now() + Duration::hours(1);
        db.save_acme_challenge("tok-1", "example.com", "auth-1", Some(expires))
            .await
            .unwrap();

        let ch = db.get_acme_challenge("tok-1").await.unwrap().unwrap();
        assert_eq!(ch.token, "tok-1");
        assert_eq!(ch.domain, "example.com");
        assert_eq!(ch.authorization, "auth-1");
        assert_eq!(ch.status, "pending");

        // upsert same token resets status to pending and updates fields
        db.update_acme_challenge_status("tok-1", "valid")
            .await
            .unwrap();
        assert_eq!(
            db.get_acme_challenge("tok-1")
                .await
                .unwrap()
                .unwrap()
                .status,
            "valid"
        );
        // re-save same token should reset status to pending
        db.save_acme_challenge("tok-1", "other.example.com", "auth-2", None)
            .await
            .unwrap();
        let ch2 = db.get_acme_challenge("tok-1").await.unwrap().unwrap();
        assert_eq!(ch2.domain, "other.example.com");
        assert_eq!(ch2.authorization, "auth-2");
        assert_eq!(ch2.status, "pending");
        assert!(ch2.expires_at.is_none());

        // update status to invalid
        db.update_acme_challenge_status("tok-1", "invalid")
            .await
            .unwrap();
        assert_eq!(
            db.get_acme_challenge("tok-1")
                .await
                .unwrap()
                .unwrap()
                .status,
            "invalid"
        );

        // save with no expiry
        db.save_acme_challenge("tok-2", "example.com", "auth-x", None)
            .await
            .unwrap();
        assert!(db.get_acme_challenge("tok-2").await.unwrap().is_some());

        // delete one
        db.delete_acme_challenge("tok-1").await.unwrap();
        assert!(db.get_acme_challenge("tok-1").await.unwrap().is_none());
        assert!(db.get_acme_challenge("tok-2").await.unwrap().is_some());

        // delete non-existent is no-op
        db.delete_acme_challenge("tok-missing").await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_expired_challenges_removes_only_expired() {
        let db = in_memory_db().await;

        let past = Utc::now() - Duration::hours(2);
        let future = Utc::now() + Duration::hours(2);

        db.save_acme_challenge("tok-expired", "a.example.com", "auth-a", Some(past))
            .await
            .unwrap();
        db.save_acme_challenge("tok-future", "b.example.com", "auth-b", Some(future))
            .await
            .unwrap();
        db.save_acme_challenge("tok-no-expiry", "c.example.com", "auth-c", None)
            .await
            .unwrap();

        let removed = db.cleanup_expired_acme_challenges().await.unwrap();
        assert_eq!(removed, 1);

        assert!(db
            .get_acme_challenge("tok-expired")
            .await
            .unwrap()
            .is_none());
        assert!(db.get_acme_challenge("tok-future").await.unwrap().is_some());
        assert!(db
            .get_acme_challenge("tok-no-expiry")
            .await
            .unwrap()
            .is_some());

        // second cleanup removes nothing
        let removed2 = db.cleanup_expired_acme_challenges().await.unwrap();
        assert_eq!(removed2, 0);

        // empty table cleanup also returns 0
        db.delete_acme_challenge("tok-future").await.unwrap();
        db.delete_acme_challenge("tok-no-expiry").await.unwrap();
        assert_eq!(db.cleanup_expired_acme_challenges().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn load_certificates_needing_renewal_filters_correctly() {
        let db = in_memory_db().await;

        let now = Utc::now();

        // should be returned: active, auto_renew=1, expires within 30 days
        db.save_acme_certificate(
            "soon.example.com",
            "active",
            None,
            None,
            None,
            Some(now - Duration::days(60)),
            Some(now + Duration::days(10)),
            true,
        )
        .await
        .unwrap();

        // not returned: active, auto_renew=1, expires far in future
        db.save_acme_certificate(
            "far.example.com",
            "active",
            None,
            None,
            None,
            Some(now - Duration::days(10)),
            Some(now + Duration::days(90)),
            true,
        )
        .await
        .unwrap();

        // not returned: active, auto_renew=0, expires soon (auto_renew disabled)
        db.save_acme_certificate(
            "no-renew.example.com",
            "active",
            None,
            None,
            None,
            Some(now - Duration::days(10)),
            Some(now + Duration::days(5)),
            false,
        )
        .await
        .unwrap();

        // not returned: pending status, even though expires soon
        db.save_acme_certificate(
            "pending.example.com",
            "pending",
            None,
            None,
            None,
            Some(now - Duration::days(10)),
            Some(now + Duration::days(5)),
            true,
        )
        .await
        .unwrap();

        // not returned: active, auto_renew=1, expires_at is NULL
        db.save_acme_certificate(
            "null-expiry.example.com",
            "active",
            None,
            None,
            None,
            None,
            None,
            true,
        )
        .await
        .unwrap();

        // not returned: failed status
        db.save_acme_certificate(
            "failed.example.com",
            "failed",
            None,
            None,
            None,
            Some(now - Duration::days(10)),
            Some(now + Duration::days(5)),
            true,
        )
        .await
        .unwrap();

        // already expired but active+auto_renew → should be returned when window covers it
        db.save_acme_certificate(
            "expired.example.com",
            "active",
            None,
            None,
            None,
            Some(now - Duration::days(90)),
            Some(now - Duration::days(1)),
            true,
        )
        .await
        .unwrap();

        // 30-day window: soon (10d) and expired (-1d) match; far (90d) does not
        let needing = db.load_acme_certificates_needing_renewal(30).await.unwrap();
        let domains: Vec<&str> = needing.iter().map(|r| r.domain.as_str()).collect();
        assert!(
            domains.contains(&"soon.example.com"),
            "domains: {domains:?}"
        );
        assert!(
            domains.contains(&"expired.example.com"),
            "domains: {domains:?}"
        );
        assert_eq!(domains.len(), 2, "unexpected domains: {domains:?}");

        // 5-day window: expired (-1d) still matches (expired <= cutoff), soon (10d) does not
        let needing5 = db.load_acme_certificates_needing_renewal(5).await.unwrap();
        let domains5: Vec<&str> = needing5.iter().map(|r| r.domain.as_str()).collect();
        assert!(
            domains5.contains(&"expired.example.com"),
            "domains5: {domains5:?}"
        );
        assert!(
            !domains5.contains(&"soon.example.com"),
            "domains5: {domains5:?}"
        );

        // 100-day window: soon, far (90d), expired all match
        let needing100 = db
            .load_acme_certificates_needing_renewal(100)
            .await
            .unwrap();
        let domains100: Vec<&str> = needing100.iter().map(|r| r.domain.as_str()).collect();
        assert!(domains100.contains(&"soon.example.com"));
        assert!(domains100.contains(&"far.example.com"));
        assert!(domains100.contains(&"expired.example.com"));
        assert_eq!(domains100.len(), 3, "domains100: {domains100:?}");
    }

    #[tokio::test]
    async fn update_renewal_attempt_and_status_on_missing_domain_is_noop() {
        let db = in_memory_db().await;
        // should not error even though domain does not exist
        db.update_acme_certificate_status("ghost.example.com", "failed", Some("err"))
            .await
            .unwrap();
        db.update_acme_certificate_renewal_attempt("ghost.example.com")
            .await
            .unwrap();
        assert!(db
            .get_acme_certificate("ghost.example.com")
            .await
            .unwrap()
            .is_none());
    }
}
