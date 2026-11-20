//! 会话内数据面路径迁移（协议 v12）：把一场已经建立的中继会话无缝换到 P2P 打洞路径。
//!
//! # 为什么需要它
//!
//! 数据面路径（打洞 / 直连 / 中继）原本只在建立时选定一次。一次瞬时打洞失败会让**整场**
//! 会话走中继，用户唯一的补救手段是重启客户端——而打洞本身是尽力而为的，失败往往只是
//! 一次丢包或一次窗口错开。
//!
//! # 核心不变量（改这个文件之前先读完）
//!
//! 1. **提交点**：[`SessionMessage::PathFreeze`] 一旦由控制器入队，迁移即**不可回退**。
//!    此后任何超时/失败都按会话故障处理（交给既有断线重连），绝不回滚旧流。理由：被控端
//!    收到 `PathFreeze` 后会把写方向切到新流，回滚无法让两端的 AEAD 计数器重新对齐。
//!    提交点之前的任何失败都可以用 [`SessionMessage::PathAbort`] 安全放弃。
//! 2. **写方向切换只由 `writer_loop` 触发**，且只在它自己那条屏障帧 `send()` 成功 flush
//!    之后、下一帧开始之前。理由：`write_paced` 一帧要多次 `poll_write`，从外部切会把一帧
//!    撕到两条流上。
//! 3. **读方向切换只由会话循环在解码完对端的屏障帧之后触发**。对端在屏障帧之后不会再写旧流。
//! 4. 新流必须在 `PathMigrateReady` / `PathFreeze` 之前完成 `install_pending`（本模块的
//!    `MigrateMachine` 与 `run_establish` 的顺序保证了这一点）。
//!
//! 只要以上成立，每个方向在旧流上的最后一条字节就是屏障帧（被控端→控制器是
//! `PathFrozen`，控制器→被控端是 `PathCommit`），两个方向的 AEAD 计数器在中继与新流之间
//! **严格连续**——`SessionCodec` 与密钥都不需要重建。
//!
//! # 建立与 `PathMigrateReady` 的时序（别改成「等 Ready 再建立」）
//!
//! 控制器的建立任务（发 SYN / 连 TCP）与 `PathMigratePrepare` **同时**发起；被控端收到
//! Prepare 后才注册 accept。若改成「等 `PathMigrateReady` 再建立」，被控端的 accept 要等一个
//! SYN，而 SYN 又要等 Ready——两端互等，真机永远走不到下一步（内存 mock 流因为瞬时完成，
//! 测不出来）。`PathMigrateReady` 只承担一件事：告诉控制器「对端已经 `install_pending`，
//! 可以接受 `PathFreeze` 了」。控制器必须同时满足「本端建立完成」与「已收到 Ready」才提交。
//!
//! # 新流身份：打洞靠 `conn_id`，直连靠一条前导帧
//!
//! **打洞路径**不做额外的应用层握手：可靠层的 `SYN`/`SYN|ACK` 已经证明双向可达，流身份由
//! `conn_id = HMAC-SHA256(token, "rt-migrate-v1" ‖ attempt)` 绑定（与初始打洞同一套机制，
//! 每次尝试不同）。再加一层握手不会增加安全性（会话帧本身还是 AEAD 加密的），又会引入
//! 「install 与 ACK 谁先」的竞态，故有意不做。
//!
//! **直连路径（同内网 TCP）没有这层**：目标端的直连监听端口对任何能到达它的机器都是敞开的，
//! 裸 TCP 连接本身不携带任何身份。若不在 `install_pending` 之前验证来源，一个无关连接就会被
//! 当成迁移流装进会话，首帧 AEAD 认证失败、会话直接断掉（而且是在**提交点之前**就把会话
//! 搞死）。故直连迁移先交换一条极小的前导帧（`RTMG` | 版本 | 角色 | attempt | MAC），MAC 密钥
//! 是 `HMAC-SHA256(token, "rt-migrate-hello-v1")`（[`crate::punch::migration_hello_key`]），
//! 被控端对不匹配的连接直接丢弃并继续等下一条。
//!
//! # 重试与退避
//!
//! 每场会话最多自动尝试 `MigrateTiming::max_attempts` 次（默认 5），首次在会话
//! 建立 30s 后，之后按 30s / 60s / 2min / 5min 退避；用尽后不再自动尝试。用户的手动
//! 「立即尝试 P2P」（`MigrateNow`）**不计入**这个上限，也不占用退避表——手动是显式意图，
//! 不该被自动策略锁死。

use crate::error::RemoteError;
use crate::punch::{conn_id_for_migration, migration_hello_key};
use crate::reliable::ReliableMetrics;
use crate::session::SessionMessage;
use crate::stats::{SessionStats, SessionStatsReporter};
use crate::transport::{BoxDuplex, MigrateRequest, Role, TransportEstablisher, TransportPath};
use rust_tunnel_common::protocol::RemoteParams;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// 读/写半边（切换前先拆好，避免在 poll 里做 `tokio::io::split`）。
type ReadSide = Box<dyn AsyncRead + Send + Unpin>;
type WriteSide = Box<dyn AsyncWrite + Send + Unpin>;

/// 迁移包装的内部状态。
struct Core {
    /// 当前读源。
    rd: ReadSide,
    /// 当前写目标。
    wr: WriteSide,
    /// 新流的读/写半边（已 `install`，等待 `activate_*`）。
    pending_rd: Option<ReadSide>,
    pending_wr: Option<WriteSide>,
    /// 已登记、等待 `install_pending` 的 attempt。
    want: Option<u32>,
    /// `pending_*` 属于哪个 attempt。
    attempt: u32,
    /// `pending_*` 的实际路径（`Punch` / `Direct`）。
    pending_path: Option<TransportPath>,
    /// `pending_*` 的可靠层指标（打洞路径才有）。
    pending_metrics: Option<Arc<ReliableMetrics>>,
    read_swapped: bool,
    write_swapped: bool,
}

