//! 无头控制器宿主：生命周期、重连循环与状态聚合。

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use rust_tunnel_client::{ClientConfig, ClientStatus, ReconnectPolicy};
use rust_tunnel_remote::signaling::LazyTransport;
use rust_tunnel_remote::{
    ApprovalPolicy, EncodeOptions, FrameSink, RemoteManager, RemoteManagerConfig, RemoteUiState,
};

use crate::backoff::Backoff;
use crate::commands::ControllerCommands;
use crate::config::ControllerConfig;
use crate::error::{ControllerError, ControllerResult};
use crate::state::{ControllerSnapshot, LoopPhase};

/// `stop()` 等待后台任务收尾的上限；超时后直接 abort。
pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// 无头控制器宿主。
///
/// 构造即启动：一个任务跑控制通道重连循环，另一个把三份原始状态聚合成
/// [`ControllerSnapshot`] 广播给 UI。`Drop` 会尽力发出停止信号；需要确定性收尾时
/// 显式调用 [`ControllerHost::stop`]。
pub struct ControllerHost {
    /// 远程控制引擎（控制器视角）。
    manager: Arc<RemoteManager>,
    /// 启动配置。
    config: ControllerConfig,
    /// 控制通道状态广播（原始）。
    status_rx: watch::Receiver<ClientStatus>,
    /// 远程引擎状态广播（原始）。
    remote_rx: watch::Receiver<RemoteUiState>,
    /// 聚合快照广播。
    snapshot_rx: watch::Receiver<ControllerSnapshot>,
    /// 宿主重连阶段广播。
    phase_rx: watch::Receiver<LoopPhase>,
    /// 停止信号发送端。
    shutdown_tx: watch::Sender<bool>,
    /// 后台任务（重连循环 + 状态镜像）。
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for ControllerHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerHost")
            .field("server", &self.config.server)
            .field("client_name", &self.config.client_name)
            .finish_non_exhaustive()
    }
}

impl ControllerHost {
    /// 校验配置、装配引擎并启动后台任务。
    ///
    /// **必须在 Tokio 运行时内调用**（Android 层先建 runtime，再在 `block_on` 或
    /// runtime 线程里调用）。管理器会在构造时捕获运行时句柄，用于派生会话任务。
    ///
    /// # Errors
    ///
    /// 配置不合法、当前线程没有 Tokio 运行时、或下载目录无法创建时返回错误。
    pub fn start(config: ControllerConfig) -> ControllerResult<Self> {
        config.validate()?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| ControllerError::NoRuntime)?;
        let client_config = config.to_client_config();
        let manager = build_manager(&config)?;

        let initial_status = ClientStatus::new(
            config.server.clone(),
            config.client_name.clone(),
            env!("CARGO_PKG_VERSION").to_owned(),
        );
        let (status_tx, status_rx) = watch::channel(initial_status);
        let (phase_tx, phase_rx) = watch::channel(LoopPhase::Connecting);
        let remote_rx = manager.status();
        let (snapshot_tx, snapshot_rx) = watch::channel(ControllerSnapshot::derive(
            &status_rx.borrow(),
            &LoopPhase::Connecting,
            &remote_rx.borrow(),
        ));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let shared = Arc::new(HostShared {
            manager: Arc::clone(&manager),
            client_config,
            status_tx: Arc::new(status_tx),
            phase_tx,
        });

        let loop_task = runtime.spawn(run_loop(Arc::clone(&shared), shutdown_rx.clone()));
        let mirror_task = runtime.spawn(run_mirror(
            status_rx.clone(),
            phase_rx.clone(),
            remote_rx.clone(),
            snapshot_tx,
            shutdown_rx,
        ));

