//! 会话循环与建立编排的端到端测试（内存 transport 替身，无真实网络）。
//!
//! 这些测试放在 crate 内部（而非 `tests/`）以便复用 lib.rs 头部的 `cfg_attr(test, allow(...))`
//! 豁免，并直接访问私有辅助。

use crate::auth::PendingSessions;
use crate::controller::{run_controller, ControllerConfig};
use crate::controlled::{run_controlled, ControlledConfig};
use crate::transfer::{TransferEvent, TransferHub};
use crate::error::{RemoteError, RemoteResult};
use crate::manager::{RemoteManager, RemoteManagerConfig};
use crate::screen::{
    Clipboard, DecodedFrame, DisplayInfo, EncodeOptions, FrameSink, InputEvent, InputSink, KeyEvent, MouseButton,
    RawFrame, ScreenSource, TileUpdate,
};
use crate::session::plan_controller_handshake;
use crate::transport::{BoxDuplex, EstablishRequest, Role, TransportEstablisher};
use async_trait::async_trait;
use rust_tunnel_common::protocol::{ControlMessage, RemoteMode, RemoteParams};
use rust_tunnel_common::remote_bridge::RemoteBridge as _;
use std::path::PathBuf;
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

/// 把帧内子矩形填成指定颜色（必须落在帧内）。用于制造「局部变化」。
fn fill_rect(frame: &mut RawFrame, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
    let stride = frame.width as usize * 4;
    for yy in y..y + h {
        for xx in x..x + w {
            let off = yy as usize * stride + xx as usize * 4;
            frame.rgba[off..off + 4].copy_from_slice(&color);
        }
    }
}

/// 造一段确定性的伪随机字节（LCG；不引额外依赖，便于逐字节比对）。
fn pseudo_bytes(len: usize) -> Vec<u8> {
    let mut x: u32 = 0x1234_5678;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push(u8::try_from(x >> 24).unwrap_or(0));
    }
    out
}

/// 轮询等待条件成立（有界；超时后返回条件的最终值）。
async fn poll_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..250 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
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
    /// 收到的瓦片更新批次数。
    tiles: AtomicUsize,
    /// 收到的瓦片更新（供断言矩形与尺寸）。
    tile_updates: Mutex<Vec<TileUpdate>>,
    /// 最近一次收到的剪贴板文本。
    clipboard: Mutex<Option<String>>,
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
    fn on_tiles(&self, update: TileUpdate) {
        self.tiles.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.tile_updates.lock() {
            guard.push(update);
        }
    }
    fn on_clipboard(&self, text: String) {
        if let Ok(mut guard) = self.clipboard.lock() {
            *guard = Some(text);
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
    manager_with_dirs(establisher, enabled, name, None, std::env::temp_dir())
}

/// 与 [`manager`] 同源，但可指定共享目录与下载目录（文件传输用例用）。
fn manager_with_dirs(
    establisher: Arc<dyn TransportEstablisher>,
    enabled: bool,
    name: &str,
    share_dir: Option<PathBuf>,
    download_dir: PathBuf,
) -> Arc<RemoteManager> {
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
            max_fps: 10,
        },
        share_dir,
        download_dir,
    })
}

/// 采集源：`capture` 返回共享缓冲里的当前帧，测试可中途改它（制造局部变化）。
struct MutableScreen {
    frame: Mutex<RawFrame>,
}

impl MutableScreen {
    fn new(frame: RawFrame) -> Self {
        Self {
            frame: Mutex::new(frame),
        }
    }
}

impl ScreenSource for MutableScreen {
    fn displays(&self) -> Vec<DisplayInfo> {
        vec![DisplayInfo {
            id: 0,
            width: 192,
            height: 128,
            name: "fake".into(),
            primary: true,
        }]
    }

    fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
        let guard = self.frame.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut frame = guard.clone();
        frame.display = display;
        Ok(frame)
    }
}

/// 双显示器采集源：记录每次 `capture` 收到的显示器编号，供切换断言。
#[derive(Default)]
struct TwoDisplayScreen {
    captured: Mutex<Vec<u32>>,
}

impl ScreenSource for TwoDisplayScreen {
    fn displays(&self) -> Vec<DisplayInfo> {
        vec![
            DisplayInfo {
                id: 0,
                width: 64,
                height: 64,
                name: "left".into(),
                primary: true,
            },
            DisplayInfo {
                id: 1,
                width: 96,
                height: 48,
                name: "right".into(),
                primary: false,
            },
        ]
    }

    fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(display);
        }
        let (w, h) = if display == 1 { (96, 48) } else { (64, 64) };
        Ok(solid(display, w, h, [50, 60, 70, 255]))
    }
}

/// 内存剪贴板替身（被控端侧）。
struct FakeClipboard {
    value: Mutex<String>,
}

