use std::sync::{atomic::AtomicBool, Arc, Mutex};

use tokio::sync::watch;

use rust_tunnel_client::{ClientConfig, ClientStatus, LogBuffer};
use rust_tunnel_remote::RemoteManager;

pub struct AppState {
    pub status_rx: watch::Receiver<ClientStatus>,
    pub status_tx: Arc<watch::Sender<ClientStatus>>,
    pub log_buffer: Arc<LogBuffer>,
    pub config: Arc<Mutex<Option<ClientConfig>>>,
    pub reconnect_requested: Arc<AtomicBool>,
    pub should_exit: Arc<AtomicBool>,
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
        Self {
            status_rx: rx,
            status_tx: tx,
            log_buffer: Arc::new(LogBuffer::default()),
            config: Arc::new(Mutex::new(None)),
            reconnect_requested: Arc::new(AtomicBool::new(false)),
            should_exit: Arc::new(AtomicBool::new(false)),
            remote: Mutex::new(None),
        }
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
