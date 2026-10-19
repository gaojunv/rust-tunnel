//! Windows 上的主流程（**本文件是整个 bin 里唯一 Windows 专属的部分**）。
//!
//! 为什么要把 Windows 代码集中在一个文件：`main.rs` 在 macOS/Linux 上也要能编译
//! （workspace 的 `cargo build` 会构建全部成员），所以平台分支只留一个 `#[cfg]`
//! 出口，其余全是普通 Rust。
//!
//! # 三个身份
//!
//! | 身份 | 启动者 | 命令 | 它能做什么 |
//! |---|---|---|---|
//! | 控制台工具 | 人（要管理员） | `--install` / `--uninstall` / `--probe` / `--probe-in-service` | 动 SCM、复制 exe、跑对照探针 |
//! | 服务进程 | SCM（SYSTEM / 会话 0） | `--run [--probe]` | 拿令牌、把子实例拉进控制台会话、监督它 |
//! | 子实例 | 服务（`CreateProcessAsUser`） | `--probe-instance`（探针）或 `--instance`（本体） | 在**目标桌面**上采集与注入 |
//!
//! 阶段 1 起，`--run`（不带 `--probe`）是**真正的服务本体**：它监督的是
//! `rust-tunnel-client-gui.exe --instance`（承载客户端身份的那个进程）。
//! 阶段 0 的探针链路（`--probe*`）原样保留，两条路共用同一份「取令牌 → 拉起 → 等」的实现。
//!
//! # 监督循环的形状
//!
//! ```text
//! Start ──派发到自己的线程──> 等控制台会话（最多 60s）
//!                               └─> 取实例令牌（B: winlogon / A: 自持）
//!                                     └─> 单实例锁 → CreateProcessAsUserW → 作业对象
//!                                           └─> 等它退出 / 等会话变化 / 等 Stop
//!                                                 ├─ 退出   → 退避后重建
//!                                                 ├─ 会话变化 → 立刻重建
//!                                                 └─ Stop   → 杀掉实例并收尾
//! ```
//!
//! `ServiceEvent::Start` **必须立刻返回**（它在 SCM 派发线程上被调），所以整个循环跑在
//! 自己的线程里；`SessionChange` / `Stop` 只往共享状态里写一个标志，由监督线程在
//! 轮询切片边界上取走。

use crate::args::{self, Command};
use crate::supervise::{self, SessionChangeAction};
use rust_tunnel_win_platform::instance_lock::{InstanceGuard, LockOutcome};
use rust_tunnel_win_platform::paths;
use rust_tunnel_win_platform::probe::{self, ProbeOptions, ProbeOutcome};
use rust_tunnel_win_platform::process::{self, SpawnedProcess};
use rust_tunnel_win_platform::security;
use rust_tunnel_win_platform::service::{self, ServiceEvent, ServiceState};
use rust_tunnel_win_platform::WinError;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`：`StartServiceCtrlDispatcherW` 在
/// 「不是被 SCM 启动」时给的就是它。用它把「手工跑了 `--run`」这条常见错误
/// 翻译成一句人话（码本身是诊断信息，别丢）。
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: u32 = 1063;

/// `ERROR_SERVICE_MARKED_FOR_DELETE`：上一个服务还在删除中。
///
/// `DeleteService` 只是**标记**删除，服务对象要等所有句柄关闭才真的消失；紧接着的
/// `CreateServiceW` 可能撞上它。重试几次就没事（见 [`install_with_retry`]）。
const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;

/// `ERROR_SERVICE_ALREADY_RUNNING`：服务已经在跑（`StartServiceW` 在已运行时给的就是它）。
///
/// `--start` 是**幂等**的：这个码不是错误，是「无需启动」，被 [`start_service`] 当成成功。
const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;

/// `--run` 是否跑探针。由 [`run_probe_service`] / [`run_instance_service`] 在
/// `service::run` **之前**设定，由 [`on_service_event`] 读——事件回调只能是 `fn` 指针
/// （没有地方挂捕获环境）。
static PROBE_MODE: AtomicBool = AtomicBool::new(false);

/// 服务注册时写进 `ImagePath` 的固定参数。
///
/// **必须是编译期常量**：未转义的用户输入进 `ImagePath` 就是往 SYSTEM 服务的命令行里
/// 注入（`service::install` 的文档写明了这一点）。这里只有 `--run` 一个字面量，
/// 不接受任何来自命令行/配置文件的值。
const SERVICE_RUN_ARGS: [&str; 1] = ["--run"];

/// 等 `--probe-in-service` 那个服务自己停下来的上限。
///
/// 探针的实际耗时是「采一帧 + 扫 8 步 × 60ms + 等子实例（上限 30s）」，40 秒都算紧的；
/// 给 180 秒是为了让**慢机器/慢磁盘**不至于被误判成卡死。真卡住时用户直接 Ctrl+C，
/// 服务还留着，用 `--uninstall` 收。
const SERVICE_PROBE_TIMEOUT: Duration = Duration::from_secs(180);
/// 轮询服务状态的步长。
const SERVICE_POLL_STEP: Duration = Duration::from_millis(500);
/// `--start` 等第一次看到 `Running` 的轮询步长。
const START_SETTLE_POLL: Duration = Duration::from_millis(250);
/// `--start` 等第一次看到 `Running` 的上限；超时按失败处理（服务始终没进「运行中」）。
const START_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
/// `--start` 看到 `Running` 之后的「稳住」复查等待。
///
/// 拒跑闸门（见 [`refuse_reason`]）让服务在 `Running` 后**几毫秒**就自停；若 `--start`
/// 一看到 `Running` 就宣告成功，用户会看到「启动成功」紧跟着状态又变回「已停止」——一句假话。
/// 1.5 秒足够跨过那个窗口，又不至于让一次正常启动多等太久。
const START_SETTLE_HOLD: Duration = Duration::from_millis(1_500);
/// 请 SCM 停服务时的重试次数（见 [`request_stop`]：要跨过 `START_PENDING` 那个窗口）。
const STOP_ATTEMPTS: u32 = 5;
/// 每次重试之间的间隔。
const STOP_RETRY_STEP: Duration = Duration::from_millis(500);

/// 监督线程等「控制台会话出现」的上限（与探针共用同一个常数）。
const CONSOLE_SESSION_WAIT: Duration = probe::CONSOLE_SESSION_WAIT;
/// 等控制台会话 / 等实例退出的轮询步长。
const WAIT_POLL_STEP: Duration = Duration::from_millis(500);
/// 没有控制台会话时的重试步长（会话没出现不是错误，开机早期就是这样）。
const NO_CONSOLE_RETRY: Duration = Duration::from_secs(5);
/// 等实例结束的轮询切片：要短到能在切片边界上看到停止/重建请求。
const INSTANCE_WAIT_SLICE_MS: u32 = 500;
/// 杀掉实例后等它真的退出的上限。
const INSTANCE_EXIT_WAIT: Duration = Duration::from_secs(10);
/// 等实例退出时的轮询步长（毫秒；`wait_for_process` 收 `u32`）。
const INSTANCE_EXIT_POLL_MS: u32 = 500;
/// 服务自己杀实例时用的退出码（只进日志，不参与任何判断）。
const INSTANCE_TERMINATED_EXIT_CODE: u32 = 1;
/// 分片睡觉的片长（让「停止请求」最多 250ms 就被看到）。
const SLEEP_SLICE: Duration = Duration::from_millis(250);
/// 等监督线程收尾的上限。
///
/// **有上限**：万一监督线程卡住（真机上的未知情形），不能把服务进程一起拖在退出上
/// ——作业对象（`KILL_ON_JOB_CLOSE`）会在进程退出时兜底回收实例。
const SUPERVISOR_JOIN_WAIT: Duration = Duration::from_secs(20);
/// 等监督线程收尾时的轮询步长。
const SUPERVISOR_JOIN_POLL: Duration = Duration::from_millis(200);
/// 装服务前等旧服务停下来的上限。
const STOP_WAIT: Duration = Duration::from_secs(30);
/// 等「上一个服务还在删除中」消失的上限。
const SERVICE_DELETE_WAIT: Duration = Duration::from_secs(10);

/// 程序的入口（由 `main.rs` 的 `#[cfg(windows)]` 分支调用）。
pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args::parse(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}\n");
            eprintln!("{}", args::USAGE);
            std::process::exit(2);
        }
    };

    // `--run` 的日志必须落到文件：SCM 起的进程**不继承控制台**，stdout 直接丢弃，
    // 而「连日志都没有」正是最需要日志的时候。其余模式有控制台，就写控制台。
    //
    // **必须在 `StartServiceCtrlDispatcherW` 之前**（也就是在 `execute` → `service::run`
    // 之前）：一旦 SCM 接管，进程就没有可看的输出了，而启动失败恰恰是最需要线索的时刻。
    if matches!(&command, Command::Run { .. }) {
        init_file_logging();
    } else {
        init_console_logging();
    }

    if let Err(message) = execute(command) {
        tracing::error!(%message, "命令执行失败");
        eprintln!("错误：{message}");
        std::process::exit(1);
    }
}

