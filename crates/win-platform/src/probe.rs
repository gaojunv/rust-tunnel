//! 真机探针：**阶段 0 的产物**（跑完出结论，不是特性）。
//!
//! # 为什么需要它
//!
//! 「锁屏界面上到底能不能注入」以及「SYSTEM 令牌能不能把进程拉进控制台会话」
//! 这两件事**没有任何本机替代品**：它们取决于真机的安全描述符、UIPI、会话隔离三者
//! 的实际组合。设计再对，也得有一份**可判定的证据**才能决定要不要把阶段 1 那几百行
//! 编排代码写下去。
//!
//! # 两个入口，缺一不可
//!
//! | 入口 | 谁在跑 | 在哪跑 | 回答什么 |
//! |---|---|---|---|
//! | [`run`] | 用户自己敲命令 / 服务拉起的**子实例** | 交互式会话 | 采集与注入在这条链路上对不对 |
//! | [`run_from_service`] | SCM 启动的服务进程 | 会话 0 / SYSTEM | 令牌两条路通不通、能不能把子实例拉进控制台会话 |
//!
//! 服务进程自己**做不了**安全桌面 I/O——它在会话 0，那里的窗口站跟控制台会话的
//! `WinSta0` 根本不是同一个。所以 [`run_from_service`] 的正事是：拿令牌 →
//! `CreateProcessAsUserW` 拉起自己（`--probe-instance`）→ 等它跑完 → 把它的报告
//! 嵌进自己的报告。**这条链就是阶段 1 的编排本身**，探针顺带把它验了。
//!
//! # 三种跑法（用户侧）
//!
//! | 跑法 | 命令 | 当时机器状态 | 这一趟回答什么 |
//! |---|---|---|---|
//! | 对照组 | `--probe` | 已登录、未锁屏 | 这套代码本身对不对（默认桌面上注入**应当**成功） |
//! | 锁屏 | `--probe-in-service` | 敲完命令后按 Win+L | **闸门**：Winlogon 桌面上注入落不落地 |
//! | 登录界面 | 装完重启，服务自启 | 开机停在登录界面 | 无人值守场景（此时无人能点「接入确认」） |
//!
//! 后两趟都**没法**「先锁屏再敲命令」——那时用户已经没有控制台了。所以顺序反过来：
//! 命令先跑起来，测试体自己等桌面变安全（见 [`ProbeOptions::wait_secure`]）；
//! 登录界面那一趟则靠服务是 `SERVICE_AUTO_START`：开机它比 LogonUI 先起来，
//! 所以它还得先等控制台会话出现（见 [`wait_for_console_session`]）。
//!
//! 对照组不可省：如果三趟都失败，没有对照组就分不清「Winlogon 特有问题」还是
//! 「我这段代码本来就错」。
//!
//! # 唯一的硬判据：光标真的动了
//!
//! `SendInput` 的返回值只能说「事件被收下了」，说不了「落到了这个桌面上」。
//! 故本探针在注入前后各读一次 `GetCursorPos`，并**轮询**等光标走到目标点：
//!
//! - 光标到位 → 注入**确实**作用在当前桌面（这是闸门的正面证据）；
//! - 返回值全接受但光标没动 → 事件被收下却没送到桌面（**最容易被误判成成功的形态**）；
//! - 返回值少于请求 → 被 UIPI/ACL 拦下（`GetLastError` 常常是 0，只有计数能看出来）。
//!
//! 报告里这三态分开写，因为它们指向**完全不同的后续动作**。
//!
//! # 落盘
//!
//! 报告与截图写到 [`ProbeOptions::out_dir`]（默认 [`default_out_dir`] =
//! `%ProgramData%\rust-tunnel\probe\`，DACL 收敛到 SYSTEM + Administrators）：
//! `probe-<时间戳>-<模式>.txt`、`shot-<时间戳>-<模式>.bmp`。
//! BMP 是**原始采集的 BGRA**（真值），ASCII 缩略图来自转换后的 RGBA——
//! 刻意不让两者共用一条通道转换，这样通道顺序的错误才会暴露而不是互相抵消。

use crate::inject::{self, InjectOutcome};
use crate::monitor::{Monitor, VirtualScreen};
use crate::report::ProbeReport;
use crate::session::ConsoleSession;
use crate::sysinfo::{local_now, windows_version, LocalTime};
use crate::WinError;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 默认落盘目录：[`crate::paths::probe_dir()`]（`%ProgramData%\rust-tunnel\probe`）。
///
/// 阶段 0 曾把报告与截图写在 `C:\Users\Public\rust-tunnel-probe\`——那是个**全局可写**
/// 目录：任何本机用户都能预建 junction 把 SYSTEM 的写入重定向到任意位置（junction
/// 不需要 `SeCreateSymbolicLinkPrivilege`），而报告里含**锁屏/登录界面的截图**。
/// 换到这里意味着「取日志不必提权」这个便利没有了——**现在读报告要管理员**。
/// 这可以接受：探针本身就需要管理员（见 `win-service` 的 USAGE），而且产物落在一个
/// 能证明只有 SYSTEM + Administrators 可写的目录里，比「随手能读」重要得多。
#[must_use]
pub fn default_out_dir() -> PathBuf {
    crate::paths::probe_dir()
}

/// 子实例的模式标签（进文件名）。
pub const TAG_INSTANCE: &str = "instance";
/// 服务进程自己的模式标签。
pub const TAG_SERVICE: &str = "service";

/// 等光标走到目标点的上限。
///
/// `SendInput` 把事件投进输入队列就返回了，桌面把它处理完是**之后**的事；
/// 立刻回读会读到旧位置，把一次成功的注入判成失败。200ms 的轮询上限在真机上
/// 远大于实际延迟（通常在个位数毫秒），而失败时也只多花 200ms。
const CURSOR_WAIT_TIMEOUT: Duration = Duration::from_millis(200);
/// 光标轮询步长。
const CURSOR_POLL_STEP: Duration = Duration::from_millis(10);
/// 光标判据的容差（像素）。
///
/// `SendInput` 收的是 `0..=65535` 的归一化坐标，由**内核**换算回像素，而 MSDN 只
/// 保证两个端点怎么映射、不规定中间的取整方式（[`crate::monitor::VirtualScreen::
/// normalize_absolute`] 的文档里记了两种常见写法，二者最多差 1 像素）。要求**逐
/// 像素相等**会把一次成功的注入判成「存疑」，而「存疑」在
/// `WINDOWS-SECURE-DESKTOP.md` 里正是「闸门未通过」——于是一个取整约定就能让整个
/// 阶段 0 得到错误结论。故判据放宽到 ±1 像素。
///
/// 放宽之后三态依然分得开：事件真的没送到时，光标停在原处，而扫动的目标点在屏幕
/// 横向 7/8 处，**不可能**落在原处 ±1 像素内。
const CURSOR_TOLERANCE_PX: i32 = 1;
/// 鼠标扫动时每一步之间的停顿：肉眼要能看到光标「走」过去，而不是瞬移。
const SWEEP_STEP_PAUSE: Duration = Duration::from_millis(60);
/// 服务等子实例结束的上限。
///
/// # 这个数**必须**盖住子实例自己的等待
///
/// 子实例会先等安全桌面（[`CHILD_SECURE_WAIT`]，60 秒——那是留给用户按 Win+L 的），
/// **然后**才开始采一帧、扫一遍鼠标、写两份文件。所以它的最坏耗时是
/// `CHILD_SECURE_WAIT + 探针耗时`，这里必须不小于它，否则会出现最难查的一种失败：
/// 用户在 35 秒时锁屏，子实例 37 秒做完一次**成功**的测量并写出报告，而父进程
/// 30 秒就放弃了 —— 报告里是「等待子实例 超时」+「子实例多半没跑起来」+
/// 「本次没测出结论」，一次成功的闸门测量被记成「什么都没测到」。
///
/// 90 秒 = 60 秒（等安全桌面）+ 30 秒（探针自身：8 步扫动 + 采集 + 落两块文件，
/// 真机上远用不到）。再往上加没有意义：控制台侧的上限见
/// `win-service` 的 `SERVICE_PROBE_TIMEOUT`，两边有单测钉着不等式。
pub const CHILD_WAIT_TIMEOUT: Duration = Duration::from_secs(90);
/// 服务侧等「控制台会话出现」的上限。
///
/// 服务是**自动启动**的（`SERVICE_AUTO_START`），开机时它往往比控制台会话
/// ——以及跑在里面的 LogonUI——先起来。那一刻还没有交互式会话，直接放弃的话，
/// 「开机停在登录界面」那一趟就白跑了。上限必须有：卡在等会话上，SCM 会把服务
/// 显示成无响应。
pub const CONSOLE_SESSION_WAIT: Duration = Duration::from_secs(60);
/// 子实例等「输入桌面变成安全桌面」的上限（见 [`ProbeOptions::wait_secure`]）。
///
/// 60 秒是给人按 Win+L 留的余量。它和 [`CONSOLE_SESSION_WAIT`] 相加必须明显小于
/// `--probe-in-service` 那个等待上限（见 `win-service` 的 `SERVICE_PROBE_TIMEOUT`），
/// 否则一次**正常**的锁屏测量会被控制台侧当成「卡住」。
pub const CHILD_SECURE_WAIT: Duration = Duration::from_secs(60);
/// 等会话/等桌面的轮询步长。
const WAIT_POLL_STEP: Duration = Duration::from_millis(500);

