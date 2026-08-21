use crate::llm::{LlmState, ProviderConfig};

/// 模型上游协议类型。
///
/// 与 [`agent::compact::context_limit_for`] 同为 per-model `extra_config` JSON 键；
/// 本枚举读取 `upstream_protocol` 键，默认 `ChatCompletions`（零迁移成本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpstreamProtocol {
    /// 标准 Chat Completions API（`/v1/chat/completions`），默认值。
    #[default]
    ChatCompletions,
    /// OpenAI Responses API（`/v1/responses`）。
    Responses,
}

/// 从 model `extra_config` JSON 读取 `upstream_protocol` 键，返回协议类型。
///
/// - `"responses"` → [`UpstreamProtocol::Responses`]
/// - 其余值 / 缺失 / JSON 解析失败 → [`UpstreamProtocol::ChatCompletions`]（默认）
pub fn parse_upstream_protocol(extra_config: Option<&str>) -> UpstreamProtocol {
    let Some(ec) = extra_config else {
        return UpstreamProtocol::ChatCompletions;
    };
    serde_json::from_str::<serde_json::Value>(ec)
        .ok()
        .and_then(|v| v.get("upstream_protocol")?.as_str().map(str::to_lowercase))
        .map_or(UpstreamProtocol::ChatCompletions, |s| {
            if s == "responses" {
                UpstreamProtocol::Responses
            } else {
                UpstreamProtocol::ChatCompletions
            }
        })
}

