//! 集成测试：内置 RustDesk 兼容服务（hbbs 会合 + hbbr 中继）的黑盒 e2e。
//!
//! 目标是证明**装配后的整条链路可用**：配置（`POST /api/rustdesk`）→ 运行时
//! （`rustdesk_runtime::apply_rustdesk_config` 绑定监听）→ 真实套接字 → 管理 API。
//! 协议细节（字段号、头宽、AddrMangle 位布局）已由 `rust-tunnel-rustdesk` 的
//! 87 条单测覆盖，这里不重复验证，只调用库的 `frame` / `proto` / `addr` 收发线上
//! 字节，驱动服务端**自己绑定**的真实端口。
//!
//! # 端口分配
//!
//! 会合端口 `P` 需要 TCP + UDP 同时空闲，且 NAT 探测端口 `P - 1` 也空闲——后者无法
//! 靠 `bind(0)` 由内核保证，只能主动探测。见 [`alloc_rustdesk_ports`]。

#[path = "common/mod.rs"]
mod common;

use bytes::BytesMut;
use common::api_client::ApiClient;
use common::harness::{HarnessOpts, TestHarness};
use common::wait_until;
use reqwest::StatusCode;
use rust_tunnel_rustdesk::addr;
use rust_tunnel_rustdesk::frame;
use rust_tunnel_rustdesk::key::ServerKey;
use rust_tunnel_rustdesk::proto::{
    punch_hole_response, register_pk_response, rendezvous_message, LocalAddr, NatType,
    ProstMessage, PunchHoleRequest, PunchHoleSent, RegisterPeer, RegisterPk, RendezvousMessage,
    RequestRelay,
};
use std::collections::HashSet;
use std::net::{TcpListener as StdTcpListener, UdpSocket as StdUdpSocket};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// RustDesk 管理 API 路由。
const RUSTDESK_PATH: &str = "/api/rustdesk";

/// 一次测试独占的 RustDesk 端口三元组（会合 / NAT 探测由 -1 推导 / 中继）。
struct RustdeskPorts {
    rendezvous: u16,
    relay: u16,
}

impl RustdeskPorts {
    /// NAT 探测端口：由会合端口推导（-1），且必须与会合端口一起空闲。
    fn nat(&self) -> u16 {
        self.rendezvous - 1
    }
}

/// 本测试二进制内已发放的端口集合（含会合、NAT、中继），保证并发用例永不重号。
fn used_ports() -> &'static Mutex<HashSet<u16>> {
    static USED: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    USED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// xorshift64：不引入 rand 依赖，只要一个可随机化的起点。
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// 由「纳秒时间戳 ^ 进程号」构造随机种子：不同测试进程起点不同，减小跨进程撞段概率。
fn seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id())).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x1234_5678_9ABC_DEF0
}

/// 分配「会合端口 P + NAT 端口 P-1 + 中继端口 Q」三元组。
///
/// 步骤：
/// 1. 在 `20000..32000` 取候选 —— 该区间低于 macOS（49152）与 Linux（32768）的临时
///    端口下界，避开其他 e2e 用例由 `bind(0)` 拿到的临时端口；
/// 2. **同时**绑定五个探测套接字（TCP `P`、UDP `P`、TCP `P-1`、UDP `P-1`、TCP `Q`）
///    确认全空闲，再一起释放，把「探测→真正 bind」的 TOCTOU 窗口压到最小。
///    NAT 探测端口现在 TCP 与 UDP 两个半边都要绑，少探一个就等于放弃「全空闲」的保证，
///    真实 bind 会偶发失败；
/// 3. 进程内全局集合记录已发放端口，保证同二进制内并发用例不重号。
///
/// 服务端绑定的是 `0.0.0.0:<port>`，故探测也用 `0.0.0.0` 对齐。
fn alloc_rustdesk_ports() -> RustdeskPorts {
    let mut used = used_ports().lock().expect("端口集合锁");
    let mut state = seed();
    for _ in 0..20_000 {
        let rendezvous = u16::try_from(20_000 + next_rand(&mut state) % 12_000).expect("取模范围");
        let relay = u16::try_from(20_000 + next_rand(&mut state) % 12_000).expect("取模范围");
        if rendezvous < 3 {
            continue;
        }
        let nat = rendezvous - 1;
        if relay == rendezvous || relay == nat {
            continue;
        }
        if used.contains(&rendezvous) || used.contains(&nat) || used.contains(&relay) {
            continue;
        }
        let probes = (
            StdTcpListener::bind(("0.0.0.0", rendezvous)),
            StdUdpSocket::bind(("0.0.0.0", rendezvous)),
            StdTcpListener::bind(("0.0.0.0", nat)),
            StdUdpSocket::bind(("0.0.0.0", nat)),
            StdTcpListener::bind(("0.0.0.0", relay)),
        );
        // 五个探测同时持有；任一失败即放弃这组候选。
        if let (Ok(t_rp), Ok(u_rp), Ok(t_nat), Ok(u_nat), Ok(t_relay)) = probes {
            drop((t_rp, u_rp, t_nat, u_nat, t_relay));
            used.insert(rendezvous);
            used.insert(nat);
            used.insert(relay);
            return RustdeskPorts { rendezvous, relay };
        }
    }
    panic!("在 20000..32000 内找不到空闲的 RustDesk 端口三元组");
}

