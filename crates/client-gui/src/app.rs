use std::sync::{atomic::AtomicBool, Arc, Mutex};

use tokio::sync::watch;

use rust_tunnel_client::{ClientConfig, ClientStatus, LogBuffer};
use rust_tunnel_remote::RemoteManager;

use crate::service_watch::IdentityOwner;

pub struct AppState {
    pub status_rx: watch::Receiver<ClientStatus>,
    pub status_tx: Arc<watch::Sender<ClientStatus>>,
    pub log_buffer: Arc<LogBuffer>,
    pub config: Arc<Mutex<Option<ClientConfig>>>,
    pub reconnect_requested: Arc<AtomicBool>,
    pub should_exit: Arc<AtomicBool>,
    /// 客户端身份的当前归属：Windows 服务在跑时本进程**不注册**（服务端按客户端名唯一，
    /// 同名会互相踢）。由 [`crate::service_watch`] 的轮询任务持续刷新；初值
    /// [`IdentityOwner::Gui`]，对从不安装服务的用户就是今天的行为。
    ///
    /// UI 只读这个接收端（`watch` 是同步的，不需要锁，也不会阻塞 UI 线程）。
    pub service_owner_rx: watch::Receiver<IdentityOwner>,
    /// 归属的发布端：交给服务状态轮询任务写。非 Windows 上没人写，恒为初值。
    pub service_owner_tx: Arc<watch::Sender<IdentityOwner>>,
    /// 远程控制引擎：由后台运行时线程在配置就绪后构造并注入；GUI 每帧读取。
    ///
    /// 用 `Mutex<Option<..>>` 而非 `OnceLock`：引擎构造依赖运行时才拿到的客户端配置
    /// （服务端地址/客户端名），且需在控制通道启动前完成。
    remote: Mutex<Option<Arc<RemoteManager>>>,
}

impl AppState {
    #[must_use]
    pub fn new(initial_status: ClientStatus) -> Self {
        let (tx, rx) = watch::channel(initial_status);
        let tx = Arc::new(tx);
        let (owner_tx, owner_rx) = watch::channel(IdentityOwner::Gui);
        Self {
            status_rx: rx,
            status_tx: tx,
            log_buffer: Arc::new(LogBuffer::default()),
            config: Arc::new(Mutex::new(None)),
            reconnect_requested: Arc::new(AtomicBool::new(false)),
            should_exit: Arc::new(AtomicBool::new(false)),
            service_owner_rx: owner_rx,
            service_owner_tx: Arc::new(owner_tx),
            remote: Mutex::new(None),
        }
    }

    /// 当前谁在承载客户端身份（UI 每帧读取，`watch` 读取是同步且廉价的）。
    #[must_use]
    pub fn service_owner(&self) -> IdentityOwner {
        *self.service_owner_rx.borrow()
    }

    /// 注入远程控制引擎（后台运行时线程调用一次）。
    pub fn set_remote(&self, manager: Arc<RemoteManager>) {
        if let Ok(mut guard) = self.remote.lock() {
            *guard = Some(manager);
        }
    }

    /// 取远程控制引擎（未就绪时为 `None`）。
    #[must_use]
    pub fn remote(&self) -> Option<Arc<RemoteManager>> {
        self.remote.lock().ok().and_then(|guard| guard.clone())
    }

    #[must_use]
    pub fn config_snapshot(&self) -> Option<ClientConfig> {
        self.config.lock().ok().and_then(|g| g.clone())
    }

    pub fn set_config(&self, cfg: Option<ClientConfig>) {
        if let Ok(mut g) = self.config.lock() {
            *g = cfg;
        }
    }
}