/// 模型路由解析失败的类别。
#[derive(Debug)]
pub enum ResolveError {
    /// 模型名/别名未匹配（404，应附可用模型列表）
    ModelNotFound(String),
    /// 模型所属提供商被禁用
    ProviderDisabled(String),
    /// 数据库不可用或查询失败
    Db(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelNotFound(m) => write!(f, "model '{}' not found", m),
            Self::ProviderDisabled(m) => write!(f, "provider for model '{}' is disabled", m),
            Self::Db(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ResolveError {}

/// 候选链中的单个候选（provider 敏感字段已解密，可直接发请求）。
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 候选模型所属 provider 配置。
    pub provider: ProviderConfig,
    /// 上游真实 model 值（写入请求体 `model` 字段）。
    pub model_name: String,
    /// 模型记录 id（熔断器粒度键）。
    pub model_id: String,
    /// 组内尝试顺序（小者优先）；单模型链为 0。
    pub priority: i64,
    /// 模型上游协议（从 `extra_config` 解析）。
    pub upstream_protocol: UpstreamProtocol,
}

/// 一次请求的有序候选链。
#[derive(Debug, Clone)]
pub struct CandidateChain {
    /// 按 priority 升序的候选列表。
    pub candidates: Vec<Candidate>,
    /// 命中模型组时为组名；单模型解析为 None。
    pub group_name: Option<String>,
}

/// 解析模型名 / 别名 / 模型组名为有序候选链。
///
/// 解析顺序：`model_name` 精确 → `alias` → 组名（组 enabled）。
/// 组成员中模型禁用或 provider 禁用者被过滤；过滤后为空视同 `ModelNotFound`。
///
/// 全部走 [`crate::llm::route_cache::RouteCache`] 内存快照，不触碰 DB：
/// 数据一致性由管理面写入后的 `invalidate` 保证（见 `mgmt/api/llm.rs`）。
pub async fn resolve_with_failover(
    state: &LlmState,
    model: &str,
) -> Result<CandidateChain, ResolveError> {
    let db = state
        .db
        .as_ref()
        .ok_or_else(|| ResolveError::Db("database not available".into()))?;
    let snap = state
        .route_cache
        .snapshot(Some(db), state.cipher.as_ref())
        .await;

    // 1) model_name / alias 精确命中 → 单元素链（现有行为）
    if let Some(m) = snap.find_model_by_name_or_alias(model) {
        let provider = snap
            .provider(&m.provider_id)
            .ok_or_else(|| ResolveError::ModelNotFound(model.to_string()))?;
        if !provider.enabled {
            return Err(ResolveError::ProviderDisabled(model.to_string()));
        }
        return Ok(CandidateChain {
            candidates: vec![Candidate {
                provider: provider.clone(),
                model_name: m.model_name.clone(),
                model_id: m.id.clone(),
                priority: 0,
                upstream_protocol: parse_upstream_protocol(m.extra_config.as_deref()),
            }],
            group_name: None,
        });
    }

    // 2) 组名命中
    let Some(group) = snap.group_by_name(model) else {
        return Err(ResolveError::ModelNotFound(model.to_string()));
    };

    let mut candidates = Vec::new();
    for (model_id, priority) in &group.members {
        let Some(m) = snap.model(model_id) else {
            continue; // 孤儿成员（模型已删）：过滤
        };
        if !m.enabled {
            continue; // 模型禁用：过滤
        }
        let Some(provider) = snap.provider(&m.provider_id) else {
            continue; // provider 缺失（row 悬空）：过滤
        };
        if !provider.enabled {
            continue; // provider 禁用：过滤
        }
        candidates.push(Candidate {
            provider: provider.clone(),
            model_name: m.model_name.clone(),
            model_id: m.id.clone(),
            priority: *priority as i64,
            upstream_protocol: parse_upstream_protocol(m.extra_config.as_deref()),
        });
    }

    if candidates.is_empty() {
        return Err(ResolveError::ModelNotFound(model.to_string()));
    }

    Ok(CandidateChain {
        candidates,
        group_name: Some(group.name.clone()),
    })
}

/// Resolve a model name (or alias) to a ProviderConfig with the actual model_name.
/// Returns (provider, actual_model_name, model_id) or a typed error.
///
/// 单模型薄封装：内部走 `resolve_with_failover`，取候选链首元素。
pub async fn resolve_model(
    state: &LlmState,
    model: &str,
) -> Result<(ProviderConfig, String, String), ResolveError> {
    let chain = resolve_with_failover(state, model).await?;
    let c = chain
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| ResolveError::ModelNotFound(model.to_string()))?;
    Ok((c.provider, c.model_name, c.model_id))
}

/// 把路由解析错误转成 HTTP 响应。
/// 模型未找到时按 spec 返回 404 + 可用模型列表；其余错误返回 404 + 错误信息。
pub async fn resolve_error_response(
    state: &LlmState,
    err: ResolveError,
) -> axum::response::Response {
    use super::upstream::{error_response, model_not_found_response};

    match err {
        ResolveError::ModelNotFound(_) => {
            let models = list_available_models(state).await.unwrap_or_default();
            let ids: Vec<String> = models
                .iter()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect();
            model_not_found_response(err.to_string(), ids)
        }
        other => error_response(
            axum::http::StatusCode::NOT_FOUND,
            other.to_string(),
            "invalid_request_error",
        ),
    }
}

/// 判断模型名（或别名/组名）能否被网关解析：模型须自身启用且所属 provider
/// 启用（与 `available_models` 过滤语义一致）；组只须启用（成员在路由期
/// 逐个解析，failover 自行跳过禁用项）。llm_bridge 用它决定保留还是覆盖
/// 请求体中的 model。
pub async fn model_resolvable(state: &LlmState, name: &str) -> bool {
    let Some(db) = state.db.as_ref() else {
        return false;
    };
    let snap = state
        .route_cache
        .snapshot(Some(db), state.cipher.as_ref())
        .await;
    if let Some(m) = snap.find_model_by_name_or_alias(name) {
        return snap.provider(&m.provider_id).is_some_and(|p| p.enabled);
    }
    snap.group_by_name(name).is_some()
}

/// Get list of all enabled models (for /v1/models).
/// Only returns models whose provider is also enabled.
pub async fn list_available_models(state: &LlmState) -> Result<Vec<serde_json::Value>, String> {
    let db = state.db.as_ref().ok_or("database not available")?;
    let snap = state
        .route_cache
        .snapshot(Some(db), state.cipher.as_ref())
        .await;
    Ok(snap.available_models())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmState;

    #[test]
    fn test_resolve_without_db_returns_error() {
        let state = LlmState::new(None, None);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(resolve_model(&state, "any-model"));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("database not available"));
    }

    #[tokio::test]
    async fn test_resolve_decrypts_provider_api_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();

