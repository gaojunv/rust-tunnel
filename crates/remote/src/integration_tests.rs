//! 会话循环与建立编排的端到端测试（内存 transport 替身，无真实网络）。
//!
//! 这些测试放在 crate 内部（而非 `tests/`）以便复用 lib.rs 头部的 `cfg_attr(test, allow(...))`
//! 豁免，并直接访问私有辅助。

use crate::approval::ApprovalPolicy;
use crate::auth::PendingSessions;
use crate::controller::{run_controller, ControllerConfig};
use crate::controlled::{run_controlled, ControlledConfig, CAPTURE_WATCHDOG};
use crate::transfer::{TransferEvent, TransferHub};
use crate::error::{RemoteError, RemoteResult};
use crate::manager::{RemoteManager, RemoteManagerConfig, SessionState};
use crate::screen::{
    Clipboard, DecodedFrame, DisplayInfo, EncodeOptions, FrameSink, InputEvent, InputSink, KeyEvent, MouseButton,
    RawFrame, ScreenSource,
};
use crate::session::{plan_controller_handshake, SessionCodec, SessionEnd, SessionMessage};
use crate::stats::SessionStatsReporter;
use crate::transport::{BoxDuplex, EstablishedStream, EstablishRequest, Role, TransportEstablisher, TransportPath};
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
    /// 上报给管理器的路径（内存替身默认取保守值 [`TransportPath::Relay`]）。
    path: TransportPath,
}

impl OneShotEstablisher {
    fn new(duplex: BoxDuplex, write_early: bool) -> Self {
        Self {
            slot: Mutex::new(Some(duplex)),
            write_early,
            calls: AtomicUsize::new(0),
            path: TransportPath::Relay,
        }
    }

    /// 同 [`Self::new`]，但指定上报的路径（用于断言路径透传到 UI 状态）。
    fn with_path(duplex: BoxDuplex, write_early: bool, path: TransportPath) -> Self {
        Self {
            path,
            ..Self::new(duplex, write_early)
        }
    }
}

#[async_trait]
impl TransportEstablisher for OneShotEstablisher {
    async fn establish(&self, request: EstablishRequest, _role: Role) -> RemoteResult<EstablishedStream> {
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
        Ok(EstablishedStream {
            stream: duplex,
            path: self.path,
        })
    }
}

/// 永不成功的建立器（只测状态机）。
struct FailingEstablisher;

#[async_trait]
impl TransportEstablisher for FailingEstablisher {
    async fn establish(&self, _request: EstablishRequest, _role: Role) -> RemoteResult<EstablishedStream> {
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
            target_bitrate_kbps: 2000,
            scale: 1.0,
            max_fps: 10,
        },
        share_dir,
        download_dir,
        // 既有用例都按「直接接受」跑；确认门控的新用例各自 `set_approval_policy(Confirm)`。
        approval_policy: ApprovalPolicy::Auto,
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
    establish_session_with_mode(ctrl, ctld, RemoteMode::Control, RemoteMode::Control).await;
}

