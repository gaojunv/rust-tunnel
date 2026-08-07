//! End-to-end: agent exec over a real control channel between the harness
//! server and a real `run_client` client spawned with `enable_agent = true`.
//!
//! Covers spec §10: Shell roundtrip, WriteFile/ReadFile roundtrip, git
//! lifecycle, and the offline-client error path.
//!
//! 审批 e2e（runner 层，降级路径）：现有基建无 WS 客户端（无 tokio-tungstenite
//! dev-dependency）与 mock LLM 网关链路，全链路 WS 模拟不低成本。改为直接驱动
//! `run_agent_turn`——真实客户端走真实控制通道，mock LLM 上游只负责按序回吐
//! tool_call / 文本，审批挂起/唤醒、拒绝落库等行为与 WS 路径完全同源（同一
//! `AgentState::request_approval`/`resolve_approval`）。规则矩阵已在
//! `src/server/agent/approval.rs` 单测覆盖，此处补集成层：safe 模式 shell
//! 必须挂起审批、批准后真实执行、拒绝后 `[denied by user]` 落库。

#[path = "common/mod.rs"]
mod common;

use common::{HarnessOpts, TestHarness};
use rust_tunnel::common::{AgentCommand, AgentResult};
use rust_tunnel::server::agent::session::SessionRuntime;
use rust_tunnel::server::client_registry::ClientRegistry;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Per-command deadline passed to `agent_exec` (well under the 30s test cap).
const EXEC_TIMEOUT: Duration = Duration::from_secs(10);

fn client_registry(harness: &TestHarness) -> ClientRegistry {
    harness
        .server_state
        .client_registry
        .clone()
        .expect("harness server has a client registry")
}

/// Run a shell command on the agent client and assert it exited 0.
async fn shell_ok(registry: &ClientRegistry, client: &str, root: &str, cmd: &str) -> String {
    match registry
        .agent_exec(
            client,
            "e2e-shell",
            "e2e-session",
            root,
            None,
            AgentCommand::Shell {
                cmd: cmd.to_string(),
                cwd: None,
            },
            EXEC_TIMEOUT,
        )
        .await
        .expect("agent_exec shell")
    {
        AgentResult::Shell {
            stdout,
            stderr,
            exit_code,
        } => {
            assert_eq!(exit_code, 0, "shell `{cmd}` failed: stderr={stderr:?}");
            stdout
        }
        other => panic!("expected Shell result for `{cmd}`, got {other:?}"),
    }
}

