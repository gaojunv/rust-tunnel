//! [`RemoteManager`]：`crates/client-gui` 消费的唯一门面，也是 UI 的唯一状态源。
//!
//! # 与控制通道的关系
//!
//! 实现 [`RemoteBridge`]：`crates/client` 的控制通道读循环把远程变体交给
//! [`RemoteManager::handle_control_message`]，并用 [`RemoteManager::attach`] 注入上行发送端。
//! 三个 trait 方法都**非阻塞**——它们跑在读循环里，任何阻塞都会拖住心跳与隧道消息。
//!
//! # 状态
//!
//! [`RemoteUiState`] 经 `tokio::sync::watch` 广播：UI 每帧读一次即可，里面只放小对象
//! （`Clone` 廉价）。所有状态变更都走 [`RemoteManager::update_ui`]。
//!
//! # 会话编排
//!
//! - 控制器：收到 `RemoteConnectGrant` → 记录授权并置 `Negotiating`，**等被控端确认**
//!   （`RemoteSessionResponse{accepted:true}`）→ 预计算握手 → [`TransportEstablisher::establish`]
//!   （中继/直连）→ [`crate::controller::run_controller`]。
//! - 被控端：收到 `RemoteSessionIncoming` 且 `enabled` → 按 [`ApprovalPolicy`] 决定：`Auto`
//!   或已信任对端则直接接受；`Confirm` 且未信任则挂起等用户
//!   [`RemoteManager::approve_pending`] / [`RemoteManager::reject_pending`]（[`APPROVAL_TIMEOUT`]
//!   到点自动拒绝）→ 登记一次性 token、回 `accepted` → `establish`
//!   → [`crate::controlled::run_controlled`]。
//!
//! 确认门控复用既有 `RemoteSessionResponse` 通道，不新增任何控制消息变体；服务端本来就无条件
//! 把被控端的响应转达给控制器，所以两端只需各自「延迟应答 / 等待应答」。
//!
//! 两者都接受 [`CancellationToken`]，由 [`RemoteManager::abort_active`] 一键中断。

use crate::approval::{ApprovalPolicy, PendingApproval, APPROVAL_TIMEOUT, APPROVAL_WAIT_TIMEOUT};
use crate::auth::{PendingSession, PendingSessions};
use crate::controller::{run_controller, ControllerCommand, ControllerConfig};
use crate::controlled::{run_controlled, ControlledConfig};
use crate::error::{RemoteError, RemoteResult};
use crate::file::FileService;
use crate::screen::{
    Clipboard, EncodeOptions, FileEntry, FrameSink, InputEvent, InputSink, NullFrameSink, QualityPreset, ScreenSource,
};
use crate::session::plan_controller_handshake;
use crate::transfer::{TransferCommand, TransferEvent, TransferHub, TransferProgress};
use crate::transport::{EstablishRequest, Role, TransportEstablisher};
use rust_tunnel_common::protocol::{ControlMessage, RemoteMode, RemoteParams, RemotePeerInfo};
use rust_tunnel_common::remote_bridge::RemoteBridge;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// 会话命令队列容量。
const COMMAND_CAPACITY: usize = 256;

/// 本端在会话中的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    /// 控制方。
    Controller,
    /// 被控方。
    Controlled,
}

/// 会话生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 已受理，正在协商。
    Negotiating,
    /// 正在建立数据面传输。
    Connecting,
    /// 数据面已就绪，画面/输入在流动。
    Active,
    /// 已结束。
    Ended,
}

/// 当前会话的只读快照（UI 展示用，`Clone` 廉价）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSessionInfo {
    /// 会话 ID。
    pub session_id: String,
    /// 本端角色。
    pub role: SessionRole,
    /// 对端展示名（控制器为被控端名，被控端为发起方名）。
    pub peer_name: String,
    /// 会话模式。
    pub mode: RemoteMode,
    /// 生命周期状态。
    pub state: SessionState,
    /// 建立时间（毫秒，自 epoch）。
    pub started_at_ms: u64,
}

/// UI 可见的远程控制状态。`Clone` + 适合 `watch`（只含小对象）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteUiState {
    /// 控制通道是否已连接（`attach` / `on_disconnected` 维护）。
    pub connected: bool,
    /// 本端是否允许被远程控制。
    pub enabled: bool,
    /// 服务端是否支持远程控制（`RemoteCapability.params` 是否为 `Some`）。
    pub capability: bool,
    /// 服务端分配的本端 RustDesk ID（未启用时为空串）。
    pub rd_id: String,
    /// 本端平台标识。
    pub self_platform: String,
    /// 可被远程控制的在线客户端列表。
    pub peers: Vec<RemotePeerInfo>,
    /// 当前会话（无则为 `None`）。
    pub active: Option<ActiveSessionInfo>,
    /// 最近一次错误（UI 提示用）。
    pub last_error: Option<String>,
    /// 控制器当前选中的画质档位（下发后由被控端生效）。
    pub quality: QualityPreset,
    /// 控制器当前观看的显示器编号。
    pub selected_display: u32,
    /// 最近一次收到的对端剪贴板文本（UI 提供「粘贴到本机」按钮）。
    pub clipboard_in: Option<String>,
    /// 文件浏览器当前虚拟路径。
    pub remote_dir: String,
    /// 文件浏览器当前目录项。
    pub remote_entries: Vec<FileEntry>,
    /// 在途/刚结束的传输（UI 进度条）。
    pub transfers: Vec<TransferProgress>,
    /// 最近一次传输结果提示（成功落盘路径或失败原因）。
    pub transfer_note: Option<String>,
    /// 被控端接入确认策略。
    pub approval_policy: ApprovalPolicy,
    /// 待本机确认的接入请求（UI 弹窗数据；无则为 `None`）。
    pub pending_approval: Option<PendingApproval>,
    /// 已信任的对端客户端名：命中后不再弹窗、直接接受。
    ///
    /// **以客户端名为键**：`RemoteSessionIncoming` 只带 `from_name`、不带 `from_rd_id`，
    /// 没有别的标识可用。后果有二——服务端管理员把客户端改名后该条目即失效（新名字不在表里，
    /// 会重新弹窗）；且信任**只存内存**，进程退出即清空。这两点都是刻意的，不做持久化。
    pub trusted_peers: Vec<String>,
}

/// [`RemoteManager`] 的构造参数。
pub struct RemoteManagerConfig {
    /// 本机连接的服务端 `host:port`（用于补齐留空的 `rendezvous_host`）。
    pub server_host: String,
    /// 本端客户端名。
    pub self_client_name: String,
    /// 平台标识：`macos` / `windows` / `linux`。
    pub platform: String,
    /// 显示器数量（安装平台实现前的占位；安装后以 [`ScreenSource::displays`] 为准）。
    pub displays: u32,
    /// 初始是否允许被远程控制。
    pub enabled: bool,
    /// 数据面建立器。
    pub establisher: Arc<dyn TransportEstablisher>,
    /// 屏幕编码参数。
    pub encode: EncodeOptions,
    /// 被控端对外共享的目录；`None` = 本端不接受任何文件请求。
    pub share_dir: Option<PathBuf>,
    /// 控制器下载文件的落地目录。
    pub download_dir: PathBuf,
    /// 被控端接入确认策略（默认 [`ApprovalPolicy::Confirm`]）。
    pub approval_policy: ApprovalPolicy,
}

/// 正在进行的会话句柄。
struct ActiveHandle {
    /// 会话 ID。
    session_id: String,
    /// 中断信号。
    cancel: CancellationToken,
}

/// 被控端当前待用户确认的接入请求（内部）。
///
/// `token` 是握手凭证，**绝不进 UI 状态**——[`PendingApproval`] 只带展示字段。
struct PendingRequest {
    /// 会话 ID。
    session_id: String,
    /// 服务端下发的一次性 token。
    token: String,
    /// 发起方客户端名。
    from_name: String,
    /// 会话模式。
    mode: RemoteMode,
}

