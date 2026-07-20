//! Integration tests: /api/logs SSE stream and the unified stats API.
//!
//! Route verification:
//!   * SSE: `/api/logs/stream` — public route; token auth only when password
//!     is enabled. Emits named `event: log` frames whose `data:` is a JSON
//!     object with `message`, `level`, `source`, etc.
//!   * Stats: `/api/stats/summary` returns the unified StatsSummary JSON;
//!     the removed legacy `/api/traffic` endpoint must 404.
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

use common::{HarnessOpts, TestHarness};
use futures_util::StreamExt;
use std::time::Duration;

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
        harness.spawn_client(Some("sse-client"));

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

/// The legacy `/api/traffic` endpoint was removed in the stats unification;
/// it must now 404, and its replacement `/api/stats/summary` must serve the
/// unified StatsSummary JSON object.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_traffic_endpoint_removed_stats_summary_serves() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let api = harness.api_client();

        let (status, _body) = api.get_json("/api/traffic").await;
        assert_eq!(
            status,
            reqwest::StatusCode::NOT_FOUND,
            "legacy /api/traffic should be gone, got {status}"
        );

        let (status, body) = api.get_json("/api/stats/summary").await;
        assert!(
            status.is_success(),
            "/api/stats/summary should return 2xx, got {status}"
        );
        assert!(
            body.is_object(),
            "/api/stats/summary must return a JSON object (StatsSummary), got: {body}"
        );
        for field in ["clients", "proxy", "shadowsocks", "trojan"] {
            assert!(
                body.get(field).is_some(),
                "/api/stats/summary missing field {field}, got: {body}"
            );
        }
    })
    .await;
    result.expect("test timed out");
}
