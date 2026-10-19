//! 命令行解析：**闭合**的（只认 [`USAGE`] 里列出的旗标，多一个都不认）。
//!
//! # 为什么手写而不是用 `clap`
//!
//! 本程序的命令行只有两种消费者：SCM（它只会看到写进注册表的 `ImagePath` 参数）
//! 和文档里照抄命令的人。形态固定、没有可选值、没有配置文件——手写能让「SCM 到底
//! 会拿到哪几个参数」这件事保持在一眼看得完的几条上，也省掉一个依赖。
//!
//! 未知参数一律**报错**而不是忽略：静默忽略会让「命令行写错了」表现成
//! 「服务装上了但行为不对」，那比直接报错难查得多。

// 非 Windows 上 `main` 只打印 `USAGE`，`Command`/`parse` 就只有单测在用——
// 它们**不是**死代码（Windows 上由 `host` 使用），所以在本机构建时别报 dead_code。
#![cfg_attr(not(windows), allow(dead_code))]

/// 用法说明（`--help` 与解析失败时打印）。
pub const USAGE: &str = "\
rust-tunnel-client-service —— Windows 无人值守被控端（SYSTEM 服务）

本程序有三个身份，别在错误的上下文里跑错命令：
  · 安装/卸载/启停（--install / --uninstall / --start / --stop）需要**管理员**权限
    （要动 SCM、要写 %ProgramFiles%）。其中 --start / --stop 是**幂等**的（见各自的说明）。
  · 控制台侧的调试/诊断命令（--probe / --probe-in-service）同样要管理员（要动 SCM）。
  · 服务进程本身（--run / --probe-instance）由 SCM 启动，不该手工运行。

命令：
  --probe                对照组探针：在当前（已登录的）会话里跑一遍采集与注入。
                         结果写到 %ProgramData%\\rust-tunnel\\probe\\（要管理员才读得到）。
  --probe-in-service     闸门探针：安装（若未装）并启动服务，由服务以 SYSTEM 身份把
                         子实例拉进控制台会话，在**当时的输入桌面**（锁屏时是 Winlogon）
                         上做同一套采集与注入。跑完服务会自己停下来。
                         **它一启动就在等安全桌面**（最多 60 秒）：所以先敲命令、
                         再按 Win+L 锁屏，锁上之后它才动手测。
                         服务是自动启动的，故「开机停在登录界面」那一趟的做法是：
                         装一次（本命令）→ 重启机器 → 登录后看报告。
  --install [--gui-exe <路径>]
                         安装无人值守被控端：把服务 exe 与实例（GUI）exe 复制到
                         %ProgramFiles%\\rust-tunnel\\（DACL 收敛到 SYSTEM + Administrators）、
                         建好 %ProgramData%\\rust-tunnel\\ 与它的 DACL，再把服务注册成自动启动。
                         需要**管理员**权限。
                         --gui-exe 指定实例 exe 的位置；省略时在**本 exe 所在目录**里按
                         rust-tunnel-client-gui.exe / rust-tunnel-client-gui-windows-x86_64.exe
                         查找（CI 把两个产物并排放在同一个目录里）。
                         **装完还要自己写 host.toml**（含服务端地址与客户端 token）：
                         本程序不生成它——那个 token 只有服务端 Web 界面能给。
  --uninstall            卸载服务（幂等；服务不在也算成功）。**不删**已复制到
                         %ProgramFiles% / %ProgramData% 的文件。
  --start                启动已安装的服务（StartServiceW），并**确认它真的在跑**：看到
                         「运行中」后稳 1.5 秒复查，若它已自行停下就报失败。需要**管理员**权限。
                         **幂等**：已经在跑也算成功（SCM 报 1056 会被当成「无需启动」）。
                         这里**不**预检 host.toml——缺 host.toml 时服务**自己**会拒跑并停下
                         （把客户端身份交回 GUI），本命令只是把这个结果报出来。不预检是为了
                         --probe-in-service 那趟真机验收：它直接调 StartServiceW、不经过本命令，
                         且探针实例压根不读那个文件。
  --stop                 停止已安装的服务（ControlService(SERVICE_CONTROL_STOP)）并**等它真的停下**
                         （上限 30 秒），好让 GUI 立刻看到状态变化。需要**管理员**权限。
                         **幂等**：本来就没在跑也算成功。
  --run [--probe]        服务入口，**由 SCM 调用**（不要手工跑）。
                         不带 --probe 时是服务本体：把实例拉进控制台会话并监督它。
  --probe-instance [戳]  子实例入口，**由服务用 CreateProcessAsUser 拉起**（不要手工跑）。
                         戳是服务算好的时间戳（YYYYMMDD-HHMMSS），用来让父子两边
                         算出同一个报告文件名；手工跑时省略，用当前时刻。
  --help, -h             本说明。

