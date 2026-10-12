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
    ApprovalPolicy, DecodedFrame, DisplayInfo, EncodeOptions, EstablishedStream, EstablishRequest, FrameSink,
    InputEvent, InputSink, KeyEvent, MouseButton, RawFrame, RemoteManager, RemoteManagerConfig, RemoteResult, Role,
    ScreenSource, TransportEstablisher, TransportPath,
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

/// 与 [`recv_match`] 同源，但用于**有界**上行通道（[`RemoteBridge::attach`] 注入的那条）。
async fn recv_match_bounded<T>(
    rx: &mut mpsc::Receiver<ControlMessage>,
    desc: &str,
    mut f: impl FnMut(ControlMessage) -> Option<T>,
) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            let msg = rx.recv().await.expect("上行通道关闭");
            if let Some(v) = f(msg) {
                return v;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("等待 {desc} 超时"))
}

/// 模拟服务端把被控端的**接受**应答转达给控制器。
///
/// 控制器现在必须等到 `accepted:true` 才建立数据面（接入确认门控），而这两个 e2e 用例没有
/// 真实控制通道、控制消息全靠手工注入，所以「服务端转发应答」这一步也得手工补上；否则控制器
/// 会一直停在 `Negotiating`，画面永远不会到——表现为等画面超时。
async fn relay_acceptance(ctld_upstream: &mut mpsc::Receiver<ControlMessage>, ctrl: &Arc<RemoteManager>) {
    let msg = recv_match_bounded(ctld_upstream, "被控端接受应答", |m| {
        matches!(m, ControlMessage::RemoteSessionResponse { accepted: true, .. }).then_some(m)
    })
    .await;
    assert!(ctrl.handle_control_message(msg), "控制器应消费被控端应答");
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
///
/// `establisher` 收 `Arc<dyn TransportEstablisher>`（而非具体类型）：既接受真实
/// [`RustDeskTransport`]，也接受测试自己的包装建立器（见 [`CuttableEstablisher`]）。
fn manager(
    server_host: &str,
    name: &str,
    enabled: bool,
    rd_id: &str,
    params: &RemoteParams,
    establisher: Arc<dyn TransportEstablisher>,
) -> Arc<RemoteManager> {
    let mgr = RemoteManager::new(RemoteManagerConfig {
        server_host: server_host.to_owned(),
        self_client_name: name.to_owned(),
        platform: "linux".to_owned(),
        displays: 1,
        enabled,
        establisher,
        encode: EncodeOptions {
            quality: 60,
            scale: 1.0,
            max_fps: 10,
        },
        share_dir: None,
        download_dir: std::env::temp_dir(),
        // 这两个用例测的是**真实 hbbr 中继的数据面**，不是接入确认门控（后者由
        // `crates/remote` 的集成测试覆盖）。用 `Auto` 免去弹窗，但控制器仍会等应答，
        // 故下面仍要手工转达 `accepted`——见 `relay_acceptance`。
        approval_policy: ApprovalPolicy::Auto,
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
            max_fps: 10,
        },
        share_dir: None,
        download_dir: std::env::temp_dir(),
        // 这两个用例测的是**真实 hbbr 中继的数据面**，不是接入确认门控（后者由
        // `crates/remote` 的集成测试覆盖）。用 `Auto` 免去弹窗，但控制器仍会等应答，
        // 故下面仍要手工转达 `accepted`——见 `relay_acceptance`。
        approval_policy: ApprovalPolicy::Auto,
    });
    mgr.handle_control_message(ControlMessage::RemoteCapability {
        params: Some(params.clone()),
        rd_id: rd_id.to_owned(),
    });
    mgr
}

