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
pub fn validate_token(token: &str, jwt_secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
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
                        if cookie.starts_with("token=") {
                            Some(&cookie[6..])
                        } else {
                            None
                        }
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