/// 与 [`establish_session`] 同源，但可指定**两端各自的会话模式**。
///
/// 模式决定被控端的权限边界（采集 / 注入 / 文件服务，见 `manager.rs::controlled_task`），
/// 因此 P0-3 的 `ViewOnly` / `FileTransfer` 用例要能把它灌进来。
async fn establish_session_with_mode(
    ctrl: &Arc<RemoteManager>,
    ctld: &Arc<RemoteManager>,
    ctrl_mode: RemoteMode,
    ctld_mode: RemoteMode,
) {
    // 双方先拿到能力通告（设置 params / rd_id）。
    ctrl.handle_control_message(ControlMessage::RemoteCapability {
        params: Some(params()),
        rd_id: "111111111".to_owned(),
    });
    ctld.handle_control_message(ControlMessage::RemoteCapability {
        params: Some(params()),
        rd_id: "222222222".to_owned(),
    });
    // 控制器记录本次请求模式：`RemoteConnectGrant` 只带 `target_rd_id`、不带模式，
    // 控制器侧的模式是从这里回查的（见 `start_controller`）。
    ctrl.request_connect("office-pc", ctrl_mode);

    let (ctrl_tx, mut ctrl_rx) = mpsc::channel(64);
    let (ctld_tx, mut ctld_rx) = mpsc::channel(64);
    ctrl.attach(ctrl_tx);
    ctld.attach(ctld_tx);
    // `attach` 会补上报一次 `RemoteAnnounce`；先排空这类旁路消息。
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
        mode: ctld_mode,
    });

    // 被控端应回 accepted:true（此后两端各自异步建立数据面）。
    // 用 `next_response` 跳过任何旁路消息（如额外的 `RemoteAnnounce`），只取真正的应答。
    let response = next_response(&mut ctld_rx).await;
    match &response {
        ControlMessage::RemoteSessionResponse { accepted, .. } => assert!(accepted, "应接受会话"),
        other => panic!("应为 RemoteSessionResponse，实得 {other:?}"),
    }
    // 服务端会把该响应无条件转达给控制器（新门控下控制器要等它才建立数据面）。
    ctrl.handle_control_message(response);
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
    match &response {
        ControlMessage::RemoteSessionResponse { accepted, .. } => assert!(accepted, "应接受会话"),
        other => panic!("应为 RemoteSessionResponse，实得 {other:?}"),
    }
    // 服务端转达响应 → 控制器才建立数据面。
    ctrl.handle_control_message(response);

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