/// 控制器侧的「可掐断」建立器：`establish` 转发给真实 [`RustDeskTransport`]，但把返回的流穿过
/// 一条内存管道；测试可随时 [`Self::cut`] 丢弃桥接，让会话循环在收到任何数据之前先读到 **EOF**。
///
/// # 为什么必须这样构造「断线」
///
/// 普通收尾路径**总会尽力**发一条 `Bye`（`controller.rs` / `controlled.rs` 的 `BYE_TIMEOUT`
/// 收尾），而收到 `Bye` 就等价于「对端主动结束」（`deliberate = true`）。因此 `abort_active`
/// 这类**优雅**结束永远走不到 `deliberate = false`。真实的「断线/失败」是**粗鲁**断开——对端
/// 进程崩溃、网线被拔——控制器在收到任何结束帧之前先读到裸 EOF / IO 错误。内存 e2e 里没法让
/// 对端进程凭空消失，故用这层可丢弃的桥接来复现：掐断桥接 = 数据面瞬断。
///
/// 桥接只搬字节，两端之间那条**真实**的中继链路照常建立（握手、鉴权、加密都走真实路径），
/// 只是多了一跳进程内转发，测试据此拿到一个断线开关。
struct CuttableEstablisher {
    /// 真正的中继建立器。
    inner: Arc<RustDeskTransport>,
    /// 桥接任务句柄；`cut` 时 abort 之，管道两端随之关闭、会话循环读到 EOF。
    bridge: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CuttableEstablisher {
    fn new(inner: Arc<RustDeskTransport>) -> Self {
        Self {
            inner,
            bridge: Mutex::new(None),
        }
    }

    /// 粗鲁切断数据面（模拟对端崩溃 / 掉线）。
    fn cut(&self) {
        if let Some(handle) = self.bridge.lock().expect("桥接锁").take() {
            handle.abort();
        }
    }
}

/// `#[async_trait]` 脱糖后的 `establish` 返回类型。
///
/// 根元包的 `tests/` 不能新增 dev-dep（`async-trait` 未列入），所以这里**手写**展开后的
/// `Pin<Box<dyn Future + Send>>` 形态来直接实现 trait，而不是加 `#[async_trait]` 标注。
type BoxedEstablish<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = RemoteResult<EstablishedStream>> + Send + 'a>>;

impl TransportEstablisher for CuttableEstablisher {
    fn establish<'life0, 'async_trait>(
        &'life0 self,
        request: EstablishRequest,
        role: Role,
    ) -> BoxedEstablish<'async_trait>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let established = self.inner.establish(request, role).await?;
            let (local, mut remote) = tokio::io::duplex(256 * 1024);
            let mut real = established.stream;
            let handle = tokio::spawn(async move {
                // 双向搬运；任一端关闭即返回，两个管道端随之 drop。
                let _ = tokio::io::copy_bidirectional(&mut real, &mut remote).await;
            });
            *self.bridge.lock().expect("桥接锁") = Some(handle);
            Ok(EstablishedStream {
                stream: Box::new(local),
                path: established.path,
            })
        })
    }
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
    // 被控端的上行要留着读：控制器现在要等它的 `accepted` 才建立数据面。
    let (ctld_tx, mut ctld_upstream) = mpsc::channel(64);
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
    relay_acceptance(&mut ctld_upstream, &ctrl).await;

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

    // ── 阶段 3 新增①：数据面路径如实上报 ────────────────────────────────
    // 本链路 `always_use_relay = true`，两端都实打实走 hbbr 中继，`transport` 就必须报
    // [`TransportPath::Relay`]。建立方若把「实际走了中继」谎报成 `Direct`，UI 会向用户宣称一条
    // 根本不存在的内网直连，误导「为什么这么慢」的判断。这是 `EstablishedStream::path` →
    // `RemoteUiState::transport` 这条透传链路的真实网络版对照（内存版见 `crates/remote` 的
    // `controller_records_transport_path`）。两侧都断言：控制器与被控端各自独立回报路径。
    assert!(
        wait_within(Duration::from_secs(3), || ctrl.snapshot().transport == Some(TransportPath::Relay)).await,
        "控制器应如实上报中继路径，实得 {:?}",
        ctrl.snapshot().transport
    );
    assert!(
        wait_within(Duration::from_secs(3), || ctld.snapshot().transport == Some(TransportPath::Relay)).await,
        "被控端应如实上报中继路径，实得 {:?}",
        ctld.snapshot().transport
    );

    // ── 阶段 3 新增②：延迟读数「开窗即有」 ──────────────────────────────
    // 会话循环**复用既有** `Heartbeat` → `HeartbeatAck` 往返测 RTT（数据面变体一个都不加），
    // 经 `SessionStatsReporter` 上报，管理器 EMA 平滑后写入 `latency_ms`。本断言的关键不是「有
    // 读数」，而是「**很快**有」：心跳 interval 的首个 tick **立即到期**，所以会话一建立就有样本。
    // 这里 3s 的上限远小于 `HEARTBEAT_INTERVAL`（10s）——把心跳起点改回 `now + HEARTBEAT_INTERVAL`
    // （首个样本要等满一个周期）必然让本断言超时。回环 + hbbr 中继的 RTT 只可能是亚毫秒到毫秒级，
    // 2000ms 是宽松上限。两侧各跑一次心跳循环，故两侧都应有读数。
    assert!(
        wait_within(Duration::from_secs(3), || ctrl.snapshot().latency_ms.is_some()).await,
        "控制器应在会话建立后 3s 内就有延迟读数（首个心跳 tick 立即到期），实得 {:?}",
        ctrl.snapshot().latency_ms
    );
    let latency = ctrl.snapshot().latency_ms.expect("刚断言过有读数");
    assert!(latency < 2000, "中继回环延迟应是合理小值，实得 {latency}ms");
    assert!(
        wait_within(Duration::from_secs(3), || ctld.snapshot().latency_ms.is_some()).await,
        "被控端应在会话建立后 3s 内就有延迟读数，实得 {:?}",
        ctld.snapshot().latency_ms
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

    // ── 阶段 3 新增③：会话结束语义（`deliberate` 是断线重连的唯一判据）──────────
    // 用户主动中断是**主动结束**，`deliberate` 必须为 `true`。`client-gui` 的断线自动重连**只**
    // 看这一个布尔（见 `crates/client-gui/src/remote/reconnect.rs::on_session_end`）：判成
    // `false` 会把「用户点了挂断」当成一次断线而自动重连回来。
    //
    // 这里断言**被控端**侧的快照：控制器 `abort_active()` 后数据面 `Bye` 会把这次「主动结束」
    // 传过去，被控端据此记录 `deliberate = true`。（控制器**自身**主动中断后其 `session_end`
    // 不被写入——`abort_active` → `conclude_session` 先清空 `active`，会话任务随后返回时
    // `finish_session` 因 `active` 已空而早退、丢弃了那个 `SessionEnd`。这是本阶段发现的引擎侧
    // 问题，已在交付说明中记录；不在这里把它放宽成同义反复的断言。）
    //
    // 被控端 `active` 置空与 `session_end` 写入发生在**同一个** `update_ui` 闭包里
    // （`finish_session`），所以上面那轮轮询退出时 `session_end` 必然已就位，无需再等。
    //
    // **不断言**结束原因字符串：控制器侧收尾时，`cancel`（→「用户中断」）与命令通道被
    // `conclude_session` 关掉（→「会话命令通道已关闭」）是 `select!` 里的两个竞速分支，
    // 结束原因因此不确定（两条都是本端主动收尾）。被控端经 `Bye` 收到的永远是 `deliberate=true`，
    // 这才是本条要锁的不变量。
    let (end_sid, end) = ctld
        .snapshot()
        .session_end
        .expect("被控端应记录到一次会话结束");
    assert_eq!(end_sid, "sess-1", "session_end 应指向本次会话");
    assert!(end.deliberate, "用户主动中断应记为 deliberate=true，实得 {end:?}");
    assert!(!end.reason.is_empty(), "结束原因不应为空，实得 {end:?}");

    // 传输现在靠 `Drop` 取消后台登记/直连任务（见 `signaling.rs` 的 `impl Drop`）；本用例断言
    // 已完成，两个 `Arc<RustDeskTransport>` 随函数结束一起释放即可，无需显式关闭。
    drop((ctrl_transport, ctld_transport));
}