/// `POST /api/rustdesk` 开启服务；断言 200 并返回响应体。
async fn enable(
    api: &ApiClient,
    ports: &RustdeskPorts,
    key: Option<&str>,
    generate_key: bool,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "enabled": true,
        "rendezvous_port": ports.rendezvous,
        "relay_port": ports.relay,
    });
    if let Some(k) = key {
        body["key"] = serde_json::Value::String(k.to_owned());
    }
    if generate_key {
        body["generate_key"] = serde_json::Value::Bool(true);
    }
    let (status, resp) = api.post_json(RUSTDESK_PATH, body).await;
    assert!(status.is_success(), "开启 RustDesk 应成功，实得 {status}: {resp}");
    resp
}

/// `POST /api/rustdesk` 停用服务。
async fn disable(api: &ApiClient, ports: &RustdeskPorts) {
    let (status, resp) = api
        .post_json(
            RUSTDESK_PATH,
            serde_json::json!({
                "enabled": false,
                "rendezvous_port": ports.rendezvous,
                "relay_port": ports.relay,
            }),
        )
        .await;
    assert!(status.is_success(), "停用 RustDesk 应成功，实得 {status}: {resp}");
}

/// 原始 `GET /api/rustdesk`：返回状态码与**未解析的响应体文本**。
///
/// 密钥泄漏断言必须打在原始 JSON 上，避免「反序列化后字段被丢弃」掩盖问题。
async fn get_raw(base: &str) -> (StatusCode, String) {
    let resp = reqwest::Client::new()
        .get(format!("{base}{RUSTDESK_PATH}"))
        .send()
        .await
        .expect("GET /api/rustdesk 发送");
    let status = resp.status();
    let body = resp.text().await.expect("GET /api/rustdesk 读体");
    (status, body)
}

/// 把原始响应体解析为 JSON。
fn parse_json(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|e| panic!("GET /api/rustdesk 应为 JSON: {e}: {raw}"))
}

/// 原始 `POST /api/rustdesk`：返回状态码与**未解析的响应体文本**。
///
/// 用于断言 400/500 边界——错误体是纯文本（`(StatusCode, String)`），
/// 经 `ApiClient::post_json` 的 `resp.json()` 会被吞成 `Null`，看不见错误文案。
async fn post_raw(api: &ApiClient, body: serde_json::Value) -> (StatusCode, String) {
    let resp = reqwest::Client::new()
        .post(format!("{}{RUSTDESK_PATH}", api.base()))
        .json(&body)
        .send()
        .await
        .expect("POST /api/rustdesk 发送");
    let status = resp.status();
    let body = resp.text().await.expect("POST /api/rustdesk 读体");
    (status, body)
}

/// 断言 `127.0.0.1:<port>` 可以建立 TCP 连接（监听已就绪）。
async fn assert_tcp_reachable(port: u16) {
    tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(("127.0.0.1", port)))
        .await
        .unwrap_or_else(|_| panic!("连接 127.0.0.1:{port} 超时"))
        .unwrap_or_else(|e| panic!("应能连接 127.0.0.1:{port}: {e}"));
}

/// 探测 `127.0.0.1:<port>` 是否可连（用于「停用后拒连」的轮询断言）。
async fn tcp_open(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(300),
            TcpStream::connect(("127.0.0.1", port))
        )
        .await,
        Ok(Ok(_))
    )
}

