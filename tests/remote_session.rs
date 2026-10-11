//! 集成测试：GUI 客户端互相远程控制的端到端链路（真实套接字，非内存替身）。
//!
//! 分两部分：
//!
//! - **A 部分（控制面）**：`TestHarness` 起真实服务端 → `POST /api/rustdesk` 开启内置
//!   RustDesk → 两个**真实** `rust-tunnel-client`（经 `run_client_with_status` 注入远程桥接）
//!   连上来 → 断言控制面消息流：`RemoteCapability` 下发 → `RemoteAnnounce` → `RemotePeerList`
//!   → `RemoteConnectRequest` → `RemoteSessionIncoming`/`RemoteConnectGrant` → 应答转发 →
//!   `RemoteSessionEnd` 清理。
//! - **B 部分（数据面）**：真实 hbbs 会合 + hbbr 中继（A 部分开出的端口）+ 两个真实
//!   [`RemoteManager`]，各自用**真实** [`RustDeskTransport`] 走**中继路径**建立会话 →
//!   断言往返屏幕帧与输入事件。
//! - **C 部分（数据面时序回归）**：与 B 相同的中继链路，但两侧改用 [`LazyTransport`]
//!   （GUI 客户端的真实行为：拿到 `RemoteCapability` 才预登记，否则等到首个 `establish`），
//!   并**刻意**让控制器先发起、被控端后进入会话，锁定「预登记」这个不变量。
//!
//! 控制面协议细节（版本门控、会话签发/转发/清理的状态机）已由
//! `crates/server/src/control_plane/remote.rs` 的单测覆盖；这里只验证「装配后的整条链路」。
//! 会话循环的内存版对照见 `crates/remote/src/integration_tests.rs`——B 部分是它的真实网络版。

#[path = "common/mod.rs"]
mod common;

use common::api_client::ApiClient;
use common::harness::{HarnessOpts, TestHarness};
use rust_tunnel_common::protocol::{ControlMessage, RemoteMode, RemoteParams};
use rust_tunnel_common::remote_bridge::RemoteBridge;
use rust_tunnel_remote::signaling::{LazyTransport, RustDeskTransport, SignalingConfig};
use rust_tunnel_remote::{
    DecodedFrame, DisplayInfo, EncodeOptions, FrameSink, InputEvent, InputSink, KeyEvent, MouseButton,
    RawFrame, RemoteManager, RemoteManagerConfig, RemoteResult, ScreenSource,
};
use std::collections::HashSet;
use std::net::{TcpListener as StdTcpListener, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

/// RustDesk 管理 API 路由。
const RUSTDESK_PATH: &str = "/api/rustdesk";

/// 单条等待的上限，避免测试挂死。
const WAIT: Duration = Duration::from_secs(10);

// ── 端口分配（照搬 tests/rustdesk_server.rs，独立二进制内自带去重表）──────

/// 一次测试独占的 RustDesk 端口对（会合 / NAT 探测由 -1 推导 / 中继）。
struct RustdeskPorts {
    rendezvous: u16,
    relay: u16,
}

/// 本测试二进制内已发放的端口集合，保证并发用例永不重号。
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

/// 由「纳秒时间戳 ^ 进程号」构造随机种子。
fn seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id())).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x1234_5678_9ABC_DEF0
}

/// 分配「会合端口 P + NAT 端口 P-1 + 中继端口 Q」三元组：同时探测四个套接字确认全空闲。
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
            StdTcpListener::bind(("0.0.0.0", relay)),
        );
        if let (Ok(t_rp), Ok(u_rp), Ok(t_nat), Ok(t_relay)) = probes {
            drop((t_rp, u_rp, t_nat, t_relay));
            used.insert(rendezvous);
            used.insert(nat);
            used.insert(relay);
            return RustdeskPorts { rendezvous, relay };
        }
    }
    panic!("在 20000..32000 内找不到空闲的 RustDesk 端口三元组");
}

