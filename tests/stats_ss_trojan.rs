//! Integration tests: Shadowsocks / Trojan 连接埋点写入 StatsCollector，
//! 并通过 `GET /api/stats/summary` 暴露（内存读取，无需等 60s flush）。
//!
//! 路由核对：
//!   * `GET /api/stats/summary` — src/server/api/mod.rs:2551 附近，
//!     返回 `StatsSummary`：`{ "clients": ..., "shadowsocks": {...},
//!     "trojan": { "total_bytes_in", "total_bytes_out", "total_conns",
//!     "entity_count" } }`（字段名见 src/server/stats.rs）。
//!   * SS 转发路径：listener → handle_inbound_connection →
//!     proxy_ss_connection（src/server/proxy.rs）。
//!   * Trojan 转发路径：listener/shared_listener → handle_trojan_connection →
//!     proxy_trojan_connection（src/server/proxy.rs）。
//!
//! entity_id 约定：`ss:{port}` / `trojan:{port}`。

#[path = "common/mod.rs"]
mod common;

use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use rust_tunnel::common::{
    create_insecure_client_config, create_server_config, load_or_generate_cert,
};
use rust_tunnel::server::listener;
use rust_tunnel::server::trojan::sha224_hex;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 从 /api/stats/summary 响应中提取指定桶的
/// (bytes_in, bytes_out, conns, entity_count)。
fn parse_bucket(body: &serde_json::Value, bucket: &str) -> Option<(u64, u64, u64, u64)> {
    let b = body.get(bucket)?;
    let bin = b.get("total_bytes_in")?.as_u64()?;
    let bout = b.get("total_bytes_out")?.as_u64()?;
    let conns = b.get("total_conns")?.as_u64()?;
    let entities = b.get("entity_count")?.as_u64()?;
    Some((bin, bout, conns, entities))
}