/// 控制器已受理、等待被控端确认的授权（内部）。
struct PendingGrant {
    /// 会话 ID。
    session_id: String,
    /// 服务端下发的一次性 token。
    token: String,
    /// 被控端 RustDesk ID。
    target_rd_id: String,
    /// 被控端平台标识（仅日志）。
    target_platform: String,
    /// 会合/中继参数（已补齐 host）。
    params: RemoteParams,
    /// 会话模式。
    mode: RemoteMode,
    /// 中断信号（等待期间与建立后共用同一个）。
    cancel: CancellationToken,
    /// 会话命令接收端，建立后交给 `controller_task`。
    cmd_rx: mpsc::Receiver<ControllerCommand>,
}

/// 管理器内部可变状态。
#[derive(Default)]
struct Inner {
    /// 服务端下发的会合/中继参数（已补齐 host）。
    params: Option<RemoteParams>,
    /// 本端 RustDesk ID。
    rd_id: String,
    /// 当前会话句柄。
    active: Option<ActiveHandle>,
    /// 被控端挂起会话表。
    pending: Arc<PendingSessions>,
    /// 最近一次请求的会话模式（`RemoteConnectGrant` 不携带模式）。
    last_requested_mode: Option<RemoteMode>,
    /// 被控端待用户确认的接入请求。
    pending_request: Option<PendingRequest>,
    /// 控制器已受理、等待被控端确认的授权。
    pending_grant: Option<PendingGrant>,
    /// 乱序防御：被控端 `accepted` 先于授权到达时暂存的 `session_id`。
    early_accept: Option<String>,
    /// 被控端等待确认的时限（默认 [`APPROVAL_TIMEOUT`]，测试可注入）。
    approval_timeout: Duration,
    /// 控制器等待确认的时限（默认 [`APPROVAL_WAIT_TIMEOUT`]，测试可注入）。
    approval_wait_timeout: Duration,
}

/// 已安装的平台实现。
struct PlatformHandles {
    /// 屏幕采集源。
    screen: Arc<dyn ScreenSource>,
    /// 输入注入目标。
    input: Arc<dyn InputSink>,
}

/// 被控端共享目录（构造一次，会话间复用）。
struct ShareDir {
    /// 受限文件服务。
    service: Arc<FileService>,
    /// 原始路径（仅用于日志/UI 展示）。
    path: PathBuf,
}

/// 远程控制引擎门面。
pub struct RemoteManager {
    /// 构造参数。
    cfg: RemoteManagerConfig,
    /// 控制通道上行发送端。
    upstream: Mutex<Option<mpsc::Sender<ControlMessage>>>,
    /// UI 状态广播端。
    ui_tx: watch::Sender<RemoteUiState>,
    /// 内部状态。
    inner: Mutex<Inner>,
    /// 已安装的平台实现。
    platform: RwLock<Option<PlatformHandles>>,
    /// 控制器侧画面上报目标。
    frame_sink: RwLock<Option<Arc<dyn FrameSink>>>,
    /// 当前控制器会话的命令投递端（输入 / 切档位 / 剪贴板 / 文件）。
    cmd_tx: Mutex<Option<mpsc::Sender<ControllerCommand>>>,
    /// 被控端剪贴板实现。
    clipboard: RwLock<Option<Arc<dyn Clipboard>>>,
    /// 被控端共享目录（`share_dir` 为 `None` 时此处也是 `None`）。
    share: Option<ShareDir>,
    /// 自引用弱指针（供同步方法 `spawn` 使用）。
    self_weak: Weak<RemoteManager>,
    /// 构造时所在的 Tokio 运行时句柄。
    ///
    /// [`Self::set_enabled`] 由 GUI 的 UI 线程调用（**不在运行时上下文内**），此时
    /// `tokio::spawn` 会 panic；预登记必须走这个句柄。构造发生在 `block_on` 内，故句柄可用。
    rt: Option<tokio::runtime::Handle>,
}

impl std::fmt::Debug for RemoteManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteManager")
            .field("client", &self.cfg.self_client_name)
            .field("platform", &self.cfg.platform)
            .finish_non_exhaustive()
    }
}

impl RemoteManager {
    /// 构造管理器（返回 `Arc`，内部自引用用于同步方法派生任务）。
    #[must_use]
    pub fn new(cfg: RemoteManagerConfig) -> Arc<Self> {
        let ui = RemoteUiState {
            enabled: cfg.enabled,
            self_platform: cfg.platform.clone(),
            quality: QualityPreset::default(),
            approval_policy: cfg.approval_policy,
            ..RemoteUiState::default()
        };
        let (ui_tx, _rx) = watch::channel(ui);
        // 共享目录在构造时就定下来：会话期间改目录会让正在传的文件落到别处。
        let share = cfg.share_dir.clone().map(|path| ShareDir {
            service: Arc::new(FileService::new(path.clone())),
            path,
        });
        Arc::new_cyclic(|weak| Self {
            cfg,
            upstream: Mutex::new(None),
            ui_tx,
            inner: Mutex::new(Inner {
                pending: Arc::new(PendingSessions::new()),
                approval_timeout: APPROVAL_TIMEOUT,
                approval_wait_timeout: APPROVAL_WAIT_TIMEOUT,
                ..Inner::default()
            }),
            platform: RwLock::new(None),
            frame_sink: RwLock::new(None),
            cmd_tx: Mutex::new(None),
            clipboard: RwLock::new(None),
            share,
            self_weak: weak.clone(),
            rt: tokio::runtime::Handle::try_current().ok(),
        })
    }

    /// 本端共享目录路径（未配置时 `None`）。
    #[must_use]
    pub fn share_dir(&self) -> Option<&std::path::Path> {
        self.share.as_ref().map(|s| s.path.as_path())
    }

    /// 取桥接句柄，交给 `crates/client` 的控制通道。
    #[must_use]
    pub fn bridge(self: &Arc<Self>) -> Arc<dyn RemoteBridge> {
        Arc::clone(self) as Arc<dyn RemoteBridge>
    }

    /// 订阅 UI 状态（UI 每帧读一次即可）。
    #[must_use]
    pub fn status(&self) -> watch::Receiver<RemoteUiState> {
        self.ui_tx.subscribe()
    }

    /// 当前状态快照。
    #[must_use]
    pub fn snapshot(&self) -> RemoteUiState {
        self.ui_tx.borrow().clone()
    }

    /// 请求连接某个客户端（控制器）。
    pub fn request_connect(&self, target_client: &str, mode: RemoteMode) {
        if !self.ui_tx.borrow().capability {
            self.update_ui(|s| s.last_error = Some("服务端未启用远程控制".to_owned()));
            return;
        }
        self.lock_inner().last_requested_mode = Some(mode);
        self.update_ui(|s| s.last_error = None);
        self.send(ControlMessage::RemoteConnectRequest {
            target_client: target_client.to_owned(),
            mode,
        });
    }

    /// 设置是否允许被远程控制；变化时上报 `RemoteAnnounce`。关闭时中断在途会话。
    pub fn set_enabled(&self, on: bool) {
        self.update_ui(|s| s.enabled = on);
        self.announce();
        if on {
            // 刚打开开关：补一次预登记，别等到第一个会话才登记（见 `TransportEstablisher::prepare`）。
            self.prewarm();
        } else {
            self.abort_active();
        }
    }

    /// 一键中断当前会话（UI 中断按钮）。若正有接入请求待确认，也一并拒绝。
    pub fn abort_active(&self) {
        // 待确认的接入请求随之中止：不该在用户已关闭/中断后还留着弹窗等点击。
        // 理由是给**控制器**看的，而本方法是「中断会话」「关闭被控开关」「新请求取代旧会话」
        // 三个入口的公共路径，故用中性措辞，不写死成其中某一个的原因。
        self.reject_pending_request("被控端中止了接入");
        self.lock_inner().pending_grant = None;
        if let Some(active) = self.take_active() {
            active.cancel.cancel();
            self.lock_cmd().take();
            // 被控端：控制器还没拨进来时挂起记录仍在表里，一并撤掉，不必等下一个 TTL 窗口。
            self.lock_inner().pending.remove(&active.session_id);
            self.send(ControlMessage::RemoteSessionEnd {
                session_id: active.session_id,
                reason: "用户中断".to_owned(),
            });
            self.update_ui(|s| s.active = None);
        }
    }