        Ok(Self {
            manager,
            config,
            status_rx,
            remote_rx,
            snapshot_rx,
            phase_rx,
            shutdown_tx,
            tasks: vec![loop_task, mirror_task],
        })
    }

    /// 远程控制引擎门面。
    #[must_use]
    pub fn manager(&self) -> &Arc<RemoteManager> {
        &self.manager
    }

    /// 命令句柄（廉价克隆，可传给你自己的 UI 线程）。
    #[must_use]
    pub fn commands(&self) -> ControllerCommands {
        ControllerCommands::new(Arc::clone(&self.manager))
    }

    /// 启动配置。
    #[must_use]
    pub fn config(&self) -> &ControllerConfig {
        &self.config
    }

    /// 下载目录（引擎把远端文件落到这里）。
    #[must_use]
    pub fn download_dir(&self) -> &Path {
        &self.config.download_dir
    }

    /// 订阅聚合快照。
    #[must_use]
    pub fn snapshots(&self) -> watch::Receiver<ControllerSnapshot> {
        self.snapshot_rx.clone()
    }

    /// 当前聚合快照。
    #[must_use]
    pub fn snapshot(&self) -> ControllerSnapshot {
        ControllerSnapshot::derive(
            &self.status_rx.borrow(),
            &self.phase_rx.borrow(),
            &self.remote_rx.borrow(),
        )
    }

    /// 订阅控制通道原始状态。
    #[must_use]
    pub fn client_status(&self) -> watch::Receiver<ClientStatus> {
        self.status_rx.clone()
    }

    /// 订阅远程引擎原始状态（除快照外的完整读数都在这里）。
    #[must_use]
    pub fn remote_state(&self) -> watch::Receiver<RemoteUiState> {
        self.remote_rx.clone()
    }

    /// 注入宿主实现的画面/剪贴板/光标回调（Android 层的渲染管线）。
    pub fn set_frame_sink(&self, sink: Arc<dyn FrameSink>) {
        self.manager.install_frame_sink(sink);
    }

    /// 干净停止：发出停止信号并等待后台任务收尾（超时则 abort）。
    ///
    /// 控制通道会在消息循环内走完整收尾（停心跳、清隧道表、通知引擎断开），
    /// 状态广播最终落在 `Stopped`。
    pub async fn stop(mut self) {
        let _ = self.shutdown_tx.send(true);
        for mut task in self.tasks.drain(..) {
            if tokio::time::timeout(STOP_TIMEOUT, &mut task).await.is_err() {
                task.abort();
            }
        }
    }
}

impl Drop for ControllerHost {
    fn drop(&mut self) {
        // 尽力而为：任务在下一个 await 点看到信号后自行退出。
        let _ = self.shutdown_tx.send(true);
    }
}

/// 后台任务共享状态。
struct HostShared {
    /// 远程控制引擎。
    manager: Arc<RemoteManager>,
    /// 控制通道配置（每次重连克隆一份）。
    client_config: ClientConfig,
    /// 控制通道状态广播端。
    status_tx: Arc<watch::Sender<ClientStatus>>,
    /// 重连阶段广播端。
    phase_tx: watch::Sender<LoopPhase>,
}

/// 按控制器视角装配 [`RemoteManager`]：不承担被控、只解控制器需要的 codec。
///
/// 解码器工厂全部来自 [`ControllerConfig`]（未注入时为 `None`，协商结果只有 VP9）；
/// `CodecOffer` 由引擎按工厂能力自动填写。
pub(crate) fn build_manager(config: &ControllerConfig) -> ControllerResult<Arc<RemoteManager>> {
    // 下载目录必须存在，否则文件传输在收尾阶段才会失败。Android 应传入应用私有目录。
    std::fs::create_dir_all(&config.download_dir).map_err(|e| {
        ControllerError::InvalidConfig(format!(
            "无法创建下载目录 {}：{e}",
            config.download_dir.display()
        ))
    })?;

    let manager = RemoteManager::new(manager_config(config));
    manager.set_auto_lock_on_disconnect(false);
    Ok(manager)
}

