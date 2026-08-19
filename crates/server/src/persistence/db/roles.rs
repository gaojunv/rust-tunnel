//! Agent 角色数据访问层：`agent_roles` 表。
//!
//! 角色定义了系统提示词、工具白名单/黑名单、模型覆盖和作用域，
//! 供 subagent 调度和主会话角色切换使用。scope 范式对齐 `agent_skills`
//! （global|client|workspace，坐标 = scope_type + client_id + workspace_id）。

use super::agent::ser_de_normalized_dt;
use super::Database;

/// 一条角色的完整行。`tools_allow`/`tools_deny` 为 JSON 数组字符串
/// （`'["shell","read_file"]'`），NULL 表示不限制/无排除。
/// `model_override` 为网关模型引用（alias/model:/group:），NULL 继承 workspace 默认。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentRoleRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub tools_allow: Option<String>,
    pub tools_deny: Option<String>,
    pub model_override: Option<String>,
    pub mode: String,
    pub scope_type: String,
    pub client_id: String,
    pub workspace_id: String,
    pub is_builtin: i32,
    pub enabled: i32,
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub created_at: String,
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub updated_at: String,
}

/// 角色列表行（与完整行相同字段——角色数据量小，无需 Summary 分离）。
pub type AgentRoleSummary = AgentRoleRecord;

