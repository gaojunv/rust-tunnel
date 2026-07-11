//! Tiny TCP echo backends used as tunnel targets.

use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a TCP echo server on 127.0.0.1:0. Returns the bound address.
/// Every incoming byte is echoed back verbatim. Task lives until the process exits.
pub async fn spawn_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// Spawn a tiny HTTP server that returns `200 OK\r\n\r\nhello` on any request.
/// Useful once we add SS/Trojan tests; safe to include now.
pub async fn spawn_http_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind http_echo");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                // Read the request headers (best-effort, up to the first \r\n\r\n).
                let _ = sock.read(&mut buf).await;
                let body =
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
                let _ = sock.write_all(body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}
