//! 监督策略：**纯逻辑**（无 FFI、无平台分支），故在 macOS 上也能被单测钉住。
//!
//! 服务本体（`host`）在 Windows 上跑，`cargo test` 在本机根本编译不到它——所以凡是
//! 「一条 if 就能说清、但写错了会在真机上表现为诡异行为」的决策，都挪到这里：
//!
//! | 决策 | 写错的表现 |
//! |---|---|
//! | [`classify_session_change`]：哪些会话变化要重建实例 | 锁屏时把实例重启（正是要覆盖的场景）／注销后不重建（实例卡在死会话里） |
//! | [`backoff_for`]：退出后多久重启 | 忙等（把 CPU 与日志打满）／退避太短（服务端被反复注册打扰） |
//! | [`instance_command_line`]：实例的命令行 | 路径带空格时参数被拆开，实例读到半个路径 |
//!
//! 非 Windows 上本模块的函数只有单测在用——它们**不是**死代码（Windows 上由 `host` 使用），
//! 故在本机构建时别报 dead_code。

#![cfg_attr(not(windows), allow(dead_code))]

use rust_tunnel_win_platform::process::build_command_line;
use rust_tunnel_win_platform::session;
use std::path::Path;
use std::time::Duration;

/// 实例退出后的重启退避阶梯（秒）。
///
/// 1 → 2 → 5 → 15 → 30 → 60，此后停在 60 秒。为什么是这个形状：
/// - **第一档不是 0**：实例若因为配置错误（`host.toml` 缺失）秒退，0 退避会让服务陷入
///   忙循环——日志刷满、CPU 打满，而问题（配置没写）一点都不会因此更快被发现。
/// - **上限 60 秒**：无人值守下没人来手工重启，退避再长就等于长时间失联；60 秒是
///   「别把日志打满」与「别失联太久」的折中。
/// - **递增**：持续的失败多半是环境问题（没装 GUI exe、`host.toml` 不对），
///   快速重试没有意义；偶发失败（网络抖动）则由第一档的 1 秒兜住。
pub const BACKOFF_SECONDS: [u64; 6] = [1, 2, 5, 15, 30, 60];

/// 第 `attempt` 次失败后的退避时长（`attempt` 从 1 开始计数）。
///
/// `attempt` 超过阶梯长度时返回最后一档（**封顶**，不是无限增长）。
#[must_use]
pub fn backoff_for(attempt: u32) -> Duration {
    // 下标钳到阶梯范围内：`attempt` 会随失败次数一直涨，不钳会越界。
    let index = usize::try_from(attempt.saturating_sub(1))
        .unwrap_or(0)
        .min(BACKOFF_SECONDS.len() - 1);
    Duration::from_secs(BACKOFF_SECONDS[index])
}

/// 一次会话变化事件之后的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionChangeAction {
    /// 忽略：不影响实例所在的会话（**锁屏/解锁**是这一类）。
    Ignore,
    /// 重建实例：它所在的会话要走了，或控制台已经挪到别的会话。
    Rebuild,
}

/// 「实例所在会话要走了」的事件——落在**实例自己的会话**上时才意味着重建。
const SESSION_GOING_AWAY: [u32; 3] = [
    session::WTS_SESSION_LOGOFF,
    session::WTS_SESSION_TERMINATE,
    session::WTS_CONSOLE_DISCONNECT,
];

/// 「控制台可能挪到别的会话去了」的事件——落在**别的会话**上时才意味着重建。
///
/// **只有 `WTS_CONSOLE_CONNECT`**：它才是「有会话接到了物理屏幕」这件事本身
/// （快速用户切换会为**新**控制台会话发它）。刻意**不含** `WTS_SESSION_LOGON`：
/// 那个事件对**任何**会话都会发，包括「RDP 登录进一个不是控制台的新会话」——那时
/// 控制台压根没动，判成重建纯属自找：`wait_for_console_session` 会把**同一个**控制台
/// 会话原样交回来，实例被拆掉重建，正在进行的远程会话被掐断（正是本模块要避免的代价）。
/// 别被内部一致性误导：既然 `WTS_REMOTE_CONNECT` 被刻意排除，跟在它后面的
/// `WTS_SESSION_LOGON` 就更不该进这张表。
const CONSOLE_MOVED: [u32; 1] = [session::WTS_CONSOLE_CONNECT];