典型流程（在**管理员**命令行里）：

  A) 装无人值守被控端：
       1) rust-tunnel-client-service --install --gui-exe <rust-tunnel-client-gui.exe>
       2) 写 %ProgramData%\\rust-tunnel\\host.toml（服务端地址 + 客户端 token + 审批策略）
       3) 立刻起它（不想重启就先起；重启后服务也会自动起）：
          rust-tunnel-client-service --start
       4) 重启机器验证「登录界面上也能被控」

  B) 阶段 0 的真机探针（仍在）：
       1) 登录着、没锁屏：      rust-tunnel-client-service --probe
       2) 敲完这条后立刻 Win+L： rust-tunnel-client-service --probe-in-service
       3) 重启机器（服务已装好，会自动起）：登录后看报告
       4) 收尾：                rust-tunnel-client-service --uninstall
";

/// 解析出来的动作。
///
/// **不是 `Copy`**：`ProbeInstance` 带着一个 `String`（服务传下来的时间戳）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 打印用法。
    Help,
    /// `--probe`：当前会话里的对照探针。
    Probe,
    /// `--probe-in-service`：装服务→起服务→等它自己跑完。
    ProbeInService,
    /// `--probe-instance [时间戳]`：子实例（由服务拉起）。
    ///
    /// 时间戳是**服务侧算好交给它的**（`YYYYMMDD-HHMMSS`），用于让父子两边算出
    /// 同一个报告文件名；手工运行时省略即可，用自己的启动时刻。
    ProbeInstance {
        /// 报告文件名里的时间戳。
        stamp: Option<String>,
    },
    /// `--run`：服务入口；`probe == true` 时跑的是服务侧探针。
    Run {
        /// 是否跑探针（`--run --probe`）。
        probe: bool,
    },
    /// `--install [--gui-exe <路径>]`：把两个 exe 复制到 `%ProgramFiles%`、建目录、注册服务。
    Install {
        /// 实例（GUI）exe 的显式路径；`None` = 在服务 exe 同目录里按已知文件名找。
        ///
        /// 这个路径**不进服务的命令行**（只用于安装时的文件复制），故不构成
        /// `ImagePath` 注入面——与 [`Command::Run`] 的 `extra_args` 是两回事。
        gui_exe: Option<String>,
    },
    /// `--uninstall`。
    Uninstall,
    /// `--start`：启动已安装的服务（**幂等**；见 [`USAGE`]）。
    Start,
    /// `--stop`：停止已安装的服务并等它真的停下（**幂等**；见 [`USAGE`]）。
    Stop,
}

