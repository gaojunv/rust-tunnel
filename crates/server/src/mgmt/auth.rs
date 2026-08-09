use axum::{
    async_trait,
    extract::{FromRequestParts, Request},
    http::{request::Parts, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// JWT claims
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    pub iat: usize,
    pub jti: String,
}

impl Default for Claims {
    fn default() -> Self {
        Self::new()
    }
}

impl Claims {
    pub fn new() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards");
        let exp = now + Duration::from_secs(24 * 60 * 60); // 24 hours
        Self {
            sub: "admin".to_string(),
            exp: exp.as_secs() as usize,
            iat: now.as_secs() as usize,
            jti: Uuid::new_v4().to_string(),
        }
    }
}

/// Authentication configuration
#[derive(Clone)]
pub struct AuthConfig {
    pub admin_password: Option<String>,
    pub jwt_secret: String,
}

impl AuthConfig {
    pub fn new(admin_password: Option<String>, jwt_secret: Option<String>) -> Self {
        let jwt_secret = jwt_secret.unwrap_or_else(|| Uuid::new_v4().to_string());
        Self {
            admin_password,
            jwt_secret,
        }
    }

    /// Check if authentication is enabled
    pub fn is_enabled(&self) -> bool {
        self.admin_password.is_some()
    }

    /// Verify password
    pub fn verify_password(&self, password: &str) -> bool {
        match &self.admin_password {
            Some(expected) => password == expected,
            None => true, // No password set, allow anything
        }
    }
}

/// Create JWT token
pub fn create_token(jwt_secret: &str) -> Result<String, jsonwebtoken::errors::Error> {
    let claims = Claims::new();
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )
}

/// Validate JWT token
pub fn validate_token(
    token: &str,
    jwt_secret: &str,
) -> Result<Claims, jsonwebtoken::errors::Error> {
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(jwt_secret.as_bytes()),
        &Validation::default(),
    )
    .map(|data| data.claims)
}

/// Auth extractor for protected routes
pub struct Auth(pub Claims);

#[async_trait]
impl<S> FromRequestParts<S> for Auth
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Try to get from extensions first (set by middleware)
        if let Some(claims) = parts.extensions.get::<Claims>() {
            return Ok(Auth(claims.clone()));
        }
        Err(StatusCode::UNAUTHORIZED.into_response())
    }
}

/// Auth middleware that checks for valid JWT token
pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AuthConfig>>,
    mut req: Request,
    next: Next,
) -> Response {
    // If auth is disabled, just pass through
    if !state.is_enabled() {
        return next.run(req).await;
    }

    // Extract token from Authorization header
    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());

    let token = match auth_header {
        Some(h) if h.starts_with("Bearer ") => &h[7..],
        _ => {
            // Try cookie as fallback
            req.headers()
                .get(axum::http::header::COOKIE)
                .and_then(|c| c.to_str().ok())
                .and_then(|cookies| {
                    cookies.split(';').find_map(|cookie| {
                        let cookie = cookie.trim();
                        cookie.strip_prefix("token=")
                    })
                })
                .unwrap_or("")
        }
    };

    if token.is_empty() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match validate_token(token, &state.jwt_secret) {
        Ok(claims) => {
            req.extensions_mut().insert(claims);
            next.run(req).await
        }
        Err(_) => StatusCode::UNAUTHORIZED.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claims_new() {
        let claims = Claims::new();
        assert_eq!(claims.sub, "admin");
        assert!(claims.exp > claims.iat);
        assert!(!claims.jti.is_empty());
    }

    #[test]
    fn test_auth_config_new() {
        // With password and secret
        let config = AuthConfig::new(Some("password".into()), Some("secret".into()));
        assert_eq!(config.admin_password, Some("password".into()));
        assert_eq!(config.jwt_secret, "secret");

        // Without password and secret
        let config = AuthConfig::new(None, None);
        assert!(config.admin_password.is_none());
        assert!(!config.jwt_secret.is_empty());

        // With password only
        let config = AuthConfig::new(Some("password".into()), None);
        assert_eq!(config.admin_password, Some("password".into()));
        assert!(!config.jwt_secret.is_empty());

        // With secret only
        let config = AuthConfig::new(None, Some("secret".into()));
        assert!(config.admin_password.is_none());
        assert_eq!(config.jwt_secret, "secret");
    }

    #[test]
    fn test_auth_config_is_enabled() {
        let config = AuthConfig::new(Some("password".into()), None);
        assert!(config.is_enabled());

        let config = AuthConfig::new(None, None);
        assert!(!config.is_enabled());
    }

    #[test]
    fn test_auth_config_verify_password() {
        let config = AuthConfig::new(Some("correct_password".into()), None);
        assert!(config.verify_password("correct_password"));
        assert!(!config.verify_password("wrong_password"));

        // Auth disabled - any password works
        let config = AuthConfig::new(None, None);
        assert!(config.verify_password("any_password"));
        assert!(config.verify_password(""));
    }

    #[test]
    fn test_create_and_validate_token() {
        let secret = "test-secret-key";

        // Create token
        let token_result = create_token(secret);
        assert!(token_result.is_ok());
        let token = token_result.unwrap();
        assert!(!token.is_empty());

        // Validate token
        let validate_result = validate_token(&token, secret);
        assert!(validate_result.is_ok());
        let claims = validate_result.unwrap();
        assert_eq!(claims.sub, "admin");
    }

    #[test]
    fn test_validate_token_invalid_secret() {
        let token = create_token("correct-secret").unwrap();
        let result = validate_token(&token, "wrong-secret");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_token_invalid_token() {
        let result = validate_token("invalid-token", "secret");
        assert!(result.is_err());
    }

    #[test]
    fn test_auth_config_clone() {
        let config = AuthConfig::new(Some("password".into()), Some("secret".into()));
        let cloned = config.clone();
        assert_eq!(config.admin_password, cloned.admin_password);
        assert_eq!(config.jwt_secret, cloned.jwt_secret);
    }
}
