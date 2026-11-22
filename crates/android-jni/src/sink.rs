//! 帧回调适配：`FrameSink` 实现 + 远端画面尺寸跟踪。
//!
//! [`FrameSink`] 的方法都在**引擎的运行时线程**上被调用，且会话的吞吐直接取决于它的返回速度：
//! - 整帧与补丁都只做「推入同一条有界 FIFO（整帧容量 16 / 补丁容量 32，满则丢最旧）」，
//!   绝不阻塞；
//! - 尺寸变化才回调 `onFrameInfo`（Kotlin 用它把触摸坐标映射成远端像素）；
//! - 显示器列表/剪贴板/光标/关闭都转成 JSON 或整数回调，调用频率都很低。
//!
//! 补丁（协议 v6 `RegionUpdate`）与整帧走**同一条队列**，渲染线程按 FIFO 顺序把它们叠进
//! 持久 NV12 合成缓冲——顺序就是引擎解码队列的顺序，不需要在这里再做同步。

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rust_tunnel_controller_core::{
    CursorShape, DecodedFrame, DecodedPatch, DisplayInfo, FrameSink,
};

use crate::bridge::JniBridge;
use crate::render::RenderHandle;
use crate::state_json::{displays_json, FrameGeometry};

/// 最近一帧远端画面的几何（`FrameSink::on_frame` 写，状态 JSON 读）。
pub(crate) struct FrameInfo {
    /// `(宽, 高, 显示器, 是否已有过帧)`。
    geometry: Mutex<(u32, u32, u32, bool)>,
}

impl FrameInfo {
    /// 新建（尚未见过任何帧）。
    pub(crate) fn new() -> Self {
        Self {
            geometry: Mutex::new((0, 0, 0, false)),
        }
    }

    /// 加锁（中毒时取回内层数据：一帧回调 panic 不应让状态读取永久失败）。
    fn lock(&self) -> MutexGuard<'_, (u32, u32, u32, bool)> {
        self.geometry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 记录一帧的几何；尺寸或显示器变化时回调 `onFrameInfo(width, height, display)`。
    pub(crate) fn note(&self, bridge: &JniBridge, width: u32, height: u32, display: u32) {
        {
            let mut guard = self.lock();
            if guard.3 && guard.0 == width && guard.1 == height && guard.2 == display {
                return;
            }
            *guard = (width, height, display, true);
        }
        // 回调放在锁外：Kotlin 端可能立刻回问 nativeSnapshot。
        bridge.frame_info(
            i32::try_from(width).unwrap_or(i32::MAX),
            i32::try_from(height).unwrap_or(i32::MAX),
            i32::try_from(display).unwrap_or(0),
        );
    }

    /// 当前几何（还没出过帧时宽高为 0）。
    pub(crate) fn geometry(&self) -> FrameGeometry {
        let guard = self.lock();
        FrameGeometry {
            width: guard.0,
            height: guard.1,
            display: guard.2,
        }
    }
}

/// Android 宿主的 [`FrameSink`] 实现。
pub(crate) struct AndroidFrameSink {
    /// 回调桥。
    bridge: Arc<JniBridge>,
    /// 渲染线程投递句柄。
    render: RenderHandle,
    /// 画面几何共享状态。
    frame_info: Arc<FrameInfo>,
}

impl AndroidFrameSink {
    /// 装配 sink。
    pub(crate) fn new(
        bridge: Arc<JniBridge>,
        render: RenderHandle,
        frame_info: Arc<FrameInfo>,
    ) -> Self {
        Self {
            bridge,
            render,
            frame_info,
        }
    }
}

impl FrameSink for AndroidFrameSink {
    fn on_displays(&self, displays: Vec<DisplayInfo>) {
        self.bridge.displays(&displays_json(&displays));
    }

    fn on_frame(&self, frame: DecodedFrame) {
        self.frame_info
            .note(&self.bridge, frame.width, frame.height, frame.display);
        // 投递不阻塞：队列满时丢最旧帧。
        self.render.push_frame(frame);
    }

    fn on_patch(&self, patch: DecodedPatch) {
        // 补丁与整帧同一条 FIFO：渲染线程会在上一次整帧之上按序合成。
        self.render.push_patch(patch);
    }

    fn on_clipboard(&self, text: String) {
        self.bridge.clipboard(&text);
    }

    fn on_cursor(&self, shape: CursorShape) {
        // 判别式顺序即线上契约（0 = Arrow … 15 = Hidden）。
        self.bridge.cursor(shape as i32);
    }

    fn on_closed(&self, reason: String) {
        self.bridge.session_ended(&reason);
    }
}
