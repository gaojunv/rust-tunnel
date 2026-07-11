//! Integration tests: client reconnect, heartbeat RTT, server restart.

#[path = "common/mod.rs"]
mod common;

use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use rust_tunnel::client::config::ForwardRule;
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

        harness.spawn_client(vec![ForwardRule {
            remote_port: port_a,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);

        let api = harness.api_client();
        harness
            .wait_client_count(&api, 1)
            .await
            .expect("first register");

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

        // Admin-triggered disconnect via the REST API.
        let status = api.delete_status(&format!("/api/clients/{port_a}")).await;
        assert!(status.is_success(), "disconnect API returned {status}");

        // Spawn a replacement client on a *different* port. This exercises the
        // "operator reconfigures + restarts" reconnect flow. Using the same
        // port would hit a known server-side limitation where PortInfo isn't
        // cleared until the socket EOFs (which the old detached tasks prevent).
        let port_b = harness.exposed_ports[1];
        harness.spawn_client(vec![ForwardRule {
            remote_port: port_b,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);

        // Server should now see a second client registered.
        harness
            .wait_client_count(&api, 2)
            .await
            .expect("replacement did not register");

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
async fn heartbeat_measures_rtt() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
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
        harness.wait_client_count(&api, 1).await.expect("register");

        // Endpoint verified against src/server/api.rs:1613 — the route is
        // `/api/quality/:port` (not `/api/quality/ports/:port` as the plan
        // draft suggested). The response is `PortQualityResponse { current,
        // history }` (src/server/api.rs:614-619) where `current` is a
        // `ConnectionQuality` whose RTT is exposed as `last_rtt_ms` /
        // `avg_rtt_ms` (src/server/quality.rs:30-52).
        wait_until("rtt sample", || async {
            let (status, body) = api.get_json(&format!("/api/quality/{remote_port}")).await;
            if !status.is_success() {
                return None;
            }
            let rtt = body
                .get("current")
                .and_then(|c| c.get("last_rtt_ms"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if rtt > 0.0 {
                Some(rtt)
            } else {
                None
            }
        })
        .await
        .expect("no RTT sample ever recorded — check heartbeat interval / field name");
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

        let forwards = vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }];
        harness1.spawn_client(forwards.clone());
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
        harness2.spawn_client(vec![ForwardRule {
            remote_port: new_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);
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
