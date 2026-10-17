//! Windows 服务控制管理器（SCM）交互：安装 / 卸载 / 查询 / 启停 / 服务主循环
//! （**整模块只在 Windows 上编译**，见 `lib.rs`）。
//!
//! # 这一层替谁说话
//!
//! 无人值守被控端的形态是：**一个以 LocalSystem 跑的 Windows 服务**（本模块管的那个），
//! 由它把 GUI 实例拉进控制台会话。为什么必须是服务而不是「以管理员身份运行」，
//! 见 `lib.rs` 顶部的三层原因（桌面 ACL / UIPI / 会话隔离）。
//!
//! 本模块把「装/卸/查/启/停」这一整套 SCM 样板收在一处，调用方是 `crates/win-service`
//! 那个 bin。它拿到的是**原始的 Win32 错误码**（[`crate::WinError`]），因为这几个数字
//! 本身就是结论：
//!
//! | 码 | 含义 | 在本模块里的意义 |
//! |---|---|---|
//! | 5 | `ERROR_ACCESS_DENIED` | [`install`]/[`uninstall`] **没提权**（要管理员 token） |
//! | 183 | `ERROR_ALREADY_EXISTS` | [`install`] 时服务已存在（**原样透出**，不回滚） |
//! | 1060 | `ERROR_SERVICE_DOES_NOT_EXIST` | 服务没装；[`query_state`] 把它变成 `Ok(NotInstalled)` |
//! | 1063 | `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` | [`run`] **不是被 SCM 启动的**，该走前台模式 |
//!
//! # [`run`] 的两条硬契约
//!
//! 1. **只能在 SCM 启动的进程里调，且要最先调。** 服务进程的主线程一进来就应该调它，
//!    它**阻塞到服务停止**（SCM 通过它派发控制码）。如果不是被 SCM 启动的（用户双击了
//!    那个 exe），它立刻返回 `0` 且 `GetLastError() == 1063`——本函数把这个码原样包成
//!    `Err` 返回，bin 应当据此打印「请用 `--console` 前台运行」这类提示，而**不是**把它
//!    当成崩溃。
//! 2. **[`ServiceEvent::Start`] 会在 SCM 派发线程上被调用，必须立刻返回。** 那个回调里
//!    只能「把活派到自己的工作线程」（或起一个运行时长驻的线程）——一旦在里面干活，
//!    SCM 就迟迟等不到 `RUNNING` 状态，用户看到的是「服务未能及时响应启动或控制请求」
//!    （事件日志里的 1053），而那是**本层唯一会把服务拖死的方式**。
//!
//! 同理，`on_event` **绝不能在回调里把控制流交给 SCM 之外的东西再阻塞回来**：
//! `Stop` 事件的回调若自己也去 join 那条工作线程，就会和 `service_main` 的停止标志
//! 轮询互相等待——上层要么在这里不做等待，要么把等待挪进 `Start` 起的线程里。
//!
//! # `install` 的前提（不支持就会静默写错）
//!
//! - **需要提权**：`OpenSCManagerW(SC_MANAGER_CREATE_SERVICE)` 在没有管理员 token 时
//!   返回 `ERROR_ACCESS_DENIED`(5)，本模块原样返回它。
//! - **`exe` 必须是绝对路径**：SCM 存的就是这串字符，相对路径在服务启动时按
//!   `%SystemRoot%\System32` 解析，多半报「找不到文件」。这里**不做**校验（调用方
//!   `current_exe()` 拿到的本来就是绝对路径），但会原样写入。
//! - **路径整体加双引号，但 `extra_args` 不做引号转义**：`C:\Program Files\...` 里的
//!   空格必须靠引号挡住，否则 SCM 把它当参数分隔符。而本项目的参数都是自己写死的简单值
//!   （`--server host:port` 之类），不含空格与引号，故不引入转义层。**若将来把用户输入
//!   接到 `extra_args` 上，必须补上转义**——否则那是一个命令行注入。
//!
//! # 为什么不复用 [`crate::handle::OwnedHandle`]
//!
//! 那个包装的 `Drop` 调 `CloseHandle`，而 SCM 句柄（`SC_HANDLE`）只认
//! `CloseServiceHandle`——[`ScHandle`] 是本模块自己的 RAII，这两类句柄**不能混**。
//!
//! # 可验证的部分
//!
//! 本文件在 macOS/Linux 上**连编译都不参与**，所以这里的一切只能上真机验。唯一例外是
//! [`map_state`]（状态码 → [`ServiceState`]）：它**不碰 FFI**，故被单测钉住，
//! 且 Windows 上跑 `cargo test` 时同样会执行。

use crate::error::{check_bool, last_error, WinError};
use crate::handle::require_handle;
use crate::wide::to_wide;
use crate::WinResult;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;
use windows_sys::Win32::Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, NO_ERROR};
use windows_sys::Win32::System::RemoteDesktop::WTSSESSION_NOTIFICATION;
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, RegisterServiceCtrlHandlerExW,
    SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW, SC_ACTION, SC_ACTION_RESTART,
    SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SC_STATUS_PROCESS_INFO,
    SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTINUE_PENDING,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_STOP,
    SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_PAUSED,
    SERVICE_PAUSE_PENDING, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STATUS_PROCESS,
    SERVICE_STOP, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS,
};

// ── 服务的身份（写进 SCM 的那几个字符串）────────────────────────────