/// 分发命令。
fn execute(command: Command) -> Result<(), String> {
    match command {
        Command::Help => {
            println!("{}", args::USAGE);
            Ok(())
        }
        Command::Probe => run_local_probe(&ProbeOptions::default()),
        Command::ProbeInstance { stamp } => run_local_probe(&ProbeOptions {
            stamp,
            // 服务拉起的那条路一律先等安全桌面：那一趟的意义就在「锁上之后」。
            wait_secure: Some(probe::CHILD_SECURE_WAIT),
            ..ProbeOptions::with_tag(probe::TAG_INSTANCE)
        }),
        Command::Run { probe: true } => run_probe_service(),
        Command::Run { probe: false } => run_instance_service(),
        Command::ProbeInService => probe_in_service(),
        Command::Install { gui_exe } => {
            let gui_exe = gui_exe.map(PathBuf::from);
            install_service(gui_exe.as_deref())
        }
        Command::Uninstall => service::uninstall()
            .map(|()| {
                println!(
                    "服务已卸载（若它本来就不在，这也算成功）。\n\
                     注意：{} 与 {} 下的文件**没有**被删除——要清干净请手工删目录。",
                    paths::program_files_root().display(),
                    paths::program_data_root().display()
                );
            })
            .map_err(|err| {
                format!("卸载失败：{err}\n（SCM 相关的操作需要管理员权限；错误码 5 = 拒绝访问）")
            }),
        Command::Start => start_service(),
        Command::Stop => stop_service(),
    }
}

// ── 本地探针（阶段 0，原样保留）────────────────────────────────

/// 本地探针（`--probe` 与 `--probe-instance` 共用）。
///
/// **退出码只看「报告写没写出来」**，不看注入结论：结论是**数据**，
/// 由报告承载。把「注入没生效」映射成非零退出码会让服务侧把一次**成功的测量**
/// 报成失败。
fn run_local_probe(options: &ProbeOptions) -> Result<(), String> {
    let outcome = probe::run(options).map_err(|err| {
        format!(
            "探针没跑完（{err}）。日志初始化失败或输出目录写不进去时会是这样；\
             默认输出目录是 {}",
            probe::default_out_dir().display()
        )
    })?;
    println!("{}", summary_line(&outcome));
    Ok(())
}

/// 一行结论摘要（控制台里看一眼就够，细节在报告里）。
fn summary_line(outcome: &ProbeOutcome) -> String {
    let verdict = match outcome.secure_desktop_injection_worked {
        Some(true) => "安全桌面注入：生效",
        Some(false) => "安全桌面注入：**未生效**",
        None => "安全桌面注入：本次没测（当前不在安全桌面上）",
    };
    format!("{verdict}；报告 {}", outcome.report_path.display())
}

// ── 服务入口（两种模式）──────────────────────────────────────

/// `--run --probe`：接上 SCM，等它把探针跑完再自己停下。
fn run_probe_service() -> Result<(), String> {
    PROBE_MODE.store(true, Ordering::SeqCst);
    // 探针模式没有监督线程，收尾是空操作。
    service::run(service::SERVICE_NAME, on_service_event, || {})
        .map_err(|err| translate_run_error(&err))
}

/// `--run`：接上 SCM，监督控制台会话里的客户端实例（**服务本体**）。
fn run_instance_service() -> Result<(), String> {
    PROBE_MODE.store(false, Ordering::SeqCst);
    // 收尾回调在 `service::run` 上报 `SERVICE_STOPPED` **之前**被调——这样 SCM 一看到
    // `Stopped`，服务进程（以及实例的 exe 映像）就已经真的退出了，`--install` 的
    // 「先停旧服务」随后覆盖 exe 才不会撞上共享冲突。见 `service::SHUTDOWN_SINK`。
    let result = service::run(service::SERVICE_NAME, on_service_event, || {
        request_supervisor_stop();
        join_supervisor();
    });
    // `service::run` 返回 = 服务已停（正常停止时上面的回调已收尾），**或**它压根没被 SCM
    // 启动（1063 这类错误——此时 `Start` 从没触发、监督线程根本不存在）。收尾在**两条路径**
    // 上都要跑：正常路径上它是幂等的（`request_supervisor_stop` 只是置同一个标志；
    // `join_supervisor` 取走句柄槽后第二次是 no-op），错误路径上回调没跑过、这里兜住。
    // 作业对象（`KILL_ON_JOB_CLOSE`）也能回收实例，但显式等一等能保证「服务停了，实例也确实没了」。
    request_supervisor_stop();
    join_supervisor();
    result.map_err(|err| translate_run_error(&err))
}

/// 把 `service::run` 的错误翻译成一句人话（1063 = 不是被 SCM 启动的）。
fn translate_run_error(err: &WinError) -> String {
    if err.code() == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT {
        format!(
            "StartServiceCtrlDispatcherW 报 1063：本进程不是被 SCM 启动的。\
             `--run` 只能由 SCM 调用——请用 `--install` 装好服务后由 SCM 启动它\
             （或 `--probe-in-service` 跑探针）。原始错误：{err}"
        )
    } else {
        err.to_string()
    }
}

// ── 服务事件回调与监督线程的共享状态 ─────────────────────────

/// 监督线程与 SCM 事件回调之间共享的状态。
///
/// 事件回调只能是 `fn` 指针（没有地方挂捕获环境），所以状态只能放静态。
/// 锁只在这两处极短地持有（读/写一个字段），不会自锁。
struct SupervisorState {
    /// 实例当前所在的会话；`None` = 还没拉起（或正在等新会话）。
    session: Option<u32>,
    /// 有会话变化要求重建实例。
    rebuild: bool,
    /// SCM 要求停止。
    stop: bool,
}

/// 监督线程的共享状态。
static SUPERVISOR: Mutex<SupervisorState> = Mutex::new(SupervisorState {
    session: None,
    rebuild: false,
    stop: false,
});

/// 监督线程的句柄（`--run` 收尾时等它把实例杀掉）。
static SUPERVISOR_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// 取监督状态锁，**中毒也继续用**（理由同 `win-platform` 的 `service::lock_sink`：
/// 这里存的是几个普通字段，没有需要回滚的不变量，而 `unwrap()` 会让事件回调 panic
/// ——那正是本文件最不能发生的事）。
fn supervisor_state() -> MutexGuard<'static, SupervisorState> {
    SUPERVISOR.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 取监督线程句柄槽，中毒也继续用。
fn supervisor_thread_slot() -> MutexGuard<'static, Option<std::thread::JoinHandle<()>>> {
    SUPERVISOR_THREAD
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// 记下实例当前所在的会话。
fn set_supervised_session(session: Option<u32>) {
    supervisor_state().session = session;
}

/// 请求重建实例。
fn request_rebuild() {
    supervisor_state().rebuild = true;
}

