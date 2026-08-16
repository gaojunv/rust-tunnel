//! 本地全链路探针：真实 server + 真实 client（enable_agent）+ 本机
//! `opencode acp` 进程，验证 remember MCP 注入在 opencode 会话的握手
//! 兼容性（2026-08-16 曾误诊挂起并加门控，复验后移除）。非 CI 回归
//! 测试（依赖本机 opencode 二进制 ~/.opencode/bin/opencode），默认
//! `#[ignore]`，手动运行：
//!
//! ```sh
//! cargo test --test opencode_mcp_probe -- --ignored --nocapture --test-threads=1
//! ```

#[path = "common/mod.rs"]
mod common;

use common::{HarnessOpts, TestHarness};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

fn bridge(harness: &TestHarness) -> rust_tunnel_server::agent::acp_bridge::AcpBridge {
    harness
        .server_state
        .agent_state
        .as_ref()
        .expect("agent_state")
        .acp_bridge
        .clone()
        .expect("acp_bridge")
}

/// opencode + MCP 注入：eager 连接隧道端点（initialize/initialized/tools/list）
/// 应正常完成，ensure_session 秒级返回。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local opencode binary at ~/.opencode/bin/opencode"]
async fn opencode_mcp_inject_probe() {
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
        harness.spawn_agent_client(Some("opencode-probe"));
        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("online");

        let root = tempfile::tempdir().expect("tempdir");
        let db = harness.server_state.db().expect("db").clone();
        let opencode_path = format!("{}/.opencode/bin/opencode", std::env::var("HOME").unwrap());
        assert!(std::path::Path::new(&opencode_path).exists(), "opencode binary missing");
        db.agent_create_workspace(
            "ws-oc",
            "oc-probe",
            "opencode-probe",
            "host",
            &root.path().to_string_lossy(),
            None,
            None,
            "opencode",
            Some(&opencode_path),
            Some("fake-model-gate"), // 仅过门禁；本探针不跑回合
            None,
        )
        .await
        .expect("create workspace");
        let ws = db
            .agent_get_workspace("ws-oc")
            .await
            .expect("get workspace")
            .expect("workspace exists");
        db.agent_create_session("sess-oc", "ws-oc", None, None)
            .await
            .expect("create session");

        let bridge = bridge(&harness);
        let (ws_tx, mut ws_rx) = mpsc::channel(256);
        tokio::spawn(async move { while ws_rx.recv().await.is_some() {} });

        let t0 = Instant::now();
        bridge
            .ensure_session("sess-oc", &ws, ws_tx, 1)
            .await
            .expect("ensure_session");
        println!("[probe] ensure_session (opencode + MCP 注入): {:?}", t0.elapsed());

        bridge.wait_ready("sess-oc").await.expect("wait_ready");
        assert!(bridge.session_spawned("sess-oc").await);
    })
    .await;
    result.expect("probe timed out after 120s (opencode MCP 挂起回归?)");
}
