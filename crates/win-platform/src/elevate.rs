//! 自提权（UAC）：把「安装服务」那个 exe 以管理员身份拉起来并等它结束。
//!
//! # 为什么需要它
//!
//! 无人值守被控端要落地成一个 **SYSTEM 服务**，而安装/卸载服务（`--install` /
//! `--uninstall`）必须写 SCM 数据库——那要求**管理员 token**，普通用户跑起来的 GUI
//! 没有。于是 GUI 的「安装服务」按钮不能自己干活，只能把
//! [`crate::paths::installed_service_exe`] **提权**重新拉起一次，由提权后的那个进程
//! 去调 [`crate::service::install`]。本模块就是那次「重新拉起」的平台层。
//!
//! # 为什么是 `ShellExecuteExW` 而不是 `ShellExecuteW`
//!
//! `ShellExecuteW` 只回一个 `HINSTANCE`：它**不是错误码**（成功与失败共用「> 32 即成功」
//! 的宽泛约定），而且**拿不到子进程句柄**。于是三件本模块必须分辨的事，它一件都答不了：
//!
//! 1. **「用户在 UAC 提示上点了否」**——这在 `ShellExecuteW` 里和「文件不存在」一样，
//!    都只表现为「返回值 ≤ 32」，无法区分；
//! 2. **「安装到底成没成」**——设置界面要显示「安装成功 / 安装失败（码 N）」，那需要
//!    子进程**自己的退出码**；
//! 3. **「子进程跑完没有」**——`--install` 要复制 exe、加固 DACL、停旧服务（有界 20 秒），
//!    界面必须等它结束才知道结果。
//!
//! `ShellExecuteExW` 回一个**真正的**成功/失败布尔，失败时 `GetLastError()` 有意义；
//! 配合 `SEE_MASK_NOCLOSEPROCESS` 还能在 `hProcess` 里拿到子进程句柄，从而
//! `WaitForSingleObject` 等它、`GetExitCodeProcess` 读它的退出码。
//!
//! # `runas` 与「取消」
//!
//! `lpVerb` 必须是宽字符串 `"runas"`：它让 ShellExecute 走 **UAC 提权**路径
//! （弹同意提示 / 在安全桌面上要凭据），而不是普通的「打开」。
//!
//! 用户在提示上点「否」时，`ShellExecuteExW` **本身**返回失败，且
//! `GetLastError() == ERROR_CANCELLED`（**1223**）。这**不是**错误——它是一次合法的
//! 用户选择，故映射到 [`ElevationOutcome::Cancelled`]，调用方据此**什么都不提示**
//! （而不是弹一条「安装失败」的报错）。
//!
//! # 线程模型
//!
//! [`run_elevated`] **可以从非 UI 线程调用**：GUI 会把它丢进一个后台任务，因为
//! `WaitForSingleObject(INFINITE)` 会阻塞到子进程结束——放在 UI 线程上就是「界面卡死」。
//! `hwnd` 因此传 null：后台线程没有可当 owner 的窗口，UAC 提示会无主地出现
//! （这是允许的，提示由系统在安全桌面上呈现）。
//!
//! # 为什么不调 `CoInitializeEx`（一个**刻意**的决定）
//!
//! MSDN 说 COM「应当」在 `ShellExecuteEx` 之前初始化，且要用 STA
//! （`CoInitializeEx(NULL, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)`，因为某些
//! shell 扩展要求 STA）。但同一段 Remarks 也说，有些情况下 `ShellExecuteEx` **根本不用
//! 任何 shell 扩展**，那些情况「不需要初始化 COM」。
//!
//! 本模块的调用正是后者：用 `lpVerb = "runas"` 按**绝对路径**拉起一个 exe（`lpFile` 是我们
//! 自己的服务 exe 全路径，`lpParameters` 只是一串普通参数），**不牵涉任何 shell 扩展**。
//!
//! 而「规范地」做一遍的代价并不小：
//!
//! - `CoInitializeEx` 符号在 `Win32_System_Com` feature 下，而**整个 workspace 目前哪里都
//!   没开**这个 feature——开它是一次 workspace 级的改动；
//! - `COINIT_APARTMENTTHREADED` 的类型是 `COINIT`（`i32`），要塞进 `u32` 形参就得写
//!   `as u32`，而这个 cast 会踩到本 workspace 拒绝的 `clippy::pedantic`（`cast_sign_loss`）；
//! - 一个**没有消息泵**的 STA 线程本身就是隐患，而这条路径还压在 UAC 上。
//!
//! 于是这是一个**有记录的一致性缺口**：就这一处调用而言，判定其影响很低。这里如实写明，
//! 而不是假装代码已完全合规。
//!
//! # 可验证的部分
//!
//! [`run_elevated`] 只在 Windows 上编译，**macOS 上无法测**（提权是 Windows 独有的系统
//! 行为，本机连编译都不参与）。本文件里唯一有真实测试覆盖的是可移植的
//! [`build_parameter_string`]：它实现 `CommandLineToArgvW` 的引号规则，而这条规则正是
//! 「安装路径带空格 + 参数含用户可控路径」下最容易写错、写错又最危险的地方（参数会以
//! 错误的边界拆开，甚至变成命令行注入）。

