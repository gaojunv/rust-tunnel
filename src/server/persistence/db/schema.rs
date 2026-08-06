use super::Database;
use sqlx::{Pool, Sqlite};

impl Database {
    /// Initialize database tables
    pub(crate) async fn initialize_schema(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        // Enable WAL mode for concurrent reads/writes and set synchronous to NORMAL
        // (NORMAL is safe in WAL mode and avoids extra fsync on every write)
        sqlx::query("PRAGMA journal_mode=WAL").execute(pool).await?;
        sqlx::query("PRAGMA synchronous=NORMAL")
            .execute(pool)
            .await?;

        // Schema v2 migration: drop legacy stats tables replaced by stats_snapshots
        for tbl in &[
            "traffic_buckets",
            "port_traffic",
            "proxy_traffic",
            "connection_quality_history",
        ] {
            sqlx::query(&format!("DROP TABLE IF EXISTS {}", tbl))
                .execute(pool)
                .await?;
        }

        // Client session history
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS client_sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL,
                hostname TEXT,
                connected_at DATETIME NOT NULL,
                disconnected_at DATETIME,
                duration_seconds INTEGER
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Create indexes
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_client_sessions_port ON client_sessions(port)")
            .execute(pool)
            .await?;
        // Shadowsocks configuration table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS shadowsocks_config (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL UNIQUE,
                cipher TEXT NOT NULL,
                password TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Trojan configuration table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS trojan_config (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL UNIQUE,
                password TEXT NOT NULL,
                fallback TEXT NOT NULL DEFAULT '127.0.0.1:80',
                enabled INTEGER NOT NULL DEFAULT 1,
                domain TEXT NOT NULL DEFAULT '',
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Migrate: 旧库补 domain 列（幂等——报 "duplicate column" 时忽略）
        if let Err(e) =
            sqlx::query("ALTER TABLE trojan_config ADD COLUMN domain TEXT NOT NULL DEFAULT ''")
                .execute(pool)
                .await
        {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(e);
            }
        }

        // Server logs table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS server_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                level TEXT NOT NULL,
                source TEXT NOT NULL,
                target TEXT NOT NULL,
                message TEXT NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_timestamp ON server_logs(timestamp)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_level ON server_logs(level)")
            .execute(pool)
            .await?;

        // Mesh networks table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS mesh_networks (
                id TEXT PRIMARY KEY,
                created_at DATETIME NOT NULL,
                description TEXT
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Mesh services table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS mesh_services (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mesh_id TEXT NOT NULL REFERENCES mesh_networks(id),
                client_name TEXT NOT NULL,
                service_name TEXT NOT NULL,
                protocol TEXT NOT NULL,
                local_addr TEXT NOT NULL,
                dns_record TEXT NOT NULL,
                UNIQUE(mesh_id, service_name)
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_mesh_services_mesh ON mesh_services(mesh_id)")
            .execute(pool)
            .await?;

        // ============================================================
        // Reverse Proxy tables
        // ============================================================

        // Proxy rules table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS proxy_rules (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                type TEXT NOT NULL CHECK(type IN ('http', 'tcp', 'udp', 'llm')),
                listen_addr TEXT NOT NULL,
                domains TEXT,
                routes TEXT,
                tls_enabled INTEGER NOT NULL DEFAULT 0,
                tls_acme INTEGER NOT NULL DEFAULT 0,
                tls_domain TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL,
                cert_source TEXT,
                cert_covering_domain TEXT,
                cert_status_updated_at DATETIME
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Migrate: add columns if missing (idempotent — errors ignored on "duplicate column")
        for col_sql in [
            "ALTER TABLE proxy_rules ADD COLUMN cert_source TEXT",
            "ALTER TABLE proxy_rules ADD COLUMN cert_covering_domain TEXT",
            "ALTER TABLE proxy_rules ADD COLUMN cert_status_updated_at DATETIME",
        ] {
            if let Err(e) = sqlx::query(col_sql).execute(pool).await {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(e);
                }
            }
        }

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_proxy_rules_type ON proxy_rules(type)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_proxy_rules_enabled ON proxy_rules(enabled)")
            .execute(pool)
            .await?;

        // Migration: old DBs have CHECK(type IN ('http','tcp','udp')) without 'llm'.
        // Test if 'llm' is accepted and rebuild the table if not.
        Self::migrate_proxy_rules_check_for_llm(pool).await?;

        // ── Unified stats snapshots (replaces proxy_traffic / connection_quality_history) ──
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS stats_snapshots (
                entity_type  TEXT NOT NULL,
                entity_id    TEXT NOT NULL,
                timestamp    DATETIME NOT NULL,
                bytes_in     BIGINT NOT NULL DEFAULT 0,
                bytes_out    BIGINT NOT NULL DEFAULT 0,
                bytes_in_rate  REAL NOT NULL DEFAULT 0.0,
                bytes_out_rate REAL NOT NULL DEFAULT 0.0,
                rtt_ms       REAL,
                loss_pct     REAL,
                active_conns INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (entity_type, entity_id, timestamp)
            )
            "#,
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_stats_snapshots_type ON stats_snapshots(entity_type, timestamp)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_stats_snapshots_entity ON stats_snapshots(entity_type, entity_id, timestamp)",
        )
        .execute(pool)
        .await?;

