//! 进程：命令行拼接（可移植）+ 定位进程 / 取令牌 / 在指定会话里拉起进程（Windows FFI）。
//!
//! # 这一层要解决的唯一问题
//!
//! 「服务在会话 0，用户在会话 1+」这条会话隔离，只能靠**替目标进程拿一个会话正确的
//! 令牌**来解决。两条路，探针两条都试、报告里都记：
//!
//! | 路 | 做法 | 什么时候会失败 |
//! |---|---|---|
//! | A（自持） | [`crate::token::current_process_token`] → `DuplicateTokenEx` → `SetTokenInformation(TokenSessionId)` | 令牌缺 `SeTcbPrivilege` 的**启用**时会失败（SYSTEM 默认持有但未必启用，故要先 [`crate::token::enable_privilege`]） |
//! | B（借道） | 复制**`winlogon.exe` 的令牌**（它本来就在控制台会话里、且是 SYSTEM） | 拿不到 winlogon 的进程句柄（权限不足），或目标会话里没有 winlogon（纯 RDP 会话） |
//!
//! B 路更稳（RustDesk/TeamViewer 都是这么做的），A 路不需要依赖任何别的进程。
//! 探针把两条都跑一遍，就是为了**用真机数据**决定阶段 1 走哪条。
//!
//! # 作业对象（job object）
//!
//! 服务把实例放进一个 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的作业里：服务进程一死，
//! 作业句柄关闭，**实例被系统自动杀掉**。这是「服务停了但实例还在后台跑」这类
//! 僵尸状态的兜底（显式 `TerminateProcess` 也行，但那要求服务活着走到那一步）。

// 注意：与 monitor.rs/token.rs 同理——`quote_arg`/`build_command_line` 是可移植的
// （本模块**不是**整模块 cfg(windows)），故顶层不能 `use crate::WinResult`
// （在 macOS 上会变成未使用导入）。FFI 函数的签名里写全路径。

/// 按 Windows 的 `CommandLineToArgvW` 规则给一个参数加引号。
///
/// 规则（与 MSDN 的 `CommandLineToArgvW` 文档一致）：
/// - 不含空格/TAB/引号时**原样返回**——包括以反斜杠结尾的情况：裸命令行里反斜杠
///   只在**引号前**才有转义意义，`C:\dir\` 不带引号传过去，解析回来一模一样。
///   （这一点容易「顺手修坏」：加引号本身不是错，但会与 std 的 `Command::arg`
///   行为不一致，而我们的实例命令行要和它对齐。）
/// - 否则用 `"` 包起来，内部的 `"` 用 `\"` 转义；
/// - **反斜杠的加倍只发生在「引号之前」**：引号前的连续反斜杠要加倍，
///   收尾的连续反斜杠也要加倍（否则会把我们加的收尾引号转义掉）。
///   这是这段逻辑唯一容易写错的地方，有单测。
#[must_use]
pub fn quote_arg(arg: &str) -> String {
    let needs_quoting = arg.is_empty()
        || arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\n' || c == '\u{b}');
    if !needs_quoting {
        return arg.to_string();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
            }
            '"' => {
                // 引号前的反斜杠要加倍，然后转义引号本身。
                for _ in 0..backslashes {
                    out.push('\\');
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            other => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(other);
            }
        }
    }
    // 收尾的反斜杠必须加倍：后面紧跟的是我们加的收尾引号。
    for _ in 0..backslashes {
        out.push('\\');
        out.push('\\');
    }
    out.push('"');
    out
}

/// 把命令与参数拼成一条 `CreateProcess` 用的命令行。
///
/// 第一个元素按惯例是 exe 路径（**也会加引号**——路径带空格而不加引号是
/// 经典的提权路径劫持面）。
#[must_use]
pub fn build_command_line(program: &str, args: &[String]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(args.len() + 1);
    parts.push(quote_arg(program));
    for arg in args {
        parts.push(quote_arg(arg));
    }
    parts.join(" ")
}

