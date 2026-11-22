//! 渲染线程：EGL/GLES 上屏，帧/补丁队列**绝不阻塞**引擎。
//!
//! 分工：
//! - `FrameSink::on_frame` / `FrameSink::on_patch`（引擎运行时线程）只把消息推进
//!   **单一有界 FIFO**（[`crate::queue::BoundedQueue`]），整帧满则丢最旧、补丁满则丢最旧并
//!   记 warning，绝不阻塞；
//! - `nativeSetSurface`（JNI 线程）只推一条 [`RenderMsg::SetSurface`]，
//!   **不在 JNI 线程上碰任何 EGL 调用**（EGL 句柄与 GL 上下文都是线程绑定状态）；
//! - 本模块的专用线程独占 `EGLDisplay`/`EGLContext`/`EGLSurface`/`glow::Context`。
//!
//! 队列里只有整帧与补丁受容量约束；surface 设置/清除与关闭消息**永不丢弃**（丢帧只影响
//! 画面新旧，丢 surface 消息会让渲染目标错乱）。
//!
//! # 两条渲染路径
//!
//! - **RGBA 整帧**（VP9 软解）：上传一张 `GL_RGBA8` 纹理，原样贴屏；
//! - **NV12**（H.264/H.265 硬解）：整帧替换持久合成缓冲（Y `GL_R8` + UV `GL_RG8`），
//!   补丁用 `glTexSubImage2D` 只覆写矩形区域，随后按 BT.709 limited 上屏。
//!
//! 补丁**严格应用在上一次整帧之上**：引擎保证 `on_frame`/`on_patch` 同序，队列也保持 FIFO；
//! 没有基底帧或尺寸不符时丢弃并记 warning（引擎侧会在尺寸不符时请求关键帧）。

mod egl;
mod gl;

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use ndk::native_window::NativeWindow;
use rust_tunnel_controller_core::DecodedPatch;
use rust_tunnel_remote::DecodedFrame;

use crate::geometry::contain_rect;
use crate::queue::{BoundedQueue, MsgKind, PushOutcome, QueuedMsg};

/// 整帧队列上限（契约 §6）。
///
/// 16 帧在 60 fps 下约 270 ms：渲染线程偶尔被驱动/合成器卡住时仍能平滑追上，又不会让
/// 内存无限增长（4K NV12 一帧约 12.4 MB，16 帧即约 200 MB 上限）。
const MAX_QUEUED_FRAMES: usize = 16;

/// 补丁队列上限。
///
/// 补丁要比整帧少受淘汰（它们必须叠进持久帧缓冲），但渲染线程停摆时也不能无限堆积：
/// 触到上限丢最旧并记 warning（画面会短暂缺块，下一个整帧自愈）。
const MAX_QUEUED_PATCHES: usize = 32;

/// 发给渲染线程的一条消息。
pub(crate) enum RenderMsg {
    /// 设置/清除渲染目标（`None` = `nativeSetSurface(null)` 或窗口销毁）。
    SetSurface(Option<NativeWindow>),
    /// 一帧已解码的 RGBA 整帧（VP9）。
    FullRgba(Box<DecodedFrame>),
    /// 一帧已解码的 NV12 整帧（H.264/H.265）。
    FullNv12(Box<DecodedFrame>),
    /// 一批脏矩形补丁（协议 v6 `RegionUpdate`，已解压为 NV12）。
    Patch(Box<DecodedPatch>),
    /// 退出渲染线程并释放全部 EGL/GL 资源。
    Shutdown,
}

impl QueuedMsg for RenderMsg {
    fn kind(&self) -> MsgKind {
        match self {
            Self::SetSurface(_) | Self::Shutdown => MsgKind::Control,
            Self::FullRgba(_) | Self::FullNv12(_) => MsgKind::Frame,
            Self::Patch(_) => MsgKind::Patch,
        }
    }
}