/// 从 TCP 读一条长度前缀控制消息（跨多次读重组）。
///
/// 用库的 `frame::decode_header` 解析头部，不在此重实现线上格式。
async fn read_message(stream: &mut TcpStream, buf: &mut BytesMut) -> RendezvousMessage {
    let read = async {
        // decode_header 会把头部字节消耗掉并返回载荷长度；必须把长度记下来，
        // 否则下一轮会把载荷当成头部再解析一次。
        let mut pending: Option<usize> = None;
        loop {
            if pending.is_none() {
                pending = frame::decode_header(buf).expect("帧头解码");
            }
            if let Some(len) = pending {
                if buf.len() >= len {
                    let payload = buf.split_to(len);
                    return RendezvousMessage::decode(&payload[..]).expect("protobuf 解码");
                }
            }
            fill(stream, buf).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("等待控制消息超时")
}

/// 读更多字节；对端在完整消息到达前关闭即 panic。
async fn fill(stream: &mut TcpStream, buf: &mut BytesMut) {
    if stream.read_buf(buf).await.expect("TCP 读") == 0 {
        panic!("连接在收到完整控制消息前被关闭");
    }
}

/// 按长度前缀帧写出一条控制消息。
async fn send_message(stream: &mut TcpStream, msg: &RendezvousMessage) {
    let framed = frame::frame(&msg.encode_to_vec()).expect("帧编码");
    stream.write_all(&framed).await.expect("TCP 写");
}

/// 向会合端口的 UDP 侧发一条裸 protobuf 数据报。
async fn udp_send(sock: &UdpSocket, msg: &RendezvousMessage, port: u16) {
    sock.send_to(&msg.encode_to_vec(), ("127.0.0.1", port))
        .await
        .expect("UDP 发送");
}

/// 收一条 UDP 数据报并解码（UDP 自带边界，无帧前缀）。
async fn udp_recv(sock: &UdpSocket) -> RendezvousMessage {
    let mut buf = vec![0u8; 64 * 1024];
    let (len, _) = tokio::time::timeout(Duration::from_secs(5), sock.recv_from(&mut buf))
        .await
        .expect("等 UDP 应答超时")
        .expect("UDP 接收");
    RendezvousMessage::decode(&buf[..len]).expect("UDP protobuf 解码")
}

/// 让一个 ID 走完「心跳 → 登记公钥」，使其在会合表里在线且公钥可被签名。
async fn register_online(sock: &UdpSocket, port: u16, id: &str, pk: &[u8]) {
    udp_send(
        sock,
        &RendezvousMessage::with(rendezvous_message::Union::RegisterPeer(RegisterPeer {
            id: id.to_owned(),
            serial: 0,
        })),
        port,
    )
    .await;
    // 首次心跳的应答（request_pk）内容不关心，收掉即可。
    let _ = udp_recv(sock).await;

    udp_send(
        sock,
        &RendezvousMessage::with(rendezvous_message::Union::RegisterPk(RegisterPk {
            id: id.to_owned(),
            uuid: b"test-uuid-0001".to_vec(),
            pk: pk.to_vec(),
            ..Default::default()
        })),
        port,
    )
    .await;
    match udp_recv(sock).await.union {
        Some(rendezvous_message::Union::RegisterPkResponse(r)) => assert_eq!(
            r.result,
            register_pk_response::Result::Ok as i32,
            "登记公钥应成功"
        ),
        other => panic!("期望 RegisterPkResponse，实得 {other:?}"),
    }
}

/// 构造一帧 `RequestRelay`（含帧前缀）并追加应用字节，返回**一次 write** 的完整载荷。
fn request_relay_with_leftover(id: &str, uuid: &str, key: &str, leftover: &[u8]) -> Vec<u8> {
    let msg = RendezvousMessage::with(rendezvous_message::Union::RequestRelay(RequestRelay {
        id: id.to_owned(),
        uuid: uuid.to_owned(),
        licence_key: key.to_owned(),
        ..Default::default()
    }));
    let mut wire = frame::frame(&msg.encode_to_vec())
        .expect("帧编码")
        .to_vec();
    wire.extend_from_slice(leftover);
    wire
}

#[tokio::test]
async fn api_enable_round_trip_reflects_state_and_never_leaks_key() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    let key = ServerKey::generate();
    let secret = key.to_secret_base64().expect("生成的密钥必有私钥");
    let public = key.key().to_owned();

    let _ = enable(&harness.api_client(), &ports, Some(&secret), false).await;

    let (status, raw) = get_raw(&harness.api_base).await;
    assert!(status.is_success(), "GET 应成功，实得 {status}: {raw}");
    let v = parse_json(&raw);
    assert_eq!(v["enabled"], serde_json::json!(true));
    assert_eq!(v["running"], serde_json::json!(true));
    assert_eq!(v["key_set"], serde_json::json!(true));
    assert_eq!(v["rendezvous_port"], serde_json::json!(ports.rendezvous));
    assert_eq!(v["relay_port"], serde_json::json!(ports.relay));
    assert_eq!(
        v["nat_test_port"],
        serde_json::json!(ports.nat()),
        "NAT 探测端口必须是会合端口 - 1"
    );

    // 密钥串（私钥或公钥）绝不能出现在原始响应体里，且不应有 key 字段。
    assert!(!raw.contains(&secret), "GET 响应体泄漏了私钥: {raw}");
    assert!(!raw.contains(&public), "GET 响应体泄漏了公钥: {raw}");
    assert!(!raw.contains("\"key\":"), "GET 响应体出现了 key 字段: {raw}");

    // 配置 → 运行时 → 真实套接字：三个端口都应在监听。
    assert_tcp_reachable(ports.rendezvous).await;
    assert_tcp_reachable(ports.nat()).await;
    assert_tcp_reachable(ports.relay).await;
}

#[tokio::test]
async fn generate_key_returns_public_once_and_persists_a_secret() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();

    let resp = enable(&harness.api_client(), &ports, None, true).await;
    let returned = resp["key"]
        .as_str()
        .unwrap_or_else(|| panic!("generate_key 响应应带 key: {resp}"))
        .to_owned();
    assert!(!returned.is_empty(), "生成的公钥不应为空");

    let (status, raw) = get_raw(&harness.api_base).await;
    assert!(status.is_success(), "GET 应成功，实得 {status}: {raw}");
    let v = parse_json(&raw);
    assert_eq!(
        v["key_set"],
        serde_json::json!(true),
        "生成的密钥应已持久化为可签名的形态"
    );
    assert!(!raw.contains(&returned), "GET 不应回传公钥: {raw}");
    assert!(!raw.contains("\"key\":"), "GET 响应体出现了 key 字段: {raw}");
}

