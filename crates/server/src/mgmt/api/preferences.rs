//! 用户偏好（主题/语言/标题动效）持久化 API。

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use super::ApiState;

/// 偏好在 `server_settings` 中的键名。
pub const PREFERENCES_KEY: &str = "user_preferences";

const VALID_THEMES: &[&str] = &["system", "light", "dark"];
const VALID_LANGUAGES: &[&str] = &["system", "zh-CN", "en"];
const VALID_TITLE_EFFECTS: &[&str] = &["particles", "grid-wave", "none"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
/// 用户偏好（主题/语言/标题动效）。
pub struct Preferences {
    /// 主题：`system`/`light`/`dark`。
    pub theme: String,
    /// 语言：`system`/`zh-CN`/`en`。
    pub language: String,
    /// 标题动效：`particles`/`grid-wave`/`none`。
    pub title_effect: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            language: "system".to_string(),
            title_effect: "grid-wave".to_string(),
        }
    }
}

impl Preferences {
    fn validate(&self) -> Result<(), String> {
        if !VALID_THEMES.contains(&self.theme.as_str()) {
            return Err(format!("invalid theme: {}", self.theme));
        }
        if !VALID_LANGUAGES.contains(&self.language.as_str()) {
            return Err(format!("invalid language: {}", self.language));
        }
        if !VALID_TITLE_EFFECTS.contains(&self.title_effect.as_str()) {
            return Err(format!("invalid title_effect: {}", self.title_effect));
        }
        Ok(())
    }
}

/// `GET /api/preferences`：读取用户偏好（无记录时返回默认值）。
pub async fn get_preferences(State(state): State<ApiState>) -> Response {
    let db = match state.server_state.db() {
        Some(db) => db.clone(),
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
        }
    };
    match db.load_server_setting(PREFERENCES_KEY).await {
        Ok(Some(json)) => match serde_json::from_str::<Preferences>(&json) {
            Ok(prefs) => Json(prefs).into_response(),
            Err(_) => Json(Preferences::default()).into_response(),
        },
        Ok(None) => Json(Preferences::default()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `PUT /api/preferences`：校验后持久化用户偏好。
pub async fn put_preferences(
    State(state): State<ApiState>,
    Json(body): Json<Preferences>,
) -> Response {
    if let Err(msg) = body.validate() {
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }
    let db = match state.server_state.db() {
        Some(db) => db.clone(),
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
        }
    };
    let json = match serde_json::to_string(&body) {
        Ok(j) => j,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    match db.save_server_setting(PREFERENCES_KEY, &json).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_preferences_are_valid() {
        let prefs = Preferences::default();
        assert_eq!(prefs.theme, "dark");
        assert_eq!(prefs.language, "system");
        assert_eq!(prefs.title_effect, "grid-wave");
        assert!(prefs.validate().is_ok());
    }

    #[test]
    fn validate_rejects_invalid_theme() {
        let prefs = Preferences {
            theme: "neon".to_string(),
            ..Preferences::default()
        };
        assert!(prefs.validate().is_err());
    }

    #[test]
    fn validate_rejects_invalid_language() {
        let prefs = Preferences {
            language: "fr".to_string(),
            ..Preferences::default()
        };
        assert!(prefs.validate().is_err());
    }

    #[test]
    fn validate_rejects_invalid_title_effect() {
        let prefs = Preferences {
            title_effect: "sparkle".to_string(),
            ..Preferences::default()
        };
        assert!(prefs.validate().is_err());
    }

    #[test]
    fn serde_roundtrip() {
        let prefs = Preferences::default();
        let json = serde_json::to_string(&prefs).unwrap();
        let parsed: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(prefs, parsed);
    }
}
