//! End-to-end: agent exec over a real control channel between the harness
//! server and a real `run_client` client spawned with `enable_agent = true`.
//!
//! Covers spec §10: Shell roundtrip, WriteFile/ReadFile roundtrip, git
//! lifecycle, and the offline-client error path.

#[path = "common/mod.rs"]
mod common;

use common::{HarnessOpts, TestHarness};
use rust_tunnel::common::{AgentCommand, AgentResult};
use rust_tunnel::server::client_registry::ClientRegistry;
use std::io::ErrorKind;
use std::time::Duration;

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
