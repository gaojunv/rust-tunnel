//! `ClientTunnelStream` bridges a per-connection pair of control-channel
//! messages (`Data` / `Close`) with `AsyncRead + AsyncWrite`. Used by
//! `ClientConnector` to hand the reverse-proxy handler a stream that
//! transparently talks to a client's internal target.
//!
//! Write path: chunk the buffer to <=64 KiB `Data { cid, bytes }` messages
//!             sent on `control_sender`.
//! Read path:  await bytes on a per-connection mpsc receiver fed by the
//!             server main loop's message dispatcher.
//! Shutdown:   emit a single `Close { cid }` on `poll_shutdown` or drop.
//!
//! Back-pressure: when the control sender is full, `poll_write` returns
//! `Poll::Pending` and registers a waker via `Sender::reserve()`. Natural
//! TCP back-pressure propagates end-to-end.
//!
//! See docs/superpowers/specs/2026-07-18-web-managed-client-routing-design.md §3.4

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

use rust_tunnel_common::ControlMessage;
use crate::server::stats::{EntityType, StatsCollector};

/// Max payload of a single `Data` message. Chosen well under the 1 MiB
/// serialize cap in `common::protocol` so header/framing overhead fits
/// comfortably, and aligned with `tokio::io::copy` default buffer size.
const MAX_CHUNK: usize = 64 * 1024;

/// 绑定的统计上下文：按 client_name 记录流量与连接数。
struct StreamStats {
    collector: StatsCollector,
    entity_id: String,
}

pub struct ClientTunnelStream {
    connection_id: u64,
    control_sender: mpsc::Sender<ControlMessage>,
    inbound: mpsc::Receiver<Vec<u8>>,
    /// Bytes left over from a `Data` frame that did not fit in the last read
    /// buffer. Drained on the next `poll_read` before pulling a new frame.
    read_remainder: Vec<u8>,
    /// Set once `poll_shutdown` or drop has emitted `Close`. Subsequent writes
    /// return `BrokenPipe`, subsequent shutdowns are no-ops.
    closed: bool,
    /// 统计上下文（None = 不记录，测试用）
    stats: Option<StreamStats>,
}

impl ClientTunnelStream {
    #[must_use]
    pub fn new(
        connection_id: u64,
        control_sender: mpsc::Sender<ControlMessage>,
        inbound: mpsc::Receiver<Vec<u8>>,
    ) -> Self {
        Self {
            connection_id,
            control_sender,
            inbound,
            read_remainder: Vec::new(),
            closed: false,
            stats: None,
        }
    }

    /// 绑定统计上下文：绑定即视为 tunnel 打开（incr_conns），Drop 时
    /// 视为关闭（decr_conns），读写时按 client_name 累计流量。
    #[must_use]
    pub fn with_stats(mut self, collector: StatsCollector, client_name: &str) -> Self {
        collector.incr_conns(EntityType::Client, client_name);
        self.stats = Some(StreamStats {
            collector,
            entity_id: client_name.to_string(),
        });
        self
    }

    /// 服务器 → 客户端方向的字节（bytes_out）
    fn record_bytes_out(&self, n: u64) {
        if let Some(stats) = &self.stats {
            stats
                .collector
                .record_bytes(EntityType::Client, &stats.entity_id, 0, n);
        }
    }
}

impl AsyncRead for ClientTunnelStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let filled_before = buf.filled().len();
        // Drain leftover from a previous partial read first.
        let result = if !self.read_remainder.is_empty() {
            let n = std::cmp::min(self.read_remainder.len(), buf.remaining());
            buf.put_slice(&self.read_remainder[..n]);
            self.read_remainder.drain(..n);
            Poll::Ready(Ok(()))
        } else {
            match self.inbound.poll_recv(cx) {
                Poll::Ready(Some(mut bytes)) => {
                    let n = std::cmp::min(bytes.len(), buf.remaining());
                    buf.put_slice(&bytes[..n]);
                    if n < bytes.len() {
                        bytes.drain(..n);
                        self.read_remainder = bytes;
                    }
                    Poll::Ready(Ok(()))
                }
                // Inbound channel closed → EOF: leave buf untouched, return Ok(())
                Poll::Ready(None) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        };
        // 客户端 → 服务器方向的字节（bytes_in）
        if let Poll::Ready(Ok(())) = &result {
            let n = (buf.filled().len() - filled_before) as u64;
            if n > 0 {
                if let Some(stats) = &self.stats {
                    stats
                        .collector
                        .record_bytes(EntityType::Client, &stats.entity_id, n, 0);
                }
            }
        }
        result
    }
}

