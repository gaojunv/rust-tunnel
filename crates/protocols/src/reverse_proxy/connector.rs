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

use crate::reverse_proxy::Backend;
use crate::tunnel_opener::TunnelOpener;
use std::sync::Arc;

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

/// Dials a backend via a tunneled client control channel using `ClientRegistry`.
/// Backend must have `kind == Client` and `client_name` must be set.
pub struct ClientConnector {
    opener: Arc<dyn TunnelOpener>,
}

impl ClientConnector {
    #[must_use]
    pub fn new(opener: Arc<dyn TunnelOpener>) -> Self {
        Self { opener }
    }
}

#[async_trait]
impl Connector for ClientConnector {
    async fn connect(&self, backend: &Backend) -> io::Result<BoxedStream> {
        use crate::reverse_proxy::BackendKind;
        if backend.kind != BackendKind::Client {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ClientConnector requires backend.kind == Client",
            ));
        }
        let client_name = backend.client_name.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "client backend missing client_name",
            )
        })?;
        let stream = self.opener.open_tunnel(client_name, &backend.addr).await?;
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reverse_proxy::{BackendKind, BackendProtocol, BackendScheme};
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

    #[tokio::test]
    async fn client_connector_full_path() {
        use crate::tunnel_opener::TunnelOpener;
        use std::sync::Arc;
        use tokio::io::duplex;

        struct EchoOpener;
        #[async_trait::async_trait]
        impl TunnelOpener for EchoOpener {
            async fn open_tunnel(
                &self,
                _client_name: &str,
                _target_addr: &str,
            ) -> std::io::Result<crate::reverse_proxy::connector::BoxedStream> {
                let (a, b) = duplex(64);
                // Echo task: copy a->b
                tokio::spawn(async move {
                    let (mut ra, mut wa) = tokio::io::split(a);
                    let (mut rb, mut wb) = tokio::io::split(b);
                    let _ = tokio::join!(
                        tokio::io::copy(&mut ra, &mut wb),
                        tokio::io::copy(&mut rb, &mut wa),
                    );
                });
                // Return one end that the connector caller will use; but duplex gives two ends, we need to return a duplex stream
                // Instead, use a simple echo duplex pair: create a new duplex for the caller
                let (client_side, server_side) = duplex(64);
                tokio::spawn(async move {
                    let (mut rs, mut ws) = tokio::io::split(server_side);
                    let mut buf = vec![0u8; 64];
                    loop {
                        match tokio::io::AsyncReadExt::read(&mut rs, &mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if tokio::io::AsyncWriteExt::write_all(&mut ws, &buf[..n])
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
                Ok(Box::new(client_side))
            }
        }

        let backend = Backend {
            kind: BackendKind::Client,
            addr: "irrelevant:80".into(),
            client_name: Some("home-nas".into()),
            weight: 100,
            protocol: BackendProtocol::Http1,
            scheme: BackendScheme::Http,
        };
        let opener: Arc<dyn TunnelOpener> = Arc::new(EchoOpener);
        let connector = ClientConnector::new(opener);
        let mut stream = connector.connect(&backend).await.unwrap();

        stream.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
    }

    #[tokio::test]
    async fn client_connector_offline_returns_err() {
        use crate::tunnel_opener::TunnelOpener;
        use std::sync::Arc;

        struct OfflineOpener;
        #[async_trait::async_trait]
        impl TunnelOpener for OfflineOpener {
            async fn open_tunnel(
                &self,
                _client_name: &str,
                _target_addr: &str,
            ) -> std::io::Result<crate::reverse_proxy::connector::BoxedStream> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "offline",
                ))
            }
        }

        let backend = Backend {
            kind: BackendKind::Client,
            addr: "x:80".into(),
            client_name: Some("nope".into()),
            weight: 100,
            protocol: BackendProtocol::Http1,
            scheme: BackendScheme::Http,
        };
        let opener: Arc<dyn TunnelOpener> = Arc::new(OfflineOpener);
        let connector = ClientConnector::new(opener);
        let result = connector.connect(&backend).await;
        match result {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotConnected),
            Ok(_) => panic!("expected NotConnected error, got Ok"),
        }
    }
}
