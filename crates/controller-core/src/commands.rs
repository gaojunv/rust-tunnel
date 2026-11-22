//! 控制器对外命令 API。
//!
//! 全部是普通同步方法（内部把消息投进引擎的通道/缓冲），可在 JNI 线程直接调用；
//! 没有会话时命令静默丢弃（与桌面 GUI 的语义一致），参数错误则返回
//! [`ControllerError::InvalidCommand`] 且**不下发**。

use std::path::PathBuf;
use std::sync::Arc;

use rust_tunnel_common::RemoteMode;
use rust_tunnel_remote::{InputEvent, QualityPreset, RemoteManager};

use crate::error::{ControllerError, ControllerResult};

/// 画质档位对应的最低帧率（与引擎 QoS 地板一致）。
pub const MIN_FRAME_RATE: u8 = 15;
/// 画质档位对应的最高帧率。
pub const MAX_FRAME_RATE: u8 = 60;

/// 控制器命令句柄（廉价克隆，可与 UI 线程共享）。
#[derive(Clone)]
pub struct ControllerCommands {
    /// 远程控制引擎门面。
    manager: Arc<RemoteManager>,
}

impl ControllerCommands {
    /// 由引擎门面构造（宿主内部使用）。
    pub(crate) fn new(manager: Arc<RemoteManager>) -> Self {
        Self { manager }
    }

    /// 取底层引擎（需要直接访问未封装的 API 时）。
    #[must_use]
    pub fn manager(&self) -> &Arc<RemoteManager> {
        &self.manager
    }

    /// 请求连接某个在线客户端。
    ///
    /// # Errors
    ///
    /// `target_client` 为空时返回 [`ControllerError::InvalidCommand`]。
    pub fn connect(&self, target_client: &str, mode: RemoteMode) -> ControllerResult<()> {
        let target = target_client.trim();
        if target.is_empty() {
            return Err(ControllerError::InvalidCommand(
                "目标客户端名不能为空".to_owned(),
            ));
        }
        self.manager.request_connect(target, mode);
        Ok(())
    }

    /// 中断当前会话（不分角色）。
    pub fn cancel_session(&self) {
        self.manager.abort_active();
    }

    /// 发送一条输入事件（鼠标/滚轮/键盘）。
    pub fn send_input(&self, event: InputEvent) {
        self.manager.send_input(event);
    }

    /// 把本机剪贴板文本发给被控端。
    pub fn send_clipboard(&self, text: String) {
        self.manager.send_clipboard(text);
    }

    /// 请求被控端把它当前的剪贴板发过来（结果经 `FrameSink::on_clipboard` 回传）。
    pub fn request_clipboard(&self) {
        self.manager.request_clipboard();
    }

    /// 切换被控端的显示器。
    pub fn select_display(&self, display: u32) {
        self.manager.select_display(display);
    }

    /// 设置画质档位（由被控端生效）。
    pub fn set_quality(&self, preset: QualityPreset) {
        self.manager.set_quality(preset);
    }

    /// 设置帧率上限（[`MIN_FRAME_RATE`]..=[`MAX_FRAME_RATE`]）。
    ///
    /// # Errors
    ///
    /// 帧率越界时返回 [`ControllerError::InvalidCommand`]。
    pub fn set_frame_rate(&self, fps: u8) -> ControllerResult<()> {
        if !(MIN_FRAME_RATE..=MAX_FRAME_RATE).contains(&fps) {
            return Err(ControllerError::InvalidCommand(format!(
                "帧率必须在 {MIN_FRAME_RATE}..={MAX_FRAME_RATE} 之间，收到 {fps}"
            )));
        }
        self.manager.set_frame_rate(fps);
        Ok(())
    }

    /// 请求被控端锁定屏幕。
    pub fn lock_screen(&self) {
        self.manager.lock_screen();
    }

    /// 立即尝试一次会话内路径迁移（中继 → P2P 打洞）。
    pub fn migrate_now(&self) {
        self.manager.trigger_migration();
    }

    /// 设置是否允许会话内路径迁移（只对下一次会话生效）。
    pub fn set_punch_migration(&self, on: bool) {
        self.manager.set_punch_migration(on);
    }

    /// 立刻探测一次延迟（结果异步进快照的 `latency_ms`）。
    pub fn ping(&self) {
        self.manager.ping();
    }

    /// 请求被控端启停音频采集（本地播放由宿主实现）。
    pub fn set_audio_enabled(&self, enabled: bool) {
        self.manager.set_audio_enabled(enabled);
    }

    /// 列出被控端共享目录（空串视为虚拟根 `/`）。
    pub fn list_remote_dir(&self, path: &str) {
        let path = if path.trim().is_empty() { "/" } else { path };
        self.manager.list_remote_dir(path);
    }

    /// 下载被控端的某个文件到宿主的下载目录。
    ///
    /// # Errors
    ///
    /// 远端路径为空时返回 [`ControllerError::InvalidCommand`]。
    pub fn download_remote_file(&self, path: &str) -> ControllerResult<()> {
        require_remote_path(path)?;
        self.manager.download_remote_file(path);
        Ok(())
    }

    /// 上传本机文件到被控端的共享目录。
    ///
    /// # Errors
    ///
    /// 本地路径或远端路径为空时返回 [`ControllerError::InvalidCommand`]。
    pub fn upload_local_file(&self, local: PathBuf, remote: &str) -> ControllerResult<()> {
        if local.as_os_str().is_empty() {
            return Err(ControllerError::InvalidCommand(
                "本地上传路径不能为空".to_owned(),
            ));
        }
        require_remote_path(remote)?;
        self.manager.upload_local_file(local, remote);
        Ok(())
    }

    /// 取消一次在途传输。
    pub fn cancel_transfer(&self, id: u64) {
        self.manager.cancel_transfer(id);
    }
}

/// 远端虚拟路径必须非空。
fn require_remote_path(path: &str) -> ControllerResult<()> {
    if path.trim().is_empty() {
        return Err(ControllerError::InvalidCommand(
            "远端路径不能为空".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ControllerConfig;
    use crate::host::build_manager;

    fn commands() -> ControllerCommands {
        let cfg = ControllerConfig::new("host:3000", "token");
        ControllerCommands::new(build_manager(&cfg).expect("manager"))
    }

    #[test]
    fn connect_rejects_empty_target() {
        let c = commands();
        assert!(c.connect("  ", RemoteMode::Control).is_err());
        assert!(c.connect("home-pc", RemoteMode::Control).is_ok());
    }

    #[test]
    fn frame_rate_range_is_enforced() {
        let c = commands();
        assert!(c.set_frame_rate(14).is_err());
        assert!(c.set_frame_rate(61).is_err());
        assert!(c.set_frame_rate(15).is_ok());
        assert!(c.set_frame_rate(60).is_ok());
    }

    #[test]
    fn file_paths_are_validated() {
        let c = commands();
        assert!(c.download_remote_file("").is_err());
        assert!(c.download_remote_file("/etc/hosts").is_ok());
        assert!(c.upload_local_file(PathBuf::new(), "/tmp/a").is_err());
        assert!(c.upload_local_file(PathBuf::from("a.txt"), " ").is_err());
        assert!(c
            .upload_local_file(PathBuf::from("a.txt"), "/tmp/a")
            .is_ok());
    }
}
