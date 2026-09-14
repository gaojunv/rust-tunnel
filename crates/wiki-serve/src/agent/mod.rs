// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! Agent 管理层：状态机、独立 runtime 线程、事件分发与 initialize 握手.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

pub mod config;
pub mod jsonrpc;
pub mod process;
pub mod resolve;

#[cfg(feature = "tauri")]
pub mod bridge;

pub use config::{AgentSettings, ApprovalPolicy, AuthMode, CodexHome, SandboxMode};
pub use resolve::{ResolveSource, ResolvedBinary};

/// Agent 状态（供前端展示）.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "phase")]
pub enum AgentStatus {
    /// 未运行.
    #[serde(rename = "stopped")]
    Stopped,
    /// 启动中.
    #[serde(rename = "starting")]
    Starting,
    /// 运行中.
    #[serde(rename = "running")]
    Running {
        /// 二进制路径.
        bin: String,
        /// 版本（若已知）.
        version: Option<String>,
    },
    /// 已退出.
    #[serde(rename = "exited")]
    Exited {
        /// 退出码.
        code: Option<i32>,
        /// stderr 尾部 20 行.
        stderr_tail: Vec<String>,
        /// 原因.
        reason: String,
    },
}

/// 事件接收端（由 bridge 或测试注入）.
pub trait AgentEventSink: Send + Sync + 'static {
    /// 收到 notification.
    fn on_notification(&self, method: String, params: Option<Value>);
    /// 收到 server 发起的请求（需审批）.
    fn on_server_request(&self, id: Value, method: String, params: Option<Value>);
    /// 状态变化.
    fn on_status(&self, status: AgentStatus);
    /// 解析错误（不中断连接）.
    fn on_parse_error(&self, line: String, error: String);
}

/// 空 sink（测试兜底）.
#[derive(Debug, Default)]
pub struct NoopAgentSink;

impl AgentEventSink for NoopAgentSink {
    fn on_notification(&self, _method: String, _params: Option<Value>) {}
    fn on_server_request(&self, _id: Value, _method: String, _params: Option<Value>) {}
    fn on_status(&self, _status: AgentStatus) {}
    fn on_parse_error(&self, _line: String, _error: String) {}
}

/// 测试收集型 sink.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct CollectAgentSink {
    /// 通知.
    pub notifications: Mutex<Vec<(String, Option<Value>)>>,
    /// 审批请求.
    pub server_requests: Mutex<Vec<(Value, String, Option<Value>)>>,
    /// 状态序列.
    pub statuses: Mutex<Vec<AgentStatus>>,
    /// 解析错误.
    pub parse_errors: Mutex<Vec<(String, String)>>,
}

#[cfg(test)]
impl AgentEventSink for CollectAgentSink {
    fn on_notification(&self, method: String, params: Option<Value>) {
        self.notifications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((method, params));
    }
    fn on_server_request(&self, id: Value, method: String, params: Option<Value>) {
        self.server_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, method, params));
    }
    fn on_status(&self, status: AgentStatus) {
        self.statuses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(status);
    }
    fn on_parse_error(&self, line: String, error: String) {
        self.parse_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((line, error));
    }
}