/// 轮询 /api/stats/summary，直到指定桶的双向字节都 > 0。
async fn wait_bucket_bytes(
    api: &common::api_client::ApiClient,
    bucket: &str,
) -> (u64, u64, u64, u64) {
    wait_until("bucket bytes recorded", || async {
        let (status, body) = api.get_json("/api/stats/summary").await;
        if !status.is_success() {
            return None;
        }
        let (bin, bout, conns, entities) = parse_bucket(&body, bucket)?;
        if bin > 0 && bout > 0 {
            Some((bin, bout, conns, entities))
        } else {
            None
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{bucket} bytes never appeared in /api/stats/summary: {e}"))
}

/// 轮询 /api/stats/summary，直到指定桶的活跃连接数降为 0。
async fn wait_bucket_conns_zero(api: &common::api_client::ApiClient, bucket: &str) {
    wait_until("bucket conns back to 0", || async {
        let (status, body) = api.get_json("/api/stats/summary").await;
        if !status.is_success() {
            return None;
        }
        let (_, _, conns, _) = parse_bucket(&body, bucket)?;
        if conns == 0 {
            Some(())
        } else {
            None
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{bucket} conn count never returned to 0: {e}"));
}

/// 构造 Trojan 请求头（IPv4 目标，CONNECT 命令）。
fn build_trojan_header(password: &str, target_ip: Ipv4Addr, target_port: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(68);
    buf.extend_from_slice(sha224_hex(password).as_bytes());
    buf.extend_from_slice(b"\r\n");
    buf.push(0x01); // CMD = CONNECT
    buf.push(0x01); // ATYP = IPv4
    buf.extend_from_slice(&target_ip.octets());
    buf.extend_from_slice(&target_port.to_be_bytes());
    buf.extend_from_slice(b"\r\n");
    buf
}

/// SS：纯 Rust 客户端（shadowsocks crate 的 ProxyClientStream）真实穿过
/// SS 监听器访问 echo 后端，字节与连接事件应进入 shadowsocks 桶。
#[tokio::test(flavor = "multi_thread")]
async fn ss_stats_appear_in_summary() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        use shadowsocks::config::{ServerConfig, ServerType};
        use shadowsocks::context::Context;
        use shadowsocks::crypto::CipherKind;
        use shadowsocks::relay::socks5::Address;
        use shadowsocks::relay::tcprelay::proxy_stream::client::ProxyClientStream;

        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let ss_port = harness.exposed_ports[0];
        let password = "stats-ss-password";
        let echo_addr = spawn_echo().await;

        // 用 harness 的 ServerState 起 SS 监听器（共享同一 StatsCollector）。
        let state = harness.server_state.clone();
        tokio::spawn(async move {
            let _ = listener::start_shadowsocks_listener(
                state,
                ss_port,
                "aes-256-gcm".into(),
                password.into(),
            )
            .await;
        });

        // 等监听器开始 accept（裸 TCP 连接会握手失败，仅用于探活）。
        wait_until("ss listener accepting", || async {
            TcpStream::connect(("127.0.0.1", ss_port))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("ss listener never opened");

        // 纯 Rust SS 客户端：连 SS 端口，经 SS 协议访问 echo 后端。
        let svr_cfg = ServerConfig::new(
            format!("127.0.0.1:{ss_port}")
                .parse::<SocketAddr>()
                .unwrap(),
            password,
            CipherKind::AES_256_GCM,
        )
        .expect("ss server config");
        let context = Context::new_shared(ServerType::Local);
        let payload = vec![0xAAu8; 4096];
        let mut stream = ProxyClientStream::connect(context, &svr_cfg, Address::from(echo_addr))
            .await
            .expect("ss client connect");
        stream.write_all(&payload).await.expect("ss write");
        let mut recv = vec![0u8; payload.len()];
        stream.read_exact(&mut recv).await.expect("ss read echo");
        assert_eq!(recv, payload);

        // 字节在连接关闭（copy 结束）时一次性入账。
        drop(stream);

        let api = harness.api_client();
        let (bin, bout, _, entities) = wait_bucket_bytes(&api, "shadowsocks").await;
        assert!(
            bin as usize >= payload.len(),
            "shadowsocks.total_bytes_in ({bin}) < payload ({})",
            payload.len()
        );
        assert!(
            bout as usize >= payload.len(),
            "shadowsocks.total_bytes_out ({bout}) < payload ({})",
            payload.len()
        );
        assert!(entities >= 1, "shadowsocks.entity_count ({entities}) < 1");

        // 连接关闭后 decr_conns 应把活跃连接降回 0（覆盖正常退出路径）。
        wait_bucket_conns_zero(&api, "shadowsocks").await;
    })
    .await;
    result.expect("test timed out");
}

/// Trojan：纯 Rust 客户端（tokio-rustls + 手写 Trojan 头）真实穿过
/// Trojan 监听器访问 echo 后端，字节与连接事件应进入 trojan 桶。
#[tokio::test(flavor = "multi_thread")]
async fn trojan_stats_appear_in_summary() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let trojan_port = harness.exposed_ports[0];
        let password = "stats-trojan-password";
        let echo_addr = spawn_echo().await;

        // 自签名证书 + watch channel（与 trojan_test.rs 同一模式）。
        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let cert_path = tmp_dir.path().join("cert.pem");
        let key_path = tmp_dir.path().join("key.pem");
        let cert_pair =
            load_or_generate_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap())
                .expect("generate cert");
        let server_config = create_server_config(cert_pair).expect("server config");
        let (_tx, tls_config_rx) = tokio::sync::watch::channel(server_config);

        // 用 harness 的 ServerState 起 Trojan 监听器（共享同一 StatsCollector）。
        let state = harness.server_state.clone();
        tokio::spawn(async move {
            let _ = listener::start_trojan_listener(
                state,
                trojan_port,
                password.into(),
                "127.0.0.1:1".into(),
                tls_config_rx,
            )
            .await;
        });

        // 等监听器开始 accept（裸 TCP 连接会 TLS 握手失败，仅用于探活）。
        wait_until("trojan listener accepting", || async {
            TcpStream::connect(("127.0.0.1", trojan_port))
                .await
                .ok()
                .map(|_| ())
        })
        .await
        .expect("trojan listener never opened");

        // TLS + Trojan 握手（insecure 客户端配置，接受自签名证书）。
        let client_config = create_insecure_client_config().expect("client config");
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let tcp = TcpStream::connect(("127.0.0.1", trojan_port))
            .await
            .expect("connect trojan");
        let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
        let mut tls_stream = connector
            .connect(server_name, tcp)
            .await
            .expect("tls handshake");

        let echo_ip = match echo_addr.ip() {
            std::net::IpAddr::V4(v4) => v4,
            _ => panic!("echo addr should be IPv4"),
        };
        let header = build_trojan_header(password, echo_ip, echo_addr.port());
        tls_stream.write_all(&header).await.expect("write header");

        let api = harness.api_client();

        // 等服务端完成握手并计入活跃连接，再发 payload——
        // 确保 payload 走 copy_bidirectional 而不是 initial_payload
        // （initial_payload 不计入 bytes_in，与旧 traffic_store 语义一致）。
        wait_until("trojan conn counted", || async {
            let (status, body) = api.get_json("/api/stats/summary").await;
            if !status.is_success() {
                return None;
            }
            let (_, _, conns, _) = parse_bucket(&body, "trojan")?;
            if conns >= 1 {
                Some(())
            } else {
                None
            }
        })
        .await
        .expect("active trojan conn never counted in stats summary");

        let payload = vec![0xBBu8; 4096];
        tls_stream.write_all(&payload).await.expect("write payload");
        let mut recv = vec![0u8; payload.len()];
        tls_stream.read_exact(&mut recv).await.expect("read echo");
        assert_eq!(recv, payload);

        // 字节在连接关闭（copy 结束）时一次性入账。
        // 优雅关闭：shutdown 发送 close_notify 并读到 EOF，
        // 服务端 copy_bidirectional 才能以 Ok 结束并入账（与 trojan_test.rs 同一模式）。
        tls_stream.shutdown().await.ok();
        let mut drain = [0u8; 64];
        let _ = tls_stream.read(&mut drain).await;
        drop(tls_stream);

        let (bin, bout, _, entities) = wait_bucket_bytes(&api, "trojan").await;
        assert!(
            bin as usize >= payload.len(),
            "trojan.total_bytes_in ({bin}) < payload ({})",
            payload.len()
        );
        assert!(
            bout as usize >= payload.len(),
            "trojan.total_bytes_out ({bout}) < payload ({})",
            payload.len()
        );
        assert!(entities >= 1, "trojan.entity_count ({entities}) < 1");

        // 连接关闭后 decr_conns 应把活跃连接降回 0（覆盖正常退出路径）。
        wait_bucket_conns_zero(&api, "trojan").await;
    })
    .await;
    result.expect("test timed out");
}