/// B 部分续：数据面被**粗鲁切断**时，控制器必须把结束记成 `deliberate = false`。
///
/// 锁的不变量：`SessionEnd.deliberate` 是断线自动重连的**唯一**判据（`client-gui` 的
/// `reconnect.rs::on_session_end`）——`false` 才触发重连、`true` 不触发。优雅收尾永远带一条
/// `Bye`（`deliberate = true`），所以「断线」只能走**裸 EOF / IO 错误**这条路径；把它错记成
/// `true`，一次真实的网络闪断就会被当成「用户主动挂断」而**永不重连**（用户看到画面卡死后
/// 再无反应）。这里用 [`CuttableEstablisher`] 强制走这条路径：真实中继链路照常建立，随后
/// 掐断桥接模拟对端掉线。
#[tokio::test]
async fn abrupt_disconnect_is_recorded_as_non_deliberate_end() {
    let harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    // always_use_relay=true：强制中继（回环两端同 IP 会被 hbbs 判成同内网走直连）。
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

    // 控制器侧套一层可掐断的桥接；被控端用真实传输——它的正常收尾 `Bye` 会被已断的桥接吞掉，
    // 于是控制器只看到裸 EOF，正是「断线」而非「对端主动结束」。
    let cuttable = Arc::new(CuttableEstablisher::new(ctrl_transport.clone()));
    let ctrl = manager(&server_host, "home-nas", false, "111111111", &params, cuttable.clone());
    let ctld = manager(&server_host, "office-pc", true, "222222222", &params, ctld_transport.clone());

    let (ctrl_sink, mut frame_rx) = RecordingSink::with_notify();
    let (ctld_input, _input_rx) = RecordingInput::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    ctld.install_platform(Arc::new(FakeScreen), ctld_input);

    let (ctrl_tx, _ctrl_upstream) = mpsc::channel(64);
    let (ctld_tx, mut ctld_upstream) = mpsc::channel(64);
    ctrl.attach(ctrl_tx);
    ctld.attach(ctld_tx);

    // 等两端在 hbbs 登记完成再发起（原因同 B 部分：UDP 登记没有 ack）。
    tokio::time::sleep(Duration::from_millis(500)).await;

    ctrl.handle_control_message(ControlMessage::RemoteConnectGrant {
        session_id: "sess-drop".to_owned(),
        token: "tok-drop".to_owned(),
        target_rd_id: "222222222".to_owned(),
        target_platform: "linux".to_owned(),
        params: params.clone(),
    });
    ctld.handle_control_message(ControlMessage::RemoteSessionIncoming {
        session_id: "sess-drop".to_owned(),
        token: "tok-drop".to_owned(),
        from_name: "home-nas".to_owned(),
        mode: RemoteMode::Control,
    });
    relay_acceptance(&mut ctld_upstream, &ctrl).await;

    // 先确认会话真的跑起来了（收到一帧并如实报了中继路径），否则后面的「断线」无从谈起。
    tokio::time::timeout(WAIT, frame_rx.recv())
        .await
        .expect("等画面超时")
        .expect("画面通道关闭");
    assert!(
        wait_within(Duration::from_secs(3), || ctrl.snapshot().transport == Some(TransportPath::Relay)).await,
        "会话应经中继建立，实得 {:?}",
        ctrl.snapshot().transport
    );

    // 粗鲁切断：控制器应在收到任何结束帧之前读到裸 EOF（对端掉线）→ deliberate=false。
    cuttable.cut();

    assert!(
        wait_within(Duration::from_secs(5), || ctrl
            .snapshot()
            .session_end
            .as_ref()
            .is_some_and(|(sid, end)| sid == "sess-drop" && !end.deliberate))
        .await,
        "断线必须记为 deliberate=false（否则断线自动重连永远不会触发），实得 {:?}",
        ctrl.snapshot().session_end
    );
    let snap = ctrl.snapshot();
    assert!(snap.active.is_none(), "断线后控制器 active 应清空");
    assert_eq!(snap.transport, None, "会话结束后路径读数应清零");
    assert_eq!(snap.latency_ms, None, "会话结束后延迟读数应清零");

    drop((ctrl_transport, ctld_transport));
}