impl Core {
    fn lock(core: &Mutex<Core>) -> MutexGuard<'_, Core> {
        // 与 `manager.rs` 的既有风格一致：锁中毒按「没有损坏」处理。这里的临界区都是
        // 纯内存操作，即便前一个持锁者 panic 也不会留下半更新的结构。
        core.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 可换内芯的双向字节流。
///
/// 作为 `BoxDuplex` 传给会话循环；会话照旧 `tokio::io::split` 成读写两半，读半在
/// `FramedRead` 里、写半在 `writer_loop` 里。两个半的 `poll_*` 由外层 `split` 的 BiLock
/// 串行化，因此本结构内部用一把短临界区互斥锁就够，不需要命令队列或存储 waker。
pub(crate) struct MigratableDuplex {
    core: Arc<Mutex<Core>>,
}

impl MigratableDuplex {
    /// 包住一条已建立的流，返回（包装, 迁移句柄）。
    #[must_use]
    pub(crate) fn new(inner: BoxDuplex, role: Role) -> (Self, MigrateHandle) {
        let (rd, wr) = tokio::io::split(inner);
        let core = Arc::new(Mutex::new(Core {
            rd: Box::new(rd),
            wr: Box::new(wr),
            pending_rd: None,
            pending_wr: None,
            want: None,
            attempt: 0,
            pending_path: None,
            pending_metrics: None,
            read_swapped: false,
            write_swapped: false,
        }));
        (
            Self {
                core: Arc::clone(&core),
            },
            MigrateHandle { core, role },
        )
    }
}

impl AsyncRead for MigratableDuplex {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        // 持锁跨越一次 inner poll：inner 的 poll 是**非阻塞**的（Pending 立即返回），
        // 外层 BiLock 又保证读写不会同时进来，所以这里不会长期占用锁。
        let mut core = Core::lock(&self.core);
        Pin::new(&mut *core.rd).poll_read(cx, buf)
    }
}

impl AsyncWrite for MigratableDuplex {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let mut core = Core::lock(&self.core);
        Pin::new(&mut *core.wr).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut core = Core::lock(&self.core);
        Pin::new(&mut *core.wr).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut core = Core::lock(&self.core);
        Pin::new(&mut *core.wr).poll_shutdown(cx)
    }
}

/// `install_pending` 失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigrateInstallError {
    /// 没有登记这个 attempt（例如已被 abort，或 attempt 号不符）。
    NotWanted,
}

impl std::fmt::Display for MigrateInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWanted => f.write_str("本次迁移尝试已失效（未登记或 attempt 不符）"),
        }
    }
}

/// 会话循环 / 建立任务 / 写任务共享的迁移句柄。
#[derive(Clone)]
pub(crate) struct MigrateHandle {
    core: Arc<Mutex<Core>>,
    role: Role,
}

impl std::fmt::Debug for MigrateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrateHandle").field("role", &self.role).finish_non_exhaustive()
    }
}

impl MigrateHandle {
    /// 登记一次尝试：此后只接受该 attempt 的 `install_pending`。
    pub(crate) fn want(&self, attempt: u32) {
        let mut core = Core::lock(&self.core);
        core.want = Some(attempt);
    }

    /// 把新流拆成读写半边放进 pending 槽位。**必须在通知对端之前调用**。
    pub(crate) fn install_pending(
        &self,
        attempt: u32,
        stream: BoxDuplex,
        path: TransportPath,
        metrics: Option<Arc<ReliableMetrics>>,
    ) -> Result<(), MigrateInstallError> {
        let mut core = Core::lock(&self.core);
        if core.want != Some(attempt) {
            return Err(MigrateInstallError::NotWanted);
        }
        let (rd, wr) = tokio::io::split(stream);
        core.pending_rd = Some(Box::new(rd));
        core.pending_wr = Some(Box::new(wr));
        core.attempt = attempt;
        core.pending_path = Some(path);
        core.pending_metrics = metrics;
        core.want = None;
        Ok(())
    }

    /// 把读方向切到 pending 流（会话读任务调用）。返回是否真的切了。
    pub(crate) fn activate_read(&self, attempt: u32) -> bool {
        let mut core = Core::lock(&self.core);
        if core.attempt != attempt {
            return false;
        }
        let Some(rd) = core.pending_rd.take() else {
            return false;
        };
        core.rd = rd; // 旧读半在这里被 drop
        core.read_swapped = true;
        true
    }

    /// 把写方向切到 pending 流（**只允许 `writer_loop` 在帧边界调用**）。
    pub(crate) fn activate_write(&self, attempt: u32) -> bool {
        let mut core = Core::lock(&self.core);
        if core.attempt != attempt {
            return false;
        }
        let Some(wr) = core.pending_wr.take() else {
            return false;
        };
        core.wr = wr; // 旧写半在这里被 drop
        core.write_swapped = true;
        true
    }

    /// 放弃一次尝试：丢掉 pending 半边与其指标。
    pub(crate) fn clear_pending(&self, attempt: u32) {
        let mut core = Core::lock(&self.core);
        if core.attempt != attempt && core.want != Some(attempt) {
            return;
        }
        core.pending_rd = None;
        core.pending_wr = None;
        core.pending_path = None;
        core.pending_metrics = None;
        core.want = None;
    }

    /// 该帧是否是**本端写方向**的屏障帧；返回其 attempt。
    ///
    /// 被控端写 `PathFrozen`、控制器写 `PathCommit`——各自都是本端在旧流上的最后一条字节。
    #[must_use]
    pub(crate) fn write_barrier(&self, msg: &SessionMessage) -> Option<u32> {
        match (self.role, msg) {
            (Role::Controlled, SessionMessage::PathFrozen { attempt })
            | (Role::Controller, SessionMessage::PathCommit { attempt }) => Some(*attempt),
            _ => None,
        }
    }

    /// 新流**实际**走通的路径（`install_pending` 之后才有值）。
    ///
    /// 控制器的路径上报以它为准，而不是状态机事件里的字面值——包装层看到的才是真相。
    #[must_use]
    pub(crate) fn pending_path(&self) -> Option<TransportPath> {
        Core::lock(&self.core).pending_path
    }

    /// 迁移流的可靠层指标（打洞路径才有）；控制器在切换后用它继续上报传输读数。
    #[must_use]
    pub(crate) fn pending_metrics(&self) -> Option<Arc<ReliableMetrics>> {
        Core::lock(&self.core).pending_metrics.clone()
    }
}