impl Database {
    /// 插入一条角色。`enabled` 默认 1、`is_builtin` 默认 0（不显式提供）。
    #[allow(clippy::too_many_arguments)]
    pub async fn role_insert(
        &self,
        id: &str,
        name: &str,
        description: &str,
        system_prompt: &str,
        tools_allow: Option<&str>,
        tools_deny: Option<&str>,
        model_override: Option<&str>,
        mode: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO agent_roles (
                id, name, description, system_prompt, tools_allow, tools_deny,
                model_override, mode, scope_type, client_id, workspace_id
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(system_prompt)
        .bind(tools_allow)
        .bind(tools_deny)
        .bind(model_override)
        .bind(mode)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn role_get_by_id(
        &self,
        id: &str,
    ) -> Result<Option<AgentRoleRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentRoleRecord>("SELECT * FROM agent_roles WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// 同作用域同名查重（去重用）。`name` 必须已由调用方归一化（trim + lowercase）。
    pub async fn role_get_by_name_scope(
        &self,
        name: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<Option<AgentRoleRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentRoleRecord>(
            "SELECT * FROM agent_roles \
             WHERE name = ? AND scope_type = ? AND client_id = ? AND workspace_id = ?",
        )
        .bind(name)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// 更新角色可变字段（name/description/system_prompt/tools_allow/tools_deny/
    /// model_override/mode/scope 坐标）。is_builtin 角色的 name 修改由 API 层拦截，
    /// DAO 层照常执行。
    #[allow(clippy::too_many_arguments)]
    pub async fn role_update(
        &self,
        id: &str,
        name: &str,
        description: &str,
        system_prompt: &str,
        tools_allow: Option<&str>,
        tools_deny: Option<&str>,
        model_override: Option<&str>,
        mode: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_roles SET name = ?, description = ?, system_prompt = ?, \
             tools_allow = ?, tools_deny = ?, model_override = ?, mode = ?, \
             scope_type = ?, client_id = ?, workspace_id = ?, updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(name)
        .bind(description)
        .bind(system_prompt)
        .bind(tools_allow)
        .bind(tools_deny)
        .bind(model_override)
        .bind(mode)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 删除角色，并将关联 `agent_sessions.role_id` 置空（同一事务内两条 SQL）。
    pub async fn role_delete(&self, id: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE agent_sessions SET role_id = NULL WHERE role_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM agent_roles WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 翻转 enabled（1 <-> 0）。不存在的 id 无副作用。
    pub async fn role_toggle_enabled(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_roles SET enabled = 1 - enabled, updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 列出角色。支持 scope/client/workspace/q/enabled 过滤与排序分页。
    ///
    /// `scope_type` 精确过滤；`client_id`/`workspace_id` 传空串等同不过滤；`q` 对
    /// `name`/`description` 做 LIKE 模糊匹配；`enabled`：Some(true) 只看启用、
    /// Some(false) 只看停用、None 不过滤。
    #[allow(clippy::too_many_arguments)]
    pub async fn role_list(
        &self,
        scope_type: Option<&str>,
        client_id: Option<&str>,
        workspace_id: Option<&str>,
        q: Option<&str>,
        enabled: Option<bool>,
        mode: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AgentRoleSummary>, sqlx::Error> {
        let mut qb = sqlx::QueryBuilder::new("SELECT * FROM agent_roles WHERE 1=1");
        if let Some(s) = scope_type.filter(|s| !s.is_empty()) {
            qb.push(" AND scope_type = ").push_bind(s);
        }
        if let Some(c) = client_id.filter(|c| !c.is_empty()) {
            qb.push(" AND client_id = ").push_bind(c);
        }
        if let Some(w) = workspace_id.filter(|w| !w.is_empty()) {
            qb.push(" AND workspace_id = ").push_bind(w);
        }
        if let Some(q) = q.filter(|q| !q.is_empty()) {
            qb.push(" AND (name LIKE ")
                .push_bind(format!("%{q}%"))
                .push(" OR description LIKE ")
                .push_bind(format!("%{q}%"))
                .push(")");
        }
        if enabled == Some(true) {
            qb.push(" AND enabled = 1");
        } else if enabled == Some(false) {
            qb.push(" AND enabled = 0");
        }
        if let Some(m) = mode.filter(|m| !m.is_empty()) {
            qb.push(" AND mode = ").push_bind(m);
        }
        qb.push(" ORDER BY created_at DESC")
            .push(" LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        qb.build_query_as::<AgentRoleSummary>()
            .fetch_all(&self.pool)
            .await
    }

    /// 可见性查询（供 task 调度与 @ 补全）：enabled=1 且对 (client_id, workspace_id)
    /// **作用域可见**（global 恒可见 / client 匹配 client_id / workspace 需
    /// client_id + workspace_id 都匹配），可选 mode 过滤。
    pub async fn role_list_visible(
        &self,
        client_id: &str,
        workspace_id: &str,
        mode_filter: Option<&str>,
    ) -> Result<Vec<AgentRoleRecord>, sqlx::Error> {
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT * FROM agent_roles WHERE enabled = 1 AND (",
        );
        qb.push("scope_type = 'global'")
            .push(" OR (scope_type = 'client' AND client_id = ")
            .push_bind(client_id)
            .push(")")
            .push(" OR (scope_type = 'workspace' AND client_id = ")
            .push_bind(client_id)
            .push(" AND workspace_id = ")
            .push_bind(workspace_id)
            .push(")");
        qb.push(")");
        if let Some(m) = mode_filter.filter(|m| !m.is_empty()) {
            // 模式过滤：匹配 mode = <m> OR mode = 'all'
            qb.push(" AND (mode = ")
                .push_bind(m.to_string())
                .push(" OR mode = 'all')");
        }
        qb.push(" ORDER BY name ASC");
        qb.build_query_as::<AgentRoleRecord>()
            .fetch_all(&self.pool)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_role(
        db: &Database,
        id: &str,
        name: &str,
        scope: &str,
        client: &str,
        ws: &str,
    ) {
        db.role_insert(
            id,
            name,
            "desc",
            "system prompt",
            None,
            None,
            None,
            "subagent",
            scope,
            client,
            ws,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn roles_crud_and_scope() {
        let db = Database::new(":memory:").await.unwrap();
        db.role_insert(
            "r1",
            "code-reviewer",
            "代码审查角色",
            "Review code thoroughly",
            Some(r#"["read_file","search"]"#),
            None,
            Some("sonnet"),
            "subagent",
            "workspace",
            "c1",
            "w1",
        )
        .await
        .unwrap();

        let row = db.role_get_by_id("r1").await.unwrap().unwrap();
        assert_eq!(row.name, "code-reviewer");
        assert_eq!(row.description, "代码审查角色");
        assert_eq!(row.system_prompt, "Review code thoroughly");
        assert_eq!(
            row.tools_allow.as_deref(),
            Some(r#"["read_file","search"]"#)
        );
        assert!(row.tools_deny.is_none());
        assert_eq!(row.model_override.as_deref(), Some("sonnet"));
        assert_eq!(row.mode, "subagent");
        assert_eq!(row.scope_type, "workspace");
        assert_eq!(row.client_id, "c1");
        assert_eq!(row.workspace_id, "w1");
        assert_eq!(row.is_builtin, 0);
        assert_eq!(row.enabled, 1);

        // 同名同作用域查重命中；异作用域未命中
        assert!(
            db.role_get_by_name_scope("code-reviewer", "workspace", "c1", "w1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.role_get_by_name_scope("code-reviewer", "global", "", "")
                .await
                .unwrap()
                .is_none()
        );

        // update
        db.role_update(
            "r1",
            "code-reviewer-v2",
            "新描述",
            "New prompt",
            Some(r#"["read_file"]"#),
            Some(r#"["shell"]"#),
            None,
            "primary",
            "global",
            "",
            "",
        )
        .await
        .unwrap();
        let row = db.role_get_by_id("r1").await.unwrap().unwrap();
        assert_eq!(row.name, "code-reviewer-v2");
        assert_eq!(row.system_prompt, "New prompt");
        assert_eq!(row.mode, "primary");
        assert_eq!(row.tools_deny.as_deref(), Some(r#"["shell"]"#));
        assert_eq!(row.enabled, 1, "enabled 不被 update 重置");

        // toggle
        db.role_toggle_enabled("r1").await.unwrap();
        assert_eq!(db.role_get_by_id("r1").await.unwrap().unwrap().enabled, 0);
        db.role_toggle_enabled("r1").await.unwrap();
        assert_eq!(db.role_get_by_id("r1").await.unwrap().unwrap().enabled, 1);

        // delete
        db.role_delete("r1").await.unwrap();
        assert!(db.role_get_by_id("r1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn role_delete_clears_session_role_id() {
        let db = Database::new(":memory:").await.unwrap();
        db.role_insert(
            "r1", "tmp", "", "", None, None, None, "subagent", "global", "", "",
        )
        .await
        .unwrap();
        // 创建 workspace + session + 绑定角色
        db.agent_create_workspace(
            "w1", "p", "c1", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_update_session_role("s1", Some("r1")).await.unwrap();

        // 删除角色 → session.role_id 置空
        db.role_delete("r1").await.unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert!(s.role_id.is_none(), "删角色后 session.role_id 应置空");
    }

    #[tokio::test]
    async fn role_list_filters() {
        let db = Database::new(":memory:").await.unwrap();
        // 注意：Database::new 已 seed 2 个内置角色（general, explore）
        seed_role(&db, "g1", "global-role", "global", "", "").await;
        seed_role(&db, "c1", "client-role", "client", "c1", "").await;
        seed_role(&db, "w1a", "ws-role", "workspace", "c1", "w1").await;
        seed_role(&db, "w2", "other-ws", "workspace", "c1", "w2").await;
        // 一个停用的
        db.role_insert(
            "d1", "disabled", "", "", None, None, None, "subagent", "global", "", "",
        )
        .await
        .unwrap();
        db.role_toggle_enabled("d1").await.unwrap();

        // 全量：2 builtin + 4 seeded + 1 disabled = 7
        let all = db.role_list(None, None, None, None, None, None, 100, 0).await.unwrap();
        assert_eq!(all.len(), 7);

        // scope 过滤
        let ws = db
            .role_list(Some("workspace"), None, None, None, None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(ws.len(), 2);

        // q 模糊
        let q = db
            .role_list(None, None, None, Some("client"), None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].name, "client-role");

        // enabled 过滤：7 total - 1 disabled = 6
        let en = db
            .role_list(None, None, None, None, Some(true), None, 100, 0)
            .await
            .unwrap();
        assert_eq!(en.len(), 6);
        let dis = db
            .role_list(None, None, None, None, Some(false), None, 100, 0)
            .await
            .unwrap();
        assert_eq!(dis.len(), 1);

        // mode 过滤
        db.role_insert(
            "p1", "primary-role", "", "", None, None, None, "primary", "global", "", "",
        )
        .await
        .unwrap();
        let primary = db
            .role_list(None, None, None, None, None, Some("primary"), 100, 0)
            .await
            .unwrap();
        assert_eq!(primary.len(), 1);
        assert_eq!(primary[0].name, "primary-role");
    }

    #[tokio::test]
    async fn role_visible_scope_visibility() {
        let db = Database::new(":memory:").await.unwrap();
        // 注意：Database::new 已 seed 2 个内置 subagent 角色（general, explore）
        seed_role(&db, "g1", "global-role", "global", "", "").await;
        seed_role(&db, "c1", "client-role", "client", "c1", "").await;
        seed_role(&db, "c2", "other-client", "client", "c2", "").await;
        seed_role(&db, "w1a", "ws-role", "workspace", "c1", "w1").await;
        seed_role(&db, "w2", "other-ws", "workspace", "c1", "w2").await;
        // 停用的不可见
        db.role_insert(
            "d1", "disabled", "", "", None, None, None, "subagent", "global", "", "",
        )
        .await
        .unwrap();
        db.role_toggle_enabled("d1").await.unwrap();

        // primary 模式单独过滤
        db.role_insert(
            "p1", "primary", "", "", None, None, None, "primary", "global", "", "",
        )
        .await
        .unwrap();

        let rows = db.role_list_visible("c1", "w1", None).await.unwrap();
        // 可见：builtin general/explore + g1 + p1 + c1 + w1a；不可见：c2, w2, disabled
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"role-builtin-general-0000000000000000"), "builtin general 应可见: {ids:?}");
        assert!(ids.contains(&"role-builtin-explore-0000000000000000"), "builtin explore 应可见: {ids:?}");
        assert!(ids.contains(&"g1"), "global 应可见: {ids:?}");
        assert!(ids.contains(&"c1"), "client c1 应可见: {ids:?}");
        assert!(ids.contains(&"w1a"), "workspace w1a 应可见: {ids:?}");
        assert!(ids.contains(&"p1"), "global primary 应可见: {ids:?}");
        assert!(!ids.contains(&"c2"), "client c2 不应可见");
        assert!(!ids.contains(&"w2"), "workspace w2 不应可见");
        assert!(!ids.contains(&"d1"), "disabled 不应可见");

        // mode 过滤：只要 subagent
        let sub = db.role_list_visible("c1", "w1", Some("subagent")).await.unwrap();
        let sub_ids: Vec<&str> = sub.iter().map(|r| r.id.as_str()).collect();
        assert!(sub_ids.contains(&"role-builtin-general-0000000000000000"), "builtin general subagent 应可见");
        assert!(sub_ids.contains(&"g1"), "global subagent 应可见");
        assert!(!sub_ids.contains(&"p1"), "primary 不应在 subagent 列表中");
    }

    #[tokio::test]
    async fn builtin_roles_seeded() {
        let db = Database::new(":memory:").await.unwrap();
        let general = db
            .role_get_by_name_scope("general", "global", "", "")
            .await
            .unwrap();
        assert!(general.is_some(), "内置角色 general 应存在");
        let general = general.unwrap();
        assert_eq!(general.is_builtin, 1);
        assert_eq!(general.mode, "subagent");
        assert!(general.tools_allow.is_none(), "general 无白名单");

        let explore = db
            .role_get_by_name_scope("explore", "global", "", "")
            .await
            .unwrap();
        assert!(explore.is_some(), "内置角色 explore 应存在");
        let explore = explore.unwrap();
        assert_eq!(explore.is_builtin, 1);
        assert!(explore.tools_allow.is_some(), "explore 应有白名单");
    }
}