/// 探针参数。
#[derive(Debug, Clone)]
pub struct ProbeOptions {
    /// 报告与截图的落盘目录。
    pub out_dir: PathBuf,
    /// 鼠标扫动的步数（越大越慢、越容易肉眼确认）。
    pub sweep_steps: u32,
    /// 是否在屏幕左上角附近点一下（默认开：左上是**所有桌面**上都安全的位置——
    /// 桌面的左上角不会被任何按钮占据，最坏情况是把桌面图标选中）。
    pub click: bool,
    /// 是否发一个空格键（**默认关**，且命令行故意没有暴露开关——见 [`inject_and_observe`]
    /// 里那段说明：登录界面上密码框本来就聚焦，空格会直接写进密码框）。
    pub inject_keys: bool,
    /// 模式标签，进文件名与报告（`console` / `instance`）。
    pub tag: String,
    /// 报告文件名里的时间戳（`YYYYMMDD-HHMMSS`）；`None` = 用本进程启动时刻。
    ///
    /// # 为什么必须能由外部指定
    ///
    /// 服务侧要**在子实例跑之前**就算出它的报告路径（跑完才去读那份文件、嵌进
    /// 自己的报告）。若父进程用「我启动的时刻」、子进程用「它启动的时刻」，两个
    /// 值跨过秒边界就**对不上**——一次成功的测量会被报成「子实例没写出报告」。
    /// 所以父进程把自己的时间戳通过命令行交给子实例（`--probe-instance <stamp>`），
    /// 两边由此算出**同一个**文件名。
    pub stamp: Option<String>,
    /// 测之前先等「输入桌面」变成**安全桌面**（锁屏/登录界面）多久；`None` = 不等。
    ///
    /// # 为什么必须有这个等待
    ///
    /// 要测的正是机器**锁上之后**的行为，而那时用户已经没有控制台可以敲命令了
    /// ——所以只能反过来：命令先跑起来，测试体自己等到桌面变安全再动手
    /// （用户在预算内按 Win+L）。开机停在登录界面的那一趟同理：服务比 LogonUI
    /// 先起来，等一会儿桌面才是 Winlogon。
    ///
    /// 只有 `--probe-instance`（服务拉起的那条路）会设它：对照组的 `--probe`
    /// 就该**立刻**测当前桌面，等一等反而把「当时是不是安全桌面」这个对照信息弄丢。
    ///
    /// 超时**不算失败**：到点后照样测当前桌面，报告里会写明等了多久、期间见过哪些
    /// 桌面名（所以「用户没来得及锁」与「锁了但看不到 Winlogon」能区分开）。
    pub wait_secure: Option<Duration>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            out_dir: default_out_dir(),
            sweep_steps: 8,
            click: true,
            inject_keys: false,
            tag: String::from("console"),
            stamp: None,
            wait_secure: None,
        }
    }
}

impl ProbeOptions {
    /// 带上模式标签的构造。
    #[must_use]
    pub fn with_tag(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            ..Self::default()
        }
    }

    /// 本次用的时间戳：显式给了就用它，否则取本进程启动时刻。
    #[must_use]
    pub fn stamp_or(&self, started: LocalTime) -> String {
        self.stamp.clone().unwrap_or_else(|| started.file_stamp())
    }
}

/// 探针的产物。
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    /// 报告（已渲染好的文本与结构化条目都在里面）。
    pub report: ProbeReport,
    /// 报告文件路径。
    pub report_path: PathBuf,
    /// 截图路径（采集失败时为 `None`）。
    pub screenshot_path: Option<PathBuf>,
    /// 判定结论：安全桌面上的注入是否**确实生效**。
    ///
    /// `None` = 本次没跑到安全桌面（对照组，或者采集/挂载就已经失败）。
    pub secure_desktop_injection_worked: Option<bool>,
}

/// 在**任何写入之前**把输出目录（及其父目录）的 DACL 收敛到 SYSTEM + Administrators。
///
/// 必须在采集**之前**调用：截图是锁屏/登录界面的画面，写进全局可写目录就等于把登录
/// 界面公开；而且任何本机用户都能预建 junction 把 SYSTEM 的写入重定向走（见
/// [`default_out_dir`]）。**父目录也要加固**——只加固叶子、而父目录
/// （`%ProgramData%\rust-tunnel`）保留继承来的宽松 DACL 的话，非管理员仍能改名或替换
/// 整个探针目录。两者都幂等（见 `security::harden_directory`）。
///
/// # Errors
///
/// 加固失败即整体失败（fail-closed）：宁可不写报告，也不把敏感截图落到一个没能证明
/// 安全的目录里。这与 [`run`]/[`run_from_service`] 的「只有写不出去才 `Err`」一致
/// ——拒绝写就是写不出去。
fn harden_output_dir(out_dir: &Path) -> crate::WinResult<()> {
    crate::security::harden_directory(&crate::paths::program_data_root())?;
    crate::security::harden_directory(out_dir)?;
    Ok(())
}

/// 跑一遍**本地**探针（在调用线程上）。`--probe` 与 `--probe-instance` 都走它。
///
/// # Errors
///
/// 只有「报告写不出去」会成为 `Err`：单步失败全部记进报告，
/// 因为**探针的价值就在于把失败也带回来**（多半我们现在还没权限失败）。
pub fn run(options: &ProbeOptions) -> crate::WinResult<ProbeOutcome> {
    // 先加固再采集：截图必须在「已证明只有 SYSTEM + Administrators 可写」的目录里产生。
    harden_output_dir(&options.out_dir)?;
    let started = local_now();
    let mut report = ProbeReport::new("rust-tunnel Windows 无人值守探针");

    collect_environment(&mut report, started, options);
    collect_self_token(&mut report);
    let monitors = collect_monitors(&mut report);

    // 桌面这一段必须在专用线程里：`SetThreadDesktop` 要求线程不拥有任何窗口，
    // 且**绝不能**在会被复用的池线程上调用（见 `crate::desktop` 的硬约束）。
    let stamp = options.stamp_or(started);
    let desktop = run_desktop_thread(options, &monitors, &stamp);
    merge_desktop_report(&mut report, &desktop.report);

    collect_token_paths(&mut report);
    write_verdict(&mut report, desktop.injection_on_secure_desktop, options);

    let report_path = report_path_for(options, started);
    write_report(&report, &report_path)?;

    Ok(ProbeOutcome {
        report,
        report_path,
        screenshot_path: desktop.screenshot,
        secure_desktop_injection_worked: desktop.injection_on_secure_desktop,
    })
}