        // ============================================================
        // ACME Certificate tables
        // ============================================================

        // ACME certificates table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS acme_certificates (
                domain TEXT PRIMARY KEY,
                status TEXT NOT NULL CHECK(status IN ('pending', 'active', 'expired', 'failed')),
                cert_pem TEXT,
                key_pem TEXT,
                chain_pem TEXT,
                issued_at DATETIME,
                expires_at DATETIME,
                auto_renew INTEGER NOT NULL DEFAULT 1,
                last_renewal_attempt DATETIME,
                error_message TEXT,
                created_at DATETIME NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_acme_certificates_status ON acme_certificates(status)",
        )
        .execute(pool)
        .await?;

        // ACME challenges table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS acme_challenges (
                token TEXT PRIMARY KEY,
                domain TEXT NOT NULL,
                authorization TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending', 'valid', 'invalid')),
                created_at DATETIME NOT NULL,
                expires_at DATETIME
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_acme_challenges_domain ON acme_challenges(domain)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_acme_challenges_expires ON acme_challenges(expires_at)",
        )
        .execute(pool)
        .await?;

        // ============================================================
        // Dynamic configuration tables
        // ============================================================

        // Reverse proxy global config (singleton)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS reverse_proxy_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                max_connections INTEGER NOT NULL DEFAULT 10000,
                connection_timeout_secs INTEGER NOT NULL DEFAULT 30,
                buffer_size INTEGER NOT NULL DEFAULT 8192,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // DNS config (singleton)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS dns_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                tunnel_domain TEXT NOT NULL DEFAULT 'tunnel.local',
                mesh_domain TEXT NOT NULL DEFAULT 'mesh.local',
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // General server settings (key-value)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS server_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Client registry table (spec §2.1)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS clients (
                name          TEXT PRIMARY KEY,
                hostname      TEXT,
                first_seen_at DATETIME NOT NULL,
                last_seen_at  DATETIME NOT NULL,
                note          TEXT
            )
            "#,
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_clients_last_seen ON clients(last_seen_at)")
            .execute(pool)
            .await?;

        // Single-row server auth table (spec §2.2)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS server_auth (
                id           INTEGER PRIMARY KEY CHECK(id = 1),
                client_token TEXT NOT NULL,
                updated_at   DATETIME NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        // ============================================================
        // LLM Gateway tables
        // ============================================================

        // LLM providers table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_providers (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                provider_type TEXT NOT NULL CHECK(provider_type IN ('deepseek', 'volcengine', 'kimi', 'mimo')),
                base_url TEXT NOT NULL,
                api_key TEXT NOT NULL DEFAULT '',
                extra_config TEXT,
                anthropic_base_url TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // LLM models table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_models (
                id TEXT PRIMARY KEY,
                provider_id TEXT NOT NULL REFERENCES llm_providers(id) ON DELETE CASCADE,
                model_name TEXT NOT NULL,
                alias TEXT NOT NULL DEFAULT '',
                tags TEXT NOT NULL DEFAULT '[]',
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_llm_models_provider ON llm_models(provider_id)",
        )
        .execute(pool)
        .await?;

        // Migration: old DBs lack extra_config column on llm_models (per-model config,
        // 如 agent_context_limit，供压缩模块读取）。
        Self::migrate_llm_models_extra_config(pool).await?;

        // LLM API keys table (gateway-level keys for external callers)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_api_keys (
                id TEXT PRIMARY KEY,
                key_hash TEXT NOT NULL UNIQUE,
                key_prefix TEXT NOT NULL,
                name TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_used_at TEXT
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Migration: old DBs lack anthropic_base_url column on llm_providers.
        Self::migrate_llm_providers_add_anthropic_url(pool).await?;

        // LLM usage logs table — one row per gateway request (usage stats).
        // 冗余存 *_name 列，使 Key/模型/供应商删除后历史记录仍可读。
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_usage_logs (
                id TEXT PRIMARY KEY,
                timestamp TEXT NOT NULL,
                api_key_id TEXT,
                api_key_name TEXT NOT NULL DEFAULT '',
                provider_id TEXT,
                provider_name TEXT NOT NULL DEFAULT '',
                model_id TEXT,
                model_name TEXT NOT NULL DEFAULT '',
                requested_model TEXT NOT NULL DEFAULT '',
                protocol TEXT NOT NULL DEFAULT 'openai',
                stream INTEGER NOT NULL DEFAULT 0,
                status_code INTEGER NOT NULL DEFAULT 0,
                success INTEGER NOT NULL DEFAULT 0,
                prompt_tokens INTEGER NOT NULL DEFAULT 0,
                cache_hit_tokens INTEGER NOT NULL DEFAULT 0,
                cache_miss_tokens INTEGER NOT NULL DEFAULT 0,
                completion_tokens INTEGER NOT NULL DEFAULT 0,
                total_tokens INTEGER NOT NULL DEFAULT 0,
                latency_ms INTEGER NOT NULL DEFAULT 0,
                error_type TEXT
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_llm_usage_timestamp ON llm_usage_logs(timestamp)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_llm_usage_api_key ON llm_usage_logs(api_key_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_llm_usage_provider ON llm_usage_logs(provider_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_llm_usage_model ON llm_usage_logs(model_id)")
            .execute(pool)
            .await?;

        // LLM model groups (failover routing): 组聚合多个模型，按 priority 依次尝试
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_model_groups (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // 组成员：一组多模型，priority 升序即故障转移尝试顺序
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_model_group_members (
                group_id TEXT NOT NULL REFERENCES llm_model_groups(id) ON DELETE CASCADE,
                model_id TEXT NOT NULL REFERENCES llm_models(id) ON DELETE CASCADE,
                priority INTEGER NOT NULL,
                PRIMARY KEY (group_id, model_id)
            )
            "#,
        )
        .execute(pool)
        .await?;

        // RAG knowledge bases
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS rag_knowledge_bases (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                emb_base_url TEXT NOT NULL,
                emb_api_key TEXT NOT NULL,
                emb_model TEXT NOT NULL,
                emb_dimension INTEGER NOT NULL,
                top_k INTEGER NOT NULL DEFAULT 5,
                chunk_size INTEGER NOT NULL DEFAULT 512,
                chunk_overlap INTEGER NOT NULL DEFAULT 64,
                score_threshold REAL NOT NULL DEFAULT 0.3,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS rag_documents (
                id TEXT PRIMARY KEY,
                kb_id TEXT NOT NULL REFERENCES rag_knowledge_bases(id) ON DELETE CASCADE,
                filename TEXT NOT NULL,
                file_type TEXT NOT NULL DEFAULT '',
                content_hash TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                chunk_count INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS rag_chunks (
                id TEXT PRIMARY KEY,
                doc_id TEXT NOT NULL REFERENCES rag_documents(id) ON DELETE CASCADE,
                kb_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                heading_path TEXT NOT NULL DEFAULT '',
                content TEXT NOT NULL,
                token_count INTEGER NOT NULL DEFAULT 0
            )
            "#,
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_rag_chunks_doc ON rag_chunks(doc_id)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_rag_chunks_kb ON rag_chunks(kb_id)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_rag_documents_kb ON rag_documents(kb_id)")
            .execute(pool)
            .await?;

        // 幂等迁移：llm_api_keys 加 kb_id，llm_usage_logs 加 rag_chunks_injected / failover_from
        Self::migrate_llm_api_keys_add_kb_id(pool).await?;
        Self::migrate_llm_usage_add_rag_chunks(pool).await?;
        Self::migrate_llm_usage_add_failover_from(pool).await?;
        Self::migrate_rag_documents_add_file_type(pool).await?;

        // ============================================================
        // Agent workbench tables
        // ============================================================

        // Agent workspaces
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS agent_workspaces (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                client_id TEXT NOT NULL,
                runtime_type TEXT NOT NULL,
                root_path TEXT NOT NULL,
                docker_image TEXT,
                docker_container_id TEXT,
                approval_mode TEXT NOT NULL DEFAULT 'safe',
                system_prompt TEXT,
                created_at DATETIME NOT NULL DEFAULT (datetime('now')),
                updated_at DATETIME NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Agent sessions
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS agent_sessions (
                id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL REFERENCES agent_workspaces(id) ON DELETE CASCADE,
                title TEXT,
                status TEXT NOT NULL DEFAULT 'active',
                model TEXT,
                created_at DATETIME NOT NULL DEFAULT (datetime('now')),
                updated_at DATETIME NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Agent messages
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS agent_messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES agent_sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                tool_calls TEXT,
                created_at DATETIME NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_agent_messages_session ON agent_messages(session_id)",
        )
        .execute(pool)
        .await?;

        Self::migrate_agent_messages_v2(pool).await?;
        Self::migrate_agent_workspaces_v2(pool).await?;

        Ok(())
    }

    /// agent_messages 补全 tool_calls 结构列。幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_messages_v2(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        for (column, ddl) in [
            (
                "tool_call_id",
                "ALTER TABLE agent_messages ADD COLUMN tool_call_id TEXT",
            ),
            ("name", "ALTER TABLE agent_messages ADD COLUMN name TEXT"),
            (
                "kind",
                "ALTER TABLE agent_messages ADD COLUMN kind TEXT NOT NULL DEFAULT 'message'",
            ),
        ] {
            match sqlx::query(ddl).execute(pool).await {
                Ok(_) => {}
                Err(e) => {
                    // SQLite: "duplicate column name: xxx" —— 已迁移过
                    if !e.to_string().contains("duplicate column") {
                        return Err(e);
                    }
                    tracing::debug!(column, "agent_messages migration: column already exists");
                }
            }
        }
        Ok(())
    }

    /// agent_workspaces 补全审批模式与自定义提示词列。幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_workspaces_v2(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        for (column, ddl) in [
            (
                "approval_mode",
                "ALTER TABLE agent_workspaces ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'safe'",
            ),
            (
                "system_prompt",
                "ALTER TABLE agent_workspaces ADD COLUMN system_prompt TEXT",
            ),
        ] {
            match sqlx::query(ddl).execute(pool).await {
                Ok(_) => {}
                Err(e) => {
                    if !e.to_string().contains("duplicate column") {
                        return Err(e);
                    }
                    tracing::debug!(column, "agent_workspaces migration: column already exists");
                }
            }
        }
        Ok(())
    }

    /// Migrate old proxy_rules CHECK constraint to include 'llm'.
    /// Idempotent: tests if 'llm' type is accepted; if so, no-op.
    async fn migrate_proxy_rules_check_for_llm(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        // Test if the current CHECK accepts 'llm'
        let probe_id = "__migration_probe_llm_check__";
        let test_result = sqlx::query(
            "INSERT INTO proxy_rules (id, name, type, listen_addr, created_at, updated_at)
             VALUES (?, 'probe', 'llm', '127.0.0.1:1', datetime('now'), datetime('now'))",
        )
        .bind(probe_id)
        .execute(pool)
        .await;

        match test_result {
            Ok(_) => {
                // CHECK already accepts 'llm' — clean up probe and return
                sqlx::query("DELETE FROM proxy_rules WHERE id = ?")
                    .bind(probe_id)
                    .execute(pool)
                    .await?;
                return Ok(());
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("CHECK constraint failed") {
                    return Err(e);
                }
                // Fall through to migration
            }
        }

        // Run migration: rebuild table with updated CHECK.
        // 必须在单个事务（同一条连接）内完成——此前用 pool 逐条 execute，
        // BEGIN EXCLUSIVE 与后续语句落到不同连接，互相锁死报 "database is locked"。
        let mut tx = pool.begin().await?;
        // Create new table with updated CHECK (including 'llm')
        sqlx::query(
            r#"
            CREATE TABLE proxy_rules_new (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                type TEXT NOT NULL CHECK(type IN ('http', 'tcp', 'udp', 'llm')),
                listen_addr TEXT NOT NULL,
                domains TEXT,
                routes TEXT,
                tls_enabled INTEGER NOT NULL DEFAULT 0,
                tls_acme INTEGER NOT NULL DEFAULT 0,
                tls_domain TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL,
                cert_source TEXT,
                cert_covering_domain TEXT,
                cert_status_updated_at DATETIME
            )
            "#,
        )
        .execute(&mut *tx)
        .await?;

        // Copy existing data
        sqlx::query("INSERT INTO proxy_rules_new SELECT * FROM proxy_rules")
            .execute(&mut *tx)
            .await?;

        // Drop old table and rename new
        sqlx::query("DROP TABLE proxy_rules")
            .execute(&mut *tx)
            .await?;
        sqlx::query("ALTER TABLE proxy_rules_new RENAME TO proxy_rules")
            .execute(&mut *tx)
            .await?;

        // Recreate indexes
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_proxy_rules_type ON proxy_rules(type)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_proxy_rules_enabled ON proxy_rules(enabled)")
            .execute(&mut *tx)
            .await?;

        // 任一步失败时 tx 在 drop 时自动回滚，不会残留中间表或悬挂锁
        tx.commit().await?;
        Ok(())
    }

    /// Migration: old DBs lack `anthropic_base_url` column on `llm_providers`.
    /// Idempotent: tries ALTER TABLE; ignores "duplicate column" error.
    async fn migrate_llm_providers_add_anthropic_url(
        pool: &Pool<Sqlite>,
    ) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE llm_providers ADD COLUMN anthropic_base_url TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("duplicate column") {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Migration: old DBs lack `kb_id` on `llm_api_keys`. Idempotent.
    async fn migrate_llm_api_keys_add_kb_id(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE llm_api_keys ADD COLUMN kb_id TEXT REFERENCES rag_knowledge_bases(id) ON DELETE SET NULL")
            .execute(pool)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("duplicate column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Migration: old DBs lack `rag_chunks_injected` on `llm_usage_logs`. Idempotent.
    async fn migrate_llm_usage_add_rag_chunks(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE llm_usage_logs ADD COLUMN rag_chunks_injected INTEGER")
            .execute(pool)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("duplicate column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Migration: old DBs lack `failover_from` on `llm_usage_logs`. Idempotent.
    async fn migrate_llm_usage_add_failover_from(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE llm_usage_logs ADD COLUMN failover_from TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("duplicate column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// llm_models 加 extra_config JSON 列（per-model 配置，如 agent_context_limit）。幂等。
    async fn migrate_llm_models_extra_config(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE llm_models ADD COLUMN extra_config TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("duplicate column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// 回填 SQL：旧版所有上传（含 .txt）一律落盘为 `<doc_id>.md`（原文不保留真实
    /// 扩展名），故 legacy 行无条件回填 'md'。绝不能按 filename 扩展名推导——
    /// 否则 notes.txt 会被推导成 'txt'，reindex 找 `.txt` 原文 409、delete 孤儿化
    /// 真实的 `.md` 文件。
    /// 与 `Database::backfill_rag_document_file_type`（db/rag.rs）共享，避免两份 SQL 漂移。
    pub(crate) const BACKFILL_RAG_DOCUMENT_FILE_TYPE_SQL: &str =
        "UPDATE rag_documents SET file_type = 'md' WHERE file_type = ''";

    /// Migration: old DBs lack `file_type` on `rag_documents`. Idempotent.
    /// 列添加成功后在同一函数内回填老数据为 'md'（老数据落盘一律 .md，见上方常量注释）。
    async fn migrate_rag_documents_add_file_type(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE rag_documents ADD COLUMN file_type TEXT NOT NULL DEFAULT ''")
            .execute(pool)
            .await
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column") => {}
            Err(e) => return Err(e),
        }
        sqlx::query(Self::BACKFILL_RAG_DOCUMENT_FILE_TYPE_SQL)
            .execute(pool)
            .await?;
        Ok(())
    }
}