#[tokio::test]
async fn udp_heartbeat_requests_pk_for_an_unknown_id() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    let _ = enable(&harness.api_client(), &ports, None, false).await;

    let sock = UdpSocket::bind("127.0.0.1:0").await.expect("绑定 UDP");
    udp_send(
        &sock,
        &RendezvousMessage::with(rendezvous_message::Union::RegisterPeer(RegisterPeer {
            id: "hb-unknown-0001".to_owned(),
            serial: 0,
        })),
        ports.rendezvous,
    )
    .await;

    match udp_recv(&sock).await.union {
        Some(rendezvous_message::Union::RegisterPeerResponse(r)) => {
            assert!(r.request_pk, "未知 ID 的心跳应要求上报公钥");
        }
        other => panic!("期望 RegisterPeerResponse，实得 {other:?}"),
    }
}

#[tokio::test]
async fn udp_register_pk_is_accepted() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    let _ = enable(&harness.api_client(), &ports, None, false).await;

    let sock = UdpSocket::bind("127.0.0.1:0").await.expect("绑定 UDP");
    // 先心跳（未知 ID 会被要求上报公钥），再登记公钥。
    udp_send(
        &sock,
        &RendezvousMessage::with(rendezvous_message::Union::RegisterPeer(RegisterPeer {
            id: "pk-peer-0001".to_owned(),
            serial: 0,
        })),
        ports.rendezvous,
    )
    .await;
    let _ = udp_recv(&sock).await;

    udp_send(
        &sock,
        &RendezvousMessage::with(rendezvous_message::Union::RegisterPk(RegisterPk {
            id: "pk-peer-0001".to_owned(),
            uuid: b"uuid-pk-0001".to_vec(),
            pk: b"peer-public-key-bytes".to_vec(),
            ..Default::default()
        })),
        ports.rendezvous,
    )
    .await;

    match udp_recv(&sock).await.union {
        Some(rendezvous_message::Union::RegisterPkResponse(r)) => {
            assert_eq!(r.result, register_pk_response::Result::Ok as i32);
        }
        other => panic!("期望 RegisterPkResponse，实得 {other:?}"),
    }
}