/// SCM 里的**内部名**（`CreateServiceW` / `OpenServiceW` 用它，大小写不敏感）。
///
/// 一旦发布就不能改：改名等于「旧服务不会被卸载」。
pub const SERVICE_NAME: &str = "rust-tunnel-client-service";

/// 服务管理器里显示的名字。
pub const SERVICE_DISPLAY_NAME: &str = "rust-tunnel 无人值守被控端";

/// 服务描述（写进 `SERVICE_CONFIG_DESCRIPTION`，任务管理器/`services.msc` 里能看到）。
///
/// 这段文字是**运维唯一能看到的解释**：说明白它是什么、谁在用、什么时候需要它。
pub const SERVICE_DESCRIPTION: &str =
    "rust-tunnel 无人值守被控端：以 LocalSystem 运行，把被控实例拉进控制台会话，\
     使远程控制能够覆盖 Windows 自己的锁屏/登录界面。停用它等于放弃无人值守接入。";

/// `SERVICE_DELETE`（`0x0001_0000`）。
///
/// windows-sys 0.61.2 **没有**导出这个名字：`Win32::System::Services` 里有
/// `SERVICE_START`(16) / `SERVICE_STOP`(32) / `SERVICE_QUERY_STATUS`(4) / `SERVICE_ALL_ACCESS`，
/// 而 `SERVICE_DELETE` 就是通用访问权 `DELETE` 本身（windows-sys 把它放在
/// `Win32::Storage::FileSystem::DELETE` 下，值同为 `65536`），本 crate 没开那个 feature，
/// 故按 Win32 头文件的字面值自带一份。[`uninstall`] 的单测钉住它的值。
const SERVICE_DELETE: u32 = 0x0001_0000;

/// 停止标志的轮询步长。
///
/// 用「小步轮询」而不是条件变量：`on_event` 是 `fn` 指针、handler 由 SCM 调用，
/// 本 crate 只有 `tracing` 一个依赖，为这一个布尔再引入同步原语（或 std 之外的
/// 依赖）不划算；200ms 的唤醒代价在服务生命周期里可以忽略。
const STOP_POLL_STEP: Duration = Duration::from_millis(200);

// ── 状态与事件 ───────────────────────────────────────────────────

/// 服务在 SCM 眼里的状态。
///
/// [`ServiceState::NotInstalled`] **不是** `dwCurrentState` 的一个取值——它是
/// [`query_state`] 在 `OpenServiceW` 报 1060 时给出的结论，故调用方拿到的
/// `Ok(NotInstalled)` 就足以决定「GUI 该显示安装按钮还是启停按钮」，不必再看 `Err`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// 服务**不存在**（SCM 报 `ERROR_SERVICE_DOES_NOT_EXIST` = 1060）。
    NotInstalled,
    /// 已停止（`SERVICE_STOPPED`）。
    Stopped,
    /// 正在启动（`SERVICE_START_PENDING`，含 `SERVICE_CONTINUE_PENDING`）。
    StartPending,
    /// 正在运行（`SERVICE_RUNNING`）。
    Running,
    /// 正在停止（`SERVICE_STOP_PENDING`）。
    StopPending,
    /// 已暂停（`SERVICE_PAUSED`，含 `SERVICE_PAUSE_PENDING`）。
    ///
    /// 本服务的控制码里**没有** `SERVICE_ACCEPT_PAUSE_CONTINUE`，所以正常永远到不了这里；
    /// 留着它是为了让映射对**任何**服务句柄都成立（探针/诊断会去看别的服务）。
    Paused,
    /// 没建模的原始状态码（原样保留，便于诊断）。
    Unknown(u32),
}

impl ServiceState {
    /// 服务是否**存在于 SCM**（与它当前在不在跑无关）。
    #[must_use]
    pub const fn is_installed(self) -> bool {
        !matches!(self, Self::NotInstalled)
    }

    /// 服务是否**正在运行**。
    ///
    /// `StartPending` / `StopPending` **都不算**运行中：它们各自会在几百毫秒内变成
    /// `Running` / `Stopped`，把中间态算成运行中会让 UI 在启动瞬间误报「已就绪」。
    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// [`run`] 交给上层的服务事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceEvent {
    /// 服务已就绪，可以开始干活。
    ///
    /// **必须立刻返回**（见 [`run`] 的文档）：它是在 SCM 派发线程上、`RUNNING` 状态
    /// 上报**之前**被调的，在这里干活会让 SCM 判定「服务未能及时响应启动请求」。
    Start,
    /// SCM 要求停止（用户按了「停止」，或系统关机）。
    ///
    /// 收到它时 SCM 已经拿到 `STOP_PENDING`；上层应当在**自己的线程**上收尾，
    /// 然后放 [`run`] 里的轮询看到停止标志（本模块会再报 `STOPPED`）。
    Stop,
    /// 会话变化（WTS 事件）。
    ///
    /// 这是无人值守那条链的**触发器**：登录/注销/锁屏/解锁都会以它到达，
    /// 上层据此决定「把实例拉进哪个控制台会话」。
    SessionChange {
        /// WTS 事件类型（`WTS_EVENT_*`，如 `WTS_SESSION_LOGON`）。
        event_type: u32,
        /// 相关会话 id。
        session_id: u32,
    },
    /// 其它控制码（含未知码）——**只上报，不解释**，免得本层替上层猜语义。
    Other {
        /// SCM 给的控制码。
        control: u32,
    },
}