// 只在 Windows 上用到：非 Windows 上 `run_elevated` 整段不编译，带上它会变成未使用导入。
#[cfg(windows)]
use std::path::Path;

// ── 可移植：参数拼接（唯一能在 macOS 上单测的逻辑）──────────────

/// `ERROR_CANCELLED`（Win32）：用户在 UAC 提示上点了「否」。
///
/// 值 **1223**。当 [`run_elevated`] 的 `ShellExecuteExW` 失败且 `GetLastError()` 等于它时，
/// 说明这不是故障、而是一次用户选择，应映射到 [`ElevationOutcome::Cancelled`]。
///
/// windows-sys 0.61.2 确实导出了这个名字（`Win32::Foundation::ERROR_CANCELLED`），但那是
/// `#[cfg(windows)]` 的；这里自带一份字面值，好让这个**语义**在文档与代码里都显式可见，
/// 也让它能在 macOS 上被单测钉住（1223 这个数字本身就是结论，值得一条机械锁）。
pub const ERROR_CANCELLED: u32 = 1223;

/// 把参数拼成 `ShellExecuteExW` 的 `lpParameters`。
///
/// 规则就是 `CommandLineToArgvW` 实际用的那一套（也是 `CreateProcess` 的约定）：
///
/// - **不含空格/TAB/引号且非空**的参数**原样**输出——包括以反斜杠结尾的：裸命令行里
///   反斜杠只在**引号前**才有转义意义，`C:\dir\` 不加引号传过去，解析回来还是 `C:\dir\`；
/// - 否则用 `"` 包起来，且**紧挨着引号**（含我们加的收尾引号）的连续反斜杠要**加倍**，
///   其余反斜杠原样。写错这一条，`C:\Program Files\a\"b` 这类路径会在另一端拆成错误的
///   参数边界；
/// - 空参数输出 `""`（不加引号它会在另一端「消失」，参数个数就变了）。
///
/// # 为什么复用 [`crate::process::quote_arg`] 而不是再抄一份
///
/// 这条规则在本仓已经有**唯一一份**带单测的实现（`process::quote_arg`，连
/// 「裸的结尾反斜杠不需要加引号」这种边角都钉住了）。引号规则是安全相关的（写错 =
/// 命令行参数注入），两份实现必然漂移——所以这里只做**拼接**，引号交给那一份。
#[must_use]
pub fn build_parameter_string(args: &[String]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        parts.push(crate::process::quote_arg(arg));
    }
    parts.join(" ")
}

// ── Windows FFI：提权拉起并等待 ─────────────────────────────────

/// 提权运行的结果。
#[cfg(windows)]
#[derive(Debug)]
pub enum ElevationOutcome {
    /// 进程跑完并退出，`exit_code` 是它自己的退出码。
    ///
    /// 设置界面靠它显示「安装成功（0）/ 安装失败（码 N）」——**非零不代表提权失败**，
    /// 只代表那个子进程自己失败了（如 SCM 报 `ERROR_ALREADY_EXISTS`(183)）。
    Completed {
        /// 子进程的退出码（`GetExitCodeProcess`）。
        exit_code: u32,
    },
    /// 用户在 UAC 提示上点了「否」，或系统策略不允许提权
    /// （`ShellExecuteExW` 返回失败且 `GetLastError() == ERROR_CANCELLED`(1223)）。
    ///
    /// **不是错误**：调用方应当**静默**处理（什么都不提示），而不是报「安装失败」。
    Cancelled,
}