/// 迁移的时间参数（单测可注入）。
#[derive(Debug, Clone)]
pub(crate) struct MigrateTiming {
    /// 会话建立后等待多久再发起第一次尝试（给 QoS/心跳留出稳定时间）。
    pub first_delay: Duration,
    /// 失败后的退避序列；用尽后沿用最后一项（直到次数上限）。
    pub retry_backoff: Vec<Duration>,
    /// **自动**尝试次数上限（不含手动触发）。
    pub max_attempts: u32,
    /// 单次建立（信令 + 打洞握手）的预算。
    pub establish_timeout: Duration,
    /// 每个状态的最长等待。**必须小于 `session::LIVENESS_TIMEOUT`(4s)**。
    pub state_timeout: Duration,
}

impl Default for MigrateTiming {
    fn default() -> Self {
        Self {
            first_delay: Duration::from_secs(30),
            // 第一次失败后 30s、之后 60s / 2min / 5min。十分钟里最多试 5 次——足够覆盖一次
            // 瞬时失败，又不会在真的打不了洞的链路上持续骚扰对端。
            retry_backoff: vec![
                Duration::from_secs(30),
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(300),
            ],
            max_attempts: 5,
            establish_timeout: Duration::from_secs(10),
            state_timeout: Duration::from_secs(2),
        }
    }
}

/// 会话内迁移的运行时上下文。
pub(crate) struct MigrateRuntime {
    /// 本端角色。
    pub role: Role,
    /// 建立器（只用于 [`TransportEstablisher::migrate`]）。
    pub establisher: Arc<dyn TransportEstablisher>,
    /// 会合/中继参数。
    pub params: RemoteParams,
    /// 会话 ID（仅日志/统计）。
    pub session_id: String,
    /// 本端 RustDesk ID。
    pub local_rd_id: String,
    /// 对端 RustDesk ID（被控端为空）。
    pub peer_rd_id: String,
    /// 会话一次性 token（派生迁移 `conn_id`）。被控端在 `authorize` 之后才能填。
    pub token: std::sync::OnceLock<Arc<str>>,
    /// 建立时的实际路径（只有 `Relay` 才值得尝试迁移）。
    pub initial_path: TransportPath,
    /// 统计上报端。
    pub stats: SessionStatsReporter,
    /// 会话的取消令牌：会话结束时立刻放弃进行中的建立任务。
    ///
    /// 没有它的话，建立任务最长会活到 `establish_timeout`（10s）。被控端的直连候选会一直
    /// 持有 `direct_rx` 的互斥锁（`next_direct` 跨 await 持锁），下一场会话的直连候选会被
    /// 它挡住——虽然仍能从打洞/中继建立，但不该让陈旧任务干扰新会话。
    pub cancel: CancellationToken,
    /// 时间参数。
    pub timing: MigrateTiming,
}

impl MigrateRuntime {
    /// 填入本端 token（重复调用只保留第一次）。
    pub(crate) fn set_token(&self, token: &str) {
        let _ = self.token.set(Arc::from(token));
    }
}

/// 建立任务 → 会话循环的事件。
#[derive(Debug)]
pub(crate) enum MigrateEvent {
    /// 新流已建立并 `install_pending` 完成。
    Established { attempt: u32, path: TransportPath },
    /// 建立失败（此时尚未提交，可以安全放弃）。
    EstablishFailed { attempt: u32, reason: String },
    /// 写任务已把写方向切到新流（屏障帧已 flush）。
    WriteActivated { attempt: u32 },
}

/// 状态机交给会话循环执行的动作。
///
/// `Send` 携带完整的 [`SessionMessage`]（含 `Vec` 数据），比其它变体大得多；这些动作每次
/// 迁移只有个位数、且立刻被消费，装箱反而多一次分配，故有意不 Box。
#[expect(
    clippy::large_enum_variant,
    reason = "迁移动作每次至多几条、立即消费；为 272 字节的 Send 变体装箱没有收益"
)]
#[derive(Debug)]
pub(crate) enum MigrateAction {
    /// 通过控制车道发一条会话消息。
    Send(SessionMessage),
    /// 派生建立任务。
    SpawnEstablish(u32),
    /// 切读方向（由会话读任务执行）。
    ActivateRead(u32),
    /// 清掉 pending 半边。
    ClearPending(u32),
    /// 上报迁移后的实际路径。
    ReportPath(TransportPath),
    /// 不可恢复：按会话故障处理。
    Fatal(String),
}

/// 迁移状态机的内部状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 空闲（可能在等下一次尝试）。
    Idle,
    /// 双方：本端建立任务在跑。控制器同时等对端的 `PathMigrateReady`（两个条件齐了才提交）。
    Establishing(u32),
    /// 被控端：新流已装好、`PathMigrateReady` 已发，等 `PathFreeze`。
    Ready(u32),
    /// 控制器：`PathFreeze` 已发（**提交点**），等 `PathFrozen`。
    Committed(u32),
    /// 被控端：`PathFrozen` 已发（**提交点**），等 `PathCommit`。
    Frozen(u32),
    /// 控制器：`PathCommit` 已发，等写任务 `WriteActivated`。
    WriteWait(u32),
    /// 迁移完成。
    Done,
}

/// 纯状态机（无 I/O，便于表驱动单测）。
pub(crate) struct MigrateMachine {
    role: Role,
    timing: MigrateTiming,
    state: State,
    /// 当前/最近一次 attempt 号。
    attempt: u32,
    /// 已用掉的自动尝试次数。
    attempts: u32,
    /// 提交点之前才允许放弃。
    abortable: bool,
    /// 控制器：本端建立任务已完成（收到 `Established`）。
    self_established: bool,
    /// 控制器：对端已装好新流（收到 `PathMigrateReady`）。
    peer_ready: bool,
    /// 下一次自动尝试的时刻。
    next_at: Option<Instant>,
    /// 当前状态的超时时刻。
    deadline: Option<Instant>,
}