// ── 状态码映射（唯一能在本机被单测的逻辑）──────────────────────────

/// `dwCurrentState` → [`ServiceState`]。**不碰 FFI 的纯函数**。
///
/// 两个中间态按**去向**归类而不是丢进 [`ServiceState::Unknown`]：
/// `SERVICE_CONTINUE_PENDING`(5) 必然收敛到 `Running`（与启动中间态同类），
/// `SERVICE_PAUSE_PENDING`(6) 必然收敛到 `Paused`。`Unknown` 留给真正没建模的码。
#[must_use]
const fn map_state(raw: u32) -> ServiceState {
    match raw {
        SERVICE_STOPPED => ServiceState::Stopped,
        SERVICE_START_PENDING | SERVICE_CONTINUE_PENDING => ServiceState::StartPending,
        SERVICE_STOP_PENDING => ServiceState::StopPending,
        SERVICE_RUNNING => ServiceState::Running,
        SERVICE_PAUSED | SERVICE_PAUSE_PENDING => ServiceState::Paused,
        other => ServiceState::Unknown(other),
    }
}

// ── SCM 句柄的 RAII ─────────────────────────────────────────────

/// `SC_HANDLE` 的 RAII 包装：`Drop` 时 `CloseServiceHandle`。
///
/// 失败分支提前返回是这些调用点的常态，靠 `Drop` 兜住才不漏关（见模块文档里
/// 「为什么不复用 `OwnedHandle`」）。
#[derive(Debug)]
struct ScHandle(SC_HANDLE);

impl ScHandle {
    /// 接管一个 SCM 句柄。**所有权转移**：不要再对它调 `CloseServiceHandle`。
    ///
    /// `NULL` 视为空句柄，`Drop` 时不去关（与 [`crate::handle::OwnedHandle`] 同约定）。
    const fn new(handle: SC_HANDLE) -> Self {
        Self(handle)
    }

    /// 裸句柄，用于传给 Win32。
    ///
    /// 取 `&self` 而不是 `self`：本类型带 `Drop`，按值取会把句柄从 RAII 里挪出去
    /// （`const fn` 更直接地拒绝带析构的值按值消耗）。
    const fn raw(&self) -> SC_HANDLE {
        self.0
    }
}

impl Drop for ScHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 句柄由本对象独占（见 new 的契约），本对象之外没有关过它；
            // CloseServiceHandle 只读这个句柄值，关失败也只影响返回值。
            unsafe {
                CloseServiceHandle(self.0);
            }
        }
    }
}

/// `OpenSCManagerW`：打开本机 SCM 数据库，申请 `access` 权限。
///
/// # Errors
///
/// 没提权（`ERROR_ACCESS_DENIED` = 5）、或 SCM 不可用（服务被禁用/系统正在关机）时
/// 返回 [`WinError`]，**原始错误码保留**。
fn open_scm(access: u32) -> WinResult<ScHandle> {
    // 机器名/数据库名传 null 表示「本机 + 默认数据库」；无输出参数可悬垂。
    // SAFETY: 两个 PCWSTR 参数为 null 是文档允许的（本机、默认库），无别名问题。
    let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), access) };
    require_handle("OpenSCManagerW", scm).map(ScHandle::new)
}

/// `OpenServiceW`：打开本服务。
///
/// 服务不存在时返回 `Ok(None)`——**这不是失败**，调用方要把它与真失败区分开
/// （[`query_state`] 把它变成 `Ok(NotInstalled)`，[`uninstall`] 把它当幂等成功）。
///
/// # Errors
///
/// 服务存在但权限不够（`access` 要得太多）时返回 [`WinError`]。
fn open_service(scm: SC_HANDLE, access: u32) -> WinResult<Option<ScHandle>> {
    // 缓冲先绑局部变量：OpenServiceW 在调用期间读它，写成临时值就是悬垂。
    let name = to_wide(SERVICE_NAME);
    // SAFETY: scm 是仍存活的 SCM 句柄；name 是本栈帧上 NUL 结尾的 UTF-16 缓冲。
    let service = unsafe { OpenServiceW(scm, name.as_ptr(), access) };
    if service.is_null() {
        // last_error 必须紧跟失败的调用读，中间不能插入别的 Win32 调用。
        let err = last_error("OpenServiceW");
        if err.code() == ERROR_SERVICE_DOES_NOT_EXIST {
            return Ok(None);
        }
        return Err(err);
    }
    Ok(Some(ScHandle::new(service)))
}

/// [`open_service`] 的「服务必须存在」版本，给 [`start`]/[`stop`] 用。
///
/// # Errors
///
/// 服务不存在（错误码原样是 1060）或权限不足时返回 [`WinError`]。
fn open_service_required(scm: SC_HANDLE, access: u32) -> WinResult<ScHandle> {
    open_service(scm, access)?
        .ok_or_else(|| WinError::new("OpenServiceW", ERROR_SERVICE_DOES_NOT_EXIST))
}

/// 拼 `CreateServiceW` 的命令行：`"<exe>" <arg1> <arg2> ...`。
///
/// 引号只包 `exe`（`C:\Program Files\...` 的空格必须挡住），`extra_args` **原样**按空格
/// 拼接、不做转义——见模块文档的说明与限制。
fn build_bin_path(exe: &std::path::Path, extra_args: &[String]) -> String {
    let mut path = String::with_capacity(exe.as_os_str().len() + 2);
    path.push('"');
    path.push_str(&exe.to_string_lossy());
    path.push('"');
    for arg in extra_args {
        path.push(' ');
        path.push_str(arg);
    }
    path
}

