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
        assert_eq!(
            status,
            StatusCode::OK,
            "login should return 200, body={body:?}"
        );
        let token = body
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("response must contain a token, got {body:?}"));
        assert!(
            !token.is_empty() && token.matches('.').count() == 2,
            "token should look like a JWT (three dot-separated segments), got {token:?}"
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
async fn stats_endpoints_require_auth_when_enabled() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;

        // 无 token：query/summary（受保护路由）与 stream（内部 token 校验）都必须 401。
        let api = harness.api_client();
        assert_eq!(
            api.get_status("/api/stats/summary").await,
            StatusCode::UNAUTHORIZED,
            "/api/stats/summary without token must be 401"
        );
        assert_eq!(
            api.get_status("/api/stats/query").await,
            StatusCode::UNAUTHORIZED,
            "/api/stats/query without token must be 401"
        );
        assert_eq!(
            api.get_status("/api/stats/stream").await,
            StatusCode::UNAUTHORIZED,
            "/api/stats/stream without token must be 401"
        );

        // 带有效 token：summary 返回 200。
        let mut api = harness.api_client();
        let (login_status, _) = api.login("secret").await;
        assert_eq!(login_status, StatusCode::OK);
        assert_eq!(
            api.get_status("/api/stats/summary").await,
            StatusCode::OK,
            "/api/stats/summary with token must be 200"
        );
        assert_eq!(
            api.get_status("/api/stats/query?start=2026-07-20T00:00:00Z&end=2026-07-21T00:00:00Z")
                .await,
            StatusCode::OK,
            "/api/stats/query with token must be 200"
        );
        // 重复的 entity_type 参数（前端 Dashboard 的用法）也必须能解析。
        assert_eq!(
            api.get_status("/api/stats/query?entity_type=client&entity_type=proxy&entity_type=shadowsocks&entity_type=trojan&start=2026-07-20T00:00:00Z&end=2026-07-21T00:00:00Z")
                .await,
            StatusCode::OK,
            "/api/stats/query with repeated entity_type must be 200"
        );
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
        assert_eq!(
            status,
            StatusCode::OK,
            "no-password mode should allow all routes"
        );

        // Login should still succeed in open mode — the handler returns a
        // dummy token regardless of password so existing UI code works.
        let mut api = harness.api_client();
        let (login_status, body) = api.login("anything").await;
        assert_eq!(
            login_status,
            StatusCode::OK,
            "login should still 200 in open mode, body={body:?}"
        );
    })
    .await;
    result.expect("test timed out");
}
