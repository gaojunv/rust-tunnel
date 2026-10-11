//! 会话循环与建立编排的端到端测试（内存 transport 替身，无真实网络）。
//!
//! 这些测试放在 crate 内部（而非 `tests/`）以便复用 lib.rs 头部的 `cfg_attr(test, allow(...))`
//! 豁免，并直接访问私有辅助。

use crate::auth::PendingSessions;
use crate::controller::{run_controller, ControllerConfig};
use crate::controlled::{run_controlled, ControlledConfig};
use crate::error::{RemoteError, RemoteResult};
use crate::manager::{RemoteManager, RemoteManagerConfig};
use crate::screen::{
    DecodedFrame, DisplayInfo, EncodeOptions, FrameSink, InputEvent, InputSink, KeyEvent, MouseButton, RawFrame,
    ScreenSource,
};
use crate::session::plan_controller_handshake;
use crate::transport::{BoxDuplex, EstablishRequest, Role, TransportEstablisher};
use async_trait::async_trait;
use rust_tunnel_common::protocol::{ControlMessage, RemoteMode, RemoteParams};
use rust_tunnel_common::remote_bridge::RemoteBridge as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 单条等待的上限，避免测试挂死。
const WAIT: Duration = Duration::from_secs(5);

/// 造一帧纯色 RGBA。
fn solid(display: u32, w: u32, h: u32, color: [u8; 4]) -> RawFrame {
    let mut rgba = Vec::with_capacity((w * h) as usize * 4);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&color);
    }
    RawFrame {
        display,
        width: w,
        height: h,
        rgba,
    }
}

/// 固定内容的采集源。
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
        Ok(solid(display, 32, 24, [40, 80, 120, 255]))
    }
}

/// 记录收到的输入事件。
#[derive(Default)]
struct RecordingInput {
    events: Mutex<Vec<InputEvent>>,
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

impl RecordingInput {
    fn record(&self, event: InputEvent) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.events.lock() {
            guard.push(event.clone());
        }
        if let Ok(guard) = self.notify.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(event);
            }
        }
    }
}

/// 记录画面上报的 sink。
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

/// 一次性建立器：取出预置的流；控制器角色会把 `early`（Hello）写进去。
struct OneShotEstablisher {
    slot: Mutex<Option<BoxDuplex>>,
    /// 是否模拟「建立阶段已流水线写出 Hello」。
    write_early: bool,
    calls: AtomicUsize,
}