/// 写入服务描述（`SERVICE_CONFIG_DESCRIPTION`）。
///
/// **失败只告警**：描述纯属锦上添花，写不上不该让「安装」整体失败。
fn write_description(service: SC_HANDLE, description: &mut [u16]) {
    let info = SERVICE_DESCRIPTIONW {
        lpDescription: description.as_mut_ptr(),
    };
    // SAFETY: service 是刚创建/打开且仍未关闭的服务句柄；info 是本栈帧上的结构体，
    // 它里面的 lpDescription 指向调用方持有的缓冲——那个缓冲在本调用期间存活
    // （函数形参的 &mut [u16] 保证），且 SCM 只读它（字段是 PWSTR 只是 C 声明的形状）。
    let ok = unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DESCRIPTION,
            std::ptr::addr_of!(info).cast(),
        )
    };
    if ok == 0 {
        // 错误码要紧接着失败读——宏的参数求值里它是最先算的。
        tracing::warn!(
            code = last_error("ChangeServiceConfig2W(SERVICE_CONFIG_DESCRIPTION)").code(),
            "写入服务描述失败（不影响服务本身，服务管理器里少一行说明而已）"
        );
    }
}

/// 写入失败动作（`SERVICE_CONFIG_FAILURE_ACTIONS`）：崩溃后由 SCM 自动重启。
///
/// 三条动作 `5s → 15s → 60s`，`dwResetPeriod = 86400`（24 小时无失败就把计数归零）。
/// 为什么要它：无人值守的被控端一旦进程崩了，**没有人**能去手动点「启动」，
/// SCM 的重启动作是最后一道自愈。
///
/// **失败只告警**：同上，写不上不该让安装整体失败。
fn write_failure_actions(service: SC_HANDLE) {
    // 必须绑成局部变量：SCM 在本调用期间读这个数组。
    let mut actions = [
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 5_000,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 15_000,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 60_000,
        },
    ];
    let info = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86_400,
        // 这两个是「重启前弹给用户看的消息 / 跑的命令」：本服务都在后台，
        // 无人值守场景下也没人看，故留空。
        lpRebootMsg: std::ptr::null_mut(),
        lpCommand: std::ptr::null_mut(),
        cActions: u32::try_from(actions.len()).unwrap_or(0),
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: service 是刚创建且仍未关闭的服务句柄；info 是本栈帧上的结构体，
    // lpsaActions 指向同栈帧的 actions（寿命覆盖本调用）；SCM 会**复制**这份数组，
    // 调用返回后不再引用它。
    let ok = unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            std::ptr::addr_of!(info).cast(),
        )
    };
    if ok == 0 {
        tracing::warn!(
            code = last_error("ChangeServiceConfig2W(SERVICE_CONFIG_FAILURE_ACTIONS)").code(),
            "写入崩溃重启策略失败（服务仍可用，只是崩了不会自动起回来）"
        );
    }
}

// ── 公开操作：安装 / 卸载 / 查询 / 启停 ────────────────────────────

