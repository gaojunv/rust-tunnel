//! 本地全链路探针：真实 server + 真实 client（enable_agent）+ 本机
//! `claude-code-acp` 进程，测量 ACP spawn 流水线各阶段耗时，复现/排除
//! 「等待 ACP agent 就绪超时」类问题。非 CI 回归测试（依赖本机 agent
//! 二进制与交互式环境），默认 `#[ignore]`，手动运行：
//!
//! ```sh
//! cargo test --test acp_spawn_probe -- --ignored --nocapture --test-threads=1
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

async fn seed_workspace(
    harness: &TestHarness,
    client_id: &str,
    root: &std::path::Path,
    overrides: Option<&str>,
) -> rust_tunnel_server::persistence::db::agent::AgentWorkspaceRecord {
    let db = harness.server_state.db().expect("db").clone();
    db.agent_create_workspace(
        "ws-acp",
        "acp-probe",
        client_id,
        "host",
        &root.to_string_lossy(),
        None,
        None,
        "claude-code",
        Some("/usr/local/bin/claude-code-acp"),
        Some("fake-model-gate"), // 仅过门禁；本探针不跑回合
        overrides,
    )
    .await
    .expect("create workspace");
    db.agent_get_workspace("ws-acp")
        .await
        .expect("get workspace")
        .expect("workspace exists")
}

/// 基础路径：无 overrides、无 config_state。预 spawn → wait_ready 应秒级完成。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local claude-code-acp binary"]
async fn acp_spawn_baseline() {
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
        harness.spawn_agent_client(Some("acp-probe"));
        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("online");

        let root = tempfile::tempdir().expect("tempdir");
        let ws = seed_workspace(&harness, "acp-probe", root.path(), None).await;
        let bridge = bridge(&harness);

        let (ws_tx, mut ws_rx) = mpsc::channel(256);
        tokio::spawn(async move { while ws_rx.recv().await.is_some() {} });

        let t0 = Instant::now();
        bridge
            .ensure_session("sess-acp", &ws, ws_tx)
            .await
            .expect("ensure_session");
        println!("[probe] ensure_session (full pipeline): {:?}", t0.elapsed());

        let t1 = Instant::now();
        bridge.wait_ready("sess-acp").await.expect("wait_ready");
        println!("[probe] wait_ready after ensure_session: {:?}", t1.elapsed());

        assert!(bridge.session_spawned("sess-acp").await);
    })
    .await;
    result.expect("probe timed out after 120s");
}

/// 配置注入路径：overrides + config_state 各置若干项，测量注入耗时
/// （agent 不响应 set_config_option 时每条烧满 CONFIG_OPTION_TIMEOUT=15s）。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local claude-code-acp binary"]
async fn acp_spawn_with_config_injection() {
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
        harness.spawn_agent_client(Some("acp-probe"));
        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("online");

        let root = tempfile::tempdir().expect("tempdir");
        // 故意给一个 agent 很可能没有的 config_id：校验应秒拒（unknown
        // config option），验证「无效项不拖时间」；再给真实存在的项测耗时。
        let ws = seed_workspace(
            &harness,
            "acp-probe",
            root.path(),
            Some(r#"{"__nonexistent__":"x"}"#),
        )
        .await;
        let db = harness.server_state.db().expect("db").clone();
        db.agent_create_session("sess-acp", "ws-acp", None, None)
            .await
            .expect("create session");
        // 模拟用户在 UI 点过快捷选项后的持久化状态
        db.agent_update_session_config_state("sess-acp", "mode", Some("default"))
            .await
            .expect("set config_state");

        let bridge = bridge(&harness);
        let (ws_tx, mut ws_rx) = mpsc::channel(256);
        tokio::spawn(async move { while ws_rx.recv().await.is_some() {} });

        let t0 = Instant::now();
        bridge
            .ensure_session("sess-acp", &ws, ws_tx)
            .await
            .expect("ensure_session");
        println!(
            "[probe] ensure_session with config injection: {:?}",
            t0.elapsed()
        );

        bridge.wait_ready("sess-acp").await.expect("wait_ready");
        assert!(bridge.session_spawned("sess-acp").await);
    })
    .await;
    result.expect("probe timed out after 180s");
}
