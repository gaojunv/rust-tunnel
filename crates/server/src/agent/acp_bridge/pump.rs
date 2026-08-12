//! stdio pump：进程 stdout/stdin 与 ACP 连接（duplex）之间的字节转发。
//!
//! 每 session 一对 `tokio::io::duplex`：ACP 端（`agent_io`）交给
//! `agent-client-protocol` 的 `Client` 作为 `ByteStreams`；pump 端（`pump_io`）
//! 由 [`run_stdio_pump`] 驱动。duplex 本身就是缓冲区：handshake 期间进程早产
//! stdout 会缓冲在 duplex/通道里（不会丢），ACP 连接建立后随即消费。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use rust_tunnel_common::ControlMessage;

/// stdio pump：进程 stdout（`stdout_rx`，即 `AgentSpawnData{stdin:false}` 转来）
/// → 写 duplex 喂 ACP crate；ACP crate 写出的字节从 duplex 读回 → 以
/// `AgentSpawnData{stdin:true}` 下发客户端进程 stdin。
///
/// 任一端关闭即整体退出：`stdout_rx` 通道关闭（进程 exit / 会话被移除）时先
/// 排空再退出；ACP 端 EOF（duplex 对端 drop）也退出。
pub(super) async fn run_stdio_pump(
    pump_io: tokio::io::DuplexStream,
    mut stdout_rx: mpsc::Receiver<Vec<u8>>,
    control_tx: mpsc::Sender<ControlMessage>,
    session_id: String,
) {
    let (mut pump_rd, mut pump_wr) = tokio::io::split(pump_io);
    let mut buf = vec![0u8; 8192];
    loop {
        tokio::select! {
            data = stdout_rx.recv() => {
                match data {
                    // 进程 stdout → ACP。写失败 = ACP 端已关闭，收尾。
                    Some(data) => {
                        if pump_wr.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    // 通道关闭（exit/会话移除）：排空已完成，退出。
                    None => break,
                }
            }
            // ACP → 进程 stdin。
            n = pump_rd.read(&mut buf) => {
                match n {
                    Ok(0) => break, // ACP 连接已关闭
                    Ok(n) => {
                        let msg = ControlMessage::AgentSpawnData {
                            session_id: session_id.clone(),
                            data: buf[..n].to_vec(),
                            stdin: true,
                        };
                        if control_tx.send(msg).await.is_err() {
                            break; // 客户端断连
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    tracing::debug!(session_id, "acp stdio pump exited");
}

/// 把 tokio 的 `AsyncRead`/`AsyncWrite` 适配成 futures 的——ACP crate 的
/// `ByteStreams` 走 futures-rs trait，而我们的 stdio duplex 是 tokio 类型。
/// 由 [`tokio::io::split`] 出的读写半各自包一层，保持 `Unpin`。
pub(super) struct FuturesIo<T>(pub(super) T);

impl<T: tokio::io::AsyncRead + Unpin> futures_util::io::AsyncRead for FuturesIo<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // tokio 1.x 的 poll_read 走 ReadBuf；futures 的直接给 &mut [u8]。
        let mut rb = tokio::io::ReadBuf::new(buf);
        match std::pin::Pin::new(&mut self.0).poll_read(cx, &mut rb) {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(rb.filled().len())),
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> futures_util::io::AsyncWrite for FuturesIo<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
