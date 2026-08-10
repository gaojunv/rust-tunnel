//! Long-lived process spawn manager: handles AgentSpawnRequest from the server,
//! bridges stdin/stdout over the control channel.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};

use rust_tunnel_common::ControlMessage;

/// 单次 stdout 转发上限（协议 1MB 消息内留余量）
const MAX_CHUNK: usize = 512 * 1024;

struct SpawnedProcess {
    stdin_tx: mpsc::Sender<Vec<u8>>,
    /// kill 句柄：cancel 时中断 stdout 读取任务并杀进程
    kill_tx: tokio::sync::oneshot::Sender<()>,
}

#[derive(Clone, Default)]
pub struct SpawnManager {
    processes: Arc<Mutex<HashMap<String, SpawnedProcess>>>,
}

impl SpawnManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle an AgentSpawnRequest: spawn the process and start stdio bridging.
    /// `control_tx` 用于回发 AgentSpawnResponse/Data/Exit。
    pub async fn handle_spawn(
        &self,
        session_id: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
        control_tx: mpsc::Sender<ControlMessage>,
    ) {
        let result = self
            .spawn_inner(&session_id, &command, &args, &env, cwd.as_deref(), control_tx.clone())
            .await;
        let (success, error) = match result {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e)),
        };
        let _ = control_tx
            .send(ControlMessage::AgentSpawnResponse { session_id, success, error })
            .await;
    }

    async fn spawn_inner(
        &self,
        session_id: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
        control_tx: mpsc::Sender<ControlMessage>,
    ) -> Result<(), String> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child: Child = cmd.spawn().map_err(|e| format!("spawn '{command}' failed: {e}"))?;

        let mut stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let mut stdout = child.stdout.take().ok_or("no stdout pipe")?;

        // stdin: channel -> process
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(32);
        tokio::spawn(async move {
            while let Some(data) = stdin_rx.recv().await {
                if stdin.write_all(&data).await.is_err() {
                    break;
                }
            }
        });

        // stdout: process -> control channel（原始字节块透传，按 read 大小切分；
        // ACP 的 newline-delimited JSON-RPC 重组由服务端接收方负责）
        let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
        let sid = session_id.to_string();
        let tx = control_tx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_CHUNK];
            loop {
                tokio::select! {
                    _ = &mut kill_rx => {
                        let _ = child.kill().await;
                        break;
                    }
                    n = stdout.read(&mut buf) => {
                        match n {
                            Ok(0) | Err(_) => break, // EOF：进程退出
                            Ok(n) => {
                                if tx.send(ControlMessage::AgentSpawnData {
                                    session_id: sid.clone(),
                                    data: buf[..n].to_vec(),
                                    stdin: false,
                                }).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            let code = child.wait().await.ok().and_then(|s| s.code());
            let _ = tx.send(ControlMessage::AgentSpawnExit { session_id: sid, code }).await;
        });

        self.processes.lock().await.insert(
            session_id.to_string(),
            SpawnedProcess { stdin_tx, kill_tx },
        );
        Ok(())
    }

    /// Write data to a spawned process's stdin (server -> client AgentSpawnData).
    pub async fn write_stdin(&self, session_id: &str, data: Vec<u8>) -> Result<(), String> {
        let tx = {
            let procs = self.processes.lock().await;
            procs.get(session_id).map(|p| p.stdin_tx.clone())
        };
        match tx {
            Some(tx) => tx.send(data).await.map_err(|_| "stdin channel closed".to_string()),
            None => Err(format!("no spawned process for session {session_id}")),
        }
    }

    /// Kill a spawned process (server AgentExecCancel or session teardown).
    /// 返回 false 表示无此进程。
    pub async fn kill(&self, session_id: &str) -> bool {
        let proc = self.processes.lock().await.remove(session_id);
        if let Some(p) = proc {
            let _ = p.kill_tx.send(());
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn test_spawn_echo_roundtrip() {
        let mgr = SpawnManager::new();
        let (tx, mut rx) = mpsc::channel(32);
        mgr.handle_spawn(
            "s1".into(), "cat".into(), vec![], vec![], None, tx,
        )
        .await;
        // 第一条应是 AgentSpawnResponse success
        let resp = timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(resp, ControlMessage::AgentSpawnResponse { success: true, .. }));

        mgr.write_stdin("s1", b"hello\n".to_vec()).await.unwrap();
        // 读 stdout 回显
        let mut got = Vec::new();
        while got.len() < 6 {
            match timeout(Duration::from_secs(5), rx.recv()).await.unwrap() {
                Some(ControlMessage::AgentSpawnData { data, stdin: false, .. }) => got.extend(data),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(&got[..6], b"hello\n");

        assert!(mgr.kill("s1").await);
        // 应收到 Exit
        let exit = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(ControlMessage::AgentSpawnExit { .. }) = rx.recv().await {
                    break;
                }
            }
        }).await;
        assert!(exit.is_ok());
    }

    #[tokio::test]
    async fn test_spawn_missing_command() {
        let mgr = SpawnManager::new();
        let (tx, mut rx) = mpsc::channel(32);
        mgr.handle_spawn(
            "s2".into(), "/nonexistent/binary".into(), vec![], vec![], None, tx,
        )
        .await;
        let resp = timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        match resp {
            ControlMessage::AgentSpawnResponse { success, error, .. } => {
                assert!(!success);
                assert!(error.unwrap().contains("spawn"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_write_stdin_unknown_session() {
        let mgr = SpawnManager::new();
        assert!(mgr.write_stdin("nope", vec![1]).await.is_err());
        assert!(!mgr.kill("nope").await);
    }
}
