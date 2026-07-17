//! Unit and integration tests for Shadowsocks handshake and data relay.
//!
//! Integration tests require `ss-local` from shadowsocks-libev to be installed.

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::server::control::ServerState;
use crate::server::listener;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Bind to port 0 to get a random available port, then return it.
async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Start a TCP echo server on a random port. Returns the port and a handle
/// that aborts the server on drop (just abort the JoinHandle).
async fn start_echo_server() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let (mut reader, mut writer) = tokio::io::split(stream);
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if writer.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    (port, handle)
}

/// Spawn `ss-local` as a child process.
fn start_ss_local(ss_server_port: u16, socks5_port: u16, cipher: &str, password: &str) -> Child {
    Command::new("ss-local")
        .args([
            "-s",
            "127.0.0.1",
            "-p",
            &ss_server_port.to_string(),
            "-l",
            &socks5_port.to_string(),
            "-k",
            password,
            "-m",
            cipher,
        ])
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("Failed to start ss-local - is shadowsocks-libev installed?")
}

/// Retry connecting to `port` until success or `dur` elapses.
async fn wait_for_port(port: u16, dur: Duration) {
    let start = tokio::time::Instant::now();
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        if start.elapsed() >= dur {
            panic!("Timed out waiting for port {}", port);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Perform a minimal SOCKS5 CONNECT handshake through `proxy_port`, then
/// send `data` to `target_addr` and return the echoed response.
async fn socks5_send_recv(proxy_port: u16, target_addr: SocketAddr, data: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("Failed to connect to SOCKS5 proxy");

    // ---- Step 1: method negotiation ----
    // Client -> Proxy: [VER=0x05, NMETHODS=1, METHOD=0x00 (no-auth)]
    stream.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    // Proxy -> Client: [VER=0x05, METHOD=0x00]
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf, [0x05, 0x00], "SOCKS5 auth method mismatch");

    // ---- Step 2: CONNECT request ----
    let ip = match target_addr.ip() {
        std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
        std::net::IpAddr::V6(_v6) => panic!("IPv6 not used in tests"),
    };
    let port = target_addr.port();
    let connect_req = [
        &[0x05, 0x01, 0x00, 0x01][..], // VER, CMD=CONNECT, RSV, ATYP=IPv4
        &ip,
        &[(port >> 8) as u8, (port & 0xff) as u8],
    ]
    .concat();
    stream.write_all(&connect_req).await.unwrap();

    // Read SOCKS5 reply: VER, REP, RSV, ATYP, BND.ADDR (4 or 16), BND.PORT (2)
    let mut reply = [0u8; 4]; // fixed header
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[0], 0x05, "SOCKS5 VER mismatch");
    assert_eq!(reply[1], 0x00, "SOCKS5 REP error: {:02x}", reply[1]);

    let bind_addr_len = match reply[3] {
        0x01 => 4,  // IPv4
        0x04 => 16, // IPv6
        other => panic!("Unexpected ATYP: {}", other),
    };
    let mut bind_rest = vec![0u8; bind_addr_len + 2];
    stream.read_exact(&mut bind_rest).await.unwrap();
    // bind_rest = [BND.ADDR..., BND.PORT(2)]

    // ---- Step 3: send data and read exact-length echo ----
    stream.write_all(data).await.unwrap();

    // Read back exactly the same number of bytes we sent.
    // Avoid half-close (shutdown) because ss-local may tear down the SS tunnel
    // before the response fully propagates.
    let expected_len = data.len();
    let mut response = vec![0u8; expected_len];
    stream.read_exact(&mut response).await.unwrap();

    // Now close cleanly
    stream.shutdown().await.ok();

    response
}

// ---------------------------------------------------------------------------
// unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unit_tests {
    use crate::server::shadowsocks::{derive_key, parse_cipher_kind};

    #[tokio::test]
    async fn test_key_derivation_consistency() {
        let password = "testpassword";
        let cipher = "aes-256-gcm";

        let key = derive_key(password, cipher).unwrap();
        assert_eq!(key.len(), 32);

        let kind = parse_cipher_kind(cipher).unwrap();
        let server_addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let cfg = ::shadowsocks::config::ServerConfig::new(server_addr, password, kind).unwrap();
        assert_eq!(key, cfg.key());
    }
}

