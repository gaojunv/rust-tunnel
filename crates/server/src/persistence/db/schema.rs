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
                acp_session_id TEXT,
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

        // AI 记忆体全局设置（单行 id=1，CHECK 约束）。emb_api_key 用 LlmCipher
        // 加密存储；emb_dimension 首次 test-embedding 探测后固定，改动需清空重建。
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS agent_memory_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                enabled INTEGER NOT NULL DEFAULT 0,
                emb_base_url TEXT NOT NULL DEFAULT '',
                emb_api_key TEXT NOT NULL DEFAULT '',
                emb_model TEXT NOT NULL DEFAULT '',
                emb_dimension INTEGER NOT NULL DEFAULT 0,
                distill_model TEXT NOT NULL DEFAULT '',
                top_k INTEGER NOT NULL DEFAULT 8,
                score_threshold REAL NOT NULL DEFAULT 0.40,
                inject_budget_tokens INTEGER NOT NULL DEFAULT 1500,
                pin_always_inject INTEGER NOT NULL DEFAULT 1,
                skill_enabled INTEGER NOT NULL DEFAULT 0,
                skill_list_max INTEGER NOT NULL DEFAULT 20,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // AI 记忆体主表：原子事实（≤2KB），作用域隔离（global|client|workspace）。
        // 向量本体在 `<data_dir>/rag/memory/`（kb_id 常量 "memory"），ChunkPoint 的
        // id 与 doc_id 均取本表 id（删除走 delete_by_doc("memory", dim, memory_id)）。
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS agent_memories (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                scope_type TEXT NOT NULL DEFAULT 'workspace',
                client_id TEXT NOT NULL DEFAULT '',
                workspace_id TEXT NOT NULL DEFAULT '',
                tags TEXT NOT NULL DEFAULT '[]',
                confidence REAL NOT NULL DEFAULT 0.8,
                source_session_id TEXT NOT NULL DEFAULT '',
                source_trigger TEXT NOT NULL DEFAULT '',
                pinned INTEGER NOT NULL DEFAULT 0,
                hit_count INTEGER NOT NULL DEFAULT 0,
                last_hit_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_scope ON agent_memories(client_id, workspace_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_source ON agent_memories(source_session_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_pinned ON agent_memories(pinned)")
            .execute(pool)
            .await?;

        // AI Skill 库主表：可复用执行经验（排障手册/发布清单/评审步骤等），
        // 含触发边界（description）+ 执行步骤（content Markdown 全文）。**不向量化**
        // （数量少、清单注入无需语义检索、按 name+scope 文本去重）——纯 SQLite + SQL，
        // embedding 未配置也能工作。作用域隔离对齐 agent_memories（global|client|workspace）。
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS agent_skills (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                content TEXT NOT NULL,
                scope_type TEXT NOT NULL DEFAULT 'workspace',
                client_id TEXT NOT NULL DEFAULT '',
                workspace_id TEXT NOT NULL DEFAULT '',
                tags TEXT NOT NULL DEFAULT '[]',
                enabled INTEGER NOT NULL DEFAULT 1,
                source_session_id TEXT NOT NULL DEFAULT '',
                source_trigger TEXT NOT NULL DEFAULT '',
                use_count INTEGER NOT NULL DEFAULT 0,
                last_used_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_skills_scope ON agent_skills(client_id, workspace_id)")
            .execute(pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_skills_source ON agent_skills(source_session_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_skills_enabled ON agent_skills(enabled)")
            .execute(pool)
            .await?;

        Self::migrate_agent_messages_v2(pool).await?;
        Self::migrate_agent_workspaces_v2(pool).await?;
        Self::migrate_agent_workspaces_v3(pool).await?;
        Self::migrate_agent_workspaces_v4(pool).await?;
        Self::migrate_agent_workspaces_v5(pool).await?;
        Self::migrate_agent_sessions_v2(pool).await?;
        Self::migrate_agent_sessions_v3(pool).await?;
        Self::migrate_agent_sessions_add_distilled(pool).await?;
        Self::migrate_agent_memory_settings_add_skill_columns(pool).await?;
        Self::migrate_agent_messages_v3(pool).await?;
        Self::migrate_agent_messages_v4(pool).await?;

        // ============================================================
        // Agent roles table
        // ============================================================
        Self::migrate_agent_roles(pool).await?;
        Self::migrate_agent_sessions_add_role(pool).await?;
        Self::seed_builtin_roles(pool).await?;

        Ok(())
    }

    /// agent_memory_settings 补 Skill 库设置列（skill_enabled/skill_list_max）。
    /// 幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_memory_settings_add_skill_columns(
        pool: &Pool<Sqlite>,
    ) -> Result<(), sqlx::Error> {
        for (column, ddl) in [
            (
                "skill_enabled",
                "ALTER TABLE agent_memory_settings ADD COLUMN skill_enabled INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "skill_list_max",
                "ALTER TABLE agent_memory_settings ADD COLUMN skill_list_max INTEGER NOT NULL DEFAULT 20",
            ),
        ] {
            match sqlx::query(ddl).execute(pool).await {
                Ok(_) => {}
                Err(e) => {
                    if !e.to_string().contains("duplicate column") {
                        return Err(e);
                    }
                    tracing::debug!(column, "agent_memory_settings migration: column already exists");
                }
            }
        }
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

    /// agent_sessions 补全 ACP 会话配置状态列（JSON map：config_id → value）。
    /// 幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_sessions_v2(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE agent_sessions ADD COLUMN config_state TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
                tracing::debug!("agent_sessions migration: column already exists");
            }
        }
        Ok(())
    }

    /// agent_sessions 补全 ACP 会话 id 列（agent 侧持久化会话的 session_id，
    /// 供断线重拉时 session/resume 恢复上下文）。幂等：列已存在时跳过。
    async fn migrate_agent_sessions_v3(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE agent_sessions ADD COLUMN acp_session_id TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
                tracing::debug!("agent_sessions migration: column already exists");
            }
        }
        Ok(())
    }

    /// agent_sessions 补蒸馏防重列（distilled=1 表示该会话已完成蒸馏）。原子 CAS
    /// （`memory_mark_distilled_if_not`）在归档/删除/断线/idle 多路并发中保证唯一
    /// 赢家触发蒸馏。幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_sessions_add_distilled(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query(
            "ALTER TABLE agent_sessions ADD COLUMN distilled INTEGER NOT NULL DEFAULT 0",
        )
        .execute(pool)
        .await
        {
            Ok(_) => {}
            Err(e) => {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
                tracing::debug!("agent_sessions migration: column already exists");
            }
        }
        Ok(())
    }

    /// agent_workspaces 补全 ACP 远程 agent 列（agent_type/agent_path/llm_model_id）。
    /// 幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_workspaces_v3(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        for (column, ddl) in [
            (
                "agent_type",
                "ALTER TABLE agent_workspaces ADD COLUMN agent_type TEXT NOT NULL DEFAULT ''",
            ),
            (
                "agent_path",
                "ALTER TABLE agent_workspaces ADD COLUMN agent_path TEXT",
            ),
            (
                "llm_model_id",
                "ALTER TABLE agent_workspaces ADD COLUMN llm_model_id TEXT",
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

    /// agent_messages 去重迁移 v3：历史版本对每个 ACP 事件纯 INSERT，同一
    /// (session_id, tool_call_id, kind) 会产生多行（tool_call 每次事件、tool_result
    /// 每个中间态各一行），前端刷新后重复卡片。按分组收敛为一行：
    /// `tool_result` 保留「content 非空中 rowid 最大者」，全空则保留 rowid 最大者；
    /// `tool_calls` 保留 length(tool_calls) 最大者（并列取 rowid 最大）。
    /// 事务内执行，幂等（去重后分组 COUNT 均 ≤ 1，重复运行无变化）。不做唯一索引
    /// 收敛：compact.rs 压缩时会带相同 (session_id, tool_call_id, kind) 重插 kept
    /// 段，唯一索引会直接冲突报错。
    async fn migrate_agent_messages_v3(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        let dup_groups: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT session_id, tool_call_id, kind FROM agent_messages \
             WHERE kind IN ('tool_calls', 'tool_result') \
               AND tool_call_id IS NOT NULL AND tool_call_id != '' \
             GROUP BY session_id, tool_call_id, kind \
             HAVING COUNT(*) > 1",
        )
        .fetch_all(pool)
        .await?;
        if dup_groups.is_empty() {
            return Ok(());
        }
        let mut tx = pool.begin().await?;
        for (session_id, tool_call_id, kind) in dup_groups {
            // 组内所有行（rowid 升序 = 插入顺序）
            let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
                "SELECT rowid, content, tool_calls FROM agent_messages \
                 WHERE session_id = ? AND tool_call_id = ? AND kind = ? ORDER BY rowid",
            )
            .bind(&session_id)
            .bind(&tool_call_id)
            .bind(&kind)
            .fetch_all(&mut *tx)
            .await?;
            let keep_rid = match kind.as_str() {
                // 保留规则：非空 content 中 rowid 最大者；全空则 rowid 最大者。
                "tool_result" => rows
                    .iter()
                    .filter(|(_, c, _)| !c.is_empty())
                    .map(|(rid, _, _)| *rid)
                    .max()
                    .or_else(|| rows.iter().map(|(rid, _, _)| *rid).max()),
                // 保留规则：length(tool_calls) 最大者，并列取 rowid 最大。
                "tool_calls" => rows
                    .iter()
                    .max_by(|a, b| {
                        a.2.as_deref()
                            .map(str::len)
                            .unwrap_or(0)
                            .cmp(&b.2.as_deref().map(str::len).unwrap_or(0))
                            .then(a.0.cmp(&b.0))
                    })
                    .map(|(rid, _, _)| *rid),
                _ => None,
            };
            if let Some(keep) = keep_rid {
                for (rid, _, _) in &rows {
                    if *rid != keep {
                        sqlx::query("DELETE FROM agent_messages WHERE rowid = ?")
                            .bind(rid)
                            .execute(&mut *tx)
                            .await?;
                    }
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// agent_messages 补全子 agent 归属列（claude-code-acp `_meta.claudeCode
    /// .parentToolUseId`：发起本消息的 Task 工具调用 id，主 agent 消息为 NULL）。
    /// 幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_messages_v4(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE agent_messages ADD COLUMN parent_tool_call_id TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
                tracing::debug!("agent_messages migration: column already exists");
            }
        }
        Ok(())
    }

    /// agent_workspaces 补全 ACP 引擎选项覆盖列（JSON map：config_id → value，
    /// 会话建立后经 set_config_option 注入 agent）。幂等：列已存在时跳过。
    async fn migrate_agent_workspaces_v4(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE agent_workspaces ADD COLUMN agent_config_overrides TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
                tracing::debug!("agent_workspaces migration: column already exists");
            }
        }
        Ok(())
    }

    /// agent_workspaces 补全 GitHub Actions 集成列：`github_token`（TEXT 可空，
    /// 存 LlmCipher 加密后的密文，绝不存明文）、`github_owner`/`github_repo`
    /// （TEXT 可空，手工填写的仓库定位，否则经隧道 `git remote get-url` 探测）。
    /// 幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_workspaces_v5(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        for (column, ddl) in [
            (
                "github_token",
                "ALTER TABLE agent_workspaces ADD COLUMN github_token TEXT",
            ),
            (
                "github_owner",
                "ALTER TABLE agent_workspaces ADD COLUMN github_owner TEXT",
            ),
            (
                "github_repo",
                "ALTER TABLE agent_workspaces ADD COLUMN github_repo TEXT",
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

    /// 建 `agent_roles` 表：可配置角色定义（系统提示词/工具过滤/模型覆盖/scope）。
    /// 幂等：CREATE IF NOT EXISTS。
    async fn migrate_agent_roles(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS agent_roles (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                system_prompt TEXT NOT NULL DEFAULT '',
                tools_allow TEXT,
                tools_deny TEXT,
                model_override TEXT,
                mode TEXT NOT NULL DEFAULT 'all',
                scope_type TEXT NOT NULL DEFAULT 'global',
                client_id TEXT NOT NULL DEFAULT '',
                workspace_id TEXT NOT NULL DEFAULT '',
                is_builtin INTEGER NOT NULL DEFAULT 0,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at DATETIME NOT NULL DEFAULT (datetime('now')),
                updated_at DATETIME NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // scope 内唯一索引
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_roles_name_scope \
             ON agent_roles(name, scope_type, client_id, workspace_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_roles_enabled ON agent_roles(enabled)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_roles_mode ON agent_roles(mode)",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// agent_sessions 补列 `role_id TEXT`（引用 agent_roles.id，应用层校验无 FK）。
    /// 幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_sessions_add_role(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE agent_sessions ADD COLUMN role_id TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
                tracing::debug!("agent_sessions migration: role_id column already exists");
            }
        }
        Ok(())
    }

    /// 插入内置角色（INSERT OR IGNORE 幂等）。general（全工具 subagent）、
    /// explore（只读 subagent）。
    async fn seed_builtin_roles(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        // general：现有 SUBAGENT_SYSTEM_PROMPT 内容，全工具
        sqlx::query(
            r#"INSERT OR IGNORE INTO agent_roles
               (id, name, description, system_prompt, mode, scope_type, is_builtin, enabled)
               VALUES (?, 'general', '通用子代理：全工具访问，适用于大多数任务',
                       'You are a helpful general-purpose AI assistant. Complete the user''s task thoroughly and accurately. Use the available tools to read, write, and modify files, run commands, and search the codebase as needed.',
                       'subagent', 'global', 1, 1)"#,
        )
        .bind("role-builtin-general-0000000000000000")
        .execute(pool)
        .await?;

        // explore：只读白名单（read_file 已含行区间参数，无需 read_file_range——
        // 后者是协议变体不是独立工具名）
        sqlx::query(
            r#"INSERT OR IGNORE INTO agent_roles
               (id, name, description, system_prompt, tools_allow, mode, scope_type, is_builtin, enabled)
               VALUES (?, 'explore', '只读探索代理：仅查看文件和搜索，不修改任何内容',
                       'You are a code exploration assistant. Analyze, read, and search code to answer questions. Do NOT modify any files. Report your findings clearly and concisely.',
                       '["read_file","list_dir","search","git_status","git_diff","git_log","git_show","git_branch","code_outline","read_symbol"]',
                       'subagent', 'global', 1, 1)"#,
        )
        .bind("role-builtin-explore-0000000000000000")
        .execute(pool)
        .await?;

        // 存量修正：早期 seed 的 explore 白名单含 read_file_range（协议变体而非
        // 独立工具名，schema 过滤永不命中），按 id 定点清除（用户可编辑过 prompt
        // 等字段，只规范 tools_allow）。
        sqlx::query(
            r#"UPDATE agent_roles
               SET tools_allow = '["read_file","list_dir","search","git_status","git_diff","git_log","git_show","git_branch","code_outline","read_symbol"]'
               WHERE id = 'role-builtin-explore-0000000000000000'
                 AND tools_allow LIKE '%read_file_range%'"#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Migrate old proxy_rules CHECK constraint to include 'llm'.
    /// Idempotent: 直接读 `sqlite_master` 里 proxy_rules 的建表 SQL，判断 CHECK 是否
    /// 已含 'llm'，已含则无操作。
    ///
    /// 不再用「插入探针行再删除」的写探测：WAL 模式下失败 INSERT（CHECK 拒绝）偶发
    /// 留下幽灵探针行，且只在表重建（DROP+RENAME）后才可见——`rules.len()` 偶得 3
    /// （`migration_rebuilds_proxy_rules_check_for_llm` flake，已在 -j 2 多跑复现：
    /// 探针失败/迁移拷贝均不含探针，`Database::new` 返回后探针行却出现在表里）。
    /// 纯读探测无写入、无残留，且语义更直接（直接看 CHECK 是否含 'llm'）。
    async fn migrate_proxy_rules_check_for_llm(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'proxy_rules'",
        )
        .fetch_optional(pool)
        .await?;
        let Some((ddl,)) = row else {
            return Ok(()); // 表不存在（全新库由 CREATE IF NOT EXISTS 用新 schema 建）
        };
        if ddl.contains("'llm'") {
            return Ok(()); // CHECK 已接受 'llm'：无操作
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

#[cfg(test)]
mod tests {
    /// Task 7：v3 迁移把 ACP 三列落到 agent_workspaces 后，直接 INSERT 新列应成功
    /// （证明列存在且约束正确：agent_type NOT NULL 默认空串、后两列可空）。
    #[tokio::test]
    async fn test_migrate_agent_workspaces_v3() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO agent_workspaces
                (id, name, client_id, runtime_type, root_path, agent_type, agent_path, llm_model_id)
             VALUES ('w1', 'test', 'c1', 'host', '/tmp', 'gemini', '/opt/acp-agent', 'm1')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // 幂等：再次跑迁移不报错（列已存在，跳过 ALTER）
        super::Database::initialize_schema(&pool).await.unwrap();

        let row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT agent_type, agent_path, llm_model_id FROM agent_workspaces WHERE id = 'w1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "gemini");
        assert_eq!(row.1.as_deref(), Some("/opt/acp-agent"));
        assert_eq!(row.2.as_deref(), Some("m1"));
    }

    /// v3 迁移：同一 (session_id, tool_call_id, kind) 多行收敛为一行，且幂等。
    /// tool_result 保留非空中 rowid 最大者；tool_calls 保留 length(tool_calls)
    /// 最大者；无关行（message / tool_call_id 为空）不参与去重。
    #[tokio::test]
    async fn test_migrate_agent_messages_v3_dedup() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO agent_workspaces (id, name, client_id, runtime_type, root_path) \
             VALUES ('w1', 'w', 'c1', 'host', '/p')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO agent_sessions (id, workspace_id) VALUES ('s1', 'w1')")
            .execute(&pool)
            .await
            .unwrap();

        // tool_result：两个中间态空 content + 一个终态非空
        for (id, content) in [("m1", ""), ("m2", ""), ("m3", "final output")] {
            sqlx::query(
                "INSERT INTO agent_messages (id, session_id, role, content, tool_call_id, name, kind) \
                 VALUES (?, 's1', 'assistant', ?, 'c1', 'shell', 'tool_result')",
            )
            .bind(id)
            .bind(content)
            .execute(&pool)
            .await
            .unwrap();
        }
        // tool_calls：短 JSON 先插、长 JSON 后插（保留长 JSON）
        sqlx::query(
            "INSERT INTO agent_messages (id, session_id, role, content, tool_calls, tool_call_id, name, kind) \
             VALUES ('m4', 's1', 'assistant', '', 'short', 'c2', 'Terminal', 'tool_calls')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_messages (id, session_id, role, content, tool_calls, tool_call_id, name, kind) \
             VALUES ('m5', 's1', 'assistant', '', 'a much longer tool_calls json with real arguments', 'c2', 'Terminal', 'tool_calls')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // 无关行：kind='message'（tool_call_id 为空）不参与去重
        sqlx::query(
            "INSERT INTO agent_messages (id, session_id, role, content, name, kind) \
             VALUES ('m6', 's1', 'user', 'hi', NULL, 'message')",
        )
        .execute(&pool)
        .await
        .unwrap();

        super::Database::migrate_agent_messages_v3(&pool)
            .await
            .unwrap();

        let (count, content): (i64, String) = sqlx::query_as(
            "SELECT COUNT(*), MAX(content) FROM agent_messages \
             WHERE kind = 'tool_result' AND tool_call_id = 'c1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "tool_result should converge to one row");
        assert_eq!(content, "final output");

        let (count, tc): (i64, String) = sqlx::query_as(
            "SELECT COUNT(*), MAX(tool_calls) FROM agent_messages \
             WHERE kind = 'tool_calls' AND tool_call_id = 'c2'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "tool_calls should converge to one row");
        assert_eq!(
            tc, "a much longer tool_calls json with real arguments",
            "tool_calls should keep the longest json"
        );

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_messages")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 3, "converged rows + unrelated row: {total}");

        // 幂等：再跑一次结果一致
        super::Database::migrate_agent_messages_v3(&pool)
            .await
            .unwrap();
        let total2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_messages")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total2, 3, "migration must be idempotent");
    }

    /// v5 迁移：github_token/github_owner/github_repo 三列落到 agent_workspaces 后
    /// 直接 INSERT 应成功（三列均可空），且重复跑迁移幂等（列已存在时跳过 ALTER）。
    #[tokio::test]
    async fn test_migrate_agent_workspaces_v5() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();

        // 三列直接 INSERT（新库 schema 直接建列，旧库走 ALTER，两条路径都应可写）
        sqlx::query(
            "INSERT INTO agent_workspaces
                (id, name, client_id, runtime_type, root_path,
                 github_token, github_owner, github_repo)
             VALUES ('w1', 'test', 'c1', 'host', '/tmp',
                     'enc:v1:ciphertext', 'octo', 'repo')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // 幂等：再次跑初始化不报错（列已存在，跳过 ALTER）
        super::Database::initialize_schema(&pool).await.unwrap();

        let row: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT github_token, github_owner, github_repo FROM agent_workspaces WHERE id = 'w1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0.as_deref(), Some("enc:v1:ciphertext"));
        assert_eq!(row.1.as_deref(), Some("octo"));
        assert_eq!(row.2.as_deref(), Some("repo"));

        // 三列默认 NULL（未显式提供时 INSERT 成功）
        sqlx::query(
            "INSERT INTO agent_workspaces (id, name, client_id, runtime_type, root_path)
             VALUES ('w2', 'test', 'c1', 'host', '/tmp')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let nulls: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT github_token, github_owner, github_repo FROM agent_workspaces WHERE id = 'w2'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(nulls.0.is_none() && nulls.1.is_none() && nulls.2.is_none());
    }

    /// 未显式提供 agent_type 时走列默认空串，INSERT 仍应成功（NOT NULL 约束满足）。
    #[tokio::test]
    async fn test_agent_workspaces_default_agent_type() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO agent_workspaces (id, name, client_id, runtime_type, root_path)
             VALUES ('w2', 'test', 'c1', 'host', '/tmp')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let agent_type: String =
            sqlx::query_scalar("SELECT agent_type FROM agent_workspaces WHERE id = 'w2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(agent_type, "");
    }

    /// AI 记忆体 schema：agent_sessions 蒸馏防重列迁移幂等（initialize_schema
    /// 调两次不报错、列存在且默认 0）；agent_memory_settings 单行 CHECK 约束生效；
    /// agent_memories 建表且默认值正确。
    #[tokio::test]
    async fn test_ai_memory_schema_and_distilled_migration() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();

        // 迁移幂等：再次初始化不报错（distilled 列已存在，ALTER 跳过）
        super::Database::initialize_schema(&pool).await.unwrap();

        // agent_sessions.distilled 列存在且默认 0
        sqlx::query(
            "INSERT INTO agent_workspaces (id, name, client_id, runtime_type, root_path) \
             VALUES ('w1', 'w', 'c1', 'host', '/p')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO agent_sessions (id, workspace_id) VALUES ('s1', 'w1')")
            .execute(&pool)
            .await
            .unwrap();
        let distilled: i64 =
            sqlx::query_scalar("SELECT distilled FROM agent_sessions WHERE id = 's1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(distilled, 0, "distilled 列应存在且默认 0");

        // agent_memory_settings：单行 id=1 约束（非 1 拒绝），其余列默认值落库
        sqlx::query("INSERT INTO agent_memory_settings (id) VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();
        let err = sqlx::query("INSERT INTO agent_memory_settings (id) VALUES (2)")
            .execute(&pool)
            .await;
        assert!(err.is_err(), "id != 1 应被 CHECK 约束拒绝");
        let (enabled, top_k, budget): (i64, i64, i64) = sqlx::query_as(
            "SELECT enabled, top_k, inject_budget_tokens FROM agent_memory_settings WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((enabled, top_k, budget), (0, 8, 1500));

        // agent_memories：默认值正确
        sqlx::query("INSERT INTO agent_memories (id, content) VALUES ('m1', 'fact')")
            .execute(&pool)
            .await
            .unwrap();
        let (scope, pin, conf, trigger): (String, i64, f64, String) = sqlx::query_as(
            "SELECT scope_type, pinned, confidence, source_trigger FROM agent_memories \
             WHERE id = 'm1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(scope, "workspace");
        assert_eq!(pin, 0);
        assert_eq!(conf, 0.8);
        assert_eq!(trigger, "");

        // 索引存在
        for idx in [
            "idx_memories_scope",
            "idx_memories_source",
            "idx_memories_pinned",
        ] {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
            )
            .bind(idx)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(exists, 1, "索引 {idx} 应存在");
        }
    }

    /// Skill 库 schema：agent_skills 建表默认值 + 三索引；agent_memory_settings
    /// 两列（skill_enabled=0/skill_list_max=20）幂等迁移（初始化两次不报错）。
    #[tokio::test]
    async fn test_skill_schema_and_settings_migration() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();
        // 幂等：再次初始化（skill_* 列已存在，ALTER 跳过）
        super::Database::initialize_schema(&pool).await.unwrap();

        // agent_skills 默认值
        sqlx::query("INSERT INTO agent_skills (id, name, content) VALUES ('s1', 'deploy', '## 步骤')")
            .execute(&pool)
            .await
            .unwrap();
        let (scope, enabled, use_count, trigger): (String, i64, i64, String) = sqlx::query_as(
            "SELECT scope_type, enabled, use_count, source_trigger FROM agent_skills WHERE id = 's1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(scope, "workspace");
        assert_eq!(enabled, 1);
        assert_eq!(use_count, 0);
        assert_eq!(trigger, "");

        // settings 新列默认值
        sqlx::query("INSERT INTO agent_memory_settings (id) VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();
        let (skill_enabled, skill_list_max): (i64, i64) = sqlx::query_as(
            "SELECT skill_enabled, skill_list_max FROM agent_memory_settings WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(skill_enabled, 0, "skill_enabled 默认 0（opt-in）");
        assert_eq!(skill_list_max, 20, "skill_list_max 默认 20");

        // 三索引存在
        for idx in ["idx_skills_scope", "idx_skills_source", "idx_skills_enabled"] {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
            )
            .bind(idx)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(exists, 1, "索引 {idx} 应存在");
        }
    }

    /// agent_roles schema：建表 + 内置角色 seed + 幂等迁移 + 唯一索引 + role_id 列。
    #[tokio::test]
    async fn test_agent_roles_schema_and_seed() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();

        // 幂等：再次初始化不报错
        super::Database::initialize_schema(&pool).await.unwrap();

        // 内置角色 seed：general 和 explore
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_roles WHERE is_builtin = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2, "应有 2 个内置角色");

        let general: (String, String, Option<String>) = sqlx::query_as(
            "SELECT name, mode, tools_allow FROM agent_roles WHERE name = 'general'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(general.0, "general");
        assert_eq!(general.1, "subagent");
        assert!(general.2.is_none(), "general 无工具白名单（全工具）");

        let explore: (String, String, Option<String>) = sqlx::query_as(
            "SELECT name, mode, tools_allow FROM agent_roles WHERE name = 'explore'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(explore.1, "subagent");
        assert!(
            explore.2.as_deref().unwrap_or("").contains("read_file"),
            "explore 应有只读白名单"
        );

        // agent_sessions.role_id 列存在（默认 NULL）
        sqlx::query(
            "INSERT INTO agent_workspaces (id, name, client_id, runtime_type, root_path) \
             VALUES ('w1', 'w', 'c1', 'host', '/p')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO agent_sessions (id, workspace_id) VALUES ('s1', 'w1')")
            .execute(&pool)
            .await
            .unwrap();
        let role_id: Option<String> =
            sqlx::query_scalar("SELECT role_id FROM agent_sessions WHERE id = 's1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(role_id.is_none(), "role_id 默认 NULL");

        // 唯一索引：同名同 scope 插入第二个应失败
        let err = sqlx::query(
            "INSERT INTO agent_roles (id, name, scope_type, client_id, workspace_id) \
             VALUES ('dup', 'general', 'global', '', '')",
        )
        .execute(&pool)
        .await;
        assert!(err.is_err(), "同名同 scope 应违反唯一索引");

        // 不同 scope 可以同名
        sqlx::query(
            "INSERT INTO agent_roles (id, name, scope_type, client_id, workspace_id) \
             VALUES ('custom', 'general', 'client', 'c1', '')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // 索引存在
        for idx in [
            "idx_roles_name_scope",
            "idx_roles_enabled",
            "idx_roles_mode",
        ] {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
            )
            .bind(idx)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(exists, 1, "索引 {idx} 应存在");
        }
    }
}