/// 提权运行一个可执行文件并**等它结束**。
///
/// 走 `ShellExecuteExW` + `lpVerb = "runas"`（见模块文档）：成功则等子进程退出并读它的
/// 退出码；用户在 UAC 提示上点「否」则返回 [`ElevationOutcome::Cancelled`]。
///
/// # 阻塞
///
/// 本函数会**阻塞到子进程结束**（`WaitForSingleObject(INFINITE)`）。`--install` 要复制
/// exe、加固 DACL、停旧服务（上限约 20 秒），所以这里用 `INFINITE` 而不是一个猜出来的
/// 超时：调用方是 GUI 的**后台任务**，不是 UI 线程，长阻塞不会冻结界面。**不要**在
/// UI 线程或服务主循环里调它（那会让界面/服务看起来无响应）。
///
/// # Errors
///
/// - `ShellExecuteExW` 失败且**不是** `ERROR_CANCELLED` 时返回 [`crate::WinError`]
///   （常见：路径不存在 `ERROR_FILE_NOT_FOUND`(2)、无 Shell 关联等）；
/// - 成功但拿不到子进程句柄（`hProcess` 为空，理论上不该发生）；
/// - `WaitForSingleObject` 或 `GetExitCodeProcess` 失败。
#[cfg(windows)]
pub fn run_elevated(exe: &Path, args: &[String]) -> crate::WinResult<ElevationOutcome> {
    use crate::error::last_error;
    use crate::handle::{require_handle, OwnedHandle};
    use crate::wide::to_wide;
    use windows_sys::Win32::System::Threading::INFINITE;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // 每个进 ShellExecuteExW 的字符串都要先绑成局部变量再取指针：把 `to_wide(..).as_ptr()`
    // 直接写进结构体字面量，那个临时 Vec 会在语句结束（也就是调用返回后）被释放，而
    // ShellExecuteExW 是**在调用期间**读它的——这是本文件最容易写错的悬垂点。
    let verb = to_wide("runas");
    let file = to_wide(&exe.to_string_lossy());
    let parameters = to_wide(&build_parameter_string(args));

    // 全零 + cbSize 是 Win32 的硬要求，否则未定义行为。用 `..default()` 而不是逐个字段
    // 填零，保证将来 windows-sys 给这个结构加字段时这里自动跟上。
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
        // 要子进程句柄（hProcess），否则拿不到退出码、也等不了它结束。
        //
        // 再 OR 上 `SEE_MASK_NOASYNC`：MSDN 的 `ShellExecuteEx` Remarks 要求，**调用线程没有
        // 消息循环**（或线程/进程在它返回后马上结束）时**必须**带这个标志。本函数的调用方
        // 正是 GUI 的后台线程——它没有消息循环，而整件事的目的就是拿返回的进程句柄去等它。
        // 缺了它，`ShellExecuteEx` 返回时**不保证**操作已经完成。
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        // 后台线程没有可当 owner 的窗口，传 null 是允许的：UAC 提示由系统在安全桌面上
        // 呈现，不需要父窗口。
        hwnd: std::ptr::null_mut(),
        // "runas" 触发 UAC 提权（同意提示 / 凭据），不是普通的「打开」。
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..SHELLEXECUTEINFOW::default()
    };

    // SAFETY: info 是本栈帧上的结构，cbSize 已按文档设置；三个字符串指针都指向本栈帧上
    // 在调用期间存活的 NUL 结尾 UTF-16 缓冲。ShellExecuteExW 只读这些输入，并在
    // SEE_MASK_NOCLOSEPROCESS 下把子进程句柄写进 info.hProcess（所有权随之归我们）。
    let ok = unsafe { ShellExecuteExW(&raw mut info) };
    if ok == 0 {
        // 错误码必须紧接着失败读（中间不插别的 Win32 调用）。
        let err = last_error("ShellExecuteExW");
        if err.code() == ERROR_CANCELLED {
            // 用户点了「否」：这是一次合法的选择，不是错误（见模块文档）。
            return Ok(ElevationOutcome::Cancelled);
        }
        return Err(err);
    }

    // hProcess 可能为 null（罕见：某些 Shell 实现不返回句柄）。包成 OwnedHandle 让
    // **所有**返回路径（含下面 wait/exit_code 失败）都由 Drop 关掉它——「WaitForSingleObject
    // 失败也要关句柄」正是靠这个 RAII 兜住的，不必在每个分支手写 CloseHandle。
    let process = require_handle("ShellExecuteExW(hProcess)", info.hProcess)?;
    let process = OwnedHandle::from_raw(process);

    // 等它结束。`wait_for_process` 就是 `WaitForSingleObject`；INFINITE 下不会超时，故
    // 这里只关心错误（它返回的 `Ok(false)` 只可能来自一个有限的 timeout_ms）。
    // 注意：`wait_for_process` 的文档反对在**服务**里用 INFINITE（那会把服务显示成无响应），
    // 而这里调用方是 GUI 的后台任务，长阻塞正是要的行为。
    crate::process::wait_for_process(&process, INFINITE)?;
    // 读子进程自己的退出码：设置界面据此显示「安装成功 / 安装失败（码 N）」。
    let code = crate::process::exit_code(&process)?;
    Ok(ElevationOutcome::Completed { exit_code: code })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CommandLineToArgvW` 的**测试用**实现：把一条命令行拆回参数列表。
    ///
    /// 这不是生产代码——它的唯一用途是让引号测试做**往返**断言：先
    /// [`build_parameter_string`] 拼一条命令行，再用这个（独立按文档实现的）解析器拆回来，
    /// 逐参数比较。拼对了往返必然相等；拼错了（比如反斜杠没加倍）往返立刻露馅。
    ///
    /// 规则来自 MSDN 的「Parsing C++ Command-Line Arguments」：
    ///
    /// - `2n` 个反斜杠 + `"` → `n` 个反斜杠，且该 `"` 切换「在引号内」状态；
    /// - `2n+1` 个反斜杠 + `"` → `n` 个反斜杠 + 一个字面 `"`；
    /// - `n` 个反斜杠（后面不是 `"`）→ `n` 个反斜杠；
    /// - 引号内的 `""` → 一个字面 `"`；
    /// - 引号外的空格/TAB 是参数分隔符。
    fn parse_command_line(line: &str) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        let mut args = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            // 跳过参数之间的分隔符。
            while i < chars.len() && matches!(chars[i], ' ' | '\t') {
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            let mut arg = String::new();
            let mut in_quotes = false;
            while i < chars.len() {
                let c = chars[i];
                if c == '\\' {
                    let mut n = 0usize;
                    while i < chars.len() && chars[i] == '\\' {
                        n += 1;
                        i += 1;
                    }
                    if i < chars.len() && chars[i] == '"' {
                        for _ in 0..n / 2 {
                            arg.push('\\');
                        }
                        if n % 2 == 1 {
                            arg.push('"');
                        } else {
                            in_quotes = !in_quotes;
                        }
                        i += 1;
                    } else {
                        for _ in 0..n {
                            arg.push('\\');
                        }
                    }
                } else if c == '"' {
                    if in_quotes && i + 1 < chars.len() && chars[i + 1] == '"' {
                        arg.push('"');
                        i += 2;
                    } else {
                        in_quotes = !in_quotes;
                        i += 1;
                    }
                } else if !in_quotes && matches!(c, ' ' | '\t') {
                    break;
                } else {
                    arg.push(c);
                    i += 1;
                }
            }
            args.push(arg);
        }
        args
    }

    #[test]
    fn error_cancelled_is_the_uac_refusal_code() {
        // 1223 这个数字本身就是结论（见常量文档），钉住它防止被改错。
        assert_eq!(ERROR_CANCELLED, 1223);
    }

    /// 表驱动：`build_parameter_string` 对**单个**参数的引号规则。
    #[test]
    fn parameter_quoting_follows_command_line_to_argv_rules() {
        let cases: &[(&str, &str)] = &[
            // 1. 普通参数：原样（无需引号）。
            ("--install", "--install"),
            // 2. 带空格的参数（安装后的 exe 就在 %ProgramFiles% 下）：必须加引号。
            (
                r"C:\Program Files\rust-tunnel\rust-tunnel-client-service.exe",
                r#""C:\Program Files\rust-tunnel\rust-tunnel-client-service.exe""#,
            ),
            // 3. 空参数：输出一对引号（不加它会在另一端「消失」，参数个数就变了）。
            ("", r#""""#),
            // 4. 内嵌引号：转义。
            (r#"a"b"#, r#""a\"b""#),
            // 5. 引号前的反斜杠：加倍。
            (r#"a\"b"#, r#""a\\\"b""#),
            // 6. 带空格 + 结尾反斜杠：收尾反斜杠加倍（否则我们加的收尾引号被它转义掉）。
            (r"C:\dir with space\", r#""C:\dir with space\\""#),
            // 7. 内部反斜杠（不在引号前）：原样。
            (r"C:\a b\c", r#""C:\a b\c""#),
            // 8. **裸的**结尾反斜杠：原样（反斜杠只在引号前才特殊）。见下方专门测试。
            (r"C:\dir\", r"C:\dir\"),
            // 9. UNC 路径同样按上述规则。
            (r"\\server\share\", r"\\server\share\"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                build_parameter_string(&[String::from(*input)]),
                *expected,
                "输入 {input:?}"
            );
        }
        // 表里至少 8 行（任务要求的下限），钉住它防止被删减。
        assert!(cases.len() >= 8);
    }

    /// 裸的结尾反斜杠**不需要**加引号：反斜杠只在**引号前**才特殊，`C:\dir\` 原样传过去
    /// 解析回来一模一样。
    ///
    /// **注意**：任务描述里给的期望是 `C:\dir\` → `"C:\dir\\"`（加引号并加倍）。那与它自己
    /// 写的第一条规则（「不含空格/TAB/引号且非空 → 原样」）**矛盾**，也与本仓既有的
    /// `process::quote_arg` 和 std 的 `Command::arg` 都不一致（它们对裸的结尾反斜杠都
    /// 原样输出）。这里按**正确**的规则实现：加引号虽也能往返，但会给这条规则凭空添一个
    /// 特例，并与另一份实现漂移。
    #[test]
    fn a_bare_trailing_backslash_is_left_verbatim() {
        assert_eq!(build_parameter_string(&[String::from(r"C:\dir\")]), r"C:\dir\");
        assert_eq!(
            build_parameter_string(&[String::from(r"\\server\share\")]),
            r"\\server\share\"
        );
        // 往返也成立：原样输出解析回来还是原值。
        assert_eq!(
            parse_command_line(&build_parameter_string(&[String::from(r"C:\dir\")])),
            vec![String::from(r"C:\dir\")]
        );
    }

    /// 任务点名的「结尾反斜杠」往返：收尾反斜杠在**被迫加引号**的形态下必须加倍，
    /// 解析回来才是原值（`"C:\dir\\"` → `C:\dir\`）。
    #[test]
    fn trailing_backslash_round_trips_when_the_argument_is_quoted() {
        // 用空格迫使它加引号。
        let line = build_parameter_string(&[String::from(r"C:\my dir\")]);
        assert_eq!(line, r#""C:\my dir\\""#);
        assert_eq!(
            parse_command_line(&line),
            vec![String::from(r"C:\my dir\")]
        );
    }

    /// 往返：`build_parameter_string` 的输出经 `CommandLineToArgvW` 规则解析回来，
    /// 必须逐参数等于输入。
    #[test]
    fn quoting_round_trips_through_command_line_parsing() {
        let args = vec![
            String::from(r"C:\Program Files\rust-tunnel\rust-tunnel-client-service.exe"),
            String::from("--install"),
            String::from(r"--data C:\ProgramData\rust-tunnel\"),
            String::new(),
            String::from(r#"weird"arg"#),
            String::from(r"C:\dir with space\"),
        ];
        let line = build_parameter_string(&args);
        assert_eq!(parse_command_line(&line), args, "命令行：{line}");
    }

    #[test]
    fn multiple_arguments_are_joined_by_single_spaces() {
        let args = vec![
            String::from("--install"),
            String::from("--server"),
            String::from("example.com:3000"),
        ];
        assert_eq!(
            build_parameter_string(&args),
            "--install --server example.com:3000"
        );
    }

    #[test]
    fn no_arguments_yields_an_empty_parameter_string() {
        assert_eq!(build_parameter_string(&[]), "");
    }

    /// 本 helper 存在的**具体**理由：安装后的服务 exe 落在 `%ProgramFiles%` 下，路径
    /// **必然**含空格，不加引号 `ShellExecuteExW` 会把它在空格处截断成「找不到文件」。
    #[test]
    fn the_installed_service_path_is_quoted_because_it_lives_under_program_files() {
        let arg = crate::paths::installed_service_exe()
            .to_string_lossy()
            .into_owned();
        let quoted = build_parameter_string(std::slice::from_ref(&arg));
        // 只有真机上 `%ProgramFiles%` 才存在（含空格）；macOS 上它回退到相对路径，可能
        // 不含空格——那时这个断言无意义，故按「含空格才断言」处理。
        if arg.contains(' ') {
            assert_eq!(quoted, format!("\"{arg}\""), "{quoted}");
        } else {
            assert_eq!(quoted, arg);
        }
    }
}