// ---------------------------------------------------------------------------
// integration tests — require ss-local
// ---------------------------------------------------------------------------

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[tokio::test]
    #[ignore] // Requires ss-local from shadowsocks-libev
    async fn test_ss_echo_aes256gcm_via_ss_local() {
        // 1. echo server
        let (echo_port, echo_handle) = start_echo_server().await;

        // 2. SS listener
        let state = ServerState::new();
        let ss_port = find_available_port().await;
        let state_c = state.clone();
        tokio::spawn(async move {
            let _ = listener::start_shadowsocks_listener(
                state_c,
                ss_port,
                "aes-256-gcm".into(),
                "testpass".into(),
            )
            .await;
        });

        // give the SS listener a moment to bind
        wait_for_port(ss_port, Duration::from_secs(3)).await;

        // 3. ss-local
        let socks5_port = find_available_port().await;
        let mut ss_local = start_ss_local(ss_port, socks5_port, "aes-256-gcm", "testpass");
        wait_for_port(socks5_port, Duration::from_secs(5)).await;

        // 4. SOCKS5 -> echo server
        let echo_addr: SocketAddr = ([127, 0, 0, 1], echo_port).into();
        let echoed = socks5_send_recv(socks5_port, echo_addr, b"hello shadowsocks").await;
        assert_eq!(echoed, b"hello shadowsocks");

        // 5. cleanup
        ss_local.kill().ok();
        ss_local.wait().ok();
        echo_handle.abort();
    }

    #[tokio::test]
    #[ignore] // Requires ss-local from shadowsocks-libev
    async fn test_ss_chacha20_poly1305_via_ss_local() {
        let (echo_port, echo_handle) = start_echo_server().await;

        let state = ServerState::new();
        let ss_port = find_available_port().await;
        let state_c = state.clone();
        tokio::spawn(async move {
            let _ = listener::start_shadowsocks_listener(
                state_c,
                ss_port,
                "chacha20-ietf-poly1305".into(),
                "testpass".into(),
            )
            .await;
        });
        wait_for_port(ss_port, Duration::from_secs(3)).await;

        let socks5_port = find_available_port().await;
        let mut ss_local =
            start_ss_local(ss_port, socks5_port, "chacha20-ietf-poly1305", "testpass");
        wait_for_port(socks5_port, Duration::from_secs(5)).await;

        let echo_addr: SocketAddr = ([127, 0, 0, 1], echo_port).into();
        let echoed = socks5_send_recv(socks5_port, echo_addr, b"chacha20 test").await;
        assert_eq!(echoed, b"chacha20 test");

        ss_local.kill().ok();
        ss_local.wait().ok();
        echo_handle.abort();
    }

    #[tokio::test]
    #[ignore] // Requires ss-local from shadowsocks-libev
    async fn test_ss_large_data_transfer() {
        let (echo_port, echo_handle) = start_echo_server().await;

        let state = ServerState::new();
        let ss_port = find_available_port().await;
        let state_c = state.clone();
        tokio::spawn(async move {
            let _ = listener::start_shadowsocks_listener(
                state_c,
                ss_port,
                "aes-256-gcm".into(),
                "testpass".into(),
            )
            .await;
        });
        wait_for_port(ss_port, Duration::from_secs(3)).await;

        let socks5_port = find_available_port().await;
        let mut ss_local = start_ss_local(ss_port, socks5_port, "aes-256-gcm", "testpass");
        wait_for_port(socks5_port, Duration::from_secs(5)).await;

        // 64 KB payload
        let payload: Vec<u8> = (0u8..=255).cycle().take(65536).collect();
        let echo_addr: SocketAddr = ([127, 0, 0, 1], echo_port).into();
        let echoed = socks5_send_recv(socks5_port, echo_addr, &payload).await;
        assert_eq!(echoed, payload);

        ss_local.kill().ok();
        ss_local.wait().ok();
        echo_handle.abort();
    }

    #[tokio::test]
    #[ignore] // Requires ss-local from shadowsocks-libev
    async fn test_ss_active_connection_count() {
        let (echo_port, echo_handle) = start_echo_server().await;

        let state = ServerState::new();
        let ss_port = find_available_port().await;
        let state_c = state.clone();
        tokio::spawn(async move {
            let _ = listener::start_shadowsocks_listener(
                state_c,
                ss_port,
                "aes-256-gcm".into(),
                "testpass".into(),
            )
            .await;
        });
        wait_for_port(ss_port, Duration::from_secs(3)).await;

        // No active SS connections yet
        assert_eq!(
            state.get_connection_count_for_port(ss_port).await,
            0,
            "expected 0 SS connections before any connection"
        );

        let socks5_port = find_available_port().await;
        let mut ss_local = start_ss_local(ss_port, socks5_port, "aes-256-gcm", "testpass");
        wait_for_port(socks5_port, Duration::from_secs(5)).await;

        // Open a connection through the SS tunnel
        let echo_addr: SocketAddr = ([127, 0, 0, 1], echo_port).into();
        let echoed = socks5_send_recv(socks5_port, echo_addr, b"connection count test").await;
        assert_eq!(echoed, b"connection count test");

        // After communicating, the SS connection counter should have been
        // incremented (the SS proxy calls increment_ss_connections on start).
        let count = state.get_connection_count_for_port(ss_port).await;
        assert!(
            count >= 1,
            "SS connection count should be >= 1 after data transfer, got {}",
            count
        );

        ss_local.kill().ok();
        ss_local.wait().ok();
        echo_handle.abort();
    }
}