/// 端到端打洞：B 心跳 + 上报公钥 → A 发起 `PunchHoleRequest` → 服务端推送给 B →
/// B 应答 → A 在**同一条 TCP 连接**上收到带签名的 `PunchHoleResponse`，并独立验签。
#[tokio::test]
async fn punch_hole_response_is_signed_end_to_end() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    // 用自己造的密钥对，把**私钥**交给服务端：服务端才能签 IdPk，测试端也能独立验签。
    let key = ServerKey::generate();
    let secret = key.to_secret_base64().expect("必有私钥");
    let public = key.key().to_owned();
    let _ = enable(&harness.api_client(), &ports, Some(&secret), false).await;

    const PEER: &str = "peer-b-0001";
    let peer_pk = b"peer-b-public-key".to_vec();
    let peer_udp = UdpSocket::bind("127.0.0.1:0").await.expect("绑定 B 的 UDP");
    register_online(&peer_udp, ports.rendezvous, PEER, &peer_pk).await;

    // A：TCP 信令连接，带上服务端公钥（licence_key）。
    let mut requester = TcpStream::connect(("127.0.0.1", ports.rendezvous))
        .await
        .expect("A 连接信令端口");
    send_message(
        &mut requester,
        &RendezvousMessage::with(rendezvous_message::Union::PunchHoleRequest(
            PunchHoleRequest {
                id: PEER.to_owned(),
                licence_key: public.clone(),
                version: "1.2.2".to_owned(),
                ..Default::default()
            },
        )),
    )
    .await;

    // B：收到服务端推送。回环两端同 IP → 走「同内网」分支发 FetchLocalAddr；
    // 否则发 PunchHole。两种都处理，让断言不依赖内网判定。
    let pushed = udp_recv(&peer_udp).await;
    let mut responder = TcpStream::connect(("127.0.0.1", ports.rendezvous))
        .await
        .expect("B 连接信令端口");
    match pushed.union {
        Some(rendezvous_message::Union::PunchHole(ph)) => {
            send_message(
                &mut responder,
                &RendezvousMessage::with(rendezvous_message::Union::PunchHoleSent(
                    PunchHoleSent {
                        socket_addr: ph.socket_addr,
                        id: PEER.to_owned(),
                        nat_type: NatType::Asymmetric.into(),
                        version: "1.2.2".to_owned(),
                        ..Default::default()
                    },
                )),
            )
            .await;
        }
        Some(rendezvous_message::Union::FetchLocalAddr(fla)) => {
            send_message(
                &mut responder,
                &RendezvousMessage::with(rendezvous_message::Union::LocalAddr(LocalAddr {
                    socket_addr: fla.socket_addr,
                    local_addr: addr::encode("127.0.0.1:21118".parse().expect("地址")),
                    id: PEER.to_owned(),
                    version: "1.2.2".to_owned(),
                    ..Default::default()
                })),
            )
            .await;
        }
        other => panic!("B 应收到 PunchHole 或 FetchLocalAddr，实得 {other:?}"),
    }

    // A：同一条连接上收到带签名的 PunchHoleResponse。
    let mut buf = BytesMut::new();
    let msg = read_message(&mut requester, &mut buf).await;
    let Some(rendezvous_message::Union::PunchHoleResponse(resp)) = msg.union else {
        panic!("A 应收到 PunchHoleResponse，实得 {msg:?}");
    };
    assert_eq!(resp.failure, 0, "不应带失败原因");
    assert!(!resp.pk.is_empty(), "PunchHoleResponse 应带对端公钥签名");
    assert!(
        addr::decode(&resp.socket_addr).is_some(),
        "应答应带可解码的对端地址"
    );
    // 独立验签：签名输入是 IdPk{id, pk} 的 protobuf 编码（见 sign_pk / sign_id_pk）。
    ServerKey::parse(&secret)
        .expect("密钥应可解析")
        .verify_id_pk(PEER, &peer_pk, &resp.pk)
        .expect("PunchHoleResponse 的签名应可被服务端密钥验证");
}

