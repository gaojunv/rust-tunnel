//! Integration tests: GET/PUT /api/preferences — user preferences (theme, language,
//! title animation effect).
//!
//! GET is on public_routes (no JWT required), PUT is on protected_routes (requires JWT
//! when an admin password is configured).

#[path = "common/mod.rs"]
mod common;

use common::{HarnessOpts, TestHarness};
use reqwest::StatusCode;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn get_preferences_returns_defaults_when_unset() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;

        let api = harness.api_client();
        let (status, body) = api.get_json("/api/preferences").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["theme"], "dark");
        assert_eq!(body["language"], "system");
        assert_eq!(body["title_effect"], "grid-wave");
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn get_preferences_is_public_without_jwt() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;

        // 未登录的 api_client 也能 GET
        let api = harness.api_client();
        let (status, _body) = api.get_json("/api/preferences").await;
        assert_eq!(status, StatusCode::OK);
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn put_preferences_requires_jwt() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;

        // 无 token → 401
        let api = harness.api_client();
        let (status, _) = api
            .put_json(
                "/api/preferences",
                serde_json::json!({
                    "theme": "light",
                    "language": "en",
                    "title_effect": "particles"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // 登录后 → 204
        let mut api = harness.api_client();
        let (login_status, _) = api.login("secret").await;
        assert_eq!(login_status, StatusCode::OK);
        let (status, _) = api
            .put_json(
                "/api/preferences",
                serde_json::json!({
                    "theme": "light",
                    "language": "en",
                    "title_effect": "particles"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // GET 读回
        let (status, body) = api.get_json("/api/preferences").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["theme"], "light");
        assert_eq!(body["language"], "en");
        assert_eq!(body["title_effect"], "particles");
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn put_preferences_rejects_invalid_value() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;

        let mut api = harness.api_client();
        let (login_status, _) = api.login("secret").await;
        assert_eq!(login_status, StatusCode::OK);

        // 非法 theme
        let (status, _) = api
            .put_json(
                "/api/preferences",
                serde_json::json!({
                    "theme": "neon",
                    "language": "en",
                    "title_effect": "particles"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // 非法 title_effect
        let (status, _) = api
            .put_json(
                "/api/preferences",
                serde_json::json!({
                    "theme": "dark",
                    "language": "en",
                    "title_effect": "sparkle"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn preferences_persist_across_requests() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;

        let mut api = harness.api_client();
        api.login("secret").await;

        // PUT 一次
        api.put_json(
            "/api/preferences",
            serde_json::json!({
                "theme": "light",
                "language": "zh-CN",
                "title_effect": "none"
            }),
        )
        .await;

        // 用一个新的 api_client GET，验证数据真的写到了 SQLite（不只是内存）
        let api2 = harness.api_client();
        let (status, body) = api2.get_json("/api/preferences").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["theme"], "light");
        assert_eq!(body["title_effect"], "none");
    })
    .await;
    result.expect("test timed out");
}
