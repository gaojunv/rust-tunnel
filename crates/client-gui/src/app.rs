//! GUI 应用状态（watch/日志/配置/重连控制）。

use std::sync::Arc;

use tokio::sync::watch;

use rust_tunnel_client::{ClientConfig, ClientStatus, LogBuffer};

/// GUI 全局状态。
pub struct AppState {
    /// 连接状态 watch 接收端。
    pub status_rx: watch::Receiver<ClientStatus>,
    /// 连接状态 watch 发送端（由 control 回调驱动）。
    pub status_tx: Arc<watch::Sender<ClientStatus>>,
    /// 本地环形日志缓冲。
    pub log_buffer: Arc<LogBuffer>,
    /// 当前生效的配置（设置面板编辑后经保存落盘并触发重连）。
    pub config: Option<ClientConfig>,
    /// 重连取消标记：设置后后台 run 循环的下一次迭代感知并走重连。
    pub reconnect_requested: Arc<std::sync::atomic::AtomicBool>,
}

impl AppState {
    /// 创建初始状态。
    #[must_use]
    pub fn new(initial_status: ClientStatus) -> Self {
        let (tx, rx) = watch::channel(initial_status);
        let tx = Arc::new(tx);
        Self {
            status_rx: rx,
            status_tx: tx,
            log_buffer: Arc::new(LogBuffer::default()),
            config: None,
            reconnect_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}
