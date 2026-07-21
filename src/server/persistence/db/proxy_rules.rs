use chrono::{DateTime, Utc};

use super::records::ProxyRuleRecord;
use super::Database;

impl Database {
    // ============================================================
    // Reverse Proxy methods
    // ============================================================

    /// Save or update a proxy rule
    #[allow(clippy::too_many_arguments)]
    pub async fn save_proxy_rule(
        &self,
        id: &str,
        name: &str,
        rule_type: &str,
        listen_addr: &str,
        domains: Option<&str>,
        routes: Option<&str>,
        tls_enabled: bool,
        tls_acme: bool,
        tls_domain: Option<&str>,
        enabled: bool,
        cert_source: Option<&str>,
        cert_covering_domain: Option<&str>,
        cert_status_updated_at: Option<&DateTime<Utc>>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO proxy_rules (id, name, type, listen_addr, domains, routes,
                tls_enabled, tls_acme, tls_domain, enabled, created_at, updated_at,
                cert_source, cert_covering_domain, cert_status_updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                type = excluded.type,
                listen_addr = excluded.listen_addr,
                domains = excluded.domains,
                routes = excluded.routes,
                tls_enabled = excluded.tls_enabled,
                tls_acme = excluded.tls_acme,
                tls_domain = excluded.tls_domain,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at,
                cert_source = excluded.cert_source,
                cert_covering_domain = excluded.cert_covering_domain,
                cert_status_updated_at = excluded.cert_status_updated_at
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(rule_type)
        .bind(listen_addr)
        .bind(domains)
        .bind(routes)
        .bind(tls_enabled as i32)
        .bind(tls_acme as i32)
        .bind(tls_domain)
        .bind(enabled as i32)
        .bind(now)
        .bind(now)
        .bind(cert_source)
        .bind(cert_covering_domain)
        .bind(cert_status_updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all proxy rules
    pub async fn load_proxy_rules(&self) -> Result<Vec<ProxyRuleRecord>, sqlx::Error> {
        sqlx::query_as::<_, ProxyRuleRecord>(
            r#"
            SELECT id, name, type, listen_addr, domains, routes,
                   tls_enabled, tls_acme, tls_domain, enabled, created_at, updated_at,
                   cert_source, cert_covering_domain, cert_status_updated_at
            FROM proxy_rules
            ORDER BY created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Load enabled proxy rules
    pub async fn load_enabled_proxy_rules(&self) -> Result<Vec<ProxyRuleRecord>, sqlx::Error> {
        sqlx::query_as::<_, ProxyRuleRecord>(
            r#"
            SELECT id, name, type, listen_addr, domains, routes,
                   tls_enabled, tls_acme, tls_domain, enabled, created_at, updated_at,
                   cert_source, cert_covering_domain, cert_status_updated_at
            FROM proxy_rules
            WHERE enabled = 1
            ORDER BY created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Get a proxy rule by ID
    pub async fn get_proxy_rule(&self, id: &str) -> Result<Option<ProxyRuleRecord>, sqlx::Error> {
        sqlx::query_as::<_, ProxyRuleRecord>(
            r#"
            SELECT id, name, type, listen_addr, domains, routes,
                   tls_enabled, tls_acme, tls_domain, enabled, created_at, updated_at,
                   cert_source, cert_covering_domain, cert_status_updated_at
            FROM proxy_rules
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Delete a proxy rule
    pub async fn delete_proxy_rule(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM proxy_rules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}