impl MigrateMachine {
    /// 构造。
    #[must_use]
    pub(crate) fn new(role: Role, timing: MigrateTiming) -> Self {
        let next_at = Some(Instant::now() + timing.first_delay);
        Self {
            role,
            timing,
            state: State::Idle,
            attempt: 0,
            attempts: 0,
            abortable: false,
            self_established: false,
            peer_ready: false,
            next_at,
            deadline: None,
        }
    }

    /// 下一次自动尝试时刻（会话循环据此决定是否还需要 tick）。
    #[must_use]
    pub(crate) fn next_attempt_at(&self) -> Option<Instant> {
        self.next_at
    }

    /// 是否已完成。
    #[must_use]
    pub(crate) fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// 是否空闲（没有进行中的尝试）。
    #[must_use]
    pub(crate) fn is_idle(&self) -> bool {
        self.state == State::Idle
    }

    fn enter(&mut self, state: State, now: Instant) {
        self.state = state;
        self.deadline = match state {
            State::Idle | State::Done => None,
            // `Establishing` 覆盖的是**建立任务**（信令 + 打洞/直连握手），不是一次消息往返：
            // 它的预算是 `establish_timeout`（默认 10s）。用 2s 的 `state_timeout` 会在正常慢
            // 链路上把合法尝试全部掐死——打洞单项预算就有 3s，直连要 3s 连接 + 2s 前导。
            // `Ready`（被控端）同理：等的是对端**建立完成**后才会发出的 `PathFreeze`。
            State::Establishing(_) | State::Ready(_) => Some(now + self.timing.establish_timeout),
            _ => Some(now + self.timing.state_timeout),
        };
    }

    /// 周期驱动：到点发起尝试、或检查当前状态的超时。
    pub(crate) fn on_tick(&mut self, now: Instant) -> Vec<MigrateAction> {
        if matches!(self.state, State::Idle) {
            if let Some(at) = self.next_at {
                if now >= at {
                    return self.start_attempt(now, false);
                }
            }
            return Vec::new();
        }
        if self.deadline.is_some_and(|d| now >= d) {
            return self.on_deadline(now);
        }
        Vec::new()
    }

    /// 手动触发（`MigrateNow`）：忽略退避与次数上限，但仍在空闲时才启动。
    pub(crate) fn force_start(&mut self, now: Instant) -> Vec<MigrateAction> {
        if !matches!(self.state, State::Idle) {
            return Vec::new();
        }
        self.start_attempt(now, true)
    }

    fn start_attempt(&mut self, now: Instant, forced: bool) -> Vec<MigrateAction> {
        // 自动尝试有次数上限；手动触发（`MigrateNow`）永远可以再来一次。
        if !forced && self.attempts >= self.timing.max_attempts {
            self.next_at = None;
            return Vec::new();
        }
        self.attempt = self.attempt.saturating_add(1);
        if !forced {
            self.attempts = self.attempts.saturating_add(1);
        }
        self.next_at = None;
        self.abortable = true;
        self.self_established = false;
        self.peer_ready = false;
        match self.role {
            Role::Controller => {
                let attempt = self.attempt;
                self.enter(State::Establishing(attempt), now);
                // **建立与 Prepare 同时发起**：被控端只在收到 Prepare 后才注册 accept，
                // 而控制器的 SYN 每 250 ms 重发、预算 10s，这点注册延迟由重试覆盖。
                // 若反过来「等 Ready 再建立」，被控端的 accept 要等 SYN、SYN 又要等 Ready
                // ——两端互等，真机上永远走不到下一步。
                vec![
                    MigrateAction::Send(SessionMessage::PathMigratePrepare { attempt }),
                    MigrateAction::SpawnEstablish(attempt),
                ]
            }
            Role::Controlled => {
                // 被控端由对端的 `PathMigratePrepare` 驱动；手动触发对它无意义。
                Vec::new()
            }
        }
    }

    /// 处理一条迁移消息；非迁移消息返回 `None`。
    pub(crate) fn on_message(&mut self, msg: &SessionMessage, now: Instant) -> Option<Vec<MigrateAction>> {
        match msg {
            SessionMessage::PathMigratePrepare { attempt } => Some(self.on_prepare(*attempt, now)),
            SessionMessage::PathMigrateReady { attempt } => Some(self.on_ready(*attempt, now)),
            SessionMessage::PathFreeze { attempt } => Some(self.on_freeze(*attempt, now)),
            SessionMessage::PathFrozen { attempt } => Some(self.on_frozen(*attempt, now)),
            SessionMessage::PathCommit { attempt } => Some(self.on_commit(*attempt, now)),
            SessionMessage::PathAbort { attempt, reason } => Some(self.on_abort(*attempt, reason, now)),
            _ => None,
        }
    }

    /// 建立任务的结果。
    pub(crate) fn on_event(&mut self, event: MigrateEvent, now: Instant) -> Vec<MigrateAction> {
        match event {
            MigrateEvent::Established { attempt, path } => self.on_established(attempt, path, now),
            MigrateEvent::EstablishFailed { attempt, reason } => self.on_establish_failed(attempt, &reason, now),
            MigrateEvent::WriteActivated { attempt } => self.on_write_activated(attempt, now),
        }
    }

    fn on_prepare(&mut self, attempt: u32, now: Instant) -> Vec<MigrateAction> {
        if self.role != Role::Controlled {
            return Vec::new();
        }
        if !matches!(self.state, State::Idle) || attempt <= self.attempt {
            return vec![MigrateAction::Send(SessionMessage::PathAbort {
                attempt,
                reason: "本端正忙或 attempt 过期".to_owned(),
            })];
        }
        self.attempt = attempt;
        self.attempts = self.attempts.saturating_add(1);
        self.abortable = true;
        self.enter(State::Establishing(attempt), now);
        vec![MigrateAction::SpawnEstablish(attempt)]
    }

    fn on_ready(&mut self, attempt: u32, now: Instant) -> Vec<MigrateAction> {
        if self.role != Role::Controller || self.state != State::Establishing(attempt) {
            return Vec::new();
        }
        // 对端已 `install_pending`，可以接受 `PathFreeze` 了。但只有本端建立也完成时才能提交。
        self.peer_ready = true;
        if self.self_established {
            self.commit(attempt, now)
        } else {
            Vec::new()
        }
    }