/// Register an agent-enabled client and wait for it to come online.
async fn spawn_online_agent_client(harness: &mut TestHarness, name: &str) -> ClientRegistry {
    harness.spawn_agent_client(Some(name));
    let api = harness.api_client();
    harness
        .wait_client_count(&api, 1)
        .await
        .expect("client registered");
    client_registry(harness)
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_exec_shell_roundtrip() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
        let registry = spawn_online_agent_client(&mut harness, "agent-shell-client").await;
        let root = tempfile::tempdir().expect("tempdir");
        let root_str = root.path().to_string_lossy().to_string();

        let stdout = shell_ok(&registry, "agent-shell-client", &root_str, "echo e2e-ok").await;
        assert!(stdout.contains("e2e-ok"), "unexpected stdout: {stdout:?}");
    })
    .await;
    result.expect("test timed out after 30s");
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_write_then_read_file() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
        let registry = spawn_online_agent_client(&mut harness, "agent-file-client").await;
        let root = tempfile::tempdir().expect("tempdir");
        let root_str = root.path().to_string_lossy().to_string();

        let content = "hello from the agent over the tunnel\nline two\n";
        match registry
            .agent_exec(
                "agent-file-client",
                "e2e-write",
                "e2e-session",
                &root_str,
                None,
                AgentCommand::WriteFile {
                    path: "notes.txt".to_string(),
                    content: content.to_string(),
                },
                EXEC_TIMEOUT,
            )
            .await
            .expect("agent_exec write")
        {
            AgentResult::Success => {}
            other => panic!("expected Success from WriteFile, got {other:?}"),
        }

        match registry
            .agent_exec(
                "agent-file-client",
                "e2e-read",
                "e2e-session",
                &root_str,
                None,
                AgentCommand::ReadFile {
                    path: "notes.txt".to_string(),
                },
                EXEC_TIMEOUT,
            )
            .await
            .expect("agent_exec read")
        {
            AgentResult::FileContent { content: got } => assert_eq!(got, content),
            other => panic!("expected FileContent from ReadFile, got {other:?}"),
        }

        // Sanity: the file really landed inside the tempdir on the test side.
        let on_disk = std::fs::read_to_string(root.path().join("notes.txt")).expect("file on disk");
        assert_eq!(on_disk, content);
    })
    .await;
    result.expect("test timed out after 30s");
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_git_lifecycle() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
        let registry = spawn_online_agent_client(&mut harness, "agent-git-client").await;
        let root = tempfile::tempdir().expect("tempdir");
        let root_str = root.path().to_string_lossy().to_string();
        let client = "agent-git-client";

        // Init the repo and configure a commit identity inside the sandbox.
        shell_ok(
            &registry,
            client,
            &root_str,
            "git init -q -b main && git config user.name 'agent-test' && git config user.email 'agent-test@example.com'",
        )
        .await;

        // Write a tracked file, then GitStatus must show it as untracked.
        match registry
            .agent_exec(
                client,
                "e2e-write-app",
                "e2e-session",
                &root_str,
                None,
                AgentCommand::WriteFile {
                    path: "app.rs".to_string(),
                    content: "fn main() {}\n".to_string(),
                },
                EXEC_TIMEOUT,
            )
            .await
            .expect("write app.rs")
        {
            AgentResult::Success => {}
            other => panic!("expected Success from WriteFile, got {other:?}"),
        }

        match registry
            .agent_exec(
                client,
                "e2e-status",
                "e2e-session",
                &root_str,
                None,
                AgentCommand::GitStatus,
                EXEC_TIMEOUT,
            )
            .await
            .expect("git status")
        {
            AgentResult::FileContent { content } => {
                assert!(
                    content.contains("?? app.rs"),
                    "expected untracked app.rs, got {content:?}"
                );
            }
            other => panic!("expected FileContent from GitStatus, got {other:?}"),
        }

        // Commit everything; then GitStatus must be clean.
        match registry
            .agent_exec(
                client,
                "e2e-commit",
                "e2e-session",
                &root_str,
                None,
                AgentCommand::GitCommit {
                    message: "initial".to_string(),
                },
                EXEC_TIMEOUT,
            )
            .await
            .expect("git commit")
        {
            AgentResult::FileContent { .. } | AgentResult::Success => {}
            other => panic!("expected commit success, got {other:?}"),
        }

        match registry
            .agent_exec(
                client,
                "e2e-status-clean",
                "e2e-session",
                &root_str,
                None,
                AgentCommand::GitStatus,
                EXEC_TIMEOUT,
            )
            .await
            .expect("git status after commit")
        {
            AgentResult::FileContent { content } => {
                assert_eq!(
                    content.trim(),
                    "## main",
                    "expected clean working tree, got {content:?}"
                );
            }
            other => panic!("expected FileContent from GitStatus, got {other:?}"),
        }
    })
    .await;
    result.expect("test timed out after 30s");
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_exec_offline_client() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let root = tempfile::tempdir().expect("tempdir");
        let root_str = root.path().to_string_lossy().to_string();
        let registry = client_registry(&harness);

        let err = registry
            .agent_exec(
                "no-such-client",
                "e2e-offline",
                "e2e-session",
                &root_str,
                None,
                AgentCommand::Shell {
                    cmd: "echo hi".to_string(),
                    cwd: None,
                },
                EXEC_TIMEOUT,
            )
            .await
            .expect_err("offline client should fail with NotConnected");
        assert_eq!(err.kind(), ErrorKind::NotConnected);
    })
    .await;
    result.expect("test timed out after 30s");
}

// ── 审批 e2e：runner 层集成测试 ────────────────────────────────

/// 有状态 mock LLM 上游：第 1 次请求回 shell tool_call，之后一律回纯文本收尾。
/// runner 走 `resolve_with_failover` → 单候选 → `call_upstream_with_body`，
/// mock 直接回非 SSE JSON 即可（runner 有非 SSE 回退路径）。
struct ApprovalMockLlm {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
}

