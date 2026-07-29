use crate::server::llm::{LlmState, ProviderConfig};

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

/// Resolve a model name (or alias) to a ProviderConfig with the actual model_name.
/// Returns (provider, actual_model_name, model_id) or a typed error.
pub async fn resolve_model(
    state: &LlmState,
    model: &str,
) -> Result<(ProviderConfig, String, String), ResolveError> {
    let db = state
        .db
        .as_ref()
        .ok_or_else(|| ResolveError::Db("database not available".into()))?;

    // Find the model by name or alias (exact model_name first, then alias)
    let model_record = db
        .llm_find_model_by_name_or_alias(model)
        .await
        .map_err(|e| ResolveError::Db(format!("database error: {}", e)))?
        .ok_or_else(|| ResolveError::ModelNotFound(model.to_string()))?;

    // Find its provider
    let provider_record = db
        .llm_get_provider(&model_record.provider_id)
        .await
        .map_err(|e| ResolveError::Db(format!("database error: {}", e)))?
        .ok_or_else(|| ResolveError::ModelNotFound(model.to_string()))?;

    if provider_record.enabled == 0 {
        return Err(ResolveError::ProviderDisabled(model.to_string()));
    }

    // 提供商 API Key 落库时是密文（enc:v1: 前缀），使用前解密
    let api_key = super::crypto::decrypt_field(state.cipher.as_ref(), &provider_record.api_key)
        .map_err(|e| ResolveError::Db(format!("failed to decrypt provider api key: {}", e)))?;

    // extra_config 同样加密落库（可能含开关配置），用前解密；历史明文原样透传。
    let extra_config = match provider_record.extra_config {
        Some(ec) => Some(
            super::crypto::decrypt_field(state.cipher.as_ref(), &ec)
                .map_err(|e| ResolveError::Db(format!("failed to decrypt extra_config: {}", e)))?,
        ),
        None => None,
    };

    let provider = ProviderConfig {
        id: provider_record.id,
        name: provider_record.name,
        provider_type: provider_record.provider_type,
        base_url: provider_record.base_url,
        api_key,
        extra_config,
        anthropic_base_url: provider_record.anthropic_base_url,
        enabled: provider_record.enabled != 0,
        created_at: provider_record.created_at,
        updated_at: provider_record.updated_at,
    };

    Ok((provider, model_record.model_name, model_record.id))
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

/// Get list of all enabled models (for /v1/models).
/// Only returns models whose provider is also enabled.
pub async fn list_available_models(state: &LlmState) -> Result<Vec<serde_json::Value>, String> {
    let db = state.db.as_ref().ok_or("database not available")?;

    let models = db
        .llm_list_models()
        .await
        .map_err(|e| format!("database error: {}", e))?;

    let providers = db
        .llm_list_providers()
        .await
        .map_err(|e| format!("database error: {}", e))?;

    let enabled_providers: std::collections::HashSet<String> = providers
        .iter()
        .filter(|p| p.enabled != 0)
        .map(|p| p.id.clone())
        .collect();

    let provider_map: std::collections::HashMap<String, String> = providers
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();

    let result: Vec<serde_json::Value> = models
        .into_iter()
        .filter(|m| m.enabled != 0 && enabled_providers.contains(&m.provider_id))
        .map(|m| {
            serde_json::json!({
                "id": if m.alias.is_empty() { &m.model_name } else { &m.alias },
                "object": "model",
                "created": 0,
                "owned_by": provider_map
                    .get(&m.provider_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string()),
            })
        })
        .collect();

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::llm::LlmState;

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
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();

        let cipher = crate::server::llm::crypto::LlmCipher::from_master_key([7u8; 32]);
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
        db.llm_save_model(&mid, &pid, "deepseek-chat", "", "[]", true)
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
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
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
        db.llm_save_model(&mid, &pid, "deepseek-chat", "", "[]", true)
            .await
            .unwrap();

        let state = LlmState::new(
            Some(db),
            Some(crate::server::llm::crypto::LlmCipher::from_master_key(
                [9u8; 32],
            )),
        );
        let (provider, _, _) = resolve_model(&state, "deepseek-chat").await.unwrap();
        assert_eq!(provider.api_key, "sk-plain");
    }

    #[tokio::test]
    async fn test_resolve_disabled_provider_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
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
        db.llm_save_model(&mid, &pid, "deepseek-chat", "", "[]", true)
            .await
            .unwrap();

        let state = LlmState::new(Some(db), None);
        let err = resolve_model(&state, "deepseek-chat").await.unwrap_err();
        assert!(matches!(err, ResolveError::ProviderDisabled(_)));
    }

    #[tokio::test]
    async fn test_resolve_unknown_model_is_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let state = LlmState::new(Some(db), None);
        let err = resolve_model(&state, "nope").await.unwrap_err();
        assert!(matches!(err, ResolveError::ModelNotFound(_)));
    }
}