    /// 控制器：提交（发 `PathFreeze`，从此不可回退）。
    fn commit(&mut self, attempt: u32, now: Instant) -> Vec<MigrateAction> {
        self.abortable = false;
        self.enter(State::Committed(attempt), now);
        vec![MigrateAction::Send(SessionMessage::PathFreeze { attempt })]
    }

    fn on_established(&mut self, attempt: u32, _path: TransportPath, now: Instant) -> Vec<MigrateAction> {
        match self.role {
            Role::Controller if self.state == State::Establishing(attempt) => {
                self.self_established = true;
                // 对端还没装好就发 Freeze，会把对端逼进「收到 Freeze 却没有流」的致命分支。
                if self.peer_ready {
                    self.commit(attempt, now)
                } else {
                    Vec::new()
                }
            }
            Role::Controlled if self.state == State::Establishing(attempt) => {
                // 新流已 install，才通知对端可以冻结。
                self.enter(State::Ready(attempt), now);
                vec![MigrateAction::Send(SessionMessage::PathMigrateReady { attempt })]
            }
            _ => Vec::new(),
        }
    }

    fn on_establish_failed(&mut self, attempt: u32, reason: &str, now: Instant) -> Vec<MigrateAction> {
        if !self.abortable || self.state == State::Idle {
            return Vec::new();
        }
        if self.attempt != attempt {
            return Vec::new();
        }
        self.schedule_retry(now);
        vec![
            MigrateAction::ClearPending(attempt),
            MigrateAction::Send(SessionMessage::PathAbort {
                attempt,
                reason: reason.to_owned(),
            }),
        ]
    }

    fn on_freeze(&mut self, attempt: u32, now: Instant) -> Vec<MigrateAction> {
        if self.role != Role::Controlled {
            return Vec::new();
        }
        if self.state != State::Ready(attempt) {
            // 控制器已经提交，本端却没有可切换的流：无法安全回退。
            return vec![MigrateAction::Fatal(format!(
                "收到 PathFreeze(attempt={attempt}) 时没有可用的迁移流"
            ))];
        }
        self.abortable = false;
        self.enter(State::Frozen(attempt), now);
        vec![MigrateAction::Send(SessionMessage::PathFrozen { attempt })]
    }

    fn on_frozen(&mut self, attempt: u32, now: Instant) -> Vec<MigrateAction> {
        if self.role != Role::Controller || self.state != State::Committed(attempt) {
            return Vec::new();
        }
        self.enter(State::WriteWait(attempt), now);
        vec![
            // 读方向立刻切：对端在 PathFrozen 之后不会再写旧流。
            MigrateAction::ActivateRead(attempt),
            MigrateAction::Send(SessionMessage::PathCommit { attempt }),
        ]
    }

    fn on_commit(&mut self, attempt: u32, now: Instant) -> Vec<MigrateAction> {
        if self.role != Role::Controlled || self.state != State::Frozen(attempt) {
            return Vec::new();
        }
        self.enter(State::Done, now);
        vec![
            // 读方向切到新流：控制器在 PathCommit 之后不会再写旧流。
            MigrateAction::ActivateRead(attempt),
            MigrateAction::ReportPath(TransportPath::Punch),
        ]
    }

    fn on_abort(&mut self, attempt: u32, _reason: &str, now: Instant) -> Vec<MigrateAction> {
        if self.state == State::Idle || self.attempt != attempt {
            return Vec::new();
        }
        if !self.abortable {
            // 提交点之后一律忽略——两端可能已经各自切了一个方向。
            return Vec::new();
        }
        self.schedule_retry(now);
        vec![MigrateAction::ClearPending(attempt)]
    }

    fn on_write_activated(&mut self, attempt: u32, now: Instant) -> Vec<MigrateAction> {
        match (self.role, self.state) {
            (Role::Controller, State::WriteWait(active)) if active == attempt => {
                self.enter(State::Done, now);
                vec![MigrateAction::ReportPath(TransportPath::Punch)]
            }
            // 被控端的写任务在 `PathFrozen` flush 后切写；此时仍在等 `PathCommit`。
            _ => Vec::new(),
        }
    }

    fn on_deadline(&mut self, now: Instant) -> Vec<MigrateAction> {
        let attempt = self.attempt;
        if self.abortable {
            self.schedule_retry(now);
            return vec![
                MigrateAction::ClearPending(attempt),
                MigrateAction::Send(SessionMessage::PathAbort {
                    attempt,
                    reason: "迁移状态超时".to_owned(),
                }),
            ];
        }
        vec![MigrateAction::Fatal(format!(
            "迁移在提交点之后超时（attempt={attempt}, state={:?}）",
            self.state
        ))]
    }

    fn schedule_retry(&mut self, now: Instant) {
        self.abortable = false;
        self.enter(State::Idle, now);
        if self.attempts >= self.timing.max_attempts {
            self.next_at = None;
            return;
        }
        // 第 N 次失败用退避表的第 N-1 项；表用尽后沿用最后一项。
        let index = usize::try_from(self.attempts.saturating_sub(1)).unwrap_or(usize::MAX);
        let delay = self
            .timing
            .retry_backoff
            .get(index)
            .copied()
            .or_else(|| self.timing.retry_backoff.last().copied());
        self.next_at = delay.map(|d| now + d);
    }
}