/// 由一条 `SERVICE_CONTROL_SESSIONCHANGE` 决定要不要重建实例。
///
/// # 输入
///
/// - `event_type`：WTS 事件类型（`session::WTS_*`）；
/// - `event_session`：**事件载荷里的**会话 id（SCM 给的，不再自己调 API 去猜）；
/// - `supervised`：实例**当前**所在的会话；`None` = 还没拉起实例（或正在等新会话）。
///
/// # 规则
///
/// 1. **锁屏/解锁永远忽略**：控制台会话 id 不变，而这正是本功能要覆盖的场景——
///    路径上零重启是设计目标，不是巧合。
/// 2. 还没拉起实例（`supervised == None`）：没有「旧实例」要换，忽略。
/// 3. 事件落在**实例自己的会话**里：只有「这个会话要走掉」（注销/终止/控制台断开）
///    才重建。
/// 4. 事件落在**别的**会话里：只有「控制台接入」（`WTS_CONSOLE_CONNECT`）才重建。
///    其余（**别处有人登录**、别处的 RDP 接入断开、别处的会话被终止、别处被远程控制）
///    **不**重建——一次多余的重建会**掐断正在进行中的远程会话**，代价远大于收益。
///    特别地，`WTS_SESSION_LOGON` 对**任何**会话都发（含 RDP 登录进非控制台会话），
///    此时控制台并没有动，判成重建纯属自找。
///
/// 幂等：同一个 `(event_type, event_session, supervised)` 永远给出同一个答案
/// （调用方拿到 `Rebuild` 后自己清标志，重复的事件不会造成第二次重建）。
#[must_use]
pub fn classify_session_change(
    event_type: u32,
    event_session: u32,
    supervised: Option<u32>,
) -> SessionChangeAction {
    if matches!(
        event_type,
        session::WTS_SESSION_LOCK | session::WTS_SESSION_UNLOCK
    ) {
        return SessionChangeAction::Ignore;
    }
    match supervised {
        None => SessionChangeAction::Ignore,
        Some(current) if current == event_session => {
            if SESSION_GOING_AWAY.contains(&event_type) {
                SessionChangeAction::Rebuild
            } else {
                SessionChangeAction::Ignore
            }
        }
        Some(_) => {
            if CONSOLE_MOVED.contains(&event_type) {
                SessionChangeAction::Rebuild
            } else {
                SessionChangeAction::Ignore
            }
        }
    }
}

/// 实例的命令行参数（**顺序固定**，与 `crates/client-gui` 的 `--instance` 解析器一致）。
///
/// 四个路径参数全部**显式**传入：实例以 SYSTEM 身份跑，「按当前 profile 解析」的默认值
/// 会落到 `systemprofile`。服务是这些路径的唯一权威，显式传下去就不存在
/// 「服务的预期」与「实例的默认值」不一致这个静默失败面。
#[must_use]
pub fn instance_args(config: &Path, share: &Path, download: &Path) -> Vec<String> {
    vec![
        String::from("--instance"),
        String::from("--config"),
        config.to_string_lossy().into_owned(),
        String::from("--share"),
        share.to_string_lossy().into_owned(),
        String::from("--download"),
        download.to_string_lossy().into_owned(),
    ]
}

/// 拼实例的完整命令行。
///
/// 必须走 [`build_command_line`]（它实现了 `CommandLineToArgvW` 的引号规则）：
/// 路径里可能有空格（`C:\Program Files\...`）甚至引号，**手工拼接**会把一个参数拆成
/// 两个，实例读到半个路径。
#[must_use]
pub fn instance_command_line(
    program: &Path,
    config: &Path,
    share: &Path,
    download: &Path,
) -> String {
    build_command_line(
        &program.to_string_lossy(),
        &instance_args(config, share, download),
    )
}

/// 一条前置条件的**观察结果**：它的规范路径，以及「它现在在不在」。
///
/// 用结构体而不是「路径 + 布尔」两个裸参数：两个相邻的 `bool` 在调用点极易写反，
/// 而写反的后果是「明明缺配置，日志却说 exe 缺失」这种把人引到错方向的诊断。
#[derive(Debug, Clone, Copy)]
pub struct Precondition<'a> {
    /// 这条前置条件的规范路径（进日志与错误文案）。
    pub path: &'a Path,
    /// 它现在在不在（`false` = 缺）。
    pub present: bool,
}

