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

use crate::server::client_registry::ClientRegistry;
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

/// Dials a backend via a tunneled client control channel using `ClientRegistry`.
/// Backend must have `kind == Client` and `client_name` must be set.
pub struct ClientConnector {
    registry: ClientRegistry,
}

impl ClientConnector {
    #[must_use]
    pub fn new(registry: ClientRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Connector for ClientConnector {
    async fn connect(&self, backend: &Backend) -> io::Result<BoxedStream> {
        use crate::server::reverse_proxy::BackendKind;
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
        let stream = self
            .registry
            .open_tunnel(client_name, &backend.addr)
            .await?;
        Ok(Box::new(stream))
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

    #[tokio::test]
    async fn client_connector_full_path() {
        use crate::common::ControlMessage;
        use crate::server::client_registry::{ClientRegistry, TunnelOpenOutcome};
        use crate::server::db::Database;

        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("pw").await.unwrap();
        let registry = ClientRegistry::new(db);

        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel(32);
        let entry = registry
            .register("home-nas", None, None, "pw", client_tx)
            .await
            .unwrap();

        // Fake client task: reply TunnelOpenResult{true}, then echo any Data.
        let entry_for_client = entry.clone();
        tokio::spawn(async move {
            while let Some(msg) = client_rx.recv().await {
                match msg {
                    ControlMessage::OpenTunnel { connection_id, .. } => {
                        let mut conns = entry_for_client.active_connections.lock().await;
                        if let Some(active) = conns.get_mut(&connection_id) {
                            if let Some(tx) = active.open_result.take() {
                                let _ = tx.send(TunnelOpenOutcome::Ok);
                            }
                        }
                    }
                    ControlMessage::Data {
                        connection_id,
                        data,
                    } => {
                        let conns = entry_for_client.active_connections.lock().await;
                        if let Some(active) = conns.get(&connection_id) {
                            let _ = active.inbound.send(data).await;
                        }
                    }
                    _ => {}
                }
            }
        });

        let backend = Backend {
            kind: BackendKind::Client,
            addr: "irrelevant:80".into(),
            client_name: Some("home-nas".into()),
            weight: 100,
            protocol: BackendProtocol::Http1,
            scheme: BackendScheme::Http,
        };
        let connector = ClientConnector::new(registry);
        let mut stream = connector.connect(&backend).await.unwrap();

        stream.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
    }

    #[tokio::test]
    async fn client_connector_offline_returns_err() {
        use crate::server::client_registry::ClientRegistry;
        use crate::server::db::Database;

        let db = Database::new(":memory:").await.unwrap();
        let registry = ClientRegistry::new(db);
        let backend = Backend {
            kind: BackendKind::Client,
            addr: "x:80".into(),
            client_name: Some("nope".into()),
            weight: 100,
            protocol: BackendProtocol::Http1,
            scheme: BackendScheme::Http,
        };
        let connector = ClientConnector::new(registry);
        let result = connector.connect(&backend).await;
        match result {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotConnected),
            Ok(_) => panic!("expected NotConnected error, got Ok"),
        }
    }
}
