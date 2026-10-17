//! Windows 上的主流程（**本文件是整个 bin 里唯一 Windows 专属的部分**）。
//!
//! 为什么要把 Windows 代码集中在一个文件：`main.rs` 在 macOS/Linux 上也要能编译
//! （workspace 的 `cargo build` 会构建全部成员），所以平台分支只留一个 `#[cfg]`
//! 出口，其余全是普通 Rust。
//!
//! # 两个身份
//!
//! | 身份 | 启动者 | 命令 | 它能做什么 |
//! |---|---|---|---|
//! | 控制台工具 | 人（要管理员） | `--probe` / `--probe-in-service` / `--uninstall` | 动 SCM、跑对照探针 |
//! | 服务进程 | SCM（SYSTEM / 会话 0） | `--run [--probe]` | 拿令牌、把子实例拉进控制台会话 |
//! | 子实例 | 服务（`CreateProcessAsUser`） | `--probe-instance` | 在**目标桌面**上采集与注入 |
//!
//! 阶段 0 只有这些；阶段 1 会把「子实例」换成 `rust-tunnel-client-gui.exe --instance`
//! （那才是真正承载客户端身份、跑网络与引擎的那个进程）。

use crate::args::{self, Command};
use rust_tunnel_win_platform::probe::{self, ProbeOptions, ProbeOutcome};
use rust_tunnel_win_platform::service::{self, ServiceEvent, ServiceState};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`：`StartServiceCtrlDispatcherW` 在
/// 「不是被 SCM 启动」时给的就是它。用它把「手工跑了 `--run`」这条常见错误
/// 翻译成一句人话（码本身是诊断信息，别丢）。
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: u32 = 1063;

/// `--run` 是否跑探针。由 [`run_service`] 在 `service::run` **之前**设定，
/// 由 [`on_service_event`] 读——事件回调只能是 `fn` 指针（没有地方挂捕获环境）。
static PROBE_MODE: AtomicBool = AtomicBool::new(false);

/// 等 `--probe-in-service` 那个服务自己停下来的上限。
///
/// 探针的实际耗时是「采一帧 + 扫 8 步 × 60ms + 等子实例（上限 30s）」，40 秒都算紧的；
/// 给 180 秒是为了让**慢机器/慢磁盘**不至于被误判成卡死。真卡住时用户直接 Ctrl+C，
/// 服务还留着，用 `--uninstall` 收。
const SERVICE_PROBE_TIMEOUT: Duration = Duration::from_secs(180);
/// 轮询服务状态的步长。
const SERVICE_POLL_STEP: Duration = Duration::from_millis(500);
/// 请 SCM 停服务时的重试次数（见 [`request_stop`]：要跨过 `START_PENDING` 那个窗口）。
const STOP_ATTEMPTS: u32 = 5;
/// 每次重试之间的间隔。
const STOP_RETRY_STEP: Duration = Duration::from_millis(500);

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
    // 而「连报告都没写出来」正是最需要日志的时候。其余模式有控制台，就写控制台。
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
        Command::Run { probe: false } => Err(String::from(
            "服务本体属于阶段 1，现在还没有：阶段 0 的服务只会跑探针。\
             请用 `--probe-in-service`（它会带着 --run --probe 装好并启动服务）。",
        )),
        Command::Run { probe: true } => run_service(),
        Command::ProbeInService => probe_in_service(),
        Command::Install => Err(String::from(
            "阶段 0 暂不提供 `--install`：服务本体（阶段 1）没落地前，装一个什么都不做的\
             常驻服务没有意义。现在请用 `--probe-in-service`，它自己会装、会起、会停。",
        )),
        Command::Uninstall => service::uninstall()
            .map(|()| println!("服务已卸载（若它本来就不在，这也算成功）。"))
            .map_err(|err| {
                format!("卸载失败：{err}\n（SCM 相关的操作需要管理员权限；错误码 5 = 拒绝访问）")
            }),
    }
}

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
            probe::DEFAULT_OUT_DIR
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

/// `--run --probe`：接上 SCM，等它把探针跑完再自己停下。
fn run_service() -> Result<(), String> {
    PROBE_MODE.store(true, Ordering::SeqCst);
    service::run(service::SERVICE_NAME, on_service_event).map_err(|err| {
        if err.code() == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT {
            format!(
                "StartServiceCtrlDispatcherW 报 1063：本进程不是被 SCM 启动的。\
                 换用 `--probe-in-service`——它会先把服务装好再让 SCM 来启动。\
                 （原始错误：{err}）"
            )
        } else {
            err.to_string()
        }
    })
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
            // 阶段 1 会在这里拉起实例；阶段 0 走不到（`--run` 不带 `--probe` 被拒）。
            tracing::warn!("服务已启动，但本次没有要做的事");
        }
        ServiceEvent::Stop => tracing::info!("SCM 要求停止"),
        ServiceEvent::SessionChange {
            event_type,
            session_id,
        } => {
            // 阶段 1 的触发器：会话变了就重启实例。阶段 0 只记下来——
            // 探针恰恰要在**当前**桌面上做，跟着切反而会跑偏。
            tracing::debug!(event_type, session_id, "会话变化（阶段 0 不响应）");
        }
        ServiceEvent::Other { control } => tracing::debug!(control, "其它服务控制码"),
    }
}

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
fn probe_in_service() -> Result<(), String> {
    let state = service::query_state().map_err(|err| {
        format!("查询服务状态失败：{err}\n（SCM 相关的操作需要管理员权限；错误码 5 = 拒绝访问）")
    })?;

    match state {
        ServiceState::NotInstalled => {
            let exe = std::env::current_exe()
                .map_err(|err| format!("拿不到本程序的路径，没法注册服务：{err}"))?;
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

    let dir = PathBuf::from(probe::DEFAULT_OUT_DIR);
    if let Some(path) = newest_report(&dir) {
        println!("服务已跑完并自行停止。报告：\n  {}", path.display());
        println!("（同一个目录里还有 shot-*.bmp 截图；服务自己的日志在同目录的 service.log）");
    } else {
        println!(
            "服务已停止，但 {} 下没找到 probe-*-service.txt。",
            dir.display()
        );
        println!("看同目录的 service.log；若文件都不存在，说明连目录都没建起来（很可能是权限）。");
    }
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
                 它可能卡在别处：看 {}\\service.log，然后 `--uninstall` 收尾。",
                probe::DEFAULT_OUT_DIR
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

/// 日志写文件（服务模式用）。
///
/// 失败**不致命**：日志是辅助，探针报告才是产物。所以这里只 `eprintln!` 一句
/// （服务进程的 stderr 也是丢的，但至少控制台模式下手跑 `--run` 时能看见）。
fn init_file_logging() {
    let dir = PathBuf::from(probe::DEFAULT_OUT_DIR);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("service.log"))
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
}