/// 取走「请求重建」标志（读后即清）。
fn take_rebuild() -> bool {
    let mut state = supervisor_state();
    let requested = state.rebuild;
    state.rebuild = false;
    requested
}

/// 请求监督线程停止。
fn request_supervisor_stop() {
    supervisor_state().stop = true;
}

/// 是否已请求停止。
fn supervisor_stop_requested() -> bool {
    supervisor_state().stop
}

/// 服务事件回调。
///
/// **不得 panic**（它在 `extern "system"` 边界内被调，见 `service::run` 的契约），
/// 故这里没有 `unwrap`、没有索引。`Start` 还**必须立刻返回**，所以真正的活派到
/// 自己的线程上——在派发线程上干活会让 SCM 判定「服务未能及时响应启动请求」。
fn on_service_event(event: ServiceEvent) {
    match event {
        ServiceEvent::Start if PROBE_MODE.load(Ordering::SeqCst) => {
            if let Err(err) = std::thread::Builder::new()
                .name(String::from("service-probe"))
                .spawn(service_probe_worker)
            {
                // 起不了线程就等于这次探针失败，但**不能 panic**。
                tracing::error!(%err, "起不了探针线程，本次探针失败");
                request_stop();
            }
        }
        ServiceEvent::Start => {
            // 服务本体：整个监督循环派到自己的线程上（见文件头）。
            match std::thread::Builder::new()
                .name(String::from("instance-supervisor"))
                .spawn(supervisor_worker)
            {
                Ok(handle) => *supervisor_thread_slot() = Some(handle),
                Err(err) => tracing::error!(%err, "起不了监督线程，实例不会被拉起"),
            }
        }
        ServiceEvent::Stop if PROBE_MODE.load(Ordering::SeqCst) => tracing::info!("SCM 要求停止"),
        ServiceEvent::Stop => {
            tracing::info!("SCM 要求停止：让监督线程杀掉实例");
            request_supervisor_stop();
        }
        ServiceEvent::SessionChange {
            event_type,
            session_id,
        } if !PROBE_MODE.load(Ordering::SeqCst) => {
            // 决策是**纯函数**（`supervise::classify_session_change`，本机有单测）：
            // 锁屏/解锁永不重建，会话真的变了才重建。
            let supervised = supervisor_state().session;
            match supervise::classify_session_change(event_type, session_id, supervised) {
                SessionChangeAction::Rebuild => {
                    tracing::info!(event_type, session_id, supervised, "会话变化 → 重建实例");
                    request_rebuild();
                }
                SessionChangeAction::Ignore => {
                    tracing::debug!(event_type, session_id, supervised, "会话变化（忽略）");
                }
            }
        }
        ServiceEvent::SessionChange {
            event_type,
            session_id,
        } => {
            // 探针恰恰要在**当前**桌面上做，跟着切反而会跑偏。
            tracing::debug!(event_type, session_id, "会话变化（阶段 0 探针不响应）");
        }
        ServiceEvent::Other { control } => tracing::debug!(control, "其它服务控制码"),
    }
}

// ── 监督循环 ─────────────────────────────────────────────

/// 一次实例运行的结果。
enum InstanceOutcome {
    /// SCM 要求停止：实例已杀，监督循环该收尾。
    Stopped,
    /// 会话变了：实例已杀，**立刻**重建（不退避——这不是失败）。
    SessionChanged,
    /// 实例自己退出了（带退出码）。
    Exited {
        /// 实例进程的退出码。
        code: u32,
    },
    /// 压根没拉起来（单实例锁被占 / 取令牌失败 / `CreateProcessAsUserW` 失败）。
    Failed {
        /// 给日志看的原因。
        reason: String,
    },
}

/// 服务**无法提供服务**时的拒跑理由（`None` = 前置条件都在）。
///
/// 只读两条确定性前置条件——它们与监督循环真正要用的东西**一一对应**，故不存在第二个事实
/// 来源：`paths::host_config_path()`（实例的配置，见 `run_instance` 里给实例拼的命令行）与
/// `paths::installed_instance_exe()`（监督循环要 spawn 的那个 exe）。判据与文案是纯函数
/// （[`supervise::refuse_reason`]，本机有单测），这里只负责把两条真实路径读出来。
fn refuse_reason() -> Option<String> {
    let host_config = paths::host_config_path();
    let instance_exe = paths::installed_instance_exe();
    supervise::refuse_reason(
        supervise::Precondition {
            path: &host_config,
            present: host_config.is_file(),
        },
        supervise::Precondition {
            path: &instance_exe,
            present: instance_exe.is_file(),
        },
    )
}

/// 监督线程主体：等控制台会话 → 拉起实例 → 监督 → 退出即退避重建。
fn supervisor_worker() {
    tracing::info!("监督线程启动：目标 = 控制台会话里的客户端实例");
    let mut attempt: u32 = 0;
    loop {
        if supervisor_stop_requested() {
            break;
        }
        // **先判自己能不能干活，再等控制台会话。** 顺序是刻意的：缺 host.toml / 缺实例 exe
        // 时，`wait_for_console_session` 会白等最多 `CONSOLE_SESSION_WAIT` 才轮到检查——而
        // 那段时间里服务一直停在 `Running` 却没有实例，正是「GUI 转被动、整机失去身份」那个
        // 状态。开机时尤其明显：服务是 AUTO_START，比 LogonUI 先起来。
        //
        // 判据只放在循环顶部（**不放** `ServiceEvent::Start` 那条 arm）：循环顶部用一个机制
        // 同时盖住「开机就缺」与「跑到一半被删」，而 `Start` 那条 arm 跑在 SCM 派发线程上、
        // **必须立刻返回**，塞不进这种检查。
        //
        // 拒跑时**不是简单 return**：那会让 `service_main` 永远轮询下去、服务停在 `Running`
        // 却什么也没干——正是本检查要消灭的状态。置停止标志后 `service_main` 会走完收尾并上报
        // `SERVICE_STOPPED`（退出码 0），于是 SCM 不触发 `SC_ACTION_RESTART` 失败动作，
        // GUI 的 3 秒轮询也能拿回客户端身份。
        if let Some(reason) = refuse_reason() {
            tracing::error!(%reason, "服务无法提供服务，自行停止并把客户端身份交回 GUI");
            service::request_self_stop();
            break;
        }
        let Some(session) = wait_for_console_session() else {
            if sleep_or_stop(NO_CONSOLE_RETRY) {
                break;
            }
            continue;
        };
        // 丢掉此刻还挂着的「重建请求」——它必然是**旧**会话留下的。
        //
        // `set_supervised_session(None)` 只在 `run_instance` 返回后才跑，所以在
        // 「杀旧实例（`kill_instance`，上限 10 秒）→ 取令牌 → spawn 新实例」这段窗口里，
        // 状态里写着的仍是**上一个**会话，一条会话变化事件会被判成 `Rebuild` 并置这个标志。
        // 而那条请求要的正是「换到当前控制台会话」——我们刚刚 `wait_for_console_session`
        // 读到的 `session` 就是它。留着这个标志会让**刚拉起的**实例的 `supervise_loop`
        // 在第一次 `take_rebuild()` 就返回 `SessionChanged`，把已经拉进正确会话的实例
        // 立刻杀掉重建（日志里表现为「会话变化，立刻重建实例」出现在一个全新实例上，
        // 客户端连上又掉一次）。清在这里、且清在 `set_supervised_session` **之前**是安全的：
        // 此刻 `session` 还是 `None`，`classify_session_change` 对任何事件都返回 `Ignore`，
        // 不会有新请求挤进来。
        let _ = take_rebuild();
        set_supervised_session(Some(session));
        let outcome = run_instance(session);
        set_supervised_session(None);

        match outcome {
            InstanceOutcome::Stopped => break,
            InstanceOutcome::SessionChanged => {
                // 会话换了不是失败：立刻重建，退避计数归零。
                attempt = 0;
                tracing::info!("会话变化，立刻重建实例");
            }
            InstanceOutcome::Exited { code } => {
                attempt = attempt.saturating_add(1);
                let wait = supervise::backoff_for(attempt);
                tracing::warn!(
                    code,
                    attempt,
                    backoff_seconds = wait.as_secs(),
                    "实例退出，退避后重启"
                );
                if sleep_or_stop(wait) {
                    break;
                }
            }
            InstanceOutcome::Failed { reason } => {
                attempt = attempt.saturating_add(1);
                let wait = supervise::backoff_for(attempt);
                tracing::error!(
                    %reason,
                    attempt,
                    backoff_seconds = wait.as_secs(),
                    "拉起实例失败，退避后重试"
                );
                if sleep_or_stop(wait) {
                    break;
                }
            }
        }
    }
    tracing::info!("监督线程退出");
}