/// 内部命令（tauri command 经 mpsc 转发至 agent 线程）.
enum AgentCommand {
    /// 请求.
    Request {
        method: String,
        params: Option<Value>,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// 应答 server request.
    Respond {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// 停止.
    Stop {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// 获取状态（保留供未来 mpsc 同步查询）.
    #[allow(dead_code)]
    GetStatus { reply: oneshot::Sender<AgentStatus> },
}

/// `request_sync`/`respond_sync` 等待 agent 线程回执的超时：超时释放调用方
/// （`Arc<Mutex<AgentManager>>` 的 Tauri 锁），挂起的命令由 agent 线程侧继续
/// 处理或随连接关闭丢弃，调用方仅报超时错误。
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 带超时的 oneshot 同步等待：轮询 `try_recv`，超时/对端关闭均返回错误.
///
/// 内层 `Result<T, String>`（agent 线程的业务错误）直接透传，外层超时/关闭转为错误.
fn recv_with_timeout<T>(
    mut rx: oneshot::Receiver<Result<T, String>>,
    timeout: std::time::Duration,
    op: &str,
) -> Result<T, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(v) => return v,
            Err(oneshot::error::TryRecvError::Closed) => {
                return Err(format!("{op} 响应通道已关闭"));
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "{op} 超时（{}s），agent 线程可能卡死",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

/// `start` 握手等待上限：`begin_start` 拉起线程后，调用方在锁外等这么久.
pub const START_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 进行中的启动：`begin_start` 已置 `Starting` 并拉起线程，调用方在锁外
/// `wait_handshake`，再持锁 `complete_start` 回写。
///
/// `epoch` 用于识别等待期间是否发生过 `stop`：若有则回写直接放弃，
/// 避免把 `Stopped` 覆盖回 `Running`。`sink` 供 `complete_start` 广播回写结果。
pub struct PendingStart {
    start_rx: std::sync::mpsc::Receiver<Result<AgentStatus, String>>,
    /// 事件 sink（`complete_start` 回写时广播用）.
    pub sink: Arc<dyn AgentEventSink>,
    /// 启动代际（`complete_start` 校验用）.
    pub epoch: u64,
}

impl PendingStart {
    /// 在锁外等待握手结果（最多 `START_HANDSHAKE_TIMEOUT`）.
    ///
    /// # Errors
    ///
    /// 超时返回 `RecvTimeoutError`；内层 `Err` 为 agent 线程报告的启动失败。
    pub fn wait_handshake(
        &self,
    ) -> Result<Result<AgentStatus, String>, std::sync::mpsc::RecvTimeoutError> {
        self.start_rx.recv_timeout(START_HANDSHAKE_TIMEOUT)
    }
}

/// AgentManager：状态机 + 独立 OS 线程上的 current_thread runtime.
pub struct AgentManager {
    status: Arc<Mutex<AgentStatus>>,
    app_config_dir: PathBuf,
    vault_root: PathBuf,
    cmd_tx: Option<mpsc::Sender<AgentCommand>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<std::thread::JoinHandle<()>>,
    consecutive_failures: u32,
    /// 启动代际：`begin_start`/`stop` 每次递增，用于 `complete_start` 识别
    /// 握手等待期间是否发生过 `stop`（ epoch 失配则放弃回写）.
    start_epoch: u64,
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field(
                "status",
                &self
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            )
            .field("app_config_dir", &self.app_config_dir)
            .field("vault_root", &self.vault_root)
            .field("cmd_tx", &self.cmd_tx)
            .field("shutdown_tx", &self.shutdown_tx)
            .field(
                "join_handle",
                &self.join_handle.as_ref().map(|_| "<thread>"),
            )
            .finish_non_exhaustive()
    }
}

impl AgentManager {
    /// 构造（未启动），需在 `start` 时传入 `AgentEventSink`.
    #[must_use]
    pub fn new(app_config_dir: PathBuf, vault_root: PathBuf) -> Self {
        Self {
            status: Arc::new(Mutex::new(AgentStatus::Stopped)),
            app_config_dir,
            vault_root,
            cmd_tx: None,
            shutdown_tx: None,
            join_handle: None,
            consecutive_failures: 0,
            start_epoch: 0,
        }
    }

    /// 连续启动失败次数（用于熔断，3 次后需人工介入）.
    #[must_use]
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// 重置连续失败计数（`save_settings` 与显式 `stop` 后调用）.
    fn reset_failures(&mut self) {
        self.consecutive_failures = 0;
    }

    /// 记录一次启动失败，返回新的计数值.
    fn record_failure(&mut self) -> u32 {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.consecutive_failures
    }

    /// 当前状态快照.
    ///
    /// # Panics
    ///
    /// 内部 `Mutex` 中毒时不会 panic，通过 `PoisonError::into_inner` 自动恢复.
    #[must_use]
    pub fn status_snapshot(&self) -> AgentStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 幂等 `start`：预检（持锁）→ 握手等待（放锁）→ 回写（持锁）三阶段.
    ///
    /// 若已为 `Running`/`Starting` 则直接返回当前状态。
    /// 调用链：`begin_start`（持 Tauri 锁，快）→ 放锁 →
    /// `PendingStart::wait_handshake`（最长 15s，不占锁）→ 持锁 `complete_start`。
    ///
    /// # Errors
    ///
    /// 解析失败、spawn 失败或握手失败时返回错误字符串。
    ///
    /// # Panics
    ///
    /// 内部 `Mutex` 中毒时通过 `PoisonError::into_inner` 恢复，不会 panic.
    #[allow(clippy::needless_pass_by_value, reason = "Arc 克隆语义需要拥有所有权")]
    pub fn start(&mut self, sink: Arc<dyn AgentEventSink>) -> Result<AgentStatus, String> {
        let pending = self.begin_start(Arc::clone(&sink))?;
        let Some(pending) = pending else {
            // 幂等：已 Running/Starting，直接返回当前状态
            return Ok(self.status_snapshot());
        };
        let epoch = pending.epoch;
        let outcome = pending.wait_handshake();
        self.complete_start(outcome, epoch, &sink)
    }

    /// 预检并拉起 agent 线程：置 `Starting`、加载 settings、校验、resolve、
    /// 渲染 `CODEX_HOME`、spawn 线程。持锁，但只做快操作（无 15s 等待）。
    ///
    /// 已 `Running`/`Starting` 时返回 `Ok(None)`（幂等，调用方直接读当前状态）。
    ///
    /// # Errors
    ///
    /// 熔断/预检失败时返回错误字符串（预检失败已落 `Exited` + 记熔断）。
    ///
    /// # Panics
    ///
    /// 内部 `Mutex` 中毒时通过 `PoisonError::into_inner` 恢复，不会 panic.
    #[allow(clippy::needless_pass_by_value, reason = "Arc 克隆语义需要拥有所有权")]
    pub fn begin_start(
        &mut self,
        sink: Arc<dyn AgentEventSink>,
    ) -> Result<Option<PendingStart>, String> {
        {
            let st = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if matches!(st, AgentStatus::Running { .. } | AgentStatus::Starting) {
                return Ok(None);
            }
        }
        if self.consecutive_failures >= 3 {
            return Err(format!(
                "连续启动失败 {} 次，已熔断需人工介入：请检查 Agent 设置（CODEX_HOME/认证/二进制路径）后保存设置或显式 stop 后重试",
                self.consecutive_failures
            ));
        }
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Starting;
        sink.on_status(AgentStatus::Starting);

        // 统一早退：设置 Exited + 广播 + 熔断（不借用 self 的闭包形态避免 E0501）
        let app_config_dir_for_early = self.app_config_dir.clone();
        let sink_for_early = Arc::clone(&sink);
        let status_for_early = Arc::clone(&self.status);
        // 抽为局部函数，通过可变引用更新 failure 计数
        let do_fail_early =
            |mgr: &mut Self, reason: String| -> Result<Option<PendingStart>, String> {
                let count = mgr.record_failure();
                let reason_out = if count >= 3 {
                    format!("{reason}（连续失败 {count} 次，已熔断需人工介入）")
                } else {
                    reason.clone()
                };
                let st = AgentStatus::Exited {
                    code: None,
                    stderr_tail: Vec::new(),
                    reason: reason_out.clone(),
                };
                *status_for_early
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = st.clone();
                sink_for_early.on_status(st);
                Err(reason_out)
            };

        // 1) 加载 settings
        let settings_path = AgentSettings::default_path(&app_config_dir_for_early);
        let settings = match AgentSettings::load(&settings_path) {
            Ok(s) => s,
            Err(e) => return do_fail_early(self, format!("加载 settings 失败：{e}")),
        };

        // 1.5) 启动前校验（网关地址 / 所选 auth 对应 key）
        if let Err(reason) = settings.validate() {
            return do_fail_early(self, reason);
        }

        // 2) resolve 二进制
        let override_path = settings
            .codex_path_override
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        let Some(resolved) = resolve::resolve_binary_live(override_path.as_deref(), None) else {
            return do_fail_early(
                self,
                "未找到 codex 二进制（sidecar/覆盖/PATH 均未命中）".to_owned(),
            );
        };

        // 3) CODEX_HOME 渲染
        let codex_home = CodexHome::default_dir(&app_config_dir_for_early);
        if let Err(e) = CodexHome::ensure(&codex_home, &settings) {
            return do_fail_early(self, format!("渲染 CODEX_HOME 失败：{e}"));
        }

        // 4) 启动独立 runtime 线程
        let app_config_dir = self.app_config_dir.clone();
        let vault_root = self.vault_root.clone();
        let status_arc = Arc::clone(&self.status);
        let (cmd_tx, cmd_rx) = mpsc::channel::<AgentCommand>(64);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let sink_clone = Arc::clone(&sink);

        // 同步等待启动结果
        let (start_tx, start_rx) = std::sync::mpsc::channel::<Result<AgentStatus, String>>();

        let thread = std::thread::spawn(move || {
            Self::agent_thread_main(
                resolved,
                settings,
                codex_home,
                vault_root,
                app_config_dir,
                status_arc,
                sink_clone,
                cmd_rx,
                shutdown_rx,
                start_tx,
            );
        });

        self.cmd_tx = Some(cmd_tx);
        self.shutdown_tx = Some(shutdown_tx);
        self.join_handle = Some(thread);
        self.start_epoch = self.start_epoch.wrapping_add(1);

        // 返回 PendingStart：调用方放锁等待握手，再持锁 complete_start 回写。
        // 15s 等待不再占用 Tauri `Arc<Mutex<AgentManager>>` 全局锁，
        // `stop`/`request` 在等待期间仍可进入（epoch 失配保护见 complete_start）。
        Ok(Some(PendingStart {
            start_rx,
            sink,
            epoch: self.start_epoch,
        }))
    }

    /// 回写握手结果：成功清零熔断（"成功启动时清零"），失败记熔断并落 `Exited`.
    ///
    /// 若等待期间发生过 `stop`（`epoch` 失配），放弃回写并返回当前状态，
    /// 避免把 `Stopped` 覆盖回 `Running`/`Exited`。
    ///
    /// # Errors
    ///
    /// 握手失败（线程侧报告或超时）时返回错误字符串（已落 `Exited` + 记熔断）。
    ///
    /// # Panics
    ///
    /// 内部 `Mutex` 中毒时通过 `PoisonError::into_inner` 恢复，不会 panic.
    pub fn complete_start(
        &mut self,
        outcome: Result<Result<AgentStatus, String>, std::sync::mpsc::RecvTimeoutError>,
        epoch: u64,
        sink: &Arc<dyn AgentEventSink>,
    ) -> Result<AgentStatus, String> {
        if epoch != self.start_epoch {
            // 等待期间发生过 stop/begin_start：放弃回写
            return Ok(self.status_snapshot());
        }
        match outcome {
            Ok(Ok(st)) => {
                // 启动成功：清零连续失败计数
                self.consecutive_failures = 0;
                Ok(st)
            }
            Ok(Err(e)) => {
                let count = self.record_failure();
                let reason = if count >= 3 {
                    format!("{e}（连续失败 {count} 次，已熔断需人工介入）")
                } else {
                    e.clone()
                };
                // 保留线程侧已写入的 stderr_tail（如 initialize 失败携带的尾部），避免空 tail 覆盖
                let existing_tail = self.status_snapshot();
                let tail = if let AgentStatus::Exited { stderr_tail, .. } = existing_tail {
                    stderr_tail
                } else {
                    Vec::new()
                };
                let st = AgentStatus::Exited {
                    code: None,
                    stderr_tail: tail,
                    reason: reason.clone(),
                };
                *self
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = st.clone();
                sink.on_status(st);
                Err(reason)
            }
            Err(_) => {
                let count = self.record_failure();
                let base = "initialize 握手超时（15s）".to_owned();
                let msg = if count >= 3 {
                    format!("{base}（连续失败 {count} 次，已熔断需人工介入）")
                } else {
                    base.clone()
                };
                let st = AgentStatus::Exited {
                    code: None,
                    stderr_tail: Vec::new(),
                    reason: msg.clone(),
                };
                *self
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = st.clone();
                sink.on_status(st);
                Err(msg)
            }
        }
    }

    /// 停止 agent.
    ///
    /// # Errors
    ///
    /// 通信失败时返回错误字符串。
    ///
    /// # Panics
    ///
    /// 内部 `Mutex` 中毒时通过 `PoisonError::into_inner` 恢复，不会 panic.
    pub fn stop(&mut self) -> Result<(), String> {
        // 递增代际：握手等待中的 complete_start 见 epoch 失配即放弃回写，
        // 避免迟到的 Running/Exited 覆盖 Stopped。
        self.start_epoch = self.start_epoch.wrapping_add(1);
        if let Some(tx) = self.cmd_tx.take() {
            let (reply_tx, reply_rx) = oneshot::channel();
            let cmd = AgentCommand::Stop { reply: reply_tx };
            // 同步阻塞发送（cmd_tx 为 tokio mpsc，需 runtime；此处用 blocking_send 需 handle）
            // 但 AgentManager::stop 可能在同步上下文调用，需用 try_send 或阻塞
            if let Err(err) = tx.try_send(cmd) {
                // 通道满或已关闭：记 warn 并回退为停止
                eprintln!("agent stop: 命令通道发送失败（{err}），回退为立即停止");
                *self
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Stopped;
                return Ok(());
            }
            // 带超时等待：agent 线程卡死时 stop 不无限期冻结调用方的 Tauri 锁
            if recv_with_timeout(reply_rx, REQUEST_TIMEOUT, "stop").is_err() {
                eprintln!("agent stop: 等待 agent 线程回执超时，继续强制回收线程");
            }
        }
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Stopped;
        // 普通 stop 不清零熔断：熔断记录的是"启动失败史"，stop 成功不代表
        // 下次启动会成功；仅成功 start 与显式变更配置可清零。
        Ok(())
    }

    /// 透传 request（经 mpsc 转发至 agent 线程）.
    ///
    /// # Errors
    ///
    /// 未运行或通信失败时返回错误字符串。
    ///
    /// # Panics
    ///
    /// 内部通道关闭时不会 panic，仅返回错误.
    pub fn request_sync(&self, method: String, params: Option<Value>) -> Result<Value, String> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| "agent 未运行".to_owned())?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.try_send(AgentCommand::Request {
            method,
            params,
            reply: reply_tx,
        })
        .map_err(|e| format!("发送 request 失败：{e}"))?;
        recv_with_timeout(reply_rx, REQUEST_TIMEOUT, "request")
    }

    /// 透传 respond.
    ///
    /// # Errors
    ///
    /// 未运行或通信失败时返回错误字符串。
    ///
    /// # Panics
    ///
    /// 内部通道关闭时不会 panic，仅返回错误.
    pub fn respond_sync(
        &self,
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    ) -> Result<(), String> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| "agent 未运行".to_owned())?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.try_send(AgentCommand::Respond {
            id,
            result,
            error,
            reply: reply_tx,
        })
        .map_err(|e| format!("发送 respond 失败：{e}"))?;
        recv_with_timeout(reply_rx, REQUEST_TIMEOUT, "respond")
    }

    /// 获取当前状态（同步）.
    #[must_use]
    pub fn get_status(&self) -> AgentStatus {
        self.status_snapshot()
    }

    /// 加载 settings.
    ///
    /// # Errors
    ///
    /// IO 失败时返回错误字符串。
    pub fn get_settings(&self) -> Result<AgentSettings, String> {
        let path = AgentSettings::default_path(&self.app_config_dir);
        AgentSettings::load(&path).map_err(|e| format!("加载 settings 失败：{e}"))
    }

    /// 保存 settings.
    ///
    /// # Errors
    ///
    /// 序列化或写入失败时返回错误字符串。
    pub fn save_settings(&mut self, settings: &AgentSettings) -> Result<(), String> {
        let path = AgentSettings::default_path(&self.app_config_dir);
        // 仅"用户显式变更 agent 路径/配置"时清零熔断：对比落盘旧值，
        // 熔断相关字段任一变化才重置；自动保存（内容无变化）不清零，避免绕过熔断。
        let old = AgentSettings::load(&path).ok();
        settings
            .save(&path)
            .map_err(|e| format!("保存 settings 失败：{e}"))?;
        let changed = old
            .as_ref()
            .is_none_or(|o| o.fuse_relevant_changed(settings));
        if changed {
            self.reset_failures();
        }
        Ok(())
    }

    /// 探测二进制（不启动）.
    #[must_use]
    pub fn detect_binary(&self) -> Option<ResolvedBinary> {
        let settings = self.get_settings().ok()?;
        let override_path = settings
            .codex_path_override
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        resolve::resolve_binary_live(override_path.as_deref(), None)
    }

    // -------------------------------------------------------------------------
    // Agent 线程主循环（独立 current_thread runtime）
    // -------------------------------------------------------------------------

    #[allow(clippy::too_many_lines, reason = "agent 线程主循环包含握手与事件分发")]
    #[allow(
        clippy::too_many_arguments,
        reason = "线程主循环需一次性传入全部上下文"
    )]
    fn agent_thread_main(
        resolved: ResolvedBinary,
        settings: AgentSettings,
        codex_home: PathBuf,
        vault_root: PathBuf,
        _app_config_dir: PathBuf,
        status_arc: Arc<Mutex<AgentStatus>>,
        sink: Arc<dyn AgentEventSink>,
        mut cmd_rx: mpsc::Receiver<AgentCommand>,
        mut shutdown_rx: oneshot::Receiver<()>,
        start_tx: std::sync::mpsc::Sender<Result<AgentStatus, String>>,
    ) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let msg = format!("创建 agent runtime 失败：{e}");
                eprintln!("{msg}");
                let _ = start_tx.send(Err(msg.clone()));
                *status_arc
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Exited {
                    code: None,
                    stderr_tail: Vec::new(),
                    reason: msg,
                };
                return;
            }
        };

        rt.block_on(async move {
            // 桥接 JsonRpcConnection 的 sink 到 AgentEventSink
            #[allow(clippy::items_after_statements, reason = "桥接类型就近定义更清晰")]
            struct AgentSinkBridge(Arc<dyn AgentEventSink>);
            #[allow(clippy::items_after_statements, reason = "桥接实现就近定义")]
            impl jsonrpc::JsonRpcSink for AgentSinkBridge {
                fn on_notification(&self, method: String, params: Option<Value>) {
                    self.0.on_notification(method, params);
                }
                fn on_server_request(&self, id: Value, method: String, params: Option<Value>) {
                    self.0.on_server_request(id, method, params);
                }
                fn on_status(&self, status: jsonrpc::JsonRpcStatus) {
                    // 退出状态仅由 exit-watcher 以含 stderr_tail 的 Exited 统一发出，
                    // 忽略 JSON-RPC 读循环的空 tail Exited 以避免竞态覆盖。
                    let _ = status;
                }
                fn on_parse_error(&self, line: String, error: String) {
                    self.0.on_parse_error(line, error);
                }
            }

            // spawn 子进程
            let child_env = CodexHome::child_env(&settings);
            let spawn_cfg = process::SpawnConfig {
                bin: resolved.path.clone(),
                argv_prefix: resolved.argv_prefix.clone(),
                env: child_env,
                codex_home,
                cwd: vault_root,
            };
            let mut proc = match process::spawn(&spawn_cfg).await {
                Ok(p) => p,
                Err(e) => {
                    let msg = format!("spawn 失败：{e}");
                    let st = AgentStatus::Exited {
                        code: None,
                        // spawn 前无子进程可采 tail，仅以 reason 传递原因
                        stderr_tail: Vec::new(),
                        reason: msg.clone(),
                    };
                    *status_arc
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = st.clone();
                    sink.on_status(st);
                    let _ = start_tx.send(Err(msg));
                    return;
                }
            };

            // 缓存 tail 句柄，供 stdout/stdin 缺失及 initialize 失败路径携带 stderr 尾部
            let stderr_tail_handle = proc.stderr_tail_handle();
            let snapshot_tail = || {
                stderr_tail_handle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .cloned()
                    .collect::<Vec<String>>()
            };

            let Some(stdout) = proc.take_stdout() else {
                let msg = "子进程 stdout 不可用".to_owned();
                // 显式 kill + wait 收割（旧代码直接 return，kill_on_drop 只 kill 不 wait）
                let _ = proc.kill_and_reap().await;
                let st = AgentStatus::Exited {
                    code: None,
                    stderr_tail: snapshot_tail(),
                    reason: msg.clone(),
                };
                *status_arc
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = st.clone();
                sink.on_status(st);
                let _ = start_tx.send(Err(msg));
                return;
            };
            let Some(stdin) = proc.take_stdin() else {
                let msg = "子进程 stdin 不可用".to_owned();
                let _ = proc.kill_and_reap().await;
                let st = AgentStatus::Exited {
                    code: None,
                    stderr_tail: snapshot_tail(),
                    reason: msg.clone(),
                };
                *status_arc
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = st.clone();
                sink.on_status(st);
                let _ = start_tx.send(Err(msg));
                return;
            };

            let bridge: Arc<dyn jsonrpc::JsonRpcSink> =
                Arc::new(AgentSinkBridge(Arc::clone(&sink)));
            let conn = Arc::new(jsonrpc::JsonRpcConnection::new(stdout, stdin, bridge));

            // 退出监听任务：等待子进程退出后广播 Exited；收到停止信号则
            // kill + wait 收割后静默退出（广播由 teardown 方负责，避免覆盖
            // `Stopped`）。teardown 点发信号后 `await` 该 handle，保证收割
            // 完成才返回——旧代码 `abort()` 后直接 drop，留 killed-but-unreaped 僵尸。
            let status_for_exit = Arc::clone(&status_arc);
            let sink_for_exit = Arc::clone(&sink);
            let (exit_stop_tx, exit_stop_rx) = oneshot::channel::<()>();
            let mut exit_stop_tx = Some(exit_stop_tx);
            let mut exit_handle = tokio::spawn(async move {
                let mut proc = proc;
                let code = tokio::select! {
                    r = proc.wait() => r.ok().flatten(),
                    _ = exit_stop_rx => {
                        // teardown 收割：kill + wait，不广播直接退出
                        let _ = proc.kill_and_reap().await;
                        return;
                    }
                };
                let tail = proc.stderr_tail();
                let reason = if let Some(c) = code {
                    format!("codex 进程已退出（code={c}）")
                } else {
                    "codex 进程已退出".to_owned()
                };
                let st = AgentStatus::Exited {
                    code,
                    stderr_tail: tail.clone(),
                    reason: reason.clone(),
                };
                *status_for_exit
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = st.clone();
                sink_for_exit.on_status(st);
            });

            // initialize 握手（需满足 ClientInfo={name,title,version}，缺 title 会被 codex 拒）
            let init_params = serde_json::json!({
                "clientInfo": {
                    "name": "wiki-desktop",
                    "title": "Wiki Desktop",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true,
                    "requestAttestation": false
                }
            });
            let init_result = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                conn.request("initialize", Some(init_params)),
            )
            .await;

            match init_result {
                Ok(Ok(_)) => {
                    // 发送 initialized 通知（失败不阻塞）
                    let _ = conn.notify("initialized", None).await;
                    let running = AgentStatus::Running {
                        bin: resolved.path.display().to_string(),
                        version: resolved.version.clone(),
                    };
                    *status_arc
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = running.clone();
                    sink.on_status(running.clone());
                    let _ = start_tx.send(Ok(running));
                }
                Ok(Err(e)) => {
                    let msg = format!("initialize 失败：{e}");
                    let tail = snapshot_tail();
                    let st = AgentStatus::Exited {
                        code: None,
                        stderr_tail: tail,
                        reason: msg.clone(),
                    };
                    *status_arc
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = st.clone();
                    sink.on_status(st);
                    let _ = start_tx.send(Err(msg));
                    // 发停止信号并等待收割完成（watcher 内 kill + wait），不留僵尸
                    let _ = exit_stop_tx.take().map(|t| t.send(()));
                    let _ = (&mut exit_handle).await;
                    return;
                }
                Err(_) => {
                    let msg = "initialize 超时（10s）".to_owned();
                    let tail = snapshot_tail();
                    let st = AgentStatus::Exited {
                        code: None,
                        stderr_tail: tail,
                        reason: msg.clone(),
                    };
                    *status_arc
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = st.clone();
                    sink.on_status(st);
                    let _ = start_tx.send(Err(msg));
                    let _ = exit_stop_tx.take().map(|t| t.send(()));
                    let _ = (&mut exit_handle).await;
                    return;
                }
            }

            // 命令分发循环
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        let Some(cmd) = cmd else {
                            break;
                        };
                        match cmd {
                            AgentCommand::Request { method, params, reply } => {
                                let res = conn.request(&method, params).await.map_err(|e| e.0);
                                let _ = reply.send(res);
                            }
                            AgentCommand::Respond { id, result, error, reply } => {
                                let res = conn.respond(id, result, error).await.map_err(|e| e.0);
                                let _ = reply.send(res);
                            }
                            AgentCommand::Stop { reply } => {
                                // 尝试优雅关闭：shutdown writer，再发信号等收割完成
                                let _ = conn.shutdown_writer().await;
                                let _ = exit_stop_tx.take().map(|t| t.send(()));
                                let _ = (&mut exit_handle).await;
                                *status_arc
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    AgentStatus::Stopped;
                                sink.on_status(AgentStatus::Stopped);
                                let _ = reply.send(Ok(()));
                                break;
                            }
                            AgentCommand::GetStatus { reply } => {
                                let st = status_arc
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .clone();
                                let _ = reply.send(st);
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        let _ = conn.shutdown_writer().await;
                        let _ = exit_stop_tx.take().map(|t| t.send(()));
                        let _ = (&mut exit_handle).await;
                        break;
                    }
                }
            }
        });
    }
}

