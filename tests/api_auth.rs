//! Integration tests: /api/login, JWT bearer enforcement, no-password mode.

#[path = "common/mod.rs"]
mod common;

use common::{HarnessOpts, TestHarness};
use reqwest::StatusCode;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn login_returns_jwt() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;
        let mut api = harness.api_client();
        let (status, body) = api.login("secret").await;
        assert_eq!(status, StatusCode::OK, "login should return 200, body={body:?}");
        assert!(
            body.get("token").and_then(|v| v.as_str()).is_some(),
            "response must contain a token, got {body:?}"
        );
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_route_requires_bearer() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;

        // No token → expect 401.
        let api = harness.api_client();
        let status = api.get_status("/api/clients").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // With a valid token → expect 200.
        let mut api = harness.api_client();
        let (login_status, _) = api.login("secret").await;
        assert_eq!(login_status, StatusCode::OK);
        let status = api.get_status("/api/clients").await;
        assert_eq!(status, StatusCode::OK);
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_returns_401() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;
        let mut api = harness.api_client();
        let (status, _) = api.login("WRONG").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_admin_password_disables_auth() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        // admin_password: None → all routes should be open, no Bearer required.
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: None,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let status = api.get_status("/api/clients").await;
        assert_eq!(status, StatusCode::OK, "no-password mode should allow all routes");
    })
    .await;
    result.expect("test timed out");
}