/// `POST /api/rustdesk` 开启服务；`generate_key=true` 时一次性回传新公钥。
///
/// `always_use_relay` 控制是否强制走中继：B 部分传 `true`，否则回环两端同 IP 会被
/// hbbs 判成「同内网」走直连，绕过要验证的中继路径。
async fn enable(api: &ApiClient, ports: &RustdeskPorts, always_use_relay: bool) -> serde_json::Value {
    let body = serde_json::json!({
        "enabled": true,
        "rendezvous_port": ports.rendezvous,
        "relay_port": ports.relay,
        "always_use_relay": always_use_relay,
        "generate_key": true,
    });
    let (status, resp) = api.post_json(RUSTDESK_PATH, body).await;
    assert!(status.is_success(), "开启 RustDesk 应成功，实得 {status}: {resp}");
    resp
}

// ── A 部分：控制面测试替身 ────────────────────────────────────────────

/// 测试替身桥接：`attach` 存下上行发送端供测试主动驱动；`handle_control_message`
/// 把下行消息投进 channel 供断言。
///
/// 与 `crates/remote/src/integration_tests.rs` 的内存替身同构，但这里挂在**真实**
/// 控制通道上——消息确实经过了服务端。
struct TestBridge {
    /// `attach` 注入的上行发送端（client → server）。
    upstream: Mutex<Option<mpsc::Sender<ControlMessage>>>,
    /// 下行消息（server → client）转发端。
    down_tx: mpsc::UnboundedSender<ControlMessage>,
}

impl TestBridge {
    fn new(down_tx: mpsc::UnboundedSender<ControlMessage>) -> Arc<Self> {
        Arc::new(Self {
            upstream: Mutex::new(None),
            down_tx,
        })
    }

    /// 取可交给 `spawn_remote_client` 的桥接句柄。
    fn bridge(self: &Arc<Self>) -> Arc<dyn RemoteBridge> {
        Arc::clone(self) as Arc<dyn RemoteBridge>
    }

    /// 以客户端身份向服务端发一条控制消息（模拟 GUI 的主动操作）。
    ///
    /// 只在收到过 `RemoteCapability`（即 `attach` 已完成）之后调用。
    fn send(&self, msg: ControlMessage) {
        let guard = self.upstream.lock().expect("桥接锁");
        guard
            .as_ref()
            .expect("桥接尚未 attach（应先等到 RemoteCapability）")
            .try_send(msg)
            .expect("上行控制通道拥塞");
    }
}

impl RemoteBridge for TestBridge {
    fn attach(&self, sender: mpsc::Sender<ControlMessage>) {
        *self.upstream.lock().expect("桥接锁") = Some(sender);
    }

    fn handle_control_message(&self, msg: ControlMessage) -> bool {
        self.down_tx.send(msg).is_ok()
    }

    fn on_disconnected(&self) {
        *self.upstream.lock().expect("桥接锁") = None;
    }
}

/// 从桥接 inbox 里等到第一条满足 `f` 的消息（其余消息丢弃），超时即 panic。
async fn recv_match<T>(
    rx: &mut mpsc::UnboundedReceiver<ControlMessage>,
    desc: &str,
    mut f: impl FnMut(ControlMessage) -> Option<T>,
) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            let msg = rx.recv().await.expect("桥接通道关闭");
            if let Some(v) = f(msg) {
                return v;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("等待 {desc} 超时"))
}

/// 断言 `rd_id` 是 9 位数字（服务端分配的稳定 RustDesk ID）。
fn assert_nine_digit_rd_id(rd_id: &str) {
    assert_eq!(rd_id.len(), 9, "rd_id 应为 9 位，实得 {rd_id:?}");
    assert!(rd_id.chars().all(|c| c.is_ascii_digit()), "rd_id 应全为数字：{rd_id:?}");
}

