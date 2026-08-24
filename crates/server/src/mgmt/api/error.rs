//! 统一 API 错误类型：handler 只做 DTO + 错误映射，错误响应经
//! `IntoResponse` 一处产出，替代散布的 `(StatusCode, String)` 元组。
//!
//! 响应格式承诺：与 `(StatusCode, String)` 元组的 axum 默认行为一致
//! （纯文本 body + 状态码），前端消费方式不变。

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// 统一 API 错误：HTTP 状态码 + 面向调用方的消息文本。
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// 400 — 请求参数校验失败
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// 401 — 未认证 / 认证信息无效
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    /// 403 — 已认证但无权操作
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    /// 404 — 资源不存在
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    /// 409 — 资源状态冲突（重名、约束冲突等）
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    /// 500 — 服务端内部错误
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// 503 — 依赖的运行时未就绪（agent 未初始化、特性未启用等）
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    /// DB 错误 → 500（message 含底层错误文本；与既有 `format!("DB error: {e}")`
    /// 文本约定一致），并记 error 级日志保留现场。
    pub fn db(err: &sqlx::Error) -> Self {
        tracing::error!(error = %err, "database error in API handler");
        Self::internal(format!("DB error: {err}"))
    }

    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // 与 (StatusCode, String) 元组的默认响应格式一致：text/plain body
        (self.status, self.message).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        Self::db(&err)
    }
}

/// 便捷 Result 别名：`Result<T, ApiError>`
pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn api_error_response_matches_tuple_format() {
        // 与 (StatusCode, String) 元组响应格式等价：状态码 + 纯文本 body
        let err = ApiError::not_found("workspace not found");
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"workspace not found");
    }

    #[test]
    fn constructors_map_status() {
        assert_eq!(ApiError::bad_request("x").status(), StatusCode::BAD_REQUEST);
        assert_eq!(ApiError::unauthorized("x").status(), StatusCode::UNAUTHORIZED);
        assert_eq!(ApiError::forbidden("x").status(), StatusCode::FORBIDDEN);
        assert_eq!(ApiError::not_found("x").status(), StatusCode::NOT_FOUND);
        assert_eq!(ApiError::conflict("x").status(), StatusCode::CONFLICT);
        assert_eq!(
            ApiError::internal("x").status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            ApiError::unavailable("x").status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn sqlx_error_maps_to_internal_with_db_prefix() {
        let err = sqlx::Error::Io(std::io::Error::other("disk gone"));
        let api_err = ApiError::from(err);
        assert_eq!(api_err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(api_err.message().starts_with("DB error: "));
        assert!(api_err.message().contains("disk gone"));
    }
}
