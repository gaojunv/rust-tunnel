//! Integration tests: 客户端 tunnel 埋点写入 StatsCollector，
//! 并通过 `GET /api/stats/summary` 暴露（内存读取，无需等 60s flush）。
//!
//! 路由核对：
//!   * `GET /api/stats/summary` — src/server/api/mod.rs:2552 附近，
//!     返回 `StatsSummary`：`{ "clients": { "total_bytes_in", "total_bytes_out",
//!     "total_conns", "entity_count" }, "proxy": ..., ... }`。
//!   * v2 tunnel 数据路径：TcpProxy → ClientConnector → ClientRegistry::open_tunnel
//!     → ClientTunnelStream（src/server/tunnel_stream.rs）。

#[path = "common/mod.rs"]
mod common;

use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 从 /api/stats/summary 响应中提取 clients 桶的 (bytes_in, bytes_out, conns)。
fn parse_clients(body: &serde_json::Value) -> Option<(u64, u64, u64)> {
    let clients = body.get("clients")?;
    let bin = clients.get("total_bytes_in")?.as_u64()?;
    let bout = clients.get("total_bytes_out")?.as_u64()?;
    let conns = clients.get("total_conns")?.as_u64()?;
    Some((bin, bout, conns))
}

#[tokio::test(flavor = "multi_thread")]
async fn client_tunnel_stats_appear_in_summary() {
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness.exposed_ports[0];
        harness.spawn_client(Some("stats-client"));

        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("register");

        // Start TCP tunnel on server side so traffic can flow through.
        harness
            .start_tcp_tunnel(remote_port, &echo_addr.to_string(), "stats-client")
            .await;

        wait_until("port open", || async {
            TcpStream::connect(("127.0.0.1", remote_port))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("port never opened");

        // Hold a connection open: tunnel 打开应 incr_conns。
        let mut sock = TcpStream::connect(("127.0.0.1", remote_port))
            .await
            .unwrap();

        wait_until("tunnel conn counted", || async {
            let (status, body) = api.get_json("/api/stats/summary").await;
            if !status.is_success() {
                return None;
            }
            let (_, _, conns) = parse_clients(&body)?;
            if conns >= 1 {
                Some(())
            } else {
                None
            }
        })
        .await
        .expect("active tunnel conn never counted in stats summary");

        // Push 64 KiB through the tunnel and read the echo back.
        let payload = vec![0xAAu8; 65_536];
        sock.write_all(&payload).await.unwrap();
        let mut recv = vec![0u8; payload.len()];
        sock.read_exact(&mut recv).await.unwrap();
        assert_eq!(recv, payload);

        // summary 是内存读取，写入立刻可见。
        let (bin, bout, _) = wait_until("client bytes recorded", || async {
            let (status, body) = api.get_json("/api/stats/summary").await;
            if !status.is_success() {
                return None;
            }
            let (bin, bout, conns) = parse_clients(&body)?;
            if bin > 0 && bout > 0 {
                Some((bin, bout, conns))
            } else {
                None
            }
        })
        .await
        .expect("client bytes never appeared in /api/stats/summary");

        assert!(
            bin as usize >= payload.len(),
            "clients.total_bytes_in ({bin}) < payload ({})",
            payload.len()
        );
        assert!(
            bout as usize >= payload.len(),
            "clients.total_bytes_out ({bout}) < payload ({})",
            payload.len()
        );
    })
    .await;
    result.expect("test timed out");
}