impl ApprovalMockLlm {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let hits = hits_task.clone();
                tokio::spawn(async move {
                    // 读 headers + body（按 Content-Length 收全）。
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if let Some(pos) =
                                    buf.windows(4).position(|w| w == b"\r\n\r\n")
                                {
                                    break pos + 4;
                                }
                            }
                        }
                    };
                    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_length = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + content_length {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }

                    let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = if hit == 1 {
                        serde_json::json!({
                            "id": "cmpl-approval-1",
                            "object": "chat.completion",
                            "model": "approval-model",
                            "choices": [{
                                "index": 0,
                                "message": {
                                    "role": "assistant",
                                    "content": null,
                                    "tool_calls": [{
                                        "id": "call_shell",
                                        "type": "function",
                                        "function": {
                                            "name": "shell",
                                            "arguments": "{\"cmd\":\"echo approve-e2e-ok\"}"
                                        }
                                    }]
                                },
                                "finish_reason": "tool_calls"
                            }]
                        })
                    } else {
                        serde_json::json!({
                            "id": "cmpl-approval-2",
                            "object": "chat.completion",
                            "model": "approval-model",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "done"},
                                "finish_reason": "stop"
                            }]
                        })
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.to_string().len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self { addr, hits }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// 审批 e2e 环境：真实 agent 客户端在线 + mock LLM 上游 + approval_mode='safe'
/// 的 workspace + 已落库 session（model 指向 mock 上游注册的模型）。
struct ApprovalEnv {
    session_id: String,
    /// 保持 tempdir 存活（root_path 指向它）。
    _root: tempfile::TempDir,
}

async fn setup_approval_env(
    harness: &mut TestHarness,
    client_name: &str,
    mock: &ApprovalMockLlm,
) -> ApprovalEnv {
    // 先确认客户端在线（agent_exec 需要真实注册条目）。
    spawn_online_agent_client(harness, client_name).await;

    // 注册 provider + model，base_url 直指 mock 上游（单候选 → 直连，无需网关）。
    let api = harness.api_client();
    let (status, body) = api
        .post_json(
            "/api/llm/providers",
            serde_json::json!({
                "name": "approval-mock",
                "provider_type": "deepseek",
                "base_url": mock.url(),
                "api_key": "sk-test",
            }),
        )
        .await;
    assert!(status.is_success(), "create provider: {status} {body}");
    let pid = body["id"].as_str().unwrap().to_string();
    let (status, body) = api
        .post_json(
            &format!("/api/llm/providers/{pid}/models"),
            serde_json::json!({"model_name": "approval-model", "alias": "approval-model"}),
        )
        .await;
    assert!(status.is_success(), "create model: {status} {body}");

    // workspace（approval_mode='safe'）+ session，直接走 DB（与 API 同库）。
    let db = harness.server_state.db().expect("harness db");
    let root = tempfile::tempdir().expect("tempdir");
    let root_str = root.path().to_string_lossy().to_string();
    db.agent_create_workspace(
        "ws-approval-e2e",
        "approval-e2e",
        client_name,
        "host",
        &root_str,
        None,
        None,
    )
    .await
    .expect("create workspace");
    db.agent_update_workspace("ws-approval-e2e", "approval-e2e", &root_str, None, Some("safe"))
        .await
        .expect("set approval_mode=safe");
    let session_id = "sess-approval-e2e";
    db.agent_create_session(session_id, "ws-approval-e2e", None, Some("approval-model"))
        .await
        .expect("create session");

    ApprovalEnv {
        session_id: session_id.to_string(),
        _root: root,
    }
}