    /// 安装平台实现（截屏 + 输入注入）。安装后若已具备能力则重新上报显示器数。
    pub fn install_platform(&self, screen: Arc<dyn ScreenSource>, input: Arc<dyn InputSink>) {
        if let Ok(mut guard) = self.platform.write() {
            *guard = Some(PlatformHandles { screen, input });
        }
        if self.ui_tx.borrow().capability {
            self.announce();
        }
    }

    /// 安装控制器侧画面上报目标（UI 实现）。
    pub fn install_frame_sink(&self, sink: Arc<dyn FrameSink>) {
        if let Ok(mut guard) = self.frame_sink.write() {
            *guard = Some(sink);
        }
    }

    /// 安装被控端剪贴板实现（`arboard` 等）。
    pub fn install_clipboard(&self, clipboard: Arc<dyn Clipboard>) {
        if let Ok(mut guard) = self.clipboard.write() {
            *guard = Some(clipboard);
        }
    }

    /// 投递一条输入事件到当前控制器会话（无会话或队列满时静默丢弃）。
    pub fn send_input(&self, event: InputEvent) {
        self.send_command(ControllerCommand::Input(event));
    }

    /// 请求被控端切换到某台显示器。
    pub fn select_display(&self, display: u32) {
        self.update_ui(|s| s.selected_display = display);
        self.send_command(ControllerCommand::SelectDisplay(display));
    }

    /// 下发画质档位（由被控端生效）。
    pub fn set_quality(&self, preset: QualityPreset) {
        self.update_ui(|s| s.quality = preset);
        self.send_command(ControllerCommand::SetQuality(preset));
    }

    /// 把本机剪贴板文本发给被控端。
    pub fn send_clipboard(&self, text: String) {
        self.send_command(ControllerCommand::Clipboard(text));
    }

    /// 请求被控端把它的剪贴板发过来（结果经 [`FrameSink::on_clipboard`] 回给 UI）。
    pub fn request_clipboard(&self) {
        self.send_command(ControllerCommand::ClipboardRequest);
    }

    /// 列出被控端共享目录下的内容（结果进 [`RemoteUiState::remote_entries`]）。
    pub fn list_remote_dir(&self, path: &str) {
        self.update_ui(|s| {
            path.clone_into(&mut s.remote_dir);
            s.remote_entries.clear();
        });
        self.send_command(ControllerCommand::File(TransferCommand::List {
            path: path.to_owned(),
        }));
    }

    /// 下载被控端的某个文件到本机 `download_dir`。
    pub fn download_remote_file(&self, path: &str) {
        self.send_command(ControllerCommand::File(TransferCommand::Download {
            path: path.to_owned(),
        }));
    }

    /// 上传本机文件到被控端的共享目录。
    pub fn upload_local_file(&self, local: PathBuf, remote: &str) {
        self.send_command(ControllerCommand::File(TransferCommand::Upload {
            local,
            remote: remote.to_owned(),
        }));
    }

    /// 取消一次在途传输。
    pub fn cancel_transfer(&self, id: u64) {
        self.send_command(ControllerCommand::File(TransferCommand::Cancel { id }));
    }

    /// 设置被控端接入确认策略。
    pub fn set_approval_policy(&self, policy: ApprovalPolicy) {
        self.update_ui(|s| s.approval_policy = policy);
    }

    /// 当前被控端接入确认策略。
    #[must_use]
    pub fn approval_policy(&self) -> ApprovalPolicy {
        self.ui_tx.borrow().approval_policy
    }

    /// 批准当前待确认的接入请求；`remember` 为真时把该对端加入信任列表（此后免弹窗）。
    ///
    /// 由 UI 线程调用，**不在 Tokio 运行时内**，故派生会话任务走构造时捕获的运行时句柄。
    pub fn approve_pending(&self, remember: bool) {
        let Some(req) = self.lock_inner().pending_request.take() else {
            return;
        };
        if remember {
            let name = req.from_name.clone();
            self.update_ui(move |s| {
                if !s.trusted_peers.iter().any(|n| n == &name) {
                    s.trusted_peers.push(name);
                }
            });
        }
        self.update_ui(|s| s.pending_approval = None);
        // 理论上平台实现在 `handle_incoming` 里已校验过；若中途被卸下，只能拒绝。
        let Some((screen, input)) = self.platform_arc() else {
            self.send(ControlMessage::RemoteSessionResponse {
                session_id: req.session_id,
                accepted: false,
                reason: Some("被控端缺少屏幕/输入实现".to_owned()),
            });
            return;
        };
        self.accept_incoming(req, screen, input);
    }

    /// 拒绝当前待确认的接入请求；`reason` 为空时用「用户拒绝」。
    pub fn reject_pending(&self, reason: Option<String>) {
        let reason = reason.unwrap_or_else(|| "用户拒绝".to_owned());
        self.reject_pending_request(&reason);
    }

    /// 清空信任列表。
    pub fn clear_trusted(&self) {
        self.update_ui(|s| s.trusted_peers.clear());
    }

    /// 从信任列表移除某个对端名。
    pub fn untrust(&self, name: &str) {
        let name = name.to_owned();
        self.update_ui(move |s| s.trusted_peers.retain(|n| n != &name));
    }

    /// 注入确认时限（仅测试用，避免真等 45s）。
    #[cfg(test)]
    pub(crate) fn set_timeouts(&self, approval: Duration, wait: Duration) {
        let mut inner = self.lock_inner();
        inner.approval_timeout = approval;
        inner.approval_wait_timeout = wait;
    }

    // ── 内部辅助 ───────────────────────────────────────────────────────

    /// 锁内部状态（容忍中毒）。
    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 锁上行发送端（容忍中毒）。
    fn lock_upstream(&self) -> MutexGuard<'_, Option<mpsc::Sender<ControlMessage>>> {
        self.upstream.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 锁会话命令发送端（容忍中毒）。
    fn lock_cmd(&self) -> MutexGuard<'_, Option<mpsc::Sender<ControllerCommand>>> {
        self.cmd_tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 投递一条会话命令（无会话或队列满时静默丢弃）。
    fn send_command(&self, cmd: ControllerCommand) {
        if let Some(tx) = self.lock_cmd().as_ref() {
            if tx.try_send(cmd).is_err() {
                debug!("会话命令队列满或会话已结束，丢弃命令");
            }
        } else {
            debug!("当前无控制器会话，丢弃命令");
        }
    }

    /// 让数据面提前完成会合服务器登记（见 [`TransportEstablisher::prepare`]）。
    ///
    /// 只在**本端允许被远程控制**时做：关闭时既不会出现在别人的「可远程」列表里，也没必要
    /// 登记。关闭开关后**不主动撤销**登记——登记只代表「在线存在」，建立会话仍需服务端签发的
    /// 一次性 token，而被控端关闭的客户端服务端根本不会签发。
    fn prewarm(&self) {
        if !self.ui_tx.borrow().enabled {
            return;
        }
        let (params, rd_id) = {
            let inner = self.lock_inner();
            (inner.params.clone(), inner.rd_id.clone())
        };
        let Some(params) = params else {
            debug!("会合参数尚未下发，跳过预登记");
            return;
        };
        if rd_id.is_empty() {
            debug!("本端 rd_id 尚未分配，跳过预登记");
            return;
        }
        let Some(rt) = self.rt.clone() else {
            // 无运行时上下文（如未在 Tokio 内构造管理器的单测）：跳过即可，首个会话仍会
            // 在 `establish` 时构造传输。
            debug!("无 Tokio 运行时句柄，跳过数据面预登记");
            return;
        };
        let establisher = Arc::clone(&self.cfg.establisher);
        rt.spawn(async move {
            if let Err(err) = establisher.prepare(&params, &rd_id).await {
                warn!(error = %err, "数据面预登记失败；会话发起时会重新构造传输");
            }
        });
    }