/// 按映像名（如 `winlogon.exe`）找进程 id，可能不止一个（多会话各一个）。
///
/// # Errors
///
/// 建立快照失败时返回 [`crate::WinError`]。**枚举中途结束**（`Process32NextW`
/// 返回 0）是正常终止，不算错误。
#[cfg(windows)]
pub fn find_process_ids_by_name(name: &str) -> crate::WinResult<Vec<u32>> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: 快照句柄不需要权限；th32ProcessID 传 0（忽略）是文档规定的用法。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return Err(crate::error::last_error("CreateToolhelp32Snapshot"));
    }
    let snapshot = crate::handle::OwnedHandle::from_raw(snapshot);

    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
        ..PROCESSENTRY32W::default()
    };
    let mut out = Vec::new();
    // SAFETY: entry 是本栈帧上的结构且 dwSize 已按文档设置；快照句柄有效。
    let mut ok = unsafe { Process32FirstW(snapshot.raw(), &raw mut entry) };
    while ok != 0 {
        if crate::wide::from_wide(&entry.szExeFile).eq_ignore_ascii_case(name) {
            out.push(entry.th32ProcessID);
        }
        entry.dwSize = u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0);
        // SAFETY: 同上一处；entry 是同一个本栈帧结构。
        ok = unsafe { Process32NextW(snapshot.raw(), &raw mut entry) };
    }
    Ok(out)
}

/// 打开一个进程并取其令牌。
///
/// 用 `PROCESS_QUERY_LIMITED_INFORMATION` 而不是 `PROCESS_QUERY_INFORMATION`：
/// 前者在**跨完整性级别**时也能成功（Vista 起的「有限查询」权限），
/// 后者会被 UIPI 相关检查挡住。取令牌本身只要求查询权限。
///
/// # Errors
///
/// 进程不存在、或权限不足时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn open_process_token(pid: u32) -> crate::WinResult<crate::handle::OwnedHandle> {
    use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: bInheritHandle = 0；pid 由调用方给出。返回值交给 OwnedHandle 关闭。
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    let process = crate::handle::require_handle("OpenProcess", process)?;
    let process = crate::handle::OwnedHandle::from_raw(process);

    let mut token = std::ptr::null_mut();
    // SAFETY: process 是有效的进程句柄且权限足够；token 是本栈帧上的 HANDLE。
    let ok =
        unsafe { OpenProcessToken(process.raw(), TOKEN_QUERY | TOKEN_DUPLICATE, &raw mut token) };
    crate::error::check_bool("OpenProcessToken", ok)?;
    let token = crate::handle::require_handle("OpenProcessToken", token)?;
    Ok(crate::handle::OwnedHandle::from_raw(token))
}

/// 找一个**在该会话里**跑的 `winlogon.exe`，复制它的主令牌。
///
/// `session == None` 表示「任意会话里第一个 winlogon」（探针用；阶段 1 必须给出
/// 明确的控制台会话 id，否则可能拿到别的会话的令牌）。
///
/// # Errors
///
/// 找不到进程、或复制令牌失败时返回 [`crate::WinError`]。调用方应当**据此回退**
/// 到 [`crate::token::duplicate_primary_for_session`] 那条自持路径。
#[cfg(windows)]
pub fn winlogon_primary_token(
    session: Option<u32>,
) -> crate::WinResult<crate::handle::OwnedHandle> {
    let pids = find_process_ids_by_name("winlogon.exe")?;
    let pid = match session {
        Some(want) => pids
            .into_iter()
            .find(|pid| crate::session::process_session_id(*pid).is_ok_and(|s| s == want)),
        None => pids.into_iter().next(),
    };
    let pid = pid.ok_or_else(|| crate::WinError::new("find winlogon.exe", 0))?;

    let token = open_process_token(pid)?;
    // winlogon 本来就在目标会话里，故这里**不需要**再改会话 id；
    // 传它自己的会话 id 是幂等的（也能顺带验证 SetTokenInformation 这条路通不通）。
    let session = crate::token::token_session_id(&token).unwrap_or(0);
    crate::token::duplicate_primary_for_session(&token, session)
}

