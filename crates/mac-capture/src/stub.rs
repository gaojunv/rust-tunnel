//! 非 macOS 的占位实现：保证本 crate 在 Windows/Linux 上空壳编译。
//!
//! 所有真实采集都在 `sck`（`cfg(target_os = "macos")`）。这里保留完整签名是为了让
//! `cargo check -p rust-tunnel-mac-capture` 在开发机（Windows）上能验证调用侧形状，
//! 也让 `client-gui` 的条件编译少一处分叉——真正在非 macOS 上调用采集会得到
//! [`MacCaptureError::Unsupported`]。

use crate::{
    CaptureInfo, CapturedFrame, CapturedGpuFrame, CapturedPatches, CursorShape, DisplayInfo,
    MacCaptureError, MacCaptureResult,
};

/// ScreenCaptureKit 采集源的占位实现（非 macOS）。
#[derive(Debug, Default)]
pub struct MacCapture;

impl MacCapture {
    /// 创建一个占位采集源。
    ///
    /// # Errors
    /// 占位实现不会失败。
    pub fn new() -> MacCaptureResult<Self> {
        Ok(Self)
    }

    /// 当前显示器列表（非 macOS 恒为空）。
    #[must_use]
    pub fn displays(&self) -> Vec<DisplayInfo> {
        Vec::new()
    }

    /// 设置本会话的编码宽度上限（占位实现忽略）。
    pub fn set_max_encode_width(&mut self, _width: u32) {}

    /// 采集一帧（非 macOS 恒失败）。
    ///
    /// # Errors
    /// 恒返回 [`MacCaptureError::Unsupported`]。
    pub fn capture(&mut self, _display: u32) -> MacCaptureResult<CapturedFrame> {
        Err(MacCaptureError::Unsupported(
            "仅 macOS 支持 ScreenCaptureKit 采集",
        ))
    }

    /// 采集一帧脏矩形补丁（非 macOS 恒失败）。
    ///
    /// # Errors
    /// 恒返回 [`MacCaptureError::Unsupported`]。
    pub fn capture_patches(
        &mut self,
        _display: u32,
        _scale: f32,
        _max_width: u32,
    ) -> MacCaptureResult<Option<CapturedPatches>> {
        Err(MacCaptureError::Unsupported(
            "仅 macOS 支持 ScreenCaptureKit 采集",
        ))
    }

    /// 本采集源是否支持 GPU 直通采集（非 macOS 恒 `false`）。
    #[must_use]
    pub fn supports_gpu_capture(&self) -> bool {
        false
    }

    /// 采集一帧 GPU 直通画面（非 macOS 恒失败）。
    ///
    /// # Errors
    /// 恒返回 [`MacCaptureError::Unsupported`]。
    pub fn capture_gpu(&mut self, _display: u32) -> MacCaptureResult<CapturedGpuFrame> {
        Err(MacCaptureError::Unsupported(
            "仅 macOS 支持 ScreenCaptureKit GPU 采集",
        ))
    }

    /// 远端光标形状（非 macOS 恒为 `None`）。
    #[must_use]
    pub fn cursor_shape(&self) -> Option<CursorShape> {
        None
    }

    /// 采集源的平台信息（占位实现只报后端名）。
    #[must_use]
    pub fn info(&self) -> CaptureInfo {
        CaptureInfo {
            backend: "ScreenCaptureKit".to_owned(),
            ..CaptureInfo::default()
        }
    }

    /// 停止采集（占位实现无事可做）。
    pub fn stop(&mut self) {}
}