impl Drop for AgentManager {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serde() {
        let s = AgentStatus::Stopped;
        let v = serde_json::to_value(&s).expect("ser stopped");
        assert_eq!(v.get("phase").and_then(|x| x.as_str()), Some("stopped"));

        let r = AgentStatus::Running {
            bin: "/usr/bin/codex".to_owned(),
            version: Some("0.1.0".to_owned()),
        };
        let v2 = serde_json::to_value(&r).expect("ser running");
        assert_eq!(v2.get("phase").and_then(|x| x.as_str()), Some("running"));
        assert_eq!(
            v2.get("bin").and_then(|x| x.as_str()),
            Some("/usr/bin/codex")
        );

        let e = AgentStatus::Exited {
            code: Some(1),
            stderr_tail: vec!["err".to_owned()],
            reason: "oops".to_owned(),
        };
        let v3 = serde_json::to_value(&e).expect("ser exited");
        assert_eq!(v3.get("phase").and_then(|x| x.as_str()), Some("exited"));
    }

    #[test]
    fn manager_initial_status_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mgr = AgentManager::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        assert_eq!(mgr.status_snapshot(), AgentStatus::Stopped);
    }

    #[test]
    fn detect_binary_none_when_no_binary() {
        // 注入空 PATH 目录，不触碰进程级 env
        let dir = tempfile::tempdir().expect("tempdir");
        let empty_path: Vec<std::path::PathBuf> = vec![dir.path().to_path_buf()];
        let res =
            crate::agent::resolve::resolve_binary_with_path_dirs(None, None, &empty_path, None);
        assert!(res.is_none(), "空 PATH 注入时应为 None");
    }

    #[test]
    fn settings_persist_via_manager() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mgr = AgentManager::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        let s = AgentSettings {
            enabled: true,
            gateway_base_url: Some("https://example.com".to_owned()),
            ..AgentSettings::default()
        };
        mgr.save_settings(&s).expect("save");
        let loaded = mgr.get_settings().expect("load");
        assert!(loaded.enabled);
        assert_eq!(
            loaded.gateway_base_url.as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn consecutive_failures_fuse_after_three() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mgr = AgentManager::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        assert_eq!(mgr.consecutive_failures(), 0);
        // 模拟 3 次启动失败（resolve 缺失属于 start 前失败，需经 record_failure）
        // 不实际 spawn，直接调内部计数器
        assert_eq!(mgr.record_failure(), 1);
        assert_eq!(mgr.record_failure(), 2);
        assert_eq!(mgr.record_failure(), 3);
        // 已达熔断阈值，start 应直接拒绝（不触及二进制探测）
        let sink: std::sync::Arc<dyn AgentEventSink> = std::sync::Arc::new(NoopAgentSink);
        let err = mgr.start(sink).expect_err("熔断后 start 应失败");
        assert!(
            err.contains("熔断") || err.contains("连续"),
            "错误应提示熔断，实际：{err}"
        );
    }

    #[test]
    fn consecutive_failures_reset_on_save_and_stop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mgr = AgentManager::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        mgr.record_failure();
        mgr.record_failure();
        assert_eq!(mgr.consecutive_failures(), 2);
        // 显式变更熔断相关字段（如 gateway key）后清零
        let changed = AgentSettings {
            gateway_base_url: Some("https://gw.example.com".to_owned()),
            gateway_api_key: Some("tok".to_owned()),
            ..AgentSettings::default()
        };
        mgr.save_settings(&changed).expect("save");
        assert_eq!(mgr.consecutive_failures(), 0, "显式变更配置后应清零");
        // 内容无变化的自动保存不清零
        mgr.record_failure();
        mgr.record_failure();
        assert_eq!(mgr.consecutive_failures(), 2);
        mgr.save_settings(&changed).expect("save same");
        assert_eq!(mgr.consecutive_failures(), 2, "无变化保存不应清零熔断");
        // 非熔断字段变化也不清零
        let mut non_fuse = changed.clone();
        non_fuse.model = Some("gpt-4o".to_owned());
        mgr.save_settings(&non_fuse).expect("save non-fuse");
        assert_eq!(mgr.consecutive_failures(), 2, "非熔断字段变化不应清零");
        // 普通 stop 不清零（熔断记录启动失败史，stop 成功不代表下次能启动）
        mgr.record_failure();
        assert_eq!(mgr.consecutive_failures(), 3);
        mgr.stop().expect("stop");
        assert_eq!(mgr.consecutive_failures(), 3, "stop 后不应清零");
        assert_eq!(mgr.status_snapshot(), AgentStatus::Stopped);
    }

    #[test]
    fn begin_start_idempotent_returns_none() {
        // 已 Running 时 begin_start 返回 Ok(None)，调用方直接读快照
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mgr = AgentManager::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        *mgr.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Running {
            bin: "/usr/bin/codex".to_owned(),
            version: None,
        };
        let sink: std::sync::Arc<dyn AgentEventSink> = std::sync::Arc::new(NoopAgentSink);
        let pending = mgr.begin_start(sink).expect("begin_start 不应错");
        assert!(pending.is_none(), "已运行时应返回 None");
    }

    #[test]
    fn complete_start_epoch_mismatch_abandons_writeback() {
        // 握手等待期间发生 stop：complete_start 见 epoch 失配应放弃回写
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mgr = AgentManager::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        let sink: std::sync::Arc<dyn AgentEventSink> = std::sync::Arc::new(NoopAgentSink);
        // 模拟：旧 epoch 的迟到 Running 回写
        let stale: Result<Result<AgentStatus, String>, std::sync::mpsc::RecvTimeoutError> =
            Ok(Ok(AgentStatus::Running {
                bin: "/usr/bin/codex".to_owned(),
                version: None,
            }));
        mgr.stop().expect("stop 递增 epoch");
        let before = mgr.status_snapshot();
        let out = mgr.complete_start(stale, 0, &sink).expect("放弃回写不应错");
        assert_eq!(out, before, "epoch 失配应返回当前状态而非迟到结果");
        assert_eq!(mgr.status_snapshot(), AgentStatus::Stopped);
    }

    #[test]
    fn request_sync_times_out_without_agent_thread() {
        // cmd_tx 存在但 agent 线程不回执：30s 太长，测 recv_with_timeout 本体（100ms）
        let (tx, rx) = oneshot::channel::<Result<Value, String>>();
        // 有意不 send：对端存活但无响应，应超时而非无限期阻塞
        let err = super::recv_with_timeout(rx, std::time::Duration::from_millis(100), "request")
            .expect_err("应超时");
        assert!(err.contains("超时"), "实际：{err}");
        drop(tx);
        // 对端关闭：立即报关闭而非超时
        let (tx2, rx2) = oneshot::channel::<Result<Value, String>>();
        drop(tx2);
        let err2 = super::recv_with_timeout(rx2, std::time::Duration::from_secs(5), "request")
            .expect_err("应报关闭");
        assert!(err2.contains("关闭"), "实际：{err2}");
    }

    #[test]
    fn start_early_failure_sets_exited_and_broadcasts_and_fuses() {
        // 预启动失败：网关模式缺 base_url，在 Starting 后应落 Exited+广播+熔断计数
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mgr = AgentManager::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        // 先写入缺 base_url 的网关设置（未过 validate）
        let bad = AgentSettings {
            enabled: true,
            auth_mode: crate::agent::config::AuthMode::Gateway,
            gateway_base_url: None,
            gateway_api_key: Some("k".to_owned()),
            ..AgentSettings::default()
        };
        mgr.save_settings(&bad).expect("save bad");
        // 重置计数到 0 后用 CollectAgentSink 捕获广播
        let sink = std::sync::Arc::new(CollectAgentSink::default());
        let sink_dyn: std::sync::Arc<dyn AgentEventSink> = sink.clone();
        let err = mgr.start(sink_dyn).expect_err("应因网关地址缺失失败");
        assert!(
            err.contains("网关模式需要在设置中填写网关地址"),
            "实际：{err}"
        );
        assert_eq!(mgr.consecutive_failures(), 1);
        let statuses = sink.statuses.lock().expect("lock").clone();
        // 应依次 Starting -> Exited
        assert!(
            statuses.contains(&AgentStatus::Starting),
            "应广播 Starting，实际：{statuses:?}"
        );
        assert!(
            statuses.iter().any(
                |s| matches!(s, AgentStatus::Exited { reason, .. } if reason.contains("网关模式"))
            ),
            "应广播 Exited，实际：{statuses:?}"
        );
        assert!(
            matches!(mgr.status_snapshot(), AgentStatus::Exited { .. }),
            "快照应为 Exited"
        );

        // 连续失败至熔断
        for _ in 0..2 {
            let sink2: std::sync::Arc<dyn AgentEventSink> = std::sync::Arc::new(NoopAgentSink);
            let _ = mgr.start(sink2);
        }
        assert_eq!(mgr.consecutive_failures(), 3);
        let sink3: std::sync::Arc<dyn AgentEventSink> = std::sync::Arc::new(NoopAgentSink);
        let e2 = mgr.start(sink3).expect_err("熔断后应拒");
        assert!(e2.contains("熔断"), "实际：{e2}");
    }

    #[test]
    fn start_missing_key_is_early_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mgr = AgentManager::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        // 网关模式缺 key
        let bad = AgentSettings {
            enabled: true,
            auth_mode: crate::agent::config::AuthMode::Gateway,
            gateway_base_url: Some("https://gw.example.com".to_owned()),
            gateway_api_key: None,
            ..AgentSettings::default()
        };
        mgr.save_settings(&bad).expect("save");
        let sink: std::sync::Arc<dyn AgentEventSink> = std::sync::Arc::new(NoopAgentSink);
        let err = mgr.start(sink).expect_err("缺 key 应失败");
        assert!(err.contains("网关模式未配置 API Key"), "实际：{err}");

        // OpenAI 模式缺 key
        let bad2 = AgentSettings {
            enabled: true,
            auth_mode: crate::agent::config::AuthMode::OpenaiKey,
            openai_api_key: None,
            ..AgentSettings::default()
        };
        let mut mgr2 = AgentManager::new(dir.path().join("mgr2"), dir.path().join("vault2"));
        std::fs::create_dir_all(dir.path().join("mgr2")).expect("mkdir");
        mgr2.save_settings(&bad2).expect("save2");
        let sink2: std::sync::Arc<dyn AgentEventSink> = std::sync::Arc::new(NoopAgentSink);
        let err2 = mgr2.start(sink2).expect_err("缺 openai key 应失败");
        assert!(
            err2.contains("OpenAI Key 模式未配置 API Key"),
            "实际：{err2}"
        );

        // ChatGPT 无需 key（但会因缺二进制而报另一错，说明校验通过）
        let chat = AgentSettings {
            enabled: true,
            auth_mode: crate::agent::config::AuthMode::ChatGpt,
            ..AgentSettings::default()
        };
        let mut mgr3 = AgentManager::new(dir.path().join("mgr3"), dir.path().join("vault3"));
        std::fs::create_dir_all(dir.path().join("mgr3")).expect("mkdir3");
        mgr3.save_settings(&chat).expect("save chat");
        let sink3: std::sync::Arc<dyn AgentEventSink> = std::sync::Arc::new(NoopAgentSink);
        let err3 = mgr3
            .start(sink3)
            .expect_err("chatgpt 缺二进制应走 resolve 失败");
        // 校验已过，错误应为二进制缺失而非 key 缺失
        assert!(
            err3.contains("未找到 codex 二进制") || err3.contains("二进制"),
            "实际：{err3}"
        );
    }
}