/// 由宿主配置拼出引擎配置（纯函数，便于单测注入结果）。
///
/// 控制器**不承担被控**：`enabled = false`、不共享目录、不注入任何编码器或音频设备。
/// 解码器工厂与 `prefer_h265` 原样透传——注入后本端才会在被控端协商时声明 H.264/H.265 能力。
pub(crate) fn manager_config(config: &ControllerConfig) -> RemoteManagerConfig {
    RemoteManagerConfig {
        server_host: config.server.clone(),
        self_client_name: config.client_name.clone(),
        // 仅用于遥测展示，不参与 codec/能力协商。
        platform: "android".to_owned(),
        displays: 1,
        // 控制器不承担被控：不接受接入请求、不上报「可被远程控制」。
        enabled: false,
        establisher: Arc::new(LazyTransport::new(config.server.clone())),
        native_resolution: false,
        encode: EncodeOptions::default(),
        // 控制器不共享目录、不提供服务端文件。
        share_dir: None,
        download_dir: config.download_dir.clone(),
        approval_policy: ApprovalPolicy::Confirm,
        // 控制器只解不编：编码器工厂恒为 `None`。
        h264_encoder_factory: None,
        h264_decoder_factory: config.h264_decoder_factory.clone(),
        h265_encoder_factory: None,
        h265_decoder_factory: config.h265_decoder_factory.clone(),
        prefer_h265: config.prefer_h265,
        prefer_444: false,
        audio_source: None,
        audio_sink: None,
        audio_enabled: false,
        audio_allowed: Arc::new(AtomicBool::new(false)),
        punch_migration: true,
    }
}

/// 控制通道重连循环。
async fn run_loop(shared: Arc<HostShared>, mut shutdown: watch::Receiver<bool>) {
    let mut backoff = Backoff::new();
    loop {
        if *shutdown.borrow_and_update() {
            break;
        }
        let _ = shared.phase_tx.send(LoopPhase::Connecting);
        let result = rust_tunnel_client::control::run_client_with_status_shutdown(
            shared.client_config.clone(),
            Some(Arc::clone(&shared.status_tx)),
            None,
            Some(shared.manager.bridge()),
            None,
            shutdown.clone(),
        )
        .await;

        // 停止请求：函数内部已走完收尾，不再重连。
        if *shutdown.borrow_and_update() {
            break;
        }

        let reason = match result {
            Ok(()) => {
                backoff.reset();
                None
            }
            Err(e) if !ReconnectPolicy::should_reconnect(&e) => {
                let message = e.to_string();
                set_status_error(&shared, &message);
                let _ = shared
                    .phase_tx
                    .send(LoopPhase::Rejected { reason: message });
                return;
            }
            Err(e) => {
                let message = e.to_string();
                tracing::warn!("控制通道断开，准备重连：{message}");
                set_status_error(&shared, &message);
                Some(message)
            }
        };

        let _ = shared.phase_tx.send(LoopPhase::BackingOff { reason });
        let delay = backoff.next_delay();
        tracing::info!("{}s 后重连控制通道", delay.as_secs());
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = wait_shutdown(&mut shutdown) => break,
        }
    }
    let _ = shared.phase_tx.send(LoopPhase::Stopped);
}

/// 把两份原始 `watch` 聚合成一份快照广播。
async fn run_mirror(
    mut status_rx: watch::Receiver<ClientStatus>,
    mut phase_rx: watch::Receiver<LoopPhase>,
    mut remote_rx: watch::Receiver<RemoteUiState>,
    snapshot_tx: watch::Sender<ControllerSnapshot>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let snapshot = ControllerSnapshot::derive(
            &status_rx.borrow(),
            &phase_rx.borrow(),
            &remote_rx.borrow(),
        );
        if snapshot_tx.send(snapshot).is_err() {
            break;
        }
        tokio::select! {
            r = status_rx.changed() => if r.is_err() { break; },
            r = phase_rx.changed() => if r.is_err() { break; },
            r = remote_rx.changed() => if r.is_err() { break; },
            () = wait_shutdown(&mut shutdown) => break,
        }
    }
}

