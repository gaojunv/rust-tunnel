use super::records::DnsConfigRecord;
use super::Database;

impl Database {
    /// Load DNS config (singleton)
    pub async fn load_dns_config(&self) -> Result<Option<DnsConfigRecord>, sqlx::Error> {
        sqlx::query_as::<_, DnsConfigRecord>(
            "SELECT tunnel_domain, mesh_domain FROM dns_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Save DNS config (singleton, upsert)
    pub async fn save_dns_config(
        &self,
        tunnel_domain: &str,
        mesh_domain: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            INSERT INTO dns_config (id, tunnel_domain, mesh_domain, updated_at)
            VALUES (1, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                tunnel_domain = excluded.tunnel_domain,
                mesh_domain = excluded.mesh_domain,
                updated_at = excluded.updated_at
            ",
        )
        .bind(tunnel_domain)
        .bind(mesh_domain)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
