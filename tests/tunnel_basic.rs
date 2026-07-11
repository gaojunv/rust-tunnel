//! Integration tests: tunnel establishment and bidirectional forwarding.

#[path = "common/mod.rs"]
mod common;

use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use rust_tunnel::client::config::ForwardRule;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread")]
async fn tunnel_forwards_bytes_bidirectionally() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness.exposed_ports[0];

        harness.spawn_client(vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);

        let api = harness.api_client();
        harness
            .wait_client_count(&api, 1)
            .await
            .expect("client did not register");

        // Wait for the listener to be actually bound on the exposed port.
        wait_until("exposed port open", || async {
            TcpStream::connect(("127.0.0.1", remote_port))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("exposed port never opened");

        // Now send 128 KiB of random-ish bytes and verify echo.
        let mut sock = TcpStream::connect(("127.0.0.1", remote_port))
            .await
            .expect("connect exposed");
        let payload: Vec<u8> = (0..131_072u32)
            .map(|i| (i as u8).wrapping_mul(31))
            .collect();
        sock.write_all(&payload).await.expect("write");

        let mut recv = vec![0u8; payload.len()];
        sock.read_exact(&mut recv).await.expect("read");
        assert_eq!(recv, payload, "bytes should round-trip through tunnel");
    })
    .await;

    result.expect("test timed out after 15s");
}

#[tokio::test(flavor = "multi_thread")]
async fn tunnel_forwards_with_tls_disabled() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        // Ride HarnessOpts::default() (currently tls=false via bool::default()).
        // If someone later flips the default, this test will start exercising
        // that new path instead of silently sticking to tls=false.
        let mut harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness.exposed_ports[0];

        harness.spawn_client(vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);

        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("register");

        wait_until("exposed port open", || async {
            TcpStream::connect(("127.0.0.1", remote_port))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("exposed port never opened");

        let mut sock = TcpStream::connect(("127.0.0.1", remote_port))
            .await
            .expect("connect");
        sock.write_all(b"ping").await.expect("write");

        let mut buf = [0u8; 4];
        sock.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"ping");
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn tunnel_multi_port() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 2,
            ..HarnessOpts::default()
        })
        .await;

        let echo_a = spawn_echo().await;
        let echo_b = spawn_echo().await;
        let port_a = harness.exposed_ports[0];
        let port_b = harness.exposed_ports[1];

        harness.spawn_client(vec![
            ForwardRule {
                remote_port: port_a,
                local_addr: echo_a.to_string(),
                dns_name: None,
            },
            ForwardRule {
                remote_port: port_b,
                local_addr: echo_b.to_string(),
                dns_name: None,
            },
        ]);

        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("register");

        wait_until("port_a open", || async {
            TcpStream::connect(("127.0.0.1", port_a))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("port_a never opened");
        wait_until("port_b open", || async {
            TcpStream::connect(("127.0.0.1", port_b))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("port_b never opened");

        // Concurrent traffic on both ports must not interleave.
        let a = tokio::spawn(async move {
            let mut s = TcpStream::connect(("127.0.0.1", port_a)).await.unwrap();
            s.write_all(b"AAAA").await.unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await.unwrap();
            b
        });
        let b = tokio::spawn(async move {
            let mut s = TcpStream::connect(("127.0.0.1", port_b)).await.unwrap();
            s.write_all(b"BBBB").await.unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await.unwrap();
            b
        });

        assert_eq!(&a.await.unwrap(), b"AAAA");
        assert_eq!(&b.await.unwrap(), b"BBBB");
    })
    .await;
    result.expect("test timed out");
}
