use std::sync::{Arc, Mutex, atomic::AtomicBool};

use tokio::sync::watch;

use rust_tunnel_client::{ClientConfig, ClientStatus, LogBuffer};

pub struct AppState {
    pub status_rx: watch::Receiver<ClientStatus>,
    pub status_tx: Arc<watch::Sender<ClientStatus>>,
    pub log_buffer: Arc<LogBuffer>,
    pub config: Arc<Mutex<Option<ClientConfig>>>,
    pub reconnect_requested: Arc<AtomicBool>,
    pub should_exit: Arc<AtomicBool>,
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
        }
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
