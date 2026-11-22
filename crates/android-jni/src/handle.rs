//! 句柄表与宿主生命周期。
//!
//! Kotlin 拿到的 `Long` 只是句柄表里的键；真正的资源都在 [`HostSlot`] 里：
//! Tokio 运行时（3 个命名 worker）、[`ControllerHost`]（控制通道 + 引擎）、渲染线程、
//! 状态镜像任务。`nativeStop` 取出句柄后按固定顺序收尾，**重复调用安全**（第二次取不到句柄）。
//!
//! 句柄故意不暴露任何内部结构：Kotlin 侧只做「存 `Long`、传回来」。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle as ThreadHandle;
use std::time::{Duration, Instant};

use rust_tunnel_controller_core::{ControllerCommands, ControllerHost, ControllerSnapshot};
use rust_tunnel_remote::RemoteUiState;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::JoinHandle as TaskHandle;

use crate::bridge::JniBridge;
use crate::render::RenderHandle;
use crate::sink::FrameInfo;
use crate::state_json::state_json;

/// 运行时收尾的等待上限（后台任务已在 `ControllerHost::stop` 里等过，这里只是兜底）。
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// 状态镜像回调的最小间隔（契约 §3：节流后每秒可多次，200ms 即 ≤5 次/秒）。
const MIRROR_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// 控制器运行时的工作线程数。
///
/// 2–4 是「控制通道 + 引擎会话 + 状态镜像」都能跑得开、又不与渲染线程抢太多核的数量。
pub(crate) const WORKER_THREADS: usize = 3;

/// 一个已启动的控制器宿主及其附属资源。
pub(crate) struct HostSlot {
    /// 命令句柄（廉价克隆，JNI 线程直接调用）。
    pub(crate) commands: ControllerCommands,
    /// 聚合快照广播（状态 JSON 用）。
    pub(crate) snapshots: watch::Receiver<ControllerSnapshot>,
    /// 引擎原始状态广播（补 `selected_display`/`recv_kbps`/`audio_available`）。
    pub(crate) remote_state: watch::Receiver<RemoteUiState>,
    /// 渲染线程投递句柄。
    pub(crate) render: RenderHandle,
    /// 回调桥。
    pub(crate) bridge: Arc<JniBridge>,
    /// 画面几何共享状态。
    pub(crate) frame_info: Arc<FrameInfo>,
    /// 最近一次 `nativeConnect` 的目标（无活动会话时用作状态 JSON 的 `target`）。
    ///
    /// 与状态镜像任务**共享同一个 `Arc`**：两处各持一份 `Mutex<String>` 会让
    /// `nativeSnapshot` 与 `onState` 给出不一致的 `target`。
    pub(crate) target: Arc<Mutex<String>>,
    /// 宿主本体（`shutdown` 时 take 走）。
    host: Mutex<Option<ControllerHost>>,
    /// Tokio 运行时（`shutdown` 时 take 走）。
    runtime: Mutex<Option<Runtime>>,
    /// 渲染线程 join 句柄。
    render_thread: Mutex<Option<ThreadHandle<()>>>,
    /// 状态镜像任务（`shutdown` 时 abort）。
    mirror_task: Mutex<Option<TaskHandle<()>>>,
}

impl HostSlot {
    /// 新建槽位（由 `nativeStart` 在宿主启动成功后调用）。
    #[expect(
        clippy::too_many_arguments,
        reason = "槽位就是把一组互不相关的共享句柄装在一起，构造点只有一处；拆参数结构体只会多一层转手"
    )]
    pub(crate) fn new(
        commands: ControllerCommands,
        snapshots: watch::Receiver<ControllerSnapshot>,
        remote_state: watch::Receiver<RemoteUiState>,
        render: RenderHandle,
        render_thread: ThreadHandle<()>,
        bridge: Arc<JniBridge>,
        frame_info: Arc<FrameInfo>,
        host: ControllerHost,
        runtime: Runtime,
        mirror_task: TaskHandle<()>,
        target: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            commands,
            snapshots,
            remote_state,
            render,
            bridge,
            frame_info,
            target,
            host: Mutex::new(Some(host)),
            runtime: Mutex::new(Some(runtime)),
            render_thread: Mutex::new(Some(render_thread)),
            mirror_task: Mutex::new(Some(mirror_task)),
        }
    }

    /// 当前目标的克隆（`nativeConnect` 写入，状态 JSON 读取）。
    pub(crate) fn target_name(&self) -> String {
        lock(&self.target).clone()
    }

    /// 记录最近一次连接请求的目标。
    pub(crate) fn set_target(&self, target: &str) {
        target.clone_into(&mut lock(&self.target));
    }

    /// 当前状态 JSON（`nativeSnapshot` 用）。
    pub(crate) fn snapshot_json(&self) -> String {
        let snap: ControllerSnapshot = (*self.snapshots.borrow()).clone();
        let remote: RemoteUiState = (*self.remote_state.borrow()).clone();
        state_json(
            &snap,
            &remote,
            self.frame_info.geometry(),
            &self.target_name(),
        )
    }

    /// 干净收尾（可重复调用；第二次起全部为空操作）。
    ///
    /// 顺序：停渲染投递/线程 → 停状态镜像 → 停宿主 → 关运行时。
    /// 渲染先停是刻意的：宿主停止过程中引擎仍可能交来最后几帧，队列已关闭后这些帧
    /// 直接被丢弃，不会再触碰即将释放的 EGL 资源。
    pub(crate) fn shutdown(&self) {
        self.render.shutdown();
        self.render.close();
        if let Some(thread) = take(&self.render_thread) {
            if thread.join().is_err() {
                tracing::warn!("渲染线程在收尾时 panic");
            }
        }

        if let Some(task) = take(&self.mirror_task) {
            task.abort();
        }

        let host = take(&self.host);
        let runtime = take(&self.runtime);
        match (host, runtime) {
            (Some(host), Some(runtime)) => {
                // `ControllerHost::stop` 是 async 且吃掉 self；在运行时上跑完再关运行时。
                runtime.block_on(async move { host.stop().await });
                runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            }
            (None, Some(runtime)) => runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT),
            _ => {}
        }
    }
}