/// 等控制台会话出现（最多 [`CONSOLE_SESSION_WAIT`]）。
///
/// 服务是**自动启动**的，开机时往往比 LogonUI 先起来；那一刻还没有交互式会话，
/// 直接放弃就等于「开机停在登录界面」这一趟白跑——而那正是无人值守要覆盖的场景。
/// 上限必须有：卡在等会话上，SCM 会把服务显示成无响应。
fn wait_for_console_session() -> Option<u32> {
    let deadline = Instant::now() + CONSOLE_SESSION_WAIT;
    loop {
        if supervisor_stop_requested() {
            return None;
        }
        if let Some(id) = rust_tunnel_win_platform::session::active_console_session().id() {
            return Some(id);
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                seconds = CONSOLE_SESSION_WAIT.as_secs(),
                "等控制台会话超时（WTSGetActiveConsoleSessionId 一直是 0xFFFFFFFF）"
            );
            return None;
        }
        std::thread::sleep(WAIT_POLL_STEP);
    }
}

/// 分片睡 `total`；期间看到停止请求就提前返回 `true`（调用方据此收尾）。
fn sleep_or_stop(total: Duration) -> bool {
    let deadline = Instant::now() + total;
    loop {
        if supervisor_stop_requested() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep((deadline - now).min(SLEEP_SLICE));
    }
}

/// 拉起一个实例并监督它，直到它退出 / 会话变化 / 服务停止。
fn run_instance(session: u32) -> InstanceOutcome {
    // 单实例锁：**已被别人持有**就说明已经有一个实例在跑（多半是上一个还没死透），
    // 绝不「再拉一个」——服务端按客户端名唯一，两个实例会互相踢，用户看到的是
    // 「每隔几秒掉一次」。
    //
    // 但**创建锁本身失败不算致命**：`Global\` 命名对象要 `SeCreateGlobalPrivilege`，
    // 若某个环境里拿不到它，`CreateMutexW` 会以拒绝访问失败。那种情况下退化成
    // 「没有这道防线」继续拉起，比「服务永远拉不起实例」好得多——「先杀旧实例、等它
    // 真的退出、再拉新的」这条主路径本来就独立成立。
    let _guard = match InstanceGuard::acquire() {
        Ok(LockOutcome::Acquired(guard)) => Some(guard),
        Ok(LockOutcome::AlreadyHeld) => {
            return InstanceOutcome::Failed {
                reason: String::from("已有一个实例在运行（单实例锁被别的进程持有）"),
            };
        }
        Err(err) => {
            tracing::warn!(%err, "单实例锁创建失败（多半是缺 SeCreateGlobalPrivilege）；本次没有这道防线");
            None
        }
    };

    let Some((token, source)) = instance_token(session) else {
        return InstanceOutcome::Failed {
            reason: String::from("取实例令牌失败（见上一条日志里的两条路失败原因）"),
        };
    };

    let exe = paths::installed_instance_exe();
    let command_line = supervise::instance_command_line(
        &exe,
        &paths::host_config_path(),
        &paths::share_dir(),
        &paths::download_dir(),
    );
    tracing::info!(session, source, %command_line, "拉起实例");

    // 「句柄一关就把里面的进程全杀掉」的作业对象：服务进程一死，实例被系统回收
    // ——这是「服务停了但实例还在后台跑」这类僵尸状态的兜底。
    let job = match process::create_kill_on_close_job() {
        Ok(job) => Some(job),
        Err(err) => {
            tracing::warn!(%err, "建作业对象失败（实例不会被自动回收）");
            None
        }
    };

    let spawned = match process::spawn_with_token(&token, &command_line, None) {
        Ok(spawned) => spawned,
        Err(err) => {
            return InstanceOutcome::Failed {
                reason: format!(
                    "CreateProcessAsUserW 失败：{err}\
                     （1314 = 特权没启用或令牌不是主令牌，5 = 拒绝访问）"
                ),
            };
        }
    };
    tracing::info!(pid = spawned.pid, "实例已拉起");

    if let Some(job) = &job {
        if let Err(err) = process::assign_to_job(job, &spawned.process) {
            // 不是致命错误：显式 TerminateProcess 那条路还在。
            tracing::warn!(%err, "放进作业对象失败");
        }
    }

    let outcome = supervise_loop(&spawned);
    kill_instance(job, &spawned);
    outcome
}

/// 取实例令牌，并把两条路的失败原因写进日志；成功时返回（令牌，来源名）。
fn instance_token(session: u32) -> Option<(rust_tunnel_win_platform::handle::OwnedHandle, &'static str)> {
    let picked = process::pick_instance_token(session);
    for (name, outcome) in &picked.privilege_failures {
        // 逐项渲染：`NotAssigned`（持有但没生效，`AdjustTokenPrivileges` 返回 TRUE 但
        // last-error 是 1300）会被渲染成「未生效(ERROR_NOT_ALL_ASSIGNED)」，而不是被
        // 静默当成成功——后者会让真机上的 1314 看起来毫无来由。
        tracing::warn!(
            privilege = name,
            outcome = %process::privilege_enable_text(name, outcome),
            "启用特权未成功（真缺了会在 spawn 那步报 1314）"
        );
    }
    if let Some(err) = &picked.winlogon_error {
        tracing::debug!(%err, "B 路（winlogon 令牌）不可用，回退 A 路");
    }
    if let Some(err) = &picked.own_token_error {
        tracing::error!(%err, "A 路（自身令牌 + TokenSessionId）不可用");
    }
    let (Some(token), Some(source)) = (picked.token, picked.source) else {
        return None;
    };
    Some((token, source.as_str()))
}

/// 等实例结束 / 会话变化 / 服务停止。
fn supervise_loop(spawned: &SpawnedProcess) -> InstanceOutcome {
    loop {
        if supervisor_stop_requested() {
            return InstanceOutcome::Stopped;
        }
        if take_rebuild() {
            return InstanceOutcome::SessionChanged;
        }
        match process::wait_for_process(&spawned.process, INSTANCE_WAIT_SLICE_MS) {
            Ok(true) => {
                let code = process::exit_code(&spawned.process).unwrap_or(0);
                return InstanceOutcome::Exited { code };
            }
            // 切片超时不是错误：回头再看那两个标志。
            Ok(false) => {}
            Err(err) => {
                return InstanceOutcome::Failed {
                    reason: format!("等实例结束失败：{err}"),
                };
            }
        }
    }
}

/// 结束实例：先直接终止进程，再关掉作业对象兜底，并等它真的退出。
///
/// 为什么两道都做：作业对象（`KILL_ON_JOB_CLOSE`）是「句柄关闭时才杀」的兜底，
/// 但它要求实例**确实被放进了作业**（[`process::assign_to_job`] 可能失败）。
/// 显式 `TerminateProcess` 是另一条独立的路——两者都用，才不依赖任何单一机制。
///
/// **必须等它退出**：不等的话，下一次 spawn 可能与还没死透的旧实例并存一小段时间，
/// 而那正是单实例锁要防的窗口。
fn kill_instance(job: Option<rust_tunnel_win_platform::handle::OwnedHandle>, spawned: &SpawnedProcess) {
    match process::terminate_process(&spawned.process, INSTANCE_TERMINATED_EXIT_CODE) {
        Ok(()) => {}
        Err(err) => tracing::debug!(%err, "TerminateProcess 失败（多半是它已经退出了）"),
    }
    let deadline = Instant::now() + INSTANCE_EXIT_WAIT;
    loop {
        match process::wait_for_process(&spawned.process, INSTANCE_EXIT_POLL_MS) {
            Ok(true) => break,
            Ok(false) if Instant::now() >= deadline => {
                tracing::warn!("等实例退出超时；改由作业对象兜底回收");
                break;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::debug!(%err, "等实例退出失败");
                break;
            }
        }
    }
    // 关掉作业句柄 → 作业里的进程被系统杀掉（这是「服务被杀时实例也一起走」的那条路）。
    drop(job);
}