/// `CreateProcessAsUserW` 要求**调用方**持有的三个特权。
///
/// 为什么是这三个（都开在**调用方自己**的令牌上，见 [`enable_launch_privileges`]）：
/// - `SeTcbPrivilege`：`SetTokenInformation(TokenSessionId)` 改令牌会话 id 要用它；
/// - `SeAssignPrimaryTokenPrivilege`：`CreateProcessAsUser` 换主令牌要用它；
/// - `SeIncreaseQuotaPrivilege`：给新进程分配配额要用它。
///
/// SYSTEM 默认**持有**这三个但**不启用**——不显式启用的话，`CreateProcessAsUserW` 会以
/// 1314（`ERROR_PRIVILEGE_NOT_HELD`）失败，那个码在日志里长得像「权限不够」，
/// 其实是这里少做了一步。
pub const INSTANCE_LAUNCH_PRIVILEGES: [&str; 3] = [
    "SeTcbPrivilege",
    "SeAssignPrimaryTokenPrivilege",
    "SeIncreaseQuotaPrivilege",
];

/// 启用一个特权的结果（[`crate::token::enable_privilege`] 的 `WinResult<bool>` 归类）。
///
/// **三态而不是两态**：`AdjustTokenPrivileges` 在「特权在令牌里**持有但没被赋予**」时
/// 返回 `TRUE` 并把 last-error 置成 `ERROR_NOT_ALL_ASSIGNED`(1300)——那**不是**成功，
/// `CreateProcessAsUserW` 随后会以 1314（`ERROR_PRIVILEGE_NOT_HELD`）失败。若把它压进
/// `Err`/`Ok` 两态（先前 [`enable_launch_privileges`] 只收 `Err` 就是这么做的），
/// 「持有但没生效」会被**静默当成成功**：探针报告写「全部成功」、服务日志一声不吭，
/// 而真机上的表现是 `CreateProcessAsUserW` 报 1314——排障的人会去别处找原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegeEnable {
    /// 确实启用了（`AdjustTokenPrivileges` 返回 TRUE 且 last-error 是 `ERROR_SUCCESS`）。
    Enabled,
    /// 特权**持有但没被赋予**（`AdjustTokenPrivileges` 返回 TRUE，last-error 是
    /// `ERROR_NOT_ALL_ASSIGNED` = 1300）。
    NotAssigned,
    /// 调用本身失败（名字无效，或 `AdjustTokenPrivileges` 直接失败）。
    Failed(crate::WinError),
}