/// 中继数据面：两条 `RequestRelay` 连接按 uuid 配对，双向裸字节转发；
/// 并显式覆盖「控制帧与首批应用字节放在同一次 write」的经典帧后残留场景。
#[tokio::test]
async fn relay_forwards_bytes_including_leftover_after_handshake() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    let key = ServerKey::generate();
    let secret = key.to_secret_base64().expect("必有私钥");
    let public = key.key().to_owned();
    let _ = enable(&harness.api_client(), &ports, Some(&secret), false).await;

    let uuid = "pair-leftover-0001";
    let mut first = TcpStream::connect(("127.0.0.1", ports.relay))
        .await
        .expect("连接中继");
    let mut second = TcpStream::connect(("127.0.0.1", ports.relay))
        .await
        .expect("连接中继");

    // 关键：控制帧与首批应用字节同一次 write（帧后残留）。
    first
        .write_all(&request_relay_with_leftover("c1", uuid, &public, b"EARLY"))
        .await
        .expect("写 c1 握手+残留字节");
    // 等 c1 进入等待表，避免两条连接在服务端并发 remove 同一 uuid 的竞态。
    // 与本文件的其余断言无关：无论谁先到，每一端收到的都是对端附带的残留字节。
    tokio::time::sleep(Duration::from_millis(150)).await;
    second
        .write_all(&request_relay_with_leftover("c2", uuid, &public, b"LATE"))
        .await
        .expect("写 c2 握手+残留字节");

    // 无论配对次序，每端都应收到**对端**的残留字节。
    let mut got = [0u8; 4];
    first.read_exact(&mut got).await.expect("c1 读 LATE");
    assert_eq!(&got, b"LATE", "c1 应收到 c2 的残留字节");
    let mut got = [0u8; 5];
    second.read_exact(&mut got).await.expect("c2 读 EARLY");
    assert_eq!(&got, b"EARLY", "c2 应收到 c1 的残留字节");

    // 配对成立后是无头部的裸字节双向通道。
    first.write_all(b"ping").await.expect("写 ping");
    let mut got = [0u8; 4];
    second.read_exact(&mut got).await.expect("c2 读 ping");
    assert_eq!(&got, b"ping");
    second.write_all(b"pong").await.expect("写 pong");
    let mut got = [0u8; 4];
    first.read_exact(&mut got).await.expect("c1 读 pong");
    assert_eq!(&got, b"pong");
}

/// `licence_key` 不匹配的打洞请求应被拒，并回 `Failure::LicenseMismatch`。
#[tokio::test]
async fn wrong_licence_key_is_rejected() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    let key = ServerKey::generate();
    let secret = key.to_secret_base64().expect("必有私钥");
    let _ = enable(&harness.api_client(), &ports, Some(&secret), false).await;

    let mut conn = TcpStream::connect(("127.0.0.1", ports.rendezvous))
        .await
        .expect("连接信令端口");
    send_message(
        &mut conn,
        &RendezvousMessage::with(rendezvous_message::Union::PunchHoleRequest(
            PunchHoleRequest {
                id: "any-target-id".to_owned(),
                licence_key: "definitely-not-the-key".to_owned(),
                version: "1.2.2".to_owned(),
                ..Default::default()
            },
        )),
    )
    .await;

    let mut buf = BytesMut::new();
    match read_message(&mut conn, &mut buf).await.union {
        Some(rendezvous_message::Union::PunchHoleResponse(r)) => {
            assert_eq!(
                r.failure,
                punch_hole_response::Failure::LicenseMismatch as i32,
                "错误 key 应回 LicenseMismatch"
            );
        }
        other => panic!("期望 LicenseMismatch 的 PunchHoleResponse，实得 {other:?}"),
    }
}

