//! Agent workbench persistence: workspaces / sessions / messages.
use super::Database;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWorkspaceRecord {
    pub id: String,
    pub name: String,
    pub client_id: String,
    pub runtime_type: String,
    pub root_path: String,
    pub docker_image: Option<String>,
    pub docker_container_id: Option<String>,
    pub approval_mode: String,
    pub system_prompt: Option<String>,
    // ACP 远程 agent 字段。列由 `migrate_agent_workspaces_v3`（schema.rs）落地；
    // `#[sqlx(default)]` 保证旧库未跑 v3 迁移前 `SELECT *` 仍可解码（agent_type 为
    // NOT NULL 列，默认空串）。`agent_create/update_workspace` 均已支持读写这三列。
    #[sqlx(default)]
    #[serde(default)]
    pub agent_type: String,
    #[sqlx(default)]
    #[serde(default)]
    pub agent_path: Option<String>,
    #[sqlx(default)]
    #[serde(default)]
    pub llm_model_id: Option<String>,
    /// ACP 引擎选项覆盖（JSON map：config_id → value），会话建立时经
    /// `set_config_option` 注入。列由 `migrate_agent_workspaces_v4` 落地。
    #[sqlx(default)]
    #[serde(default)]
    pub agent_config_overrides: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentSessionRecord {
    pub id: String,
    pub workspace_id: String,
    pub title: Option<String>,
    pub status: String,
    pub model: Option<String>,
    /// ACP 会话配置状态（JSON map：config_id → value；仅用户显式切换过的项）
    pub config_state: Option<String>,
    /// agent 侧 ACP 会话 id（`session/new` 返回，断线重拉时 `session/resume`
    /// 凭它恢复上下文）。列由 `migrate_agent_sessions_v3`（schema.rs）落地；
    /// `#[sqlx(default)]` 保证旧库未跑迁移前 `SELECT *` 仍可解码。
    #[sqlx(default)]
    #[serde(default)]
    pub acp_session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentMessageRecord {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    pub kind: String,
    pub created_at: String,
}

impl Database {
    // ── Workspace CRUD ──────────────────────────────────────────

    /// 创建 agent workspace。`agent_type` 为 ACP 远程 agent 类型（非空列，默认空串），
    /// `agent_path`/`llm_model_id`/`agent_config_overrides` 可空（后者为 ACP 引擎
    /// 选项覆盖，JSON map：config_id → value，None 表示未配置）。调用方暂未接入请求
    /// DTO 时传 `""` / `None` 占位。
    #[allow(clippy::too_many_arguments)]
    pub async fn agent_create_workspace(
        &self,
        id: &str,
        name: &str,
        client_id: &str,
        runtime_type: &str,
        root_path: &str,
        docker_image: Option<&str>,
        docker_container_id: Option<&str>,
        agent_type: &str,
        agent_path: Option<&str>,
        llm_model_id: Option<&str>,
        agent_config_overrides: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO agent_workspaces
                (id, name, client_id, runtime_type, root_path, docker_image, docker_container_id,
                 agent_type, agent_path, llm_model_id, agent_config_overrides)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(client_id)
        .bind(runtime_type)
        .bind(root_path)
        .bind(docker_image)
        .bind(docker_container_id)
        .bind(agent_type)
        .bind(agent_path)
        .bind(llm_model_id)
        .bind(agent_config_overrides)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_get_workspace(
        &self,
        id: &str,
    ) -> Result<Option<AgentWorkspaceRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkspaceRecord>("SELECT * FROM agent_workspaces WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn agent_list_workspaces(&self) -> Result<Vec<AgentWorkspaceRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkspaceRecord>(
            "SELECT * FROM agent_workspaces ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// 更新 agent workspace 的可变字段。ACP 字段（agent_type/agent_path/llm_model_id/
    /// agent_config_overrides）采用 COALESCE 语义：`None` 保持原值，`Some` 写入新值，
    /// 与 `approval_mode` 一致。`agent_config_overrides` 为 ACP 引擎选项覆盖（JSON
    /// map：config_id → value）；`Some("{}")` 显式清空，`None` 保持原值。
    #[allow(clippy::too_many_arguments)]
    pub async fn agent_update_workspace(
        &self,
        id: &str,
        name: &str,
        root_path: &str,
        system_prompt: Option<&str>,
        approval_mode: Option<&str>,
        agent_type: Option<&str>,
        agent_path: Option<&str>,
        llm_model_id: Option<&str>,
        agent_config_overrides: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_workspaces SET name = ?, root_path = ?, system_prompt = ?, \
             approval_mode = COALESCE(?, approval_mode), \
             agent_type = COALESCE(?, agent_type), \
             agent_path = COALESCE(?, agent_path), \
             llm_model_id = COALESCE(?, llm_model_id), \
             agent_config_overrides = COALESCE(?, agent_config_overrides), \
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(name)
        .bind(root_path)
        .bind(system_prompt)
        .bind(approval_mode)
        .bind(agent_type)
        .bind(agent_path)
        .bind(llm_model_id)
        .bind(agent_config_overrides)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_delete_workspace(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_workspaces WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Session CRUD ────────────────────────────────────────────

    pub async fn agent_create_session(
        &self,
        id: &str,
        workspace_id: &str,
        title: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_sessions (id, workspace_id, title, model) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(workspace_id)
        .bind(title)
        .bind(model)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_get_session(
        &self,
        id: &str,
    ) -> Result<Option<AgentSessionRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentSessionRecord>("SELECT * FROM agent_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn agent_list_sessions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentSessionRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentSessionRecord>(
            "SELECT * FROM agent_sessions WHERE workspace_id = ? ORDER BY created_at DESC",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn agent_update_session_title(
        &self,
        id: &str,
        title: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET title = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(title)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_update_session_model(
        &self,
        id: &str,
        model: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET model = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(model)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 写入/清除 session 的 ACP 会话 id（handshake 完成后落库，供断线重拉时
    /// `session/resume` 恢复上下文）。None 清空（如重拉后 session/delete 删除）。
    pub async fn agent_set_acp_session_id(
        &self,
        id: &str,
        acp_session_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET acp_session_id = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(acp_session_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// upsert/删除 session 的 ACP 配置项：value=Some 写入该 key，None 删除；
    /// map 为空时列置 NULL。config_state 非 JSON（历史脏数据）时视为空 map 重建。
    pub async fn agent_update_session_config_state(
        &self,
        id: &str,
        config_id: &str,
        value: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let row = self.agent_get_session(id).await?;
        let mut map: serde_json::Map<String, serde_json::Value> = row
            .and_then(|r| r.config_state)
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        match value {
            Some(v) => {
                map.insert(
                    config_id.to_string(),
                    serde_json::Value::String(v.to_string()),
                );
            }
            None => {
                map.remove(config_id);
            }
        }
        let serialized = if map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(map).to_string())
        };
        sqlx::query(
            "UPDATE agent_sessions SET config_state = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(serialized)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_archive_session(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_sessions SET status = 'archived', updated_at = datetime('now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn agent_delete_session(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Messages ────────────────────────────────────────────────

    pub async fn agent_add_message(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
        tool_calls: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        // 旧接口兼容：role=tool 的合并行推导 kind="tool"（重放时按旧格式跳过），
        // 其余为普通 message。
        let kind = if role == "tool" { "tool" } else { "message" };
        self.agent_add_message_v2(id, session_id, role, content, tool_calls, None, None, kind)
            .await
    }

    /// 新格式消息写入（全列）。`kind` 取值：message / tool_calls / tool_result / summary。
    #[allow(clippy::too_many_arguments)]
    pub async fn agent_add_message_v2(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        name: Option<&str>,
        kind: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_messages (id, session_id, role, content, tool_calls, tool_call_id, name, kind) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(session_id)
        .bind(role)
        .bind(content)
        .bind(tool_calls)
        .bind(tool_call_id)
        .bind(name)
        .bind(kind)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 回填 tool_calls 行里指定调用的 arguments（claude-code-acp 的 ToolCall 首帧
    /// rawInput 常是 {}，真正的参数经后续 ToolCallUpdate.rawInput 才到达；若不回填，
    /// 重载后历史卡片无操作内容）。只重写 `tool_calls` JSON 数组中 id 匹配项的
    /// `arguments` 字段，其余字段（name/tool_kind/diffs/locations）保持原样。
    pub async fn agent_update_tool_call_args(
        &self,
        tool_call_id: &str,
        args: &str,
    ) -> Result<(), sqlx::Error> {
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT rowid, tool_calls FROM agent_messages \
             WHERE kind = 'tool_calls' AND tool_call_id = ?",
        )
        .bind(tool_call_id)
        .fetch_all(&self.pool)
        .await?;
        for (rowid, json) in rows {
            let Ok(mut calls) = serde_json::from_str::<Vec<serde_json::Value>>(&json) else {
                continue; // 畸形 JSON 跳过（best-effort，不影响实时路径）
            };
            let mut touched = false;
            for c in &mut calls {
                if c.get("id").and_then(|v| v.as_str()) == Some(tool_call_id) {
                    c["arguments"] = serde_json::Value::String(args.to_string());
                    touched = true;
                }
            }
            if touched {
                sqlx::query("UPDATE agent_messages SET tool_calls = ? WHERE rowid = ?")
                    .bind(serde_json::to_string(&calls).unwrap_or_default())
                    .bind(rowid)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// ACP 工具行 upsert：按 (session_id, tool_call_id, kind) 定位 agent_messages
    /// 行。历史版本对每个 ACP 事件纯 INSERT，同一 tool_call_id 会产生多行
    /// （tool_call 每次事件、tool_result 每个中间态各一行），前端刷新后重复卡片。
    /// 此函数把同组多行收敛为一行（保留 rowid 最大者，删除其余）后做 UPDATE/INSERT。
    ///
    /// 不能加唯一索引收敛——compact.rs 压缩时会带相同 (session_id, tool_call_id,
    /// kind) 重插 kept 段，唯一索引会直接冲突报错。
    ///
    /// tool_calls 覆盖规则：新 JSON 长度 >= 旧值时覆盖（新帧通常带更完整的
    /// rawInput/diffs），否则保持旧值（如回放带来的短占位不覆盖已回填的真实参数）。
    pub async fn agent_upsert_tool_call(
        &self,
        id: &str,
        session_id: &str,
        tool_call_id: &str,
        name: Option<&str>,
        tool_calls_json: &str,
    ) -> Result<(), sqlx::Error> {
        let rows: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
            "SELECT rowid, tool_calls FROM agent_messages \
             WHERE session_id = ? AND tool_call_id = ? AND kind = 'tool_calls' \
             ORDER BY rowid",
        )
        .bind(session_id)
        .bind(tool_call_id)
        .fetch_all(&self.pool)
        .await?;
        match rows.split_last() {
            Some((max, rest)) => {
                // 收敛：删除非 rowid 最大的其余行
                for (rid, _) in rest {
                    sqlx::query("DELETE FROM agent_messages WHERE rowid = ?")
                        .bind(rid)
                        .execute(&self.pool)
                        .await?;
                }
                let (rowid, old_json) = max;
                if tool_calls_json.len() >= old_json.len() {
                    sqlx::query(
                        "UPDATE agent_messages SET tool_calls = ?, name = COALESCE(?, name) \
                         WHERE rowid = ?",
                    )
                    .bind(tool_calls_json)
                    .bind(name)
                    .bind(rowid)
                    .execute(&self.pool)
                    .await?;
                } else if name.is_some() {
                    // 旧 JSON 更完整：仅补名（不覆盖已回填的完整 tool_calls）
                    sqlx::query("UPDATE agent_messages SET name = ? WHERE rowid = ?")
                        .bind(name)
                        .bind(rowid)
                        .execute(&self.pool)
                        .await?;
                }
                Ok(())
            }
            None => {
                self.agent_add_message_v2(
                    id,
                    session_id,
                    "assistant",
                    "",
                    Some(tool_calls_json),
                    Some(tool_call_id),
                    name,
                    "tool_calls",
                )
                .await
            }
        }
    }

    /// tool_result upsert：同 [`Self::agent_upsert_tool_call`] 的收敛规则。content
    /// 覆盖规则：新 content 非空时覆盖（终态覆盖中间态空占位）；新 content 为空且
    /// 已有非空 content 则不动（空占位不抹掉真实结果）。
    pub async fn agent_upsert_tool_result(
        &self,
        id: &str,
        session_id: &str,
        tool_call_id: &str,
        name: Option<&str>,
        content: &str,
    ) -> Result<(), sqlx::Error> {
        let rows: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
            "SELECT rowid, content FROM agent_messages \
             WHERE session_id = ? AND tool_call_id = ? AND kind = 'tool_result' \
             ORDER BY rowid",
        )
        .bind(session_id)
        .bind(tool_call_id)
        .fetch_all(&self.pool)
        .await?;
        match rows.split_last() {
            Some((max, rest)) => {
                for (rid, _) in rest {
                    sqlx::query("DELETE FROM agent_messages WHERE rowid = ?")
                        .bind(rid)
                        .execute(&self.pool)
                        .await?;
                }
                let (rowid, old_content) = max;
                if !content.is_empty() || old_content.is_empty() {
                    sqlx::query(
                        "UPDATE agent_messages SET content = ?, name = COALESCE(?, name) \
                         WHERE rowid = ?",
                    )
                    .bind(content)
                    .bind(name)
                    .bind(rowid)
                    .execute(&self.pool)
                    .await?;
                } else if name.is_some() {
                    sqlx::query("UPDATE agent_messages SET name = ? WHERE rowid = ?")
                        .bind(name)
                        .bind(rowid)
                        .execute(&self.pool)
                        .await?;
                }
                Ok(())
            }
            None => {
                self.agent_add_message_v2(
                    id,
                    session_id,
                    "assistant",
                    content,
                    None,
                    Some(tool_call_id),
                    name,
                    "tool_result",
                )
                .await
            }
        }
    }

    pub async fn agent_list_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentMessageRecord>, sqlx::Error> {
        // 按 rowid（插入顺序）而非 created_at 排序：created_at 是秒级精度
        // （datetime('now')），同一秒内多条消息（工具帧密集到达时很常见）之间的
        // 相对顺序由 rowid 兜底，但 created_at 不同秒但插入乱序的场景（ACP 落库
        // 走并发任务，wall-clock 与插入顺序可能错开）下按 created_at 排会把后插入
        // 的行提前。rowid 自增且与插入顺序严格一致，是唯一的正确排序键。
        sqlx::query_as::<_, AgentMessageRecord>(
            "SELECT * FROM agent_messages WHERE session_id = ? ORDER BY rowid",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用：给 workspace 补写 llm_model_id（正式 CRUD 写入口随 Task 7
    /// 提供）。llm_bridge 测试用它构造已配置链路。
    #[cfg(test)]
    impl Database {
        pub(crate) async fn agent_set_workspace_llm_model_id(
            &self,
            id: &str,
            model_id: &str,
        ) -> Result<(), sqlx::Error> {
            sqlx::query(
                "UPDATE agent_workspaces SET llm_model_id = ?, updated_at = datetime('now') \
                 WHERE id = ?",
            )
            .bind(model_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_workspace_crud() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1",
            "my-proj",
            "nas",
            "host",
            "/home/user/proj",
            None,
            None,
            "",
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_workspace(
            "w2",
            "dproj",
            "nas",
            "docker",
            "/container/work",
            Some("node:20"),
            Some("dev-ctr"),
            "",
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.name, "my-proj");
        assert_eq!(ws.client_id, "nas");
        assert_eq!(ws.runtime_type, "host");
        assert_eq!(ws.root_path, "/home/user/proj");
        assert!(ws.docker_image.is_none());
        assert!(ws.docker_container_id.is_none());

        // docker 运行时：image 与 container_id 均持久化
        let ws = db.agent_get_workspace("w2").await.unwrap().unwrap();
        assert_eq!(ws.runtime_type, "docker");
        assert_eq!(ws.docker_image.as_deref(), Some("node:20"));
        assert_eq!(ws.docker_container_id.as_deref(), Some("dev-ctr"));

        assert_eq!(db.agent_list_workspaces().await.unwrap().len(), 2);

        db.agent_update_workspace(
            "w1",
            "renamed",
            "/new/path",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.name, "renamed");
        assert_eq!(ws.root_path, "/new/path");

        db.agent_delete_workspace("w1").await.unwrap();
        assert!(db.agent_get_workspace("w1").await.unwrap().is_none());

        // approval_mode / system_prompt 默认值与读写
        let ws = db.agent_get_workspace("w2").await.unwrap().unwrap();
        assert_eq!(ws.approval_mode, "safe");
        assert!(ws.system_prompt.is_none());

        db.agent_delete_workspace("w2").await.unwrap();
        assert!(db.agent_get_workspace("w2").await.unwrap().is_none());
    }

    /// ACP 三个新字段（agent_type/agent_path/llm_model_id）的创建→读取→更新→读取
    /// 完整往返。COALESCE 更新语义：`None` 保持原值。
    #[tokio::test]
    async fn test_workspace_acp_fields_roundtrip() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1",
            "acp-proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "gemini",
            Some("/opt/acp-agent"),
            Some("m1"),
            None,
        )
        .await
        .unwrap();

        // 创建后读取：三个字段全部落库
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "gemini");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-agent"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m1"));

        // 更新为另一组值（含改变 agent_type）
        db.agent_update_workspace(
            "w1",
            "acp-proj",
            "/workspace",
            None,
            None,
            Some("claude"),
            Some("/opt/acp-claude"),
            Some("m2"),
            None,
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "claude");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-claude"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m2"));

        // COALESCE：None 保持原值
        db.agent_update_workspace(
            "w1",
            "acp-proj",
            "/workspace",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "claude");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-claude"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m2"));

        // 可空字段显式清空为 None
        db.agent_update_workspace(
            "w1",
            "acp-proj",
            "/workspace",
            None,
            None,
            None,
            None,
            Some("m3"),
            None,
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_type, "claude");
        assert_eq!(ws.agent_path.as_deref(), Some("/opt/acp-claude"));
        assert_eq!(ws.llm_model_id.as_deref(), Some("m3"));
    }

    #[tokio::test]
    async fn test_session_crud_and_archive() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", Some("fix bug"), Some("gpt-4o"))
            .await
            .unwrap();
        db.agent_create_session("s2", "w1", None, None)
            .await
            .unwrap();

        let sessions = db.agent_list_sessions("w1").await.unwrap();
        assert_eq!(sessions.len(), 2);

        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.title.as_deref(), Some("fix bug"));
        assert_eq!(s.status, "active");
        assert_eq!(s.model.as_deref(), Some("gpt-4o"));

        db.agent_update_session_title("s1", "renamed session")
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.title.as_deref(), Some("renamed session"));

        db.agent_archive_session("s1").await.unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.status, "archived");

        db.agent_delete_session("s2").await.unwrap();
        assert_eq!(db.agent_list_sessions("w1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_update_session_model() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();

        // 更新为新模型
        db.agent_update_session_model("s1", Some("claude-opus-5"))
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-opus-5"));

        // 空（None）清除，回退默认
        db.agent_update_session_model("s1", None).await.unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert!(s.model.is_none());
    }

    #[tokio::test]
    async fn test_message_append_and_list() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        db.agent_add_message("m1", "s1", "user", "帮我修 bug", None)
            .await
            .unwrap();
        db.agent_add_message(
            "m2",
            "s1",
            "assistant",
            "好的，我先看下代码",
            Some(r#"[{"name":"shell","args":{"cmd":"ls"},"result":"a.rs"}]"#),
        )
        .await
        .unwrap();
        db.agent_add_message("m3", "s1", "user", "继续", None)
            .await
            .unwrap();

        let msgs = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        assert!(msgs[1].tool_calls.as_deref().unwrap().contains("shell"));
        assert_eq!(msgs[2].content, "继续");

        // 删除会话级联删除消息
        db.agent_delete_session("s1").await.unwrap();
        assert!(db.agent_list_messages("s1").await.unwrap().is_empty());
    }

    /// 插入顺序 ≠ created_at 顺序时，列表必须按插入顺序（rowid）返回。
    ///
    /// ACP 路径的落库走并发任务（tool_call/tool_result 直接落，文本/thought 缓冲
    /// 到回合终态才落），wall-clock（created_at，秒级精度）与插入顺序可能错开：
    /// 快速连续的工具帧在同一秒内多条插入时 created_at 相同靠 rowid 兜底没问题，
    /// 但「回合中段的 tool_result 在 N 秒落库、回合末的文本合并在 N+1 秒落库」
    /// 这种正常时序下按 created_at 排序本就对——真正出错的是旧排序键
    /// `ORDER BY created_at, rowid` 在「晚到的帧带着晚 created_at 却应该先显示」
    /// 时仍按 created_at 优先排，会把后插入的行甩到列表尾部、语义上提前。
    ///
    /// 这个测试直接用 SQL 显式篡改 created_at 制造「rowid 升序但 created_at 降序」
    /// 的数据（与 ACP 并发落库的真实效果一致），断言按 rowid 而非 created_at 返回。
    #[tokio::test]
    async fn test_message_list_orders_by_rowid_not_created_at() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        db.agent_add_message("m1", "s1", "user", "第一", None)
            .await
            .unwrap();
        db.agent_add_message("m2", "s1", "assistant", "第二", None)
            .await
            .unwrap();
        db.agent_add_message("m3", "s1", "user", "第三", None)
            .await
            .unwrap();

        // 把 m2 的 created_at 改成未来，m3 保持现在：若按 created_at 排序，
        // m2（未来）会排到 m3（现在）之后；按 rowid 排序则 m2 仍在 m3 之前。
        sqlx::query("UPDATE agent_messages SET created_at = '2999-01-01 00:00:00' WHERE id = 'm2'")
            .execute(&db.pool)
            .await
            .unwrap();

        let msgs = db.agent_list_messages("s1").await.unwrap();
        let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["m1", "m2", "m3"],
            "list must follow insertion order (rowid), not created_at"
        );
    }

    /// 回填 tool_calls 行 arguments（claude-code-acp rawInput 晚到场景）：只重写
    /// id 匹配项的 arguments，其余字段与无关调用不受影响。
    #[tokio::test]
    async fn test_update_tool_call_args() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        let calls = serde_json::json!([
            {"id": "c1", "name": "Terminal", "arguments": "{}", "tool_kind": "execute"},
            {"id": "c2", "name": "Read", "arguments": "{\"path\":\"a.rs\"}", "tool_kind": "read"},
        ]);
        db.agent_add_message_v2(
            "m1",
            "s1",
            "assistant",
            "",
            Some(&calls.to_string()),
            Some("c1"),
            Some("Terminal"),
            "tool_calls",
        )
        .await
        .unwrap();

        db.agent_update_tool_call_args("c1", "{\"command\":\"echo hi\"}")
            .await
            .unwrap();

        let msgs = db.agent_list_messages("s1").await.unwrap();
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(msgs[0].tool_calls.as_deref().unwrap()).unwrap();
        // c1 的 arguments 已回填为真实命令
        assert_eq!(parsed[0]["arguments"], "{\"command\":\"echo hi\"}");
        // c1 其余字段不受影响
        assert_eq!(parsed[0]["name"], "Terminal");
        assert_eq!(parsed[0]["tool_kind"], "execute");
        // 无关调用 c2 不受影响
        assert_eq!(parsed[1]["arguments"], "{\"path\":\"a.rs\"}");

        // 不存在的 tool_call_id：无错误、无变更
        db.agent_update_tool_call_args("nope", "x").await.unwrap();
        let msgs = db.agent_list_messages("s1").await.unwrap();
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(msgs[0].tool_calls.as_deref().unwrap()).unwrap();
        assert_eq!(parsed[0]["arguments"], "{\"command\":\"echo hi\"}");
    }

    /// tool_result upsert 去重：先写中间态空 content，再写终态非空 content →
    /// 只剩 1 行且 content 为终态值。
    #[tokio::test]
    async fn test_upsert_tool_result_dedup_content() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 中间态：空 content（ToolCallUpdate 首帧常无 raw_output）
        db.agent_upsert_tool_result("m1", "s1", "c1", Some("shell"), "")
            .await
            .unwrap();
        // 终态：非空 content
        db.agent_upsert_tool_result("m2", "s1", "c1", Some("shell"), "a.rs")
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(rows.len(), 1, "upsert should converge to one row: {rows:?}");
        assert_eq!(rows[0].kind, "tool_result");
        assert_eq!(rows[0].content, "a.rs");
        assert_eq!(rows[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(rows[0].name.as_deref(), Some("shell"));
    }

    /// 反向顺序：先非空后空 → 空 content 不得覆盖已有非空结果。
    #[tokio::test]
    async fn test_upsert_tool_result_empty_does_not_clear() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        db.agent_upsert_tool_result("m1", "s1", "c1", Some("shell"), "result")
            .await
            .unwrap();
        // 迟到的空占位帧：不覆盖
        db.agent_upsert_tool_result("m2", "s1", "c1", Some("shell"), "")
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "result");
    }

    /// tool_calls upsert：保更长/更完整的 JSON（回放短占位不覆盖已回填参数）。
    #[tokio::test]
    async fn test_upsert_tool_call_keeps_longer_json() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 短 JSON（首帧 rawInput={} 占位）
        let short = r#"[{"id":"c1","name":"shell","arguments":"{}"}]"#;
        db.agent_upsert_tool_call("m1", "s1", "c1", Some("shell"), short)
            .await
            .unwrap();
        // 长 JSON（参数/字段更完整）
        let long = r#"[{"id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}","tool_kind":"execute"}]"#;
        db.agent_upsert_tool_call("m2", "s1", "c1", Some("shell"), long)
            .await
            .unwrap();
        // 更短的 JSON 再写：不得回退已保存的完整 JSON
        db.agent_upsert_tool_call("m3", "s1", "c1", Some("shell"), r#"[{"id":"c1"}]"#)
            .await
            .unwrap();

        let rows = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(rows.len(), 1, "upsert should converge to one row: {rows:?}");
        assert_eq!(rows[0].kind, "tool_calls");
        let json = rows[0].tool_calls.as_deref().unwrap();
        assert!(
            json.contains("tool_kind") && json.contains("ls"),
            "longer json should be kept: {json}"
        );
        assert_eq!(rows[0].name.as_deref(), Some("shell"));
    }

    #[tokio::test]
    async fn test_delete_workspace_cascades() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        db.agent_add_message("m1", "s1", "user", "hi", None)
            .await
            .unwrap();

        db.agent_delete_workspace("w1").await.unwrap();
        assert!(db.agent_list_sessions("w1").await.unwrap().is_empty());
        assert!(db.agent_list_messages("s1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_message_v2_columns_roundtrip() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // assistant tool_calls 行
        db.agent_add_message_v2(
            "m1",
            "s1",
            "assistant",
            "",
            Some(r#"[{"id":"c1","type":"function","function":{"name":"shell","arguments":"{}"}}]"#),
            None,
            None,
            "tool_calls",
        )
        .await
        .unwrap();
        // tool 结果行
        db.agent_add_message_v2(
            "m2",
            "s1",
            "tool",
            "exit_code=0",
            None,
            Some("c1"),
            Some("shell"),
            "tool_result",
        )
        .await
        .unwrap();
        // 旧接口写入 → kind 自动推导
        db.agent_add_message("m3", "s1", "user", "hi", None)
            .await
            .unwrap();
        db.agent_add_message("m4", "s1", "tool", "", Some(r#"[{"name":"shell"}]"#))
            .await
            .unwrap();

        let msgs = db.agent_list_messages("s1").await.unwrap();
        assert_eq!(msgs[0].kind, "tool_calls");
        assert!(msgs[0].tool_call_id.is_none());
        assert_eq!(msgs[1].kind, "tool_result");
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(msgs[1].name.as_deref(), Some("shell"));
        assert_eq!(msgs[2].kind, "message");
        assert_eq!(msgs[3].kind, "tool"); // 旧格式保持 role=tool 的推导
    }

    #[tokio::test]
    async fn test_config_state_upsert_and_clear() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "w", "c1", "host", "/tmp", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 初始为空
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert!(s.config_state.is_none());

        // upsert 两个 key
        db.agent_update_session_config_state("s1", "mode", Some("plan"))
            .await
            .unwrap();
        db.agent_update_session_config_state("s1", "effort", Some("high"))
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        let map: serde_json::Value =
            serde_json::from_str(s.config_state.as_deref().unwrap()).unwrap();
        assert_eq!(map["mode"], "plan");
        assert_eq!(map["effort"], "high");

        // 覆盖已有 key
        db.agent_update_session_config_state("s1", "mode", Some("default"))
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        let map: serde_json::Value =
            serde_json::from_str(s.config_state.as_deref().unwrap()).unwrap();
        assert_eq!(map["mode"], "default");
        assert_eq!(map["effort"], "high");

        // 清空一个 key；清空全部后列回到 NULL
        db.agent_update_session_config_state("s1", "mode", None)
            .await
            .unwrap();
        db.agent_update_session_config_state("s1", "effort", None)
            .await
            .unwrap();
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert!(s.config_state.is_none());
    }

    /// agent_config_overrides（v4 列）的创建→读取→更新→清空完整往返。
    /// update 语义：None 保持原值；Some("{}") 显式清空（与 llm_model_id 的
    /// 「不支持清空」不同，见 spec 决策表）。
    #[tokio::test]
    async fn test_workspace_config_overrides_roundtrip() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1",
            "acp-proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "claude-code",
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // 创建时未配置 → None
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert!(ws.agent_config_overrides.is_none());

        // 写入配置
        db.agent_update_workspace(
            "w1",
            "acp-proj",
            "/workspace",
            None,
            None,
            None,
            None,
            None,
            Some(r#"{"model":"sonnet","fast":"haiku"}"#),
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.agent_config_overrides.as_deref(),
            Some(r#"{"model":"sonnet","fast":"haiku"}"#)
        );

        // COALESCE：None 保持原值
        db.agent_update_workspace(
            "w1",
            "acp-proj",
            "/workspace",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(
            ws.agent_config_overrides.as_deref(),
            Some(r#"{"model":"sonnet","fast":"haiku"}"#)
        );

        // 显式 Some("") 写入空串由 API 层拦截（归一化 None）；DB 层原样写入 "{}"
        // 表示清空——这里验证 DB 层忠实存储传入值
        db.agent_update_workspace(
            "w1",
            "acp-proj",
            "/workspace",
            None,
            None,
            None,
            None,
            None,
            Some("{}"),
        )
        .await
        .unwrap();
        let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
        assert_eq!(ws.agent_config_overrides.as_deref(), Some("{}"));
    }
}