impl OneShotEstablisher {
    fn new(duplex: BoxDuplex, write_early: bool) -> Self {
        Self {
            slot: Mutex::new(Some(duplex)),
            write_early,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl TransportEstablisher for OneShotEstablisher {
    async fn establish(&self, request: EstablishRequest, _role: Role) -> RemoteResult<BoxDuplex> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut duplex = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| RemoteError::Signaling("已无可用流".to_owned()))?;
        if self.write_early && !request.early.is_empty() {
            duplex.write_all(&request.early).await?;
            duplex.flush().await?;
        }
        Ok(duplex)
    }
}

/// 永不成功的建立器（只测状态机）。
struct FailingEstablisher;

#[async_trait]
impl TransportEstablisher for FailingEstablisher {
    async fn establish(&self, _request: EstablishRequest, _role: Role) -> RemoteResult<BoxDuplex> {
        Err(RemoteError::Signaling("测试失败".to_owned()))
    }
}

fn params() -> RemoteParams {
    RemoteParams {
        rendezvous_host: String::new(),
        rendezvous_port: 21116,
        relay_port: 21117,
        relay_server: String::new(),
        public_key: "server-pub".into(),
        always_use_relay: false,
    }
}

fn manager(establisher: Arc<dyn TransportEstablisher>, enabled: bool, name: &str) -> Arc<RemoteManager> {
    RemoteManager::new(RemoteManagerConfig {
        server_host: "tunnel.example:8080".to_owned(),
        self_client_name: name.to_owned(),
        platform: "linux".to_owned(),
        displays: 1,
        enabled,
        establisher,
        encode: EncodeOptions {
            quality: 60,
            scale: 1.0,
        },
    })
}

#[tokio::test]
async fn full_session_between_two_managers() {
    // 一对内存流：控制器拿 a，被控端拿 b。
    let (a, b) = tokio::io::duplex(128 * 1024);
    let ctrl_est = Arc::new(OneShotEstablisher::new(Box::new(a), true));
    let ctld_est = Arc::new(OneShotEstablisher::new(Box::new(b), false));

    let ctrl = manager(ctrl_est, false, "home-nas");
    let ctld = manager(ctld_est, true, "office-pc");

    let (ctrl_sink, mut frame_rx) = RecordingSink::with_notify();
    let (ctld_input, mut input_rx) = RecordingInput::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    ctld.install_platform(Arc::new(FakeScreen), ctld_input.clone());

    // 双方先拿到能力通告（设置 params / rd_id）。
    ctrl.handle_control_message(ControlMessage::RemoteCapability {
        params: Some(params()),
        rd_id: "111111111".to_owned(),
    });
    ctld.handle_control_message(ControlMessage::RemoteCapability {
        params: Some(params()),
        rd_id: "222222222".to_owned(),
    });

    let (ctrl_tx, mut ctrl_rx) = mpsc::channel(64);
    let (ctld_tx, mut ctld_rx) = mpsc::channel(64);
    ctrl.attach(ctrl_tx);
    ctld.attach(ctld_tx);
    let _ = ctrl_rx.try_recv();
    let _ = ctld_rx.try_recv();

    // 服务端：给控制器授权 + 给被控端转达请求（同一 session_id 与 token）。
    ctrl.handle_control_message(ControlMessage::RemoteConnectGrant {
        session_id: "sess-1".to_owned(),
        token: "tok-1".to_owned(),
        target_rd_id: "222222222".to_owned(),
        target_platform: "linux".to_owned(),
        params: params(),
    });
    ctld.handle_control_message(ControlMessage::RemoteSessionIncoming {
        session_id: "sess-1".to_owned(),
        token: "tok-1".to_owned(),
        from_name: "home-nas".to_owned(),
        mode: RemoteMode::Control,
    });

    // 被控端应回 accepted:true。
    let response = tokio::time::timeout(WAIT, ctld_rx.recv())
        .await
        .expect("等被控端应答超时")
        .expect("通道关闭");
    match response {
        ControlMessage::RemoteSessionResponse { accepted, .. } => assert!(accepted, "应接受会话"),
        other => panic!("应为 RemoteSessionResponse，实得 {other:?}"),
    }

    // 控制器应收到显示器列表与至少一帧画面。
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

    // 一键中断：两端 active 都应清空。
    ctrl.abort_active();
    for _ in 0..50 {
        if ctrl.snapshot().active.is_none() && ctld.snapshot().active.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ctrl.snapshot().active.is_none(), "控制器 active 应清空");
}

#[tokio::test]
async fn wrong_token_is_rejected_by_controlled() {
    let (a, b) = tokio::io::duplex(64 * 1024);
    let plan = plan_controller_handshake("sess-1", "GOOD-TOKEN", "111111111").unwrap();

    // 被控端登记的 token 与控制器使用的不一致。
    let pending = Arc::new(PendingSessions::new());
    pending.insert(crate::auth::PendingSession::new(
        "sess-1".to_owned(),
        "OTHER-TOKEN".to_owned(),
        "home-nas".to_owned(),
        RemoteMode::Control,
    ));

    // 控制器先写出 Hello（模拟流水线）。
    let mut ctrl_stream: BoxDuplex = Box::new(a);
    ctrl_stream.write_all(&plan.hello_frame).await.unwrap();
    ctrl_stream.flush().await.unwrap();

    let ctld_cfg = ControlledConfig {
        session_id: "sess-1".to_owned(),
        local_rd_id: "222222222".to_owned(),
        mode: RemoteMode::Control,
        pending,
        encode: EncodeOptions::default(),
    };
    let ctrl_cfg = ControllerConfig {
        session_id: "sess-1".to_owned(),
        token: "tok-1".to_owned(),
        local_rd_id: "111111111".to_owned(),
        mode: RemoteMode::Control,
    };

    let (_input_tx, input_rx) = mpsc::channel(4);
    let (sink, _sink_rx) = RecordingSink::with_notify();

    let ctld_task = tokio::spawn(run_controlled(
        Box::new(b),
        ctld_cfg,
        Arc::new(FakeScreen),
        Arc::new(RecordingInput::default()),
        CancellationToken::new(),
    ));
    let ctrl_task = tokio::spawn(run_controller(
        ctrl_stream,
        ctrl_cfg,
        plan,
        input_rx,
        sink,
        CancellationToken::new(),
    ));

    let ctrl_result = tokio::time::timeout(WAIT, ctrl_task)
        .await
        .expect("控制器超时")
        .expect("控制器 panic");
    let ctld_result = tokio::time::timeout(WAIT, ctld_task)
        .await
        .expect("被控端超时")
        .expect("被控端 panic");

    assert!(matches!(ctrl_result, Err(RemoteError::Handshake(_))), "控制器应被拒");
    assert!(matches!(ctld_result, Err(RemoteError::Auth(_))), "被控端鉴权应失败");
}

#[tokio::test]
async fn correct_token_completes_handshake_and_streams() {
    let (a, b) = tokio::io::duplex(128 * 1024);
    let plan = plan_controller_handshake("sess-2", "TOK", "111111111").unwrap();
    let pending = Arc::new(PendingSessions::new());
    pending.insert(crate::auth::PendingSession::new(
        "sess-2".to_owned(),
        "TOK".to_owned(),
        "home-nas".to_owned(),
        RemoteMode::ViewOnly,
    ));

    let mut ctrl_stream: BoxDuplex = Box::new(a);
    ctrl_stream.write_all(&plan.hello_frame).await.unwrap();
    ctrl_stream.flush().await.unwrap();

    let (sink, mut frame_rx) = RecordingSink::with_notify();
    let cancel = CancellationToken::new();

    let ctld_task = tokio::spawn(run_controlled(
        Box::new(b),
        ControlledConfig {
            session_id: "sess-2".to_owned(),
            local_rd_id: "222222222".to_owned(),
            mode: RemoteMode::ViewOnly,
            pending,
            encode: EncodeOptions::default(),
        },
        Arc::new(FakeScreen),
        Arc::new(RecordingInput::default()),
        cancel.clone(),
    ));
    let (_input_tx, input_rx) = mpsc::channel(4);
    let ctrl_task = tokio::spawn(run_controller(
        ctrl_stream,
        ControllerConfig {
            session_id: "sess-2".to_owned(),
            token: "TOK".to_owned(),
            local_rd_id: "111111111".to_owned(),
            mode: RemoteMode::ViewOnly,
        },
        plan,
        input_rx,
        sink.clone(),
        cancel.clone(),
    ));

    // 应至少收到一帧。
    tokio::time::timeout(WAIT, frame_rx.recv())
        .await
        .expect("等画面超时")
        .expect("通道关闭");
    assert!(sink.frames.load(Ordering::Relaxed) >= 1);

    // 取消 → 两端正常收尾。
    cancel.cancel();
    let ctrl_result = tokio::time::timeout(WAIT, ctrl_task).await.unwrap().unwrap();
    let _ = tokio::time::timeout(WAIT, ctld_task).await.unwrap().unwrap();
    assert!(ctrl_result.is_ok(), "取消后控制器应正常结束");
    assert!(sink.closed.lock().unwrap().is_some(), "应触发 on_closed");
}

#[tokio::test]
async fn establish_failure_surfaces_as_session_error() {
    let mgr = manager(Arc::new(FailingEstablisher), false, "home-nas");
    let (tx, mut rx) = mpsc::channel(64);
    mgr.attach(tx);
    mgr.handle_control_message(ControlMessage::RemoteCapability {
        params: Some(params()),
        rd_id: "111111111".to_owned(),
    });
    let _ = rx.try_recv();

    mgr.handle_control_message(ControlMessage::RemoteConnectGrant {
        session_id: "sess-x".to_owned(),
        token: "t".to_owned(),
        target_rd_id: "222222222".to_owned(),
        target_platform: "linux".to_owned(),
        params: params(),
    });

    // 建立失败 → 会话结束，last_error 有值。
    for _ in 0..100 {
        if mgr.snapshot().last_error.is_some() && mgr.snapshot().active.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let snap = mgr.snapshot();
    assert!(snap.active.is_none(), "失败后 active 应清空");
    assert!(snap.last_error.is_some(), "应记录错误");
}

/// 中继方 MITM 回归：把 `HelloAck.x25519_pub` 换成自己的、但无法重算 MAC。
/// 控制器必须**校验** `hmac_ack` 并拒绝，而不是拿被替换的公钥去派生密钥。
#[tokio::test]
async fn tampered_hello_ack_pubkey_is_rejected() {
    use crate::auth::ack_mac;
    use crate::session::{SessionCodec, SessionFrame, SessionMessage};
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_util::codec::{FramedRead, FramedWrite};

    let (a, b) = tokio::io::duplex(64 * 1024);
    let token = "TOK";
    let plan = plan_controller_handshake("sess-1", token, "111111111").unwrap();

    let mut ctrl_stream: BoxDuplex = Box::new(a);
    ctrl_stream.write_all(&plan.hello_frame).await.unwrap();
    ctrl_stream.flush().await.unwrap();

    // 攻击者：读 Hello 拿到 initiator_pub，回一个 x25519_pub 被替换、
    // 但 MAC 仍对应「真被控端公钥」的 HelloAck（它不知道真私钥，也没法用替换后的公钥重算 MAC）。
    let attacker = tokio::spawn(async move {
        let (rd, wr) = tokio::io::split(b);
        let mut reader = FramedRead::new(rd, SessionCodec::new());
        let mut writer = FramedWrite::new(wr, SessionCodec::new());

        let hello = reader.next().await.unwrap().unwrap();
        let initiator_pub = match hello.message {
            SessionMessage::Hello { x25519_pub, .. } => x25519_pub,
            other => panic!("期望 Hello，实得 {other:?}"),
        };
        let real_responder_pub = vec![3u8; 32];
        let mac = ack_mac(token, "sess-1", &real_responder_pub, &initiator_pub);
        // 替换成攻击者自己的临时公钥，MAC 保持不变 → 控制器应识破。
        writer
            .send(SessionFrame::control(SessionMessage::HelloAck {
                accepted: true,
                reason: None,
                displays: Vec::new(),
                x25519_pub: vec![4u8; 32],
                hmac_ack: mac,
            }))
            .await
            .unwrap();
    });

    let (_input_tx, input_rx) = mpsc::channel(4);
    let (sink, _sink_rx) = RecordingSink::with_notify();
    let result = run_controller(
        ctrl_stream,
        ControllerConfig {
            session_id: "sess-1".to_owned(),
            token: token.to_owned(),
            local_rd_id: "111111111".to_owned(),
            mode: RemoteMode::Control,
        },
        plan,
        input_rx,
        sink,
        CancellationToken::new(),
    )
    .await;

    let _ = tokio::time::timeout(WAIT, attacker).await;
    match result {
        Err(RemoteError::Handshake(msg)) => {
            assert!(msg.contains("HelloAck"), "错误应点名 HelloAck 认证失败，实得 {msg}");
        }
        other => panic!("被替换公钥的 HelloAck 必须被拒，实得 {other:?}"),
    }
}