/// A 部分：控制面完整流程（真实服务端 + 两个真实客户端）。
#[tokio::test]
async fn control_plane_full_flow_over_real_clients() {
    let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    // generate_key=true：POST 一次性回传新公钥，它正是服务端 remote_params 会下发的
    // 归一化公钥（ServerKey::parse(&rd.key).key()），可直接当客户端的 licence_key。
    let resp = enable(&harness.api_client(), &ports, false).await;
    let public_key = resp["key"]
        .as_str()
        .unwrap_or_else(|| panic!("generate_key 应回传公钥: {resp}"))
        .to_owned();
    assert!(!public_key.is_empty(), "回传的公钥不应为空");

    // 两个真实客户端，各注入一个记录型桥接。
    let (ctrl_down_tx, mut ctrl_rx) = mpsc::unbounded_channel();
    let (target_down_tx, mut target_rx) = mpsc::unbounded_channel();
    let ctrl_bridge = TestBridge::new(ctrl_down_tx);
    let target_bridge = TestBridge::new(target_down_tx);
    let _ctrl = harness.spawn_remote_client(Some("ctrl"), ctrl_bridge.bridge());
    let _target = harness.spawn_remote_client(Some("target"), target_bridge.bridge());

    // 1) RemoteCapability：注册后首包，两端各收到一次（含会合/中继参数 + 分配的 rd_id）。
    let (ctrl_params, ctrl_rd_id) = recv_match(&mut ctrl_rx, "控制器 RemoteCapability", |m| match m {
        ControlMessage::RemoteCapability { params, rd_id } => Some((params, rd_id)),
        _ => None,
    })
    .await;
    let (target_params, target_rd_id) = recv_match(&mut target_rx, "被控端 RemoteCapability", |m| match m {
        ControlMessage::RemoteCapability { params, rd_id } => Some((params, rd_id)),
        _ => None,
    })
    .await;
    for (who, params) in [("ctrl", &ctrl_params), ("target", &target_params)] {
        let p = params
            .as_ref()
            .unwrap_or_else(|| panic!("{who} 应收到 Some(params)（RustDesk 已启用）"));
        assert_eq!(p.public_key, public_key, "{who} 的 licence_key 应为服务端公钥");
        assert_eq!(p.rendezvous_port, ports.rendezvous, "{who} 会合端口");
        assert_eq!(p.relay_port, ports.relay, "{who} 中继端口");
        assert_eq!(p.rendezvous_host, "", "{who} rendezvous_host 应留空由客户端补齐");
    }
    assert_nine_digit_rd_id(&ctrl_rd_id);
    assert_nine_digit_rd_id(&target_rd_id);
    assert_ne!(ctrl_rd_id, target_rd_id, "两端应拿到不同的 rd_id");

    // 2) RemoteAnnounce：被控端上报开启被控 → 服务端广播最新可远程列表。
    target_bridge.send(ControlMessage::RemoteAnnounce {
        enabled: true,
        platform: "linux".to_owned(),
        displays: 2,
    });
    let peers = recv_match(&mut ctrl_rx, "控制器 RemotePeerList", |m| match m {
        ControlMessage::RemotePeerList { peers } => Some(peers),
        _ => None,
    })
    .await;
    assert_eq!(peers.len(), 1, "控制器应只看到被控端一个可远程对端：{peers:?}");
    assert_eq!(peers[0].client_name, "target");
    assert_eq!(peers[0].rd_id, target_rd_id, "列表里的 rd_id 应与被控端通告一致");
    assert_eq!(peers[0].platform, "linux");
    assert_eq!(peers[0].displays, 2);

    // 3) RemoteConnectRequest → 服务端签发会话 → 双向下发 Incoming / Grant。
    ctrl_bridge.send(ControlMessage::RemoteConnectRequest {
        target_client: "target".to_owned(),
        mode: RemoteMode::Control,
    });
    let (incoming_sid, incoming_token, from_name, mode) =
        recv_match(&mut target_rx, "被控端 RemoteSessionIncoming", |m| match m {
            ControlMessage::RemoteSessionIncoming {
                session_id,
                token,
                from_name,
                mode,
            } => Some((session_id, token, from_name, mode)),
            _ => None,
        })
        .await;
    let (grant_sid, grant_token, grant_target_rd_id, grant_platform, grant_params) =
        recv_match(&mut ctrl_rx, "控制器 RemoteConnectGrant", |m| match m {
            ControlMessage::RemoteConnectGrant {
                session_id,
                token,
                target_rd_id,
                target_platform,
                params,
            } => Some((session_id, token, target_rd_id, target_platform, params)),
            _ => None,
        })
        .await;
    assert_eq!(grant_sid, incoming_sid, "两端的 session_id 必须一致");
    assert_eq!(grant_token, incoming_token, "两端的 token 必须一致（数据面握手鉴权用）");
    assert_eq!(from_name, "ctrl", "被控端看到的发起方应是控制器名");
    assert_eq!(mode, RemoteMode::Control);
    assert_eq!(grant_target_rd_id, target_rd_id);
    assert_eq!(grant_platform, "linux");
    assert_eq!(grant_params.public_key, public_key);

    // 4) 被控端应答 accepted → 服务端转发给控制器。
    target_bridge.send(ControlMessage::RemoteSessionResponse {
        session_id: incoming_sid.clone(),
        accepted: true,
        reason: None,
    });
    let (accepted, reason) = recv_match(&mut ctrl_rx, "转发 RemoteSessionResponse", |m| match m {
        ControlMessage::RemoteSessionResponse {
            accepted, reason, ..
        } => Some((accepted, reason)),
        _ => None,
    })
    .await;
    assert!(accepted, "控制器应收到 accepted=true");
    assert!(reason.is_none(), "接受时不应带原因");

    // 5) 会话结束清理：控制器发起 End → 服务端通知被控端。
    ctrl_bridge.send(ControlMessage::RemoteSessionEnd {
        session_id: incoming_sid.clone(),
        reason: "用户中断".to_owned(),
    });
    let (end_sid, end_reason) = recv_match(&mut target_rx, "被控端 RemoteSessionEnd", |m| match m {
        ControlMessage::RemoteSessionEnd { session_id, reason } => Some((session_id, reason)),
        _ => None,
    })
    .await;
    assert_eq!(end_sid, incoming_sid);
    assert_eq!(end_reason, "用户中断");

    // 会话是一次性消费的：重复结束同一 session 不应再通知对端（证明已清理）。
    ctrl_bridge.send(ControlMessage::RemoteSessionEnd {
        session_id: incoming_sid.clone(),
        reason: "重复结束".to_owned(),
    });
    let extra = tokio::time::timeout(Duration::from_millis(300), target_rx.recv()).await;
    assert!(extra.is_err(), "已消费的会话重复结束不应再通知对端，实得 {extra:?}");
}

