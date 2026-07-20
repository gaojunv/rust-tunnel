//! Integration tests: 反向代理（HTTP/TCP/UDP）埋点写入 StatsCollector，
//! 并通过 `GET /api/stats/summary` 暴露（内存读取，无需等 60s flush）。
//!
//! 路由核对：
//!   * `GET /api/stats/summary` — src/server/api/mod.rs:2551 附近，
//!     返回 `StatsSummary`：`{ "clients": ..., "proxy": { "total_bytes_in",
//!     "total_bytes_out", "total_conns", "entity_count" }, ... }`
//!     （字段名见 src/server/stats.rs 的 StatsSummary/EntitySummary）。
//!   * HTTP 反代路径：SharedListener → handle_proxy_request_unified
//!     （src/server/reverse_proxy/handler.rs）。
//!   * TCP/UDP 反代路径：TcpProxy/UdpProxy
//!     （src/server/reverse_proxy/tcp_proxy.rs）。

#[path = "common/mod.rs"]
mod common;

use common::echo::spawn_http_echo;
use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use rust_tunnel::server::reverse_proxy::tcp_proxy::{TcpProxy, UdpProxy};
use rust_tunnel::server::reverse_proxy::{
    Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route, RuleType,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 从 /api/stats/summary 响应中提取 proxy 桶的
/// (bytes_in, bytes_out, conns, entity_count)。
fn parse_proxy(body: &serde_json::Value) -> Option<(u64, u64, u64, u64)> {
    let proxy = body.get("proxy")?;
    let bin = proxy.get("total_bytes_in")?.as_u64()?;
    let bout = proxy.get("total_bytes_out")?.as_u64()?;
    let conns = proxy.get("total_conns")?.as_u64()?;
    let entities = proxy.get("entity_count")?.as_u64()?;
    Some((bin, bout, conns, entities))
}

/// 构造 direct-backend 的 Backend（HTTP 规则用）。
fn direct_backend(addr: &str) -> Backend {
    Backend {
        kind: BackendKind::Direct,
        addr: addr.to_string(),
        client_name: None,
        weight: 100,
        protocol: BackendProtocol::Http1,
        scheme: BackendScheme::Http,
    }
}

/// HTTP 反代：请求真实穿过 SharedListener → handle_proxy_request_unified，
/// 请求/响应 body 字节与连接事件应进入 StatsCollector 的 proxy 桶。
#[tokio::test(flavor = "multi_thread")]
async fn http_proxy_stats_appear_in_summary() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let listen_port = harness.exposed_ports[0];
        let listen_addr = format!("127.0.0.1:{listen_port}");

        let backend_addr = spawn_http_echo().await;

        // 直接往 proxy_state 插入一条 HTTP 规则并 reconcile 出共享监听器。
        let rule = ProxyRule {
            id: "stats-http-rule".into(),
            name: "stats-http".into(),
            rule_type: RuleType::Http,
            listen: listen_addr.clone(),
            domains: vec!["stats.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![direct_backend(&backend_addr.to_string())],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        harness
            .proxy_state
            .rules
            .lock()
            .await
            .insert(rule.id.clone(), rule);
        harness
            .proxy_state
            .reconcile_http_listener(&listen_addr)
            .await
            .expect("reconcile http listener");

        // 等监听器真正开始 accept。
        wait_until("http listener accepting", || async {
            TcpStream::connect(("127.0.0.1", listen_port))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("http listener never opened");

        // POST 穿过反代：3 字节请求体 → 5 字节 "hello" 响应体。
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{listen_port}/"))
            .header("host", "stats.local")
            .body("abc")
            .send()
            .await
            .expect("proxied request");
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.expect("response body");
        assert_eq!(text, "hello");

        let api = harness.api_client();
        let (bin, bout, _, entities) = wait_until("http proxy stats", || async {
            let (status, body) = api.get_json("/api/stats/summary").await;
            if !status.is_success() {
                return None;
            }
            let (bin, bout, conns, entities) = parse_proxy(&body)?;
            if bin > 0 && bout > 0 {
                Some((bin, bout, conns, entities))
            } else {
                None
            }
        })
        .await
        .expect("http proxy bytes never appeared in /api/stats/summary");

        assert!(bin >= 3, "proxy.total_bytes_in ({bin}) < request body (3)");
        assert!(
            bout >= 5,
            "proxy.total_bytes_out ({bout}) < response body (5)"
        );
        assert!(entities >= 1, "proxy.entity_count ({entities}) < 1");
    })
    .await;
    result.expect("test timed out");
}

/// TCP 反代：连接期间 total_conns >= 1；连接关闭后双向字节计入 proxy 桶。
#[tokio::test(flavor = "multi_thread")]
async fn tcp_proxy_stats_appear_in_summary() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let listen_port = harness.exposed_ports[0];
        let echo_addr = spawn_echo().await;

        let tcp_proxy = TcpProxy::new(harness.proxy_state.clone());
        let addr: std::net::SocketAddr = format!("127.0.0.1:{listen_port}").parse().unwrap();
        tcp_proxy
            .start(
                addr,
                direct_backend(&echo_addr.to_string()),
                "stats-tcp-rule".to_string(),
            )
            .await
            .expect("start tcp proxy");

        wait_until("tcp proxy port open", || async {
            TcpStream::connect(("127.0.0.1", listen_port))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("tcp proxy port never opened");

        let api = harness.api_client();

        // 持有一条连接：accept 循环应 incr_conns。
        let mut sock = TcpStream::connect(("127.0.0.1", listen_port))
            .await
            .unwrap();
        wait_until("tcp conn counted", || async {
            let (status, body) = api.get_json("/api/stats/summary").await;
            if !status.is_success() {
                return None;
            }
            let (_, _, conns, _) = parse_proxy(&body)?;
            if conns >= 1 {
                Some(())
            } else {
                None
            }
        })
        .await
        .expect("active tcp conn never counted in stats summary");

        // 64 KiB 穿过反代并读回 echo。
        let payload = vec![0xBBu8; 65_536];
        sock.write_all(&payload).await.unwrap();
        let mut recv = vec![0u8; payload.len()];
        sock.read_exact(&mut recv).await.unwrap();
        assert_eq!(recv, payload);

        // TCP 的字节在连接关闭（copy_bidirectional 结束）时一次性入账。
        drop(sock);

        let (bin, bout, _, _) = wait_until("tcp bytes recorded", || async {
            let (status, body) = api.get_json("/api/stats/summary").await;
            if !status.is_success() {
                return None;
            }
            let (bin, bout, conns, entities) = parse_proxy(&body)?;
            if bin > 0 && bout > 0 {
                Some((bin, bout, conns, entities))
            } else {
                None
            }
        })
        .await
        .expect("tcp proxy bytes never appeared in /api/stats/summary");

        assert!(
            bin as usize >= payload.len(),
            "proxy.total_bytes_in ({bin}) < payload ({})",
            payload.len()
        );
        assert!(
            bout as usize >= payload.len(),
            "proxy.total_bytes_out ({bout}) < payload ({})",
            payload.len()
        );

        // 连接关闭后 decr_conns 应把活跃连接降回 0（覆盖正常退出路径）。
        wait_until("tcp conn decremented", || async {
            let (status, body) = api.get_json("/api/stats/summary").await;
            if !status.is_success() {
                return None;
            }
            let (_, _, conns, _) = parse_proxy(&body)?;
            if conns == 0 {
                Some(())
            } else {
                None
            }
        })
        .await
        .expect("tcp conn count never returned to 0");
    })
    .await;
    result.expect("test timed out");
}

/// UDP 反代：一次请求/响应的字节与连接事件应进入 proxy 桶。
#[tokio::test(flavor = "multi_thread")]
async fn udp_proxy_stats_appear_in_summary() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let listen_port = harness.exposed_ports[0];

        // 简易 UDP echo 后端。
        let echo_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo_socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                match echo_socket.recv_from(&mut buf).await {
                    Ok((n, peer)) => {
                        if echo_socket.send_to(&buf[..n], peer).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        let udp_proxy = UdpProxy::new(harness.proxy_state.clone());
        let addr: std::net::SocketAddr = format!("127.0.0.1:{listen_port}").parse().unwrap();
        udp_proxy
            .start(
                addr,
                direct_backend(&echo_addr.to_string()),
                "stats-udp-rule".to_string(),
            )
            .await
            .expect("start udp proxy");

        // 发一个数据报并等 echo 回来。
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let payload = b"udp-stats-ping";
        client
            .send_to(payload, format!("127.0.0.1:{listen_port}"))
            .await
            .unwrap();
        let mut buf = vec![0u8; 1024];
        let (n, _) = tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buf))
            .await
            .expect("udp echo timed out")
            .unwrap();
        assert_eq!(&buf[..n], payload);

        let api = harness.api_client();
        let (bin, bout, _, entities) = wait_until("udp proxy stats", || async {
            let (status, body) = api.get_json("/api/stats/summary").await;
            if !status.is_success() {
                return None;
            }
            let (bin, bout, conns, entities) = parse_proxy(&body)?;
            if bin > 0 && bout > 0 {
                Some((bin, bout, conns, entities))
            } else {
                None
            }
        })
        .await
        .expect("udp proxy bytes never appeared in /api/stats/summary");

        assert!(
            bin as usize >= payload.len(),
            "proxy.total_bytes_in ({bin}) < datagram ({})",
            payload.len()
        );
        assert!(
            bout as usize >= payload.len(),
            "proxy.total_bytes_out ({bout}) < datagram ({})",
            payload.len()
        );
        assert!(entities >= 1, "proxy.entity_count ({entities}) < 1");
    })
    .await;
    result.expect("test timed out");
}