    /// 读平台实现（容忍中毒）。
    fn read_platform(&self) -> RwLockReadGuard<'_, Option<PlatformHandles>> {
        self.platform.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 取画面上报目标（未安装时用空实现）。
    fn frame_sink_arc(&self) -> Arc<dyn FrameSink> {
        self.frame_sink
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| Arc::new(NullFrameSink))
    }

    /// 本端 RustDesk ID。
    fn local_rd_id(&self) -> String {
        self.lock_inner().rd_id.clone()
    }

    /// 当前会合参数（未收到 `RemoteCapability` 时为 `Err`）。
    fn current_params(&self) -> RemoteResult<RemoteParams> {
        self.lock_inner()
            .params
            .clone()
            .ok_or_else(|| RemoteError::Unavailable("尚未收到服务端远程控制能力通告".to_owned()))
    }

    /// 显示器数量（优先用平台实现）。
    fn display_count(&self) -> u32 {
        let from_platform = self.read_platform().as_ref().map(|p| p.screen.displays().len());
        match from_platform {
            Some(n) => u32::try_from(n).unwrap_or(self.cfg.displays),
            None => self.cfg.displays,
        }
    }

    /// 上行发送一条控制消息（非阻塞；通道满或未连接时丢弃并记日志）。
    fn send(&self, msg: ControlMessage) {
        let guard = self.lock_upstream();
        if let Some(tx) = guard.as_ref() {
            if tx.try_send(msg).is_err() {
                debug!("控制通道上行拥塞或已断开，丢弃远程消息");
            }
        } else {
            debug!("控制通道尚未就绪，丢弃远程消息");
        }
    }

    /// 变更 UI 状态。
    fn update_ui(&self, f: impl FnOnce(&mut RemoteUiState)) {
        self.ui_tx.send_modify(f);
    }

    /// 上报 `RemoteAnnounce`。
    fn announce(&self) {
        let enabled = self.ui_tx.borrow().enabled;
        let displays = self.display_count();
        self.send(ControlMessage::RemoteAnnounce {
            enabled,
            platform: self.cfg.platform.clone(),
            displays,
        });
    }

    /// 记录当前会话句柄。
    fn set_active(&self, handle: ActiveHandle) {
        self.lock_inner().active = Some(handle);
    }

    /// 取出并清空当前会话句柄。
    fn take_active(&self) -> Option<ActiveHandle> {
        self.lock_inner().active.take()
    }

    /// 当前是否有会话。
    fn has_active(&self) -> bool {
        self.lock_inner().active.is_some()
    }

    /// 把某会话标为 `Active`（若仍是当前会话）。
    fn mark_active(&self, session_id: &str) {
        self.update_ui(|s| {
            if let Some(active) = s.active.as_mut() {
                if active.session_id == session_id {
                    active.state = SessionState::Active;
                }
            }
        });
    }

    /// 会话收尾：仅当仍是当前会话时清理并通知对端。
    fn finish_session(&self, session_id: &str, result: RemoteResult<()>) {
        let is_current = self
            .lock_inner()
            .active
            .as_ref()
            .is_some_and(|a| a.session_id == session_id);
        if !is_current {
            return;
        }
        self.take_active();
        self.lock_cmd().take();
        let (reason, failed) = match result {
            Ok(()) => ("会话结束".to_owned(), false),
            Err(err) => (err.to_string(), true),
        };
        self.send(ControlMessage::RemoteSessionEnd {
            session_id: session_id.to_owned(),
            reason: reason.clone(),
        });
        self.update_ui(move |s| {
            s.active = None;
            if failed {
                s.last_error = Some(reason);
            }
        });
    }

    /// 处理 `RemoteCapability`。
    fn on_capability(&self, params: Option<RemoteParams>, rd_id: String) {
        let filled = params.map(|mut p| {
            if p.rendezvous_host.is_empty() {
                p.rendezvous_host.clone_from(&self.cfg.server_host);
            }
            p
        });
        let capability = filled.is_some();
        {
            let mut inner = self.lock_inner();
            inner.rd_id.clone_from(&rd_id);
            inner.params = filled;
        }
        self.update_ui(move |s| {
            s.rd_id = rd_id;
            s.capability = capability;
        });
        if capability {
            self.announce();
            // 提前登记到会合服务器。**这一步不能省**：控制器的中继请求若早于被控端登记到达
            // hbbs，会被静默丢弃（见 `TransportEstablisher::prepare` 的说明）。
            self.prewarm();
        }
    }

    /// 受理一次控制器会话（`RemoteConnectGrant`）。
    ///
    /// 这里**不建立数据面**：先记录授权、置 `Negotiating`，等被控端回 `accepted:true`
    /// （见 [`Self::begin_controller_session`]）后才 `establish`。若被控端确认先于授权到达
    /// （乱序），立刻建立。
    fn start_controller(
        &self,
        session_id: &str,
        token: String,
        target_rd_id: String,
        target_platform: &str,
        params: RemoteParams,
    ) {
        // 新请求取代旧会话（沿用既有语义）。
        self.abort_active();

        let mut filled = params;
        if filled.rendezvous_host.is_empty() {
            filled.rendezvous_host.clone_from(&self.cfg.server_host);
        }
        let mode = self.lock_inner().last_requested_mode.unwrap_or(RemoteMode::Control);
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CAPACITY);
        *self.lock_cmd() = Some(cmd_tx);
        let cancel = CancellationToken::new();
        self.set_active(ActiveHandle {
            session_id: session_id.to_owned(),
            cancel: cancel.clone(),
        });
        // `RemoteConnectGrant` 只带 `target_rd_id`，不带展示名：回查可远程列表把名字补上，
        // 否则会话窗口会显示一串 9 位数字。列表里查不到（对端刚离线等）就回落到 rd_id。
        let peer_name = {
            let ui = self.ui_tx.borrow();
            ui.peers
                .iter()
                .find(|p| p.rd_id == target_rd_id)
                .map_or_else(|| target_rd_id.clone(), |p| p.display_name.clone())
        };
        debug!(target_platform = %target_platform, peer = %peer_name, "受理控制器会话，等待对端确认");
        let info = ActiveSessionInfo {
            session_id: session_id.to_owned(),
            role: SessionRole::Controller,
            peer_name,
            mode,
            state: SessionState::Negotiating,
            started_at_ms: now_ms(),
        };
        self.update_ui(move |s| {
            s.active = Some(info);
            s.last_error = None;
        });

        let grant = PendingGrant {
            session_id: session_id.to_owned(),
            token,
            target_rd_id,
            target_platform: target_platform.to_owned(),
            params: filled,
            mode,
            cancel,
            cmd_rx,
        };
        let already_accepted = {
            let mut inner = self.lock_inner();
            inner.pending_grant = Some(grant);
            let early = inner.early_accept.take();
            early.as_deref() == Some(session_id)
        };
        if already_accepted {
            self.begin_controller_session();
        } else {
            self.arm_wait_timeout(session_id);
        }
    }

    /// 被控端确认后真正启动控制器会话：`Negotiating` → `Connecting`，派生 `controller_task`。
    fn begin_controller_session(&self) {
        let Some(grant) = self.lock_inner().pending_grant.take() else {
            return;
        };
        let Some(me) = self.self_weak.upgrade() else {
            warn!("管理器已释放，忽略连接确认");
            return;
        };
        debug!(target_platform = %grant.target_platform, "对端已确认，开始建立数据面");
        // 状态推进：`Negotiating` → `Connecting`；`mark_active` 会在 `establish` 返回后升到 `Active`。
        self.update_ui(|s| {
            if let Some(active) = s.active.as_mut() {
                if active.session_id == grant.session_id {
                    active.state = SessionState::Connecting;
                }
            }
        });
        let sid = grant.session_id.clone();
        let cancel = grant.cancel.clone();
        let task = async move {
            let result = me
                .controller_task(sid.clone(), grant.token, grant.target_rd_id, grant.params, grant.mode, grant.cmd_rx, cancel)
                .await;
            me.finish_session(&sid, result);
        };
        self.spawn_detached(task);
    }

    /// 控制器会话任务体。
    #[expect(
        clippy::too_many_arguments,
        reason = "会话任务需要完整上下文；再包一层结构体反而增加无谓样板"
    )]
    async fn controller_task(
        self: &Arc<Self>,
        session_id: String,
        token: String,
        target_rd_id: String,
        params: RemoteParams,
        mode: RemoteMode,
        cmd_rx: mpsc::Receiver<ControllerCommand>,
        cancel: CancellationToken,
    ) -> RemoteResult<()> {
        let local_rd_id = self.local_rd_id();
        let handshake = plan_controller_handshake(&session_id, &token, &local_rd_id)?;
        let request = EstablishRequest {
            session_id: session_id.clone(),
            local_rd_id: local_rd_id.clone(),
            peer_rd_id: target_rd_id,
            params,
            prefer_direct: true,
            early: handshake.hello_frame.clone(),
        };
        let duplex = self.cfg.establisher.establish(request, Role::Controller).await?;
        self.mark_active(&session_id);
        // 落地目录可能还不存在（首次使用）；建不出来就让下载直接失败，别让它到 99% 才炸。
        if let Err(err) = tokio::fs::create_dir_all(&self.cfg.download_dir).await {
            warn!(error = %err, dir = %self.cfg.download_dir.display(), "创建下载目录失败");
        }
        let config = ControllerConfig {
            session_id,
            // token 同时用于 KDF 认证，必须与被控端 `RemoteSessionIncoming` 收到的那个一致。
            token,
            local_rd_id,
            mode,
        };
        // 控制器不提供文件服务（`service: None`），只负责发起与落地。
        let transfer = self.build_transfer_hub(None, self.cfg.download_dir.clone(), cancel.clone());
        run_controller(
            duplex,
            config,
            handshake,
            cmd_rx,
            transfer,
            self.frame_sink_arc(),
            cancel,
        )
        .await
    }

    /// 造一个传输状态机并把事件泵到 UI 状态。
    fn build_transfer_hub(
        self: &Arc<Self>,
        service: Option<Arc<FileService>>,
        local_root: PathBuf,
        cancel: CancellationToken,
    ) -> TransferHub {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let hub = TransferHub::new(service, local_root, event_tx);
        if let Some(me) = self.self_weak.upgrade() {
            tokio::spawn(async move {
                pump_transfer_events(me, event_rx, cancel).await;
            });
        }
        hub
    }

    /// 处理 `RemoteSessionIncoming`（被控端）。
    ///
    /// 确认门控：`Auto` 或已信任对端直接接受；`Confirm` 且未信任则**不回响应、不登记 token、
    /// 不派生任务**，只挂起等用户（见 [`Self::approve_pending`] / [`Self::reject_pending`]），
    /// 并在 [`APPROVAL_TIMEOUT`] 后自动拒绝。
    fn handle_incoming(&self, session_id: String, token: String, from_name: String, mode: RemoteMode) {
        if !self.ui_tx.borrow().enabled {
            self.send(rejection(session_id, "被控端未开启远程控制"));
            return;
        }
        let Some((screen, input)) = self.platform_arc() else {
            self.send(rejection(session_id, "被控端缺少屏幕/输入实现"));
            return;
        };
        // 同一时刻只允许一个待确认请求，避免弹窗叠罗汉与「批准了哪一条」的歧义。
        if self.lock_inner().pending_request.is_some() {
            self.send(rejection(session_id, "被控端有待确认的接入请求"));
            return;
        }
        if self.has_active() {
            self.send(rejection(session_id, "被控端已有进行中的会话"));
            return;
        }

        let (policy, trusted) = {
            let ui = self.ui_tx.borrow();
            (
                ui.approval_policy,
                ui.trusted_peers.iter().any(|n| n == &from_name),
            )
        };
        let req = PendingRequest {
            session_id: session_id.clone(),
            token,
            from_name,
            mode,
        };
        if policy == ApprovalPolicy::Auto || trusted {
            self.accept_incoming(req, screen, input);
            return;
        }

        // `Confirm` + 未信任：挂起等用户拍板。
        let timeout = self.lock_inner().approval_timeout;
        let deadline_ms = now_ms().saturating_add(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX));
        let pending_ui = PendingApproval {
            session_id: session_id.clone(),
            from_name: req.from_name.clone(),
            mode,
            deadline_ms,
        };
        self.lock_inner().pending_request = Some(req);
        self.update_ui(move |s| s.pending_approval = Some(pending_ui));
        self.arm_approval_timeout(&session_id);
    }

    /// 真正接受一次接入请求：登记一次性 token → 回 `accepted:true` → 置 active → 派生被控任务。
    fn accept_incoming(
        &self,
        req: PendingRequest,
        screen: Arc<dyn ScreenSource>,
        input: Arc<dyn InputSink>,
    ) {
        // 登记一次性 token（握手时消费），再回 accepted。
        self.lock_inner().pending.insert(PendingSession::new(
            req.session_id.clone(),
            req.token,
            req.from_name.clone(),
            req.mode,
        ));
        self.send(ControlMessage::RemoteSessionResponse {
            session_id: req.session_id.clone(),
            accepted: true,
            reason: None,
        });
        let cancel = CancellationToken::new();
        self.set_active(ActiveHandle {
            session_id: req.session_id.clone(),
            cancel: cancel.clone(),
        });
        let info = ActiveSessionInfo {
            session_id: req.session_id.clone(),
            role: SessionRole::Controlled,
            peer_name: req.from_name,
            mode: req.mode,
            state: SessionState::Connecting,
            started_at_ms: now_ms(),
        };
        self.update_ui(move |s| {
            s.active = Some(info);
        });

        let Some(me) = self.self_weak.upgrade() else {
            return;
        };
        let sid = req.session_id;
        let mode = req.mode;
        let task = async move {
            let result = me.controlled_task(sid.clone(), mode, screen, input, cancel).await;
            me.finish_session(&sid, result);
        };
        self.spawn_detached(task);
    }

    /// 被控端会话任务体。
    async fn controlled_task(
        self: &Arc<Self>,
        session_id: String,
        mode: RemoteMode,
        screen: Arc<dyn ScreenSource>,
        input: Arc<dyn InputSink>,
        cancel: CancellationToken,
    ) -> RemoteResult<()> {
        let pending = Arc::clone(&self.lock_inner().pending);
        let request = EstablishRequest {
            session_id: session_id.clone(),
            local_rd_id: self.local_rd_id(),
            peer_rd_id: String::new(),
            params: self.current_params()?,
            prefer_direct: true,
            early: Vec::new(),
        };
        let duplex = self.cfg.establisher.establish(request, Role::Controlled).await?;
        self.mark_active(&session_id);
        let config = ControlledConfig {
            session_id,
            local_rd_id: self.local_rd_id(),
            mode,
            pending,
            encode: self.cfg.encode,
        };
        let clipboard = self
            .clipboard
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // 被控端只**应答**文件请求：共享目录未配置时传 `None`，一切请求都会被拒。
        //
        // 「仅观看」模式同样不给文件服务——这是权限边界，不只是 UI 偏好：否则控制器能在只读
        // 会话里下载被控端的文件（或写入）。真正的强制点在这里（被控端拒绝），控制器的 UI
        // 隐藏只是顺带的。
        let service = self
            .share
            .as_ref()
            .filter(|_| mode != RemoteMode::ViewOnly)
            .map(|s| Arc::clone(&s.service));
        let local_root = self
            .share
            .as_ref()
            .map_or_else(|| self.cfg.download_dir.clone(), |s| s.path.clone());
        let transfer = self.build_transfer_hub(service, local_root, cancel.clone());
        run_controlled(duplex, config, screen, input, clipboard, transfer, cancel).await
    }

    /// 处理 `RemoteSessionResponse`（控制器侧）。
    ///
    /// - `accepted:true` 命中待确认授权 → 启动数据面；若授权尚未到达（乱序）则先记下，
    ///   等 [`Self::start_controller`] 到达时立即建立。
    /// - 拒绝：清掉待确认授权并结束会话。
    ///
    /// **拒绝路径的 `session_id` 可能是空串**（服务端在会话签发之前就拒绝了：目标离线/未开启
    /// 被控/无 rd_id），因此不能拿它去对号——按「当前待决请求失败」处理即可。
    fn on_session_response(&self, session_id: &str, accepted: bool, reason: Option<String>) {
        if accepted {
            let matches_grant = self
                .lock_inner()
                .pending_grant
                .as_ref()
                .is_some_and(|g| g.session_id == session_id);
            if matches_grant {
                self.begin_controller_session();
            } else if !session_id.is_empty() {
                // 乱序防御：确认先于授权到达，暂存，等授权到达时立即建立。
                self.lock_inner().early_accept = Some(session_id.to_owned());
            }
            return;
        }

        let reason = reason.unwrap_or_else(|| "对端拒绝".to_owned());
        {
            let mut inner = self.lock_inner();
            let matches_grant = inner
                .pending_grant
                .as_ref()
                .is_some_and(|g| g.session_id == session_id);
            if session_id.is_empty() || matches_grant {
                inner.pending_grant = None;
            }
            if inner.early_accept.as_deref() == Some(session_id) {
                inner.early_accept = None;
            }
        }
        if session_id.is_empty() {
            // 尚未签发会话，没有可中断的 active；只记错误。
            self.update_ui(move |s| s.last_error = Some(reason));
            return;
        }
        let is_current = self
            .lock_inner()
            .active
            .as_ref()
            .is_some_and(|a| a.session_id == session_id);
        if !is_current {
            return;
        }
        if let Some(active) = self.take_active() {
            active.cancel.cancel();
        }
        self.lock_cmd().take();
        self.update_ui(move |s| {
            s.active = None;
            s.last_error = Some(reason);
        });
    }

    /// 处理 `RemoteSessionEnd`：中断匹配的会话且**不再回发**结束（避免回声）。
    fn on_remote_session_end(&self, session_id: &str, reason: &str) {
        {
            let mut inner = self.lock_inner();
            if inner
                .pending_grant
                .as_ref()
                .is_some_and(|g| g.session_id == session_id)
            {
                inner.pending_grant = None;
            }
            if inner.early_accept.as_deref() == Some(session_id) {
                inner.early_accept = None;
            }
        }
        let is_current = self
            .lock_inner()
            .active
            .as_ref()
            .is_some_and(|a| a.session_id == session_id);
        if !is_current {
            return;
        }
        if let Some(active) = self.take_active() {
            active.cancel.cancel();
        }
        self.lock_cmd().take();
        // 会话已被服务端判死：挂起记录留着也不会再被消费。
        self.lock_inner().pending.remove(session_id);
        let reason = reason.to_owned();
        self.update_ui(move |s| {
            s.active = None;
            s.last_error = Some(reason);
        });
    }

    /// 取平台实现（若已安装）。
    fn platform_arc(&self) -> Option<(Arc<dyn ScreenSource>, Arc<dyn InputSink>)> {
        self.read_platform()
            .as_ref()
            .map(|p| (Arc::clone(&p.screen), Arc::clone(&p.input)))
    }

    /// 派生一个后台任务；优先用构造时捕获的运行时句柄（同步方法可能不在运行时内调用）。
    fn spawn_detached<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if let Some(rt) = self.rt.clone() {
            rt.spawn(fut);
        } else {
            warn!("无 Tokio 运行时句柄，无法派生远程会话任务");
        }
    }

    /// 取出并拒绝当前待确认的接入请求（无则返回 `false`）。**不登记 token**。
    fn reject_pending_request(&self, reason: &str) -> bool {
        let Some(req) = self.lock_inner().pending_request.take() else {
            return false;
        };
        self.update_ui(|s| s.pending_approval = None);
        self.send(ControlMessage::RemoteSessionResponse {
            session_id: req.session_id,
            accepted: false,
            reason: Some(reason.to_owned()),
        });
        true
    }

    /// 武装被控端确认超时：到点若**仍是同一条**请求，自动拒绝。
    fn arm_approval_timeout(&self, session_id: &str) {
        let Some(rt) = self.rt.clone() else {
            debug!("无 Tokio 运行时句柄，跳过确认超时计时");
            return;
        };
        let timeout = self.lock_inner().approval_timeout;
        let Some(me) = self.self_weak.upgrade() else {
            return;
        };
        let sid = session_id.to_owned();
        rt.spawn(async move {
            tokio::time::sleep(timeout).await;
            me.on_approval_timeout(&sid);
        });
    }

    /// 确认超时到点：仅当同一条请求仍在挂起时拒绝。
    fn on_approval_timeout(&self, session_id: &str) {
        let mut inner = self.lock_inner();
        let is_pending = inner
            .pending_request
            .as_ref()
            .is_some_and(|r| r.session_id == session_id);
        if !is_pending {
            return;
        }
        inner.pending_request = None;
        drop(inner);
        self.update_ui(|s| s.pending_approval = None);
        self.send(ControlMessage::RemoteSessionResponse {
            session_id: session_id.to_owned(),
            accepted: false,
            reason: Some("等待确认超时".to_owned()),
        });
    }

    /// 武装控制器等待确认超时：到点若授权仍未获确认，清理并报错。
    fn arm_wait_timeout(&self, session_id: &str) {
        let Some(rt) = self.rt.clone() else {
            debug!("无 Tokio 运行时句柄，跳过确认等待计时");
            return;
        };
        let timeout = self.lock_inner().approval_wait_timeout;
        let Some(me) = self.self_weak.upgrade() else {
            return;
        };
        let sid = session_id.to_owned();
        rt.spawn(async move {
            tokio::time::sleep(timeout).await;
            me.on_wait_timeout(&sid);
        });
    }

    /// 控制器等待确认超时：清掉授权与 active，记错误。
    fn on_wait_timeout(&self, session_id: &str) {
        let mut inner = self.lock_inner();
        let is_pending = inner
            .pending_grant
            .as_ref()
            .is_some_and(|g| g.session_id == session_id);
        if !is_pending {
            return;
        }
        inner.pending_grant = None;
        if inner.early_accept.as_deref() == Some(session_id) {
            inner.early_accept = None;
        }
        drop(inner);
        // 取出会话句柄（其 `cancel` 与授权共用同一个 token），一并中断，别把 active 留在内部状态里。
        if let Some(active) = self.take_active() {
            active.cancel.cancel();
        }
        self.lock_cmd().take();
        self.update_ui(|s| {
            s.active = None;
            s.last_error = Some("等待对端确认超时".to_owned());
        });
    }
}

