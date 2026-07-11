//! Thin reqwest wrapper: keeps a JWT token, injects Bearer header when present.

use reqwest::{Client, StatusCode};
use serde_json::Value;

pub struct ApiClient {
    http: Client,
    base: String,
    token: Option<String>,
}

impl ApiClient {
    pub fn new(base: impl Into<String>) -> Self {
        // The server API listens on plain HTTP (Axum). No TLS on API port.
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("build reqwest"),
            base: base.into(),
            token: None,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// POST /api/login with the given password. Stores the returned token.
    /// Returns the raw StatusCode and parsed JSON.
    pub async fn login(&mut self, password: &str) -> (StatusCode, Value) {
        let resp = self
            .http
            .post(format!("{}/api/login", self.base))
            .json(&serde_json::json!({ "password": password }))
            .send()
            .await
            .expect("login send");
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            if let Some(t) = body.get("token").and_then(|v| v.as_str()) {
                self.token = Some(t.to_string());
            }
        }
        (status, body)
    }

    /// GET a JSON endpoint, returning `(status, body)`.
    /// Injects `Authorization: Bearer <token>` if a token is stored.
    pub async fn get_json(&self, path: &str) -> (StatusCode, Value) {
        let mut req = self.http.get(format!("{}{}", self.base, path));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.expect("get send");
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    /// Bare GET returning status (used for auth-negative tests where body is irrelevant).
    pub async fn get_status(&self, path: &str) -> StatusCode {
        let mut req = self.http.get(format!("{}{}", self.base, path));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req.send().await.expect("get send").status()
    }

    /// DELETE a path, returning the status. Injects Bearer if a token is stored.
    pub async fn delete_status(&self, path: &str) -> StatusCode {
        let mut req = self.http.delete(format!("{}{}", self.base, path));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req.send().await.expect("delete send").status()
    }
}