/// 建立器上报的路径必须透传到 UI 状态：替身返回 `Direct` 时 `RemoteUiState.transport`
/// 就应是 `Some(Direct)`。UI 靠这个字段告诉用户「现在是直连还是中继」，透传错了会误导。
#[tokio::test]
async fn controller_records_transport_path() {
    let (a, b) = tokio::io::duplex(128 * 1024);
    let ctrl_est = Arc::new(OneShotEstablisher::with_path(
        Box::new(a),
        true,
        TransportPath::Direct,
    ));
    let ctld_est = Arc::new(OneShotEstablisher::new(Box::new(b), false));
    let ctrl = manager(ctrl_est, false, "home-nas");
    let ctld = manager(ctld_est, true, "office-pc");

    let (ctrl_sink, _frame_rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink);
    ctld.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));

    establish_session_with_mode(&ctrl, &ctld, RemoteMode::Control, RemoteMode::Control).await;

    // 建立是异步的：轮询到路径写入（或超时）。
    let mut got = None;
    for _ in 0..200 {
        if let Some(path) = ctrl.snapshot().transport {
            got = Some(path);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        got,
        Some(TransportPath::Direct),
        "建立器上报的路径应透传到 UI 状态"
    );

    ctrl.abort_active();
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
        capture_watchdog: CAPTURE_WATCHDOG,
        stats: SessionStatsReporter::noop(),
    };
    let ctrl_cfg = ControllerConfig {
        session_id: "sess-1".to_owned(),
        token: "tok-1".to_owned(),
        local_rd_id: "111111111".to_owned(),
        mode: RemoteMode::Control,
        stats: SessionStatsReporter::noop(),
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
            capture_watchdog: CAPTURE_WATCHDOG,
            stats: SessionStatsReporter::noop(),
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
            stats: SessionStatsReporter::noop(),
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
    // 新门控：控制器要先收到被控端确认才会去建立数据面；服务端会转达它。
    mgr.handle_control_message(ControlMessage::RemoteSessionResponse {
        session_id: "sess-x".to_owned(),
        accepted: true,
        reason: None,
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
            stats: SessionStatsReporter::noop(),
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

/// VP9 数据面：静止画面零输出，局部变化才继续出帧。
#[tokio::test]
async fn static_screen_emits_no_frames_and_change_emits_frames() {
    let (a, b) = tokio::io::duplex(256 * 1024);
    let ctrl = manager(Arc::new(OneShotEstablisher::new(Box::new(a), true)), false, "home-nas");
    let ctld = manager(Arc::new(OneShotEstablisher::new(Box::new(b), false)), true, "office-pc");

    let (ctrl_sink, _frame_rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    let screen = Arc::new(MutableScreen::new(solid(0, 192, 128, [10, 20, 30, 255])));
    ctld.install_platform(screen.clone(), Arc::new(RecordingInput::default()));

    establish_session(&ctrl, &ctld).await;

    // 首帧是关键帧（IDR），应到达控制器。
    assert!(
        poll_until(|| ctrl_sink.frames.load(Ordering::Relaxed) >= 1).await,
        "应收到首帧关键帧"
    );

    // 静止画面：再等一段，不应有任何**新增**帧（被控端完全不喂编码器）。
    let after_first = ctrl_sink.frames.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        ctrl_sink.frames.load(Ordering::Relaxed),
        after_first,
        "静止画面不应有新帧（带宽趋零的来源）"
    );

    // 改一块区域 → 应产生新的编码帧。
    {
        let mut frame = screen.frame.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        fill_rect(&mut frame, 10, 10, 20, 20, [250, 10, 10, 255]);
    }
    assert!(
        poll_until(|| ctrl_sink.frames.load(Ordering::Relaxed) > after_first).await,
        "画面变化应产生新帧"
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

// ── 接入确认门控 ───────────────────────────────────────────────────────

/// 从通道取出下一条 `RemoteSessionResponse`（跳过 `RemoteSessionEnd` 等旁路消息）。
async fn next_response(rx: &mut mpsc::Receiver<ControlMessage>) -> ControlMessage {
    loop {
        let msg = tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("等响应超时")
            .expect("通道关闭");
        if matches!(msg, ControlMessage::RemoteSessionResponse { .. }) {
            return msg;
        }
    }
}

/// 造一个装了平台实现、确认策略为 `Confirm` 的被控端。
fn confirm_controlled() -> Arc<RemoteManager> {
    let mgr = manager(Arc::new(FailingEstablisher), true, "office-pc");
    mgr.set_approval_policy(ApprovalPolicy::Confirm);
    mgr.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));
    mgr
}

/// 发一条接入请求。
fn incoming(mgr: &Arc<RemoteManager>, session_id: &str, from_name: &str) {
    mgr.handle_control_message(ControlMessage::RemoteSessionIncoming {
        session_id: session_id.to_owned(),
        token: "tok".to_owned(),
        from_name: from_name.to_owned(),
        mode: RemoteMode::Control,
    });
}

/// 断言一条响应并返回 `(accepted, session_id, reason)`。
fn expect_response(msg: ControlMessage) -> (bool, String, Option<String>) {
    match msg {
        ControlMessage::RemoteSessionResponse {
            session_id,
            accepted,
            reason,
        } => (accepted, session_id, reason),
        other => panic!("应为 RemoteSessionResponse，实得 {other:?}"),
    }
}

/// 1. `Confirm`：请求到达后**不回响应**，直到 `approve_pending` 才回 `accepted:true`。
#[tokio::test]
async fn confirm_policy_defers_until_approved() {
    let ctld = confirm_controlled();
    let (tx, mut rx) = mpsc::channel(16);
    ctld.attach(tx);

    incoming(&ctld, "sess-1", "home-nas");

    assert!(rx.try_recv().is_err(), "确认前不应回任何响应");
    let pending = ctld.snapshot().pending_approval.expect("应有待确认请求");
    assert_eq!(pending.session_id, "sess-1");
    assert_eq!(pending.from_name, "home-nas");
    assert_eq!(pending.mode, RemoteMode::Control);
    assert!(pending.deadline_ms > 0, "应带截止时刻");

    ctld.approve_pending(false);
    let (accepted, session_id, _) = expect_response(rx.try_recv().expect("批准后应回响应"));
    assert!(accepted, "应接受");
    assert_eq!(session_id, "sess-1");
    assert!(ctld.snapshot().pending_approval.is_none(), "批准后应清空待确认");
}

/// 2. `reject_pending` 回 `accepted:false` 并带上理由（缺省为「用户拒绝」）。
#[tokio::test]
async fn reject_pending_sends_reason() {
    let ctld = confirm_controlled();
    let (tx, mut rx) = mpsc::channel(16);
    ctld.attach(tx);

    incoming(&ctld, "sess-1", "home-nas");
    ctld.reject_pending(Some("不认识这台机器".into()));
    let (accepted, session_id, reason) = expect_response(rx.try_recv().expect("应回拒绝"));
    assert!(!accepted, "应拒绝");
    assert_eq!(session_id, "sess-1");
    assert_eq!(reason.as_deref(), Some("不认识这台机器"));
    assert!(ctld.snapshot().pending_approval.is_none());

    // 缺省理由。
    incoming(&ctld, "sess-2", "home-nas");
    ctld.reject_pending(None);
    let (accepted, _, reason) = expect_response(rx.try_recv().expect("应回拒绝"));
    assert!(!accepted);
    assert_eq!(reason.as_deref(), Some("用户拒绝"));
}

/// 3. 超时自动拒绝，理由为「等待确认超时」。
#[tokio::test]
async fn approval_timeout_rejects() {
    let ctld = confirm_controlled();
    ctld.set_timeouts(Duration::from_millis(50), Duration::from_secs(5));
    let (tx, mut rx) = mpsc::channel(16);
    ctld.attach(tx);

    incoming(&ctld, "sess-1", "home-nas");
    let (accepted, session_id, reason) = expect_response(next_response(&mut rx).await);
    assert!(!accepted, "超时应拒绝");
    assert_eq!(session_id, "sess-1");
    assert_eq!(reason.as_deref(), Some("等待确认超时"));
    assert!(ctld.snapshot().pending_approval.is_none(), "超时后应清空待确认");
}

/// 4. 已信任对端再次请求时直接接受，不再弹窗。
#[tokio::test]
async fn trusted_peer_auto_accepts_without_prompt() {
    let ctld = confirm_controlled();
    let (tx, mut rx) = mpsc::channel(16);
    ctld.attach(tx);

    // 第一次：确认并记住。
    incoming(&ctld, "sess-1", "home-nas");
    ctld.approve_pending(true);
    let (accepted, _, _) = expect_response(next_response(&mut rx).await);
    assert!(accepted, "首次应接受");
    assert!(poll_until(|| ctld.snapshot().active.is_none()).await, "首会话应结束");
    assert_eq!(ctld.snapshot().trusted_peers, vec!["home-nas".to_owned()]);

    // 第二次：已信任 → 无弹窗直接接受。
    incoming(&ctld, "sess-2", "home-nas");
    assert!(
        ctld.snapshot().pending_approval.is_none(),
        "已信任对端不应再弹窗"
    );
    let (accepted, session_id, _) = expect_response(next_response(&mut rx).await);
    assert!(accepted, "已信任应直接接受");
    assert_eq!(session_id, "sess-2");
}

/// 5. 已有待确认请求时，第二条请求被拒。
#[tokio::test]
async fn second_incoming_while_pending_is_rejected() {
    let ctld = confirm_controlled();
    let (tx, mut rx) = mpsc::channel(16);
    ctld.attach(tx);

    incoming(&ctld, "sess-1", "home-nas");
    incoming(&ctld, "sess-2", "other-pc");

    let (accepted, session_id, reason) = expect_response(rx.try_recv().expect("应回拒绝"));
    assert!(!accepted);
    assert_eq!(session_id, "sess-2");
    assert_eq!(reason.as_deref(), Some("被控端有待确认的接入请求"));
    // 第一条仍在挂起。
    assert_eq!(
        ctld.snapshot().pending_approval.expect("首条应仍在").session_id,
        "sess-1"
    );
}

/// 6. `Auto` 策略仍立即接受（回归护栏）。
#[tokio::test]
async fn auto_policy_accepts_immediately() {
    let ctld = manager(Arc::new(FailingEstablisher), true, "office-pc");
    ctld.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));
    let (tx, mut rx) = mpsc::channel(16);
    ctld.attach(tx);

    incoming(&ctld, "sess-1", "home-nas");
    let (accepted, session_id, _) = expect_response(rx.try_recv().expect("应回响应"));
    assert!(accepted, "Auto 应立即接受");
    assert_eq!(session_id, "sess-1");
    assert!(ctld.snapshot().pending_approval.is_none());
}

/// 7. 控制器在获被控端确认前**不建立数据面**；确认后才真正跑起来。
#[tokio::test]
async fn controller_waits_for_acceptance_before_establishing() {
    let (a, b) = tokio::io::duplex(128 * 1024);
    let ctrl_est = Arc::new(OneShotEstablisher::new(Box::new(a), true));
    let ctld_est = Arc::new(OneShotEstablisher::new(Box::new(b), false));
    let ctrl = manager(ctrl_est.clone(), false, "home-nas");
    let ctld = manager(ctld_est, true, "office-pc");
    ctld.set_approval_policy(ApprovalPolicy::Confirm);

    let (ctrl_sink, mut frame_rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    ctld.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));

    // 双方先拿到能力通告。
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

    // 服务端：授权 + 转达请求。
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

    // 控制器已受理，但停在「正在协商」，尚未建立数据面。
    let active = ctrl.snapshot().active.expect("控制器应受理");
    assert_eq!(active.state, SessionState::Negotiating, "应处于协商态");
    assert!(
        poll_until(|| ctld.snapshot().pending_approval.is_some()).await,
        "被控端应待确认"
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        ctrl_est.calls.load(Ordering::Relaxed),
        0,
        "确认前不得建立数据面"
    );

    // 用户确认 → 被控端回 accepted → 服务端转达 → 控制器建立数据面。
    ctld.approve_pending(false);
    let response = next_response(&mut ctld_rx).await;
    ctrl.handle_control_message(response);

    tokio::time::timeout(WAIT, frame_rx.recv())
        .await
        .expect("等画面超时")
        .expect("画面通道关闭");
    assert!(ctrl_est.calls.load(Ordering::Relaxed) >= 1, "确认后应建立数据面");
    assert_eq!(
        ctrl.snapshot().active.expect("会话应进行中").state,
        SessionState::Active,
        "数据面就绪后应升为 Active"
    );
}