// ── B 部分：数据面测试替身（平台实现）────────────────────────────────

/// 固定内容的采集源（32x24 纯色）。
struct FakeScreen;

impl ScreenSource for FakeScreen {
    fn displays(&self) -> Vec<DisplayInfo> {
        vec![DisplayInfo {
            id: 0,
            width: 32,
            height: 24,
            name: "fake".into(),
            primary: true,
        }]
    }

    fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
        let (w, h) = (32u32, 24u32);
        let mut rgba = Vec::with_capacity((w * h) as usize * 4);
        for _ in 0..(w * h) {
            rgba.extend_from_slice(&[40, 80, 120, 255]);
        }
        Ok(RawFrame {
            display,
            width: w,
            height: h,
            rgba,
        })
    }
}

/// 记录收到的输入事件，并可经 channel 通知等待方。
#[derive(Default)]
struct RecordingInput {
    count: AtomicUsize,
    notify: Mutex<Option<mpsc::UnboundedSender<InputEvent>>>,
}

impl RecordingInput {
    fn with_notify() -> (Arc<Self>, mpsc::UnboundedReceiver<InputEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                notify: Mutex::new(Some(tx)),
                ..Self::default()
            }),
            rx,
        )
    }

    fn record(&self, event: InputEvent) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if let Ok(guard) = self.notify.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(event);
            }
        }
    }
}