/// 加锁（中毒时取回内层数据：单次失败不应让整张句柄表失效）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 取出 `Some` 里的值（收尾路径用）。
fn take<T>(mutex: &Mutex<Option<T>>) -> Option<T> {
    lock(mutex).take()
}

/// 全局句柄表。
fn table() -> &'static Mutex<HashMap<u64, Arc<HostSlot>>> {
    static TABLE: OnceLock<Mutex<HashMap<u64, Arc<HostSlot>>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 分配一个新句柄（从 1 开始，`0` 保留为「失败」）。
pub(crate) fn next_handle() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 登记槽位。
pub(crate) fn insert(id: u64, slot: Arc<HostSlot>) {
    lock(table()).insert(id, slot);
}

/// 按句柄查槽位（未知句柄返回 `None`）。
pub(crate) fn get(id: u64) -> Option<Arc<HostSlot>> {
    lock(table()).get(&id).cloned()
}

/// 移除并返回槽位（`nativeStop` 用；第二次调用返回 `None`）。
pub(crate) fn remove(id: u64) -> Option<Arc<HostSlot>> {
    lock(table()).remove(&id)
}

/// 状态镜像任务：把聚合快照节流成 `onState(json)` 回调。
///
/// - 只在 JSON **实际变化**时回调；
/// - 两次回调之间至少隔 [`MIRROR_MIN_INTERVAL`]，节流期间再变化则在窗口结束后补发
///   （发送前重新取最新状态，不会发过期 JSON）；
/// - 任一 `watch` 发送端消失（宿主已释放）时退出。
pub(crate) async fn mirror_loop(
    mut snapshots: watch::Receiver<ControllerSnapshot>,
    mut remote: watch::Receiver<RemoteUiState>,
    bridge: Arc<JniBridge>,
    frame_info: Arc<FrameInfo>,
    target: Arc<Mutex<String>>,
) {
    let mut last_json = String::new();
    let mut last_emit: Option<Instant> = None;

    loop {
        let current = build_json(&snapshots, &remote, &frame_info, &target);
        if current != last_json {
            if let Some(last) = last_emit {
                let elapsed = last.elapsed();
                if elapsed < MIRROR_MIN_INTERVAL {
                    tokio::time::sleep(MIRROR_MIN_INTERVAL.saturating_sub(elapsed)).await;
                }
            }
            // 等待窗口期间状态可能又变了：重新取一次，避免发过期 JSON。
            let latest = build_json(&snapshots, &remote, &frame_info, &target);
            if latest != last_json {
                bridge.state(&latest);
                last_json = latest;
                last_emit = Some(Instant::now());
            }
        }

        tokio::select! {
            result = snapshots.changed() => {
                if result.is_err() {
                    break;
                }
            }
            result = remote.changed() => {
                if result.is_err() {
                    break;
                }
            }
        }
    }
}

/// 从三份共享状态拼出状态 JSON。
fn build_json(
    snapshots: &watch::Receiver<ControllerSnapshot>,
    remote: &watch::Receiver<RemoteUiState>,
    frame_info: &FrameInfo,
    target: &Mutex<String>,
) -> String {
    let snap: ControllerSnapshot = (*snapshots.borrow()).clone();
    let remote: RemoteUiState = (*remote.borrow()).clone();
    let target = lock(target).clone();
    state_json(&snap, &remote, frame_info.geometry(), &target)
}
