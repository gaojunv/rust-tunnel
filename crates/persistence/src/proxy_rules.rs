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

#[cfg(test)]
mod tests {
    use crate::Database;
    use chrono::Utc;

    async fn in_memory_db() -> Database {
        Database::new(":memory:").await.expect("in-memory db")
    }

    #[tokio::test]
    async fn test_crud_full_chain() {
        let db = in_memory_db().await;

        // 初始空
        assert!(db.load_proxy_rules().await.unwrap().is_empty());
        assert!(db.load_enabled_proxy_rules().await.unwrap().is_empty());
        assert!(db.get_proxy_rule("r1").await.unwrap().is_none());

        // save
        db.save_proxy_rule(
            "r1",
            "web",
            "http",
            "0.0.0.0:80",
            None,
            None,
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // get
        let got = db.get_proxy_rule("r1").await.unwrap().unwrap();
        assert_eq!(got.id, "r1");
        assert_eq!(got.name, "web");
        assert_eq!(got.rule_type, "http");
        assert_eq!(got.listen_addr, "0.0.0.0:80");

        // load
        let all = db.load_proxy_rules().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "r1");

        // load_enabled
        let enabled = db.load_enabled_proxy_rules().await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, "r1");

        // delete
        db.delete_proxy_rule("r1").await.unwrap();
        assert!(db.get_proxy_rule("r1").await.unwrap().is_none());
        assert!(db.load_proxy_rules().await.unwrap().is_empty());
        assert!(db.load_enabled_proxy_rules().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_enabled_disabled_filter() {
        let db = in_memory_db().await;

        db.save_proxy_rule(
            "r1",
            "a",
            "http",
            "0.0.0.0:80",
            None,
            None,
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.save_proxy_rule(
            "r2",
            "b",
            "tcp",
            "0.0.0.0:9000",
            None,
            None,
            false,
            false,
            None,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.save_proxy_rule(
            "r3",
            "c",
            "http",
            "0.0.0.0:8080",
            None,
            None,
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let all = db.load_proxy_rules().await.unwrap();
        assert_eq!(all.len(), 3);

        let enabled = db.load_enabled_proxy_rules().await.unwrap();
        assert_eq!(enabled.len(), 2);
        assert!(enabled.iter().all(|r| r.enabled == 1));
        assert!(enabled.iter().any(|r| r.id == "r1"));
        assert!(enabled.iter().any(|r| r.id == "r3"));
        assert!(!enabled.iter().any(|r| r.id == "r2"));

        // disabled 仍可 get，但不在 enabled 列表
        let disabled = db.get_proxy_rule("r2").await.unwrap().unwrap();
        assert_eq!(disabled.enabled, 0);
    }

    #[tokio::test]
    async fn test_roundtrip_all_fields() {
        let db = in_memory_db().await;
        let now = Utc::now();

        db.save_proxy_rule(
            "r-full",
            "full-rule",
            "http",
            "0.0.0.0:443",
            Some(r#"["example.com","www.example.com"]"#),
            Some(r#"[{"path":"/","backends":[{"kind":"direct","addr":"127.0.0.1:3000"}]}]"#),
            true,
            true,
            Some("example.com"),
            true,
            Some("acme"),
            Some("example.com"),
            Some(&now),
        )
        .await
        .unwrap();

        let r = db.get_proxy_rule("r-full").await.unwrap().unwrap();
        assert_eq!(r.id, "r-full");
        assert_eq!(r.name, "full-rule");
        assert_eq!(r.rule_type, "http");
        assert_eq!(r.listen_addr, "0.0.0.0:443");
        assert_eq!(
            r.domains.as_deref(),
            Some(r#"["example.com","www.example.com"]"#)
        );
        assert_eq!(
            r.routes.as_deref(),
            Some(r#"[{"path":"/","backends":[{"kind":"direct","addr":"127.0.0.1:3000"}]}]"#)
        );
        assert_eq!(r.tls_enabled, 1);
        assert_eq!(r.tls_acme, 1);
        assert_eq!(r.tls_domain.as_deref(), Some("example.com"));
        assert_eq!(r.enabled, 1);
        assert_eq!(r.cert_source.as_deref(), Some("acme"));
        assert_eq!(r.cert_covering_domain.as_deref(), Some("example.com"));
        assert!(r.cert_status_updated_at.is_some());
        // 时间回环误差容忍 5s（SQLite 文本精度）
        let delta = (r.cert_status_updated_at.unwrap() - now)
            .num_seconds()
            .abs();
        assert!(delta < 5, "cert_status_updated_at delta {delta}s too large");

        // load 也应完整回环
        let all = db.load_proxy_rules().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "r-full");
        assert_eq!(all[0].cert_source.as_deref(), Some("acme"));

        let enabled = db.load_enabled_proxy_rules().await.unwrap();
        assert_eq!(enabled.len(), 1);
    }

    #[tokio::test]
    async fn test_roundtrip_nullable_fields() {
        let db = in_memory_db().await;

        db.save_proxy_rule(
            "r-min",
            "minimal",
            "tcp",
            "0.0.0.0:9000",
            None,
            None,
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let r = db.get_proxy_rule("r-min").await.unwrap().unwrap();
        assert_eq!(r.id, "r-min");
        assert_eq!(r.name, "minimal");
        assert_eq!(r.rule_type, "tcp");
        assert_eq!(r.listen_addr, "0.0.0.0:9000");
        assert_eq!(r.domains, None);
        assert_eq!(r.routes, None);
        assert_eq!(r.tls_enabled, 0);
        assert_eq!(r.tls_acme, 0);
        assert_eq!(r.tls_domain, None);
        assert_eq!(r.enabled, 1);
        assert_eq!(r.cert_source, None);
        assert_eq!(r.cert_covering_domain, None);
        assert_eq!(r.cert_status_updated_at, None);
    }

    #[tokio::test]
    async fn test_upsert_updates_existing_rule() {
        let db = in_memory_db().await;

        db.save_proxy_rule(
            "r1",
            "v1",
            "http",
            "0.0.0.0:80",
            Some(r#"["a.com"]"#),
            None,
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let first = db.get_proxy_rule("r1").await.unwrap().unwrap();
        assert_eq!(first.name, "v1");

        // 轻微延迟保证 updated_at 可区分（同一毫秒也接受 >=）
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let now = Utc::now();
        db.save_proxy_rule(
            "r1",
            "v2-updated",
            "http",
            "0.0.0.0:8080",
            Some(r#"["b.com"]"#),
            Some(r#"[]"#),
            true,
            true,
            Some("b.com"),
            false,
            Some("exact"),
            Some("b.com"),
            Some(&now),
        )
        .await
        .unwrap();

        let r = db.get_proxy_rule("r1").await.unwrap().unwrap();
        assert_eq!(r.name, "v2-updated");
        assert_eq!(r.listen_addr, "0.0.0.0:8080");
        assert_eq!(r.domains.as_deref(), Some(r#"["b.com"]"#));
        assert_eq!(r.routes.as_deref(), Some(r#"[]"#));
        assert_eq!(r.tls_enabled, 1);
        assert_eq!(r.tls_acme, 1);
        assert_eq!(r.tls_domain.as_deref(), Some("b.com"));
        assert_eq!(r.enabled, 0);
        assert_eq!(r.cert_source.as_deref(), Some("exact"));
        assert_eq!(r.cert_covering_domain.as_deref(), Some("b.com"));
        assert!(r.updated_at >= r.created_at);

        // upsert 不应产生重复行
        let all = db.load_proxy_rules().await.unwrap();
        assert_eq!(all.len(), 1);

        // 已禁用，不在 enabled 列表
        assert!(db.load_enabled_proxy_rules().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_get_nonexistent_and_delete_idempotent() {
        let db = in_memory_db().await;

        assert!(db.get_proxy_rule("nope").await.unwrap().is_none());
        // 删除不存在的 id 不应报错
        db.delete_proxy_rule("nope").await.unwrap();

        db.save_proxy_rule(
            "r1",
            "x",
            "http",
            "0.0.0.0:80",
            None,
            None,
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.delete_proxy_rule("r1").await.unwrap();
        // 重复删除幂等
        db.delete_proxy_rule("r1").await.unwrap();
        assert!(db.get_proxy_rule("r1").await.unwrap().is_none());
        assert!(db.load_proxy_rules().await.unwrap().is_empty());
    }
}