impl PrivilegeEnable {
    /// 是否**真的**启用了（**只有** [`PrivilegeEnable::Enabled`] 是 `true`）。
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// 把 `enable_privilege` 的 `WinResult<bool>` 归类成 [`PrivilegeEnable`]。
///
/// **纯函数、可移植**，故 macOS 上也能单测——而这条归类正是那处回归的核心：
/// `Ok(false)` 是「持有但没生效」，绝不能当成成功。
#[must_use]
pub fn classify_privilege_enable(result: crate::WinResult<bool>) -> PrivilegeEnable {
    match result {
        Ok(true) => PrivilegeEnable::Enabled,
        Ok(false) => PrivilegeEnable::NotAssigned,
        Err(err) => PrivilegeEnable::Failed(err),
    }
}

/// 把一条「启用特权」结果渲染成报告/日志用的一行文本（**可移植**，有单测）。
///
/// `NotAssigned` 用**和失败不同的词**（`未生效(ERROR_NOT_ALL_ASSIGNED)`）：
/// 它和「调用失败」是两种不同的处置（前者是令牌策略问题，后者是 API/名字问题），
/// 混成一句话就会把排障引到错的方向。
#[must_use]
pub fn privilege_enable_text(name: &str, outcome: &PrivilegeEnable) -> String {
    match outcome {
        PrivilegeEnable::Enabled => format!("{name}: 成功"),
        PrivilegeEnable::NotAssigned => format!("{name}: 未生效(ERROR_NOT_ALL_ASSIGNED)"),
        PrivilegeEnable::Failed(err) => format!("{name}: 失败 {err}"),
    }
}

/// 在**调用方**的令牌上启用 [`INSTANCE_LAUNCH_PRIVILEGES`]，返回**没真正启用**的项。
///
/// # 为什么必须在「取令牌」之前、且只开在调用方自己的令牌上
///
/// `CreateProcessAsUserW` 检查的是**调用方**（也就是服务进程自己）的令牌，不是被拉起的
/// 那个令牌。原先把这段启用逻辑写在 A 路（自持令牌）里，于是 B 路（复制 `winlogon.exe`
/// 的主令牌）一成功，这三行就**一次都没跑过**——而 B 路恰恰是更常用的那条。抽成共用实现
/// 时保留这个语义：两条取令牌路径之前都要先调它。
///
/// # 返回值为什么是 [`PrivilegeEnable`] 而不是 `WinError`
///
/// 「返回启用失败的项」这句话有个陷阱：**没启用**不等于**调用失败**。
/// `AdjustTokenPrivileges` 在特权「持有但没被赋予」时返回 `TRUE`、只把 last-error 置成
/// 1300——那是 `Ok(false)`，不是 `Err`。早先这里只收 `Err`，于是这一类被静默当成成功
/// （探针写「全部成功」、服务日志不吭声），而 `CreateProcessAsUserW` 接着报 1314。
/// 返回三态就把这个区分钉住了（见 [`PrivilegeEnable`]）。
///
/// 没启用**不致命**：真缺了会在 [`spawn_with_token`] 那一步以 1314 报出来，这里只是留痕
/// （日志里出现「未生效/失败」就能把它和别的 1314 成因分开）。
#[cfg(windows)]
#[must_use]
pub fn enable_launch_privileges(
    token: &crate::handle::OwnedHandle,
) -> Vec<(&'static str, PrivilegeEnable)> {
    let mut failures = Vec::new();
    for name in INSTANCE_LAUNCH_PRIVILEGES {
        let outcome = classify_privilege_enable(crate::token::enable_privilege(token, name));
        if !outcome.is_enabled() {
            failures.push((name, outcome));
        }
    }
    failures
}

/// 实例令牌的来源（[`pick_instance_token`] 最终选了哪条路）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// B 路：复制 `winlogon.exe` 的主令牌（天然就在控制台会话里）。
    Winlogon,
    /// A 路：自身令牌 + `SetTokenInformation(TokenSessionId)`。
    OwnToken,
}

impl TokenSource {
    /// 稳定名（写进日志/报告，别用 `Debug`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Winlogon => "B：复制 winlogon.exe 的主令牌",
            Self::OwnToken => "A：自身令牌 + TokenSessionId",
        }
    }
}