/// 派生一次建立任务：建流 → `install_pending` → 上报事件。
///
/// **顺序不能反**：控制器在收到 `Established` 后立刻发 `PathFreeze`，所以 pending 必须先装好。
pub(crate) async fn run_establish(
    rt: Arc<MigrateRuntime>,
    handle: MigrateHandle,
    attempt: u32,
    tx: mpsc::UnboundedSender<MigrateEvent>,
) {
    let Some(token) = rt.token.get().cloned() else {
        let _ = tx.send(MigrateEvent::EstablishFailed {
            attempt,
            reason: "本端尚未拿到会话 token".to_owned(),
        });
        return;
    };
    let conn_id = conn_id_for_migration(&token, attempt);
    let request = MigrateRequest {
        session_id: rt.session_id.clone(),
        local_rd_id: rt.local_rd_id.clone(),
        peer_rd_id: rt.peer_rd_id.clone(),
        params: rt.params.clone(),
        punch_cookie: conn_id.to_be_bytes(),
        attempt,
        // 直连路径的前导密钥（打洞路径不使用）。同一 token + attempt 两端算出的值必然一致。
        hello_key: migration_hello_key(&token),
        timeout: rt.timing.establish_timeout,
    };
    let established = match tokio::select! {
        // 会话结束：立即放弃建立（顺带释放 `direct_rx` 的持锁）。
        () = rt.cancel.cancelled() => return,
        result = rt.establisher.migrate(request, rt.role) => result,
    } {
        Ok(e) => e,
        Err(err) => {
            let _ = tx.send(MigrateEvent::EstablishFailed {
                attempt,
                reason: err.to_string(),
            });
            return;
        }
    };
    if established.path == TransportPath::Relay {
        let _ = tx.send(MigrateEvent::EstablishFailed {
            attempt,
            reason: "迁移不允许走中继".to_owned(),
        });
        return;
    }
    let path = established.path;
    if let Err(err) = handle.install_pending(attempt, established.stream, path, established.reliable) {
        let _ = tx.send(MigrateEvent::EstablishFailed {
            attempt,
            reason: err.to_string(),
        });
        return;
    }
    let _ = tx.send(MigrateEvent::Established { attempt, path });
}

/// 会话循环执行状态机动作时需要的上下文。
pub(crate) struct ActionContext<'a> {
    /// 会话 ID（统计事件用）。
    pub session_id: &'a str,
    /// 迁移句柄（`None` = 没有包装，动作里出现迁移动作就是 bug）。
    pub handle: Option<&'a MigrateHandle>,
    /// 建立任务的事件发送端。
    pub evt_tx: &'a mpsc::UnboundedSender<MigrateEvent>,
    /// 运行时。
    pub runtime: &'a Arc<MigrateRuntime>,
}