        let cipher = crate::llm::crypto::LlmCipher::from_master_key([7u8; 32]);
        let encrypted = cipher.encrypt("sk-real-upstream-key");

        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            &encrypted,
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "", "[]", true, None)
            .await
            .unwrap();

        // DB 里必须是密文
        let stored = db.llm_get_provider(&pid).await.unwrap().unwrap();
        assert!(stored.api_key.starts_with("enc:v1:"));

        let state = LlmState::new(Some(db), Some(cipher));
        let (provider, model, model_id) = resolve_model(&state, "deepseek-chat").await.unwrap();
        assert_eq!(model, "deepseek-chat");
        assert_eq!(model_id, mid);
        assert_eq!(
            provider.api_key, "sk-real-upstream-key",
            "resolve 应解密出明文 key"
        );
    }

    #[tokio::test]
    async fn test_resolve_legacy_plaintext_key_passthrough() {
        // 历史明文（无 enc 前缀）应原样可用
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "sk-plain",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "", "[]", true, None)
            .await
            .unwrap();

        let state = LlmState::new(
            Some(db),
            Some(crate::llm::crypto::LlmCipher::from_master_key([9u8; 32])),
        );
        let (provider, _, _) = resolve_model(&state, "deepseek-chat").await.unwrap();
        assert_eq!(provider.api_key, "sk-plain");
    }

    #[tokio::test]
    async fn test_resolve_disabled_provider_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "k",
            None::<&str>,
            None::<&str>,
            false,
        )
        .await
        .unwrap();
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "", "[]", true, None)
            .await
            .unwrap();

        let state = LlmState::new(Some(db), None);
        let err = resolve_model(&state, "deepseek-chat").await.unwrap_err();
        assert!(matches!(err, ResolveError::ProviderDisabled(_)));
    }

    #[tokio::test]
    async fn test_resolve_unknown_model_is_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let state = LlmState::new(Some(db), None);
        let err = resolve_model(&state, "nope").await.unwrap_err();
        assert!(matches!(err, ResolveError::ModelNotFound(_)));
    }

    /// 测试辅助：建临时 DB + 一个 provider（p1, deepseek）+ 若干模型。
    async fn state_with_models(
        models: &[(&str, &str, &str)], // (model_id, model_name, alias)
    ) -> (tempfile::TempDir, LlmState) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        db.llm_save_provider(
            "p1",
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        for (id, name, alias) in models {
            db.llm_save_model(id, "p1", name, alias, "[]", true, None)
                .await
                .unwrap();
        }
        let state = LlmState::new(Some(db), None);
        (tmp, state)
    }

    #[tokio::test]
    async fn test_resolve_single_model_chain() {
        let (_t, state) = state_with_models(&[("m1", "deepseek-chat", "")]).await;
        let chain = resolve_with_failover(&state, "deepseek-chat")
            .await
            .unwrap();
        assert!(chain.group_name.is_none());
        assert_eq!(chain.candidates.len(), 1);
        assert_eq!(chain.candidates[0].model_name, "deepseek-chat");
        assert_eq!(chain.candidates[0].model_id, "m1");
    }

    #[tokio::test]
    async fn test_resolve_group_chain_ordered() {
        let (_t, state) = state_with_models(&[
            ("m1", "model-a", ""),
            ("m2", "model-b", ""),
            ("m3", "model-c", ""),
        ])
        .await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members(
            "g1",
            &[("m3".into(), 3), ("m1".into(), 1), ("m2".into(), 2)],
        )
        .await
        .unwrap();

        let chain = resolve_with_failover(&state, "router").await.unwrap();
        assert_eq!(chain.group_name.as_deref(), Some("router"));
        let names: Vec<&str> = chain
            .candidates
            .iter()
            .map(|c| c.model_name.as_str())
            .collect();
        assert_eq!(names, ["model-a", "model-b", "model-c"]);
        let prios: Vec<i64> = chain.candidates.iter().map(|c| c.priority).collect();
        assert_eq!(prios, [1, 2, 3]);
    }

    #[tokio::test]
    async fn test_model_name_wins_over_group() {
        // 解析优先级：model_name 先于组名——同名时组被遮蔽。
        let (_t, state) = state_with_models(&[("m1", "router", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();

        let chain = resolve_with_failover(&state, "router").await.unwrap();
        assert!(chain.group_name.is_none(), "model_name 命中时组不生效");
        assert_eq!(chain.candidates[0].model_id, "m1");
    }

    #[tokio::test]
    async fn test_group_filters_disabled_member() {
        let (_t, state) = state_with_models(&[("m1", "model-a", ""), ("m2", "model-b", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1), ("m2".into(), 2)])
            .await
            .unwrap();
        // 禁用 m1
        db.llm_save_model("m1", "p1", "model-a", "", "[]", false, None)
            .await
            .unwrap();

        let chain = resolve_with_failover(&state, "router").await.unwrap();
        assert_eq!(chain.candidates.len(), 1);
        assert_eq!(chain.candidates[0].model_id, "m2");
    }

    #[tokio::test]
    async fn test_group_empty_after_filter_is_not_found() {
        let (_t, state) = state_with_models(&[("m1", "model-a", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1)])
            .await
            .unwrap();
        db.llm_save_model("m1", "p1", "model-a", "", "[]", false, None)
            .await
            .unwrap();

        let err = resolve_with_failover(&state, "router").await.unwrap_err();
        assert!(matches!(err, ResolveError::ModelNotFound(_)));
    }

    #[tokio::test]
    async fn test_group_disabled_is_not_found() {
        let (_t, state) = state_with_models(&[("m1", "model-a", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", false)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1)])
            .await
            .unwrap();

        let err = resolve_with_failover(&state, "router").await.unwrap_err();
        assert!(matches!(err, ResolveError::ModelNotFound(_)));
    }

    #[tokio::test]
    async fn test_group_member_provider_disabled_filtered() {
        // provider 被禁用时其下组成员被过滤
        let (_t, state) = state_with_models(&[("m1", "model-a", ""), ("m2", "model-b", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_save_provider(
            "p2",
            "Kimi",
            "kimi",
            "https://api.moonshot.cn",
            "k2",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m3", "p2", "kimi-k2", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1), ("m3".into(), 2)])
            .await
            .unwrap();
        // 禁用 p2
        db.llm_save_provider(
            "p2",
            "Kimi",
            "kimi",
            "https://api.moonshot.cn",
            "k2",
            None::<&str>,
            None::<&str>,
            false,
        )
        .await
        .unwrap();

        let chain = resolve_with_failover(&state, "router").await.unwrap();
        assert_eq!(chain.candidates.len(), 1);
        assert_eq!(chain.candidates[0].model_id, "m1");
    }

    // ── UpstreamProtocol 测试 ─────────────────────────────────────

    #[test]
    fn parse_upstream_protocol_missing_returns_default() {
        assert_eq!(parse_upstream_protocol(None), UpstreamProtocol::ChatCompletions);
        assert_eq!(parse_upstream_protocol(Some("{}")), UpstreamProtocol::ChatCompletions);
    }

    #[test]
    fn parse_upstream_protocol_invalid_json_returns_default() {
        assert_eq!(
            parse_upstream_protocol(Some("not-json")),
            UpstreamProtocol::ChatCompletions
        );
    }

    #[test]
    fn parse_upstream_protocol_other_value_returns_default() {
        assert_eq!(
            parse_upstream_protocol(Some(r#"{"upstream_protocol":"chat"}"#)),
            UpstreamProtocol::ChatCompletions
        );
    }

    #[test]
    fn parse_upstream_protocol_responses() {
        assert_eq!(
            parse_upstream_protocol(Some(r#"{"upstream_protocol":"responses"}"#)),
            UpstreamProtocol::Responses
        );
    }

    #[test]
    fn parse_upstream_protocol_case_insensitive() {
        assert_eq!(
            parse_upstream_protocol(Some(r#"{"upstream_protocol":"Responses"}"#)),
            UpstreamProtocol::Responses
        );
    }

    #[tokio::test]
    async fn test_resolve_single_model_with_extra_config_protocol() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        db.llm_save_provider(
            "p1",
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        let extra = r#"{"upstream_protocol":"responses"}"#;
        db.llm_save_model("m1", "p1", "gpt-5-codex", "", "[]", true, Some(extra))
            .await
            .unwrap();

        let state = LlmState::new(Some(db), None);
        let chain = resolve_with_failover(&state, "gpt-5-codex")
            .await
            .unwrap();
        assert_eq!(chain.candidates.len(), 1);
        assert_eq!(
            chain.candidates[0].upstream_protocol,
            UpstreamProtocol::Responses
        );
    }

    #[tokio::test]
    async fn test_resolve_group_mixes_protocols() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        db.llm_save_provider(
            "p1",
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        // m1: chat completions（无 extra_config）
        db.llm_save_model("m1", "p1", "gpt-4o", "", "[]", true, None)
            .await
            .unwrap();
        // m2: responses 协议
        let extra = r#"{"upstream_protocol":"responses"}"#;
        db.llm_save_model("m2", "p1", "gpt-5-codex", "", "[]", true, Some(extra))
            .await
            .unwrap();

        db.llm_create_model_group("g1", "mixed", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1), ("m2".into(), 2)])
            .await
            .unwrap();

        let state = LlmState::new(Some(db), None);
        let chain = resolve_with_failover(&state, "mixed").await.unwrap();
        assert_eq!(chain.candidates.len(), 2);
        assert_eq!(
            chain.candidates[0].upstream_protocol,
            UpstreamProtocol::ChatCompletions
        );
        assert_eq!(
            chain.candidates[1].upstream_protocol,
            UpstreamProtocol::Responses
        );
    }
}