/// 服务**无法提供服务**时的拒跑理由（`None` = 前置条件都在）。
///
/// 两条前置条件都是**确定性**的，且与监督循环真正要用的东西一一对应，故不存在第二个
/// 事实来源：
///
/// 1. **实例配置**（`host.toml`）不在：实例一启动就会退出（`client-gui` 的
///    `load_host_config` 读不到就 `?` 掉），这台机器随即**失去客户端身份**；
/// 2. **实例 exe** 不在：监督循环连进程都拉不起来，服务永远没有实例可跑。
///
/// 两条消息都必须说清三件事：**哪里错了、哪条路径、服务会拿它怎么办**（自行停止并把
/// 客户端身份交回 GUI），否则日志里只有一句「缺少文件」，运维不知道服务还在不在跑、
/// 身份归谁。
///
/// 决策本身是纯函数（无 FFI、无平台分支），故在 macOS 上也有单测；`host` 只负责把两条
/// 真实路径读出来喂进来（见那里的 `refuse_reason`）。
#[must_use]
pub fn refuse_reason(
    host_config: Precondition<'_>,
    instance_exe: Precondition<'_>,
) -> Option<String> {
    if !host_config.present {
        return Some(format!(
            "实例配置 {} 不存在：实例一启动就会退出（它读不到服务端地址与客户端 token），\
             这台机器会彻底失去客户端身份。服务将**自行停止、把客户端身份交回 GUI**。\
             请按「安装 → **先写 host.toml** → 再启动服务」的顺序补上这一步，\
             详见 WINDOWS-SECURE-DESKTOP.md。",
            host_config.path.display()
        ));
    }
    if !instance_exe.present {
        return Some(format!(
            "实例 exe {} 不存在：监督循环拉不起任何进程，服务永远没有实例可跑。\
             服务将**自行停止、把客户端身份交回 GUI**。请跑一次 --install \
             把它复制到 %ProgramFiles%\\rust-tunnel\\。",
            instance_exe.path.display()
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(event_type: u32, session: u32, supervised: Option<u32>) -> SessionChangeAction {
        classify_session_change(event_type, session, supervised)
    }

    #[test]
    fn lock_and_unlock_never_rebuild() {
        // 这是本功能的核心场景：锁屏时实例必须**留在原地**。若这里判成 Rebuild，
        // 用户一锁屏实例就重启，远程会话直接掉线——功能反而坏了。
        for event in [session::WTS_SESSION_LOCK, session::WTS_SESSION_UNLOCK] {
            for supervised in [None, Some(1), Some(2)] {
                for event_session in [1_u32, 2, 7] {
                    assert_eq!(
                        classify(event, event_session, supervised),
                        SessionChangeAction::Ignore,
                        "事件 {event:#x} 会话 {event_session} 监督 {supervised:?} 不该重建"
                    );
                }
            }
        }
    }

    #[test]
    fn logoff_of_the_supervised_session_rebuilds() {
        // 实例所在的会话注销了，它还留在那里就是个死进程。
        for event in [
            session::WTS_SESSION_LOGOFF,
            session::WTS_SESSION_TERMINATE,
            session::WTS_CONSOLE_DISCONNECT,
        ] {
            assert_eq!(
                classify(event, 1, Some(1)),
                SessionChangeAction::Rebuild,
                "事件 {event:#x}"
            );
        }
    }

    #[test]
    fn a_console_connect_to_another_session_rebuilds() {
        // 快速用户切换 / 另一个用户登录到控制台：`WTS_CONSOLE_CONNECT` 就是「有会话
        // 接到了物理屏幕」这件事本身，实例必须跟过去。
        assert_eq!(
            classify(session::WTS_CONSOLE_CONNECT, 2, Some(1)),
            SessionChangeAction::Rebuild
        );
    }

    #[test]
    fn a_logon_into_another_session_does_not_rebuild() {
        // `WTS_SESSION_LOGON` 对**任何**会话都发：RDP 登录进一个非控制台会话时，控制台
        // 压根没动。若判成重建，`wait_for_console_session` 会把**同一个**控制台会话原样
        // 交回来，实例被无谓地拆掉重建——一次多余的重建就掐断正在进行的远程会话。
        assert_eq!(
            classify(session::WTS_SESSION_LOGON, 2, Some(1)),
            SessionChangeAction::Ignore
        );
        // 落在**自己**会话里的登录事件同样忽略（它不在「会话要走掉」那张表里）。
        assert_eq!(
            classify(session::WTS_SESSION_LOGON, 1, Some(1)),
            SessionChangeAction::Ignore
        );
    }

    #[test]
    fn unrelated_events_in_other_sessions_do_not_rebuild() {
        // 一次多余的重建会掐断正在进行的远程会话，代价远大于收益——所以别处的
        // RDP 接入/断开、别处有人登录、别处会话被终止、别处被远程控制都不该动我们的实例。
        for event in [
            session::WTS_REMOTE_CONNECT,
            session::WTS_REMOTE_DISCONNECT,
            session::WTS_SESSION_CREATE,
            session::WTS_SESSION_REMOTE_CONTROL,
            session::WTS_SESSION_TERMINATE,
            session::WTS_SESSION_LOGOFF,
            session::WTS_SESSION_LOGON,
        ] {
            assert_eq!(
                classify(event, 9, Some(1)),
                SessionChangeAction::Ignore,
                "事件 {event:#x} 落在别的会话，不该重建"
            );
        }
    }

    #[test]
    fn nothing_to_rebuild_before_an_instance_exists() {
        // 还没拉起实例时收到会话变化：没有旧实例可换，忽略（随后的
        // `wait_for_console_session` 会自己拿到当时正确的会话）。
        for event in [
            session::WTS_SESSION_LOGON,
            session::WTS_CONSOLE_CONNECT,
            session::WTS_SESSION_LOGOFF,
        ] {
            assert_eq!(classify(event, 1, None), SessionChangeAction::Ignore);
        }
    }

    #[test]
    fn the_decision_is_idempotent() {
        // 幂等：重复到达的同一条事件必须给出同一个答案，否则「清标志」的时机一变
        // 就会多重建一次（或漏一次）。
        for event in [
            session::WTS_SESSION_LOCK,
            session::WTS_SESSION_UNLOCK,
            session::WTS_SESSION_LOGON,
            session::WTS_SESSION_LOGOFF,
            session::WTS_CONSOLE_CONNECT,
            session::WTS_CONSOLE_DISCONNECT,
            session::WTS_REMOTE_CONNECT,
        ] {
            for supervised in [None, Some(1), Some(2)] {
                let first = classify(event, 2, supervised);
                for _ in 0..3 {
                    assert_eq!(classify(event, 2, supervised), first);
                }
            }
        }
    }

    #[test]
    fn an_unknown_event_type_is_ignored_rather_than_treated_as_a_change() {
        // 没建模的事件码不该被当成「会话变了」——那会让未来的 Windows 版本悄悄
        // 变成「每次未知事件都重启实例」。
        assert_eq!(classify(0xDEAD, 1, Some(1)), SessionChangeAction::Ignore);
        assert_eq!(classify(0xDEAD, 2, Some(1)), SessionChangeAction::Ignore);
    }

    #[test]
    fn backoff_never_busy_loops_and_never_overflows() {
        // 第一档必须 > 0：实例秒退时 0 退避会让服务陷入忙循环。
        assert!(backoff_for(1) >= Duration::from_secs(1));
        assert!(backoff_for(0) >= Duration::from_secs(1), "0 也要退避");
        // 单调不减（不要求严格递增，但绝不能越退越短）。
        let mut previous = Duration::ZERO;
        for attempt in 1..=40 {
            let wait = backoff_for(attempt);
            assert!(wait >= previous, "第 {attempt} 次退避 {wait:?} 比上一次短");
            previous = wait;
        }
        // 封顶：失败一万次也不该退避到天荒地老（无人值守下退避太久 = 长时间失联）。
        assert_eq!(backoff_for(10_000), Duration::from_secs(60));
        assert_eq!(backoff_for(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn backoff_ladder_is_the_documented_one() {
        assert_eq!(BACKOFF_SECONDS, [1, 2, 5, 15, 30, 60]);
        assert_eq!(backoff_for(1), Duration::from_secs(1));
        assert_eq!(backoff_for(3), Duration::from_secs(5));
        assert_eq!(backoff_for(6), Duration::from_secs(60));
        // 第七次仍停在最后一档。
        assert_eq!(backoff_for(7), Duration::from_secs(60));
    }

    #[test]
    fn instance_args_follow_the_documented_order() {
        let args = instance_args(
            Path::new(r"C:\ProgramData\rust-tunnel\host.toml"),
            Path::new(r"C:\ProgramData\rust-tunnel\share"),
            Path::new(r"C:\ProgramData\rust-tunnel\download"),
        );
        assert_eq!(
            args,
            vec![
                "--instance",
                "--config",
                r"C:\ProgramData\rust-tunnel\host.toml",
                "--share",
                r"C:\ProgramData\rust-tunnel\share",
                "--download",
                r"C:\ProgramData\rust-tunnel\download",
            ]
        );
    }

    #[test]
    fn instance_command_line_quotes_paths_with_spaces() {
        // 路径里带空格时不加引号，实例会读到半个路径——而 `%ProgramFiles%` 恰好带空格。
        let line = instance_command_line(
            Path::new(r"C:\Program Files\rust-tunnel\rust-tunnel-client-gui.exe"),
            Path::new(r"C:\ProgramData\rust-tunnel\host.toml"),
            Path::new(r"C:\ProgramData\rust-tunnel\share"),
            Path::new(r"C:\ProgramData\rust-tunnel\download"),
        );
        assert_eq!(
            line,
            r#""C:\Program Files\rust-tunnel\rust-tunnel-client-gui.exe" --instance --config C:\ProgramData\rust-tunnel\host.toml --share C:\ProgramData\rust-tunnel\share --download C:\ProgramData\rust-tunnel\download"#
        );
    }

    #[test]
    fn instance_command_line_quotes_an_argument_that_contains_a_space() {
        // 共享目录被用户改到带空格的路径上时也必须正确引号。
        let line = instance_command_line(
            Path::new(r"C:\gui.exe"),
            Path::new(r"C:\ProgramData\rust-tunnel\host.toml"),
            Path::new(r"C:\my share\dir"),
            Path::new(r"C:\ProgramData\rust-tunnel\download"),
        );
        assert!(line.contains(r#""C:\my share\dir""#), "{line}");
    }

    /// 拒跑判据：两条前置条件都在才放行；缺哪条就报哪条，且两条消息都必须**点名路径**、
    /// **说清服务会怎么做**（自行停止 + 交回身份），并给出正确的下一步。
    #[test]
    fn refuse_reason_covers_both_preconditions_and_their_wording() {
        fn prec(path: &Path, present: bool) -> Precondition<'_> {
            Precondition { path, present }
        }
        let host = Path::new(r"C:\ProgramData\rust-tunnel\host.toml");
        let exe = Path::new(r"C:\Program Files\rust-tunnel\rust-tunnel-client-gui.exe");

        // 两条都在：不拒跑（这是**唯一**放行的那一格）。
        assert_eq!(refuse_reason(prec(host, true), prec(exe, true)), None);

        // 缺 host.toml：点名文件、说清后果、说清服务会怎么做、给出「先写再启动」的顺序与文档。
        let reason = refuse_reason(prec(host, false), prec(exe, true)).unwrap();
        assert!(
            reason.contains(r"C:\ProgramData\rust-tunnel\host.toml"),
            "{reason}"
        );
        assert!(reason.contains("失去客户端身份"), "{reason}");
        assert!(reason.contains("自行停止"), "{reason}");
        assert!(reason.contains("GUI"), "{reason}");
        assert!(reason.contains("先写 host.toml"), "{reason}");
        assert!(reason.contains("WINDOWS-SECURE-DESKTOP.md"), "{reason}");

        // 缺实例 exe：点名路径、说清服务会怎么做、指向 --install。
        let reason = refuse_reason(prec(host, true), prec(exe, false)).unwrap();
        assert!(reason.contains("rust-tunnel-client-gui.exe"), "{reason}");
        assert!(reason.contains("自行停止"), "{reason}");
        assert!(reason.contains("GUI"), "{reason}");
        assert!(reason.contains("--install"), "{reason}");

        // 两条都缺：host.toml 那条优先（它是「先写 host.toml」这个顺序里的第一步，
        // 也是用户更常踩的那一步）。
        let both = refuse_reason(prec(host, false), prec(exe, false)).unwrap();
        assert!(both.contains("host.toml"), "{both}");
        assert!(
            !both.contains("--install"),
            "两条都缺时不该报 exe 那条：{both}"
        );
    }
}