/// 等监督线程收尾（最多 [`SUPERVISOR_JOIN_WAIT`]）。
fn join_supervisor() {
    let handle = supervisor_thread_slot().take();
    let Some(handle) = handle else {
        return;
    };
    let deadline = Instant::now() + SUPERVISOR_JOIN_WAIT;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            tracing::warn!("监督线程未在期限内收尾，进程退出时由作业对象回收实例");
            return;
        }
        std::thread::sleep(SUPERVISOR_JOIN_POLL);
    }
    if handle.join().is_err() {
        tracing::error!("监督线程 panic（实例已由作业对象回收）");
    }
}

// ── 启停（--start / --stop）──────────────────────────────

/// 确认服务**已安装**；没装时给一句明确的话。返回当前状态供调用方打印。
///
/// 与 `--uninstall` 的「不在也算成功」**相反**：启停的对象必须存在，否则一句「成功」会让
/// 操作员误以为服务在跑，而其实他从来没装过（正是这个任务的由来）。
fn require_installed() -> Result<ServiceState, String> {
    let state = service::query_state().map_err(|err| {
        format!("查询服务状态失败：{err}\n（SCM 相关的操作需要管理员权限；错误码 5 = 拒绝访问）")
    })?;
    if state.is_installed() {
        Ok(state)
    } else {
        Err(String::from(
            "服务尚未安装（SCM 里没有 rust-tunnel-client-service），没有可启停的服务。\
             先跑一次 --install。",
        ))
    }
}

/// `--start`：请 SCM 启动服务，并**确认它真的跑起来了**。**幂等**——已经在跑也算成功。
///
/// # 为什么**不**在这里校验 `host.toml`
///
/// 「启动服务会让 GUI 转被动、而实例读不到配置就注册不了、整机失去客户端身份」这条链只有
/// GUI 知道（它才是那个转被动的进程），所以闸门设在 GUI（`service_ui::start_blocked_reason`）
/// 与服务自身（缺 `host.toml` 时服务会**拒跑并自停**，见 [`refuse_reason`]）。CLI 必须保留
/// 「没有 host.toml 也能起」的能力：`--probe-in-service` 的探针实例压根不读那个文件，把它拦掉
/// 会让真机验收流程走不下去。
///
/// # 为什么启动后还要**观察**一次
///
/// 上面的拒跑闸门让「起来又自己停」只隔几毫秒，而 SCM 的 `StartServiceW` **不等服务进入
/// 运行态**就返回——照旧宣告成功的话，GUI（只看子进程退出码）会说「启动服务成功」，用户却
/// 看到状态又变回「已停止」。所以这里按**结果**判定，见 [`settle_start`]。
fn start_service() -> Result<(), String> {
    let state = require_installed()?;
    match service::start() {
        Ok(()) => {}
        // 已经在跑：SCM 报 1056。不是错误——幂等，当作成功（与 --uninstall 的「不在也算成功」
        // 同精神）。
        Err(err) if err.code() == ERROR_SERVICE_ALREADY_RUNNING => {
            println!("服务已经在运行，无需启动。");
            return Ok(());
        }
        Err(err) => {
            return Err(format!(
                "启动服务失败：{err}\n（SCM 相关的操作需要管理员权限；错误码 5 = 拒绝访问）"
            ));
        }
    }
    println!("已请求启动服务（原状态 {state:?}），正在确认它真的跑起来了……");
    settle_start()
}

/// 等 SCM 把服务带到 `Running`，再稳住一小会儿，确认它没「起来又自己停了」。
///
/// 看到 `Running` 之后**必须再看一眼**：拒跑闸门（[`refuse_reason`]）使自停只隔几毫秒，
/// 慢机器/慢磁盘上则可能拉长，故留 [`START_SETTLE_HOLD`] 的窗口。
fn settle_start() -> Result<(), String> {
    let deadline = Instant::now() + START_SETTLE_TIMEOUT;
    loop {
        match service::query_state().map_err(|err| format!("查询服务状态失败：{err}"))? {
            ServiceState::Running => break,
            // 已经明确停下/没了：不必等满上限。我们刚才**成功**请求过启动，现在它不在运行，
            // 只可能是「起来又停了」（多半撞上拒跑闸门）。
            ServiceState::Stopped | ServiceState::NotInstalled => return Err(start_settle_error()),
            state => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "等了 {} 秒服务始终没进入「运行中」（当前状态 {state:?}）。\
                         它可能卡在启动早期，看 {}（读它要管理员）确认。",
                        START_SETTLE_TIMEOUT.as_secs(),
                        paths::service_log_path().display()
                    ));
                }
            }
        }
        std::thread::sleep(START_SETTLE_POLL);
    }
    std::thread::sleep(START_SETTLE_HOLD);
    match service::query_state().map_err(|err| format!("查询服务状态失败：{err}"))? {
        ServiceState::Running => {
            println!("服务已启动并在运行。");
            Ok(())
        }
        _ => Err(start_settle_error()),
    }
}

/// 服务「启动后随即自己停下」时的错误文案。
///
/// 最可能的原因是拒跑闸门（[`refuse_reason`]）：缺 `host.toml`（更常见）或缺实例 exe。
/// 此时服务已经**自行停止、把客户端身份交回 GUI**，文案必须说清这一点——否则用户会去服务
/// 管理器里找一个其实已经不在跑的服务。
fn start_settle_error() -> String {
    format!(
        "服务启动后随即自己停下了。最可能的原因是它发现**无法提供服务**——\
         host.toml 还没写（或实例 exe 不在）：那样它会自行停止、把客户端身份交回 GUI。\n\
         顺序必须是「安装 → 写 host.toml → 启动」，中间那步不能跳（见 WINDOWS-SECURE-DESKTOP.md）。\n\
         具体原因看 {}（读它要管理员）。",
        paths::service_log_path().display()
    )
}

/// `--stop`：请 SCM 停服务并**等它真的停**（上限 [`STOP_WAIT`]）。**幂等**——本来没在跑也算成功。
///
/// # 为什么等
///
/// GUI 每 3 秒查一次服务状态（`service_watch::POLL_INTERVAL`）；本命令同步等到 `Stopped` 再返回，
/// 用户就能立刻看到状态变化，而不是「点了停止、几秒后才变」。等待复用安装路径那段
/// [`stop_running_service`]（含「本来就没跑」的幂等短路，故 `ERROR_SERVICE_NOT_ACTIVE`(1062)
/// 无需在这里单独处理——那条路径只把 `service::stop()` 的报错记进 debug、以轮询为准）。
fn stop_service() -> Result<(), String> {
    let _ = require_installed()?;
    stop_running_service()?;
    println!("服务已停止（若它本来就没在跑，这也算成功）。");
    Ok(())
}

// ── 安装 ─────────────────────────────────────────────────