/// 执行动作；返回需要发出去的控制帧（由调用方按自己的队列类型发送）。
///
/// 把「执行」与「入队」拆开：控制器的队列、被控端的队列不是同一个类型，但动作语义相同。
pub(crate) fn execute_actions(
    actions: Vec<MigrateAction>,
    ctx: &ActionContext<'_>,
    mut enqueue: impl FnMut(SessionMessage),
) -> Option<RemoteError> {
    let some_handle = || {
        ctx.handle
            .cloned()
            .ok_or_else(|| RemoteError::Unavailable("迁移运行时缺少包装句柄".to_owned()))
    };
    for action in actions {
        match action {
            MigrateAction::Send(msg) => enqueue(msg),
            MigrateAction::SpawnEstablish(attempt) => {
                let Ok(handle) = some_handle() else {
                    return Some(RemoteError::Unavailable("迁移运行时缺少包装句柄".to_owned()));
                };
                // **必须在 spawn 之前登记**：建立任务跑了第一步就会 `install_pending`，
                // 晚一步就会被判 `NotWanted`。所有调用点因此不必各自记得调 `want`。
                handle.want(attempt);
                tokio::spawn(run_establish(
                    Arc::clone(ctx.runtime),
                    handle,
                    attempt,
                    ctx.evt_tx.clone(),
                ));
            }
            MigrateAction::ActivateRead(attempt) => {
                let Ok(handle) = some_handle() else {
                    return Some(RemoteError::Unavailable("迁移运行时缺少包装句柄".to_owned()));
                };
                if !handle.activate_read(attempt) {
                    return Some(RemoteError::Unavailable(format!(
                        "切换读方向失败（attempt={attempt} 没有待装的流）"
                    )));
                }
            }
            MigrateAction::ClearPending(attempt) => {
                if let Ok(handle) = some_handle() {
                    handle.clear_pending(attempt);
                }
            }
            MigrateAction::ReportPath(path) => {
                ctx.runtime.stats.report(SessionStats::TransportPath {
                    session_id: ctx.session_id.to_owned(),
                    path,
                });
            }
            MigrateAction::Fatal(reason) => {
                return Some(RemoteError::Signaling(reason));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::Duration;

    fn timing() -> MigrateTiming {
        MigrateTiming {
            first_delay: Duration::ZERO,
            retry_backoff: Vec::new(),
            // 既有用例都只看「一次尝试」的语义；显式给 1，避免将来默认值变动把它们变成重试用例。
            max_attempts: 1,
            establish_timeout: Duration::from_secs(1),
            state_timeout: Duration::from_secs(2),
        }
    }

    /// 写方向切换必须**原样保序**：屏障之前的字节留在旧流，之后的字节去新流。
    #[tokio::test]
    async fn write_switch_routes_bytes_to_the_active_stream() {
        let (old_app, mut old_peer) = tokio::io::duplex(1024);
        let (mut wrapper, handle) = MigratableDuplex::new(Box::new(old_app), Role::Controller);

        wrapper.write_all(b"before").await.unwrap();
        let mut buf = [0u8; 6];
        old_peer.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"before");

        let (new_app, mut new_peer) = tokio::io::duplex(1024);
        handle.want(1);
        handle
            .install_pending(1, Box::new(new_app), TransportPath::Punch, None)
            .unwrap();
        assert!(handle.activate_write(1), "已装好的流必须能激活");

        wrapper.write_all(b"after").await.unwrap();
        let mut buf = [0u8; 5];
        new_peer.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"after");
        // 旧流不再收到任何字节。
        let mut extra = [0u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(50), old_peer.read(&mut extra))
                .await
                .is_err(),
            "切换后旧流不应再收到写入"
        );
    }

    /// 读方向切换同样必须保序。
    #[tokio::test]
    async fn read_switch_pulls_bytes_from_the_active_stream() {
        let (old_app, mut old_peer) = tokio::io::duplex(1024);
        let (mut wrapper, handle) = MigratableDuplex::new(Box::new(old_app), Role::Controlled);

        old_peer.write_all(b"left").await.unwrap();
        let mut buf = [0u8; 4];
        wrapper.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"left");

        let (new_app, mut new_peer) = tokio::io::duplex(1024);
        handle.want(1);
        handle
            .install_pending(1, Box::new(new_app), TransportPath::Punch, None)
            .unwrap();
        assert!(handle.activate_read(1));

        new_peer.write_all(b"right").await.unwrap();
        let mut buf = [0u8; 5];
        wrapper.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"right");
    }

    /// 没有登记过 attempt 就 install，必须被拒（防止陈旧的建立任务污染新尝试）。
    #[test]
    fn install_without_want_is_rejected() {
        let (app, _peer) = tokio::io::duplex(16);
        let (_wrapper, handle) = MigratableDuplex::new(Box::new(app), Role::Controller);
        let (new_app, _new_peer) = tokio::io::duplex(16);
        assert_eq!(
            handle.install_pending(1, Box::new(new_app), TransportPath::Punch, None),
            Err(MigrateInstallError::NotWanted)
        );
    }

    /// 没有 pending 时激活返回 false（调用方据此判定为故障，而不是静默继续）。
    #[test]
    fn activate_without_pending_reports_false() {
        let (app, _peer) = tokio::io::duplex(16);
        let (_wrapper, handle) = MigratableDuplex::new(Box::new(app), Role::Controller);
        assert!(!handle.activate_read(1));
        assert!(!handle.activate_write(1));
    }

    fn assert_actions(actions: &[MigrateAction], f: impl Fn(&MigrateAction) -> bool, what: &str) {
        assert!(actions.iter().any(f), "缺少动作：{what}；实得 {actions:?}");
    }

    #[test]
    fn controller_happy_path_commits_then_reports() {
        let mut m = MigrateMachine::new(Role::Controller, timing());
        let now = Instant::now();

        let a = m.on_tick(now);
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathMigratePrepare { attempt: 1 })),
            "Prepare",
        );
        // 建立必须与 Prepare **同时**发起，否则真机上两端互等（见 `start_attempt` 的注释）。
        assert_actions(&a, |x| matches!(x, MigrateAction::SpawnEstablish(1)), "SpawnEstablish");

        // 本端建立完成、但对端还没 Ready：**不得**发 Freeze（对端可能还没装好新流）。
        let a = m.on_event(
            MigrateEvent::Established {
                attempt: 1,
                path: TransportPath::Punch,
            },
            now,
        );
        assert!(a.is_empty(), "对端未就绪前不得提交");

        // 对端 Ready：两个条件齐了，提交。
        let a = m
            .on_message(&SessionMessage::PathMigrateReady { attempt: 1 }, now)
            .unwrap_or_default();
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathFreeze { attempt: 1 })),
            "Freeze",
        );

        let a = m
            .on_message(&SessionMessage::PathFrozen { attempt: 1 }, now)
            .unwrap_or_default();
        assert_actions(&a, |x| matches!(x, MigrateAction::ActivateRead(1)), "ActivateRead");
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathCommit { attempt: 1 })),
            "Commit",
        );

        let a = m.on_event(MigrateEvent::WriteActivated { attempt: 1 }, now);
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::ReportPath(TransportPath::Punch)),
            "ReportPath",
        );
        assert!(m.is_done());
    }

    #[test]
    fn controlled_happy_path_reports_only_after_commit() {
        let now = Instant::now();
        let mut m = MigrateMachine::new(Role::Controlled, timing());

        let a = m
            .on_message(&SessionMessage::PathMigratePrepare { attempt: 1 }, now)
            .unwrap_or_default();
        assert_actions(&a, |x| matches!(x, MigrateAction::SpawnEstablish(1)), "SpawnEstablish");

        let a = m.on_event(
            MigrateEvent::Established {
                attempt: 1,
                path: TransportPath::Punch,
            },
            now,
        );
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathMigrateReady { attempt: 1 })),
            "Ready",
        );

        let a = m
            .on_message(&SessionMessage::PathFreeze { attempt: 1 }, now)
            .unwrap_or_default();
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathFrozen { attempt: 1 })),
            "Frozen",
        );

        // 本地写激活不改变状态，也不上报路径。
        let a = m.on_event(MigrateEvent::WriteActivated { attempt: 1 }, now);
        assert!(a.is_empty());

        let a = m
            .on_message(&SessionMessage::PathCommit { attempt: 1 }, now)
            .unwrap_or_default();
        assert_actions(&a, |x| matches!(x, MigrateAction::ActivateRead(1)), "ActivateRead");
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::ReportPath(TransportPath::Punch)),
            "ReportPath",
        );
        assert!(m.is_done());
    }

    /// 提交点之前失败：清 pending + 发 Abort，且**绝不激活任何方向**。
    #[test]
    fn failure_before_freeze_aborts_without_activation() {
        let mut m = MigrateMachine::new(Role::Controller, timing());
        let now = Instant::now();
        m.on_tick(now);

        let a = m.on_event(
            MigrateEvent::EstablishFailed {
                attempt: 1,
                reason: "打洞失败".to_owned(),
            },
            now,
        );
        assert_actions(&a, |x| matches!(x, MigrateAction::ClearPending(1)), "ClearPending");
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathAbort { .. })),
            "Abort",
        );
        assert!(
            !a.iter().any(|x| matches!(x, MigrateAction::ActivateRead(_))),
            "失败绝不能激活读方向"
        );
        assert!(m.is_idle());
    }

    /// 提交点之后超时：只能按会话故障处理，**绝不回滚**。
    #[test]
    fn deadline_after_commit_is_fatal() {
        let mut m = MigrateMachine::new(Role::Controller, timing());
        let now = Instant::now();
        m.on_tick(now);
        m.on_message(&SessionMessage::PathMigrateReady { attempt: 1 }, now);
        m.on_event(
            MigrateEvent::Established {
                attempt: 1,
                path: TransportPath::Punch,
            },
            now,
        );

        let later = now + Duration::from_secs(3);
        let a = m.on_tick(later);
        assert_actions(&a, |x| matches!(x, MigrateAction::Fatal(_)), "Fatal");
        assert!(
            !a.iter().any(|x| matches!(x, MigrateAction::ClearPending(_))),
            "提交点之后不能回滚"
        );
    }

    /// 提交点之后收到 Abort：忽略（两端可能已经各切了一个方向）。
    #[test]
    fn abort_after_commit_is_ignored() {
        let mut m = MigrateMachine::new(Role::Controller, timing());
        let now = Instant::now();
        m.on_tick(now);
        m.on_message(&SessionMessage::PathMigrateReady { attempt: 1 }, now);
        m.on_event(
            MigrateEvent::Established {
                attempt: 1,
                path: TransportPath::Punch,
            },
            now,
        );
        let a = m.on_message(
            &SessionMessage::PathAbort {
                attempt: 1,
                reason: "对端放弃".to_owned(),
            },
            now,
        )
        .unwrap_or_default();
        assert!(a.is_empty(), "提交点之后的 Abort 必须被忽略");
    }

    /// 退避序列必须逐级取表、用尽后按 `max_attempts` 停下；手动触发**不消耗**自动次数。
    #[test]
    fn retry_backoff_sequence_and_attempt_cap() {
        let mut m = MigrateMachine::new(
            Role::Controller,
            MigrateTiming {
                first_delay: Duration::ZERO,
                retry_backoff: vec![Duration::from_secs(10), Duration::from_secs(20)],
                max_attempts: 3,
                establish_timeout: Duration::from_secs(1),
                state_timeout: Duration::from_secs(2),
            },
        );
        let t0 = Instant::now();

        // 第 1 次（自动）→ 失败 → 退避第 1 项 10s。
        let a = m.on_tick(t0);
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathMigratePrepare { attempt: 1 })),
            "第 1 次 Prepare",
        );
        let t1 = t0 + Duration::from_millis(5);
        m.on_event(
            MigrateEvent::EstablishFailed {
                attempt: 1,
                reason: "第一次失败".to_owned(),
            },
            t1,
        );
        assert_eq!(m.next_attempt_at(), Some(t1 + Duration::from_secs(10)), "第 1 次退避 10s");

        // 第 2 次 → 失败 → 退避第 2 项 20s。
        let t2 = t1 + Duration::from_secs(10);
        let a = m.on_tick(t2);
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathMigratePrepare { attempt: 2 })),
            "第 2 次 Prepare",
        );
        let t3 = t2 + Duration::from_millis(5);
        m.on_event(
            MigrateEvent::EstablishFailed {
                attempt: 2,
                reason: "第二次失败".to_owned(),
            },
            t3,
        );
        assert_eq!(m.next_attempt_at(), Some(t3 + Duration::from_secs(20)), "第 2 次退避 20s");

        // 第 3 次（用掉最后一个自动名额）→ 失败 → 没有任何后续尝试。
        let t4 = t3 + Duration::from_secs(20);
        let a = m.on_tick(t4);
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathMigratePrepare { attempt: 3 })),
            "第 3 次 Prepare",
        );
        let t5 = t4 + Duration::from_millis(5);
        m.on_event(
            MigrateEvent::EstablishFailed {
                attempt: 3,
                reason: "第三次失败".to_owned(),
            },
            t5,
        );
        assert_eq!(m.next_attempt_at(), None, "达到 max_attempts 后不应再排自动尝试");
        assert!(m.is_idle());
        // 再 tick 多远都不会再发 Prepare。
        assert!(m.on_tick(t5 + Duration::from_secs(3600)).is_empty());

        // 手动触发：不受上限约束，attempt 号继续递增。
        let a = m.force_start(t5 + Duration::from_secs(1));
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathMigratePrepare { attempt: 4 })),
            "手动 Prepare（attempt 4）",
        );
        // 手动这次失败后同样不得排自动尝试（它没有消耗名额，但名额本来就已经用尽）。
        let t6 = t5 + Duration::from_secs(2);
        m.on_event(
            MigrateEvent::EstablishFailed {
                attempt: 4,
                reason: "手动失败".to_owned(),
            },
            t6,
        );
        assert_eq!(m.next_attempt_at(), None, "手动尝试失败不应排自动重试");
    }

    /// `Establishing` 的超时必须用 `establish_timeout`（信令 + 握手预算），不是消息往返的 2s。
    ///
    /// 回归用例：两者混用时，真实链路上任何超过 2s 的建立（打洞预算 3s、直连 3s 连接 + 2s
    /// 前导）都会被状态机提前 abort，建立任务事后 `install_pending` 得到 `NotWanted`——迁移
    /// **永远**失败，而且每次失败都要等一整段退避。
    #[test]
    fn establishing_uses_the_establish_budget() {
        let mut m = MigrateMachine::new(
            Role::Controller,
            MigrateTiming {
                first_delay: Duration::ZERO,
                retry_backoff: vec![Duration::from_secs(10)],
                max_attempts: 3,
                establish_timeout: Duration::from_secs(5),
                state_timeout: Duration::from_secs(2),
            },
        );
        let t0 = Instant::now();
        m.on_tick(t0); // → Establishing（Prepare + 建立同时发起）
        let t1 = t0 + Duration::from_millis(100);
        m.on_message(&SessionMessage::PathMigrateReady { attempt: 1 }, t1); // → Establishing

        // 超过 `state_timeout`(2s) 但仍在 `establish_timeout`(5s) 内：必须继续等，不得 abort。
        let mid = t1 + Duration::from_secs(3);
        let a = m.on_tick(mid);
        assert!(a.is_empty(), "建立预算内不得有任何动作（实得 {a:?}）");

        // 超过建立预算：按「提交点之前」的规则安全放弃。
        let late = t1 + Duration::from_secs(5) + Duration::from_millis(1);
        let a = m.on_tick(late);
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathAbort { .. })),
            "建立超时后 Abort",
        );
        assert_actions(&a, |x| matches!(x, MigrateAction::ClearPending(1)), "ClearPending");
        assert!(m.is_idle());
    }

    /// 被控端收到过期的 attempt：回 Abort，且不启动建立任务。
    #[test]
    fn controlled_rejects_stale_attempt() {
        let now = Instant::now();
        let mut m = MigrateMachine::new(Role::Controlled, timing());
        m.on_message(&SessionMessage::PathMigratePrepare { attempt: 5 }, now);
        let a = m
            .on_message(&SessionMessage::PathMigratePrepare { attempt: 5 }, now)
            .unwrap_or_default();
        assert_actions(
            &a,
            |x| matches!(x, MigrateAction::Send(SessionMessage::PathAbort { .. })),
            "Abort",
        );
    }
}