/// C 部分：**预登记**不变量的回归用例——被控端必须在会话发起前就登记到 hbbs。
///
/// 两侧都用 [`LazyTransport`]（GUI 客户端的真实行为）：传输只在预登记或首个 `establish`
/// 时才构造。控制器的 `RequestRelay` 是发给 hbbs、由 hbbs 转发到**目标已登记的**信令连接的；
/// 若不在 `RemoteCapability`（及打开被控开关）时预登记，hbbs 会因目标不在登记表里**静默丢弃**
/// 该请求（不回复、不排队、不重试），被控端随后白等满 `RELAY_WAIT`（25s）才失败。
///
/// 接入确认门控加入后，控制器不再于被授权时立刻发 `RequestRelay`——它要等被控端的 `accepted`
/// 才建立数据面，故本用例原先刻意制造的那段「控制器抢跑」时序已不可达。这里保留授权与
/// `RemoteSessionIncoming` 之间的空档，改为覆盖「授权已下发、被控端尚未进会话」这一中间态
/// 不会让任一端提前失败；预登记仍是本用例成立的前提。
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
    // 被控端的上行要留着读：控制器现在要等它的 `accepted` 才建立数据面。
    let (ctld_tx, mut ctld_upstream) = mpsc::channel(64);
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
    relay_acceptance(&mut ctld_upstream, &ctrl).await;

    // 画面仍应经真实 hbbr 中继到达（说明早先的推送被 hbbs 收下并缓存，未被丢弃）。
    tokio::time::timeout(WAIT, frame_rx.recv())
        .await
        .expect("等画面超时——中继请求很可能在目标登记前被丢弃")
        .expect("画面通道关闭");
    assert!(ctrl_sink.frames.load(Ordering::Relaxed) >= 1, "应收到画面");
}