/// 服务进程里的探针（**这条链就是阶段 1 的编排**）。
///
/// 做的事，按顺序：
/// 1. 采一份服务自己的环境事实（会话 0、SYSTEM 令牌）；
/// 2. 记录控制台会话 id（拉子实例必须以它为目标）；
/// 3. 跑两条取令牌路径（自持 + `TokenSessionId`，以及复制 `winlogon.exe` 的主令牌），
///    并**如实记下各自成败**；
/// 4. 用**能用的那条**令牌 `CreateProcessAsUserW` 拉起自己（`--probe-instance`）；
/// 5. 等子实例退出，把它的报告**嵌进**本报告（这样只需贴一份文件回去）；
/// 6. 若子实例没跑起来/没写出报告，把原因记清楚——那本身就是最重要的结论。
///
/// `child_options` 描述**子实例**怎么跑（`tag` 会被改写成 [`TAG_INSTANCE`]）；
/// 父报告固定用 [`TAG_SERVICE`] 标签。
///
/// # Errors
///
/// 只有「报告写不出去」会成为 `Err`（理由同 [`run`]）。
pub fn run_from_service(child_options: &ProbeOptions) -> crate::WinResult<ProbeOutcome> {
    // 先加固再拉起子实例：服务侧报告与子实例产物都落在这个目录里。
    harden_output_dir(&child_options.out_dir)?;
    let started = local_now();
    let mut report = ProbeReport::new("rust-tunnel Windows 无人值守探针（服务侧）");

    let parent_options = ProbeOptions {
        tag: String::from(TAG_SERVICE),
        ..child_options.clone()
    };
    collect_environment(&mut report, started, &parent_options);
    collect_self_token(&mut report);

    report.section("服务为什么做不了安全桌面 I/O");
    report.field(
        "会话",
        "本进程在会话 0（服务会话）：那里的窗口站是 Service-0x0-3e7$，\
         跟控制台会话的 WinSta0 不是同一个，故挂不到用户看到的任何桌面上",
    );
    report.field(
        "所以",
        "真正的采集/注入由**子实例**在控制台会话里做，下面就是把它拉起来的过程",
    );

    let verdict = spawn_instance_and_collect(&mut report, child_options, &started);

    let report_path = report_path_for(&parent_options, started);
    write_report(&report, &report_path)?;

    Ok(ProbeOutcome {
        report,
        report_path,
        screenshot_path: None,
        secure_desktop_injection_worked: verdict,
    })
}

/// 落盘 + 同时打一份到标准输出。
///
/// stdout 那一份是给「在命令行里跑」的：用户能直接看到，不必先去翻文件。
/// 服务模式/子实例模式下 stdout 会被丢弃（`CREATE_NO_WINDOW`），那时只有文件。
///
/// # Errors
///
/// 目录建不出来或文件写不进去时返回 [`WinError`]。
fn write_report(report: &ProbeReport, path: &Path) -> crate::WinResult<()> {
    let rendered = report.render();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| {
        tracing::error!(dir = %dir.display(), error = %e, "创建探针输出目录失败");
        WinError::new("CreateDirectory", 0)
    })?;
    std::fs::write(path, rendered.as_bytes()).map_err(|e| {
        tracing::error!(path = %path.display(), error = %e, "写探针报告失败");
        WinError::new("WriteFile", 0)
    })?;
    println!("{rendered}");
    println!("报告已写入: {}", path.display());
    Ok(())
}

