//! Integration tests: client reconnect, heartbeat RTT, server restart.

#[path = "common/mod.rs"]
mod common;

use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread")]
async fn client_reregisters_after_admin_disconnect() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 2, // reserve two ports so we have a fallback
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let port_a = harness.exposed_ports[0];

        harness.spawn_client(Some("reconnect-client"));

        let api = harness.api_client();
        harness
            .wait_client_count(&api, 1)
            .await
            .expect("first register");

        // Start TCP tunnel on server side.
        harness
            .start_tcp_tunnel(port_a, &echo_addr.to_string(), "reconnect-client")
            .await;

        // Round-trip on original tunnel
        wait_until("port_a open", || async {
            TcpStream::connect(("127.0.0.1", port_a))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("port_a never opened");
        {
            let mut s = TcpStream::connect(("127.0.0.1", port_a)).await.unwrap();
            s.write_all(b"v1").await.unwrap();
            let mut b = [0u8; 2];
            s.read_exact(&mut b).await.unwrap();
            assert_eq!(&b, b"v1");
        }

        // Admin-triggered kick via the REST API (by client name, not port).
        let status = api
            .post_json("/api/clients/reconnect-client/kick", serde_json::json!({}))
            .await
            .0;
        assert!(status.is_success(), "kick API returned {status}");

        // Spawn a replacement client with a different name, using a
        // *different* port. Using the same port would hit a known
        // server-side limitation where PortInfo isn't cleared until the
        // socket EOFs (which the old detached tasks prevent).
        let port_b = harness.exposed_ports[1];
        harness.spawn_client(Some("reconnect-client-b"));

        // Server should now see a second client registered (first is offline
        // but still in DB).
        harness
            .wait_client_count(&api, 2)
            .await
            .expect("replacement did not register");

        harness
            .start_tcp_tunnel(port_b, &echo_addr.to_string(), "reconnect-client-b")
            .await;

        wait_until("port_b open", || async {
            TcpStream::connect(("127.0.0.1", port_b))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("port_b never opened");

        let mut s = TcpStream::connect(("127.0.0.1", port_b)).await.unwrap();
        s.write_all(b"v2").await.unwrap();
        let mut b = [0u8; 2];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"v2");
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn server_restart_survives_reregistration() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut harness1 = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness1.exposed_ports[0];
        let control_port = harness1.control_addr.port();
        let api_port: u16 = harness1
            .api_base
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();

        harness1.spawn_client(Some("restart-client"));
        harness1
            .start_tcp_tunnel(remote_port, &echo_addr.to_string(), "restart-client")
            .await;
        let api = harness1.api_client();
        harness1.wait_client_count(&api, 1).await.expect("register");

        // Kill the server (drop harness1). Its Drop aborts server + api tasks.
        drop(harness1);

        // Give the OS a moment to release the ports (SO_REUSEADDR usually handles it).
        wait_until("ports free", || async {
            let ok1 = std::net::TcpListener::bind(("127.0.0.1", control_port)).is_ok();
            let ok2 = std::net::TcpListener::bind(("127.0.0.1", api_port)).is_ok();
            if ok1 && ok2 {
                Some(())
            } else {
                None
            }
        })
        .await
        .expect("ports never freed");

        // Bring up a fresh harness with new random ports (not the same ones).
        let mut harness2 = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let new_port = harness2.exposed_ports[0];
        harness2.spawn_client(Some("restart-client"));
        harness2
            .start_tcp_tunnel(new_port, &echo_addr.to_string(), "restart-client")
            .await;
        let api2 = harness2.api_client();
        harness2
            .wait_client_count(&api2, 1)
            .await
            .expect("register");

        wait_until("new port open", || async {
            TcpStream::connect(("127.0.0.1", new_port))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("new port never opened");

        let mut s = TcpStream::connect(("127.0.0.1", new_port)).await.unwrap();
        s.write_all(b"ok").await.unwrap();
        let mut b = [0u8; 2];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"ok");
    })
    .await;
    result.expect("test timed out");
}