/// [`pick_instance_token`] 的结果：选中的令牌 + 两条路的尝试记录。
///
/// 为什么把「失败原因」也带出来而不是只给 `Option`：探针报告与服务工作日志都需要
/// 说清「为什么两条路都不通」，只给一个 `None` 会让真机排障变成猜谜。
#[cfg(windows)]
#[derive(Debug)]
pub struct InstanceToken {
    /// 选中的令牌；两条路都失败时为 `None`。
    pub token: Option<crate::handle::OwnedHandle>,
    /// 选中的来源；`token` 为 `None` 时也是 `None`。
    pub source: Option<TokenSource>,
    /// 启用 [`INSTANCE_LAUNCH_PRIVILEGES`] 时**没真正启用**的项（非空**不一定**是错误：
    /// 「持有但没被赋予」（[`PrivilegeEnable::NotAssigned`]）也会出现在这里）。
    pub privilege_failures: Vec<(&'static str, PrivilegeEnable)>,
    /// B 路失败原因；成功时为 `None`。
    pub winlogon_error: Option<crate::WinError>,
    /// A 路失败原因；成功或未尝试时为 `None`。
    pub own_token_error: Option<crate::WinError>,
}

/// 决定用哪条路的令牌把实例拉进控制台会话：先 B（winlogon），不行再 A（自持 + 会话）。
///
/// B 优先是因为 winlogon 的令牌天然就在控制台会话里（RustDesk/TeamViewer 的做法），
/// 少一次 `SetTokenInformation`；A 的价值是**不依赖任何别的进程**。
/// 两条都失败时 `token` 为 `None`，原因已写进返回的结构。
///
/// **探针与服务共用这一份实现**（原先探针里有一份私有的、签名还带着探针报告的版本）：
/// 两份实现必然漂移，而这条链是无人值守被控端能不能起来的关键路径。
#[cfg(windows)]
#[must_use]
pub fn pick_instance_token(session: u32) -> InstanceToken {
    let mut out = InstanceToken {
        token: None,
        source: None,
        privilege_failures: Vec::new(),
        winlogon_error: None,
        own_token_error: None,
    };

    // 特权开在**我们自己**（调用方）的令牌上，且**必须在两条取令牌路径之前**执行——
    // 见 [`enable_launch_privileges`] 的说明（B 路一成功就跳过启用正是原 bug 的形状）。
    let own = crate::token::current_process_token();
    if let Ok(token) = &own {
        out.privilege_failures = enable_launch_privileges(token);
    }

    match winlogon_primary_token(Some(session)) {
        Ok(token) => {
            out.token = Some(token);
            out.source = Some(TokenSource::Winlogon);
            return out;
        }
        Err(err) => out.winlogon_error = Some(err),
    }

    match &own {
        Ok(token) => match crate::token::duplicate_primary_for_session(token, session) {
            Ok(dup) => {
                out.token = Some(dup);
                out.source = Some(TokenSource::OwnToken);
            }
            Err(err) => out.own_token_error = Some(err),
        },
        Err(err) => out.own_token_error = Some(err.clone()),
    }
    out
}

/// 新建一个「句柄一关就把里面的进程全杀掉」的作业对象。
///
/// 服务把自己的实例进程放进这个作业：服务一退出（正常或崩溃），作业句柄随进程
/// 回收，实例被系统自动终止——不需要服务「临死前记得杀干净」。
///
/// # Errors
///
/// 建作业或设置限制失败时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn create_kill_on_close_job() -> crate::WinResult<crate::handle::OwnedHandle> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // SAFETY: lpJobAttributes 传 null = 默认安全属性（句柄不继承）；lpName 传 null = 匿名作业。
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    let job = crate::handle::require_handle("CreateJobObjectW", job)?;
    let job = crate::handle::OwnedHandle::from_raw(job);

    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: limits 是本栈帧上的结构，长度按 size_of 如实告知；
    // JobObjectExtendedLimitInformation 要求的正是这个结构。
    let ok = unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(limits).cast(),
            u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
        )
    };
    crate::error::check_bool("SetInformationJobObject", ok)?;
    Ok(job)
}

/// 把一个进程放进作业对象。
///
/// # Errors
///
/// 失败时返回 [`crate::WinError`]（进程已在别的作业里且该作业不允许嵌套时为
/// `ERROR_ACCESS_DENIED`——Win8 起允许嵌套，故真机上一般不会碰到）。
#[cfg(windows)]
pub fn assign_to_job(
    job: &crate::handle::OwnedHandle,
    process: &crate::handle::OwnedHandle,
) -> crate::WinResult<()> {
    // SAFETY: 两个句柄都由调用方持有且有效。
    let ok = unsafe {
        windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job.raw(), process.raw())
    };
    crate::error::check_bool("AssignProcessToJobObject", ok)
}

/// 已拉起进程的句柄与 id。
#[cfg(windows)]
#[derive(Debug)]
pub struct SpawnedProcess {
    /// 进程句柄（拥有；`Drop` 时关闭，**不会**杀进程）。
    pub process: crate::handle::OwnedHandle,
    /// 主线程句柄（拥有；用完即可关，不需要留着）。
    pub thread: crate::handle::OwnedHandle,
    /// 进程 id（写日志用）。
    pub pid: u32,
}