// ── 「拒绝是引擎强制的」 ───────────────────────────────────────────────

/// 记录 `establish` 调用次数的建立器。
///
/// 用于断言**拒绝路径根本不建立数据面**——`FailingEstablisher` 只能让建立失败，无法区分
/// 「没被调用」与「被调用后失败」，而这里要锁的正是「根本没调用」。
struct CountingEstablisher {
    calls: AtomicUsize,
}

impl CountingEstablisher {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl TransportEstablisher for CountingEstablisher {
    async fn establish(&self, _request: EstablishRequest, _role: Role) -> RemoteResult<EstablishedStream> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // 被控端只**接受**会话才会走到这里；拒绝/超时路径调用它即为回归。
        Err(RemoteError::Signaling("本用例不应建立数据面".to_owned()))
    }
}

/// 驱动一次**真实数据面握手**：控制器发 `Hello`，被控端用一张**空的**挂起会话表跑
/// [`run_controlled`]。
///
/// 空表正是「用户拒绝 / 超时自动拒绝」之后 manager 留下的状态：
/// `reject_pending_request` 与 `on_approval_timeout`（`manager.rs`）**只回响应、从不
/// `insert`**，因此被控端从未登记该 token。这条 helper 把「硬闯数据面」这个动作真的跑到
/// 协议层，而不是只断言 UI 状态。
///
/// 返回 `(被控端会话结果, 控制器按明文读到的 HelloAck)`。
async fn handshake_against_unregistered_token(
    pending: Arc<PendingSessions>,
    session_id: &str,
    token: &str,
) -> (RemoteResult<SessionEnd>, SessionMessage) {
    use futures_util::StreamExt as _;
    use tokio_util::codec::FramedRead;

    let (a, b) = tokio::io::duplex(64 * 1024);
    let plan = plan_controller_handshake(session_id, token, "111111111").unwrap();

    // 控制器把 Hello 写进数据面（模拟「无视拒绝、直接硬闯」）。
    let mut ctrl_stream: BoxDuplex = Box::new(a);
    ctrl_stream.write_all(&plan.hello_frame).await.unwrap();
    ctrl_stream.flush().await.unwrap();

    let result = run_controlled(
        Box::new(b),
        ControlledConfig {
            session_id: session_id.to_owned(),
            local_rd_id: "222222222".to_owned(),
            mode: RemoteMode::Control,
            // 传**管理器真实的那张表**：断言的是「拒绝/超时路径没有往表里登记」，
            // 而不是「一张新造的空表里查不到」。
            pending,
            encode: EncodeOptions::default(),
            capture_watchdog: CAPTURE_WATCHDOG,
            stats: SessionStatsReporter::noop(),
        },
        Arc::new(FakeScreen),
        Arc::new(RecordingInput::default()),
        None,
        transfer_hub().0,
        CancellationToken::new(),
    )
    .await;

    // 被控端拒绝时会明文回一条 `HelloAck{accepted:false}`；读回来做协议层断言。
    let mut reader = FramedRead::new(ctrl_stream, SessionCodec::new());
    let ack = tokio::time::timeout(WAIT, reader.next())
        .await
        .expect("等 HelloAck 超时")
        .expect("流在拒绝应答前关闭")
        .expect("HelloAck 解码失败")
        .message;
    (result, ack)
}