/// 安装服务（自动启动），并写入描述与崩溃重启策略。
///
/// `binPath` 是 `"<exe>" <extra_args...>`——**引号与转义的前提见模块文档**。
///
/// # Errors
///
/// - `OpenSCManagerW` 失败：**没提权时是 `ERROR_ACCESS_DENIED`(5)**（要管理员 token）。
/// - `CreateServiceW` 失败：**已安装时原样返回 `ERROR_ALREADY_EXISTS`(183)**——
///   调用方要靠这个码分辨「已安装」，所以这里既不吞掉、也不做「先删再建」。
/// - 描述与失败动作的写入失败**不算错误**（只 `tracing::warn!`）。
pub fn install(exe: &std::path::Path, extra_args: &[String]) -> WinResult<()> {
    let bin_path = build_bin_path(exe, extra_args);
    // 每个进 SCM 的字符串缓冲都先绑局部变量再取指针：把 `to_wide(..).as_ptr()` 直接
    // 写进调用表达式，那个临时 Vec 会在语句结束（也就是调用返回后）被释放，
    // 而 SCM 是**在调用期间**读它的——这是本文件最容易写错的悬垂点。
    let name = to_wide(SERVICE_NAME);
    let display = to_wide(SERVICE_DISPLAY_NAME);
    let binary = to_wide(&bin_path);
    let mut description = to_wide(SERVICE_DESCRIPTION);

    let scm = open_scm(SC_MANAGER_CREATE_SERVICE)?;
    // SAFETY: scm 仍存活；四个 PCWSTR 都指向本栈帧上存活的 NUL 结尾缓冲；
    // 其余可选参数传 null（不依赖别的服务、不指定 LoadOrderGroup/账号密码，
    // 即以 LocalSystem 跑）。dwdesiredaccess 用 SERVICE_ALL_ACCESS 是因为
    // 紧接着还要用同一个句柄改配置。
    let service = unsafe {
        CreateServiceW(
            scm.raw(),
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            binary.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    let service = require_handle("CreateServiceW", service)?;
    let service = ScHandle::new(service);

    // 这两步都是锦上添花：失败不使整体失败。
    write_description(service.raw(), description.as_mut_slice());
    write_failure_actions(service.raw());

    tracing::info!(service = SERVICE_NAME, "服务已安装并设为自动启动");
    Ok(())
}

/// 卸载服务：先尽力停，再删。**幂等**——服务不存在时也算成功。
///
/// # Errors
///
/// - 没提权（`ERROR_ACCESS_DENIED` = 5）时返回 [`WinError`]。
/// - `DeleteService` 因其它原因失败时返回它（错误码原样）。
///
/// 停止那一步失败**不算错误**：没在跑时 `ControlService` 会报
/// `ERROR_SERVICE_NOT_ACTIVE`(1062)，那是常见且无害的。
pub fn uninstall() -> WinResult<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    // 服务不存在（1060）→ 幂等成功。
    let Some(service) = open_service(scm.raw(), SERVICE_STOP | SERVICE_DELETE)? else {
        return Ok(());
    };

    let mut status = SERVICE_STATUS::default();
    // SAFETY: service 仍存活；SERVICE_CONTROL_STOP 不读 lpservicestatus 的内容，
    // 只往里写（本栈帧上的 SERVICE_STATUS 可写、大小正确）。
    let stopped = unsafe { ControlService(service.raw(), SERVICE_CONTROL_STOP, &raw mut status) };
    if stopped == 0 {
        // 紧接着读错误码（中间没有别的 Win32 调用）。
        tracing::debug!(
            code = last_error("ControlService(SERVICE_CONTROL_STOP)").code(),
            "停止服务失败，多半是它本来就没在跑；继续卸载"
        );
    }

    // SAFETY: service 仍存活且带 DELETE 权限（打开时的 access 位）。
    let deleted = unsafe { DeleteService(service.raw()) };
    if deleted == 0 {
        let err = last_error("DeleteService");
        // 另一个「已经是想要的结果」：期间被别人删掉了。仍然幂等返回成功。
        if err.code() == ERROR_SERVICE_DOES_NOT_EXIST {
            return Ok(());
        }
        return Err(err);
    }
    tracing::info!(service = SERVICE_NAME, "服务已卸载");
    Ok(())
}

/// 查询服务状态。
///
/// 服务不存在时返回 **`Ok(ServiceState::NotInstalled)`**（不是 `Err`）：调用方
/// （GUI / 安装器）正是靠它决定「该显示安装按钮还是启停按钮」，把它变成错误
/// 只会逼每个调用点去解错误码。
///
/// # Errors
///
/// 没提权、或 `QueryServiceStatusEx` 本身失败时返回 [`WinError`]。
pub fn query_state() -> WinResult<ServiceState> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let Some(service) = open_service(scm.raw(), SERVICE_QUERY_STATUS)? else {
        return Ok(ServiceState::NotInstalled);
    };

    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed: u32 = 0;
    // SAFETY: service 仍存活；lpbuffer 指向本栈帧上的 SERVICE_STATUS_PROCESS，
    // 长度如实告知；needed 同样是本栈帧上的可写 u32。类型要从
    // `*mut SERVICE_STATUS_PROCESS` 转成 API 要求的 `*mut u8`。
    let ok = unsafe {
        QueryServiceStatusEx(
            service.raw(),
            SC_STATUS_PROCESS_INFO,
            std::ptr::addr_of_mut!(status).cast::<u8>(),
            u32::try_from(std::mem::size_of::<SERVICE_STATUS_PROCESS>()).unwrap_or(0),
            &raw mut needed,
        )
    };
    check_bool("QueryServiceStatusEx", ok)?;
    Ok(map_state(status.dwCurrentState))
}

/// 启动服务（`StartServiceW`，不带参数）。
///
/// # Errors
///
/// 服务不存在（1060）、权限不足，或启动被 SCM 拒绝（如已禁用 1058）时返回 [`WinError`]。
/// 注意**本函数不等它跑起来**：要确认结果请轮询 [`query_state`]。
pub fn start() -> WinResult<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let service = open_service_required(scm.raw(), SERVICE_START)?;
    // SAFETY: service 仍存活且带 SERVICE_START 权限；dwnumserviceargs = 0
    // 表示不给服务传参数，故参数数组可以为 null。
    let ok = unsafe { StartServiceW(service.raw(), 0, std::ptr::null()) };
    check_bool("StartServiceW", ok)
}

/// 停止服务（`ControlService(SERVICE_CONTROL_STOP)`）。
///
/// # Errors
///
/// 服务不存在（1060）、没在跑（1062 `ERROR_SERVICE_NOT_ACTIVE`）或权限不足时返回
/// [`WinError`]。同样**不等它停下来**。
pub fn stop() -> WinResult<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let service = open_service_required(scm.raw(), SERVICE_STOP)?;
    let mut status = SERVICE_STATUS::default();
    // SAFETY: service 仍存活且带 SERVICE_STOP 权限；status 是本栈帧上的可写结构体。
    let ok = unsafe { ControlService(service.raw(), SERVICE_CONTROL_STOP, &raw mut status) };
    check_bool("ControlService", ok)
}

// ── 服务主循环（`run`）──────────────────────────────────────────

/// 服务名（NUL 结尾 UTF-16），由 [`run`] **写一次**。
///
/// 为什么放静态而不是用 `run` 的栈上缓冲：`SERVICE_TABLE_ENTRYW.lpServiceName` 的指针
/// 会被 `StartServiceCtrlDispatcherW` 在**整段服务生命周期**（它阻塞到停止）里用，
/// 而 `service_main` 还需要同一份名字去 `RegisterServiceCtrlHandlerExW`。
/// `Box<[u16]>` 在堆上、且 `OnceLock` 永不释放，故它的 `as_ptr()` 在进程内一直有效。
static SERVICE_NAME_WIDE: OnceLock<Box<[u16]>> = OnceLock::new();