/// 停用后，会合/NAT 探测/中继三个端口都不再接受连接。
#[tokio::test]
async fn disabling_closes_all_three_ports() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    let _ = enable(&harness.api_client(), &ports, None, false).await;
    assert_tcp_reachable(ports.rendezvous).await;
    assert_tcp_reachable(ports.nat()).await;
    assert_tcp_reachable(ports.relay).await;

    disable(&harness.api_client(), &ports).await;

    // 停用是异步的（abort → 任务退出 → 释放监听），轮询等到三个端口全部拒连。
    let (rendezvous, nat, relay) = (ports.rendezvous, ports.nat(), ports.relay);
    wait_until("停用后三个端口全部拒连", || async move {
        (!tcp_open(rendezvous).await && !tcp_open(nat).await && !tcp_open(relay).await).then_some(())
    })
    .await
    .expect("停用后端口应在超时内全部拒连");

    // NAT 探测的 **UDP 半边**是新增的第 4 个监听套接字，TCP 侧拒连证明不了它已释放。
    // 泄漏的后果很具体：下一次启用同端口会在 `bind_listeners` 里 EADDRINUSE——正是本文件
    // 「改端口重启不撞旧端口」那条保证要防的事。所以这里真去绑一次，不能只看 TCP。
    wait_until("停用后 NAT 探测的 UDP 半边可重新绑定", || async move {
        StdUdpSocket::bind(("0.0.0.0", nat)).map(|_| ()).ok()
    })
    .await
    .expect("停用后 NAT 探测 UDP 端口应可重新绑定");

    let (_, raw) = get_raw(&harness.api_base).await;
    let v = parse_json(&raw);
    assert_eq!(v["enabled"], serde_json::json!(false));
    assert_eq!(v["running"], serde_json::json!(false));
}