// ── D 部分：真实控制通道上的「被控端拒绝」路径 ────────────────────────

/// 轮询等待条件成立（有界；事件驱动，不用固定 sleep 蒙）。超时后返回条件的最终值。
async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

/// 在 `budget` 内轮询等待条件成立；超时返回条件的最终值。
///
/// 与 [`wait_for`]（上限 10s）分开，是为了让「延迟读数必须**很快**出现」这类断言真的能失败：
/// 它的上限必须明显小于 `HEARTBEAT_INTERVAL`（10s），否则把心跳起点写成「延迟一个周期到期」也
/// 照样通过——断言就锁不住「首个 tick 立即到期」这条不变量。
async fn wait_within(budget: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 起一个直接挂在**真实控制通道**上的 [`RemoteManager`]（桥接就是 manager 本身）。
fn control_channel_manager(server_host: &str, name: &str, enabled: bool) -> Arc<RemoteManager> {
    RemoteManager::new(RemoteManagerConfig {
        server_host: server_host.to_owned(),
        self_client_name: name.to_owned(),
        platform: "linux".to_owned(),
        displays: 1,
        enabled,
        // 本用例不建立数据面（被拒），用延迟建立器即可——只有真的走到 `establish` 才会构造传输。
        establisher: Arc::new(LazyTransport::new(server_host.to_owned())),
        encode: EncodeOptions {
            quality: 60,
            scale: 1.0,
            max_fps: 10,
        },
        share_dir: None,
        download_dir: std::env::temp_dir(),
        // 被控端用默认的 `Confirm`（本用例要测拒绝路径）；控制器侧策略无关紧要。
        approval_policy: if enabled { ApprovalPolicy::Confirm } else { ApprovalPolicy::Auto },
    })
}

/// P0-2：真实控制通道 + 两个真实客户端，被控端 `Confirm` 且用户**拒绝**——控制器必须收敛到
/// 「无会话 + 有错误」，且不建立任何数据面会话。
///
/// 锁的不变量：接入确认门控在**真实控制通道**上端到端成立——拒绝经服务端转达后，控制器
/// 停在会话之前而不是「卡在连接中」。现有 A/B/C 三部只走 `Auto` 策略，拒绝路径完全没跑过。
/// 破坏它会怎样：用户点「拒绝」而控制器仍在后台建数据面，或 UI 永远停在「连接中」。
#[tokio::test]
async fn control_channel_reject_leaves_controller_without_session() {
    let mut harness = TestHarness::spawn(HarnessOpts::default()).await;
    let ports = alloc_rustdesk_ports();
    // 开 RustDesk 才有远程能力；本用例不建立数据面（被拒），故 relay 参数无关紧要。
    let _ = enable(&harness.api_client(), &ports, false).await;

    let server_host = harness.control_addr.to_string();

    let ctrl = control_channel_manager(&server_host, "ctrl", false);
    let target = control_channel_manager(&server_host, "target", true);
    // 被控端需装平台实现，否则 `handle_incoming` 会直接以「缺屏幕/输入」拒绝、走不到弹窗。
    target.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));
    let (ctrl_sink, _frame_rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());

    // 两个真实客户端，桥接即各自的 RemoteManager——控制通道消息直达引擎。
    let _ctrl_client = harness.spawn_remote_client(Some("ctrl"), ctrl.bridge());
    let _target_client = harness.spawn_remote_client(Some("target"), target.bridge());

    // 服务端下发 RemoteCapability 后 manager 才有 capability / rd_id。
    assert!(wait_for(|| ctrl.snapshot().capability).await, "控制器应收到能力通告");
    assert!(wait_for(|| target.snapshot().capability).await, "被控端应收到能力通告");

    // 被控端 enabled=true 会随能力自动上报 → 控制器的可远程列表应出现 target。
    assert!(
        wait_for(|| ctrl.snapshot().peers.iter().any(|p| p.client_name == "target")).await,
        "控制器应看到目标对端，实得 {:?}",
        ctrl.snapshot().peers
    );

    // 控制器发起连接 → 服务端签发会话 → 被控端进入待确认（Confirm + 未信任）。
    ctrl.request_connect("target", RemoteMode::Control);
    assert!(
        wait_for(|| target.snapshot().pending_approval.is_some()).await,
        "被控端应弹出待确认请求"
    );

    // 用户拒绝。
    target.reject_pending(None);

    // 控制器最终应进入「无会话 + 有错误信息」，且错误来自**拒绝**而非数据面失败
    // （若它错误地去建立数据面，这里会是信令/超时错误，而不是「用户拒绝」）。
    assert!(
        wait_for(|| ctrl.snapshot().active.is_none() && ctrl.snapshot().last_error.is_some()).await,
        "控制器应收敛到无会话 + 有错误，实得 {:?}",
        ctrl.snapshot()
    );
    let err = ctrl.snapshot().last_error.expect("应有错误信息");
    assert!(err.contains("拒绝"), "错误应来自被控端拒绝，实得 {err:?}");
    // 没有建立数据面会话：控制器从未收到任何画面帧。
    assert_eq!(ctrl_sink.frames.load(Ordering::Relaxed), 0, "拒绝后不应有任何画面帧");
    assert!(target.snapshot().active.is_none(), "被控端不应有活动会话");
}
