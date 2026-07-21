use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

use super::{dto::{HealthResponse, LoginRequest, LoginResponse}, ApiState};
use crate::server::auth::create_token;

// Login handler
pub async fn login(
    State(state): State<ApiState>,
    Json(request): Json<LoginRequest>,
) -> impl IntoResponse {
    if !state.auth_config.is_enabled() {
        let token = create_token(&state.auth_config.jwt_secret)
            .unwrap_or_else(|_| "dummy-token".to_string());
        return Json(LoginResponse {
            token,
            auth_required: false,
        })
        .into_response();
    }

    if state.auth_config.verify_password(&request.password) {
        match create_token(&state.auth_config.jwt_secret) {
            Ok(token) => Json(LoginResponse {
                token,
                auth_required: true,
            })
            .into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create token").into_response(),
        }
    } else {
        (StatusCode::UNAUTHORIZED, "Invalid password").into_response()
    }
}

// Logout handler (client just discards token)
pub async fn logout() -> impl IntoResponse {
    StatusCode::OK
}

// Health check
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_response() {
        let response = HealthResponse { status: "ok" };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("ok"));
    }

    #[test]
    fn test_login_request_deserialize() {
        let json = r#"{"password":"secret"}"#;
        let req: LoginRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.password, "secret");
    }
}