impl InputSink for RecordingInput {
    fn mouse_move(&self, x: i32, y: i32, display: u32) -> RemoteResult<()> {
        self.record(InputEvent::MouseMove { x, y, display });
        Ok(())
    }
    fn mouse_button(&self, button: MouseButton, pressed: bool) -> RemoteResult<()> {
        self.record(InputEvent::MouseButton { button, pressed });
        Ok(())
    }
    fn wheel(&self, dx: i32, dy: i32) -> RemoteResult<()> {
        self.record(InputEvent::Wheel { dx, dy });
        Ok(())
    }
    fn key(&self, event: KeyEvent) -> RemoteResult<()> {
        self.record(InputEvent::Key(event));
        Ok(())
    }
}

/// 记录控制器侧画面/显示器列表。
#[derive(Default)]
struct RecordingSink {
    frames: AtomicUsize,
    displays: Mutex<Vec<DisplayInfo>>,
    closed: Mutex<Option<String>>,
    notify: Mutex<Option<mpsc::UnboundedSender<()>>>,
}

impl RecordingSink {
    fn with_notify() -> (Arc<Self>, mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                notify: Mutex::new(Some(tx)),
                ..Self::default()
            }),
            rx,
        )
    }
}

impl FrameSink for RecordingSink {
    fn on_displays(&self, displays: Vec<DisplayInfo>) {
        if let Ok(mut guard) = self.displays.lock() {
            *guard = displays;
        }
    }

    fn on_frame(&self, _frame: DecodedFrame) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        if let Ok(guard) = self.notify.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(());
            }
        }
    }

    fn on_closed(&self, reason: String) {
        if let Ok(mut guard) = self.closed.lock() {
            *guard = Some(reason);
        }
    }
}

/// 构造一个连到本 harness 的 [`RemoteManager`]。
fn manager(
    server_host: &str,
    name: &str,
    enabled: bool,
    rd_id: &str,
    params: &RemoteParams,
    transport: Arc<RustDeskTransport>,
) -> Arc<RemoteManager> {
    let mgr = RemoteManager::new(RemoteManagerConfig {
        server_host: server_host.to_owned(),
        self_client_name: name.to_owned(),
        platform: "linux".to_owned(),
        displays: 1,
        enabled,
        establisher: transport,
        encode: EncodeOptions {
            quality: 60,
            scale: 1.0,
        },
    });
    // 双方先拿到能力通告（设置 params / rd_id），与真实控制通道下发等价。
    mgr.handle_control_message(ControlMessage::RemoteCapability {
        params: Some(params.clone()),
        rd_id: rd_id.to_owned(),
    });
    mgr
}

/// 构造一个用**延迟建立器**的 [`RemoteManager`]，并投递能力通告。
///
/// 与 [`manager`] 的区别：传输不在构造时建立，只在拿到 `RemoteCapability` 触发**预登记**
/// （或首次 `establish`）时才构造——这正是 GUI 客户端 [`LazyTransport`] 的真实行为。
fn manager_lazy(
    server_host: &str,
    name: &str,
    enabled: bool,
    rd_id: &str,
    params: &RemoteParams,
) -> Arc<RemoteManager> {
    let mgr = RemoteManager::new(RemoteManagerConfig {
        server_host: server_host.to_owned(),
        self_client_name: name.to_owned(),
        platform: "linux".to_owned(),
        displays: 1,
        enabled,
        establisher: Arc::new(LazyTransport::new(server_host.to_owned())),
        encode: EncodeOptions {
            quality: 60,
            scale: 1.0,
        },
    });
    mgr.handle_control_message(ControlMessage::RemoteCapability {
        params: Some(params.clone()),
        rd_id: rd_id.to_owned(),
    });
    mgr
}