impl FakeClipboard {
    fn new(initial: &str) -> Self {
        Self {
            value: Mutex::new(initial.to_owned()),
        }
    }

    fn current(&self) -> String {
        self.value.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

impl Clipboard for FakeClipboard {
    fn get(&self) -> RemoteResult<String> {
        Ok(self.current())
    }

    fn set(&self, text: &str) -> RemoteResult<()> {
        *self.value.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = text.to_owned();
        Ok(())
    }
}

/// 走完整控制面把两个管理器接成一条会话（与 `full_session_between_two_managers` 同序）。
///
/// 两端需已装好平台实现 / 画面上报目标；被控端需 `enabled: true`。
async fn establish_session(ctrl: &Arc<RemoteManager>, ctld: &Arc<RemoteManager>) {
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

    // 被控端应回 accepted:true（此后两端各自异步建立数据面）。
    let response = tokio::time::timeout(WAIT, ctld_rx.recv())
        .await
        .expect("等被控端应答超时")
        .expect("通道关闭");
    match response {
        ControlMessage::RemoteSessionResponse { accepted, .. } => assert!(accepted, "应接受会话"),
        other => panic!("应为 RemoteSessionResponse，实得 {other:?}"),
    }
}

/// 造一个空闲的传输状态机（这些用例不测文件传输）。
///
/// 返回事件接收端：调用方持有时可断言传输事件，丢弃时 hub 只是记一条 debug 日志。
fn transfer_hub() -> (TransferHub, mpsc::UnboundedReceiver<TransferEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (TransferHub::new(None, std::env::temp_dir(), tx), rx)
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
        None,
        transfer_hub().0,
        CancellationToken::new(),
    ));
    let ctrl_task = tokio::spawn(run_controller(
        ctrl_stream,
        ctrl_cfg,
        plan,
        input_rx,
        transfer_hub().0,
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
        None,
        transfer_hub().0,
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
        transfer_hub().0,
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
        transfer_hub().0,
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

/// 瓦片差分：静止画面零瓦片输出，局部变化才产生瓦片。
#[tokio::test]
async fn static_screen_emits_no_tiles_and_change_emits_tiles() {
    let (a, b) = tokio::io::duplex(256 * 1024);
    let ctrl = manager(Arc::new(OneShotEstablisher::new(Box::new(a), true)), false, "home-nas");
    let ctld = manager(Arc::new(OneShotEstablisher::new(Box::new(b), false)), true, "office-pc");

    let (ctrl_sink, _frame_rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    // 192x128 → 3x2 = 6 块瓦片；先给一张纯色静止帧。
    let screen = Arc::new(MutableScreen::new(solid(0, 192, 128, [10, 20, 30, 255])));
    ctld.install_platform(screen.clone(), Arc::new(RecordingInput::default()));

    establish_session(&ctrl, &ctld).await;

    // 首帧是关键帧（整帧 Video），不是瓦片。
    assert!(
        poll_until(|| ctrl_sink.frames.load(Ordering::Relaxed) >= 1).await,
        "应收到首帧关键帧"
    );

    // 静止画面：再等一段，瓦片输出必须为零。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(ctrl_sink.tiles.load(Ordering::Relaxed), 0, "静止画面不应有瓦片输出");

    // 只改左上角瓦片 (0,0) 内的一小块区域。
    {
        let mut frame = screen.frame.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        fill_rect(&mut frame, 10, 10, 20, 20, [250, 10, 10, 255]);
    }

    // 变化应产生至少一次瓦片更新。
    assert!(
        poll_until(|| ctrl_sink.tiles.load(Ordering::Relaxed) >= 1).await,
        "画面变化应产生瓦片"
    );
    let updates = ctrl_sink.tile_updates.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let update = updates.last().expect("应有瓦片更新");
    assert_eq!((update.display, update.width, update.height), (0, 192, 128), "瓦片应带整帧尺寸");
    assert!(
        update
            .tiles
            .iter()
            .any(|t| (t.x, t.y, t.width, t.height) == (0, 0, 64, 64)),
        "应含左上角 64x64 瓦片，实得 {:?}",
        update.tiles.iter().map(|t| (t.x, t.y, t.width, t.height)).collect::<Vec<_>>()
    );
}

/// 多显示器切换：控制器 `select_display` 后，被控端应改采目标显示器。
#[tokio::test]
async fn select_display_switches_controlled_capture_target() {
    let (a, b) = tokio::io::duplex(256 * 1024);
    let ctrl = manager(Arc::new(OneShotEstablisher::new(Box::new(a), true)), false, "home-nas");
    let ctld = manager(Arc::new(OneShotEstablisher::new(Box::new(b), false)), true, "office-pc");

    let (ctrl_sink, _rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    let screen = Arc::new(TwoDisplayScreen::default());
    ctld.install_platform(screen.clone(), Arc::new(RecordingInput::default()));

    establish_session(&ctrl, &ctld).await;

    // 初始应采集主显示器（id 0）。
    assert!(
        poll_until(|| screen
            .captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&0))
        .await,
        "初始应采集显示器 0"
    );

    // 切到显示器 1，被控端采集目标应随之改变。
    ctrl.select_display(1);
    assert!(
        poll_until(|| screen
            .captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&1))
        .await,
        "切换后应采集显示器 1"
    );
}

/// 文件传输：完整 manager 通路上的下载与上传（含多块）。
#[tokio::test]
async fn file_transfer_download_and_upload() {
    let pid = std::process::id();
    let share = std::env::temp_dir().join(format!("rt-xfer-share-{pid}"));
    let dl = std::env::temp_dir().join(format!("rt-xfer-dl-{pid}"));
    let _ = std::fs::remove_dir_all(&share);
    let _ = std::fs::remove_dir_all(&dl);
    std::fs::create_dir_all(&share).unwrap();
    std::fs::create_dir_all(&dl).unwrap();

    // 源文件大于单块（CHUNK_SIZE = 64 KiB），保证多块流动。
    let payload = pseudo_bytes(200 * 1024);
    std::fs::write(share.join("source.bin"), &payload).unwrap();

    let (a, b) = tokio::io::duplex(256 * 1024);
    let ctrl = manager_with_dirs(
        Arc::new(OneShotEstablisher::new(Box::new(a), true)),
        false,
        "home-nas",
        None,
        dl.clone(),
    );
    let ctld = manager_with_dirs(
        Arc::new(OneShotEstablisher::new(Box::new(b), false)),
        true,
        "office-pc",
        Some(share.clone()),
        dl.clone(),
    );

    let (ctrl_sink, _rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    ctld.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));

    establish_session(&ctrl, &ctld).await;

    // 下载：先列远端目录。
    ctrl.list_remote_dir("/");
    assert!(
        poll_until(|| ctrl
            .snapshot()
            .remote_entries
            .iter()
            .any(|e| e.name == "source.bin"))
        .await,
        "应列出 source.bin"
    );
    let entry = ctrl
        .snapshot()
        .remote_entries
        .into_iter()
        .find(|e| e.name == "source.bin")
        .expect("应含 source.bin");
    assert!(!entry.is_dir, "应为普通文件");
    assert_eq!(entry.size, payload.len() as u64, "应回报正确大小");

    ctrl.download_remote_file("/source.bin");
    let downloaded = dl.join("source.bin");
    assert!(
        poll_until(|| std::fs::read(&downloaded).is_ok_and(|b| b == payload)).await,
        "下载文件内容应与源一致"
    );
    // 传输结束后不应有在途项，且留下结果提示。
    assert!(
        poll_until(|| {
            let snap = ctrl.snapshot();
            snap.transfers.is_empty() && snap.transfer_note.is_some()
        })
        .await,
        "传输应结束并留下提示"
    );

    // 上传：本地文件 → 被控端共享目录。
    let up_payload = pseudo_bytes(150 * 1024);
    let local = dl.join("upload-src.bin");
    std::fs::write(&local, &up_payload).unwrap();
    ctrl.upload_local_file(local, "/up.bin");
    let uploaded = share.join("up.bin");
    assert!(
        poll_until(|| std::fs::read(&uploaded).is_ok_and(|b| b == up_payload)).await,
        "上传文件内容应与本地一致"
    );

    let _ = std::fs::remove_dir_all(&share);
    let _ = std::fs::remove_dir_all(&dl);
}

/// 剪贴板：控制器请求 → 收被控端文本；控制器下发 → 被控端剪贴板被写入。
#[tokio::test]
async fn clipboard_request_and_send_roundtrip() {
    let (a, b) = tokio::io::duplex(128 * 1024);
    let ctrl = manager(Arc::new(OneShotEstablisher::new(Box::new(a), true)), false, "home-nas");
    let ctld = manager(Arc::new(OneShotEstablisher::new(Box::new(b), false)), true, "office-pc");

    let (ctrl_sink, _rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    let clipboard = Arc::new(FakeClipboard::new("from-controlled"));
    ctld.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));
    ctld.install_clipboard(clipboard.clone());

    establish_session(&ctrl, &ctld).await;

    // 控制器请求剪贴板 → 应收到被控端的文本。
    ctrl.request_clipboard();
    assert!(
        poll_until(|| ctrl_sink
            .clipboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref()
            == Some("from-controlled"))
        .await,
        "应收到被控端剪贴板文本"
    );

    // 控制器下发文本 → 被控端剪贴板应被写入。
    ctrl.send_clipboard("from-controller".into());
    assert!(
        poll_until(|| clipboard.current() == "from-controller").await,
        "被控端剪贴板应被写入"
    );
}