/// 等待停止信号变为 `true`；发送端被丢弃时永不 resolve。
async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// 把「已断开 + 原因」写进控制通道状态（重连循环侧没有 `ClientState` 可复用）。
fn set_status_error(shared: &HostShared, message: &str) {
    let mut status = shared.status_tx.borrow().clone();
    status.connected = false;
    status.last_error = Some(message.to_owned());
    let _ = shared.status_tx.send(status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_tunnel_remote::{CodecId, RemoteError, RemoteResult, VideoDecoderApi};

    /// 只用来验证注入是否透传的解码器工厂（`create` 恒失败，不会被真正创建）。
    #[derive(Debug)]
    struct StubDecoder(CodecId);

    impl rust_tunnel_remote::VideoDecoderFactory for StubDecoder {
        fn codec(&self) -> CodecId {
            self.0
        }

        fn create(&self) -> RemoteResult<Box<dyn VideoDecoderApi>> {
            Err(RemoteError::Codec("测试桩不创建解码器".to_owned()))
        }
    }

    /// 未注入工厂时引擎只能协商 VP9：编码/解码工厂全 `None`，但 H.265 偏好仍为默认 `true`
    /// （偏好本身不构成能力声明）。
    #[test]
    fn manager_config_defaults_to_vp9_only() {
        let cfg = ControllerConfig::new("host:3000", "tok");
        let engine = manager_config(&cfg);
        assert!(engine.h264_decoder_factory.is_none());
        assert!(engine.h265_decoder_factory.is_none());
        assert!(engine.h264_encoder_factory.is_none());
        assert!(engine.h265_encoder_factory.is_none());
        assert!(engine.prefer_h265, "默认偏好应与桌面端一致为 true");
        assert!(!engine.prefer_444);
        assert!(!engine.enabled, "控制器不承担被控");
        assert!(engine.share_dir.is_none());
        assert!(engine.audio_source.is_none());
        assert!(engine.audio_sink.is_none());
        assert!(!engine.audio_enabled);
        assert_eq!(engine.self_client_name, cfg.client_name);
        assert_eq!(engine.server_host, cfg.server);
        assert_eq!(engine.download_dir, cfg.download_dir);
        assert_eq!(engine.platform, "android");
    }

    /// 注入的解码器工厂与 `prefer_h265` 必须原样透传到 [`RemoteManagerConfig`]。
    #[test]
    fn manager_config_passes_decoder_factories_through() {
        let cfg = ControllerConfig::new("host:3000", "tok")
            .with_h264_decoder(Arc::new(StubDecoder(CodecId::H264)))
            .with_h265_decoder(Arc::new(StubDecoder(CodecId::H265)))
            .with_prefer_h265(false);
        let engine = manager_config(&cfg);
        assert_eq!(
            engine.h264_decoder_factory.as_ref().map(|f| f.codec()),
            Some(CodecId::H264)
        );
        assert_eq!(
            engine.h265_decoder_factory.as_ref().map(|f| f.codec()),
            Some(CodecId::H265)
        );
        assert!(!engine.prefer_h265);
        // 控制器永远不会注入编码器（它只解不编）。
        assert!(engine.h264_encoder_factory.is_none());
        assert!(engine.h265_encoder_factory.is_none());
    }

    /// 只注入 H.264（设备无 HEVC 硬解）也是合法的组合，H.265 偏好保持默认。
    #[test]
    fn manager_config_allows_h264_only() {
        let cfg = ControllerConfig::new("host:3000", "tok")
            .with_h264_decoder(Arc::new(StubDecoder(CodecId::H264)));
        let engine = manager_config(&cfg);
        assert!(engine.h264_decoder_factory.is_some());
        assert!(engine.h265_decoder_factory.is_none());
        assert!(engine.prefer_h265);
    }
}