/// B 部分：真实 hbbs/hbbr + 两个 RemoteManager，走中继路径建立会话并往返画面/输入。
#[tokio::test]
async fn data_plane_relay_session_between_two_managers() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    // always_use_relay=true：强制中继，跳过打洞直连（回环两端同 IP 会被判成同内网）。
    let resp = enable(&harness.api_client(), &ports, true).await;
    let public_key = resp["key"]
        .as_str()
        .unwrap_or_else(|| panic!("generate_key 应回传公钥: {resp}"))
        .to_owned();

    // server_host 就是 config.server（host:port）；signaling 的 resolve 会剥掉端口。
    let server_host = harness.control_addr.to_string();
    let params = RemoteParams {
        rendezvous_host: String::new(),
        rendezvous_port: ports.rendezvous,
        relay_port: ports.relay,
        relay_server: String::new(),
        public_key,
        always_use_relay: true,
    };

    // 两个真实传输：各自后台跑 UDP 登记 + 直连监听任务。
    let ctrl_transport = RustDeskTransport::new(SignalingConfig::from_params(
        &params,
        "111111111".to_owned(),
        &server_host,
    ))
    .await
    .expect("构造控制器传输");
    let ctld_transport = RustDeskTransport::new(SignalingConfig::from_params(
        &params,
        "222222222".to_owned(),
        &server_host,
    ))
    .await
    .expect("构造被控端传输");

    let ctrl = manager(
        &server_host,
        "home-nas",
        false,
        "111111111",
        &params,
        ctrl_transport.clone(),
    );
    let ctld = manager(
        &server_host,
        "office-pc",
        true,
        "222222222",
        &params,
        ctld_transport.clone(),
    );

    let (ctrl_sink, mut frame_rx) = RecordingSink::with_notify();
    let (ctld_input, mut input_rx) = RecordingInput::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    ctld.install_platform(Arc::new(FakeScreen), ctld_input.clone());

    let (ctrl_tx, _ctrl_upstream) = mpsc::channel(64);
    let (ctld_tx, _ctld_upstream) = mpsc::channel(64);
    ctrl.attach(ctrl_tx);
    ctld.attach(ctld_tx);

    // UDP `RegisterPeer` 心跳没有 ack：等两端在 hbbs 登记完成再发起连接，否则控制器
    // 先发的 `RequestRelay` 会因目标不在登记表而被丢弃（推送丢失 → 会话建不起来）。
    // 回环往返亚毫秒，500ms 是安全裕量。
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 服务端角色（这里直接注入，等价于控制面下发的授权/转达）。
    ctrl.handle_control_message(ControlMessage::RemoteConnectGrant {
        session_id: "sess-1".to_owned(),
        token: "tok-1".to_owned(),
        target_rd_id: "222222222".to_owned(),
        target_platform: "linux".to_owned(),
        params: params.clone(),
    });
    ctld.handle_control_message(ControlMessage::RemoteSessionIncoming {
        session_id: "sess-1".to_owned(),
        token: "tok-1".to_owned(),
        from_name: "home-nas".to_owned(),
        mode: RemoteMode::Control,
    });

    // 控制器应收到显示器列表与至少一帧画面（全部经真实 hbbr 中继转发）。
    tokio::time::timeout(WAIT, frame_rx.recv())
        .await
        .expect("等画面超时")
        .expect("画面通道关闭");
    assert!(ctrl_sink.frames.load(Ordering::Relaxed) >= 1, "应收到画面");
    assert_eq!(
        ctrl_sink.displays.lock().unwrap().len(),
        1,
        "应收到显示器列表"
    );

    // 控制器回传输入 → 被控端注入。
    ctrl.send_input(InputEvent::MouseMove {
        x: 10,
        y: 20,
        display: 0,
    });
    let injected = tokio::time::timeout(WAIT, input_rx.recv())
        .await
        .expect("等输入注入超时")
        .expect("输入通道关闭");
    assert_eq!(
        injected,
        InputEvent::MouseMove {
            x: 10,
            y: 20,
            display: 0
        }
    );

    // 一键中断：两端 active 都应清空（控制器 cancel → Bye → 被控端收尾）。
    ctrl.abort_active();
    for _ in 0..100 {
        if ctrl.snapshot().active.is_none() && ctld.snapshot().active.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ctrl.snapshot().active.is_none(), "控制器 active 应清空");
    assert!(ctld.snapshot().active.is_none(), "被控端 active 应清空");

    ctrl_transport.shutdown();
    ctld_transport.shutdown();
}