/// P0-1a：用户**拒绝**后，即使控制器无视拒绝硬闯数据面，也必须因鉴权失败而终止。
///
/// 锁的不变量：`Confirm` 下的「拒绝」是**引擎强制**的、不只是 UI 语义——被控端从不登记该
/// token，所以硬闯者过不了握手鉴权。破坏它会怎样：用户点了「拒绝」，攻击者（或竞态中的
/// 控制器）照样能接管屏幕，弹窗沦为装饰。
#[tokio::test]
async fn reject_pending_makes_data_plane_handshake_fail() {
    let est = Arc::new(CountingEstablisher::new());
    let ctld = manager(est.clone(), true, "office-pc");
    ctld.set_approval_policy(ApprovalPolicy::Confirm);
    ctld.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));
    let (tx, mut rx) = mpsc::channel(16);
    ctld.attach(tx);

    incoming(&ctld, "sess-rej", "home-nas");
    ctld.reject_pending(None);
    let (accepted, sid, reason) = expect_response(rx.try_recv().expect("拒绝后应回响应"));
    assert!(!accepted, "应拒绝");
    assert_eq!(sid, "sess-rej");
    assert_eq!(reason.as_deref(), Some("用户拒绝"));
    assert!(ctld.snapshot().pending_approval.is_none(), "拒绝后应清空待确认");
    assert!(ctld.snapshot().active.is_none(), "拒绝后不应有活动会话");

    // (a) 拒绝路径根本不建立数据面（让出调度后再断言，排除竞态）。
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(est.calls(), 0, "拒绝后不应建立数据面");

    // (b) 硬闯：控制器拿着那个 token 径自握手 → 必须鉴权失败。
    //     用**管理器真实那张挂起表**：拒绝路径必须一个 token 都没登记。
    let pending = ctld.pending_sessions();
    assert!(
        pending.is_empty(),
        "拒绝后挂起表必须一个 token 都不留（len={}）",
        pending.len()
    );
    let (result, ack) = handshake_against_unregistered_token(pending, "sess-rej", "tok").await;
    assert!(
        matches!(result, Err(RemoteError::Auth(_))),
        "被控端必须因 token 未登记而鉴权失败，实得 {result:?}"
    );
    match ack {
        SessionMessage::HelloAck { accepted, .. } => assert!(!accepted, "被控端必须回 accepted:false"),
        other => panic!("应回 HelloAck，实得 {other:?}"),
    }
}