/// `run` 注册的事件回调。
///
/// 必须是静态槽：service main 与 handler 只能是**不捕获环境的
/// `extern "system" fn`**，没有地方挂闭包环境。锁只在这两处极短地持有
/// （取一个函数指针出来再调用），不会自锁。
static EVENT_SINK: Mutex<Option<fn(ServiceEvent)>> = Mutex::new(None);

/// SCM 要求停止的标志。`service_main` 小步轮询它。
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// `RegisterServiceCtrlHandlerExW` 返回的状态句柄。
///
/// 走静态槽而不是 `lpcontext`：那个参数只能在注册**时**给，而句柄是注册**的返回值**，
/// 顺序上给不了。用 `AtomicPtr` 存裸指针，避免「指针 ↔ 整数」的 `as` 转换。
static STATUS_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// 最近一次上报给 SCM 的状态。
///
/// `SERVICE_CONTROL_INTERROGATE` 要**原样重报**这一份：SCM 靠它判断服务是否还活着，
/// 重新构造一份（比如硬写 `RUNNING`）会在服务其实正在停时将管理器显示成错的。
/// `SERVICE_STATUS` 全是整数字段，故 `Mutex::new(字面量)` 是 const，能做 `static`。
static CURRENT_STATUS: Mutex<SERVICE_STATUS> = Mutex::new(SERVICE_STATUS {
    dwServiceType: SERVICE_WIN32_OWN_PROCESS,
    dwCurrentState: SERVICE_STOPPED,
    dwControlsAccepted: 0,
    dwWin32ExitCode: 0,
    dwServiceSpecificExitCode: 0,
    dwCheckPoint: 0,
    dwWaitHint: 0,
});

/// 取 [`EVENT_SINK`] 的锁，**中毒也继续用**。
///
/// 中毒只意味着上一次持锁时 panic 过；这里存的是纯函数指针，没有需要回滚的不变量，
/// 而 `unwrap()` 会让 handler 自己 panic——那正是本模块最不能发生的事。
fn lock_sink() -> MutexGuard<'static, Option<fn(ServiceEvent)>> {
    EVENT_SINK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 取 [`CURRENT_STATUS`] 的锁，理由同 [`lock_sink`]。
fn lock_status() -> MutexGuard<'static, SERVICE_STATUS> {
    CURRENT_STATUS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// 把事件交给 [`run`] 注册的回调。
///
/// 回调是在 `extern "system"` 边界内被调的，故它**不得 panic**（见 [`run`]）。
fn notify(event: ServiceEvent) {
    let handler = *lock_sink();
    if let Some(handler) = handler {
        handler(event);
    }
}

/// 把 `status` 交给 SCM（句柄为空时为无操作）。
fn report(handle: SERVICE_STATUS_HANDLE, status: &SERVICE_STATUS) {
    if handle.is_null() {
        return;
    }
    // SAFETY: handle 由 RegisterServiceCtrlHandlerExW 给出（见 STATUS_HANDLE 的说明）；
    // status 指向调用期间存活的 SERVICE_STATUS，SetServiceStatus 只读它。
    unsafe {
        SetServiceStatus(handle, status);
    }
}

/// 更新 [`CURRENT_STATUS`] 并上报。
fn publish(state: u32, controls: u32, checkpoint: u32, wait_hint: u32) {
    let handle = STATUS_HANDLE.load(Ordering::SeqCst);
    if handle.is_null() {
        return;
    }
    let mut status = lock_status();
    status.dwServiceType = SERVICE_WIN32_OWN_PROCESS;
    status.dwCurrentState = state;
    status.dwControlsAccepted = controls;
    status.dwWin32ExitCode = 0;
    status.dwServiceSpecificExitCode = 0;
    status.dwCheckPoint = checkpoint;
    status.dwWaitHint = wait_hint;
    // 先写进 CURRENT_STATUS 再上报：INTERROGATE 与它看到的必须是同一份。
    report(handle, &status);
}

/// 重新上报 [`CURRENT_STATUS`]（`SERVICE_CONTROL_INTERROGATE`）。
fn republish() {
    let handle = STATUS_HANDLE.load(Ordering::SeqCst);
    if handle.is_null() {
        return;
    }
    let status = lock_status();
    report(handle, &status);
}

