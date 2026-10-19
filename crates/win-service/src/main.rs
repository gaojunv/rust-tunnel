// 测试代码豁免 panic 风险 lint（生产代码仍告警）。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! `rust-tunnel-client-service`：Windows 无人值守被控端的 **SYSTEM 服务**。
//!
//! # 它为什么必须存在
//!
//! 从别的机器远程控制一台 **Windows 自己的锁屏/登录界面**时，用户态的采集与注入
//! 都不成立，三层原因缺一不可：`Winlogon` 桌面的 ACL 不放行 Administrators、
//! UIPI 挡住低于 System 完整性的进程、会话隔离把服务挡在会话 0。所以
//! 「以管理员身份运行」**无效**——唯一正路是像 RustDesk/TeamViewer 那样装一个
//! SYSTEM 服务，由它把工作进程拉进控制台会话。
//!
//! 本文件只是入口；两个身份的实际逻辑在：
//! - [`args`] —— 闭合的命令行解析（哪几个参数、什么组合，一眼看完）
//! - [`supervise`] —— 监督策略的**纯逻辑**（会话变化 → 是否重建、退避阶梯、实例命令行），
//!   无平台分支，故在 macOS 上也有单测
//! - `host`（仅 Windows）—— 控制台工具 / 服务进程 / 子实例三条路径
//!
//! 平台层（采集、注入、令牌、会话、服务控制、目录加固）在 `rust-tunnel-win-platform`。
//!
//! # 当前进度：阶段 1（服务本体）
//!
//! `--run` 已经是**真正的服务本体**：等控制台会话 → 取令牌 → 用
//! `CreateProcessAsUserW` 把 `rust-tunnel-client-gui.exe --instance` 拉进控制台会话 →
//! 放进 `KILL_ON_JOB_CLOSE` 的作业对象 → 监督（退出即退避重启，会话变化即重建）。
//! `--install` 会把服务与实例两个 exe 复制到 `%ProgramFiles%\rust-tunnel\`（并把 DACL
//! 收敛到 SYSTEM + Administrators），再注册服务。阶段 0 的真机探针（`--probe*`）原样保留。
//! 交付与验收流程见根目录 `WINDOWS-SECURE-DESKTOP.md`。
//!
//! # 非 Windows 上编译成什么
//!
//! 本 crate 是 workspace 成员，根目录 `cargo build` 会构建它。macOS/Linux 上
//! `main` 只打印一句「本程序只在 Windows 上有意义」并以码 2 退出——这样
//! **可移植的 `args` 模块仍能在本机跑单测**，而平台专属代码一行都不参与编译。

mod args;
mod supervise;

#[cfg(windows)]
mod host;

#[cfg(windows)]
fn main() {
    host::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("本程序只在 Windows 上有意义（无人值守被控端的 SYSTEM 服务）。");
    eprintln!("命令一览：\n");
    eprintln!("{}", args::USAGE);
    std::process::exit(2);
}