/// `--install`：复制两个 exe 到 `%ProgramFiles%\rust-tunnel\`、建目录与 DACL、注册服务。
///
/// # 为什么两个 exe 都要复制
///
/// 服务以 LocalSystem 运行。若它的 `ImagePath` 指向一个**用户可写**的目录（例如用户
/// 下载归档的那个文件夹），**任何本机用户替换那个 exe 就等于下次服务启动拿到 SYSTEM**。
/// 所以安装时把服务 exe 与实例 exe 都复制到 `%ProgramFiles%\rust-tunnel\`（对非管理员
/// 不可写），`ImagePath` 指向**副本**，并用 [`security::harden_directory`] 把那条限制
/// 写实、再读回校验。
///
/// # 为什么 `host.toml` 不由本程序生成
///
/// 它含服务端**全局 token**，而那个值只有服务端 Web 界面能给出。凭空写一个空 token 的
/// 模板只会让服务反复注册失败、日志刷屏，问题却不会因此更快被发现——所以这里只检查、
/// 并把「还差这一步」明确打出来。
fn install_service(gui_exe: Option<&Path>) -> Result<(), String> {
    let service_exe = std::env::current_exe()
        .map_err(|err| format!("拿不到本程序的路径，没法安装：{err}"))?;
    let gui_exe = locate_gui_exe(gui_exe, &service_exe)?;

    // 1) 先停掉正在跑的服务：旧实例占着 exe 文件，不先停就替换不了（`std::fs::copy`
    //    覆盖一个正在运行的 exe 会失败——升级路径必须走这一步）。
    //    这一步现在**真的**同步：`service::run` 在上报 `SERVICE_STOPPED` 之前会先跑收尾
    //    回调（杀掉实例、join 监督线程），所以轮询到 `Stopped` 时服务进程（连同实例的
    //    exe 映像）已经退出，随后的覆盖不会撞上共享冲突。
    stop_running_service()?;

    // 2) 两个 exe 复制到非管理员不可写的目录，并把 DACL 写实。
    let exe_dir = paths::program_files_root();
    security::harden_directory(&exe_dir)
        .map_err(|err| format!("加固 {} 失败：{err}", exe_dir.display()))?;
    copy_exe(&service_exe, &paths::installed_service_exe())?;
    copy_exe(&gui_exe, &paths::installed_instance_exe())?;

    // 3) 数据目录（日志 + host.toml + 共享/下载）。同样去掉继承、只留 SYSTEM + Administrators
    //    ——`%ProgramData%` 本身对 `BUILTIN\Users` 开放，直接放它下面等于全局可读。
    let data_dir = paths::program_data_root();
    security::harden_directory(&data_dir)
        .map_err(|err| format!("加固 {} 失败：{err}", data_dir.display()))?;

    // 4) 注册服务（ImagePath 指向**副本**，参数是编译期常量 `--run`）。
    register_service(&paths::installed_service_exe())?;

    // 5) host.toml 是运维/安装器写的：本程序不知道服务端 token，不能凭空生成。
    report_host_config();
    Ok(())
}

/// 实例（GUI）exe 的候选文件名。
///
/// CI 的归档名与本地构建名不同（见 `release-client.yml`），两个都认——用户下载后
/// 两个产物是**并排**放在同一个目录里的。
const GUI_EXE_CANDIDATES: [&str; 2] = [
    "rust-tunnel-client-gui.exe",
    "rust-tunnel-client-gui-windows-x86_64.exe",
];

/// 找实例 exe：显式给了就用它，否则在**本 exe 所在目录**里按已知文件名找。
///
/// # 为什么默认「同目录」
///
/// CI 把服务与 GUI 两个产物并排落到 `${DEPLOY_PATH}/client/<tag>/`，用户下载后就是同一个
/// 目录；本地构建时 `target/<triple>/release/` 里同样两个都在。所以「同目录」是覆盖面最广
/// 的默认值，`--gui-exe` 覆盖特例（例如用户把 GUI 放在别处）。
fn locate_gui_exe(explicit: Option<&Path>, service_exe: &Path) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(format!("--gui-exe 指向的文件不存在：{}", path.display()));
    }
    let dir = service_exe.parent().unwrap_or_else(|| Path::new("."));
    for name in GUI_EXE_CANDIDATES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "在 {} 里找不到实例（GUI）exe（找过：{}）。用 --gui-exe <路径> 显式指定。",
        dir.display(),
        GUI_EXE_CANDIDATES.join("、")
    ))
}

/// 复制一个 exe（目标目录必须已经存在且加固过）。
fn copy_exe(from: &Path, to: &Path) -> Result<(), String> {
    if from == to {
        // 已经在目标位置（例如用户直接在 `%ProgramFiles%\rust-tunnel` 里跑 `--install`）：
        // 自我复制会把源文件截断成 0 字节。
        return Ok(());
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|err| format!("复制 {} → {} 失败：{err}", from.display(), to.display()))
}

/// 请服务停下来并等它真的停（`--install` 与 `--stop` **共用**）。
///
/// `--install` 要它是因为旧实例占着 exe 文件；`--stop` 要它是因为用户要看到同步的状态变化。
/// 「等它真的停」是靠轮询 [`ServiceState::Stopped`]：服务侧在上报 `SERVICE_STOPPED` 之前
/// 会先跑收尾回调（杀实例 + join 监督线程），所以状态变成 `Stopped` 时进程确实已经退出，
/// 覆盖 exe 不会撞上共享冲突（见 `service::SHUTDOWN_SINK`）。
///
/// 幂等：本来就已停止（或期间被删掉）时直接返回成功。
fn stop_running_service() -> Result<(), String> {
    let state = service::query_state().map_err(|err| {
        format!("查询服务状态失败：{err}\n（SCM 相关的操作需要管理员权限；错误码 5 = 拒绝访问）")
    })?;
    if !state.is_installed() || state == ServiceState::Stopped {
        return Ok(());
    }
    println!("服务当前状态 {state:?}，正在停止它并等它真的停下……");
    // 已经在停的路上时 1062/1061 都可能，忽略——下面的轮询才是判据。
    if let Err(err) = service::stop() {
        tracing::debug!(%err, "请求停止服务失败，继续等它停");
    }
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        let state = service::query_state().map_err(|err| format!("查询服务状态失败：{err}"))?;
        if matches!(state, ServiceState::Stopped | ServiceState::NotInstalled) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "等了 {} 秒服务还在 {state:?}；先手工把它停掉再重试",
                STOP_WAIT.as_secs()
            ));
        }
        std::thread::sleep(SERVICE_POLL_STEP);
    }
}

/// 注册服务：已装过就先卸再装，保证 `ImagePath` 指向加固后的副本。
///
/// **为什么不能「已存在就跳过」**：阶段 0 的 `--probe-in-service` 会用 `current_exe()`
/// （多半是用户可写的下载目录）注册服务。那种旧登记必须被换成指向 `%ProgramFiles%` 副本的
/// `ImagePath`，否则「替换 exe = 拿到 SYSTEM」这个面还在。
fn register_service(installed_service: &Path) -> Result<(), String> {
    // 最后一道闸门：待注册的 exe **必须**落在 `%ProgramFiles%\rust-tunnel\` 下。
    // 服务以 SYSTEM 运行，注册一个普通用户能替换的路径就等于把 SYSTEM 交出去——
    // 与其「小心别写错」，不如在这里直接拒绝。
    if !paths::is_under_program_files(installed_service) {
        return Err(format!(
            "拒绝注册：待注册的 exe（{}）不在 {} 下——\
             服务以 SYSTEM 运行，注册一个普通用户能替换的路径等于把 SYSTEM 交出去。",
            installed_service.display(),
            paths::program_files_root().display()
        ));
    }

    let state = service::query_state().map_err(|err| format!("查询服务状态失败：{err}"))?;
    if state.is_installed() {
        println!("服务已存在（{state:?}），先卸载再按加固后的路径重新注册……");
        service::uninstall().map_err(|err| {
            format!("卸载旧服务失败：{err}\n（需要管理员权限；错误码 5 = 拒绝访问）")
        })?;
    }
    // 参数是**编译期常量**（见 `SERVICE_RUN_ARGS`）。
    let extra_args: Vec<String> = SERVICE_RUN_ARGS.iter().map(|arg| String::from(*arg)).collect();
    install_with_retry(installed_service, &extra_args)
}