/// 服务主函数（由 SCM 在派发线程上调用）。
///
/// 顺序是硬要求：`START_PENDING` → `on_event(Start)` → `RUNNING`。
/// `on_event(Start)` 夹在中间，是为了让上层**在 SCM 认为服务已就绪之前**就拿到活干的
/// 机会（否则「已就绪」和「开始干活」之间有一段空窗），但代价是它**必须立刻返回**。
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let Some(name) = SERVICE_NAME_WIDE.get() else {
        // 走不到这里（run 刚 set 过），但也不能 panic：拿不到名字就注册不了 handler，
        // SCM 会自己按「服务未能启动」处理。
        tracing::error!("服务名未初始化，无法注册控制处理器");
        return;
    };
    // SAFETY: name 是 OnceLock 里 NUL 结尾、进程生命周期内存活的 UTF-16 缓冲；
    // lpcontext 传 null（本实现不用它，状态句柄走 STATUS_HANDLE）。
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(ctrl_handler), std::ptr::null())
    };
    if handle.is_null() {
        // 注册失败就不再上报任何状态：SCM 会据此判定服务未能启动。
        tracing::error!(
            code = last_error("RegisterServiceCtrlHandlerExW").code(),
            "注册服务控制处理器失败，服务将无法启动"
        );
        return;
    }
    STATUS_HANDLE.store(handle, Ordering::SeqCst);

    // dwControlsAccepted 在启动期间必须是 0：还没就绪就不该接受停止/会话事件。
    publish(SERVICE_START_PENDING, 0, 1, 3_000);
    notify(ServiceEvent::Start);
    publish(
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SESSIONCHANGE,
        0,
        0,
    );

    // 阻塞到 SCM 要求停止。小步轮询的理由见 STOP_POLL_STEP。
    while !STOP_REQUESTED.load(Ordering::SeqCst) {
        std::thread::sleep(STOP_POLL_STEP);
    }
    publish(SERVICE_STOPPED, 0, 0, 0);
}

/// 控制请求处理器（`LPHANDLER_FUNCTION_EX`）。
///
/// **绝不能 panic**：它由 SCM 直接调用，panic 跨 `extern "system"` 边界是未定义行为。
/// 所以这里没有 `unwrap`、没有索引、没有 `assert`——每个可能出错的点都退化成
/// 「少做一件事」而不是「炸掉」。
unsafe extern "system" fn ctrl_handler(
    control: u32,
    event_type: u32,
    event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP => {
            // 顺序：先置标志（service_main 会看到并退出轮询）→ 再报 STOP_PENDING →
            // 最后通知上层。反过来会让上层在状态还写着 RUNNING 时就开始收尾。
            STOP_REQUESTED.store(true, Ordering::SeqCst);
            publish(SERVICE_STOP_PENDING, 0, 0, 5_000);
            notify(ServiceEvent::Stop);
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            let session_id = if event_data.is_null() {
                0
            } else {
                // SAFETY: SCM 对 SERVICE_CONTROL_SESSIONCHANGE 保证 event_data 指向一个
                // WTSSESSION_NOTIFICATION（cbSize/dwSessionId 就在头部），且在本 handler
                // 返回前一直有效；上面已判空，故解引用是安全的。
                unsafe { (*event_data.cast::<WTSSESSION_NOTIFICATION>()).dwSessionId }
            };
            notify(ServiceEvent::SessionChange {
                event_type,
                session_id,
            });
        }
        SERVICE_CONTROL_INTERROGATE => {
            // 原样重报当前状态。**不通知上层**：这不是业务事件，只是 SCM 在探活。
            republish();
        }
        other => notify(ServiceEvent::Other { control: other }),
    }
    NO_ERROR
}

