//! Connector abstraction: lets reverse-proxy handlers dial a backend without
//! caring whether it's an external host or an already-connected tunnel client.
//!
//! - `DirectConnector` wraps `TcpStream::connect(&backend.addr)` (existing
//!   behavior).
//! - `ClientConnector` will be added in a later task; it uses `ClientRegistry`
//!   to route dials through a tunneled control channel.
//!
//! See docs/superpowers/specs/2026-07-18-web-managed-client-routing-design.md
//! §1.4 and §3.4.

use std::io;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::server::reverse_proxy::Backend;

/// Marker trait combining tokio's async read+write. Blanket-impl'd for any
/// type that satisfies both — lets us return `Box<dyn AsyncReadWrite + ...>`
/// without listing both super-traits at every call site.
pub trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite + ?Sized> AsyncReadWrite for T {}

/// Boxed duplex stream returned by every `Connector::connect`. `Send + Unpin`
/// so it can cross await points and be handed to `tokio::io::copy_bidirectional`.
pub type BoxedStream = Box<dyn AsyncReadWrite + Unpin + Send>;

#[async_trait]
pub trait Connector: Send + Sync {
    /// Dial the backend and return a duplex stream ready for I/O.
    async fn connect(&self, backend: &Backend) -> io::Result<BoxedStream>;
}

/// Dials an external `host:port` directly with `TcpStream::connect`.
pub struct DirectConnector;

#[async_trait]
impl Connector for DirectConnector {
    async fn connect(&self, backend: &Backend) -> io::Result<BoxedStream> {
        let s = TcpStream::connect(&backend.addr).await?;
        Ok(Box::new(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::reverse_proxy::{BackendKind, BackendProtocol, BackendScheme};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn test_backend(addr: String) -> Backend {
        Backend {
            kind: BackendKind::Direct,
            addr,
            client_name: None,
            weight: 100,
            protocol: BackendProtocol::Http1,
            scheme: BackendScheme::Http,
        }
    }

    #[tokio::test]
    async fn direct_connector_dials_tcp_and_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            s.write_all(b"pong").await.unwrap();
        });

        let mut stream = DirectConnector
            .connect(&test_backend(addr.to_string()))
            .await
            .unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn direct_connector_returns_err_on_refused() {
        // 127.0.0.1:1 is (almost always) closed; dial should fail promptly.
        let backend = test_backend("127.0.0.1:1".to_string());
        let result = DirectConnector.connect(&backend).await;
        let err = result.err().expect("expected an error, got Ok");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::AddrNotAvailable
                    | io::ErrorKind::PermissionDenied
            ),
            "unexpected kind: {:?}",
            err.kind()
        );
    }
}