impl AsyncWrite for ClientTunnelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.closed {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "tunnel closed",
            )));
        }
        let chunk_len = std::cmp::min(buf.len(), MAX_CHUNK);
        let cid = this.connection_id;

        // Fast path: reserve a permit without allocation.
        match this.control_sender.try_reserve() {
            Ok(permit) => {
                permit.send(ControlMessage::Data {
                    connection_id: cid,
                    data: buf[..chunk_len].to_vec(),
                });
                this.record_bytes_out(chunk_len as u64);
                return Poll::Ready(Ok(chunk_len));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "control channel closed",
                )));
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Fall through to the async reserve path for back-pressure.
            }
        }

        // Slow path: capacity is full — poll a `reserve()` future once so the
        // sender's waker fires when a slot frees up.
        let fut = this.control_sender.reserve();
        tokio::pin!(fut);
        match fut.poll(cx) {
            Poll::Ready(Ok(permit)) => {
                permit.send(ControlMessage::Data {
                    connection_id: cid,
                    data: buf[..chunk_len].to_vec(),
                });
                this.record_bytes_out(chunk_len as u64);
                Poll::Ready(Ok(chunk_len))
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "control channel closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // The control channel does not buffer; a successful `poll_write` means
        // the frame is on the wire (or the writer task's queue).
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.closed {
            return Poll::Ready(Ok(()));
        }
        self.closed = true;
        let cid = self.connection_id;
        // Best-effort: if the channel is full or closed we can't do more from
        // a sync context. The peer will see EOF from a different signal.
        let _ = self
            .control_sender
            .try_send(ControlMessage::Close { connection_id: cid });
        Poll::Ready(Ok(()))
    }
}