/// 造一条拒绝响应。
fn rejection(session_id: String, reason: &str) -> ControlMessage {
    ControlMessage::RemoteSessionResponse {
        session_id,
        accepted: false,
        reason: Some(reason.to_owned()),
    }
}

impl RemoteBridge for RemoteManager {
    fn attach(&self, sender: mpsc::Sender<ControlMessage>) {
        *self.lock_upstream() = Some(sender);
        self.update_ui(|s| s.connected = true);
        // 重连后若已具备能力，重新上报开关与显示器数。
        if self.ui_tx.borrow().capability {
            self.announce();
        }
    }

    fn handle_control_message(&self, msg: ControlMessage) -> bool {
        match msg {
            ControlMessage::RemoteCapability { params, rd_id } => {
                self.on_capability(params, rd_id);
                true
            }
            // 这三个变体是「客户端 → 服务端」方向的，正常不会下发；认领并忽略。
            ControlMessage::RemoteAnnounce { .. }
            | ControlMessage::RemotePeerListRequest
            | ControlMessage::RemoteConnectRequest { .. } => true,
            ControlMessage::RemotePeerList { peers } => {
                self.update_ui(move |s| s.peers = peers);
                true
            }
            ControlMessage::RemoteConnectGrant {
                session_id,
                token,
                target_rd_id,
                target_platform,
                params,
            } => {
                self.start_controller(&session_id, token, target_rd_id, &target_platform, params);
                true
            }
            ControlMessage::RemoteSessionIncoming {
                session_id,
                token,
                from_name,
                mode,
            } => {
                self.handle_incoming(session_id, token, from_name, mode);
                true
            }
            ControlMessage::RemoteSessionResponse {
                session_id,
                accepted,
                reason,
            } => {
                self.on_session_response(&session_id, accepted, reason);
                true
            }
            ControlMessage::RemoteSessionEnd { session_id, reason } => {
                self.on_remote_session_end(&session_id, &reason);
                true
            }
            _ => false,
        }
    }