/// 把桌面专用线程的报告并入主报告。
///
/// 用「标题行之外的正文整体嵌进去」而不是结构化合并：子报告本身就是给人读的，
/// 嵌进去比拆成条目更清楚。去掉子报告自己的首行横幅，避免重复。
fn merge_desktop_report(main: &mut ProbeReport, sub: &ProbeReport) {
    let mut body = String::new();
    for line in sub.render().lines() {
        if line.starts_with("=====") {
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    main.block("安全桌面 I/O（专用线程）", body.trim_start().to_owned());
}

/// 第 0 段：不需要任何权限就能拿到的环境事实。
fn collect_environment(report: &mut ProbeReport, started: LocalTime, options: &ProbeOptions) {
    report.section("环境");
    report.field("时间", started.render());
    report.field("探针模式", &options.tag);
    report.field("输出目录", options.out_dir.display());
    report.field("系统版本", windows_version().render());

    // SAFETY: 无参数、无副作用。
    let pid = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() };
    report.field("本进程 id", pid);
    report.step(
        "本进程会话（ProcessIdToSessionId）",
        &crate::session::process_session_id(pid),
    );

    let console = crate::session::active_console_session();
    report.field(
        "活动控制台会话（WTSGetActiveConsoleSessionId）",
        describe_console(console),
    );
    report.field("原始值", console.raw());
    report.field(
        "解释",
        "服务拉起的子实例，其令牌会话必须等于上面这个值，否则它的桌面不是用户看到的那个",
    );
}

/// 控制台会话的单行描述（`None` 那一支给出「没有会话」的确切含义）。
fn describe_console(console: ConsoleSession) -> String {
    match console {
        ConsoleSession::Active(id) => format!("{id}"),
        ConsoleSession::Inactive => String::from("无（0xFFFFFFFF = 当前没人登录到控制台）"),
    }
}

/// 第 1 段：自身令牌（决定「能不能被 Winlogon 桌面的 ACL 放行」）。
fn collect_self_token(report: &mut ProbeReport) {
    report.section("自身令牌");
    let token = match crate::token::current_process_token() {
        Ok(t) => t,
        Err(err) => {
            report.field("OpenProcessToken", format!("失败 {err}"));
            return;
        }
    };
    report.field("OpenProcessToken", "成功");

    let sids = crate::token::token_sid_strings(&token);
    let user_sid = sids.first().cloned().unwrap_or_default();
    report.field("用户 SID", &user_sid);
    report.field("是否 LocalSystem（S-1-5-18）", user_sid == "S-1-5-18");
    report.field("SID 总数", sids.len());
    report.block("令牌 SID 列表", sids.join("\n"));

    let integrity = crate::token::token_integrity_level(&token);
    report.step("完整性级别", &integrity);
    report.field(
        "能否注入 LogonUI（UIPI 预判）",
        match &integrity {
            Ok(level) if level.can_inject_into(crate::token::IntegrityLevel::System) => {
                String::from("能 —— 达到 System，UIPI 这一层过了")
            }
            Ok(level) => format!(
                "不能（当前 {}）—— 低于 System，UIPI 会拦下；\
                 「以管理员身份运行」正是这种情况（High < System）",
                level.as_str()
            ),
            Err(err) => format!("无法判定（{err}）"),
        },
    );
    report.step(
        "已提权（TokenElevation）",
        &crate::token::token_is_elevated(&token),
    );
    report.step("令牌会话 id", &crate::token::token_session_id(&token));
}

/// 第 2 段：显示器与虚拟桌面几何（注入的归一化坐标靠它）。
fn collect_monitors(report: &mut ProbeReport) -> Vec<Monitor> {
    report.section("显示器");
    let monitors = match crate::monitor::enumerate_monitors() {
        Ok(m) => m,
        Err(err) => {
            report.field("EnumDisplayMonitors", format!("失败 {err}"));
            return Vec::new();
        }
    };
    report.field("显示器数量", monitors.len());
    if monitors.is_empty() {
        report.field(
            "注意",
            "一台都没枚举到 —— 在会话 0 里这是**预期**的（服务会话没有显示设备），\
             在控制台会话里则说明会话还没接上显示",
        );
    }
    for m in &monitors {
        report.field(
            format!("显示器 {}", m.id),
            format!(
                "{} {}x{} @ ({},{}){}",
                m.device,
                m.width,
                m.height,
                m.origin_x,
                m.origin_y,
                if m.primary { " [主]" } else { "" }
            ),
        );
    }
    match VirtualScreen::from_monitors(&monitors) {
        Some(vs) => {
            report.field(
                "虚拟桌面",
                format!(
                    "{}x{} @ ({},{})",
                    vs.width, vs.height, vs.origin_x, vs.origin_y
                ),
            );
        }
        None => {
            report.field("虚拟桌面", "无显示器（注入实验会跳过）");
        }
    }
    monitors
}

/// 专用线程的产物。
struct DesktopProbeResult {
    report: ProbeReport,
    screenshot: Option<PathBuf>,
    /// 只在**安全桌面**上注入时的结论（对照组与失败路径都是 `None`）。
    injection_on_secure_desktop: Option<bool>,
}

/// 在**专用线程**上做「切桌面 → 采集 → 注入」。
///
/// 线程是**一次性**的：跑完就退出。`SetThreadDesktop` 挂过一次之后不能再换桌面，
/// 复用线程会偶发失败且极难排查（见 [`crate::desktop`] 的硬约束 2、3）。
///
/// 线程 panic 只让这一段缺失，不会把整个探针带走——报告里会如实写「线程崩了」。
fn run_desktop_thread(
    options: &ProbeOptions,
    monitors: &[Monitor],
    stamp: &str,
) -> DesktopProbeResult {
    let options = options.clone();
    let monitors = monitors.to_vec();
    let stamp = stamp.to_owned();
    let spawned = std::thread::Builder::new()
        .name(String::from("secure-desktop-probe"))
        .spawn(move || desktop_probe_body(&options, &monitors, &stamp));

    let incomplete = |what: &str| DesktopProbeResult {
        report: {
            let mut r = ProbeReport::new("安全桌面 I/O");
            r.field("专用线程", what);
            r
        },
        screenshot: None,
        injection_on_secure_desktop: None,
    };

    match spawned {
        Ok(handle) => match handle.join() {
            Ok(result) => result,
            Err(_) => incomplete("panic（这一段不完整）"),
        },
        Err(err) => incomplete(&format!("创建失败 {err}")),
    }
}

/// 专用线程的主体。
fn desktop_probe_body(
    options: &ProbeOptions,
    monitors: &[Monitor],
    stamp: &str,
) -> DesktopProbeResult {
    let mut report = ProbeReport::new("安全桌面 I/O");

    // 要测的是「机器锁上之后」的行为，而那时用户已经没法敲命令了——所以反过来说：
    // 命令先跑起来，这里等到桌面变安全再动手（见 [`ProbeOptions::wait_secure`]）。
    if let Some(budget) = options.wait_secure {
        report.field("等待安全桌面", wait_for_secure_desktop(budget));
    }

    // 挂之前先看线程自己在哪个桌面——与「输入桌面」对照的关键读数。
    report.step(
        "GetThreadDesktop（挂载前）",
        &crate::desktop::current_thread_desktop_name(),
    );

    let mut desktop = match crate::desktop::InputDesktop::open() {
        Ok(d) => {
            report.field("OpenInputDesktop", "成功");
            report.field("输入桌面名", d.name());
            report.field("输入桌面种类", d.kind().as_str());
            report.field("是否安全桌面", d.kind().is_secure());
            d
        }
        Err(err) => {
            report.field("OpenInputDesktop", format!("失败 {err}"));
            report.field(
                "最可能的原因",
                "非 SYSTEM 进程在锁屏时看不到 Winlogon 桌面（错误码 5 = 拒绝访问）。\
                 若错误码是 5 而当时**没有**锁屏，那说明当前会话的窗口站本身就没接上",
            );
            return DesktopProbeResult {
                report,
                screenshot: None,
                injection_on_secure_desktop: None,
            };
        }
    };

    let input_secure = desktop.kind().is_secure();
    let attach = desktop.attach_to_current_thread();
    report.step("SetThreadDesktop", &attach);
    let after_attach = crate::desktop::current_thread_desktop_name();
    report.step("GetThreadDesktop（挂载后）", &after_attach);
    // **真的挂上去了吗**：两个条件都要——`SetThreadDesktop` 返回成功，**且**挂载后
    // 读出来的线程桌面名确实就是输入桌面。少了第二个条件，这个字段就只是把「输入
    // 桌面名」重抄一遍，读起来像一条已验证的断言，其实什么都没验。
    let on_input_desktop = match (&attach, &after_attach) {
        (Ok(()), Ok(name)) => name.eq_ignore_ascii_case(desktop.name()),
        _ => false,
    };
    report.field(
        "挂载后是否等于输入桌面",
        if on_input_desktop { "是" } else { "**否**" },
    );

    // 闸门判据只认「线程**真的**在安全桌面上」：输入桌面是安全桌面，**且**挂载成功。
    //
    // 这里必须 fail-closed。挂载失败时线程还留在创建它的那个桌面上（本进程的
    // `lpDesktop` 是 `winsta0\default`），于是采集读的是默认桌面、`SendInput` 投的
    // 也是默认桌面、`GetCursorPos` 读的还是默认桌面的光标 —— 整套读数**自洽且成功**，
    // 唯独回答的不是「安全桌面上能不能注入」这个问题。照旧往下走就会在这个项目
    // 唯一必须 fail-closed 的地方 fail-open。
    let secure = input_secure && on_input_desktop;

    let screenshot = capture_and_write(options, monitors, stamp, &mut report, secure);

    if !on_input_desktop {
        // 采集照做（它证明的是 GDI 与通道顺序，与在哪个桌面上无关），
        // **注入不做**：那才是闸门问题，在错误的桌面上做出来的读数只会误导人。
        report.field(
            "注入",
            "跳过：线程没能挂到输入桌面上（见上面 `SetThreadDesktop` / `挂载后是否等于\
             输入桌面` 两行）。在**别的**桌面上注入会得到一份自洽但无意义的成功读数，\
             所以这里刻意不做，本次也没有闸门结论",
        );
        report.field(
            "最可能的原因",
            "输入桌面不是本线程所在的窗口站（`OpenInputDesktop` 成功只说明**打开**了\
             句柄）；或 `SetThreadDesktop` 撞上 ERROR_BUSY / 拒绝访问。\
             注意它挂载前不能拥有任何窗口或钩子——本探针的专用线程满足这一点",
        );
        return DesktopProbeResult {
            report,
            screenshot,
            injection_on_secure_desktop: None,
        };
    }

    let verdict = inject_and_observe(options, monitors, &mut report, secure);

    DesktopProbeResult {
        report,
        screenshot,
        injection_on_secure_desktop: if secure { verdict } else { None },
    }
}

/// 采集一帧、落盘 BMP、并在报告里放一张 ASCII 缩略图。
fn capture_and_write(
    options: &ProbeOptions,
    monitors: &[Monitor],
    stamp: &str,
    report: &mut ProbeReport,
    secure: bool,
) -> Option<PathBuf> {
    let frame = match crate::capture::capture_primary() {
        Ok(f) => f,
        Err(err) => {
            report.field("BitBlt 采集", format!("失败 {err}"));
            return None;
        }
    };
    report.field(
        "BitBlt 采集",
        format!(
            "成功 {}x{}（{} 字节）",
            frame.width,
            frame.height,
            frame.bgra.len()
        ),
    );
    report.field("平均亮度", frame.average_luma());
    report.field(
        "亮度怎么读",
        "锁屏界面**不是**全黑；接近 0 通常意味着 BitBlt 采到了空桌面（线程其实在别的桌面上）",
    );
    if monitors.len() > 1 {
        report.field(
            "多显示器提示",
            "本探针只采主显示器；逐屏采集请用 capture_monitor(monitor)（阶段 1 会做）",
        );
    }

    // ASCII 缩略图来自**转换后**的 RGBA（人眼看的这一份）。
    report.block(
        "缩略图（转换后 RGBA；`@` 亮、` ` 暗）",
        crate::pixels::ascii_thumbnail(&frame.rgba(), frame.width, frame.height, 72, 22),
    );

    // BMP 来自**原始采集**的 BGRA——刻意不经转换，通道顺序错了才看得出来。
    let path = options.out_dir.join(format!(
        "shot-{stamp}-{}-{}.bmp",
        options.tag,
        if secure { "secure" } else { "default" }
    ));
    if let Err(err) = std::fs::create_dir_all(&options.out_dir) {
        report.field("建输出目录", format!("失败 {err}"));
        return None;
    }
    let bmp = crate::pixels::encode_bmp(&frame.bgra, frame.width, frame.height);
    match std::fs::write(&path, bmp) {
        Ok(()) => {
            report.field("截图已写入", path.display());
            report.field(
                "这张图怎么用",
                "打开它：画面是**正的**说明采集的行序对（采集侧 biHeight 取负 = 自顶向下，\
                 BMP 编码又把行序倒回去）；颜色对说明 BGRA 没被当成 RGBA",
            );
            Some(path)
        }
        Err(err) => {
            report.field("写截图", format!("失败 {err}"));
            None
        }
    }
}

/// 做注入实验并在报告里记下**三态判据**。
///
/// 返回值 = 本桌面注入是否确实生效（`None` 表示压根没跑成，如拿不到几何）。
fn inject_and_observe(
    options: &ProbeOptions,
    monitors: &[Monitor],
    report: &mut ProbeReport,
    secure: bool,
) -> Option<bool> {
    report.section(if secure {
        "注入实验（安全桌面：这是闸门）"
    } else {
        "注入对照（默认桌面：这里应当成功）"
    });

    let Some(vs) = VirtualScreen::from_monitors(monitors) else {
        report.field("注入", "跳过：拿不到虚拟桌面几何");
        return None;
    };

    let before = cursor_position();
    report.field("注入前光标", format_cursor(before));

    let steps = inject::plan_sweep(&vs, options.sweep_steps);
    let target = steps.iter().rev().find_map(|s| match s {
        inject::InjectStep::MoveTo { x, y } => Some((*x, *y)),
        _ => None,
    });
    report.field(
        "扫动规划",
        format!("{} 步，目标点 {}", steps.len(), format_cursor(target)),
    );

    // **逐步**发送（规划是步骤的列表，每一步单独一次 `SendInput`）：
    // 这样步与步之间能插入停顿，肉眼才看得见光标「走」过去。
    let mut outcome = InjectOutcome::default();
    for (index, step) in steps.iter().enumerate() {
        match inject::send_steps(std::slice::from_ref(step), &vs) {
            Ok(one) => {
                outcome.steps_attempted += one.steps_attempted;
                outcome.steps_accepted += one.steps_accepted;
                outcome.inputs_attempted += one.inputs_attempted;
                outcome.inputs_accepted += one.inputs_accepted;
                if let Some(failure) = one.first_failure {
                    outcome.record_failure(index, failure.error);
                    break;
                }
            }
            Err(err) => {
                report.field("SendInput", format!("失败 {err}"));
                return None;
            }
        }
        std::thread::sleep(SWEEP_STEP_PAUSE);
    }
    report.field("SendInput（鼠标扫动）", outcome.summary());

    let reached = target.is_some_and(|point| wait_for_cursor(point, CURSOR_WAIT_TIMEOUT));
    let after = cursor_position();
    report.field("注入后光标", format_cursor(after));

    // 「注入**前**光标就已经在目标点上」时，这次鼠标判据**什么都证明不了**：
    // 哪怕事件全被拦下，位置也照样「到达」——一个假阳性。`before` 就是为这一步记的。
    let was_there = matches!(
        (before, target),
        (Some(before), Some(target)) if near(before, target, CURSOR_TOLERANCE_PX)
    );
    let moved = reached && !was_there;
    report.field(
        format!("光标是否到达目标点（±{CURSOR_TOLERANCE_PX} 像素）"),
        moved,
    );
    if was_there {
        report.field(
            "⚠ 本次鼠标判据无效",
            "注入**前**光标就已经落在目标点上（±1 像素内），所以「到达」不能说明任何事。\
             **本次的注入结论不要信**：把光标挪到别处再跑一遍",
        );
    }

    if options.click {
        let click_at = (vs.origin_x.saturating_add(5), vs.origin_y.saturating_add(5));
        let click_steps = inject::plan_click(click_at.0, click_at.1, inject::InjectButton::Left);
        // `field` 返回 `&mut ProbeReport`（链式写法），这里当语句用，故每臂加块收尾。
        match inject::send_steps(&click_steps, &vs) {
            Ok(clicked) => {
                report.field(
                    // 这里的坐标是**请求**的点，不是读数——与上面「注入后光标」那行
                    // 的渲染一模一样，不标清楚的话，读者会把两者一对比得出
                    // 「光标根本没到点击点」的错误结论。
                    "SendInput（点击左上角；坐标为请求值）",
                    format!("{} @ {}", clicked.summary(), format_cursor(Some(click_at))),
                );
            }
            Err(err) => {
                report.field("SendInput（点击）", format!("失败 {err}"));
            }
        }
    }

    if options.inject_keys {
        match inject::send_steps(&inject::plan_key_press(inject::VK_SPACE), &vs) {
            Ok(keys) => {
                report.field("SendInput（空格键）", keys.summary());
            }
            Err(err) => {
                report.field("SendInput（空格键）", format!("失败 {err}"));
            }
        }
    } else {
        report.field(
            "空格键实验",
            "未做。默认关闭是有意的：在登录界面上密码框本来就聚焦，空格会**直接写进\
             密码框**，把密码弄脏成「末尾多一个空格」这种极难自查的形态。\
             阶段 0 的判据是鼠标扫动 + 光标确实到位，不需要它",
        );
    }

    // 三态判据：把「返回值」与「光标真的动了」分开写，它们指向不同的后续动作。
    let fully = outcome.fully_accepted();
    let verdict = fully && moved;
    report.field("本桌面注入结论", verdict_line(fully, moved));
    Some(verdict)
}

/// 注入结论那一行。
///
/// **必须是唯一一处**写这几个字符串的地方：[`child_verdict`] 靠逐字匹配它，
/// 有单测把两者钉在一起。
fn verdict_line(fully_accepted: bool, cursor_moved: bool) -> &'static str {
    if fully_accepted && cursor_moved {
        "生效（事件全被接受，且光标确实到达目标点）"
    } else if fully_accepted {
        "存疑：事件全被接受，但光标没动 —— 事件被收下却没送到这个桌面"
    } else {
        "失败：SendInput 未接受全部事件（多为 UIPI/桌面 ACL 拦下；\
         注意这种形态下 GetLastError 常常是 0，只有计数能看出来）"
    }
}