/// C 部分：复现**真实时序**的回归用例——控制器发出中继请求时，被控端尚未进入会话。
///
/// 两侧都用 [`LazyTransport`]（GUI 客户端的真实行为）：传输只在预登记或首个 `establish`
/// 时才构造。控制器被授权后**立刻**发 `RequestRelay`（`always_use_relay` 下不试直连，
/// 一个 RTT 就到 hbbs），而被控端要等 `RemoteSessionIncoming` 才进会话。若不在
/// `RemoteCapability` 时预登记，hbbs 会因目标不在登记表里**静默丢弃**该请求，被控端随后
/// 白等满 `RELAY_WAIT`（25s）才失败。本用例刻意在两者之间留出空档来锁定这个不变量。
#[tokio::test]
async fn relay_session_survives_controller_requesting_before_target_session_starts() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    // always_use_relay=true：控制器直接走中继信令，不先试直连（把时序压到最短）。
    let resp = enable(&harness.api_client(), &ports, true).await;
    let public_key = resp["key"]
        .as_str()
        .unwrap_or_else(|| panic!("generate_key 应回传公钥: {resp}"))
        .to_owned();

    let server_host = harness.control_addr.to_string();
    let params = RemoteParams {
        rendezvous_host: String::new(),
        rendezvous_port: ports.rendezvous,
        relay_port: ports.relay,
        relay_server: String::new(),
        public_key,
        always_use_relay: true,
    };

    // 被控端 enabled=true → 收到能力即预登记；控制器 enabled=false → 只在 establish 时构造。
    let ctrl = manager_lazy(&server_host, "home-nas", false, "111111111", &params);
    let ctld = manager_lazy(&server_host, "office-pc", true, "222222222", &params);

    let (ctrl_sink, mut frame_rx) = RecordingSink::with_notify();
    let (ctld_input, _input_rx) = RecordingInput::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    ctld.install_platform(Arc::new(FakeScreen), ctld_input);

    let (ctrl_tx, _ctrl_upstream) = mpsc::channel(64);
    let (ctld_tx, _ctld_upstream) = mpsc::channel(64);
    ctrl.attach(ctrl_tx);
    ctld.attach(ctld_tx);

    // 等被控端的**预登记**落到 hbbs。UDP `RegisterPeer` 没有 ack，回环往返亚毫秒，
    // 500ms 是安全裕量。
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 控制器先发起：中继请求即刻发往 hbbs。
    ctrl.handle_control_message(ControlMessage::RemoteConnectGrant {
        session_id: "sess-c".to_owned(),
        token: "tok-c".to_owned(),
        target_rd_id: "222222222".to_owned(),
        target_platform: "linux".to_owned(),
        params: params.clone(),
    });

    // 刻意留空档：确保控制器的中继请求**早于**被控端进入会话。修复前推送会在此被丢弃。
    tokio::time::sleep(Duration::from_millis(300)).await;

    ctld.handle_control_message(ControlMessage::RemoteSessionIncoming {
        session_id: "sess-c".to_owned(),
        token: "tok-c".to_owned(),
        from_name: "home-nas".to_owned(),
        mode: RemoteMode::Control,
    });

    // 画面仍应经真实 hbbr 中继到达（说明早先的推送被 hbbs 收下并缓存，未被丢弃）。
    tokio::time::timeout(WAIT, frame_rx.recv())
        .await
        .expect("等画面超时——中继请求很可能在目标登记前被丢弃")
        .expect("画面通道关闭");
    assert!(ctrl_sink.frames.load(Ordering::Relaxed) >= 1, "应收到画面");
}