/// P0-1b：`APPROVAL_TIMEOUT` 到点**自动拒绝**同样从不登记 token，硬闯数据面必失败。
///
/// 锁的不变量与 [`reject_pending_makes_data_plane_handshake_fail`] 同一条，只是触发路径换成
/// 超时。既有的 `approval_timeout_rejects` 只断言了响应/UI 层，这里补上「引擎强制」层：
/// 超时与显式拒绝留下的**协议后果完全一致**——挂起表里没有这条会话。
#[tokio::test]
async fn approval_timeout_makes_data_plane_handshake_fail() {
    let est = Arc::new(CountingEstablisher::new());
    let ctld = manager(est.clone(), true, "office-pc");
    ctld.set_approval_policy(ApprovalPolicy::Confirm);
    ctld.set_timeouts(Duration::from_millis(50), Duration::from_secs(5));
    ctld.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));
    let (tx, mut rx) = mpsc::channel(16);
    ctld.attach(tx);

    incoming(&ctld, "sess-to", "home-nas");
    let (accepted, sid, reason) = expect_response(next_response(&mut rx).await);
    assert!(!accepted, "超时应拒绝");
    assert_eq!(sid, "sess-to");
    assert_eq!(reason.as_deref(), Some("等待确认超时"));
    assert!(ctld.snapshot().active.is_none(), "超时后不应有活动会话");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(est.calls(), 0, "超时拒绝后不应建立数据面");

    let pending = ctld.pending_sessions();
    assert!(
        pending.is_empty(),
        "超时拒绝后挂起表必须一个 token 都不留（len={}）",
        pending.len()
    );
    let (result, ack) = handshake_against_unregistered_token(pending, "sess-to", "tok").await;
    assert!(
        matches!(result, Err(RemoteError::Auth(_))),
        "硬闯必须鉴权失败，实得 {result:?}"
    );
    assert!(
        matches!(ack, SessionMessage::HelloAck { accepted: false, .. }),
        "被控端必须回 accepted:false，实得 {ack:?}"
    );
}