/// 解析命令行（不含 argv[0]）。
///
/// # Errors
///
/// 参数组合不认识时返回一句给用户看的说明（**不**包含用法，调用方负责附上）。
pub fn parse(args: &[String]) -> Result<Command, String> {
    let flags: Vec<&str> = args.iter().map(String::as_str).collect();
    match flags.as_slice() {
        [] | ["--help" | "-h" | "help" | "--?"] => Ok(Command::Help),
        ["--probe"] => Ok(Command::Probe),
        ["--probe-in-service"] => Ok(Command::ProbeInService),
        ["--probe-instance"] => Ok(Command::ProbeInstance { stamp: None }),
        // 时间戳是**服务侧**传下来的，会拼进文件名——所以必须先校验形状。
        // 不校验的话 `..\..\Windows\System32\x` 就能让 SYSTEM 进程往任意路径写文件。
        ["--probe-instance", stamp] => {
            if rust_tunnel_win_platform::sysinfo::is_valid_stamp(stamp) {
                Ok(Command::ProbeInstance {
                    stamp: Some((*stamp).to_owned()),
                })
            } else {
                Err(format!(
                    "--probe-instance 的时间戳必须是 YYYYMMDD-HHMMSS，收到的是「{stamp}」"
                ))
            }
        }
        ["--run"] => Ok(Command::Run { probe: false }),
        // 顺序固定成 `--run --probe`：SCM 的 ImagePath 是我们自己写进去的，
        // 不做「任意顺序」这种宽容——宽容只会让写错的地方晚一点被发现。
        ["--run", "--probe"] => Ok(Command::Run { probe: true }),
        ["--install"] => Ok(Command::Install { gui_exe: None }),
        // 同样只认一种写法：`--install --gui-exe <路径>`。
        ["--install", "--gui-exe", path] => Ok(Command::Install {
            gui_exe: Some((*path).to_owned()),
        }),
        // 只给 `--gui-exe` 而没给 `--install` 是错的写法：它只在安装时用得到。
        ["--gui-exe", path] => Err(format!(
            "--gui-exe 只能与 --install 一起用（收到的是「{path}」）"
        )),
        ["--uninstall"] => Ok(Command::Uninstall),
        ["--start"] => Ok(Command::Start),
        ["--stop"] => Ok(Command::Stop),
        other => Err(format!("无法识别的命令行：{}", other.join(" "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, String> {
        let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        parse(&args)
    }

    #[test]
    fn every_documented_command_parses() {
        assert_eq!(parse_str("").unwrap(), Command::Help);
        assert_eq!(parse_str("--help").unwrap(), Command::Help);
        assert_eq!(parse_str("-h").unwrap(), Command::Help);
        assert_eq!(parse_str("--probe").unwrap(), Command::Probe);
        assert_eq!(
            parse_str("--probe-in-service").unwrap(),
            Command::ProbeInService
        );
        assert_eq!(
            parse_str("--probe-instance").unwrap(),
            Command::ProbeInstance { stamp: None }
        );
        assert_eq!(
            parse_str("--probe-instance 20261017-090503").unwrap(),
            Command::ProbeInstance {
                stamp: Some(String::from("20261017-090503"))
            }
        );
        assert_eq!(parse_str("--run").unwrap(), Command::Run { probe: false });
        assert_eq!(
            parse_str("--run --probe").unwrap(),
            Command::Run { probe: true }
        );
        assert_eq!(
            parse_str("--install").unwrap(),
            Command::Install { gui_exe: None }
        );
        assert_eq!(
            parse_str("--install --gui-exe C:\\dl\\rust-tunnel-client-gui.exe").unwrap(),
            Command::Install {
                gui_exe: Some(String::from("C:\\dl\\rust-tunnel-client-gui.exe"))
            }
        );
        assert_eq!(parse_str("--uninstall").unwrap(), Command::Uninstall);
        assert_eq!(parse_str("--start").unwrap(), Command::Start);
        assert_eq!(parse_str("--stop").unwrap(), Command::Stop);
    }

    #[test]
    fn gui_exe_is_only_meaningful_with_install() {
        // 单给 `--gui-exe` 是写错了：它只在安装时用来定位要复制的实例 exe。
        let err = parse_str("--gui-exe C:\\x.exe").unwrap_err();
        assert!(err.contains("--install"), "{err}");
        // 反过来（`--gui-exe` 在前）也不接受：命令行只认一种写法，宽容只会让写错的
        // 地方晚一点被发现。
        assert!(parse_str("--gui-exe C:\\x.exe --install").is_err());
    }

    #[test]
    fn unknown_flags_are_rejected_rather_than_ignored() {
        // 静默忽略会让「命令行写错」表现成「服务装上了但行为不对」。
        assert!(parse_str("--probe --verbose").is_err());
        assert!(parse_str("--probe-in-service extra").is_err());
        assert!(parse_str("--wat").is_err());
        // 顺序固定：`--probe --run` 是错的写法（SCM 里的 ImagePath 由我们写，
        // 不做宽容只会让写错的地方早一点暴露）。
        assert!(parse_str("--probe --run").is_err());
        // 启停各自独立，不接受组合写法。
        assert!(parse_str("--start --stop").is_err());
        assert!(parse_str("--stop --start").is_err());
        assert!(parse_str("--start extra").is_err());
    }

    #[test]
    fn the_error_message_quotes_what_was_typed() {
        let err = parse_str("--probe --verbose").unwrap_err();
        assert!(err.contains("--probe --verbose"), "{err}");
    }

    #[test]
    fn a_hand_written_stamp_is_rejected_before_it_reaches_a_file_name() {
        // 命令行里的戳会拼进文件名；`..`/分隔符必须在这里就被拦下
        // （服务的子实例跑在 SYSTEM 身份下，放行等于给了任意路径写）。
        let err = parse_str(r"--probe-instance ..\..\Windows\System32\x").unwrap_err();
        assert!(err.contains("YYYYMMDD-HHMMSS"), "{err}");
        assert!(parse_str("--probe-instance 2026-1017-0905").is_err());
        assert!(parse_str("--probe-instance 20261017_090503").is_err());
    }

    #[test]
    fn usage_mentions_every_command_and_the_three_states() {
        for needle in [
            "--probe",
            "--probe-in-service",
            "--probe-instance",
            "--run",
            "--install",
            "--gui-exe",
            "--uninstall",
            "--start",
            "--stop",
            "host.toml",
            "%ProgramFiles%",
            "锁屏",
            "登录界面",
        ] {
            assert!(USAGE.contains(needle), "用法说明里缺 {needle}");
        }
    }
}
