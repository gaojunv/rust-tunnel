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
//! - `host`（仅 Windows）—— 控制台工具 / 服务进程 / 子实例三条路径
//!
//! 平台层（采集、注入、令牌、会话、服务控制）在 `rust-tunnel-win-platform`。
//!
//! # 当前进度：阶段 0（探针）
//!
//! 现在这个 exe 只会跑**真机探针**：把「SYSTEM 令牌能不能把进程拉进控制台会话」
//! 与「在 Winlogon 桌面上 `SendInput` 到底落不落地」两件事打到真机上出结论。
//! 阶段 1 才会把子实例换成真正的客户端（`rust-tunnel-client-gui.exe --instance`）。
//! 交付与验收流程见根目录 `WINDOWS-SECURE-DESKTOP.md`。
//!
//! # 非 Windows 上编译成什么
//!
//! 本 crate 是 workspace 成员，根目录 `cargo build` 会构建它。macOS/Linux 上
//! `main` 只打印一句「本程序只在 Windows 上有意义」并以码 2 退出——这样
//! **可移植的 `args` 模块仍能在本机跑单测**，而平台专属代码一行都不参与编译。

mod args;

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