// ── 会话模式的权限边界 ────────────────────────────────────────────────

/// 造一对临时目录（共享目录 / 下载目录），返回 `(share, download)`。
fn temp_dirs(tag: &str) -> (PathBuf, PathBuf) {
    let pid = std::process::id();
    let share = std::env::temp_dir().join(format!("rt-{tag}-share-{pid}"));
    let dl = std::env::temp_dir().join(format!("rt-{tag}-dl-{pid}"));
    let _ = std::fs::remove_dir_all(&share);
    let _ = std::fs::remove_dir_all(&dl);
    std::fs::create_dir_all(&share).unwrap();
    std::fs::create_dir_all(&dl).unwrap();
    (share, dl)
}

/// P0-3a：`ViewOnly` 模式被控端**不给文件服务**——权限边界，不只是 UI 偏好。
///
/// 锁的不变量：只读会话里控制器既不能下载也不能写入被控端文件（`manager.rs::controlled_task`
/// 按 `mode != ViewOnly` 过滤掉 `service`，被控端据此拒绝一切文件请求）。破坏它会怎样：任何人
/// 只拿到「仅观看」授权，就能顺着文件面板把被控端磁盘拖走。
#[tokio::test]
async fn view_only_mode_rejects_file_service() {
    let (share, dl) = temp_dirs("vo");
    std::fs::write(share.join("secret.bin"), b"top-secret").unwrap();

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

    let (ctrl_sink, mut frame_rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    ctld.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));

    establish_session_with_mode(&ctrl, &ctld, RemoteMode::ViewOnly, RemoteMode::ViewOnly).await;

    // 仅观看仍会推画面：先等一帧，确认会话真的跑起来了，后面的「空表 / 拒绝」才有意义。
    tokio::time::timeout(WAIT, frame_rx.recv())
        .await
        .expect("等首帧超时")
        .expect("画面通道关闭");

    // 控制器列目录 + 下载被控端的 secret.bin：两者都必须被拒。
    ctrl.list_remote_dir("/");
    ctrl.download_remote_file("/secret.bin");
    assert!(
        poll_until(|| ctrl.snapshot().transfer_note.is_some()).await,
        "下载应得到结果提示"
    );
    let note = ctrl.snapshot().transfer_note.expect("应有传输提示");
    assert!(note.starts_with("失败："), "ViewOnly 下下载必须被拒，实得 {note}");
    // 列表：被控端不提供文件服务 → 空表（而不是把 secret.bin 列出来）。
    // 与下载共用同一条有序 TCP 流，下载的拒绝到了即证明列表应答也到了。
    assert!(
        ctrl.snapshot().remote_entries.is_empty(),
        "ViewOnly 下列表必须为空，实得 {:?}",
        ctrl.snapshot().remote_entries
    );
    // 没有任何文件落到下载目录。
    assert!(!dl.join("secret.bin").exists(), "ViewOnly 下不应有任何文件落地");

    let _ = std::fs::remove_dir_all(&share);
    let _ = std::fs::remove_dir_all(&dl);
}

