//! 提供商管理逻辑：类型校验与默认端点（spec §Provider Type 对接约定）。
//!
//! 四个提供商均为 OpenAI 兼容格式，差异只在默认 Base URL 与认证字段。
//! 每个提供商可额外配置 `anthropic_base_url`，用于 Anthropic 协议透传。

/// 合法的 provider_type 列表
pub const VALID_PROVIDER_TYPES: [&str; 4] = ["deepseek", "volcengine", "kimi", "mimo"];

/// 各 provider_type 的默认 Base URL。
/// `mimo` 无默认端点，必须由用户提供。
#[must_use] 
pub fn default_base_url(provider_type: &str) -> Option<&'static str> {
    match provider_type {
        "deepseek" => Some("https://api.deepseek.com"),
        "volcengine" => Some("https://ark.cn-beijing.volces.com/api/v3"),
        "kimi" => Some("https://api.moonshot.cn"),
        _ => None,
    }
}

#[must_use] 
pub fn is_valid_provider_type(provider_type: &str) -> bool {
    VALID_PROVIDER_TYPES.contains(&provider_type)
}

/// 解析创建/更新请求中的 base_url：为空时回落到该类型的默认端点。
/// 无默认端点（mimo）且未提供时返回 Err。
pub fn resolve_base_url(provider_type: &str, base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim();
    if !trimmed.is_empty() {
        return Ok(trimmed.trim_end_matches('/').to_string());
    }
    default_base_url(provider_type)
        .map(str::to_string)
        .ok_or_else(|| format!("base_url is required for provider_type '{provider_type}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_types_are_valid() {
        for t in VALID_PROVIDER_TYPES {
            assert!(is_valid_provider_type(t));
        }
        assert!(!is_valid_provider_type("openai"));
        assert!(!is_valid_provider_type(""));
    }

    #[test]
    fn defaults_match_spec_table() {
        assert_eq!(
            default_base_url("deepseek"),
            Some("https://api.deepseek.com")
        );
        assert_eq!(
            default_base_url("volcengine"),
            Some("https://ark.cn-beijing.volces.com/api/v3")
        );
        assert_eq!(default_base_url("kimi"), Some("https://api.moonshot.cn"));
        assert_eq!(default_base_url("mimo"), None);
    }

    #[test]
    fn resolve_base_url_falls_back_to_default() {
        assert_eq!(
            resolve_base_url("deepseek", "").unwrap(),
            "https://api.deepseek.com"
        );
        assert!(resolve_base_url("mimo", "").is_err());
    }

    #[test]
    fn resolve_base_url_strips_trailing_slash() {
        assert_eq!(
            resolve_base_url("mimo", "https://mimo.example.com/").unwrap(),
            "https://mimo.example.com"
        );
    }
}