/// 校验必拒的输入一律回 400，且错误文案能区分「字段本身不对」与「字段间打架」。
///
/// 覆盖配置层/管理 API 收敛后的五条规则：会合端口下界、两端口中继相同、中继撞 NAT
/// 探测端口（本轮新增）、`mask` 非法、`relay_server` 非法。
#[tokio::test]
async fn invalid_rustdesk_fields_are_rejected_with_400() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let api = harness.api_client();

    // 会合端口 < 2：NAT 探测端口（= 会合 - 1）会落到 0/1，无法绑定，字段级粗筛拦下。
    let (status, body) = post_raw(
        &api,
        serde_json::json!({ "enabled": true, "rendezvous_port": 1, "relay_port": 21001 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "会合端口 <2 应 400，实得 {body}");
    assert!(body.contains("rendezvous_port"), "错误文案应点名该字段: {body}");

    // 会合端口 == 中继端口：两个服务互抢同一端口。
    let (status, body) = post_raw(
        &api,
        serde_json::json!({ "enabled": true, "rendezvous_port": 21000, "relay_port": 21000 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "两端口相同应 400，实得 {body}");
    assert!(body.contains("must differ"), "错误文案应说明端口须不同: {body}");

    // 中继端口 == 会合端口 - 1：撞上会合服务自建的 NAT 探测端口。
    let (status, body) = post_raw(
        &api,
        serde_json::json!({ "enabled": true, "rendezvous_port": 21000, "relay_port": 20999 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "中继端口撞 NAT 探测端口应 400，实得 {body}"
    );
    assert!(body.contains("NAT-test port"), "错误文案应点名 NAT 探测端口: {body}");

    // mask 非法：不是 IPv4 CIDR。
    let (status, body) = post_raw(
        &api,
        serde_json::json!({
            "enabled": true,
            "rendezvous_port": 21000,
            "relay_port": 21001,
            "mask": "not-a-cidr",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非法 mask 应 400，实得 {body}");
    assert!(body.contains("Invalid rustdesk_mask"), "错误文案应点名 mask: {body}");

    // relay_server 非法：不是 host:port。
    let (status, body) = post_raw(
        &api,
        serde_json::json!({
            "enabled": true,
            "rendezvous_port": 21000,
            "relay_port": 21001,
            "relay_server": "no-port-here",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非法 relay_server 应 400，实得 {body}");
    assert!(
        body.contains("Invalid rustdesk_relay_server"),
        "错误文案应点名 relay_server: {body}"
    );
}

/// 跨服务端口冲突在**校验阶段**当场 400，而不是拖到 bind 才 500。
///
/// 校验只读 SS/Trojan 的 live 端口表（listener 真正起来时登记）。这里直接把端口登记进
/// 表即可，无需真起监听器——确定性强，且正是 API 读取的那张表。
#[tokio::test]
async fn rendezvous_port_colliding_with_live_shadowsocks_port_is_rejected() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    let registered = harness
        .server_state
        .proxy_ports
        .register_shadowsocks(ports.rendezvous, "aes-256-gcm".into(), "pw".into())
        .await;
    assert!(registered, "端口应成功登记进 live 表");

    let (status, body) = post_raw(
        &harness.api_client(),
        serde_json::json!({
            "enabled": true,
            "rendezvous_port": ports.rendezvous,
            "relay_port": ports.relay,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "会合端口撞 SS 应在校验阶段 400，实得 {body}"
    );
    assert!(
        body.contains("Shadowsocks"),
        "错误文案应指出撞了哪个服务: {body}"
    );
}

/// 绑定失败当场 500，并通过 `GET /api/rustdesk` 的 `last_error` 持久暴露原因；
/// 同时确认「apply 失败不回滚配置」的既有语义（配置写的是期望值）。
#[tokio::test]
async fn bind_failure_reports_500_and_exposes_last_error() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    // 同步监听器占住会合端口并持有不放：服务端绑定（同样是 0.0.0.0）必然 EADDRINUSE。
    let squatter = StdTcpListener::bind(("0.0.0.0", ports.rendezvous)).expect("占住会合端口");

    let (status, body) = post_raw(
        &harness.api_client(),
        serde_json::json!({
            "enabled": true,
            "rendezvous_port": ports.rendezvous,
            "relay_port": ports.relay,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "端口被占导致绑定失败应 500，实得 {body}"
    );

    let (gstatus, raw) = get_raw(&harness.api_base).await;
    assert!(gstatus.is_success(), "GET 应成功，实得 {gstatus}: {raw}");
    let v = parse_json(&raw);
    assert_eq!(
        v["running"],
        serde_json::json!(false),
        "绑定失败后 running 应为 false"
    );
    let last_error = v["last_error"]
        .as_str()
        .unwrap_or_else(|| panic!("last_error 应非 null: {v}"));
    assert!(
        last_error.contains("绑定失败"),
        "last_error 应含「绑定失败」: {last_error}"
    );
    // apply 失败不回滚：已落库/入内存的是「期望配置」，界面据此提示后可直接重试。
    assert_eq!(
        v["enabled"],
        serde_json::json!(true),
        "失败不应把期望配置回滚成停用"
    );

    drop(squatter);
}

/// 成功启用后 `last_error` 为 null——与上一条的失败态形成对照。
#[tokio::test]
async fn successful_enable_leaves_last_error_null() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    let _ = enable(&harness.api_client(), &ports, None, false).await;

    let (status, raw) = get_raw(&harness.api_base).await;
    assert!(status.is_success(), "GET 应成功，实得 {status}: {raw}");
    let v = parse_json(&raw);
    assert_eq!(v["running"], serde_json::json!(true));
    assert_eq!(
        v["last_error"],
        serde_json::Value::Null,
        "成功启用后 last_error 应为 null: {raw}"
    );
}

/// 改端口重启：旧代监听在有界时间内释放，新代在新端口就绪，状态不被旧代污染。
#[tokio::test]
async fn changing_ports_restarts_listeners_on_the_new_ports() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let first = alloc_rustdesk_ports();
    let _ = enable(&harness.api_client(), &first, None, false).await;
    assert_tcp_reachable(first.rendezvous).await;
    assert_tcp_reachable(first.relay).await;

    let second = alloc_rustdesk_ports();
    let _ = enable(&harness.api_client(), &second, None, false).await;
    assert_tcp_reachable(second.rendezvous).await;
    assert_tcp_reachable(second.relay).await;

    // 旧代释放是有界的（stop_existing 逐个 await 四个任务句柄），轮询等全部拒连。
    let (old_rendezvous, old_nat, old_relay) = (first.rendezvous, first.nat(), first.relay);
    wait_until("改端口后旧 RustDesk 端口全部拒连", || async move {
        (!tcp_open(old_rendezvous).await
            && !tcp_open(old_nat).await
            && !tcp_open(old_relay).await)
            .then_some(())
    })
    .await
    .expect("改端口后旧端口应在超时内释放");

    let (status, raw) = get_raw(&harness.api_base).await;
    assert!(status.is_success(), "GET 应成功，实得 {status}: {raw}");
    let v = parse_json(&raw);
    assert_eq!(v["rendezvous_port"], serde_json::json!(second.rendezvous));
    assert_eq!(v["relay_port"], serde_json::json!(second.relay));
    assert_eq!(v["running"], serde_json::json!(true));
    // 新代自己的状态被写入、没被旧代污染：全新实例没有任何活跃中继对。
    // （统计轮询间隔 10s，这里刻意不等 tick——R1 的算术已由运行时单测钉住。）
    assert_eq!(v["relay_pairs"], serde_json::json!(0));

    // 收尾：显式停用，避免与并发用例留下监听（harness drop 也会收）。
    disable(&harness.api_client(), &second).await;
}