/// 服务主入口：把当前线程接到 SCM 上，然后**阻塞到服务停止**。
///
/// # 只能在这里调一次，而且要最先调
///
/// 服务进程的 `main` 一进来就应该调它（除了日志初始化这类不碰 SCM 的事）。
/// 它内部走 `StartServiceCtrlDispatcherW`，那条调用要占据**主线程**。
///
/// # `on_event` 的契约
///
/// - 它在 **SCM 派发线程**上被调用，`Start`/`SessionChange`/`Other` 都在那条线程上；
/// - [`ServiceEvent::Start`] **必须立刻返回**（把活派到自己的工作线程），否则 SCM
///   等不到 `RUNNING` 而报「服务未能及时响应启动或控制请求」（事件日志 1053）；
/// - 它**不得 panic**：它是在 `extern "system"` 边界内被调的，panic 是未定义行为，
///   还会把整个服务进程带走。
///
/// # 不是被 SCM 启动时
///
/// `StartServiceCtrlDispatcherW` 立刻失败，错误码是
/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`(1063)。本函数**原样返回这个 `Err`**，
/// 让 bin 能据此打印「请用 `--console` 前台运行」之类的提示——**这不是崩溃**。
///
/// # Errors
///
/// 上述 1063；或 SCM 连接的其它失败（服务进程被 SCM 以外的东西启动、
/// 名字与 SCM 里的登记对不上等）。
pub fn run(name: &str, on_event: fn(ServiceEvent)) -> WinResult<()> {
    // 写一次。同一进程内二次调用只保留第一次的名字（不报错）：名字在整个
    // 服务生命周期里必须稳定，改它等于让 handler 注册到别的名字上。
    let _ = SERVICE_NAME_WIDE.set(to_wide(name).into_boxed_slice());
    *lock_sink() = Some(on_event);
    STOP_REQUESTED.store(false, Ordering::SeqCst);

    let Some(name_buf) = SERVICE_NAME_WIDE.get() else {
        // 刚 set 过，理论上到不了；不 panic，返回一个能看出问题的错误。
        return Err(WinError::new("StartServiceCtrlDispatcherW", 0));
    };
    let table = [
        SERVICE_TABLE_ENTRYW {
            // 字段类型是 PWSTR（mut），但 SCM 只**读**这个名字串；缓冲来自 OnceLock，
            // 进程生命周期内存活，故它在整条（阻塞的）调用期间都有效。
            lpServiceName: name_buf.as_ptr().cast_mut(),
            lpServiceProc: Some(service_main),
        },
        // Win32 要求的结束标记：名字为空、过程函数为空。
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: table 是本栈帧上的数组，以一条全空表目结尾（Win32 约定的结束标记）；
    // 表目里的名字缓冲见上（进程生命周期内存活），过程函数是不捕获环境、
    // 不 panic 的 extern "system" fn。本调用**阻塞到服务停止**，期间 table 一直存活。
    let ok = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    if ok == 0 {
        // 常见且**预期**的失败：1063 = 不是被 SCM 启动的进程（见文档）。
        return Err(last_error("StartServiceCtrlDispatcherW"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_mapping_covers_every_modelled_code() {
        assert_eq!(map_state(SERVICE_STOPPED), ServiceState::Stopped);
        assert_eq!(map_state(SERVICE_START_PENDING), ServiceState::StartPending);
        assert_eq!(map_state(SERVICE_STOP_PENDING), ServiceState::StopPending);
        assert_eq!(map_state(SERVICE_RUNNING), ServiceState::Running);
        assert_eq!(map_state(SERVICE_PAUSED), ServiceState::Paused);
    }

    #[test]
    fn pending_middle_states_are_classified_by_where_they_land() {
        // 这两个码在 ServiceState 里没有对应变体，按去向归类（见 map_state 的文档）。
        assert_eq!(
            map_state(SERVICE_CONTINUE_PENDING),
            ServiceState::StartPending
        );
        assert_eq!(map_state(SERVICE_PAUSE_PENDING), ServiceState::Paused);
    }

    #[test]
    fn unmodelled_codes_keep_the_raw_value() {
        assert_eq!(map_state(0), ServiceState::Unknown(0));
        assert_eq!(map_state(99), ServiceState::Unknown(99));
        assert_eq!(map_state(u32::MAX), ServiceState::Unknown(u32::MAX));
        // 0 不是「已停止」：SERVICE_STOPPED 是 1，0 是没建模的码。
        assert_ne!(map_state(0), ServiceState::Stopped);
    }

    #[test]
    fn only_not_installed_is_not_installed() {
        assert!(!ServiceState::NotInstalled.is_installed());
        for state in [
            ServiceState::Stopped,
            ServiceState::StartPending,
            ServiceState::Running,
            ServiceState::StopPending,
            ServiceState::Paused,
            ServiceState::Unknown(42),
        ] {
            assert!(state.is_installed(), "{state:?} 应当算「已安装」");
        }
    }

    #[test]
    fn only_running_counts_as_running() {
        assert!(ServiceState::Running.is_running());
        // 中间态必须**不算**运行中：否则 UI 会在启动/停止的那一瞬间误报「已就绪」。
        for state in [
            ServiceState::NotInstalled,
            ServiceState::Stopped,
            ServiceState::StartPending,
            ServiceState::StopPending,
            ServiceState::Paused,
            ServiceState::Unknown(42),
        ] {
            assert!(!state.is_running(), "{state:?} 不该算「运行中」");
        }
    }

    /// 机械锁：状态码的数值是 Win32 的 ABI（SCM 传回来的就是这些数），
    /// 一旦上游（windows-sys）或我们把它们改了，这里立刻炸。
    #[test]
    fn state_codes_match_win32_abi() {
        assert_eq!(SERVICE_STOPPED, 1);
        assert_eq!(SERVICE_START_PENDING, 2);
        assert_eq!(SERVICE_STOP_PENDING, 3);
        assert_eq!(SERVICE_RUNNING, 4);
        assert_eq!(SERVICE_CONTINUE_PENDING, 5);
        assert_eq!(SERVICE_PAUSE_PENDING, 6);
        assert_eq!(SERVICE_PAUSED, 7);
    }

    /// 机械锁：`SERVICE_DELETE` 是手抄的（windows-sys 0.61.2 没导出这个名字，
    /// 见常量上的注释），故把字面值钉住。
    #[test]
    fn service_delete_is_the_generic_delete_access_right() {
        assert_eq!(SERVICE_DELETE, 0x0001_0000);
        assert_eq!(SERVICE_DELETE, 65_536);
        // 打开服务时的权限位是或运算，DELETE 不能与它们撞上。
        assert_eq!(SERVICE_STOP & SERVICE_DELETE, 0);
        assert_eq!(SERVICE_QUERY_STATUS & SERVICE_DELETE, 0);
    }

    /// 机械锁：`install` 的引号前提——路径整体被引号包住，参数原样拼接。
    #[test]
    fn bin_path_quotes_the_exe_and_appends_args_verbatim() {
        let exe =
            std::path::Path::new(r"C:\Program Files\rust-tunnel\rust-tunnel-client-service.exe");
        assert_eq!(
            build_bin_path(exe, &[]),
            r#""C:\Program Files\rust-tunnel\rust-tunnel-client-service.exe""#
        );
        let args = vec!["--server".to_owned(), "example.com:3000".to_owned()];
        assert_eq!(
            build_bin_path(exe, &args),
            r#""C:\Program Files\rust-tunnel\rust-tunnel-client-service.exe" --server example.com:3000"#
        );
    }

    #[test]
    fn service_identity_strings_are_present() {
        assert!(!SERVICE_NAME.is_empty());
        assert!(!SERVICE_DISPLAY_NAME.is_empty());
        assert!(!SERVICE_DESCRIPTION.is_empty());
        // 内部名不该带空格：它是 `OpenServiceW` 的键，也是人们写脚本时敲的东西。
        assert!(!SERVICE_NAME.contains(' '), "{SERVICE_NAME}");
    }
}