/// 用给定令牌在它自己的会话里拉起一个进程（`CreateProcessAsUserW`）。
///
/// # 实现契约
///
/// - `lpDesktop` 设为 `winsta0\default`：**桌面名只影响初始窗口站**。实例进程自己
///   会在需要时 `SetThreadDesktop` 挂到 `Winlogon` 去（见 [`crate::desktop`]），
///   所以这里给 `default` 而不是 `winlogon`——锁屏/解锁会来回切，写死任何一个都不对。
/// - 环境块用 `CreateEnvironmentBlock`（**必须先启用 `SeChangeNotifyPrivilege`**，
///   一般都已启用）而不是继承服务的环境：SYSTEM 的环境变量里带着服务自己的
///   `%USERPROFILE%`（`systemprofile`），实例拿着它去写日志会写到系统配置文件里。
/// - `CREATE_UNICODE_ENVIRONMENT` 必须带上，否则环境块会被按 ANSI 解释。
/// - 用 `CREATE_NO_WINDOW`：实例是**没有 UI 的**（面板由用户会话里的 GUI 承担）。
/// - 命令行是 `PWSTR`（**可写**），故要 `to_wide` 出一个可变的缓冲；`CreateProcessAsUserW`
///   确实会就地改它，传 `PCWSTR` 会编译不过（这是好事）。
///
/// # Errors
///
/// 任一调用失败时返回 [`crate::WinError`]（`CreateProcessAsUserW` 常见失败是
/// `ERROR_PRIVILEGE_NOT_HELD`(1314)：令牌不是主令牌、或缺
/// `SeAssignPrimaryTokenPrivilege`/`SeIncreaseQuotaPrivilege`）。
#[cfg(windows)]
pub fn spawn_with_token(
    token: &crate::handle::OwnedHandle,
    command_line: &str,
    working_dir: Option<&str>,
) -> crate::WinResult<SpawnedProcess> {
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
        STARTUPINFOW,
    };

    let mut cmd = crate::wide::to_wide(command_line);
    let desktop = crate::wide::to_wide("winsta0\\default");
    let cwd = working_dir.map(crate::wide::to_wide);

    let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: token 有效且带 TOKEN_QUERY；bInherit = 0（不继承）；env 是本栈帧上的
    // 指针，成功后必须用 DestroyEnvironmentBlock 释放。
    let env_ok = unsafe { CreateEnvironmentBlock(&raw mut env, token.raw(), 0) };

    // 只读传进去（形参是 *const STARTUPINFOW），故不需要 mut。
    let si = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: desktop.as_ptr().cast_mut(),
        ..STARTUPINFOW::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    // SAFETY: 所有指针都指向本栈帧上、在调用期间存活的可变/只读缓冲；
    // lpApplicationName 传 null = 由命令行第一个 token 决定 exe（我们已给它加引号）；
    // 安全属性传 null = 不继承句柄；环境块要么是我们刚建的、要么是 null（表示继承）。
    let ok = unsafe {
        CreateProcessAsUserW(
            token.raw(),
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            if env_ok != 0 {
                env.cast_const()
            } else {
                std::ptr::null()
            },
            cwd.as_ref().map_or(std::ptr::null(), Vec::as_ptr),
            &raw const si,
            &raw mut pi,
        )
    };
    // 先把 last-error 取出来：下面 DestroyEnvironmentBlock 会覆盖它。
    let failure = if ok == 0 {
        Some(crate::error::last_error("CreateProcessAsUserW"))
    } else {
        None
    };
    if env_ok != 0 && !env.is_null() {
        // SAFETY: env 由 CreateEnvironmentBlock 分配，且只在上面那次调用里被读过。
        unsafe {
            DestroyEnvironmentBlock(env.cast_const());
        }
    }
    if let Some(err) = failure {
        return Err(err);
    }

    let process = crate::handle::require_handle("CreateProcessAsUserW(process)", pi.hProcess)?;
    let thread = crate::handle::require_handle("CreateProcessAsUserW(thread)", pi.hThread)?;
    Ok(SpawnedProcess {
        process: crate::handle::OwnedHandle::from_raw(process),
        thread: crate::handle::OwnedHandle::from_raw(thread),
        pid: pi.dwProcessId,
    })
}

/// 等一个进程结束（`WaitForSingleObject`）。
///
/// 返回 `Ok(true)` = 在超时前退出了；`Ok(false)` = 超时（**不是错误**，
/// 调用方要据此走「放弃等待」的分支）。
///
/// `timeout_ms` 传 `u32::MAX`（`INFINITE`）会一直等——**不要**在服务里这么用：
/// 服务进程卡在等子进程上，SCM 会把它显示成无响应。
///
/// # Errors
///
/// `WaitForSingleObject` 返回 `WAIT_FAILED` 时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn wait_for_process(
    process: &crate::handle::OwnedHandle,
    timeout_ms: u32,
) -> crate::WinResult<bool> {
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    // SAFETY: process 是有效的进程句柄（带 SYNCHRONIZE 权限——CreateProcessAsUserW
    // 给的句柄默认就有），本调用只读它的信号状态。
    let result = unsafe { WaitForSingleObject(process.raw(), timeout_ms) };
    match result {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(crate::error::last_error("WaitForSingleObject")),
    }
}