    fn on_disconnected(&self) {
        *self.lock_upstream() = None;
        if let Some(active) = self.take_active() {
            active.cancel.cancel();
        }
        self.lock_cmd().take();
        // 控制通道已断，服务端会把这侧的会话判死：任何挂起请求/授权都不可能再走通。
        // 待确认的接入请求**只本地清掉、不发拒绝**（没有通道可发；即便发出去，服务端也已
        // 回收该会话，只会记一条「未知 session」），否则用户点「接受」会给一个永远起不来的
        // 会话登记 token 并回 accepted:true。
        {
            let mut inner = self.lock_inner();
            inner.pending_request = None;
            inner.pending_grant = None;
            inner.early_accept = None;
        }
        self.update_ui(|s| {
            s.connected = false;
            s.active = None;
            s.pending_approval = None;
        });
    }
}

/// 把传输事件泵进 UI 状态，直到会话结束（hub 被 drop，通道关闭）或管理器释放。
async fn pump_transfer_events(
    mgr: Arc<RemoteManager>,
    mut rx: mpsc::UnboundedReceiver<TransferEvent>,
    cancel: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => break,
            event = rx.recv() => match event {
                Some(event) => event,
                // hub 已随会话结束被 drop。
                None => break,
            },
        };
        apply_transfer_event(&mgr, event);
    }
}