/// 第 3 段：两条取令牌路径的对比（阶段 1 走哪条由这里的真机结果决定）。
fn collect_token_paths(report: &mut ProbeReport) {
    report.section("把实例拉进控制台会话的两条路");
    let Some(session) = crate::session::active_console_session().id() else {
        report.field(
            "两条路都跳过",
            "当前没有交互式控制台会话（没人登录到控制台）",
        );
        return;
    };
    report.field("目标会话", session);

    match crate::token::current_process_token() {
        Ok(token) => {
            report.field("A: 取自身令牌", "成功");
            // 特权清单与服务侧共用（`process::INSTANCE_LAUNCH_PRIVILEGES`）：少一个就是
            // 1314，而那个码看起来像「权限不够」，容易被误判成别的原因。
            // `enable_launch_privileges` 返回的是**没真正启用**的项（含「持有但没被赋予」，
            // 那类在 `AdjustTokenPrivileges` 上返回 TRUE、只有 last-error 能看出来）——
            // 故这里逐项渲染，别把它压成「全部成功」。
            let failures = crate::process::enable_launch_privileges(&token);
            report.field(
                "A: 启用 CreateProcessAsUser 的三个特权",
                if failures.is_empty() {
                    format!("全部成功（{}）", crate::process::INSTANCE_LAUNCH_PRIVILEGES.join("、"))
                } else {
                    failures
                        .iter()
                        .map(|(name, outcome)| crate::process::privilege_enable_text(name, outcome))
                        .collect::<Vec<_>>()
                        .join("；")
                },
            );
            report.step(
                "A: DuplicateTokenEx + TokenSessionId → 会话 id",
                &crate::token::duplicate_primary_for_session(&token, session)
                    .and_then(|t| crate::token::token_session_id(&t)),
            );
            report.field(
                "A: 说明",
                "不依赖任何别的进程。失败点通常是令牌缺 SeTcbPrivilege 的**启用**\
                 （SYSTEM 默认持有但未必启用，故上面先启用了它）",
            );
        }
        Err(err) => {
            report.field("A: 取自身令牌", format!("失败 {err}"));
        }
    }

    match crate::process::winlogon_primary_token(Some(session)) {
        Ok(token) => {
            report.field("B: 复制 winlogon.exe 的主令牌", "成功");
            report.step("B: 会话 id", &crate::token::token_session_id(&token));
            report.step(
                "B: 完整性级别",
                &crate::token::token_integrity_level(&token),
            );
            report.field(
                "B: 说明",
                "winlogon 本来就在控制台会话里，故不需要改 TokenSessionId；\
                 找不到它就是纯 RDP 会话（那种场景 v1 不在范围内）",
            );
        }
        Err(err) => {
            report.field("B: 复制 winlogon.exe 的主令牌", format!("失败 {err}"));
        }
    }
}

