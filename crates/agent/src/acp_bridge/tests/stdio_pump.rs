use super::super::*;

#[tokio::test]
async fn test_stdio_pump_process_stdout_to_agent() {
    // 进程 stdout（stdout_tx）→ pump → duplex → ACP 端可读
    let (mut agent_io, pump_io) = tokio::io::duplex(64 * 1024);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(16);
    let (control_tx, _control_rx) = mpsc::channel::<ControlMessage>(16);
    tokio::spawn(run_stdio_pump(
        pump_io,
        stdout_rx,
        control_tx,
        "sess-1".into(),
    ));

    stdout_tx.send(b"hello".to_vec()).await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(std::time::Duration::from_secs(2), agent_io.read(&mut buf))
        .await
        .expect("timed out reading agent_io")
        .expect("read failed");
    assert_eq!(&buf[..n], b"hello");
}

#[tokio::test]
async fn test_stdio_pump_agent_to_process_stdin() {
    // ACP 端写入（模拟 ACP crate 输出到进程 stdin）→ AgentSpawnData(stdin=true)
    let (mut agent_io, pump_io) = tokio::io::duplex(64 * 1024);
    let (_stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(16);
    let (control_tx, mut control_rx) = mpsc::channel::<ControlMessage>(16);
    tokio::spawn(run_stdio_pump(
        pump_io,
        stdout_rx,
        control_tx,
        "sess-1".into(),
    ));

    agent_io.write_all(b"world").await.unwrap();
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), control_rx.recv())
        .await
        .expect("timed out waiting for stdin data")
        .expect("channel closed");
    match msg {
        ControlMessage::AgentSpawnData {
            session_id,
            data,
            stdin: true,
        } => {
            assert_eq!(session_id, "sess-1");
            assert_eq!(data, b"world");
        }
        other => panic!("expected AgentSpawnData(stdin=true), got {other:?}"),
    }
}

#[tokio::test]
async fn test_stdio_pump_exits_when_stdout_sender_dropped() {
    // 进程退出/会话移除 → stdout_tx drop → pump 收尾（排空后退出，不泄漏）
    let (mut agent_io, pump_io) = tokio::io::duplex(64 * 1024);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(16);
    let (control_tx, _control_rx) = mpsc::channel::<ControlMessage>(16);
    let task = tokio::spawn(run_stdio_pump(
        pump_io,
        stdout_rx,
        control_tx,
        "sess-1".into(),
    ));
    // 先投递一条残余字节再 drop 发送端：pump 应转发后再退出（不丢数据）
    stdout_tx.send(b"tail".to_vec()).await.unwrap();
    drop(stdout_tx);
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(std::time::Duration::from_secs(2), agent_io.read(&mut buf))
        .await
        .expect("timed out reading agent_io")
        .expect("read failed");
    assert_eq!(&buf[..n], b"tail");
    // pump 任务应自行结束
    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("pump task did not exit")
        .expect("pump task panicked");
}

// ── busy 守卫 ───────────────────────────────────────────────