/// 注册服务，遇到「上一个服务还在删除中」（1072）时重试几次。
fn install_with_retry(installed_service: &Path, extra_args: &[String]) -> Result<(), String> {
    let deadline = Instant::now() + SERVICE_DELETE_WAIT;
    loop {
        match service::install(installed_service, extra_args) {
            Ok(()) => {
                println!(
                    "服务已安装并设为自动启动：{} --run",
                    installed_service.display()
                );
                return Ok(());
            }
            Err(err) => {
                if err.code() == ERROR_SERVICE_MARKED_FOR_DELETE && Instant::now() < deadline {
                    std::thread::sleep(SERVICE_POLL_STEP);
                    continue;
                }
                return Err(format!(
                    "注册服务失败：{err}\n（需要管理员权限；183 = 已安装，先 --uninstall）"
                ));
            }
        }
    }
}

/// 检查 `host.toml` 是否存在并给出下一步。
fn report_host_config() {
    let path = paths::host_config_path();
    if path.is_file() {
        println!("实例配置已就位：{}", path.display());
    } else {
        println!(
            "**还差一步**：写 {}（服务端地址 + 客户端 token + 审批策略）。\n\
             本程序不生成它——那个 token 只有服务端 Web 界面能给。\n\
             无人值守下还要把 approval 设成 Auto：没有人能点确认弹窗，Confirm 会让所有\n\
             接入请求在 45 秒后超时被拒。注意 Auto 对**任何**对端都自动放行——实例侧没有\n\
             「只放行名单里的对端」这种模式。",
            path.display()
        );
    }
}

// ── 阶段 0 的服务侧探针 ──────────────────────────────────────

/// 服务侧的探针线程主体。
fn service_probe_worker() {
    let options = ProbeOptions::with_tag(probe::TAG_INSTANCE);
    match probe::run_from_service(&options) {
        Ok(outcome) => tracing::info!(
            path = %outcome.report_path.display(),
            verdict = ?outcome.secure_desktop_injection_worked,
            "服务侧探针完成"
        ),
        Err(err) => tracing::error!(
            %err,
            "服务侧探针失败（报告没写出来）—— 看本文件旁边的 service.log"
        ),
    }
    request_stop();
}

/// 请 SCM 停掉本服务。
///
/// 走 SCM 而不是直接 `std::process::exit`：这条路径与我们平时在服务管理器里按
/// 「停止」完全一样，于是 `ctrl_handler` 会置停止标志、`service::run` 的轮询
/// 看到后**正常收尾**（上报 `SERVICE_STOPPED`）。直接退出会让 SCM 认为服务异常终止。
///
/// # 为什么要重试
///
/// 探针是从 `Start` 事件里派出去的，而那一刻 SCM 还认为服务处于
/// `START_PENDING`（`SERVICE_RUNNING` 在 `notify(Start)` **返回之后**才上报）。
/// 如果探针在几毫秒内就结束（例如报告写不出去），停止请求会撞在那个窗口上被 SCM
/// 以 1061 拒掉——**服务会一直挂着**，用户看到的是 `--probe-in-service` 等到超时。
/// 重试几次就没事（正常情况下第一次就成功）。
fn request_stop() {
    for attempt in 1..=STOP_ATTEMPTS {
        match service::stop() {
            Ok(()) => return,
            Err(err) => {
                tracing::warn!(%err, attempt, "请求停止服务失败，稍后重试");
            }
        }
        std::thread::sleep(STOP_RETRY_STEP);
    }
    tracing::error!(
        "多次请求停止都失败，服务会一直挂到 --probe-in-service 超时；\
         请用 `--uninstall` 收尾（它会尽力先停再删）"
    );
}

/// `--probe-in-service`：把服务装好、起来、等它跑完。
///
/// 服务跑完会**自己停下**（见 [`request_stop`]），所以这里只轮询状态。
/// 不自动卸载：用户往往要接着在另一种状态下再跑一遍，留着省事；收尾用 `--uninstall`。
///
/// # 残留风险（阶段 0 探针特有）
///
/// 它注册的服务是 **AUTO_START**，`ImagePath` 指向**当前这个 exe**（`current_exe()`），
/// 而那个路径可能是用户可写的。AUTO_START 是**有意**的（第 3 趟「重启后看登录界面」靠它），
/// 但代价是：本机任何用户替换那个 exe = 下次开机拿到 SYSTEM。故本函数在**装之前**打印一段
/// 醒目的安全提示；正式安装请走 `--install`（复制到 `%ProgramFiles%` 并加固 DACL）。
fn probe_in_service() -> Result<(), String> {
    let state = service::query_state().map_err(|err| {
        format!("查询服务状态失败：{err}\n（SCM 相关的操作需要管理员权限；错误码 5 = 拒绝访问）")
    })?;

    match state {
        ServiceState::NotInstalled => {
            let exe = std::env::current_exe()
                .map_err(|err| format!("拿不到本程序的路径，没法注册服务：{err}"))?;
            // ⚠ 安全提示必须在**装之前**打出来。这个服务是 AUTO_START 的 SYSTEM 服务，
            // 而 `ImagePath` 指向**当前这个 exe**（`current_exe()`）——它可能是用户从下载
            // 目录/共享目录/宽松权限的 `C:\temp` 里跑的，那些位置**本机任何用户都能替换**。
            // 替换它 = 下次开机以 SYSTEM 跑攻击者的代码（本地提权）。AUTO_START 是有意的
            // （第 3 趟「重启后看登录界面」靠它），故不改启动类型，只把风险当面说清、并
            // 指向 `--install`（它复制到 %ProgramFiles% 并加固 DACL）。
            println!(
                "⚠ 安全提示：本命令会注册一个 **AUTO_START 的 SYSTEM 服务**，其 ImagePath 是：\n  {}\n\
                 这个 exe 就在你**当前所在的目录**里（`current_exe()`），可能是下载目录、共享目录\
                 或某个权限宽松的临时目录——**本机任何用户都能替换它**。服务以 SYSTEM 运行且\
                 自动启动，替换该 exe 就等于下次开机拿到 SYSTEM（本地提权）。\n\
                 正式安装请用 `--install`：它把 exe 复制到 %ProgramFiles%\\rust-tunnel\\\
                 （非管理员不可写）并加固 DACL。探针这样做只是为了「装一次 → 重启 → 看报告」。",
                exe.display()
            );
            println!("安装服务：{} --run --probe", exe.display());
            service::install(&exe, &[String::from("--run"), String::from("--probe")])
                .map_err(|err| format!("安装服务失败：{err}\n（需要管理员权限；183 = 已安装，那就直接再跑一次本命令即可）"))?;
        }
        ServiceState::Stopped => {
            println!("服务已安装（已停止），直接启动；若你改过它的命令行，先 --uninstall。");
        }
        ServiceState::Running | ServiceState::StartPending => {
            return Err(String::from(
                "服务已经在跑。先 `--uninstall` 或等它自己停（上一次探针可能还在跑）。",
            ));
        }
        other => {
            return Err(format!(
                "服务当前状态是 {other:?}，不像是能直接启动的样子；先 `--uninstall` 收干净。"
            ));
        }
    }

    println!("启动服务（SYSTEM / 会话 0），它会自己把子实例拉进控制台会话……");
    service::start().map_err(|err| {
        format!("启动服务失败：{err}\n（1060 = 服务不存在，先跑一次不带 --uninstall 的本命令）")
    })?;

    wait_until_stopped()?;

    let dir = probe::default_out_dir();
    if let Some(path) = newest_report(&dir) {
        println!("服务已跑完并自行停止。报告：\n  {}", path.display());
        println!("（同一个目录里还有 shot-*.bmp 截图）");
    } else {
        println!(
            "服务已停止，但 {} 下没找到 probe-*-service.txt。",
            dir.display()
        );
    }
    // 服务日志与探针报告现在都在 `%ProgramData%\rust-tunnel\` 下（各自的 DACL 都已加固，
    // 仅 SYSTEM + Administrators 可写），只是路径不同：日志在 `service.log`，报告在
    // `probe\`。两个都不再是阶段 0 那个全局可写的 `C:\Users\Public\...`。
    println!("服务自己的日志：{}", paths::service_log_path().display());
    println!("看完请执行：rust-tunnel-client-service --uninstall");
    Ok(())
}