/// 等服务出现交互式控制台会话（最多 `budget`）。
///
/// 为什么必须等而不是「没有就放弃」：服务是自动启动的，开机时它常常比控制台会话
/// 先起来；**开机停在登录界面**那一趟正是无人值守要覆盖的场景，一上来就放弃等于
/// 白跑。但也不能无限等——服务卡在等会话上，SCM 会把它显示成无响应。
fn wait_for_console_session(report: &mut ProbeReport, budget: Duration) -> Option<u32> {
    let deadline = Instant::now() + budget;
    loop {
        let console = crate::session::active_console_session();
        if let Some(id) = console.id() {
            report.field("目标会话（等到了）", id);
            return Some(id);
        }
        if Instant::now() >= deadline {
            report.field(
                "跳过",
                format!(
                    "等了 {} 秒仍然没有控制台会话（WTSGetActiveConsoleSessionId 一直是 0xFFFFFFFF）。\
                     这**不是**失败——但探针需要有人在控制台才能验注入，\
                     请确认机器已经起到登录界面",
                    budget.as_secs()
                ),
            );
            return None;
        }
        std::thread::sleep(WAIT_POLL_STEP);
    }
}

/// 等**输入桌面变成安全桌面**（锁屏/登录界面），最多 `budget`；返回一句给人读的描述。
///
/// 返回描述而不是 `Result`：超时是**有信息量的结果**（「用户没来得及锁」），
/// 不是失败——后面照样测当前桌面，报告里对比着看。
fn wait_for_secure_desktop(budget: Duration) -> String {
    let started = Instant::now();
    let deadline = started + budget;
    // 只记「变过的」桌面名：锁屏过程中可能短暂经过几个桌面，逐个记下来才看得出轨迹。
    let mut seen: Vec<String> = Vec::new();
    loop {
        match crate::desktop::InputDesktop::open() {
            Ok(desktop) => {
                let label = format!("{}（{}）", desktop.name(), desktop.kind().as_str());
                if seen.last() != Some(&label) {
                    seen.push(label);
                }
                if desktop.kind().is_secure() {
                    return format!(
                        "已等到安全桌面（等了 {} ms）；期间见过：{}",
                        started.elapsed().as_millis(),
                        seen.join(" → ")
                    );
                }
            }
            Err(err) => {
                // 打开失败也是「变过」的一种状态（例如桌面正在切换），记下来继续等。
                let label = format!("（打不开：{err}）");
                if seen.last() != Some(&label) {
                    seen.push(label);
                }
            }
        }
        if Instant::now() >= deadline {
            return format!(
                "等满 {} 秒也没变成安全桌面——**到点照样测**，所以本次测的是当前桌面；\
                 期间见过：{}。若你本来就打算测锁屏，说明 Win+L 没来得及（或没按）",
                budget.as_secs(),
                seen.join(" → ")
            );
        }
        std::thread::sleep(WAIT_POLL_STEP);
    }
}

/// 服务侧：拉起子实例，等它结束，把它的报告嵌进来。
///
/// 返回值 = 子实例在安全桌面上的注入结论（从它报告的结论行里刮出来；拿不到就是 `None`）。
fn spawn_instance_and_collect(
    report: &mut ProbeReport,
    child_options: &ProbeOptions,
    started: &LocalTime,
) -> Option<bool> {
    report.section("拉起子实例（阶段 1 的编排，在这里先验一遍）");

    let session = wait_for_console_session(report, CONSOLE_SESSION_WAIT)?;

    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            report.field("current_exe", format!("失败 {err}"));
            return None;
        }
    };
    report.field("子实例 exe", exe.display());

    // 子实例的报告路径由**父进程**先算好，再把时间戳用命令行交给它（见
    // [`ProbeOptions::stamp`]）：两段各算各的「当前时刻」会跨秒对不上，
    // 一次成功的测量就会被报成「子实例没写出报告」。
    let stamp = started.file_stamp();
    let child = ProbeOptions {
        tag: String::from(TAG_INSTANCE),
        stamp: Some(stamp.clone()),
        // 子实例的输出目录只能是默认值：命令行没有传目录的入口，
        // 它自己的 `ProbeOptions` 来自 `ProbeOptions::default()`。跟着父进程的
        // `out_dir` 走会去一个空目录里找报告，把成功的测量报成「没跑起来」。
        out_dir: default_out_dir(),
        ..child_options.clone()
    };
    let command_line = crate::process::build_command_line(
        &exe.to_string_lossy(),
        &[String::from("--probe-instance"), stamp],
    );
    report.field("子实例命令行", &command_line);

    let token = report_and_pick_instance_token(report, session)?;
    report.step(
        "所选令牌的会话 id（必须等于目标会话）",
        &crate::token::token_session_id(&token),
    );

    // 「句柄一关就把里面的进程全杀掉」的作业对象。它管的是**子实例被放弃**那种情况：
    // 等超时（[`CHILD_WAIT_TIMEOUT`]）后父进程会继续往下走，若没有作业对象兜底，
    // 那个子实例会一直活到它自己跑完——用户那边 `--probe-in-service` 早就返回了，
    // 屏幕上却过一会儿又动一次鼠标。本函数返回时 `job` 被 drop，作业里剩下的进程
    // 由系统回收。
    let job = match crate::process::create_kill_on_close_job() {
        Ok(job) => {
            report.field("作业对象", "已建（父进程一关，实例自动被回收）");
            Some(job)
        }
        Err(err) => {
            // 不是致命错误：没有它也能测量，只是「放弃的子实例」没人收。
            report.field("作业对象", format!("建不起来 {err}（不影响本次测量）"));
            None
        }
    };

    let spawned = match crate::process::spawn_with_token(&token, &command_line, None) {
        Ok(p) => {
            report.field("CreateProcessAsUserW", format!("成功（pid {}）", p.pid));
            p
        }
        Err(err) => {
            report.field("CreateProcessAsUserW", format!("失败 {err}"));
            report.field(
                "常见错误码",
                "1314 = ERROR_PRIVILEGE_NOT_HELD（令牌不是主令牌，或缺 \
                 SeAssignPrimaryToken/SeIncreaseQuota）；5 = 拒绝访问",
            );
            return None;
        }
    };

    if let Some(job) = &job {
        // `field` 返回 `&mut ProbeReport`，这里当语句用，故每臂加块收尾。
        match crate::process::assign_to_job(job, &spawned.process) {
            Ok(()) => {
                report.field("放进作业对象", "成功");
            }
            Err(err) => {
                report.field(
                    "放进作业对象",
                    format!("失败 {err}（父进程被放弃时子实例不会被自动回收）"),
                );
            }
        }
    }

    let wait_ms = u32::try_from(CHILD_WAIT_TIMEOUT.as_millis()).unwrap_or(30_000);
    match crate::process::wait_for_process(&spawned.process, wait_ms) {
        Ok(true) => {
            report.field("等待子实例", "已退出");
        }
        Ok(false) => {
            report.field(
                "等待子实例",
                format!("超时（{wait_ms} ms）后放弃 —— 子实例可能卡在别处"),
            );
        }
        Err(err) => {
            report.field("等待子实例", format!("失败 {err}"));
        }
    }
    report.step("子实例退出码", &crate::process::exit_code(&spawned.process));

    // 子实例的报告嵌进来：用户只需贴**一份**文件回去。
    // 目录见上面构造 `child` 时的说明（固定为默认目录）。
    let child_path = report_path_for(&child, *started);
    report.field("子实例报告路径", child_path.display());
    match std::fs::read_to_string(&child_path) {
        Ok(text) => {
            let verdict = child_verdict(&text);
            report.block("子实例报告（原样嵌入）", text);
            verdict
        }
        Err(err) => {
            report.field(
                "读子实例报告",
                format!("失败 {err} —— 子实例多半没跑起来，或者它写到别处去了"),
            );
            None
        }
    }
}

