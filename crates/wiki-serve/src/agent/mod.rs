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
    GetStatus {
        reply: oneshot::Sender<AgentStatus>,
    },
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

    /// 幂等 `start`：resolve → config ensure → spawn → initialize 握手 → Running.
    ///
    /// 若已为 `Running`/`Starting` 则直接返回当前状态。
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
        {
            let st = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if matches!(st, AgentStatus::Running { .. } | AgentStatus::Starting) {
                return Ok(st);
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

        // 1) 加载 settings
        let settings_path = AgentSettings::default_path(&self.app_config_dir);
        let settings =
            AgentSettings::load(&settings_path).map_err(|e| format!("加载 settings 失败：{e}"))?;

        // 2) resolve 二进制
        let override_path = settings
            .codex_path_override
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        let resolved = resolve::resolve_binary_live(override_path.as_deref(), None)
            .ok_or_else(|| "未找到 codex 二进制（sidecar/覆盖/PATH 均未命中）".to_owned())?;

        // 3) CODEX_HOME 渲染
        let codex_home = CodexHome::default_dir(&self.app_config_dir);
        CodexHome::ensure(&codex_home, &settings).map_err(|e| format!("渲染 CODEX_HOME 失败：{e}"))?;

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

        // 阻塞等待握手结果（最多 15s）
        let deadline = std::time::Duration::from_secs(15);
        match start_rx.recv_timeout(deadline) {
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
                *self
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Exited {
                    code: None,
                    stderr_tail: Vec::new(),
                    reason: reason.clone(),
                };
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
                *self
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Exited {
                    code: None,
                    stderr_tail: Vec::new(),
                    reason: msg.clone(),
                };
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
        if let Some(tx) = self.cmd_tx.take() {
            let (reply_tx, reply_rx) = oneshot::channel();
            let cmd = AgentCommand::Stop { reply: reply_tx };
            // 同步阻塞发送（cmd_tx 为 tokio mpsc，需 runtime；此处用 blocking_send 需 handle）
            // 但 AgentManager::stop 可能在同步上下文调用，需用 try_send 或阻塞
            // 简化：若 send 失败则直接标记 Stopped
            if tx.try_send(cmd).is_err() {
                *self
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Stopped;
                return Ok(());
            }
            let _ = reply_rx.blocking_recv();
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
        self.reset_failures();
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
        let tx = self.cmd_tx.as_ref().ok_or_else(|| "agent 未运行".to_owned())?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.try_send(AgentCommand::Request {
            method,
            params,
            reply: reply_tx,
        })
        .map_err(|e| format!("发送 request 失败：{e}"))?;
        reply_rx
            .blocking_recv()
            .map_err(|_| "request 响应通道已关闭".to_owned())?
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
        let tx = self.cmd_tx.as_ref().ok_or_else(|| "agent 未运行".to_owned())?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.try_send(AgentCommand::Respond {
            id,
            result,
            error,
            reply: reply_tx,
        })
        .map_err(|e| format!("发送 respond 失败：{e}"))?;
        reply_rx
            .blocking_recv()
            .map_err(|_| "respond 响应通道已关闭".to_owned())?
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
        settings
            .save(&path)
            .map_err(|e| format!("保存 settings 失败：{e}"))?;
        self.reset_failures();
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
    #[allow(clippy::too_many_arguments, reason = "线程主循环需一次性传入全部上下文")]
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
                    if let jsonrpc::JsonRpcStatus::Exited { code, reason } = status {
                        self.0.on_status(AgentStatus::Exited {
                            code,
                            stderr_tail: Vec::new(),
                            reason,
                        });
                    }
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

            let Some(stdout) = proc.take_stdout() else {
                let msg = "子进程 stdout 不可用".to_owned();
                let _ = start_tx.send(Err(msg.clone()));
                return;
            };
            let Some(stdin) = proc.take_stdin() else {
                let msg = "子进程 stdin 不可用".to_owned();
                let _ = start_tx.send(Err(msg.clone()));
                return;
            };

            let bridge: Arc<dyn jsonrpc::JsonRpcSink> =
                Arc::new(AgentSinkBridge(Arc::clone(&sink)));
            let conn = Arc::new(jsonrpc::JsonRpcConnection::new(stdout, stdin, bridge));

            // 退出监听任务：等待子进程退出后广播 Exited
            let status_for_exit = Arc::clone(&status_arc);
            let sink_for_exit = Arc::clone(&sink);
            let exit_handle = tokio::spawn(async move {
                let code = proc.wait().await.ok().flatten();
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
                    *status_arc
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Exited {
                        code: None,
                        stderr_tail: Vec::new(),
                        reason: msg.clone(),
                    };
                    let _ = start_tx.send(Err(msg));
                    exit_handle.abort();
                    return;
                }
                Err(_) => {
                    let msg = "initialize 超时（10s）".to_owned();
                    *status_arc
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = AgentStatus::Exited {
                        code: None,
                        stderr_tail: Vec::new(),
                        reason: msg.clone(),
                    };
                    let _ = start_tx.send(Err(msg));
                    exit_handle.abort();
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
                                // 尝试优雅关闭：shutdown writer
                                let _ = conn.shutdown_writer().await;
                                exit_handle.abort();
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
                        exit_handle.abort();
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
        assert_eq!(v2.get("bin").and_then(|x| x.as_str()), Some("/usr/bin/codex"));

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
        // save_settings 成功后清零
        let s = AgentSettings::default();
        mgr.save_settings(&s).expect("save");
        assert_eq!(mgr.consecutive_failures(), 0, "save_settings 后应清零");
        mgr.record_failure();
        mgr.record_failure();
        mgr.record_failure();
        assert_eq!(mgr.consecutive_failures(), 3);
        mgr.stop().expect("stop");
        assert_eq!(mgr.consecutive_failures(), 0, "stop 后应清零");
        assert_eq!(mgr.status_snapshot(), AgentStatus::Stopped);
    }
}
