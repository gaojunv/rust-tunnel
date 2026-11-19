//! 被控端采集 + 编码：默认回放设备的 loopback → Opus 20 ms 帧。
//!
//! 这一层只负责「设备 → 编码包」，不做会话协议。`client-gui` 用一个薄适配器把它包成
//! `rust_tunnel_remote::AudioSource`，从而保持 `win-audio` 不依赖引擎 crate。
//!
//! # 设备失效
//!
//! 拔耳机/切默认设备会让 `read_block` 返回错误。这里**不把错误抛给上层**：丢掉设备句柄，
//! 2 秒后从当前默认设备重建，期间返回 `Ok(None)`。只有编码器本身失败（基本不会发生）
//! 才会返回错误。

use crate::error::AudioResult;
use crate::format::{CHANNELS, FRAME_FLOATS, OPUS_BITRATE_BPS, SAMPLE_RATE};
use crate::opus::{Encoder, MAX_PACKET_BYTES};
use crate::wasapi::Capture;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 设备重建的退避间隔。
const REBUILD_BACKOFF: Duration = Duration::from_secs(2);

/// 一帧编码后的音频。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Opus 包。
    pub data: Vec<u8>,
    /// 采集完成时刻（进程单调时钟，微秒）。
    pub capture_ts_us: u64,
}

/// loopback 采集 + Opus 编码。
pub struct LoopbackEncoder {
    encoder: Encoder,
    capture: Option<Capture>,
    pcm: Vec<f32>,
    packet: Vec<u8>,
    /// 上一次尝试重建设备的时刻；`None` = 当前有设备或尚未失败。
    retry_at: Option<Instant>,
    /// 连续重建失败次数（只用于日志）。
    failures: u32,
}

impl LoopbackEncoder {
    /// 创建编码器（不打开设备）。
    ///
    /// # Errors
    /// libopus 不可用或参数非法时返回错误。
    pub fn new() -> AudioResult<Self> {
        Ok(Self {
            encoder: Encoder::new(SAMPLE_RATE, CHANNELS, OPUS_BITRATE_BPS)?,
            capture: None,
            pcm: vec![0.0; FRAME_FLOATS],
            packet: vec![0_u8; MAX_PACKET_BYTES],
            retry_at: None,
            failures: 0,
        })
    }

    /// 打开当前默认回放设备的 loopback。
    ///
    /// # Errors
    /// 没有回放设备或初始化失败时返回 [`AudioError::Windows`]。
    pub fn start(&mut self) -> AudioResult<()> {
        self.capture = Some(Capture::open_default_loopback()?);
        self.retry_at = None;
        self.failures = 0;
        Ok(())
    }

    /// 当前是否有可用的采集设备。
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.capture.is_some()
    }

    /// 读取并编码下一帧。
    ///
    /// - `Ok(Some(frame))`：产出一帧；
    /// - `Ok(None)`：`stop` 已置位、设备正在退避重建，或本次等待窗口内没有攒够一帧；
    /// - `Err`：编码器失败（不可恢复）。
    ///
    /// # Errors
    /// Opus 编码失败时返回错误；设备错误会在内部退避重建，不在这里抛出。
    pub fn read(&mut self, stop: &AtomicBool) -> AudioResult<Option<EncodedFrame>> {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if self.capture.is_none() {
            self.try_rebuild();
            return Ok(None);
        }

        let Some(capture) = self.capture.as_mut() else {
            return Ok(None);
        };
        match capture.read_block(&mut self.pcm, Duration::from_millis(200)) {
            Ok(true) => {
                let written = self.encoder.encode(&self.pcm, &mut self.packet)?;
                let data = self.packet[..written].to_vec();
                Ok(Some(EncodedFrame {
                    data,
                    capture_ts_us: crate::clock::now_us(),
                }))
            }
            Ok(false) => Ok(None),
            Err(err) => {
                // 设备失效：丢掉句柄，退避后从新的默认设备重建。**不向上抛**——会话应当
                // 继续（画面/输入不受影响），用户从统计或日志里看到的是「声音中断后恢复」。
                tracing::warn!(error = %err, "音频采集设备失效，2 秒后重建");
                self.capture = None;
                self.retry_at = Some(Instant::now() + REBUILD_BACKOFF);
                Ok(None)
            }
        }
    }

    /// 停止采集并释放设备。
    pub fn stop(&mut self) {
        self.capture = None;
        self.retry_at = None;
    }

    /// 到点后尝试重建；失败则延长退避。
    fn try_rebuild(&mut self) {
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        match Capture::open_default_loopback() {
            Ok(capture) => {
                tracing::info!("音频采集设备已重建");
                self.capture = Some(capture);
                self.retry_at = None;
                self.failures = 0;
            }
            Err(err) => {
                self.failures = self.failures.saturating_add(1);
                self.retry_at = Some(Instant::now() + REBUILD_BACKOFF);
                // 只在第一次和每 30 次（约 1 分钟）记一条，避免日志刷屏。
                if self.failures == 1 || self.failures.is_multiple_of(30) {
                    tracing::warn!(error = %err, failures = self.failures, "音频采集设备重建失败，稍后重试");
                }
            }
        }
    }
}

impl Drop for LoopbackEncoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 当前是否有活动回放设备（供能力探测；失败按「没有」处理）。
#[must_use]
pub fn has_output_device() -> bool {
    crate::wasapi::list_render_devices().is_ok_and(|devices| !devices.is_empty())
}