/// 取实例令牌并把两条路的成败写进报告（`None` = 两条都不通）。
///
/// 取令牌用的是**与服务侧共用**的那份实现（[`crate::process::pick_instance_token`]）：
/// 两份实现必然漂移，而这条链是无人值守被控端能不能起来的关键路径。
fn report_and_pick_instance_token(
    report: &mut ProbeReport,
    session: u32,
) -> Option<crate::handle::OwnedHandle> {
    let picked = crate::process::pick_instance_token(session);
    for (name, outcome) in &picked.privilege_failures {
        // 逐项渲染：`NotAssigned`（持有但没生效）会被渲染成「未生效(ERROR_NOT_ALL_ASSIGNED)」，
        // 而不是被压成「全部成功」——那正是「CreateProcessAsUserW 报 1314 却看不出原因」
        // 这条排障死路的来源。
        report.field(
            "启用特权",
            crate::process::privilege_enable_text(name, outcome),
        );
    }
    if let Some(err) = &picked.winlogon_error {
        report.field("B 路不可用", format!("{err}"));
    }
    if let Some(err) = &picked.own_token_error {
        report.field("A 路也不可用", format!("{err}"));
    }
    let (Some(token), Some(source)) = (picked.token, picked.source) else {
        report.field("取令牌", "两条路都不可用（见上面两条失败原因）");
        return None;
    };
    report.field("取令牌", source.as_str());
    Some(token)
}

/// 从子实例的报告文本里刮出注入结论那行。
///
/// 刻意用「找那一行」而不是给 `ProbeReport` 加结构化字段：报告是给人读的，
/// 这里只是顺手少让用户翻一次文件；刮不到就返回 `None`，**绝不猜**。
///
/// # 为什么必须先看「是否安全桌面」
///
/// 结论行是在**每个**桌面上都会写的：子实例等安全桌面**等超时**时会「到点照样测」，
/// 于是在默认桌面上注入——那当然成功——并写下 `本桌面注入结论: 生效（…）`。
/// 只匹配那一行的话，「Win+L 没来得及按」或「开机时 LogonUI 还没起来」就会得到一份
/// 「生效 —— 闸门通过」的假报告，而闸门结论正是本阶段唯一的交付物：
/// 拿一份**从没碰过 Winlogon** 的测量去给登录页那一半放行，是这个项目最坏的一种错。
/// 故先要求子实例自己记的「是否安全桌面」为 `true`，否则返回 `None`（＝没结论）。
fn child_verdict(text: &str) -> Option<bool> {
    let secure = text.lines().find(|l| l.starts_with("是否安全桌面:"))?;
    if secure.split_once(':').map(|(_, value)| value.trim()) != Some("true") {
        return None;
    }
    let line = text.lines().find(|l| l.starts_with("本桌面注入结论:"))?;
    if line.contains("生效（") {
        Some(true)
    } else {
        Some(false)
    }
}

/// 结尾：给出结论与「下一步该看什么」，让贴回来的日志自带解读。
fn write_verdict(report: &mut ProbeReport, verdict: Option<bool>, options: &ProbeOptions) {
    report.section("结论");
    match verdict {
        Some(true) => {
            report.field(
                "安全桌面注入",
                "生效 —— 闸门通过：Winlogon 桌面上注入确实落地，登录页/锁屏那一半可用",
            );
        }
        Some(false) => {
            report.field(
                "安全桌面注入",
                "未生效 —— **闸门未通过**：登录页/锁屏那一半不能声称可用。把本报告贴回去一起看，\
                 重点是「输入桌面种类」「SendInput（鼠标扫动）」「光标是否到达目标点」三行",
            );
        }
        None => {
            report.field(
                "安全桌面注入",
                format!(
                    "本次没测出结论（模式 {}）—— 锁屏或停在登录界面时再跑一遍 \
                     --probe-in-service 才有闸门结论",
                    options.tag
                ),
            );
        }
    }
    report.field(
        "还需要人工确认的两件事",
        "① 扫动时**肉眼**看到光标在屏幕上走过去了没有（这是判据的最终背书）；\
         ② 在锁屏/登录界面上，这次注入有没有让界面产生可见反应（密码框出现字符、\
         桌面图标被选中）。探针只能证明事件进了 desktop，界面反应才是最终目的",
    );
}

/// 读当前光标位置（虚拟桌面坐标）。失败返回 `None`。
fn cursor_position() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: point 是本栈帧上的可写 POINT；API 只写入、不持有。
    let ok = unsafe { GetCursorPos(&raw mut point) };
    if ok == 0 {
        return None;
    }
    Some((point.x, point.y))
}

/// 两个光标坐标是否在 `tolerance` 像素内（含）。
fn near(a: (i32, i32), b: (i32, i32), tolerance: i32) -> bool {
    (a.0 - b.0).abs() <= tolerance && (a.1 - b.1).abs() <= tolerance
}

/// 轮询等光标走到 `target` 的 ±[`CURSOR_TOLERANCE_PX`] 像素内（最多 `timeout`）。
///
/// 光标的移动是**异步**的：事件投进输入队列后 `SendInput` 就返回了，立刻回读会读到
/// 旧位置。故这里轮询而不是 sleep 一个固定值——成功时立刻返回，失败时也不必猜延迟。
///
/// **为什么不要求逐像素相等**：见 [`CURSOR_TOLERANCE_PX`]。归一化坐标由内核换算回
/// 像素，取整约定不同就差 1 像素，而「差 1 像素」会被判成「事件没送到这个桌面」。
fn wait_for_cursor(target: (i32, i32), timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cursor_position().is_some_and(|pos| near(pos, target, CURSOR_TOLERANCE_PX)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(CURSOR_POLL_STEP);
    }
}

/// `Option<(i32, i32)>` 的单行渲染（`None` 写成 `(读不到)`）。
fn format_cursor(pos: Option<(i32, i32)>) -> String {
    match pos {
        Some((x, y)) => format!("({x}, {y})"),
        None => String::from("(读不到)"),
    }
}