/// 轮询等服务停下（`Stopped` 或服务不存在）。
fn wait_until_stopped() -> Result<(), String> {
    let deadline = Instant::now() + SERVICE_PROBE_TIMEOUT;
    loop {
        let state = service::query_state().map_err(|err| format!("查询服务状态失败：{err}"))?;
        match state {
            ServiceState::Stopped | ServiceState::NotInstalled => return Ok(()),
            _ => {}
        }
        if Instant::now() >= deadline {
            let seconds = SERVICE_PROBE_TIMEOUT.as_secs();
            return Err(format!(
                "等了 {seconds} 秒服务还在跑（当前状态 {state:?}）。\
                 它可能卡在别处：看 {}，然后 `--uninstall` 收尾。",
                paths::service_log_path().display()
            ));
        }
        std::thread::sleep(SERVICE_POLL_STEP);
    }
}

/// 在目录里找最新的 `probe-*.txt`。
///
/// 文件名里带 `YYYYMMDD-HHMMSS`，字典序**就是**时间序，故直接取最大者；
/// 不去查文件 mtime（那要平台 API，而这里只需要「最新的一份」）。
fn newest_report(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("probe-") || !name.ends_with("-service.txt") {
            continue;
        }
        let path = entry.path();
        if best.as_ref().is_none_or(|current| current < &path) {
            best = Some(path);
        }
    }
    best
}

// ── 日志 ─────────────────────────────────────────────────

/// 日志写文件（服务模式用）。
///
/// 落点是 `%ProgramData%\rust-tunnel\service.log`，**不是**阶段 0 那个
/// `C:\Users\Public\rust-tunnel-probe\`——后者是**全局可写**目录：本机任何用户都能
/// 预建 junction 把 SYSTEM 的写入重定向走，或直接读走日志。这里先建目录并加固
/// （幂等；`--install` 已经做过一次），失败也只告警——日志是辅助，装不上也要尽力写。
///
/// 失败**不致命**：所以这里只 `eprintln!` 一句（服务进程的 stderr 也是丢的，
/// 但至少控制台模式下手跑 `--run` 时能看见）。
fn init_file_logging() {
    let dir = paths::program_data_root();
    if let Err(err) = security::harden_directory(&dir) {
        eprintln!("加固日志目录 {} 失败：{err}", dir.display());
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::service_log_path())
    else {
        return;
    };
    // 服务进程没有 ANSI 控制台，颜色码会变成乱码写进日志。
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(true)
        .with_writer(std::sync::Mutex::new(file))
        .try_init();
}

/// 日志写控制台（交互式命令用）。
fn init_console_logging() {
    // 已经初始化过就静默跳过（`try_init` 返回 Err），不是错误。
    let _ = tracing_subscriber::fmt().with_target(false).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_report_needs_the_service_tag() {
        // 只认 `probe-*-service.txt`：子实例与对照组的报告都在同一个目录里，
        // 挑错了用户就会拿一份「默认桌面上注入成功」的报告当成闸门结论。
        let dir = std::env::temp_dir().join(format!("rt-probe-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "probe-20261017-090503-console.txt",
            "probe-20261017-090601-instance.txt",
            "probe-20261017-090602-service.txt",
            "probe-20261017-090700-service.bmp",
            "service.log",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let found = newest_report(&dir).unwrap();
        assert_eq!(
            found.file_name().unwrap().to_string_lossy(),
            "probe-20261017-090602-service.txt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newest_report_is_none_for_an_empty_or_missing_dir() {
        let missing = std::env::temp_dir().join("rt-probe-does-not-exist-xyz");
        assert!(newest_report(&missing).is_none());
    }

    #[test]
    fn stop_retries_stay_small_so_they_never_look_like_a_hang() {
        // 重试是为了跨过 START_PENDING 那个几毫秒的窗口，不该变成一段新的等待。
        let budget = STOP_RETRY_STEP * STOP_ATTEMPTS;
        assert!(
            budget <= Duration::from_secs(5),
            "重试预算 {budget:?} 太大了"
        );
        assert!(SERVICE_PROBE_TIMEOUT > budget);
    }

    #[test]
    fn service_probe_timeout_covers_both_waits() {
        // 服务侧先等控制台会话出现（开机时它比 LogonUI 先起），然后等子实例结束——
        // 而子实例自己还要等安全桌面（`CHILD_WAIT_TIMEOUT` 已经把那段算进去了）。
        // 两段相加必须落在控制台命令的等待上限之内，否则一次**正常**的锁屏测量会被
        // `--probe-in-service` 判成「卡住」——那正是用户会照着去 Ctrl+C 的假警报。
        let waits = probe::CONSOLE_SESSION_WAIT + probe::CHILD_WAIT_TIMEOUT;
        assert!(
            SERVICE_PROBE_TIMEOUT >= waits + Duration::from_secs(20),
            "服务探针上限 {SERVICE_PROBE_TIMEOUT:?} 盖不住两段等待 {waits:?} 加余量"
        );
    }

    #[test]
    fn the_service_image_path_arguments_are_a_compile_time_constant() {
        // 未转义的用户输入进 `ImagePath` 就是往 SYSTEM 服务的命令行里注入。这里钉住
        // 「参数只有一个字面量 `--run`」——`service::install` 不做转义，靠的就是这一点。
        assert_eq!(SERVICE_RUN_ARGS, ["--run"]);
        for arg in SERVICE_RUN_ARGS {
            assert!(!arg.contains(' '), "参数里不该有空格：{arg}");
            assert!(!arg.contains('"'), "参数里不该有引号：{arg}");
        }
    }

    #[test]
    fn the_idempotency_and_delete_codes_match_win32() {
        // 这三个码都是手抄的（windows-sys 未导出或本 crate 没开对应 feature，见各自常量上的
        // 注释），故把字面值钉住：1056 是 --start 的幂等判据，抄错会让「已经在跑」被报成失败。
        assert_eq!(ERROR_SERVICE_ALREADY_RUNNING, 1056);
        assert_eq!(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, 1063);
        assert_eq!(ERROR_SERVICE_MARKED_FOR_DELETE, 1072);
    }

    #[test]
    fn the_gui_exe_candidates_cover_both_archive_and_local_names() {
        // CI 的归档名与本地构建名不同；两个都认，否则用户下载后 --install 会找不到实例。
        assert!(GUI_EXE_CANDIDATES.contains(&"rust-tunnel-client-gui.exe"));
        assert!(GUI_EXE_CANDIDATES
            .contains(&"rust-tunnel-client-gui-windows-x86_64.exe"));
        for name in GUI_EXE_CANDIDATES {
            assert_eq!(
                Path::new(name).extension().and_then(|ext| ext.to_str()),
                Some("exe"),
                "{name}"
            );
            assert!(!name.contains(['\\', '/']), "只放文件名，不放路径：{name}");
        }
    }

    #[test]
    fn killing_waits_long_enough_to_avoid_two_instances() {
        // 杀掉实例后必须等它真的退出，否则下一次 spawn 会与它并存——而并存正是单实例锁
        // 与「服务端按名唯一」要防的。等太短等于没等。
        assert!(INSTANCE_EXIT_WAIT >= Duration::from_secs(5));
        // 收尾上限必须盖住「等实例退出」这一段，否则服务会先退出、留下没收干净的实例。
        assert!(SUPERVISOR_JOIN_WAIT > INSTANCE_EXIT_WAIT);
    }

    #[test]
    fn the_wait_slice_is_short_enough_to_notice_a_stop_request() {
        // 停止/重建请求只在切片边界上被看到；切片太长会让「停止」看起来像卡住。
        // 走 `Duration` 比较而不是直接比常量，免得被 `assertions_on_constants` 挑出来。
        assert!(Duration::from_millis(u64::from(INSTANCE_WAIT_SLICE_MS)) <= Duration::from_secs(1));
        assert!(SLEEP_SLICE <= Duration::from_millis(500));
    }
}