/// 把一条传输事件写进 [`RemoteUiState`]。
fn apply_transfer_event(mgr: &RemoteManager, event: TransferEvent) {
    mgr.update_ui(|s| match event {
        TransferEvent::Listed { path, entries } => {
            s.remote_dir = path;
            s.remote_entries = entries;
        }
        TransferEvent::Started(progress) => {
            s.transfers.retain(|t| t.id != progress.id);
            s.transfers.push(progress);
            s.transfer_note = None;
        }
        TransferEvent::Progress { id, done, total } => {
            if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                t.done = done;
                t.total = total;
            }
        }
        TransferEvent::Finished { id, path } => {
            s.transfers.retain(|t| t.id != id);
            s.transfer_note = Some(format!("已完成：{path}"));
        }
        TransferEvent::Failed { id, reason } => {
            s.transfers.retain(|t| t.id != id);
            s.transfer_note = Some(format!("失败：{reason}"));
        }
    });
}

/// 当前时间（毫秒，自 epoch）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{DecodedFrame, DisplayInfo};
    use crate::transport::BoxDuplex;
    use async_trait::async_trait;

    /// 一个永不成功的建立器（仅用于状态机单测，不触发真实会话）。
    struct NeverEstablisher;

    #[async_trait]
    impl TransportEstablisher for NeverEstablisher {
        async fn establish(&self, _request: EstablishRequest, _role: Role) -> RemoteResult<BoxDuplex> {
            Err(RemoteError::Signaling("测试用建立器".to_owned()))
        }
    }

    /// 记录 `prepare` 调用的建立器替身（验证预登记时序）。
    #[derive(Default)]
    struct SpyEstablisher {
        /// 已收到的 `prepare` 调用：(本端 rd_id, 会合主机)。
        prepared: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl SpyEstablisher {
        /// 取已记录调用的快照。
        fn calls(&self) -> Vec<(String, String)> {
            self.prepared.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TransportEstablisher for SpyEstablisher {
        async fn prepare(&self, params: &RemoteParams, local_rd_id: &str) -> RemoteResult<()> {
            self.prepared
                .lock()
                .unwrap()
                .push((local_rd_id.to_owned(), params.rendezvous_host.clone()));
            Ok(())
        }

        async fn establish(&self, _request: EstablishRequest, _role: Role) -> RemoteResult<BoxDuplex> {
            Err(RemoteError::Signaling("测试用建立器".to_owned()))
        }
    }

    /// 以指定建立器构造管理器（`server_host` 固定为 `tunnel.example:8080`）。
    fn manager_with(enabled: bool, establisher: Arc<dyn TransportEstablisher>) -> Arc<RemoteManager> {
        RemoteManager::new(RemoteManagerConfig {
            server_host: "tunnel.example:8080".to_owned(),
            self_client_name: "home-nas".to_owned(),
            platform: "linux".to_owned(),
            displays: 1,
            enabled,
            establisher,
            encode: EncodeOptions::default(),
            share_dir: None,
            download_dir: std::path::PathBuf::from("/tmp/rt-remote-test"),
            approval_policy: ApprovalPolicy::Auto,
        })
    }

    /// 轮询等待条件成立（`prewarm` 在派生任务里跑，需要让出调度）。
    async fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        cond()
    }

    /// `RemoteCapability` 到达且本端允许被控时，应**提前**登记数据面。
    ///
    /// 回归用例：hbbs 会静默丢弃「目标尚未登记」的中继请求，而控制器的请求往往在被控端登记
    /// 之前就发出（控制器只需一个 RTT，被控端要等用户/桥接接受）。不预登记则会话必现超时。
    #[tokio::test]
    async fn capability_prewarms_data_plane_when_enabled() {
        let spy = Arc::new(SpyEstablisher::default());
        let mgr = manager_with(true, spy.clone());

        mgr.handle_control_message(ControlMessage::RemoteCapability {
            params: Some(params()),
            rd_id: "123456789".to_owned(),
        });

        assert!(
            wait_until(|| !spy.calls().is_empty()).await,
            "允许被控时应预登记数据面"
        );
        assert_eq!(
            spy.calls(),
            vec![("123456789".to_owned(), "tunnel.example:8080".to_owned())],
            "预登记应带上本端 rd_id 与补齐后的会合主机"
        );
    }

    /// 未开启「允许被远程控制」时不应登记（默认关闭，不该对外宣告在线）。
    #[tokio::test]
    async fn capability_does_not_prewarm_when_disabled() {
        let spy = Arc::new(SpyEstablisher::default());
        let mgr = manager_with(false, spy.clone());

        mgr.handle_control_message(ControlMessage::RemoteCapability {
            params: Some(params()),
            rd_id: "123456789".to_owned(),
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(spy.calls().is_empty(), "未开启被控时不应预登记");
    }

    /// 先收到能力、后打开开关：打开时应补登记（托盘/UI 切换路径）。
    #[tokio::test]
    async fn enabling_after_capability_prewarms() {
        let spy = Arc::new(SpyEstablisher::default());
        let mgr = manager_with(false, spy.clone());

        mgr.handle_control_message(ControlMessage::RemoteCapability {
            params: Some(params()),
            rd_id: "123456789".to_owned(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(spy.calls().is_empty(), "关闭状态不应预登记");

        mgr.set_enabled(true);
        assert!(
            wait_until(|| !spy.calls().is_empty()).await,
            "打开开关应补一次预登记"
        );
        assert_eq!(spy.calls().len(), 1, "只应登记一次");
    }

    /// 记录调用的假 FrameSink。
    #[derive(Default)]
    struct RecordingSink;

    impl FrameSink for RecordingSink {
        fn on_displays(&self, _displays: Vec<DisplayInfo>) {}
        fn on_frame(&self, _frame: DecodedFrame) {}
        fn on_closed(&self, _reason: String) {}
    }

    fn manager(enabled: bool) -> Arc<RemoteManager> {
        RemoteManager::new(RemoteManagerConfig {
            server_host: "tunnel.example:8080".to_owned(),
            self_client_name: "home-nas".to_owned(),
            platform: "linux".to_owned(),
            displays: 1,
            enabled,
            establisher: Arc::new(NeverEstablisher),
            encode: EncodeOptions::default(),
            share_dir: None,
            download_dir: std::path::PathBuf::from("/tmp/rt-remote-test"),
            approval_policy: ApprovalPolicy::Auto,
        })
    }

    fn params() -> RemoteParams {
        RemoteParams {
            rendezvous_host: String::new(),
            rendezvous_port: 21116,
            relay_port: 21117,
            relay_server: String::new(),
            public_key: "k".into(),
            always_use_relay: false,
        }
    }

    #[test]
    fn capability_triggers_announce_with_expected_fields() {
        let mgr = manager(true);
        let (tx, mut rx) = mpsc::channel(8);
        mgr.attach(tx);

        let consumed = mgr.handle_control_message(ControlMessage::RemoteCapability {
            params: Some(params()),
            rd_id: "123456789".to_owned(),
        });
        assert!(consumed, "远程变体应被消费");

        let sent = rx.try_recv().expect("应回发 RemoteAnnounce");
        match sent {
            ControlMessage::RemoteAnnounce {
                enabled,
                platform,
                displays,
            } => {
                assert!(enabled, "enabled 应来自初始配置");
                assert_eq!(platform, "linux");
                assert_eq!(displays, 1);
            }
            other => panic!("应为 RemoteAnnounce，实得 {other:?}"),
        }

        // rd_id 与 capability 应写入 UI 状态，rendezvous_host 用本机 server 补齐。
        let snap = mgr.snapshot();
        assert_eq!(snap.rd_id, "123456789");
        assert!(snap.capability);
        assert_eq!(mgr.current_params().unwrap().rendezvous_host, "tunnel.example:8080");
    }

    #[test]
    fn non_remote_messages_are_not_consumed() {
        let mgr = manager(true);
        // 明确的非远程消息返回 false（交给调用方的兜底分支）。
        assert!(!mgr.handle_control_message(ControlMessage::ClientMappingSummary {
            summary: rust_tunnel_common::protocol::MappingSummary {
                connected_at: None,
                active_tunnels: 0,
                rtt_ms: None,
                rules: Vec::new(),
                truncated: false,
            },
        }));
    }

    #[tokio::test]
    async fn all_nine_remote_variants_are_consumed() {
        let mgr = manager(true);
        let variants = vec![
            ControlMessage::RemoteCapability {
                params: None,
                rd_id: String::new(),
            },
            ControlMessage::RemoteAnnounce {
                enabled: false,
                platform: "linux".into(),
                displays: 0,
            },
            ControlMessage::RemotePeerListRequest,
            ControlMessage::RemotePeerList { peers: vec![] },
            ControlMessage::RemoteConnectRequest {
                target_client: "peer".into(),
                mode: RemoteMode::Control,
            },
            ControlMessage::RemoteConnectGrant {
                session_id: "s".into(),
                token: "t".into(),
                target_rd_id: "1".into(),
                target_platform: "linux".into(),
                params: params(),
            },
            ControlMessage::RemoteSessionIncoming {
                session_id: "s".into(),
                token: "t".into(),
                from_name: "peer".into(),
                mode: RemoteMode::Control,
            },
            ControlMessage::RemoteSessionResponse {
                session_id: "s".into(),
                accepted: true,
                reason: None,
            },
            ControlMessage::RemoteSessionEnd {
                session_id: "s".into(),
                reason: "x".into(),
            },
        ];
        for msg in variants {
            assert!(mgr.handle_control_message(msg.clone()), "变体应被消费: {msg:?}");
        }
    }

    #[test]
    fn set_enabled_announces_and_updates_state() {
        let mgr = manager(false);
        let (tx, mut rx) = mpsc::channel(8);
        mgr.attach(tx);
        mgr.handle_control_message(ControlMessage::RemoteCapability {
            params: Some(params()),
            rd_id: "1".into(),
        });
        let _ = rx.try_recv(); // 丢弃 capability 触发的 announce

        mgr.set_enabled(true);
        assert!(mgr.snapshot().enabled);
        match rx.try_recv().expect("set_enabled 应上报") {
            ControlMessage::RemoteAnnounce { enabled, .. } => assert!(enabled),
            other => panic!("应为 RemoteAnnounce，实得 {other:?}"),
        }
    }

    /// 控制器会话的 `peer_name` 应是**展示名**，不是服务端下发的 9 位 `rd_id`。
    #[tokio::test]
    async fn grant_resolves_peer_display_name_from_list() {
        let mgr = manager(true);
        mgr.handle_control_message(ControlMessage::RemoteCapability {
            params: Some(params()),
            rd_id: "111111111".into(),
        });
        mgr.handle_control_message(ControlMessage::RemotePeerList {
            peers: vec![RemotePeerInfo {
                client_name: "office-pc".into(),
                display_name: "Office PC".into(),
                platform: "windows".into(),
                displays: 2,
                rd_id: "987654321".into(),
            }],
        });
        mgr.handle_control_message(ControlMessage::RemoteConnectGrant {
            session_id: "s1".into(),
            token: "t1".into(),
            target_rd_id: "987654321".into(),
            target_platform: "windows".into(),
            params: params(),
        });

        let active = mgr.snapshot().active.expect("应进入控制器会话");
        assert_eq!(active.peer_name, "Office PC", "应显示对端展示名");
    }

    /// 列表里查不到（对端刚离线）时回落到 `rd_id`，而不是留空。
    #[tokio::test]
    async fn grant_falls_back_to_rd_id_when_peer_unknown() {
        let mgr = manager(true);
        mgr.handle_control_message(ControlMessage::RemoteCapability {
            params: Some(params()),
            rd_id: "111111111".into(),
        });
        mgr.handle_control_message(ControlMessage::RemoteConnectGrant {
            session_id: "s1".into(),
            token: "t1".into(),
            target_rd_id: "987654321".into(),
            target_platform: "windows".into(),
            params: params(),
        });

        let active = mgr.snapshot().active.expect("应进入控制器会话");
        assert_eq!(active.peer_name, "987654321");
    }

    #[test]
    fn peer_list_updates_state() {
        let mgr = manager(true);
        mgr.handle_control_message(ControlMessage::RemotePeerList {
            peers: vec![RemotePeerInfo {
                client_name: "peer-a".into(),
                display_name: "Peer A".into(),
                platform: "macos".into(),
                displays: 2,
                rd_id: "987654321".into(),
            }],
        });
        let peers = mgr.snapshot().peers;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].client_name, "peer-a");
    }

    #[test]
    fn request_connect_without_capability_sets_error() {
        let mgr = manager(true);
        let (tx, mut rx) = mpsc::channel(8);
        mgr.attach(tx);
        mgr.request_connect("peer-a", RemoteMode::Control);
        assert!(mgr.snapshot().last_error.is_some(), "无能力时应记错误");
        assert!(rx.try_recv().is_err(), "不应发出连接请求");
    }

    #[test]
    fn incoming_while_disabled_is_rejected() {
        let mgr = manager(false);
        let (tx, mut rx) = mpsc::channel(8);
        mgr.attach(tx);
        mgr.handle_control_message(ControlMessage::RemoteSessionIncoming {
            session_id: "s1".into(),
            token: "t".into(),
            from_name: "peer".into(),
            mode: RemoteMode::Control,
        });
        match rx.try_recv().expect("应回一条拒绝") {
            ControlMessage::RemoteSessionResponse {
                accepted, reason, ..
            } => {
                assert!(!accepted);
                assert!(reason.is_some());
            }
            other => panic!("应为 RemoteSessionResponse，实得 {other:?}"),
        }
    }

    #[test]
    fn incoming_without_platform_is_rejected() {
        let mgr = manager(true);
        let (tx, mut rx) = mpsc::channel(8);
        mgr.attach(tx);
        mgr.handle_control_message(ControlMessage::RemoteSessionIncoming {
            session_id: "s1".into(),
            token: "t".into(),
            from_name: "peer".into(),
            mode: RemoteMode::Control,
        });
        match rx.try_recv().expect("应回一条拒绝") {
            ControlMessage::RemoteSessionResponse { accepted, .. } => assert!(!accepted),
            other => panic!("应为 RemoteSessionResponse，实得 {other:?}"),
        }
    }

    #[test]
    fn on_disconnected_clears_connection_state() {
        let mgr = manager(true);
        let (tx, _rx) = mpsc::channel(8);
        mgr.attach(tx);
        assert!(mgr.snapshot().connected);
        mgr.on_disconnected();
        assert!(!mgr.snapshot().connected);
        assert!(mgr.snapshot().active.is_none());
    }

    #[test]
    fn frame_sink_default_is_installed() {
        let mgr = manager(true);
        // 未安装 sink 时不应 panic（走 NullFrameSink）。
        let _ = mgr.frame_sink_arc();
        mgr.install_frame_sink(Arc::new(RecordingSink));
    }
}