/// 跑一个 runner turn：mock LLM 第 1 轮回 shell tool_call，收到 approval_request
/// 帧时按 `resolve` 批准/拒绝，收集到 done/error 为止的全部 WS 帧。
async fn run_turn_with_approval(
    harness: &TestHarness,
    env: &ApprovalEnv,
    resolve: Option<bool>,
) -> Vec<serde_json::Value> {
    let agent = harness
        .server_state
        .agent_state
        .clone()
        .expect("harness agent_state");
    let llm = harness
        .server_state
        .proxy_state
        .llm_state
        .read()
        .await
        .clone()
        .expect("harness llm_state");
    let mut rt = SessionRuntime::load(&agent.db, &env.session_id, "default")
        .await
        .expect("load session runtime");
    let (ws_tx, mut ws_rx) = mpsc::channel(64);

    let agent_turn = agent.clone();
    let turn = tokio::spawn(async move {
        rust_tunnel::server::agent::runner::run_agent_turn(agent_turn, llm, &mut rt, ws_tx).await
    });

    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(15), ws_rx.recv())
            .await
            .expect("timed out waiting for ws frame")
            .expect("ws channel closed before turn finished");
        let ftype = frame["type"].as_str().unwrap_or("").to_string();
        if ftype == "approval_request" {
            if let Some(approved) = resolve {
                let req_id = frame["request_id"].as_str().unwrap().to_string();
                agent
                    .resolve_approval(&env.session_id, &req_id, approved, false)
                    .await;
            }
        }
        let terminal = matches!(ftype.as_str(), "done" | "error");
        frames.push(frame);
        if terminal {
            break;
        }
    }
    turn.await.expect("turn task panicked").expect("turn failed");
    frames
}

/// safe 模式：shell 必须先发 approval_request 挂起；批准后经隧道真实执行。
#[tokio::test(flavor = "multi_thread")]
async fn agent_safe_mode_shell_requires_approval() {
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
        let mock = ApprovalMockLlm::start().await;
        let env = setup_approval_env(&mut harness, "agent-approval-ok", &mock).await;

        let frames = run_turn_with_approval(&harness, &env, Some(true)).await;

        let approval = frames
            .iter()
            .find(|f| f["type"] == "approval_request")
            .expect("approval_request frame must be emitted in safe mode");
        assert_eq!(approval["tool"], "shell");
        assert_eq!(approval["summary"], "echo approve-e2e-ok");

        // 批准后 shell 在真实客户端上执行，stdout 进入 tool_result 帧。
        let tool_result = frames
            .iter()
            .find(|f| f["type"] == "tool_result")
            .expect("tool_result frame after approval");
        assert!(
            tool_result["result"].as_str().unwrap().contains("approve-e2e-ok"),
            "shell should execute after approval, got: {tool_result}"
        );

        // 第二轮 LLM 调用收到文本收尾 → 回合正常结束。
        assert!(mock.hit_count() >= 2, "expected a second LLM round");
    })
    .await;
    result.expect("test timed out after 60s");
}

/// 拒绝审批：回 [denied by user] 帧且落库 kind='tool_result'，shell 不执行。
#[tokio::test(flavor = "multi_thread")]
async fn agent_denied_tool_result_recorded() {
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
        let mock = ApprovalMockLlm::start().await;
        let env = setup_approval_env(&mut harness, "agent-approval-deny", &mock).await;

        let frames = run_turn_with_approval(&harness, &env, Some(false)).await;

        let tool_result = frames
            .iter()
            .find(|f| f["type"] == "tool_result")
            .expect("tool_result frame after denial");
        assert_eq!(
            tool_result["result"],
            "[denied by user]",
            "denied tool must record the marker, got: {tool_result}"
        );

        // DB 落库一条 kind='tool_result' 的 [denied by user]（与 WS 帧一致）。
        let agent = harness
            .server_state
            .agent_state
            .clone()
            .expect("harness agent_state");
        let msgs = agent
            .db
            .agent_list_messages(&env.session_id)
            .await
            .expect("list messages");
        let denied = msgs
            .iter()
            .find(|m| m.kind == "tool_result" && m.content == "[denied by user]")
            .expect("denied tool_result row in DB");
        assert_eq!(denied.name.as_deref(), Some("shell"));
        assert!(denied.tool_call_id.is_some(), "denied row keeps tool_call_id");

        // 拒绝后回合继续（第二轮文本收尾），未在客户端执行 shell。
        assert!(mock.hit_count() >= 2, "expected a second LLM round");
    })
    .await;
    result.expect("test timed out after 60s");
}