/// 队列 + 条件变量。
struct RenderQueue {
    /// 共享队列（容量/淘汰策略在 [`BoundedQueue`] 里，主机可单测）。
    state: Mutex<BoundedQueue<RenderMsg>>,
    /// 有新消息或已关闭时唤醒渲染线程。
    ready: Condvar,
}

impl RenderQueue {
    /// 新建空队列。
    fn new() -> Self {
        Self {
            state: Mutex::new(BoundedQueue::new(MAX_QUEUED_FRAMES, MAX_QUEUED_PATCHES)),
            ready: Condvar::new(),
        }
    }

    /// 加锁（中毒时取回内层数据：渲染线程的 panic 不应让引擎侧的投递失败）。
    fn lock(&self) -> MutexGuard<'_, BoundedQueue<RenderMsg>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 投递一条消息并记录被淘汰的旧消息（本调用不阻塞）。
    fn push(&self, msg: RenderMsg) {
        let PushOutcome { accepted, evicted } = self.lock().push(msg);
        for dropped in &evicted {
            log_evicted(dropped);
        }
        if !accepted {
            tracing::trace!("渲染队列已关闭，丢弃消息");
            return;
        }
        self.ready.notify_one();
    }

    /// 取一条消息；队列空且未关闭时阻塞等待。返回 `None` 表示队列已关闭且耗尽。
    fn pop(&self) -> Option<RenderMsg> {
        let mut guard = self.lock();
        loop {
            if let Some(msg) = guard.pop_front() {
                return Some(msg);
            }
            if guard.is_closed() {
                return None;
            }
            guard = self
                .ready
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// 关闭队列并唤醒渲染线程。
    fn close(&self) {
        self.lock().close();
        self.ready.notify_all();
    }
}

/// 记录一条被队列淘汰的消息。
///
/// 补丁丢失会让画面短暂缺块（下一个整帧自愈），所以要能看见；整帧只有新旧之别，降为 trace。
fn log_evicted(msg: &RenderMsg) {
    match msg {
        RenderMsg::FullRgba(frame) | RenderMsg::FullNv12(frame) => {
            tracing::trace!(
                "渲染队列已满，丢弃旧整帧 seq={}（{}x{}）",
                frame.seq,
                frame.width,
                frame.height
            );
        }
        RenderMsg::Patch(patch) => {
            tracing::warn!(
                "渲染队列补丁积压，丢弃最旧补丁（{} 个矩形，目标 {}x{}）",
                patch.rects.len(),
                patch.width,
                patch.height
            );
        }
        RenderMsg::SetSurface(_) | RenderMsg::Shutdown => {
            tracing::warn!("渲染队列丢了控制消息（不应发生：控制消息不参与容量限制）");
        }
    }
}

/// 渲染线程的投递句柄（廉价克隆，可跨线程）。
#[derive(Clone)]
pub(crate) struct RenderHandle {
    /// 共享队列。
    queue: Arc<RenderQueue>,
}

impl RenderHandle {
    /// 设置渲染目标；`None` 表示清除（Surface 被销毁/退后台）。
    pub(crate) fn set_surface(&self, window: Option<NativeWindow>) {
        self.queue.push(RenderMsg::SetSurface(window));
    }

    /// 投递一帧整帧（按 `nv12` 字段选择 RGBA / NV12 路径；队列满时丢最旧帧，不阻塞）。
    pub(crate) fn push_frame(&self, frame: DecodedFrame) {
        let msg = if frame.nv12.is_some() {
            RenderMsg::FullNv12(Box::new(frame))
        } else {
            RenderMsg::FullRgba(Box::new(frame))
        };
        self.queue.push(msg);
    }

    /// 投递一批补丁（与整帧同一条 FIFO，保持引擎侧的顺序；不阻塞）。
    pub(crate) fn push_patch(&self, patch: DecodedPatch) {
        self.queue.push(RenderMsg::Patch(Box::new(patch)));
    }

    /// 请求渲染线程退出并释放 EGL/GL 资源。
    pub(crate) fn shutdown(&self) {
        self.queue.push(RenderMsg::Shutdown);
    }

    /// 关闭队列（投递端随后一律被丢弃）。
    pub(crate) fn close(&self) {
        self.queue.close();
    }
}

/// 启动渲染线程。
///
/// # Errors
///
/// 线程创建失败（资源不足）时返回 `std::io::Error`。
pub(crate) fn spawn() -> std::io::Result<(RenderHandle, JoinHandle<()>)> {
    let queue = Arc::new(RenderQueue::new());
    let thread_queue = Arc::clone(&queue);
    let join = std::thread::Builder::new()
        .name("rt-render".to_owned())
        .spawn(move || run(&thread_queue))?;
    Ok((RenderHandle { queue }, join))
}

/// 渲染线程主循环：取消息 → 处理，直到 `Shutdown` 或队列关闭。
fn run(queue: &RenderQueue) {
    let mut renderer = Renderer::new();
    tracing::info!("渲染线程已启动");
    while let Some(msg) = queue.pop() {
        match msg {
            RenderMsg::SetSurface(window) => renderer.set_surface(window),
            RenderMsg::FullRgba(frame) | RenderMsg::FullNv12(frame) => {
                renderer.draw_frame(&frame);
            }
            RenderMsg::Patch(patch) => renderer.draw_patch(&patch),
            RenderMsg::Shutdown => break,
        }
    }
    renderer.teardown();
    tracing::info!("渲染线程已退出");
}

/// 渲染线程私有的 EGL/GL 状态。
struct Renderer {
    /// EGL 会话（首次拿到 surface 时创建；靠 `Option` 表达「尚未就绪」）。
    egl: Option<egl::Egl>,
    /// GLES 管线（RGBA + NV12；context current 之后才能创建）。
    gl: Option<gl::Gl>,
    /// 当前窗口（持有 `ANativeWindow` 的引用，丢弃即释放）。
    window: Option<NativeWindow>,
    /// 上一次补丁被丢弃的原因（补丁以 30 fps 到来，同一原因只记一次日志，避免刷屏）。
    last_patch_drop: Option<&'static str>,
}

impl Renderer {
    /// 空渲染器（所有资源延迟到第一次 surface 到来时创建）。
    fn new() -> Self {
        Self {
            egl: None,
            gl: None,
            window: None,
            last_patch_drop: None,
        }
    }

    /// 换 surface：先解绑/销毁旧 EGLSurface，再为新窗口建一条。
    fn set_surface(&mut self, window: Option<NativeWindow>) {
        // 1. 无条件释放旧 surface（`eglMakeCurrent(EGL_NO_SURFACE)` → destroy）。
        self.release_surface();

        let Some(window) = window else {
            tracing::info!("渲染目标已清除");
            return;
        };

        // 2. 确保 EGL 就绪。
        if self.egl.is_none() {
            match egl::Egl::new() {
                Ok(state) => self.egl = Some(state),
                Err(err) => {
                    tracing::error!("EGL 初始化失败：{err}");
                    return;
                }
            }
        }

        // 3. 建窗口 surface 并设为当前。
        let created = match self.egl.as_mut() {
            Some(state) => state.create_window_surface(&window),
            None => return,
        };
        if let Err(err) = created {
            tracing::error!("创建 EGLSurface 失败：{err}");
            return;
        }
        self.window = Some(window);

        // 4. 确保 GL 管线就绪（必须在上下文 current 之后）。
        if self.gl.is_none() {
            if let Some(state) = self.egl.as_ref() {
                let gles3 = state.is_gles3();
                match gl::Gl::new(|name| state.proc_address(name), gles3) {
                    Ok(pipeline) => self.gl = Some(pipeline),
                    Err(err) => tracing::error!("GLES 管线初始化失败：{err}"),
                }
            }
        }

        if let Some((width, height)) = self.egl.as_ref().and_then(egl::Egl::surface_size) {
            tracing::info!("渲染目标已就绪：{width}x{height}");
        } else {
            tracing::warn!("渲染目标已创建但尺寸不可用");
        }
    }

    /// 绘制一帧整帧（没有 surface / 管线未就绪 / 帧不可用时静默跳过）。
    fn draw_frame(&mut self, frame: &DecodedFrame) {
        let (Some(egl), Some(gl)) = (self.egl.as_ref(), self.gl.as_mut()) else {
            return;
        };
        let Some((surface_width, surface_height)) = egl.surface_size() else {
            return;
        };
        let is_nv12 = frame.nv12.is_some();
        if is_nv12 {
            if !gl.upload_nv12(frame) {
                return;
            }
            // 新的整帧 = 新基底：补丁的「无基底」告警重新武装。
            self.last_patch_drop = None;
        } else {
            if !gl.upload(frame) {
                return;
            }
            // RGBA 整帧意味着会话换了 codec：旧的 NV12 基底不再可信。
            gl.invalidate_nv12();
        }
        let Some(rect) = contain_rect(frame.width, frame.height, surface_width, surface_height)
        else {
            return;
        };
        let drawn = if is_nv12 {
            gl.draw_nv12((surface_width, surface_height), rect)
        } else {
            gl.draw((surface_width, surface_height), rect)
        };
        if let Err(err) = drawn {
            tracing::debug!("绘制失败：{err}");
            return;
        }
        if let Err(err) = egl.swap_buffers() {
            // 退后台/切 Surface 时 swap 失败是正常的，降级为 debug。
            tracing::debug!("交换缓冲失败：{err}");
        }
    }

    /// 把一批补丁叠进持久 NV12 缓冲并重绘（无基底/尺寸不符时丢弃并记 warning）。
    fn draw_patch(&mut self, patch: &DecodedPatch) {
        let (Some(egl), Some(gl)) = (self.egl.as_ref(), self.gl.as_mut()) else {
            return;
        };
        let Some((surface_width, surface_height)) = egl.surface_size() else {
            return;
        };
        match gl.apply_nv12_patches(patch) {
            gl::PatchOutcome::Applied => self.last_patch_drop = None,
            // 空矩形 = 保活：缓冲未变，不需要重绘。
            gl::PatchOutcome::KeepAlive => {
                self.last_patch_drop = None;
                return;
            }
            gl::PatchOutcome::Dropped(reason) => {
                // 补丁以固定节拍到来；同一原因只记一次（否则「没有基底」会刷屏 30 行/秒）。
                if self.last_patch_drop != Some(reason) {
                    tracing::warn!(
                        "丢弃补丁（{reason}）：目标 {}x{}，{} 个矩形；等下一个整帧恢复",
                        patch.width,
                        patch.height,
                        patch.rects.len()
                    );
                    self.last_patch_drop = Some(reason);
                }
                return;
            }
        }
        let Some((width, height)) = gl.nv12_dimensions() else {
            return;
        };
        let Some(rect) = contain_rect(width, height, surface_width, surface_height) else {
            return;
        };
        if let Err(err) = gl.draw_nv12((surface_width, surface_height), rect) {
            tracing::debug!("补丁重绘失败：{err}");
            return;
        }
        if let Err(err) = egl.swap_buffers() {
            tracing::debug!("交换缓冲失败：{err}");
        }
    }

    /// 解绑并销毁当前 surface，保留 EGL/GL（旋转后新 surface 到来可快速恢复）。
    fn release_surface(&mut self) {
        if let Some(state) = self.egl.as_mut() {
            state.drop_surface();
        }
        // 丢弃窗口引用：`ANativeWindow_release`。
        self.window = None;
    }

    /// 线程退出：删除 GL 对象 → 销毁 EGL（EGL 的 `Drop` 负责 context/display）。
    fn teardown(&mut self) {
        if let (Some(egl), Some(mut pipeline)) = (self.egl.as_ref(), self.gl.take()) {
            // GL 对象删除需要一个 current 的上下文（不需要 drawable）。
            if let Err(err) = egl.make_context_only() {
                tracing::warn!("退出前解绑 surface 失败：{err}");
            }
            pipeline.destroy();
        }
        self.release_surface();
        self.egl = None;
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.teardown();
    }
}
