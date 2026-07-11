//! Integration tests: /api/logs SSE stream and /api/traffic bucket API.
//!
//! Route verification:
//!   * SSE: `/api/logs/stream` — src/server/api.rs:1601 (public route; token
//!     auth only when password is enabled). Emits named `event: log` frames
//!     whose `data:` is a JSON object with `message`, `level`, `source`, etc.
//!   * Traffic: `/api/traffic` returns `Vec<PortTraffic>` where each item is
//!     `{ port, total_bytes_in, total_bytes_out, buckets }`
//!     (src/server/api.rs:43-48 + 825).
//!
//! Log-plumbing note: the test harness only calls `run_server` /
//! `run_api_server`, neither of which install a tracing subscriber. The
//! *server-side* `LogLayer` is wired up only inside `src/bin/server.rs`, so
//! `tracing::…!` calls made from inside the server crate never reach the
//! log_store in our harness. What DOES reach it: any `tracing::…!` emitted
//! from within a running client (or from *this* test process once
//! `run_client` has installed its global subscriber), which is captured by
//! `ClientLogLayer`, batched by `spawn_log_forwarder`, sent as a
//! `ControlMessage::LogBatch`, and finally `log_store.send`'d by the server
//! (src/server/control.rs:1265-1289). That is the path exercised below.

#[path = "common/mod.rs"]
mod common;

use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use futures_util::StreamExt;
use rust_tunnel::client::config::ForwardRule;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SSE_MARKER: &str = "integration-test-sse-marker-abc123";

#[tokio::test(flavor = "multi_thread")]
async fn sse_streams_log_entries() {
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        // Spawn a client so that `run_client` installs the global tracing
        // subscriber with `ClientLogLayer`. Without this, `tracing::…!`
        // events from the test process are dropped and never reach the
        // server's log_store.
        let echo_addr = spawn_echo().await;
        let remote_port = harness.exposed_ports[0];
        harness.spawn_client(vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);

        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("register");

        // Open the SSE stream.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        let resp = http
            .get(format!("{}/api/logs/stream", harness.api_base))
            .send()
            .await
            .expect("sse connect");
        assert!(
            resp.status().is_success(),
            "SSE endpoint should return 2xx, got {}",
            resp.status()
        );

        let mut stream = resp.bytes_stream();

        // Emit >= 50 warn events from a background task so the client-side
        // log forwarder trips its "buffer full" fast-path (see
        // spawn_log_forwarder in src/client/logs.rs — 2s flush interval
        // otherwise). Keep pumping until we see the marker or hit the
        // deadline, in case the SSE subscribe happened after the first
        // flush cycle.
        let pump = tokio::spawn(async move {
            for round in 0..20 {
                for _ in 0..60 {
                    tracing::warn!("{SSE_MARKER}");
                }
                // Between rounds, briefly yield so the forwarder task can
                // drain and the SSE broadcaster can emit.
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = round;
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut buf = String::new();
        let mut found = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    buf.push_str(&String::from_utf8_lossy(&chunk));
                    if buf.contains(SSE_MARKER) {
                        found = true;
                        break;
                    }
                    // Keep buf bounded; we only need to spot the marker.
                    if buf.len() > 64 * 1024 {
                        let tail_start = buf.len().saturating_sub(4096);
                        buf = buf.split_off(tail_start);
                    }
                }
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue,
            }
        }
        pump.abort();
        assert!(
            found,
            "SSE stream should have relayed our warn marker (route /api/logs/stream)"
        );
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn traffic_bucket_appears_after_transfer() {
    let result = tokio::time::timeout(Duration::from_secs(20), async {
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
        wait_until("port open", || async {
            TcpStream::connect(("127.0.0.1", remote_port))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("port never opened");

        // Push 64 KiB through the tunnel and read it back.
        let mut sock = TcpStream::connect(("127.0.0.1", remote_port))
            .await
            .unwrap();
        let payload = vec![0xAAu8; 65_536];
        sock.write_all(&payload).await.unwrap();
        let mut recv = vec![0u8; payload.len()];
        sock.read_exact(&mut recv).await.unwrap();
        assert_eq!(recv, payload);
        drop(sock);

        // Verify the traffic store has recorded non-zero bytes for our
        // port. Actual field names on `PortTraffic` (src/server/api.rs:43-48)
        // are `total_bytes_in` / `total_bytes_out`.
        let bytes = wait_until("traffic recorded", || async {
            let (status, body) = api.get_json("/api/traffic").await;
            if !status.is_success() {
                return None;
            }
            let arr = body.as_array()?;
            for item in arr {
                let port = item.get("port").and_then(|v| v.as_u64())?;
                if port as u16 != remote_port {
                    continue;
                }
                let bin = item.get("total_bytes_in").and_then(|v| v.as_u64()).unwrap_or(0);
                let bout = item.get("total_bytes_out").and_then(|v| v.as_u64()).unwrap_or(0);
                if bin > 0 && bout > 0 {
                    return Some((bin, bout));
                }
            }
            None
        })
        .await
        .expect("no traffic bucket ever recorded for our port");

        // Sanity: both directions should reflect at least the payload size.
        // The store increments per-chunk copied by tokio::io::copy so we
        // shouldn't have lost bytes.
        assert!(
            bytes.0 as usize >= payload.len(),
            "total_bytes_in ({}) < payload ({})",
            bytes.0,
            payload.len()
        );
        assert!(
            bytes.1 as usize >= payload.len(),
            "total_bytes_out ({}) < payload ({})",
            bytes.1,
            payload.len()
        );
    })
    .await;
    result.expect("test timed out");
}