/// 读一个进程的退出码（`GetExitCodeProcess`）。
///
/// 进程还在跑时给的是 `STILL_ACTIVE`(259)——不做特殊处理，原样返回，
/// 因为「259」本身在报告里就是「它还没退出」这个事实。
///
/// # Errors
///
/// `GetExitCodeProcess` 失败时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn exit_code(process: &crate::handle::OwnedHandle) -> crate::WinResult<u32> {
    let mut code = 0_u32;
    // SAFETY: process 有效（只需要 PROCESS_QUERY_LIMITED_INFORMATION）；
    // code 是本栈帧上的 u32。
    let ok = unsafe {
        windows_sys::Win32::System::Threading::GetExitCodeProcess(process.raw(), &raw mut code)
    };
    crate::error::check_bool("GetExitCodeProcess", ok)?;
    Ok(code)
}

/// 强制结束一个进程（`TerminateProcess`）。
///
/// 服务停实例时用它：作业对象（`KILL_ON_JOB_CLOSE`）是「句柄关闭时才杀」的兜底，
/// 但它要求实例**确实被放进了作业**（[`assign_to_job`] 可能失败）。显式 `TerminateProcess`
/// 是那条兜底路径之外的直接手段——两者都用，才不依赖任何单一机制。
///
/// # Errors
///
/// 进程已退出、或句柄缺 `PROCESS_TERMINATE` 时返回 [`crate::WinError`]。
/// **进程已退出不算异常**：调用方通常忽略这个错误（它要的结果已经达成了）。
#[cfg(windows)]
pub fn terminate_process(
    process: &crate::handle::OwnedHandle,
    code: u32,
) -> crate::WinResult<()> {
    // SAFETY: process 是有效的进程句柄（`CreateProcessAsUserW` 给的句柄默认带
    // PROCESS_TERMINATE）；TerminateProcess 只读这个句柄与退出码。
    let ok = unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(process.raw(), code)
    };
    crate::error::check_bool("TerminateProcess", ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_sources_have_stable_distinct_names() {
        // 稳定名要进日志：写错一个，排障时分不清走的是 B 路还是 A 路。
        assert!(TokenSource::Winlogon.as_str().contains("winlogon"));
        assert!(TokenSource::OwnToken.as_str().contains("TokenSessionId"));
        assert_ne!(TokenSource::Winlogon.as_str(), TokenSource::OwnToken.as_str());
    }

    #[test]
    fn the_launch_privilege_list_is_exactly_the_three_create_process_as_user_needs() {
        // 这三个是 `CreateProcessAsUserW` 的硬要求，少一个就是 1314（看起来像「权限不够」）。
        assert_eq!(INSTANCE_LAUNCH_PRIVILEGES.len(), 3);
        assert!(INSTANCE_LAUNCH_PRIVILEGES.contains(&"SeTcbPrivilege"));
        assert!(INSTANCE_LAUNCH_PRIVILEGES.contains(&"SeAssignPrimaryTokenPrivilege"));
        assert!(INSTANCE_LAUNCH_PRIVILEGES.contains(&"SeIncreaseQuotaPrivilege"));
    }

    #[test]
    fn not_assigned_is_not_treated_as_success() {
        // 这处回归的核心：`enable_privilege` 的 `Ok(false)` 是「持有但没被赋予」
        // （`AdjustTokenPrivileges` 返回 TRUE、last-error 是 1300），必须归到
        // NotAssigned，绝不能被当成 Enabled——否则真机上紧随其后的 1314 会被掩盖。
        assert_eq!(classify_privilege_enable(Ok(true)), PrivilegeEnable::Enabled);
        assert_eq!(
            classify_privilege_enable(Ok(false)),
            PrivilegeEnable::NotAssigned
        );
        assert!(classify_privilege_enable(Ok(true)).is_enabled());
        assert!(!classify_privilege_enable(Ok(false)).is_enabled());
    }

    #[test]
    fn a_failed_call_keeps_the_raw_error() {
        // 调用本身失败（名字无效 / AdjustTokenPrivileges 失败）保留原始错误码：
        // 它与「持有但没生效」是两种处置，不能合并。
        let err = crate::WinError::new("AdjustTokenPrivileges", 5);
        assert_eq!(
            classify_privilege_enable(Err(err.clone())),
            PrivilegeEnable::Failed(err)
        );
    }

    #[test]
    fn the_three_outcomes_render_with_distinct_words() {
        // 「没生效」必须与「调用失败」用不同的词：两者处置不同（令牌策略 vs API/名字），
        // 混成一句会把排障引到错的方向。
        let enabled = privilege_enable_text("SeTcbPrivilege", &PrivilegeEnable::Enabled);
        let not_assigned = privilege_enable_text("SeTcbPrivilege", &PrivilegeEnable::NotAssigned);
        let failed = privilege_enable_text(
            "SeTcbPrivilege",
            &PrivilegeEnable::Failed(crate::WinError::new("LookupPrivilegeValueW", 87)),
        );
        assert!(enabled.contains("成功"), "{enabled}");
        assert!(
            not_assigned.contains("未生效") && not_assigned.contains("ERROR_NOT_ALL_ASSIGNED"),
            "{not_assigned}"
        );
        assert!(failed.contains("失败"), "{failed}");
        // 三者两两不同：一个词串了两类，排障时分不清是哪种。
        assert_ne!(enabled, not_assigned);
        assert_ne!(not_assigned, failed);
        assert_ne!(enabled, failed);
    }

    #[test]
    fn plain_arguments_are_left_alone() {
        assert_eq!(quote_arg("--instance"), "--instance");
        assert_eq!(quote_arg(r"C:\Windows\x.exe"), r"C:\Windows\x.exe");
    }

    #[test]
    fn arguments_with_spaces_are_quoted() {
        assert_eq!(
            quote_arg(r"C:\Program Files\rust-tunnel\client.exe"),
            r#""C:\Program Files\rust-tunnel\client.exe""#
        );
        assert_eq!(quote_arg("--name my host"), r#""--name my host""#);
    }

    #[test]
    fn an_empty_argument_becomes_a_pair_of_quotes() {
        // 空参数不加引号就会「消失」，参数个数就变了。
        assert_eq!(quote_arg(""), r#""""#);
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
    }

    #[test]
    fn a_lone_trailing_backslash_needs_no_quoting() {
        // 反斜杠只在**引号前**才特殊：不带引号的 `C:\dir\` 解析回来还是 `C:\dir\`，
        // 故不加引号（与 std 的 `Command::arg` 行为一致）。
        assert_eq!(quote_arg(r"C:\dir\"), r"C:\dir\");
        assert_eq!(quote_arg(r"\\server\share\"), r"\\server\share\");
    }

    #[test]
    fn trailing_backslashes_are_doubled_once_the_argument_is_quoted() {
        // 一旦因为空格被迫加引号，收尾反斜杠就必须加倍，
        // 否则收尾引号被转义，参数值会变成 `C:\a b\" ...`。
        assert_eq!(quote_arg(r"C:\a b\\"), r#""C:\a b\\\\""#);
        assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
    }

    #[test]
    fn backslashes_before_a_quote_are_doubled() {
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn interior_backslashes_are_not_doubled() {
        assert_eq!(quote_arg(r"C:\a b\c"), r#""C:\a b\c""#);
    }

    #[test]
    fn command_line_quotes_the_program_too() {
        let line = build_command_line(
            r"C:\Program Files\rust-tunnel\client.exe",
            &[
                String::from("--instance"),
                String::from(r"--config C:\x y.toml"),
            ],
        );
        assert_eq!(
            line,
            r#""C:\Program Files\rust-tunnel\client.exe" --instance "--config C:\x y.toml""#
        );
    }

    #[test]
    fn command_line_with_no_arguments_is_just_the_quoted_program() {
        assert_eq!(build_command_line("x.exe", &[]), "x.exe");
    }
}
