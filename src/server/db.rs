use chrono::{DateTime, Utc};
use sqlx::Row;

pub use crate::server::persistence::db::records::*;
pub use crate::server::persistence::db::Database;

impl Database {
    
    
    
    /// Save or update Shadowsocks configuration
    pub async fn save_shadowsocks_config(
        &self,
        port: u16,
        cipher: &str,
        password: &str,
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO shadowsocks_config (port, cipher, password, enabled, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(port) DO UPDATE SET
                cipher = excluded.cipher,
                password = excluded.password,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(port as i32)
        .bind(cipher)
        .bind(password)
        .bind(enabled as i32)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Replace the entire Shadowsocks configuration with a single record.
    ///
    /// 运行时只支持单份 SS 配置（API/动态配置均如此），而按端口 upsert 的
    /// `save_shadowsocks_config` 在修改端口时会残留旧行，导致重启后读到旧配置。
    /// 配置更新接口应使用本方法：先清空再插入，保证表中始终只有一份配置。
    pub async fn replace_shadowsocks_config(
        &self,
        port: u16,
        cipher: &str,
        password: &str,
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM shadowsocks_config")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"
            INSERT INTO shadowsocks_config (port, cipher, password, enabled, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(port as i32)
        .bind(cipher)
        .bind(password)
        .bind(enabled as i32)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(())
    }

    /// Load all Shadowsocks configurations
    pub async fn load_shadowsocks_configs(
        &self,
    ) -> Result<Vec<ShadowsocksConfigRecord>, sqlx::Error> {
        let records = sqlx::query_as::<_, ShadowsocksConfigRecord>(
            r#"
            SELECT id, port, cipher, password, enabled, created_at, updated_at
            FROM shadowsocks_config
            ORDER BY port
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Load enabled Shadowsocks configurations
    pub async fn load_enabled_shadowsocks_configs(
        &self,
    ) -> Result<Vec<ShadowsocksConfigRecord>, sqlx::Error> {
        let records = sqlx::query_as::<_, ShadowsocksConfigRecord>(
            r#"
            SELECT id, port, cipher, password, enabled, created_at, updated_at
            FROM shadowsocks_config
            WHERE enabled = 1
            ORDER BY port
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Get Shadowsocks config for a specific port
    pub async fn get_shadowsocks_config(
        &self,
        port: u16,
    ) -> Result<Option<ShadowsocksConfigRecord>, sqlx::Error> {
        let record = sqlx::query_as::<_, ShadowsocksConfigRecord>(
            r#"
            SELECT id, port, cipher, password, enabled, created_at, updated_at
            FROM shadowsocks_config
            WHERE port = ?
            "#,
        )
        .bind(port as i32)
        .fetch_optional(&self.pool)
        .await?;

        Ok(record)
    }

    /// Delete Shadowsocks configuration
    pub async fn delete_shadowsocks_config(&self, port: u16) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            DELETE FROM shadowsocks_config
            WHERE port = ?
            "#,
        )
        .bind(port as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Save or update Trojan configuration
    pub async fn save_trojan_config(
        &self,
        port: u16,
        password: &str,
        fallback: &str,
        enabled: bool,
        domain: &str,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO trojan_config (port, password, fallback, enabled, domain, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(port) DO UPDATE SET
                password = excluded.password,
                fallback = excluded.fallback,
                enabled = excluded.enabled,
                domain = excluded.domain,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(port as i32)
        .bind(password)
        .bind(fallback)
        .bind(enabled as i32)
        .bind(domain)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Replace the entire Trojan configuration with a single record.
    ///
    /// 与 `replace_shadowsocks_config` 同理：修改端口时按端口 upsert 会残留旧行，
    /// 配置更新接口应使用本方法保证表中始终只有一份配置。
    pub async fn replace_trojan_config(
        &self,
        port: u16,
        password: &str,
        fallback: &str,
        enabled: bool,
        domain: &str,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM trojan_config")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"
            INSERT INTO trojan_config (port, password, fallback, enabled, domain, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(port as i32)
        .bind(password)
        .bind(fallback)
        .bind(enabled as i32)
        .bind(domain)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(())
    }

    /// Load all Trojan configurations
    pub async fn load_trojan_configs(&self) -> Result<Vec<TrojanConfigRecord>, sqlx::Error> {
        let records = sqlx::query_as::<_, TrojanConfigRecord>(
            r#"
            SELECT id, port, password, fallback, enabled, domain, created_at, updated_at
            FROM trojan_config
            ORDER BY port
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Load enabled Trojan configurations
    pub async fn load_enabled_trojan_configs(
        &self,
    ) -> Result<Vec<TrojanConfigRecord>, sqlx::Error> {
        let records = sqlx::query_as::<_, TrojanConfigRecord>(
            r#"
            SELECT id, port, password, fallback, enabled, domain, created_at, updated_at
            FROM trojan_config
            WHERE enabled = 1
            ORDER BY port
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Get Trojan config for a specific port
    pub async fn get_trojan_config(
        &self,
        port: u16,
    ) -> Result<Option<TrojanConfigRecord>, sqlx::Error> {
        let record = sqlx::query_as::<_, TrojanConfigRecord>(
            r#"
            SELECT id, port, password, fallback, enabled, domain, created_at, updated_at
            FROM trojan_config
            WHERE port = ?
            "#,
        )
        .bind(port as i32)
        .fetch_optional(&self.pool)
        .await?;

        Ok(record)
    }

    /// Delete Trojan configuration
    pub async fn delete_trojan_config(&self, port: u16) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            DELETE FROM trojan_config
            WHERE port = ?
            "#,
        )
        .bind(port as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Insert a log entry into the database
    pub async fn insert_log(
        &self,
        entry: &crate::server::logs::LogEntry,
    ) -> Result<i64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            INSERT INTO server_logs (timestamp, level, source, target, message)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(entry.timestamp)
        .bind(&entry.level)
        .bind(&entry.source)
        .bind(&entry.target)
        .bind(&entry.message)
        .execute(&self.pool)
        .await?;

        Ok(result.last_insert_rowid())
    }

    /// Insert a batch of log entries
    pub async fn insert_logs_batch(
        &self,
        entries: &[crate::server::logs::LogEntry],
    ) -> Result<(), sqlx::Error> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;

        for entry in entries {
            sqlx::query(
                r#"
                INSERT INTO server_logs (timestamp, level, source, target, message)
                VALUES (?, ?, ?, ?, ?)
                "#,
            )
            .bind(entry.timestamp)
            .bind(&entry.level)
            .bind(&entry.source)
            .bind(&entry.target)
            .bind(&entry.message)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Query logs with filters
    pub async fn query_logs(
        &self,
        level: Option<&str>,
        source: Option<&str>,
        search: Option<&str>,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<Vec<DbLogEntry>, sqlx::Error> {
        let mut query_str = String::from(
            "SELECT id, timestamp, level, source, target, message FROM server_logs WHERE 1=1",
        );
        let mut params: Vec<String> = Vec::new();

        if let Some(lvl) = level {
            let levels = match lvl.to_lowercase().as_str() {
                "error" => vec!["ERROR"],
                "warn" => vec!["ERROR", "WARN"],
                "info" => vec!["ERROR", "WARN", "INFO"],
                "debug" => vec!["ERROR", "WARN", "INFO", "DEBUG"],
                "trace" => vec!["ERROR", "WARN", "INFO", "DEBUG", "TRACE"],
                _ => vec!["ERROR", "WARN", "INFO", "DEBUG", "TRACE"],
            };
            let placeholders: Vec<String> = levels
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect();
            query_str.push_str(&format!(" AND level IN ({})", placeholders.join(",")));
            for l in levels {
                params.push(l.to_string());
            }
        }

        if let Some(ref src) = source {
            params.push(format!("{}%", src));
            query_str.push_str(&format!(" AND source LIKE ?{}", params.len()));
        }

        if let Some(ref s) = search {
            params.push(format!("%{}%", s));
            query_str.push_str(&format!(" AND message LIKE ?{}", params.len()));
        }

        if let Some(before) = before_id {
            params.push(before.to_string());
            query_str.push_str(&format!(" AND id < ?{}", params.len()));
        }

        query_str.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", params.len() + 1));
        params.push(limit.to_string());

        // Build the dynamic query
        let mut query = sqlx::query_as::<_, DbLogEntry>(&query_str);
        for p in &params {
            query = query.bind(p);
        }

        let mut rows = query.fetch_all(&self.pool).await?;
        // Reverse to get chronological order
        rows.reverse();
        Ok(rows)
    }

    /// Delete logs older than the given timestamp
    pub async fn cleanup_old_logs(&self, older_than_micros: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM server_logs
            WHERE timestamp < ?
            "#,
        )
        .bind(older_than_micros)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Save a mesh network record
    pub async fn save_mesh_network(
        &self,
        id: &str,
        description: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO mesh_networks (id, created_at, description)
            VALUES (?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET description = excluded.description
            "#,
        )
        .bind(id)
        .bind(now)
        .bind(description)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all mesh networks
    pub async fn load_mesh_networks(&self) -> Result<Vec<MeshNetworkRecord>, sqlx::Error> {
        sqlx::query_as::<_, MeshNetworkRecord>(
            "SELECT id, created_at, description FROM mesh_networks ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Save or update a mesh service
    pub async fn save_mesh_service(
        &self,
        mesh_id: &str,
        client_name: &str,
        service_name: &str,
        protocol: &str,
        local_addr: &str,
        dns_record: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO mesh_services (mesh_id, client_name, service_name, protocol, local_addr, dns_record)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(mesh_id, service_name) DO UPDATE SET
                client_name = excluded.client_name,
                protocol = excluded.protocol,
                local_addr = excluded.local_addr,
                dns_record = excluded.dns_record
            "#,
        )
        .bind(mesh_id)
        .bind(client_name)
        .bind(service_name)
        .bind(protocol)
        .bind(local_addr)
        .bind(dns_record)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load services for a mesh
    pub async fn load_mesh_services(
        &self,
        mesh_id: &str,
    ) -> Result<Vec<MeshServiceRecord>, sqlx::Error> {
        sqlx::query_as::<_, MeshServiceRecord>(
            "SELECT id, mesh_id, client_name, service_name, protocol, local_addr, dns_record \
             FROM mesh_services WHERE mesh_id = ? ORDER BY service_name",
        )
        .bind(mesh_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Delete a mesh service
    pub async fn delete_mesh_service(
        &self,
        mesh_id: &str,
        service_name: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM mesh_services WHERE mesh_id = ? AND service_name = ?")
            .bind(mesh_id)
            .bind(service_name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

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

    // ============================================================
    // Dynamic configuration methods
    // ============================================================

    /// Load reverse proxy config (singleton)
    pub async fn load_reverse_proxy_config(
        &self,
    ) -> Result<Option<ReverseProxyConfigRecord>, sqlx::Error> {
        sqlx::query_as::<_, ReverseProxyConfigRecord>(
            "SELECT max_connections, connection_timeout_secs, buffer_size FROM reverse_proxy_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Save reverse proxy config (singleton, upsert)
    pub async fn save_reverse_proxy_config(
        &self,
        max_connections: u32,
        connection_timeout_secs: u64,
        buffer_size: usize,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO reverse_proxy_config (id, max_connections, connection_timeout_secs, buffer_size, updated_at)
            VALUES (1, ?, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                max_connections = excluded.max_connections,
                connection_timeout_secs = excluded.connection_timeout_secs,
                buffer_size = excluded.buffer_size,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(max_connections as i64)
        .bind(connection_timeout_secs as i64)
        .bind(buffer_size as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

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
            r#"
            INSERT INTO dns_config (id, tunnel_domain, mesh_domain, updated_at)
            VALUES (1, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                tunnel_domain = excluded.tunnel_domain,
                mesh_domain = excluded.mesh_domain,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(tunnel_domain)
        .bind(mesh_domain)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load a server setting by key
    pub async fn load_server_setting(&self, key: &str) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query("SELECT value FROM server_settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    /// Save a server setting (upsert)
    pub async fn save_server_setting(&self, key: &str, value: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO server_settings (key, value, updated_at)
            VALUES (?, ?, datetime('now'))
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ============================================================
    // Server auth methods
    // ============================================================

    pub async fn load_server_auth(&self) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query("SELECT client_token FROM server_auth WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("client_token")))
    }

    pub async fn save_server_auth(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO server_auth (id, client_token, updated_at)
            VALUES (1, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                client_token = excluded.client_token,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(token)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    }

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_db() -> Database {
        Database::new(":memory:")
            .await
            .expect("Failed to create in-memory database")
    }
    //

    #[tokio::test]
    async fn test_shadowsocks_config_crud() {
        let db = create_test_db().await;

        // Create
        db.save_shadowsocks_config(8388, "aes-256-gcm", "password123", true)
            .await
            .unwrap();

        // Read
        let config = db.get_shadowsocks_config(8388).await.unwrap();
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.port, 8388);
        assert_eq!(config.cipher, "aes-256-gcm");
        assert_eq!(config.password, "password123");
        assert_eq!(config.enabled, 1);

        // Update
        db.save_shadowsocks_config(8388, "chacha20-ietf-poly1305", "newpass", false)
            .await
            .unwrap();
        let config = db.get_shadowsocks_config(8388).await.unwrap().unwrap();
        assert_eq!(config.cipher, "chacha20-ietf-poly1305");
        assert_eq!(config.password, "newpass");
        assert_eq!(config.enabled, 0);

        // Delete
        db.delete_shadowsocks_config(8388).await.unwrap();
        let config = db.get_shadowsocks_config(8388).await.unwrap();
        assert!(config.is_none());
    }

    #[tokio::test]
    async fn test_load_shadowsocks_configs() {
        let db = create_test_db().await;

        db.save_shadowsocks_config(8388, "aes-256-gcm", "pass1", true)
            .await
            .unwrap();
        db.save_shadowsocks_config(8389, "chacha20-ietf-poly1305", "pass2", true)
            .await
            .unwrap();
        db.save_shadowsocks_config(8390, "aes-256-gcm", "pass3", false)
            .await
            .unwrap();

        let all_configs = db.load_shadowsocks_configs().await.unwrap();
        assert_eq!(all_configs.len(), 3);

        let enabled_configs = db.load_enabled_shadowsocks_configs().await.unwrap();
        assert_eq!(enabled_configs.len(), 2);
    }

    #[tokio::test]
    async fn test_shadowsocks_config_unique_port() {
        let db = create_test_db().await;

        db.save_shadowsocks_config(8388, "aes-256-gcm", "pass1", true)
            .await
            .unwrap();
        // Upsert on same port should update, not duplicate
        db.save_shadowsocks_config(8388, "chacha20-ietf-poly1305", "pass2", true)
            .await
            .unwrap();

        let configs = db.load_shadowsocks_configs().await.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].cipher, "chacha20-ietf-poly1305");
    }

    #[tokio::test]
    async fn test_replace_shadowsocks_config_single_row() {
        let db = create_test_db().await;

        db.save_shadowsocks_config(8388, "aes-256-gcm", "pass1", true)
            .await
            .unwrap();
        // 修改端口：整表替换，不应残留旧行
        db.replace_shadowsocks_config(9999, "chacha20-ietf-poly1305", "pass2", true)
            .await
            .unwrap();

        let configs = db.load_shadowsocks_configs().await.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].port, 9999);
        assert_eq!(configs[0].cipher, "chacha20-ietf-poly1305");
        assert_eq!(configs[0].password, "pass2");
        assert_eq!(configs[0].enabled, 1);
    }

    #[tokio::test]
    async fn test_replace_trojan_config_single_row() {
        let db = create_test_db().await;

        db.save_trojan_config(443, "pass1", "127.0.0.1:80", true, "")
            .await
            .unwrap();
        // 修改端口：整表替换，不应残留旧行
        db.replace_trojan_config(8443, "pass2", "127.0.0.1:8080", false, "")
            .await
            .unwrap();

        let configs = db.load_trojan_configs().await.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].port, 8443);
        assert_eq!(configs[0].password, "pass2");
        assert_eq!(configs[0].fallback, "127.0.0.1:8080");
        assert_eq!(configs[0].enabled, 0);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_shadowsocks_config() {
        let db = create_test_db().await;
        // Should not error
        db.delete_shadowsocks_config(9999).await.unwrap();
    }

    #[tokio::test]
    async fn test_server_logs_table_creation() {
        let db = create_test_db().await;
        // Just verify the table exists by doing an insert+query
        let result = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_insert_and_query_logs() {
        let db = create_test_db().await;
        let entry = crate::server::logs::LogEntry {
            id: 0,
            timestamp: 1000000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test::module".into(),
            message: "test message".into(),
        };
        db.insert_log(&entry).await.unwrap();

        let results = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].level, "INFO");
        assert_eq!(results[0].message, "test message");
    }

    #[tokio::test]
    async fn test_query_logs_level_filter() {
        let db = create_test_db().await;
        let info_entry = crate::server::logs::LogEntry {
            id: 0,
            timestamp: 1000000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "info msg".into(),
        };
        let error_entry = crate::server::logs::LogEntry {
            id: 0,
            timestamp: 2000000,
            level: "ERROR".into(),
            source: "server".into(),
            target: "test".into(),
            message: "error msg".into(),
        };
        db.insert_log(&info_entry).await.unwrap();
        db.insert_log(&error_entry).await.unwrap();

        // Filter to WARN+ (includes WARN and ERROR)
        let results = db
            .query_logs(Some("warn"), None, None, 10, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].level, "ERROR");
    }

    #[tokio::test]
    async fn test_cleanup_old_logs() {
        let db = create_test_db().await;
        let entry = crate::server::logs::LogEntry {
            id: 0,
            timestamp: 1000000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "old".into(),
        };
        db.insert_log(&entry).await.unwrap();

        // Cleanup anything older than 2000000
        let deleted = db.cleanup_old_logs(2000000).await.unwrap();
        assert_eq!(deleted, 1);

        let results = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_insert_logs_batch() {
        let db = create_test_db().await;
        let entries: Vec<crate::server::logs::LogEntry> = (0..3)
            .map(|i| crate::server::logs::LogEntry {
                id: 0,
                timestamp: 1000000 + i * 1000,
                level: "INFO".into(),
                source: "server".into(),
                target: "test".into(),
                message: format!("msg {}", i),
            })
            .collect();
        db.insert_logs_batch(&entries).await.unwrap();

        let results = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn test_delete_client() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();
        db.delete_client("home-nas").await.unwrap();
        let list = db.list_clients().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_server_auth_load_and_save() {
        let db = Database::new(":memory:").await.unwrap();
        assert!(db.load_server_auth().await.unwrap().is_none());
        db.save_server_auth("token-abc").await.unwrap();
        assert_eq!(
            db.load_server_auth().await.unwrap().as_deref(),
            Some("token-abc")
        );
        db.save_server_auth("token-def").await.unwrap();
        assert_eq!(
            db.load_server_auth().await.unwrap().as_deref(),
            Some("token-def")
        );
    }

    #[tokio::test]
    async fn test_rules_referencing_client() {
        let db = Database::new(":memory:").await.unwrap();

        // route JSON: 一个 backend 指向 home-nas，一个 direct
        let routes_json = serde_json::json!([
            {
                "path": "/",
                "backends": [
                    { "kind": "client", "addr": "localhost:80", "client_name": "home-nas",
                      "weight": 100, "protocol": "http1", "scheme": "http" },
                    { "kind": "direct", "addr": "10.0.0.1:80",
                      "weight": 100, "protocol": "http1", "scheme": "http" }
                ],
                "load_balancing": "round_robin"
            }
        ])
        .to_string();

        db.save_proxy_rule(
            "rule-1",
            "web",
            "http",
            "0.0.0.0:80",
            None,
            Some(&routes_json),
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

        let refs = db.rules_referencing_client("home-nas").await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "rule-1");
        assert_eq!(refs[0].1, "web");

        let refs = db.rules_referencing_client("nonexistent").await.unwrap();
        assert!(refs.is_empty());
    }

    #[tokio::test]
    async fn test_rules_referencing_client_ignores_direct_only() {
        let db = Database::new(":memory:").await.unwrap();
        let routes_json = serde_json::json!([{
            "path": "/",
            "backends": [
                { "kind": "direct", "addr": "10.0.0.1:80",
                  "weight": 100, "protocol": "http1", "scheme": "http" }
            ],
            "load_balancing": "round_robin"
        }])
        .to_string();
        db.save_proxy_rule(
            "r1",
            "web",
            "http",
            "0.0.0.0:80",
            None,
            Some(&routes_json),
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
        assert!(db
            .rules_referencing_client("anything")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn test_rules_referencing_client_null_routes() {
        // rule with routes = NULL (e.g. tcp rule)
        let db = Database::new(":memory:").await.unwrap();
        db.save_proxy_rule(
            "r1",
            "tcp-rule",
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
        assert!(db
            .rules_referencing_client("anyone")
            .await
            .unwrap()
            .is_empty());
    }
}

#[cfg(test)]
mod cert_status_migration_tests {
    use super::*;

    #[tokio::test]
    async fn migration_adds_cert_columns_to_empty_db() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();

        // 查询 pragma_table_info 验证列存在
        let cols: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM pragma_table_info('proxy_rules')")
                .fetch_all(db.pool())
                .await
                .unwrap();
        let names: Vec<String> = cols.into_iter().map(|(n,)| n).collect();
        assert!(names.contains(&"cert_source".to_string()));
        assert!(names.contains(&"cert_covering_domain".to_string()));
        assert!(names.contains(&"cert_status_updated_at".to_string()));
    }

    #[tokio::test]
    async fn migration_idempotent_on_existing_db() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("test.db");
        // 打开两次应无错
        let _db1 = Database::new(path.to_str().unwrap()).await.unwrap();
        let _db2 = Database::new(path.to_str().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn save_and_load_rule_with_cert_status_fields() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();

        db.save_proxy_rule(
            "r-1",
            "test",
            "http",
            "0.0.0.0:443",
            Some(r#"["a.example.com"]"#),
            Some(r#"[]"#),
            true,
            true,
            Some("a.example.com"),
            true,
            Some("exact"),
            Some("a.example.com"),
            Some(&chrono::Utc::now()),
        )
        .await
        .unwrap();

        let rules = db.load_proxy_rules().await.unwrap();
        assert_eq!(rules.len(), 1);
        let r = &rules[0];
        assert_eq!(r.cert_source.as_deref(), Some("exact"));
        assert_eq!(r.cert_covering_domain.as_deref(), Some("a.example.com"));
        assert!(r.cert_status_updated_at.is_some());
    }

    /// 旧库（无 domain 列）经 Database::new 迁移后应自动补上 domain 列，
    /// 存量行 domain 默认 ''。
    #[tokio::test]
    async fn trojan_config_domain_column_migration() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let db_path = tempdir.path().join("migrate.db");

        // 用旧 schema（无 domain 列）手工建表并插入一行
        {
            let opts = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true);
            let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
            sqlx::query(
                r#"
                CREATE TABLE trojan_config (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    port INTEGER NOT NULL UNIQUE,
                    password TEXT NOT NULL,
                    fallback TEXT NOT NULL DEFAULT '127.0.0.1:80',
                    enabled INTEGER NOT NULL DEFAULT 1,
                    created_at DATETIME NOT NULL,
                    updated_at DATETIME NOT NULL
                )
                "#,
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO trojan_config (port, password, fallback, enabled, created_at, updated_at) \
                 VALUES (1443, 'old-pass', '127.0.0.1:80', 1, '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let db = Database::new(db_path.to_str().unwrap()).await.unwrap();
        let configs = db.load_trojan_configs().await.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].port, 1443);
        assert_eq!(configs[0].domain, "", "迁移后存量行 domain 应为空串");
    }

    /// domain 随 save/replace/load 完整往返。
    #[tokio::test]
    async fn trojan_config_domain_roundtrip() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let db_path = tempdir.path().join("roundtrip.db");
        let db = Database::new(db_path.to_str().unwrap()).await.unwrap();

        db.save_trojan_config(443, "pass1", "127.0.0.1:80", true, "trojan.example.com")
            .await
            .unwrap();
        let cfg = db.get_trojan_config(443).await.unwrap().unwrap();
        assert_eq!(cfg.domain, "trojan.example.com");

        // replace 整表替换语义保留，domain 一并更新
        db.replace_trojan_config(8443, "pass2", "127.0.0.1:8080", false, "t2.example.com")
            .await
            .unwrap();
        let all = db.load_trojan_configs().await.unwrap();
        assert_eq!(all.len(), 1, "replace 后表中应只有一份配置");
        assert_eq!(all[0].port, 8443);
        assert_eq!(all[0].domain, "t2.example.com");
        assert_eq!(all[0].enabled, 0);

        let enabled = db.load_enabled_trojan_configs().await.unwrap();
        assert!(
            enabled.is_empty(),
            "enabled=0 的行不应出现在 enabled 查询里"
        );
    }
}