/// 报告文件的路径。
///
/// `started` 只在 [`ProbeOptions::stamp`] 为 `None` 时参与命名——显式给了戳就用它
/// （服务侧靠这一点预先算出子实例的路径，见 [`ProbeOptions::stamp`]）。
#[must_use]
pub fn report_path_for(options: &ProbeOptions, started: LocalTime) -> PathBuf {
    options.out_dir.join(format!(
        "probe-{}-{}.txt",
        options.stamp_or(started),
        options.tag
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_conservative() {
        let options = ProbeOptions::default();
        // 空格键默认必须是关的：锁屏时它可能弄脏用户正在输入的密码。
        assert!(!options.inject_keys);
        assert!(options.click, "左上角的点击是安全的，默认开着");
        assert!(options.sweep_steps >= 2, "至少两点才看得出「在动」");
        // 默认落盘目录必须是**规范落点**（`%ProgramData%\rust-tunnel\probe`），而不是
        // 相对 CWD 的路径：服务/实例的工作目录不可控，相对路径会落到莫名其妙的地方。
        // 真机上 `ProgramData` 一定存在，故那时还要求它是绝对路径；macOS 上环境变量
        // 缺席、两份默认值都退化成相对路径，只能断言「是同一个落点」。
        assert_eq!(options.out_dir, crate::paths::probe_dir());
        assert!(options.out_dir.ends_with("probe"), "{}", options.out_dir.display());
        if std::env::var_os("ProgramData").is_some() {
            assert!(options.out_dir.is_absolute(), "{}", options.out_dir.display());
        }
        // 对照组**不等**桌面：等一等就把「当时到底在哪个桌面」这个对照信息弄丢了。
        assert!(options.wait_secure.is_none());
    }

    /// 父进程的等待上限必须盖住子实例**自己的**等待，否则「用户在 35 秒时锁屏」
    /// 这种正常操作会得到一份「子实例没跑起来」+「本次没测出结论」的报告——
    /// 一次成功的闸门测量被记成什么都没测到。
    #[test]
    fn the_parent_outlasts_the_childs_own_secure_wait() {
        assert!(
            CHILD_WAIT_TIMEOUT > CHILD_SECURE_WAIT,
            "父进程等待 {CHILD_WAIT_TIMEOUT:?} 盖不住子实例等安全桌面的 \
             {CHILD_SECURE_WAIT:?}：子实例还没等到安全桌面，父进程就已经放弃了"
        );
        // 除了等桌面，子实例还要采一帧、扫 8 步、落两块文件，故不能只多出一点点。
        let margin = CHILD_WAIT_TIMEOUT.saturating_sub(CHILD_SECURE_WAIT);
        assert!(
            margin >= Duration::from_secs(20),
            "只留给探针自己 {margin:?}，慢机器上会不够"
        );
    }

    /// 光标判据的容差是**有意**放宽的，不是随手写的常数。
    #[test]
    fn the_cursor_tolerance_survives_the_normalized_round_trip() {
        // 1920×1080、扫动终点 (1680, 540)：归一化再换回来，两种常见取整写法
        // 一个落在 1679/539、一个落在 1681，都可能与目标差 1 像素。
        let vs = crate::monitor::VirtualScreen {
            origin_x: 0,
            origin_y: 0,
            width: 1920,
            height: 1080,
        };
        let (tail_x, tail_y) = (1680, 540);
        let (nx, ny) = vs.normalize_absolute(tail_x, tail_y);
        assert_eq!((nx, ny), (57373, 32797), "归一化公式变了，容差要重新核");
        // 判据必须接受「差 1 像素」，同时仍然拒绝「差更多」。
        assert!(near((tail_x - 1, tail_y - 1), (tail_x, tail_y), 1));
        assert!(near((tail_x + 1, tail_y), (tail_x, tail_y), 1));
        assert!(!near((tail_x - 2, tail_y), (tail_x, tail_y), 1));
        // 事件根本没送到时，光标停在原处，离目标点至少半个屏幕——分得很开。
        assert!(!near((0, 0), (tail_x, tail_y), 1));
    }

    #[test]
    fn with_tag_only_changes_the_tag() {
        let options = ProbeOptions::with_tag("service");
        let default = ProbeOptions::default();
        assert_eq!(options.tag, "service");
        assert_eq!(options.sweep_steps, default.sweep_steps);
        assert_eq!(options.click, default.click);
        assert_eq!(options.inject_keys, default.inject_keys);
        assert_eq!(options.out_dir, default.out_dir);
    }

    #[test]
    fn cursor_rendering_marks_an_unreadable_position() {
        assert_eq!(format_cursor(Some((10, -20))), "(10, -20)");
        assert_eq!(format_cursor(None), "(读不到)");
    }

    #[test]
    fn report_path_carries_the_stamp_and_tag() {
        let options = ProbeOptions::with_tag("service");
        let path = report_path_for(&options, sample_time());
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        assert_eq!(name, "probe-20261017-090503-service.txt");
    }

    /// 显式给了戳就必须用它——服务侧正是靠这一点**预设**子实例的路径。
    ///
    /// 这条单测守的是「跨秒边界」那个坑：若父进程用自己的启动时刻、子进程用它的，
    /// 两边秒数不同就会各写各的文件，父进程读完报「子实例没写出报告」，
    /// 一次**成功**的测量被报成失败，而报告里还找不到原因。
    #[test]
    fn an_explicit_stamp_beats_the_start_time() {
        let options = ProbeOptions {
            stamp: Some(String::from("20260101-000001")),
            ..ProbeOptions::with_tag(TAG_INSTANCE)
        };
        let path = report_path_for(&options, sample_time());
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        assert_eq!(name, "probe-20260101-000001-instance.txt");
    }

    #[test]
    fn without_an_explicit_stamp_the_start_time_is_used() {
        let options = ProbeOptions::with_tag(TAG_INSTANCE);
        assert_eq!(options.stamp_or(sample_time()), "20261017-090503");
    }

    #[test]
    fn console_description_covers_both_shapes() {
        assert_eq!(describe_console(ConsoleSession::Active(3)), "3");
        assert!(describe_console(ConsoleSession::Inactive).contains("无"));
    }

    #[test]
    fn merge_drops_the_sub_banner_and_keeps_its_body() {
        let mut main = ProbeReport::new("主");
        let mut sub = ProbeReport::new("子标题");
        sub.section("段").field("键", "值");
        merge_desktop_report(&mut main, &sub);
        let text = main.render();
        assert!(!text.contains("子标题"), "子报告的横幅不该重复出现：{text}");
        assert!(text.contains("--- 安全桌面 I/O（专用线程） ---"), "{text}");
        assert!(text.contains("键: 值"), "{text}");
    }

    /// 三态判据的字符串必须与 [`child_verdict`] 的匹配**逐字**一致，
    /// 否则服务侧会把「生效」认成「没生效」。这条单测把两者钉在一起。
    #[test]
    fn child_verdict_round_trips_every_verdict_line() {
        for (fully, moved, expected) in [
            (true, true, Some(true)),
            (true, false, Some(false)),
            (false, false, Some(false)),
        ] {
            let mut report = ProbeReport::new("t");
            // 只有真的在安全桌面上测出来的结论才算闸门结论，故这个字段必须一起写。
            report.field("是否安全桌面", true);
            report.field("本桌面注入结论", verdict_line(fully, moved));
            assert_eq!(
                child_verdict(&report.render()),
                expected,
                "fully={fully} moved={moved}"
            );
        }
    }

    /// **闸门那条假阳性必须锁死**：子实例等安全桌面没等到时会「到点照样测」，
    /// 于是在**默认**桌面上注入——那当然成功——并写下 `本桌面注入结论: 生效（…）`，
    /// 同时在报告里把「是否安全桌面」如实记成 `false`。父进程若只刮结论行，
    /// 「Win+L 没来得及按」就会得到一份「生效 —— 闸门通过」的假报告。
    #[test]
    fn a_default_desktop_success_is_never_read_as_a_gate_pass() {
        let mut report = ProbeReport::new("t");
        report.field("是否安全桌面", false);
        report.field("本桌面注入结论", verdict_line(true, true));
        let text = report.render();
        assert!(text.contains("生效（"), "这份报告确实自称生效：{text}");
        assert_eq!(
            child_verdict(&text),
            None,
            "默认桌面上的成功绝不能当成闸门通过"
        );
    }

    #[test]
    fn child_verdict_returns_none_when_the_line_is_absent() {
        assert_eq!(child_verdict("什么都没有"), None);
        assert_eq!(child_verdict(""), None);
        // 这个字段没读到也不能猜：宁可没结论，也不要一个凭空的闸门通过。
        assert_eq!(child_verdict("本桌面注入结论: 生效（事件全被接受）"), None);
        assert_eq!(child_verdict("是否安全桌面: true"), None);
    }

    fn sample_time() -> LocalTime {
        LocalTime {
            year: 2026,
            month: 10,
            day: 17,
            hour: 9,
            minute: 5,
            second: 3,
            millis: 0,
        }
    }
}
