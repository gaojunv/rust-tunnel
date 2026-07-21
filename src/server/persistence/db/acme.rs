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
            r#"
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
            "#,
        )
        .bind(domain)
        .bind(status)
        .bind(cert_pem)
        .bind(key_pem)
        .bind(chain_pem)
        .bind(issued_at)
        .bind(expires_at)
        .bind(auto_renew as i32)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all ACME certificates
    pub async fn load_acme_certificates(&self) -> Result<Vec<AcmeCertificateRecord>, sqlx::Error> {
        sqlx::query_as::<_, AcmeCertificateRecord>(
            r#"
            SELECT domain, status, cert_pem, key_pem, chain_pem,
                   issued_at, expires_at, auto_renew, last_renewal_attempt,
                   error_message, created_at
            FROM acme_certificates
            ORDER BY created_at
            "#,
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
            r#"
            SELECT domain, status, cert_pem, key_pem, chain_pem,
                   issued_at, expires_at, auto_renew, last_renewal_attempt,
                   error_message, created_at
            FROM acme_certificates
            WHERE domain = ?
            "#,
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
            r#"
            UPDATE acme_certificates
            SET status = ?, error_message = ?
            WHERE domain = ?
            "#,
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
            r#"
            UPDATE acme_certificates
            SET last_renewal_attempt = ?
            WHERE domain = ?
            "#,
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
            r#"
            INSERT INTO acme_challenges (token, domain, authorization, status, created_at, expires_at)
            VALUES (?, ?, ?, 'pending', ?, ?)
            ON CONFLICT(token) DO UPDATE SET
                domain = excluded.domain,
                authorization = excluded.authorization,
                status = 'pending',
                expires_at = excluded.expires_at
            "#,
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
            r#"
            SELECT token, domain, authorization, status, created_at, expires_at
            FROM acme_challenges
            WHERE token = ?
            "#,
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
            r#"
            UPDATE acme_challenges
            SET status = ?
            WHERE token = ?
            "#,
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
            r#"
            DELETE FROM acme_challenges
            WHERE expires_at IS NOT NULL AND expires_at < ?
            "#,
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
            r#"
            SELECT domain, status, cert_pem, key_pem, chain_pem,
                   issued_at, expires_at, auto_renew, last_renewal_attempt,
                   error_message, created_at
            FROM acme_certificates
            WHERE status = 'active'
              AND auto_renew = 1
              AND expires_at IS NOT NULL
              AND expires_at <= ?
            "#,
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
    }
}