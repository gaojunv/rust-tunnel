use super::Database;
use sqlx::{Pool, Sqlite};

impl Database {
    #[allow(clippy::too_many_lines, reason = "建表与幂等迁移的顺序编排，需按依赖集中初始化，拆分会分散约束")]
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
            sqlx::query(&format!("DROP TABLE IF EXISTS {tbl}"))
                .execute(pool)
                .await?;
        }

        // Client session history
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS client_sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL,
                hostname TEXT,
                connected_at DATETIME NOT NULL,
                disconnected_at DATETIME,
                duration_seconds INTEGER
            )
            ",
        )
        .execute(pool)
        .await?;

        // Create indexes
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_client_sessions_port ON client_sessions(port)")
            .execute(pool)
            .await?;
        // Shadowsocks configuration table
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS shadowsocks_config (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL UNIQUE,
                cipher TEXT NOT NULL,
                password TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL
            )
            ",
        )
        .execute(pool)
        .await?;

        // Trojan configuration table
        sqlx::query(
            r"
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
            ",
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
            r"
            CREATE TABLE IF NOT EXISTS server_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                level TEXT NOT NULL,
                source TEXT NOT NULL,
                target TEXT NOT NULL,
                message TEXT NOT NULL
            )
            ",
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
            r"
            CREATE TABLE IF NOT EXISTS mesh_networks (
                id TEXT PRIMARY KEY,
                created_at DATETIME NOT NULL,
                description TEXT
            )
            ",
        )
        .execute(pool)
        .await?;

        // Mesh services table
        sqlx::query(
            r"
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
            ",
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
            r"
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
            ",
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
            r"
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
            ",
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
            r"
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
            ",
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
            r"
            CREATE TABLE IF NOT EXISTS acme_challenges (
                token TEXT PRIMARY KEY,
                domain TEXT NOT NULL,
                authorization TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending', 'valid', 'invalid')),
                created_at DATETIME NOT NULL,
                expires_at DATETIME
            )
            ",
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
            r"
            CREATE TABLE IF NOT EXISTS reverse_proxy_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                max_connections INTEGER NOT NULL DEFAULT 10000,
                connection_timeout_secs INTEGER NOT NULL DEFAULT 30,
                buffer_size INTEGER NOT NULL DEFAULT 8192,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await?;

        // DNS config (singleton)
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS dns_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                tunnel_domain TEXT NOT NULL DEFAULT 'tunnel.local',
                mesh_domain TEXT NOT NULL DEFAULT 'mesh.local',
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await?;

        // General server settings (key-value)
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS server_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await?;

        // Client registry table (spec §2.1)
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS clients (
                name          TEXT PRIMARY KEY,
                hostname      TEXT,
                first_seen_at DATETIME NOT NULL,
                last_seen_at  DATETIME NOT NULL,
                note          TEXT
            )
            ",
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_clients_last_seen ON clients(last_seen_at)")
            .execute(pool)
            .await?;

        // Single-row server auth table (spec §2.2)
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS server_auth (
                id           INTEGER PRIMARY KEY CHECK(id = 1),
                client_token TEXT NOT NULL,
                updated_at   DATETIME NOT NULL
            )
            ",
        )
        .execute(pool)
        .await?;

        // ============================================================
        // LLM Gateway tables
        // ============================================================

        // LLM providers table
        sqlx::query(
            r"
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
            ",
        )
        .execute(pool)
        .await?;

        // LLM models table
        sqlx::query(
            r"
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
            ",
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

        // Unified knowledge sources (replaces rag_knowledge_bases / agent_wikis)
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS knowledge_sources (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                summary TEXT NOT NULL DEFAULT '',
                index_vector INTEGER NOT NULL DEFAULT 0,
                index_pages INTEGER NOT NULL DEFAULT 0,
                scope_type TEXT NOT NULL DEFAULT 'global',
                client_id TEXT NOT NULL DEFAULT '',
                workspace_id TEXT NOT NULL DEFAULT '',
                emb_base_url TEXT NOT NULL DEFAULT '',
                emb_api_key TEXT NOT NULL DEFAULT '',
                emb_model TEXT NOT NULL DEFAULT '',
                emb_dimension INTEGER NOT NULL DEFAULT 0,
                top_k INTEGER NOT NULL DEFAULT 5,
                chunk_size INTEGER NOT NULL DEFAULT 512,
                chunk_overlap INTEGER NOT NULL DEFAULT 64,
                score_threshold REAL NOT NULL DEFAULT 0.3,
                status TEXT NOT NULL DEFAULT 'draft' CHECK(status IN ('draft','pending','processing','ready','failed')),
                version INTEGER NOT NULL DEFAULT 1,
                page_count INTEGER NOT NULL DEFAULT 0,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_ks_name_scope \
             ON knowledge_sources(name, scope_type, client_id, workspace_id) WHERE index_pages = 1",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_ks_scope ON knowledge_sources(client_id, workspace_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_ks_index ON knowledge_sources(index_vector, index_pages)")
            .execute(pool)
            .await?;

        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS knowledge_docs (
                id TEXT PRIMARY KEY,
                source_id TEXT NOT NULL REFERENCES knowledge_sources(id) ON DELETE CASCADE,
                filename TEXT NOT NULL,
                file_type TEXT NOT NULL DEFAULT '',
                content_hash TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_kdocs_source ON knowledge_docs(source_id)")
            .execute(pool)
            .await?;

        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS knowledge_doc_index (
                doc_id TEXT NOT NULL REFERENCES knowledge_docs(id) ON DELETE CASCADE,
                kind TEXT NOT NULL CHECK(kind IN ('vector','pages')),
                status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','processing','ready','failed')),
                item_count INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (doc_id, kind)
            )
            ",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_kdi_status ON knowledge_doc_index(kind, status)",
        )
        .execute(pool)
        .await?;

        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS knowledge_chunks (
                id TEXT PRIMARY KEY,
                doc_id TEXT NOT NULL REFERENCES knowledge_docs(id) ON DELETE CASCADE,
                source_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                heading_path TEXT NOT NULL DEFAULT '',
                content TEXT NOT NULL,
                token_count INTEGER NOT NULL DEFAULT 0
            )
            ",
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_kchunks_doc ON knowledge_chunks(doc_id)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_kchunks_source ON knowledge_chunks(source_id)")
            .execute(pool)
            .await?;

        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS knowledge_pages (
                id TEXT PRIMARY KEY,
                source_id TEXT NOT NULL REFERENCES knowledge_sources(id) ON DELETE CASCADE,
                ref TEXT NOT NULL,
                title TEXT NOT NULL DEFAULT '',
                summary TEXT NOT NULL DEFAULT '',
                content TEXT NOT NULL,
                locked INTEGER NOT NULL DEFAULT 0,
                source_doc_id TEXT REFERENCES knowledge_docs(id) ON DELETE SET NULL,
                use_count INTEGER NOT NULL DEFAULT 0,
                last_used_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(source_id, ref)
            )
            ",
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_kpages_source ON knowledge_pages(source_id)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_kpages_ref ON knowledge_pages(ref)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_kpages_locked ON knowledge_pages(locked)")
            .execute(pool)
            .await?;

        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS knowledge_page_edges (
                source_id TEXT NOT NULL REFERENCES knowledge_sources(id) ON DELETE CASCADE,
                src_page_id TEXT NOT NULL REFERENCES knowledge_pages(id) ON DELETE CASCADE,
                src_ref TEXT NOT NULL,
                dst_ref TEXT NOT NULL,
                dst_page_id TEXT REFERENCES knowledge_pages(id) ON DELETE SET NULL,
                PRIMARY KEY (source_id, src_page_id, dst_ref)
            )
            ",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_kedges_source ON knowledge_page_edges(source_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_kedges_src ON knowledge_page_edges(src_page_id)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_kedges_dst ON knowledge_page_edges(dst_page_id)",
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE VIRTUAL TABLE IF NOT EXISTS knowledge_pages_fts USING fts5(ref, title, summary, content, tokenize='trigram')",
        )
        .execute(pool)
        .await?;

        // LLM API keys table (gateway-level keys for external callers)
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS llm_api_keys (
                id TEXT PRIMARY KEY,
                key_hash TEXT NOT NULL UNIQUE,
                key_prefix TEXT NOT NULL,
                name TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_used_at TEXT,
                kb_id TEXT REFERENCES knowledge_sources(id) ON DELETE SET NULL
            )
            ",
        )
        .execute(pool)
        .await?;

        // Migration: old DBs lack anthropic_base_url column on llm_providers.
        Self::migrate_llm_providers_add_anthropic_url(pool).await?;

        // LLM usage logs table — one row per gateway request (usage stats).
        // 冗余存 *_name 列，使 Key/模型/供应商删除后历史记录仍可读。
        sqlx::query(
            r"
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
            ",
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
            r"
            CREATE TABLE IF NOT EXISTS llm_model_groups (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await?;

        // 组成员：一组多模型，priority 升序即故障转移尝试顺序
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS llm_model_group_members (
                group_id TEXT NOT NULL REFERENCES llm_model_groups(id) ON DELETE CASCADE,
                model_id TEXT NOT NULL REFERENCES llm_models(id) ON DELETE CASCADE,
                priority INTEGER NOT NULL,
                PRIMARY KEY (group_id, model_id)
            )
            ",
        )
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
            r"
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
            ",
        )
        .execute(pool)
        .await?;

        // Agent sessions
        sqlx::query(
            r"
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
            ",
        )
        .execute(pool)
        .await?;

        // Agent messages
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS agent_messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES agent_sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                tool_calls TEXT,
                created_at DATETIME NOT NULL DEFAULT (datetime('now'))
            )
            ",
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
            r"
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
                wiki_enabled INTEGER NOT NULL DEFAULT 1,
                wiki_list_max INTEGER NOT NULL DEFAULT 20,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await?;

        // AI 记忆体主表：原子事实（≤2KB），作用域隔离（global|client|workspace）。
        // 向量本体在 `<data_dir>/rag/memory/`（kb_id 常量 "memory"），ChunkPoint 的
        // id 与 doc_id 均取本表 id（删除走 delete_by_doc("memory", dim, memory_id)）。
        sqlx::query(
            r"
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
            ",
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
            r"
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
            ",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_skills_scope ON agent_skills(client_id, workspace_id)",
        )
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
        Self::migrate_agent_workspaces_v6(pool).await?;
        Self::migrate_agent_sessions_v2(pool).await?;
        Self::migrate_agent_sessions_v3(pool).await?;
        Self::migrate_agent_sessions_add_distilled(pool).await?;
        Self::migrate_agent_memory_settings_add_skill_columns(pool).await?;
        Self::migrate_agent_memory_settings_add_wiki_columns(pool).await?;
        Self::migrate_agent_messages_v3(pool).await?;
        Self::migrate_agent_messages_v4(pool).await?;

        // ============================================================
        // Agent roles table
        // ============================================================
        Self::migrate_agent_roles(pool).await?;
        Self::migrate_agent_sessions_add_role(pool).await?;
        Self::migrate_agent_sessions_add_context_usage(pool).await?;
        Self::migrate_agent_sessions_add_spawn_error(pool).await?;
        Self::migrate_agent_pending_prompts(pool).await?;
        Self::seed_builtin_roles(pool).await?;

        // 必须在最后：依赖 knowledge_* 新表已建，且 rag_documents 旧表若存在时先让
        // migrate_rag_documents_add_file_type 完成旧表的 file_type 回填，再统一迁移
        Self::migrate_unify_knowledge_sources(pool).await?;

        Ok(())
    }

    /// ACP 排队 prompt 持久化表：busy 时入队的消息落库（内存 VecDeque 仅作热
    /// 缓存），服务端重启/reaper 回收后 ensure_session 可从 DB 恢复 FIFO 队列。
    /// 取出执行即删行（消息本身已作为 user 消息落 agent_messages，不丢历史）。
    async fn migrate_agent_pending_prompts(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS agent_pending_prompts (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                content TEXT NOT NULL,
                refs TEXT NOT NULL DEFAULT '[]',
                created_at DATETIME NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_agent_pending_prompts_session
             ON agent_pending_prompts(session_id)",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// agent_sessions 补 `last_spawn_error` 列（最近一次 ACP spawn 失败的归因
    /// 描述，成功时清空；供重启后/会话列表追溯）。幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_sessions_add_spawn_error(
        pool: &Pool<Sqlite>,
    ) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE agent_sessions ADD COLUMN last_spawn_error TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
                tracing::debug!("agent_sessions migration: last_spawn_error column already exists");
            }
        }
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

    /// `agent_memory_settings` 补 Wiki 设置列（`wiki_enabled` 默认 1、`wiki_list_max` 默认 20）。
    /// 幂等：列已存在时 ALTER 报错即跳过（照技能列迁移模式）。
    async fn migrate_agent_memory_settings_add_wiki_columns(
        pool: &Pool<Sqlite>,
    ) -> Result<(), sqlx::Error> {
        for (column, ddl) in [
            (
                "wiki_enabled",
                "ALTER TABLE agent_memory_settings ADD COLUMN wiki_enabled INTEGER NOT NULL DEFAULT 1",
            ),
            (
                "wiki_list_max",
                "ALTER TABLE agent_memory_settings ADD COLUMN wiki_list_max INTEGER NOT NULL DEFAULT 20",
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
                            .map_or(0, str::len)
                            .cmp(&b.2.as_deref().map_or(0, str::len))
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

    /// agent_workspaces 补全 Claude Code tier 模型映射列：`claude_tier_models`
    ///（TEXT 可空，JSON object：key ∈ {opus,sonnet,haiku}，值为模型引用
    /// `model:<id>`/`group:<id>`/裸别名）。幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_workspaces_v6(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE agent_workspaces ADD COLUMN claude_tier_models TEXT")
            .execute(pool)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
                tracing::debug!(
                    "agent_workspaces migration: claude_tier_models column already exists"
                );
            }
        }
        Ok(())
    }

    /// 建 `agent_roles` 表：可配置角色定义（系统提示词/工具过滤/模型覆盖/scope）。
    /// 幂等：CREATE IF NOT EXISTS。
    async fn migrate_agent_roles(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
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
            ",
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
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_roles_enabled ON agent_roles(enabled)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_roles_mode ON agent_roles(mode)")
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

    /// agent_sessions 补上下文用量列 `context_used`/`context_size`（ACP UsageUpdate
    /// 最近一次快照；用于刷新/重连后恢复用量条）。幂等：列已存在时 ALTER 报错即跳过。
    async fn migrate_agent_sessions_add_context_usage(
        pool: &Pool<Sqlite>,
    ) -> Result<(), sqlx::Error> {
        for col in ["context_used INTEGER", "context_size INTEGER"] {
            match sqlx::query(&format!("ALTER TABLE agent_sessions ADD COLUMN {col}"))
                .execute(pool)
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    if !e.to_string().contains("duplicate column") {
                        return Err(e);
                    }
                    tracing::debug!("agent_sessions migration: {col} column already exists");
                }
            }
        }
        Ok(())
    }

    /// 插入内置角色（INSERT OR IGNORE 幂等）。general（全工具 subagent）、
    /// explore（只读 subagent）。
    async fn seed_builtin_roles(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        // general：现有 SUBAGENT_SYSTEM_PROMPT 内容，全工具
        sqlx::query(
            r"INSERT OR IGNORE INTO agent_roles
               (id, name, description, system_prompt, mode, scope_type, is_builtin, enabled)
               VALUES (?, 'general', '通用子代理：全工具访问，适用于大多数任务',
                       'You are a helpful general-purpose AI assistant. Complete the user''s task thoroughly and accurately. Use the available tools to read, write, and modify files, run commands, and search the codebase as needed.',
                       'subagent', 'global', 1, 1)",
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
            r"
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
            ",
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
        match sqlx::query("ALTER TABLE llm_api_keys ADD COLUMN kb_id TEXT REFERENCES knowledge_sources(id) ON DELETE SET NULL")
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
    /// 与 `Database::kdoc_backfill_file_type`（`knowledge.rs`）共享，避免两份 SQL 漂移。
    pub(crate) const BACKFILL_RAG_DOCUMENT_FILE_TYPE_SQL: &str =
        "UPDATE rag_documents SET file_type = 'md' WHERE file_type = ''";

    /// 统一回填 SQL（新表 `knowledge_docs`）：与上方 `BACKFILL_RAG_DOCUMENT_FILE_TYPE_SQL`
    /// 同语义，**同样绝不能改成按 filename 扩展名推导**（理由见上方注释：会孤儿化真实
    /// 的 `.md` 原文）。只作用于 legacy 空值——新表两侧的 file_type 都由迁移原值带过
    /// （`rag_documents` 与 `agent_wiki_docs` 都有该列），正常写入路径也必填。
    pub(crate) const BACKFILL_KNOWLEDGE_DOC_FILE_TYPE_SQL: &str =
        "UPDATE knowledge_docs SET file_type = 'md' WHERE file_type = ''";

    /// Migration: old DBs lack `file_type` on `rag_documents`. Idempotent.
    /// 列添加成功后在同一函数内回填老数据为 'md'（老数据落盘一律 .md，见上方常量注释）。
    /// 新库已无 `rag_documents`，此迁移为 no-op（忽略 no such table）。
    async fn migrate_rag_documents_add_file_type(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        match sqlx::query("ALTER TABLE rag_documents ADD COLUMN file_type TEXT NOT NULL DEFAULT ''")
            .execute(pool)
            .await
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column") => {}
            Err(e) if e.to_string().contains("no such table") => return Ok(()),
            Err(e) => return Err(e),
        }
        // rag_documents 可能已被统一迁移删除（新库无表），忽略错误
        let res = sqlx::query(Self::BACKFILL_RAG_DOCUMENT_FILE_TYPE_SQL)
            .execute(pool)
            .await;
        if let Err(e) = res {
            if !e.to_string().contains("no such table") {
                return Err(e);
            }
        }
        Ok(())
    }

    /// 迁移：把 `rag_knowledge_bases` / `rag_documents` / `rag_chunks`（vector
    /// 索引）与 `agent_wikis` / `agent_wiki_docs` / `agent_wiki_pages` /
    /// `agent_wiki_edges` / `agent_wiki_pages_fts`（pages 索引）统一到
    /// `knowledge_sources` / `knowledge_docs` / `knowledge_doc_index` /
    /// `knowledge_chunks` / `knowledge_pages` / `knowledge_page_edges` /
    /// `knowledge_pages_fts`。
    ///
    /// 幂等：纯读探测 `rag_knowledge_bases` 与 `agent_wikis` 是否存在，两者都
    /// 不存在则为新库或已迁库，无操作。单事务（同一连接）完成，避免
    /// 跨连接死锁。CTAS 备份旧表（不改写 FK），显式带 rowid 搬 pages 保 FTS
    /// 一致。DROP 先引用方后被引用方。
    async fn migrate_unify_knowledge_sources(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        // 幂等检测：纯读探测旧表是否存在
        let rag_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='rag_knowledge_bases'",
        )
        .fetch_one(pool)
        .await?;
        let wiki_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_wikis'",
        )
        .fetch_one(pool)
        .await?;
        if rag_exists == 0 && wiki_exists == 0 {
            return Ok(());
        }

        // 迁移内将 DROP 被引用的父表（rag_knowledge_bases 被 llm_api_keys.kb_id 以 ON DELETE SET NULL 引用）。
        // 若保持 FK 开启，DROP 父表会把子表的 kb_id 置 NULL，静默丢失绑定关系。
        // PRAGMA foreign_keys 在事务内修改是 no-op（SQLite 约束），必须在 BEGIN 之前关闭。
        // 用裸连接（pool.acquire）而非 pool.begin()，以便在事务外执行 PRAGMA。
        let mut conn = pool.acquire().await?;
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await?;

        // 主体单独成函数：主体里每个 `?` 都是提前返回。若内联在这里，失败路径会跳过
        // 下面的 `PRAGMA foreign_keys = ON`，使这条连接带着「FK 检查已关闭」的状态回到
        // 连接池——之后任何拿到它的业务查询都失去外键保护（静默的完整性风险）。
        // 目前迁移失败会让 Database::new 报错并终止启动（server.rs 用 `?` 传播），
        // 但不能依赖这个间接保证。
        let result = Self::unify_knowledge_body(&mut conn, rag_exists, wiki_exists).await;
        if result.is_err() {
            // 事务未 COMMIT：显式回滚，不要把一个打开的事务归还给连接池
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        }
        // 无论成败都恢复。此时事务已结束，PRAGMA 不再是 no-op
        let _ = sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&mut *conn)
            .await;
        result
    }

    /// `migrate_unify_knowledge_sources` 的事务主体。
    /// 调用方负责 FK 开关、回滚与连接归还，本函数只管在事务内搬数据。
    // too_many_lines: 一次性表合并迁移，主体是 8 组线性的 INSERT ... SELECT + CTAS 备份 +
    // DROP，彼此顺序强耦合（先灌新表、再备份、最后按引用顺序 DROP）。拆成子函数只会把
    // 必须连读的顺序打散，且每段都要重复传递 7 个 *_exists 标志，可读性反而更差。
    #[allow(clippy::too_many_lines)]
    async fn unify_knowledge_body(
        conn: &mut sqlx::sqlite::SqliteConnection,
        rag_exists: i64,
        wiki_exists: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("BEGIN").execute(&mut *conn).await?;

        // ── 1. knowledge_sources ← rag_knowledge_bases (index_vector=1) ──
        if rag_exists != 0 {
            sqlx::query(
                r"
                INSERT INTO knowledge_sources
                    (id, name, summary, index_vector, index_pages,
                     scope_type, client_id, workspace_id,
                     emb_base_url, emb_api_key, emb_model, emb_dimension,
                     top_k, chunk_size, chunk_overlap, score_threshold,
                     status, version, page_count, enabled, created_at, updated_at)
                SELECT id, name, description, 1, 0,
                       'global', '', '',
                       emb_base_url, emb_api_key, emb_model, emb_dimension,
                       top_k, chunk_size, chunk_overlap, score_threshold,
                       'ready', 1, 0, enabled, created_at, updated_at
                FROM rag_knowledge_bases
                ",
            )
            .execute(&mut *conn)
            .await?;
        }

        // ── 2. knowledge_sources ← agent_wikis (index_pages=1) ──
        if wiki_exists != 0 {
            sqlx::query(
                r"
                INSERT INTO knowledge_sources
                    (id, name, summary, index_vector, index_pages,
                     scope_type, client_id, workspace_id,
                     emb_base_url, emb_api_key, emb_model, emb_dimension,
                     top_k, chunk_size, chunk_overlap, score_threshold,
                     status, version, page_count, enabled, created_at, updated_at)
                SELECT id, name, summary, 0, 1,
                       scope_type, client_id, workspace_id,
                       '', '', '', 0,
                       5, 512, 64, 0.3,
                       status, version, page_count, 1, created_at, updated_at
                FROM agent_wikis
                ",
            )
            .execute(&mut *conn)
            .await?;
        }

        // ── 3. knowledge_docs ← rag_documents ──
        let rag_docs_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='rag_documents'",
        )
        .fetch_one(&mut *conn)
        .await?;
        if rag_docs_exists != 0 {
            sqlx::query(
                r"
                INSERT INTO knowledge_docs (id, source_id, filename, file_type, content_hash, created_at, updated_at)
                SELECT id, kb_id, filename, file_type, content_hash, created_at, updated_at FROM rag_documents
                ",
            )
            .execute(&mut *conn)
            .await?;
            // knowledge_doc_index(kind='vector') ← rag_documents
            sqlx::query(
                r"
                INSERT INTO knowledge_doc_index (doc_id, kind, status, item_count, error, updated_at)
                SELECT id, 'vector', status, chunk_count, error, updated_at FROM rag_documents
                ",
            )
            .execute(&mut *conn)
            .await?;
        }

        // ── 4. knowledge_docs ← agent_wiki_docs + knowledge_doc_index(kind='pages') ──
        let wiki_docs_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_wiki_docs'",
        )
        .fetch_one(&mut *conn)
        .await?;
        if wiki_docs_exists != 0 {
            sqlx::query(
                r"
                INSERT INTO knowledge_docs (id, source_id, filename, file_type, content_hash, created_at, updated_at)
                SELECT id, wiki_id, filename, file_type, content_hash, created_at, updated_at FROM agent_wiki_docs
                ",
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query(
                r"
                INSERT INTO knowledge_doc_index (doc_id, kind, status, item_count, error, updated_at)
                SELECT d.id, 'pages', d.status,
                       (SELECT COUNT(*) FROM agent_wiki_pages WHERE source_doc_id = d.id),
                       d.error, d.updated_at
                FROM agent_wiki_docs d
                ",
            )
            .execute(&mut *conn)
            .await?;
        }

        // ── 5. knowledge_chunks ← rag_chunks ──
        let rag_chunks_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='rag_chunks'",
        )
        .fetch_one(&mut *conn)
        .await?;
        if rag_chunks_exists != 0 {
            sqlx::query(
                r"
                INSERT INTO knowledge_chunks (id, doc_id, source_id, seq, heading_path, content, token_count)
                SELECT id, doc_id, kb_id, seq, heading_path, content, token_count FROM rag_chunks
                ",
            )
            .execute(&mut *conn)
            .await?;
        }

        // ── 6. knowledge_pages（带 rowid）← agent_wiki_pages ──
        let wiki_pages_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_wiki_pages'",
        )
        .fetch_one(&mut *conn)
        .await?;
        if wiki_pages_exists != 0 {
            sqlx::query(
                r"
                INSERT INTO knowledge_pages
                    (rowid, id, source_id, ref, title, summary, content, locked, source_doc_id, use_count, last_used_at, created_at, updated_at)
                SELECT rowid, id, wiki_id, ref, title, summary, content, locked, source_doc_id, use_count, last_used_at, created_at, updated_at
                FROM agent_wiki_pages
                ",
            )
            .execute(&mut *conn)
            .await?;

            // FTS 从新主表全量重灌（带 rowid，保持与主表一致）
            sqlx::query(
                "INSERT INTO knowledge_pages_fts (rowid, ref, title, summary, content) \
                 SELECT rowid, ref, title, summary, content FROM knowledge_pages",
            )
            .execute(&mut *conn)
            .await?;
        }

        // ── 7. knowledge_page_edges ← agent_wiki_edges ──
        let wiki_edges_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_wiki_edges'",
        )
        .fetch_one(&mut *conn)
        .await?;
        if wiki_edges_exists != 0 {
            sqlx::query(
                r"
                INSERT INTO knowledge_page_edges (source_id, src_page_id, src_ref, dst_ref, dst_page_id)
                SELECT wiki_id, src_page_id, src_ref, dst_ref, dst_page_id FROM agent_wiki_edges
                ",
            )
            .execute(&mut *conn)
            .await?;
        }

        // ── 8. CTAS 备份 + DROP 旧表（先引用方后被引用方），FTS 虚表直接 DROP ──
        // 必须用 CTAS 而非 RENAME：RENAME 会自动改写 FK 指向，CTAS 只是数据快照
        if rag_chunks_exists != 0 {
            sqlx::query("CREATE TABLE rag_chunks_legacy_v1 AS SELECT * FROM rag_chunks")
                .execute(&mut *conn)
                .await?;
            sqlx::query("DROP TABLE rag_chunks")
                .execute(&mut *conn)
                .await?;
        }
        if wiki_pages_exists != 0 {
            sqlx::query(
                "CREATE TABLE agent_wiki_pages_legacy_v1 AS SELECT * FROM agent_wiki_pages",
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query("DROP TABLE agent_wiki_pages")
                .execute(&mut *conn)
                .await?;
        }
        // FTS 虚表不能 CTAS，直接 DROP（内容可从 knowledge_pages 重建）
        let fts_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','view') AND name='agent_wiki_pages_fts'",
        )
        .fetch_one(&mut *conn)
        .await?;
        if fts_exists != 0 {
            // 虚表的 shadow 表（如 agent_wiki_pages_fts_content 等）随 DROP 自动清理
            sqlx::query("DROP TABLE IF EXISTS agent_wiki_pages_fts")
                .execute(&mut *conn)
                .await?;
        }
        if wiki_edges_exists != 0 {
            sqlx::query(
                "CREATE TABLE agent_wiki_edges_legacy_v1 AS SELECT * FROM agent_wiki_edges",
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query("DROP TABLE agent_wiki_edges")
                .execute(&mut *conn)
                .await?;
        }
        if rag_docs_exists != 0 {
            sqlx::query("CREATE TABLE rag_documents_legacy_v1 AS SELECT * FROM rag_documents")
                .execute(&mut *conn)
                .await?;
            sqlx::query("DROP TABLE rag_documents")
                .execute(&mut *conn)
                .await?;
        }
        if wiki_docs_exists != 0 {
            sqlx::query("CREATE TABLE agent_wiki_docs_legacy_v1 AS SELECT * FROM agent_wiki_docs")
                .execute(&mut *conn)
                .await?;
            sqlx::query("DROP TABLE agent_wiki_docs")
                .execute(&mut *conn)
                .await?;
        }
        if rag_exists != 0 {
            sqlx::query(
                "CREATE TABLE rag_knowledge_bases_legacy_v1 AS SELECT * FROM rag_knowledge_bases",
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query("DROP TABLE rag_knowledge_bases")
                .execute(&mut *conn)
                .await?;
        }
        if wiki_exists != 0 {
            sqlx::query("CREATE TABLE agent_wikis_legacy_v1 AS SELECT * FROM agent_wikis")
                .execute(&mut *conn)
                .await?;
            sqlx::query("DROP TABLE agent_wikis")
                .execute(&mut *conn)
                .await?;
        }

        // 修复 llm_api_keys 的悬空 FK：它原指向 rag_knowledge_bases，DROP 后表名不存在，
        // 任何对 llm_api_keys 的写入都会报 "no such table: main.rag_knowledge_bases"。
        // 若 llm_api_keys 存在，需重建使其 FK 指向 knowledge_sources（列序保持不变，
        // 避免 sqlx 语句缓存错位：id, key_hash, key_prefix, name, enabled, created_at, last_used_at, kb_id）。
        let has_llm_keys: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='llm_api_keys'",
        )
        .fetch_one(&mut *conn)
        .await?;
        if has_llm_keys != 0 {
            // 检查 FK 是否仍指向旧表名（新库的 llm_api_keys 已正确指向 knowledge_sources，无需重建）
            let fk_target: Option<String> = sqlx::query_scalar(
                "SELECT \"table\" FROM pragma_foreign_key_list('llm_api_keys') WHERE \"from\"='kb_id'",
            )
            .fetch_optional(&mut *conn)
            .await?;
            if fk_target.as_deref() == Some("rag_knowledge_bases") {
                sqlx::query(
                    r"
                    CREATE TABLE llm_api_keys_new (
                        id TEXT PRIMARY KEY,
                        key_hash TEXT NOT NULL UNIQUE,
                        key_prefix TEXT NOT NULL,
                        name TEXT NOT NULL DEFAULT '',
                        enabled INTEGER NOT NULL DEFAULT 1,
                        created_at TEXT NOT NULL DEFAULT (datetime('now')),
                        last_used_at TEXT,
                        kb_id TEXT REFERENCES knowledge_sources(id) ON DELETE SET NULL
                    )
                    ",
                )
                .execute(&mut *conn)
                .await?;
                // 显式列名，避免 SELECT * 按位置错配
                sqlx::query(
                    "INSERT INTO llm_api_keys_new (id, key_hash, key_prefix, name, enabled, created_at, last_used_at, kb_id) \
                     SELECT id, key_hash, key_prefix, name, enabled, created_at, last_used_at, kb_id FROM llm_api_keys",
                )
                .execute(&mut *conn)
                .await?;
                sqlx::query("DROP TABLE llm_api_keys")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("ALTER TABLE llm_api_keys_new RENAME TO llm_api_keys")
                    .execute(&mut *conn)
                    .await?;
            }
        }

        sqlx::query("COMMIT").execute(&mut *conn).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// agent_sessions 补 context_used/context_size 列（ACP usage_update 快照），
    /// 且重复跑迁移幂等（duplicate column 跳过）。
    #[tokio::test]
    async fn test_migrate_agent_sessions_add_context_usage() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();
        // 幂等：再次跑完整迁移链不报错
        super::Database::initialize_schema(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO agent_workspaces (id, name, client_id, runtime_type, root_path) \
             VALUES ('w1', 'w', 'c1', 'host', '/p')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_sessions (id, workspace_id, context_used, context_size) \
             VALUES ('s1', 'w1', 100, 200000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let row: (Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT context_used, context_size FROM agent_sessions WHERE id = 's1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, Some(100));
        assert_eq!(row.1, Some(200_000));
    }

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
        assert!((conf - 0.8).abs() < f64::EPSILON);
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
        sqlx::query(
            "INSERT INTO agent_skills (id, name, content) VALUES ('s1', 'deploy', '## 步骤')",
        )
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
        for idx in [
            "idx_skills_scope",
            "idx_skills_source",
            "idx_skills_enabled",
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

    /// agent_roles schema：建表 + 内置角色 seed + 幂等迁移 + 唯一索引 + role_id 列。
    #[tokio::test]
    async fn test_agent_roles_schema_and_seed() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();

        // 幂等：再次初始化不报错
        super::Database::initialize_schema(&pool).await.unwrap();

        // 内置角色 seed：general 和 explore
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_roles WHERE is_builtin = 1")
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

    // ── Helpers for knowledge-unify tests: create old schema ──────────

    async fn create_old_rag_schema(pool: &sqlx::Pool<sqlx::Sqlite>) {
        sqlx::query(
            r"
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
            ",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r"
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
            ",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS rag_chunks (
                id TEXT PRIMARY KEY,
                doc_id TEXT NOT NULL REFERENCES rag_documents(id) ON DELETE CASCADE,
                kb_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                heading_path TEXT NOT NULL DEFAULT '',
                content TEXT NOT NULL,
                token_count INTEGER NOT NULL DEFAULT 0
            )
            ",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_rag_chunks_doc ON rag_chunks(doc_id)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_rag_chunks_kb ON rag_chunks(kb_id)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_rag_documents_kb ON rag_documents(kb_id)")
            .execute(pool)
            .await
            .unwrap();
        // llm_api_keys 旧表（便于验证 kb_id FK 不改值）
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS llm_api_keys (
                id TEXT PRIMARY KEY,
                key_hash TEXT NOT NULL UNIQUE,
                key_prefix TEXT NOT NULL,
                name TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_used_at TEXT,
                kb_id TEXT REFERENCES rag_knowledge_bases(id) ON DELETE SET NULL
            )
            ",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn create_old_wiki_schema(pool: &sqlx::Pool<sqlx::Sqlite>) {
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS agent_wikis (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                summary TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'draft' CHECK(status IN ('draft','pending','processing','ready','failed')),
                version INTEGER NOT NULL DEFAULT 1,
                page_count INTEGER NOT NULL DEFAULT 0,
                scope_type TEXT NOT NULL DEFAULT 'workspace',
                client_id TEXT NOT NULL DEFAULT '',
                workspace_id TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_wikis_name_scope ON agent_wikis(name, scope_type, client_id, workspace_id)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_wikis_scope ON agent_wikis(client_id, workspace_id)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS agent_wiki_docs (
                id TEXT PRIMARY KEY,
                wiki_id TEXT NOT NULL REFERENCES agent_wikis(id) ON DELETE CASCADE,
                filename TEXT NOT NULL,
                file_type TEXT NOT NULL DEFAULT '',
                content_hash TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','processing','ready','failed')),
                error TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            ",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_docs_wiki ON agent_wiki_docs(wiki_id)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_docs_status ON agent_wiki_docs(status)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS agent_wiki_pages (
                id TEXT PRIMARY KEY,
                wiki_id TEXT NOT NULL REFERENCES agent_wikis(id) ON DELETE CASCADE,
                ref TEXT NOT NULL,
                title TEXT NOT NULL DEFAULT '',
                summary TEXT NOT NULL DEFAULT '',
                content TEXT NOT NULL,
                locked INTEGER NOT NULL DEFAULT 0,
                source_doc_id TEXT REFERENCES agent_wiki_docs(id) ON DELETE SET NULL,
                use_count INTEGER NOT NULL DEFAULT 0,
                last_used_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(wiki_id, ref)
            )
            ",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_pages_wiki ON agent_wiki_pages(wiki_id)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_pages_ref ON agent_wiki_pages(ref)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_pages_locked ON agent_wiki_pages(locked)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS agent_wiki_edges (
                wiki_id TEXT NOT NULL REFERENCES agent_wikis(id) ON DELETE CASCADE,
                src_page_id TEXT NOT NULL REFERENCES agent_wiki_pages(id) ON DELETE CASCADE,
                src_ref TEXT NOT NULL,
                dst_ref TEXT NOT NULL,
                dst_page_id TEXT REFERENCES agent_wiki_pages(id) ON DELETE SET NULL,
                PRIMARY KEY (wiki_id, src_page_id, dst_ref)
            )
            ",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_edges_wiki ON agent_wiki_edges(wiki_id)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_wiki_edges_src ON agent_wiki_edges(src_page_id)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_wiki_edges_dst ON agent_wiki_edges(dst_page_id)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE VIRTUAL TABLE IF NOT EXISTS agent_wiki_pages_fts USING fts5(ref, title, summary, content, tokenize='trigram')",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn setup_old_db_with_data(pool: &sqlx::Pool<sqlx::Sqlite>) {
        create_old_rag_schema(pool).await;
        create_old_wiki_schema(pool).await;

        // ── KB 1 个 + 2 文档 + 3 chunks ──
        sqlx::query(
            "INSERT INTO rag_knowledge_bases (id, name, description, emb_base_url, emb_api_key, emb_model, emb_dimension, top_k, chunk_size, chunk_overlap, score_threshold, enabled) \
             VALUES ('kb-1', 'rag-kb', 'desc', 'https://emb.example', 'sk-x', 'm1', 1536, 5, 512, 64, 0.3, 1)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO rag_documents (id, kb_id, filename, file_type, content_hash, status, chunk_count) \
             VALUES ('rd-1', 'kb-1', 'a.md', 'md', 'h1', 'ready', 2)",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO rag_documents (id, kb_id, filename, file_type, content_hash, status, chunk_count, error) \
             VALUES ('rd-2', 'kb-1', 'b.md', 'md', 'h2', 'failed', 0, 'err')",
        )
        .execute(pool)
        .await
        .unwrap();
        for (id, doc, seq) in [("c-1", "rd-1", 0), ("c-2", "rd-1", 1), ("c-3", "rd-2", 0)] {
            sqlx::query(
                "INSERT INTO rag_chunks (id, doc_id, kb_id, seq, heading_path, content, token_count) VALUES (?, ?, 'kb-1', ?, '## H', 'hello', 3)",
            )
            .bind(id)
            .bind(doc)
            .bind(seq)
            .execute(pool)
            .await
            .unwrap();
        }

        // ── Wiki 1 个 + 1 文档 + 2 pages + 1 edge + FTS ──
        sqlx::query(
            "INSERT INTO agent_wikis (id, name, summary, status, version, page_count, scope_type, client_id, workspace_id) \
             VALUES ('wiki-1', 'my-wiki', 'sum', 'ready', 2, 2, 'workspace', 'c1', 'w1')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_wiki_docs (id, wiki_id, filename, file_type, content_hash, status) \
             VALUES ('wd-1', 'wiki-1', 'doc.pdf', 'pdf', 'hh1', 'ready')",
        )
        .execute(pool)
        .await
        .unwrap();
        // 用显式 rowid 便于后续校验是否保留
        sqlx::query(
            "INSERT INTO agent_wiki_pages (id, wiki_id, ref, title, summary, content, locked, source_doc_id) \
             VALUES ('wp-1', 'wiki-1', 'intro', 'Intro', 'sum1', '这是介绍页包含独有词 AlphaBetaGamma', 0, 'wd-1')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_wiki_pages (id, wiki_id, ref, title, summary, content, locked, source_doc_id) \
             VALUES ('wp-2', 'wiki-1', 'detail', 'Detail', 'sum2', '详情页内容 DeltaEpsilonZeta', 0, 'wd-1')",
        )
        .execute(pool)
        .await
        .unwrap();
        // FTS 行：rowid 对应 pages
        let rid1: (i64,) = sqlx::query_as("SELECT rowid FROM agent_wiki_pages WHERE id='wp-1'")
            .fetch_one(pool)
            .await
            .unwrap();
        let rid2: (i64,) = sqlx::query_as("SELECT rowid FROM agent_wiki_pages WHERE id='wp-2'")
            .fetch_one(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO agent_wiki_pages_fts (rowid, ref, title, summary, content) VALUES (?, 'intro', 'Intro', 'sum1', '这是介绍页包含独有词 AlphaBetaGamma')")
            .bind(rid1.0)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO agent_wiki_pages_fts (rowid, ref, title, summary, content) VALUES (?, 'detail', 'Detail', 'sum2', '详情页内容 DeltaEpsilonZeta')")
            .bind(rid2.0)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO agent_wiki_edges (wiki_id, src_page_id, src_ref, dst_ref, dst_page_id) \
             VALUES ('wiki-1', 'wp-1', 'intro', 'detail', 'wp-2')",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// 1) KB+Wiki 存量统一到 knowledge_*，id/计数/索引标志正确。
    #[tokio::test]
    async fn migration_unifies_kb_and_wiki_into_knowledge_sources() {
        // 用裸 sqlite::memory: 手动搭旧表+数据（不走 initialize_schema，避免新表提前存在）
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        setup_old_db_with_data(&pool).await;

        // 额外：llm_api_keys 指向 KB（验证值不变）
        sqlx::query(
            "INSERT INTO llm_api_keys (id, key_hash, key_prefix, kb_id) VALUES ('k1', 'h1', 'sk-', 'kb-1')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // 先建新表（initialize_schema 的前半段逻辑）。这里直接复用
        // initialize_schema 会因旧表存在而触发迁移，故走完整链路验证最简单：
        // 但 setup 阶段已手动建旧表，此时再跑 initialize_schema 会创建新表并迁移
        super::Database::initialize_schema(&pool).await.unwrap();

        // ── knowledge_sources 2 个，标志正确，id 未变 ──
        let sources: Vec<(String, i64, i64, String, String)> = sqlx::query_as(
            "SELECT id, index_vector, index_pages, scope_type, status FROM knowledge_sources ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(sources.len(), 2, "应有 2 个 knowledge_sources");
        let kb = sources
            .iter()
            .find(|(id, _, _, _, _)| id == "kb-1")
            .unwrap();
        assert_eq!(kb.1, 1, "KB index_vector=1");
        assert_eq!(kb.2, 0, "KB index_pages=0");
        assert_eq!(kb.3, "global");
        assert_eq!(kb.4, "ready");
        let wiki = sources
            .iter()
            .find(|(id, _, _, _, _)| id == "wiki-1")
            .unwrap();
        assert_eq!(wiki.1, 0, "Wiki index_vector=0");
        assert_eq!(wiki.2, 1, "Wiki index_pages=1");
        assert_eq!(wiki.3, "workspace");
        assert_eq!(wiki.4, "ready");

        // emb/version/page_count/enabled 映射
        let kb_row: (String, i64, i64, i64) =
            sqlx::query_as("SELECT emb_model, emb_dimension, version, enabled FROM knowledge_sources WHERE id='kb-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kb_row.0, "m1");
        assert_eq!(kb_row.1, 1536);
        assert_eq!(kb_row.2, 1);
        assert_eq!(kb_row.3, 1);
        let wiki_row: (String, i64, i64, i64) =
            sqlx::query_as("SELECT emb_model, emb_dimension, version, page_count FROM knowledge_sources WHERE id='wiki-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(wiki_row.0, "");
        assert_eq!(wiki_row.1, 0);
        assert_eq!(wiki_row.2, 2);
        assert_eq!(wiki_row.3, 2);

        // ── knowledge_docs 3 个（2 rag + 1 wiki）──
        let doc_cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_docs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(doc_cnt, 3);
        // source_id 指向正确
        let rd1_src: String =
            sqlx::query_scalar("SELECT source_id FROM knowledge_docs WHERE id='rd-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rd1_src, "kb-1");
        let wd1_src: String =
            sqlx::query_scalar("SELECT source_id FROM knowledge_docs WHERE id='wd-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(wd1_src, "wiki-1");

        // ── knowledge_doc_index ──
        let kdi: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT doc_id, kind, status, item_count FROM knowledge_doc_index ORDER BY doc_id, kind",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(kdi.len(), 3);
        let rd1 = kdi
            .iter()
            .find(|(id, k, _, _)| id == "rd-1" && k == "vector")
            .unwrap();
        assert_eq!(rd1.2, "ready");
        assert_eq!(rd1.3, 2, "chunk_count→item_count");
        let rd2 = kdi
            .iter()
            .find(|(id, k, _, _)| id == "rd-2" && k == "vector")
            .unwrap();
        assert_eq!(rd2.2, "failed");
        assert_eq!(rd2.3, 0);
        let wd1 = kdi
            .iter()
            .find(|(id, k, _, _)| id == "wd-1" && k == "pages")
            .unwrap();
        assert_eq!(wd1.2, "ready");
        assert_eq!(wd1.3, 2, "pages item_count 应为该 doc 产出的 page 数");

        // ── chunks/pages/edges 行数 ──
        let c_cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_chunks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(c_cnt, 3);
        let p_cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(p_cnt, 2);
        let e_cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_page_edges")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(e_cnt, 1);
        let fts_cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages_fts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fts_cnt, 2);

        // ── kb_id 值未变 ──
        let kb_id: Option<String> =
            sqlx::query_scalar("SELECT kb_id FROM llm_api_keys WHERE id='k1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kb_id.as_deref(), Some("kb-1"));

        // ── legacy 表存在且行数一致 ──
        for (tbl, expect) in [
            ("rag_knowledge_bases_legacy_v1", 1),
            ("rag_documents_legacy_v1", 2),
            ("rag_chunks_legacy_v1", 3),
            ("agent_wikis_legacy_v1", 1),
            ("agent_wiki_docs_legacy_v1", 1),
            ("agent_wiki_pages_legacy_v1", 2),
            ("agent_wiki_edges_legacy_v1", 1),
        ] {
            let cnt: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {tbl}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(cnt, expect, "legacy {tbl} 行数不符");
        }
        // 旧表已 DROP
        for tbl in [
            "rag_knowledge_bases",
            "rag_documents",
            "rag_chunks",
            "agent_wikis",
            "agent_wiki_docs",
            "agent_wiki_pages",
            "agent_wiki_edges",
            "agent_wiki_pages_fts",
        ] {
            let cnt: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','view') AND name=?",
            )
            .bind(tbl)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(cnt, 0, "旧表 {tbl} 应已被 DROP");
        }
    }

    /// 2) rowid 显式保留：FTS JOIN rowid 返回正确页。
    #[tokio::test]
    async fn migration_preserves_page_rowid_for_fts() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        setup_old_db_with_data(&pool).await;

        // 记下迁移前的 rowid
        let before: Vec<(String, i64)> =
            sqlx::query_as("SELECT id, rowid FROM agent_wiki_pages ORDER BY ref")
                .fetch_all(&pool)
                .await
                .unwrap();

        super::Database::initialize_schema(&pool).await.unwrap();

        let after: Vec<(String, i64)> =
            sqlx::query_as("SELECT id, rowid FROM knowledge_pages ORDER BY ref")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(before, after, "rowid 必须显式保留");

        // FTS MATCH 三字子串返回正确页（trigram 需 ≥3 字符）
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT p.ref FROM knowledge_pages_fts \
             JOIN knowledge_pages p ON p.rowid = knowledge_pages_fts.rowid \
             WHERE knowledge_pages_fts MATCH 'AlphaBetaGamma'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "intro");

        let rows2: Vec<(String,)> = sqlx::query_as(
            "SELECT p.ref FROM knowledge_pages_fts \
             JOIN knowledge_pages p ON p.rowid = knowledge_pages_fts.rowid \
             WHERE knowledge_pages_fts MATCH 'DeltaEpsilonZeta'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows2.len(), 1);
        assert_eq!(rows2[0].0, "detail");
    }

    /// 3) 幂等：连跑两次迁移行数不翻倍。
    #[tokio::test]
    async fn migration_is_idempotent() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        setup_old_db_with_data(&pool).await;
        super::Database::initialize_schema(&pool).await.unwrap();

        let counts_before = (
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM knowledge_sources")
                .fetch_one(&pool)
                .await
                .unwrap(),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM knowledge_docs")
                .fetch_one(&pool)
                .await
                .unwrap(),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM knowledge_chunks")
                .fetch_one(&pool)
                .await
                .unwrap(),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM knowledge_pages")
                .fetch_one(&pool)
                .await
                .unwrap(),
        );
        super::Database::initialize_schema(&pool).await.unwrap();
        let counts_after = (
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM knowledge_sources")
                .fetch_one(&pool)
                .await
                .unwrap(),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM knowledge_docs")
                .fetch_one(&pool)
                .await
                .unwrap(),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM knowledge_chunks")
                .fetch_one(&pool)
                .await
                .unwrap(),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM knowledge_pages")
                .fetch_one(&pool)
                .await
                .unwrap(),
        );
        assert_eq!(counts_before, counts_after, "二次迁移不应翻倍");
    }

    /// 4) 全新库：knowledge_sources 存在，旧表不存在。
    #[tokio::test]
    async fn fresh_db_has_no_legacy_tables() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::Database::initialize_schema(&pool).await.unwrap();

        let ks: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='knowledge_sources'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ks, 1, "全新库应有 knowledge_sources");
        let ks_docs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='knowledge_docs'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ks_docs, 1);
        let ks_idx: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='knowledge_doc_index'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ks_idx, 1);

        for tbl in [
            "rag_knowledge_bases",
            "rag_knowledge_bases_legacy_v1",
            "rag_documents",
            "rag_documents_legacy_v1",
            "rag_chunks",
            "rag_chunks_legacy_v1",
            "agent_wikis",
            "agent_wikis_legacy_v1",
        ] {
            let cnt: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            )
            .bind(tbl)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(cnt, 0, "全新库不应有 {tbl}");
        }
    }

    /// 5) 半存量库：只有 KB 旧表，没有 Wiki 旧表。
    /// 这是真实升级路径——Wiki 功能上线前的老库只有 rag_*，`if wiki_exists != 0`
    /// 分支必须能安全跳过（而非因 agent_wikis 不存在而报错）。
    #[tokio::test]
    async fn migration_handles_kb_only_legacy_db() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        create_old_rag_schema(&pool).await;
        sqlx::query(
            "INSERT INTO rag_knowledge_bases (id, name, description, emb_base_url, emb_api_key, emb_model, emb_dimension, top_k, chunk_size, chunk_overlap, score_threshold, enabled) \
             VALUES ('kb-only', 'solo', 'd', 'https://e', 'sk', 'm', 768, 5, 512, 64, 0.3, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO rag_documents (id, kb_id, filename, file_type, content_hash, status, chunk_count) \
             VALUES ('d1', 'kb-only', 'x.md', 'md', 'h', 'ready', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        super::Database::initialize_schema(&pool).await.unwrap();

        let (id, iv, ip): (String, i64, i64) = sqlx::query_as(
            "SELECT id, index_vector, index_pages FROM knowledge_sources WHERE id = 'kb-only'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(id, "kb-only", "id 应沿用（向量 shard 目录靠它定位）");
        assert_eq!((iv, ip), (1, 0), "KB 迁入后只开 vector 索引");

        let (kind, status, item_count): (String, String, i64) = sqlx::query_as(
            "SELECT kind, status, item_count FROM knowledge_doc_index WHERE doc_id = 'd1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (kind.as_str(), status.as_str(), item_count),
            ("vector", "ready", 1)
        );

        // 没有 Wiki 旧表时不应凭空产出 pages 索引行
        let pages_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_doc_index WHERE kind = 'pages'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pages_rows, 0);
    }

    /// 6) 半存量库：只有 Wiki 旧表，没有 KB 旧表。
    /// 此时 llm_api_keys 由新 schema 建（kb_id 已指向 knowledge_sources），
    /// 迁移不应再触发 llm_api_keys 重建。
    #[tokio::test]
    async fn migration_handles_wiki_only_legacy_db() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        create_old_wiki_schema(&pool).await;
        sqlx::query(
            "INSERT INTO agent_wikis (id, name, summary, status, version, page_count, scope_type, client_id, workspace_id) \
             VALUES ('w-only', 'solo-wiki', 's', 'ready', 1, 1, 'global', '', '')",
        )
        .execute(&pool)
        .await
        .unwrap();

        super::Database::initialize_schema(&pool).await.unwrap();

        let (iv, ip, enabled): (i64, i64, i64) = sqlx::query_as(
            "SELECT index_vector, index_pages, enabled FROM knowledge_sources WHERE id = 'w-only'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((iv, ip), (0, 1), "Wiki 迁入后只开 pages 索引");
        assert_eq!(enabled, 1, "agent_wikis 无 enabled 列，迁入应默认启用");

        // llm_api_keys 的 kb_id FK 应指向新表（新 schema 直接建对，无需重建）
        let fk_target: Option<String> = sqlx::query_scalar(
            "SELECT \"table\" FROM pragma_foreign_key_list('llm_api_keys') WHERE \"from\"='kb_id'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(fk_target.as_deref(), Some("knowledge_sources"));
    }

    /// 7) 迁移失败时事务整体回滚，不留半成品。
    ///
    /// 构造脏数据触发失败——旧库里存在同名同 scope 的两个 wiki（唯一索引是本次
    /// 新加的，存量库里可能有这种历史脏数据），迁入时撞 `idx_ks_name_scope`。
    ///
    /// 本测试**刻意不断言**「失败后 foreign_keys 已恢复」：PRAGMA 是连接级状态，
    /// 而池有 5 条连接（`lib.rs` 的 `max_connections(5)`），测试从池里拿到的几乎
    /// 不是迁移用过的那条，断言只会恒真。实测已确认：把恢复逻辑改坏，这里查到的
    /// `PRAGMA foreign_keys` 仍是 1。恢复逻辑的必要性靠代码审查保证——写一个拿不到
    /// 目标连接的断言，只会留下虚假的覆盖感。
    #[tokio::test]
    async fn migration_failure_restores_fk_enforcement() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        create_old_wiki_schema(&pool).await;
        // 去掉旧唯一索引，塞入迁移后会冲突的两行
        sqlx::query("DROP INDEX IF EXISTS idx_wikis_name_scope")
            .execute(&pool)
            .await
            .unwrap();
        for id in ["dup-a", "dup-b"] {
            sqlx::query(
                "INSERT INTO agent_wikis (id, name, summary, status, version, page_count, scope_type, client_id, workspace_id) \
                 VALUES (?, 'same-name', '', 'ready', 1, 0, 'global', '', '')",
            )
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }

        let err = super::Database::initialize_schema(&pool).await;
        assert!(err.is_err(), "同名 pages 容器应触发唯一索引冲突");

        // 事务已回滚：不应留下半迁移的行
        let migrated: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_sources")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(migrated, 0, "失败应整体回滚，不留半成品");

        // 旧表必须还在（未被 CTAS 备份+DROP 掉），否则失败的迁移会让数据无处可寻
        let old_wikis: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_wikis")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(old_wikis, 2, "迁移失败后旧表须完好保留");
        let legacy: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_wikis_legacy_v1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(legacy, 0, "回滚后不应留下 legacy 备份表");
    }
}
