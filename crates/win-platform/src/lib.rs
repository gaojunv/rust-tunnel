// 测试代码豁免 panic 风险 lint（生产代码仍告警）。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Windows 无人值守被控端的平台层：安全桌面采集/注入、会话与令牌、服务控制。
//!
//! # 这个 crate 解决什么问题
//!
//! 从别的机器远程控制一台 **Windows 自己的锁屏/登录界面**（`WinSta0\Winlogon` 桌面）
//! 时，用户态的采集与注入都不成立，原因有三层，缺一不可：
//!
//! 1. **桌面 ACL**：`Winlogon` 桌面的安全描述符只放行极少数账号（含 LocalSystem），
//!    Administrators **不在**名单里——所以「以管理员身份运行」无效。
//! 2. **UIPI**：LogonUI 跑在 System 完整性级别，低完整性的进程发不进消息。
//! 3. **会话隔离**：服务在会话 0，用户在会话 1+；不显式指定会话就永远够不着。
//!
//! 唯一正路是像 RustDesk/TeamViewer 那样：一个 **SYSTEM 服务**把进程拉进**控制台会话**，
//! 让该进程自己 `OpenInputDesktop` + `SetThreadDesktop` 挂到当时的输入桌面。
//!
//! # 分层（决定了本 crate 能有多少东西在 macOS 上被验证）
//!
//! - **可移植层**（任何平台都编译、都有单测）：[`error`]（错误类型）、[`pixels`]
//!   （BGRA→RGBA、ASCII 缩略图、BMP 编码）、[`report`]（探针报告）、[`session`]
//!   （控制台会话语义）、[`desktop`]（桌面名分类）、[`monitor`]（显示器/虚拟桌面几何与
//!   绝对坐标归一化）、[`inject`]（注入步骤与规划）、[`token`]（完整性级别 RID 语义）、
//!   [`sysinfo`]（时间格式化）。**纯 std、零依赖**，是设计风险的大头。
//! - **FFI 层**（`#[cfg(windows)]`）：[`monitor`]/[`desktop`]/[`capture`]/[`inject`]/
//!   [`token`]/[`process`]/[`service`]/[`sysinfo`] 里那些直接调 Win32 的函数。本机
//!   （macOS）可用 `cargo check --target x86_64-pc-windows-gnu` 做**类型**检查，
//!   但行为只能上真机验。
//!
//! # 为什么自持而不复用 `xcap`/`enigo`
//!
//! 1. 锁屏桌面上 `xcap` 的行为不可控（它枚举的是普通窗口站里的显示器）；
//! 2. **显示器 id 必须由我们自己定义**——控制器是从 `HelloAck` 里学到 id 的，
//!    而 `DisplayInfo` 的字段是**冻结**的（见 `crates/remote/src/screen.rs`），
//!    自持 id 空间就不存在「与 xcap 的 id 对齐」这个问题；
//! 3. 注入本质上就是 `SendInput`（`enigo` 内部也是它），只要调用线程挂在目标桌面上即可。
//!
//! 用户态那条老路径（`crates/client-gui` 里的 `xcap`/`enigo`）**一行都不改**：
//! 今天能用的功能不承担任何回归风险。
//!
//! # 纪律
//!
//! - 本 crate **不得**依赖 `rust-tunnel-remote`（那会把 libvpx 拖进服务 exe，
//!   而服务 exe 走 MSVC、没有预编译库），也不得依赖 `windows`（只做裸 FFI，
//!   用 `windows-sys` 保持零 wrapper 依赖）。
//! - 所有 `unsafe` 都必须带 `// SAFETY:` 说明调用契约。

pub mod desktop;
pub mod error;
pub mod inject;
pub mod monitor;
pub mod pixels;
pub mod process;
pub mod report;
pub mod session;
pub mod sysinfo;
pub mod token;
pub mod wide;

// 纯 FFI 模块：整模块只在 Windows 上编译（macOS/Linux 上连文件都不参与构建）。
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod handle;
#[cfg(windows)]
pub mod probe;
#[cfg(windows)]
pub mod service;

pub use error::{WinError, WinResult};