/// P0-3b：`FileTransfer` 模式**一个画面帧都不发**，但文件服务照常可用。
///
/// 锁的不变量：`FileTransfer` 下被控端 `capture == false`（`controlled.rs`），屏幕内容根本
/// 不会被采集——「只传文件」是真不采集，而不是采了就扔。破坏它会怎样：以为在传文件，实际把
/// 屏幕内容也编码推了出去。
#[tokio::test]
async fn file_transfer_mode_sends_no_video_but_allows_files() {
    let (share, dl) = temp_dirs("ft");
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

    let (ctrl_sink, _frame_rx) = RecordingSink::with_notify();
    ctrl.install_frame_sink(ctrl_sink.clone());
    ctld.install_platform(Arc::new(FakeScreen), Arc::new(RecordingInput::default()));

    establish_session_with_mode(&ctrl, &ctld, RemoteMode::FileTransfer, RemoteMode::FileTransfer).await;

    // 文件服务仍可用：列出并能下载（`service` 只在 `ViewOnly` 下被过滤掉）。
    ctrl.list_remote_dir("/");
    assert!(
        poll_until(|| ctrl.snapshot().remote_entries.iter().any(|e| e.name == "source.bin")).await,
        "FileTransfer 下应能列目录"
    );
    ctrl.download_remote_file("/source.bin");
    let downloaded = dl.join("source.bin");
    assert!(
        poll_until(|| std::fs::read(&downloaded).is_ok_and(|b| b == payload)).await,
        "FileTransfer 下应能下载文件"
    );

    // 全程零画面帧：被控端根本不采集。
    assert_eq!(ctrl_sink.frames.load(Ordering::Relaxed), 0, "FileTransfer 不应有任何画面帧");

    let _ = std::fs::remove_dir_all(&share);
    let _ = std::fs::remove_dir_all(&dl);
}