impl Drop for ClientTunnelStream {
    fn drop(&mut self) {
        // tunnel 关闭：无论何种退出路径都 decr_conns（与 with_stats 的 incr 配对）
        if let Some(stats) = &self.stats {
            stats
                .collector
                .decr_conns(EntityType::Client, &stats.entity_id);
        }
        if self.closed {
            return;
        }
        self.closed = true;
        let sender = self.control_sender.clone();
        let cid = self.connection_id;
        // Spawn so we don't require sync availability of a runtime handle.
        tokio::spawn(async move {
            let _ = sender
                .send(ControlMessage::Close { connection_id: cid })
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_tunnel_common::ControlMessage;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    /// write() → Data{cid, bytes} on control_sender.
    #[tokio::test]
    async fn test_write_produces_data_messages() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let (_inbound_tx, inbound_rx) = mpsc::channel(8);
        let mut stream = ClientTunnelStream::new(42, ctl_tx, inbound_rx);

        stream.write_all(b"hello").await.unwrap();
        stream.flush().await.unwrap();

        let msg = ctl_rx.recv().await.unwrap();
        match msg {
            ControlMessage::Data {
                connection_id,
                data,
            } => {
                assert_eq!(connection_id, 42);
                assert_eq!(data, b"hello");
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    /// read() pulls bytes from the inbound mpsc.
    #[tokio::test]
    async fn test_read_pulls_from_inbound() {
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let (inbound_tx, inbound_rx) = mpsc::channel(8);
        let mut stream = ClientTunnelStream::new(42, ctl_tx, inbound_rx);

        inbound_tx.send(b"world".to_vec()).await.unwrap();
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
    }

    /// A large write is split into <=64 KiB chunks; each chunk is one Data.
    #[tokio::test]
    async fn test_write_chunks_large_payload() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let (_inbound_tx, inbound_rx) = mpsc::channel(8);
        let mut stream = ClientTunnelStream::new(7, ctl_tx, inbound_rx);

        // 100 KiB → expect 2 Data messages: 64K + 36K.
        let payload = vec![0xAB; 100 * 1024];
        stream.write_all(&payload).await.unwrap();

        let first = ctl_rx.recv().await.unwrap();
        let second = ctl_rx.recv().await.unwrap();
        match (first, second) {
            (
                ControlMessage::Data {
                    connection_id: c1,
                    data: d1,
                },
                ControlMessage::Data {
                    connection_id: c2,
                    data: d2,
                },
            ) => {
                assert_eq!(c1, 7);
                assert_eq!(c2, 7);
                assert_eq!(d1.len(), MAX_CHUNK);
                assert_eq!(d2.len(), payload.len() - MAX_CHUNK);
            }
            other => panic!("expected two Data messages, got {other:?}"),
        }
    }

    /// drop() sends Close{cid} exactly once.
    #[tokio::test]
    async fn test_drop_sends_close() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let (_inbound_tx, inbound_rx) = mpsc::channel(8);
        let stream = ClientTunnelStream::new(42, ctl_tx, inbound_rx);
        drop(stream);

        let msg = tokio::time::timeout(std::time::Duration::from_millis(500), ctl_rx.recv())
            .await
            .expect("timeout waiting for Close")
            .unwrap();
        assert!(matches!(msg, ControlMessage::Close { connection_id: 42 }));
    }

    /// poll_shutdown sends Close and suppresses drop's second Close.
    #[tokio::test]
    async fn test_shutdown_sends_close_once() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMessage>(8);
        let (_inbound_tx, inbound_rx) = mpsc::channel(8);
        let mut stream = ClientTunnelStream::new(9, ctl_tx, inbound_rx);
        stream.shutdown().await.unwrap();

        // First Close arrives from poll_shutdown's try_send.
        let first = ctl_rx.recv().await.unwrap();
        assert!(matches!(first, ControlMessage::Close { connection_id: 9 }));

        // drop must NOT emit a second Close because closed flag is already set.
        drop(stream);
        tokio::task::yield_now().await;
        assert!(
            ctl_rx.try_recv().is_err(),
            "second Close should not be sent after shutdown"
        );
    }

    /// Inbound channel closed → read() returns 0 (EOF).
    #[tokio::test]
    async fn test_read_eof_when_inbound_closed() {
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let (inbound_tx, inbound_rx) = mpsc::channel(8);
        let mut stream = ClientTunnelStream::new(42, ctl_tx, inbound_rx);
        drop(inbound_tx);

        let mut buf = [0u8; 4];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(n, 0);
    }

    /// Writing after shutdown returns BrokenPipe.
    #[tokio::test]
    async fn test_write_after_shutdown_broken_pipe() {
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let (_inbound_tx, inbound_rx) = mpsc::channel(8);
        let mut stream = ClientTunnelStream::new(1, ctl_tx, inbound_rx);
        stream.shutdown().await.unwrap();
        // Drain the Close so subsequent asserts don't confuse recv() ordering.
        let _ = ctl_rx.recv().await;

        let err = stream.write_all(b"x").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// A short inbound chunk followed by a larger read exercises the
    /// read_remainder path: two Data frames delivered, one read pulls both.
    #[tokio::test]
    async fn test_read_across_multiple_inbound_frames() {
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let (inbound_tx, inbound_rx) = mpsc::channel(8);
        let mut stream = ClientTunnelStream::new(1, ctl_tx, inbound_rx);

        inbound_tx.send(b"foo".to_vec()).await.unwrap();
        let mut buf = [0u8; 3];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"foo");

        inbound_tx.send(b"bar".to_vec()).await.unwrap();
        let mut buf = [0u8; 3];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"bar");
    }

    /// with_stats 绑定即 incr_conns，Drop 时 decr_conns（覆盖所有退出路径）。
    #[tokio::test]
    async fn test_stats_incr_on_bind_decr_on_drop() {
        use crate::server::stats::StatsCollector;
        let collector = StatsCollector::new(None);
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let (_inbound_tx, inbound_rx) = mpsc::channel(8);
        let stream =
            ClientTunnelStream::new(1, ctl_tx, inbound_rx).with_stats(collector.clone(), "c1");
        assert_eq!(collector.get_summary().clients.total_conns, 1);
        drop(stream);
        assert_eq!(collector.get_summary().clients.total_conns, 0);
    }

    /// 读写双向流量按 client_name 累计到 StatsCollector。
    #[tokio::test]
    async fn test_stats_bytes_recorded_on_read_write() {
        use crate::server::stats::StatsCollector;
        let collector = StatsCollector::new(None);
        let (ctl_tx, mut ctl_rx) = mpsc::channel(8);
        let (inbound_tx, inbound_rx) = mpsc::channel(8);
        let mut stream =
            ClientTunnelStream::new(1, ctl_tx, inbound_rx).with_stats(collector.clone(), "c1");

        // 服务器 → 客户端（bytes_out）
        stream.write_all(b"hello").await.unwrap();
        let _ = ctl_rx.recv().await;

        // 客户端 → 服务器（bytes_in）
        inbound_tx.send(b"world".to_vec()).await.unwrap();
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).await.unwrap();

        let summary = collector.get_summary();
        assert_eq!(summary.clients.total_bytes_out, 5);
        assert_eq!(summary.clients.total_bytes_in, 5);
        assert_eq!(summary.clients.entity_count, 1);
    }
}
