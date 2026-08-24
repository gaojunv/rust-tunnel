use super::super::*;
use super::helpers::*;

#[tokio::test]
async fn test_exec_fs_read_forwards_via_tunnel() {
    // 绝对路径 → 相对路径 → 客户端返回 FileContent → 回包给 agent。
    let (db, spawner, sessions) = fs_test_env(rust_tunnel_common::AgentResult::FileContent {
        content: "hello fs".into(),
    })
    .await;
    let content = exec_fs_read(&db, &spawner, &sessions, "sess-1", "/ws/src/main.rs")
        .await
        .expect("read should succeed");
    assert_eq!(content, "hello fs");
}

#[tokio::test]
async fn test_exec_fs_read_rejects_outside_workspace() {
    let (db, spawner, sessions) = fs_test_env(rust_tunnel_common::AgentResult::Success).await;
    let err = exec_fs_read(&db, &spawner, &sessions, "sess-1", "/etc/passwd")
        .await
        .expect_err("outside workspace should be rejected");
    assert!(err.contains("outside workspace"), "err: {err}");
}

#[tokio::test]
async fn test_exec_fs_read_propagates_client_error() {
    let (db, spawner, sessions) = fs_test_env(rust_tunnel_common::AgentResult::Error {
        message: "no such file".into(),
    })
    .await;
    let err = exec_fs_read(&db, &spawner, &sessions, "sess-1", "/ws/missing.txt")
        .await
        .expect_err("client error should propagate");
    assert_eq!(err, "no such file");
}

#[tokio::test]
async fn test_exec_fs_write_forwards_via_tunnel() {
    let (db, spawner, sessions) = fs_test_env(rust_tunnel_common::AgentResult::Success).await;
    exec_fs_write(&db, &spawner, &sessions, "sess-1", "/ws/a.txt", "hi")
        .await
        .expect("write should succeed");
}

#[tokio::test]
async fn test_exec_fs_session_not_spawned_errors() {
    // 会话条目缺失（进程未 spawn）：在构造 AgentCommand 前即报错，不触发隧道请求。
    let (db, spawner, _sessions) = fs_test_env(rust_tunnel_common::AgentResult::Success).await;
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    let err = exec_fs_read(&db, &spawner, &sessions, "sess-1", "/ws/a.txt")
        .await
        .expect_err("missing session entry should error");
    assert_eq!(err, "session not spawned");
}
