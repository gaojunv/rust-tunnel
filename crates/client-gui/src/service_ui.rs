//! 无人值守被控端（Windows 服务）的界面逻辑：**可移植纯逻辑** + `#[cfg(windows)]` 胶水。
//!
//! # 这一层做什么
//!
//! 「设置」Tab 的「无人值守被控端」一节要能看服务状态、装/卸/启/停服务，并把不可逆的后果
//! 讲清楚。四种操作在 Windows 上都要**提权**（要动 SCM、装/卸还要写 `%ProgramFiles%`）——本模块把
//! 「点一下按钮」翻译成一次 `ShellExecuteExW(runas)`（见 `rust_tunnel_win_platform::elevate`），
//! 再把子进程的结果回灌到 UI。
//!
//! # 为什么拆成两半
//!
//! `rust-tunnel-win-platform` 是本 crate 的 **windows-only 依赖**（见 `Cargo.toml` 的
//! `[target.'cfg(target_os = "windows")'.dependencies]`），macOS 上连名字都不存在。于是这里
//! 照 `crate::service_watch` 的办法分两半：**纯逻辑**（参数拼接、候选定位、结果文案、启动前置
//! 条件、状态文案）一行 `#[cfg]` 都不带、能在 macOS 上单测；只有真正碰 Win32/SCM/提权的几条
//! 路径包在 `#[cfg(windows)]` 里。
//!
//! # 为什么每个操作都在后台线程上跑
//!
//! `elevate::run_elevated` **阻塞到子进程结束**（`WaitForSingleObject(INFINITE)`），而
//! `--install` 要复制 exe、加固 DACL、停旧服务，`--stop` 要等它真的停下（各有二十几秒上限）。
//! 放到 UI 线程上就是界面卡死。故每个操作起一条 `std::thread`，结果经 `std::sync::mpsc` 回灌，
//! UI 每帧 [`ServiceOps::poll`] 一次；在途期间按钮禁用（见 [`ServiceOps::in_flight`]）。
//!
//! # 四种操作都经**提权重新拉起服务 exe** 完成
//!
//! 装/卸/启/停都交给服务 exe 自己的 `--install` / `--uninstall` / `--start` / `--stop`
//! （见 `crates/win-service/src/args.rs` 的 `USAGE`）：本进程**从不**自己动 SCM，于是 GUI
//! 不必以管理员身份运行——每次操作弹一次 UAC，四条路完全一致。纯函数 [`op_args`] 给出每个
//! 操作对应的 argv。

// 非 Windows 上真正用得到这些纯逻辑的只有单测（windows 胶水整段不参与编译，bin crate 的
// `pub` 也不构成「外部消费者」），故在本机构建里别把它们报成 dead_code——它们是本模块的
// **核心纯逻辑**，必须在 macOS 上可测（见模块文档）。
#![cfg_attr(
    not(windows),
    allow(dead_code, reason = "非 Windows 无服务；这些纯逻辑只被单测使用")
)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::service_watch::{IdentityOwner, ServicePresence};

/// 本平台是否有「无人值守被控端」这个概念（Windows 服务）。
///
/// 非 Windows 上整节只显示一句「仅 Windows 可用」，托盘那一行也照此措辞——否则会显示成
/// 「服务：未知」，让 macOS 用户去找一个根本不存在的服务。
pub const SERVICE_SUPPORTED: bool = cfg!(target_os = "windows");

/// 服务 exe 的候选文件名（**必须与 `paths::SERVICE_EXE_NAME` 和 `release-client.yml` 的
/// 归档名一致**）。
///
/// 与 `crates/win-service/src/host.rs` 的 `GUI_EXE_CANDIDATES` 是镜像关系：CI 把两个产物
/// **并排**放在同一个目录里，本地构建时 `target/<triple>/release/` 里同样两个都在。故在
/// 「GUI exe 所在目录」下按这两个名字找。这里**不能**引 `rust_tunnel_win_platform::paths`：
/// 那是 windows-only 依赖，而本常量要给可移植函数用。
const SERVICE_EXE_CANDIDATES: [&str; 2] = [
    "rust-tunnel-client-service.exe",
    "rust-tunnel-client-service-windows-x86_64.exe",
];

/// 界面能发起的四种服务操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceOp {
    /// 安装服务（复制 exe、加固 DACL、注册为自动启动）。**要提权。**
    Install,
    /// 卸载服务（幂等；不删已复制的文件）。**要提权。**
    Uninstall,
    /// 启动服务（服务 exe 的 `--start` → `StartServiceW`）。**要提权**（经服务 exe，见模块文档）。
    Start,
    /// 停止服务（服务 exe 的 `--stop` → `ControlService(SERVICE_CONTROL_STOP)`）。**要提权**。
    Stop,
}

impl ServiceOp {
    /// 动作名（日志与结果文案用）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Install => "安装服务",
            Self::Uninstall => "卸载服务",
            Self::Start => "启动服务",
            Self::Stop => "停止服务",
        }
    }
}

/// 一次操作交给服务 exe 的 argv（**纯函数**）。
///
/// 四种操作的 argv 都由 runner 经 `elevate::run_elevated` 交给服务 exe（见模块文档）；
/// `Install` 在有 GUI exe 路径时额外带上 `--gui-exe <路径>`，让服务侧把**正在运行的**这个 GUI
/// 复制进 `%ProgramFiles%`（而不是去猜归档名）。其余三种操作忽略 `gui_exe`。
#[must_use]
pub fn op_args(op: ServiceOp, gui_exe: Option<&Path>) -> Vec<String> {
    match op {
        ServiceOp::Install => {
            let mut args = vec![String::from("--install")];
            if let Some(path) = gui_exe {
                args.push(String::from("--gui-exe"));
                args.push(path.to_string_lossy().into_owned());
            }
            args
        }
        ServiceOp::Uninstall => vec![String::from("--uninstall")],
        ServiceOp::Start => vec![String::from("--start")],
        ServiceOp::Stop => vec![String::from("--stop")],
    }
}

/// 服务 exe 的候选路径：**先安装后的固定落点**，再「GUI exe 同目录」下的归档名/本地构建名。
///
/// `installed` 由调用方传入（windows 上就是 `paths::installed_service_exe()`）而不是在这里
/// 现算：算它要读 `%ProgramW6432%`/`%ProgramFiles%`，那份逻辑在 windows-only 的
/// `rust_tunnel_win_platform::paths` 里，本函数必须保持可移植（见模块文档）。
#[must_use]
pub fn service_exe_candidates(installed: &Path, gui_exe_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(SERVICE_EXE_CANDIDATES.len() + 1);
    out.push(installed.to_path_buf());
    for name in SERVICE_EXE_CANDIDATES {
        out.push(gui_exe_dir.join(name));
    }
    out
}

/// 取候选列表里**第一个存在**的文件（顺序即优先级）。**纯函数**，可用临时目录测。
#[must_use]
pub fn locate_service_exe(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|path| path.is_file()).cloned()
}

/// 一次操作的结局（[`rust_tunnel_win_platform::elevate::ElevationOutcome`] 的**可移植镜像**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceOpOutcome {
    /// 操作完成（子进程退出码为 0，或 SCM 调用成功）。
    Ok,
    /// 用户在 UAC 提示上点了「否」。**不是错误**——调用方应当**静默**处理。
    Cancelled,
    /// 失败，附一句给用户看的说明。
    Failed(String),
}

/// 结果文案：`Ok` → 一行成功语；`Cancelled` → **`None`（静默）**；`Failed` → 错误文本。
///
/// `Cancelled` 必须静默：用户主动取消了 UAC，再弹一句「安装失败」是错的。
#[must_use]
pub fn outcome_message(op: ServiceOp, outcome: &ServiceOpOutcome) -> Option<String> {
    match outcome {
        ServiceOpOutcome::Ok => Some(format!("{}成功", op.label())),
        ServiceOpOutcome::Cancelled => None,
        ServiceOpOutcome::Failed(err) => Some(format!("{}失败：{err}", op.label())),
    }
}

/// 启动服务前的前置条件；返回 `None` 表示**可以启动**。
///
/// # 为什么必须拦（以及这道拦为什么只是**礼貌**）
///
/// `--install` **刻意不启动**服务，只把它登记成自动启动。原因是一条链：
///
/// 1. 服务实例的配置来自 `%ProgramData%\rust-tunnel\host.toml`，而那要**运维手工写**——它含
///    服务端**全局 token**，只有服务端 Web 界面能给（见 `win-service` 的 `USAGE` 与
///    `WINDOWS-SECURE-DESKTOP.md` 的安全模型）。
/// 2. 服务一 `Running`，本窗口就转被动、**不再注册**（见 `crate::service_watch`）。
/// 3. 于是若在 `host.toml` 不存在时就启动服务：**没人注册**，整机**彻底失去客户端身份**、
///    变成不可接入，而界面还写着「服务在承载身份」。
///
/// 但**这道检查本身不可信**：`%ProgramData%\rust-tunnel` 的 DACL 只放行 SYSTEM 与
/// Administrators，本窗口（未提权）多半根本 stat 不进去（见 [`HostConfigPresence`]）。
/// 所以它只在**确定**缺失（`Absent`）时才拦——那能替用户省一次白跑的 UAC 提示；真正可信的
/// 闸门在**服务侧**（缺 `host.toml` 时服务会拒跑并自停，见 `win-service` 的 `refuse_reason`），
/// 因为只有服务进程有权限看那个目录。
///
/// # `Unknown` 必须**放行**（fail open）
///
/// 把「读不到」当成「不存在」会让启动按钮被**永久禁用**并给出一个错的解释——那正是这个
/// 函数曾经的 bug（`Path::is_file()` 把 `ACCESS_DENIED` 折叠成 `false`）。
#[must_use]
pub fn start_blocked_reason(presence: HostConfigPresence) -> Option<&'static str> {
    match presence {
        HostConfigPresence::Absent => Some(
            "host.toml 尚未创建：服务一旦启动，本窗口会转为被动、不再注册，而服务实例读不到配置\
             也注册不了——整机将失去客户端身份、彻底无法接入。请先写好 \
             %ProgramData%\\rust-tunnel\\host.toml（服务端地址 + 客户端 token + 审批策略）再启动。",
        ),
        HostConfigPresence::Present | HostConfigPresence::Unknown => None,
    }
}

/// `host.toml` 的在场情况。**三态**：`Absent` 与 `Unknown` 必须分开——见下。
///
/// `--install` 会把 `%ProgramData%\rust-tunnel` 的 DACL 收敛到「仅 SYSTEM + Administrators、
/// 无继承」。于是普通用户——以及**以 UAC 分离令牌运行的未提权管理员**（常见情形）——根本
/// stat 不进那个目录，`std::fs::metadata` 报 `ACCESS_DENIED`。那**不等于**文件不存在：
/// 前者是「本窗口没权限看」，后者才是「文件真的没有」。把两者混成一个 `false` 会让启动按钮
/// 被**永久禁用**，还配一句错的解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostConfigPresence {
    /// 文件在（本窗口读得到）。
    Present,
    /// 文件**确定**不在（`ErrorKind::NotFound`）。
    Absent,
    /// 判断不了（权限不足等；非 Windows 恒为此）。
    Unknown,
}

/// `host.toml` 的在场情况（见 [`HostConfigPresence`] 的三态理由）。
///
/// 用 `metadata` 而不是 `Path::is_file()`：后者把**一切**错误折叠成 `false`，`ACCESS_DENIED`
/// 与 `NotFound` 就此不可分。
#[must_use]
pub fn host_config_presence() -> HostConfigPresence {
    #[cfg(windows)]
    {
        match std::fs::metadata(rust_tunnel_win_platform::paths::host_config_path()) {
            Ok(_) => HostConfigPresence::Present,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HostConfigPresence::Absent,
            // 其余一律 `Unknown`：`ACCESS_DENIED` 是 `PermissionDenied`，但任何别的错误也
            // 同样不该被当成「文件不存在」。
            Err(_) => HostConfigPresence::Unknown,
        }
    }
    #[cfg(not(windows))]
    {
        // 那条路径在非 Windows 上不存在；这一节本就不渲染（见 [`SERVICE_SUPPORTED`]）。
        HostConfigPresence::Unknown
    }
}

/// 三态里 `Unknown` 的补充说明（**信息**，不是警告）：解释为什么读不到、以及读不到时的后果。
///
/// `Present` / `Absent` → `None`：前者的结论就是「没问题」，后者已由 [`start_blocked_reason`]
/// 给出警告文案，再叠一句只会重复。
#[must_use]
pub fn host_config_advisory(presence: HostConfigPresence) -> Option<&'static str> {
    match presence {
        HostConfigPresence::Unknown => Some(
            "本窗口读不到 %ProgramData%\\rust-tunnel（该目录的 DACL 只放行 SYSTEM 与 \
             Administrators，未提权的窗口 stat 不进去——这是**预期**，不是故障），因此无法确认 \
             host.toml 是否已存在。若它其实不存在，服务会自行停止并把客户端身份交回本窗口。",
        ),
        HostConfigPresence::Present | HostConfigPresence::Absent => None,
    }
}

/// 服务在场情况的短标签（托盘与设置页共用，措辞与 `crate::service_watch` 一致）。
#[must_use]
pub fn presence_label(presence: ServicePresence) -> &'static str {
    match presence {
        ServicePresence::Unknown => "未知",
        ServicePresence::Absent => "未安装",
        ServicePresence::Stopped => "已停止",
        ServicePresence::Pending => "启动/停止中",
        ServicePresence::Running => "运行中",
    }
}

/// 托盘只读项那一行：`服务：<状态>`（非 Windows 上明说仅 Windows 可用）。
#[must_use]
pub fn service_menu_text(presence: ServicePresence) -> String {
    if SERVICE_SUPPORTED {
        format!("服务：{}", presence_label(presence))
    } else {
        String::from("服务：仅 Windows 可用")
    }
}

/// 谁在承载客户端身份（设置页与状态页说法一致）。
#[must_use]
pub fn owner_label(owner: IdentityOwner) -> &'static str {
    match owner {
        IdentityOwner::Gui => "本窗口",
        IdentityOwner::Service => "Windows 服务实例",
    }
}

/// `host.toml` 的路径（给用户看的那一行）。
#[must_use]
pub fn host_config_path_display() -> String {
    #[cfg(windows)]
    {
        rust_tunnel_win_platform::paths::host_config_path()
            .display()
            .to_string()
    }
    #[cfg(not(windows))]
    {
        String::from("%ProgramData%\\rust-tunnel\\host.toml")
    }
}

/// 服务操作与状态的跨帧持有物：在途操作、最近结果、服务在场情况的缓存。
///
/// 放在 `GuiApp` 上（见 `main.rs`）而不是某一帧的局部变量里：一次 `--install` 要跑二十秒，
/// 期间「正在安装…」必须一直显示，结果也必须留到下一帧。
#[derive(Debug, Default)]
pub struct ServiceOps {
    /// 在途操作（`None` = 空闲）。同时在途只允许一个（按钮在途期间禁用）。
    in_flight: Option<ServiceOp>,
    /// 最近一次操作的结果文案（`None` = 不显示；`Cancelled` 也留 `None`，即静默）。
    last_message: Option<String>,
    /// 后台线程回灌结果的接收端（有它即代表有操作在途）。
    rx: Option<Receiver<ServiceOpOutcome>>,
    /// 服务在场情况的 1 秒缓存：设置页与托盘每帧都要读，而一次查询要开 SCM 三次系统调用。
    state_cache: Option<(Instant, ServicePresence)>,
}

impl ServiceOps {
    /// 是否有操作在途（按钮据此禁用）。
    #[must_use]
    pub fn in_flight(&self) -> Option<ServiceOp> {
        self.in_flight
    }

    /// 最近一次操作的结果文案。
    #[must_use]
    pub fn last_message(&self) -> Option<&str> {
        self.last_message.as_deref()
    }

    /// 当前服务在场情况（windows 上带 1 秒缓存；非 windows 恒 `Unknown`）。
    #[must_use]
    pub fn presence(&mut self) -> ServicePresence {
        /// 缓存有效期：够长以省下每帧的 SCM 查询，够短以让启停/装完后的状态及时反映。
        const TTL: Duration = Duration::from_secs(1);
        if let Some((at, cached)) = self.state_cache {
            if at.elapsed() < TTL {
                return cached;
            }
        }
        let fresh = query_presence();
        self.state_cache = Some((Instant::now(), fresh));
        fresh
    }

    /// 抽干结果通道；返回 `true` 表示状态有更新（调用方据此 `request_repaint`）。
    pub fn poll(&mut self) -> bool {
        let Some(rx) = self.rx.as_ref() else {
            return false;
        };
        // 先把结果取出来（`try_recv` 返回**拥有所有权**的值），下面才好改 `self.rx`。
        let result = rx.try_recv();
        match result {
            Err(TryRecvError::Empty) => false,
            Ok(outcome) => {
                if let Some(op) = self.in_flight.take() {
                    self.last_message = outcome_message(op, &outcome);
                }
                self.rx = None;
                // 操作改变了服务状态：让缓存立即失效，下一帧重新查询。
                self.state_cache = None;
                true
            }
            Err(TryRecvError::Disconnected) => {
                // 后台线程在送出结果前就没了（panic 等）：当失败处理，并清掉在途态。
                if let Some(op) = self.in_flight.take() {
                    self.last_message = Some(format!("{}失败：后台任务异常结束", op.label()));
                }
                self.rx = None;
                true
            }
        }
    }

    /// 起一个后台线程执行操作，结果经通道回灌（见模块文档的线程模型）。
    #[cfg(windows)]
    pub fn spawn_op(&mut self, op: ServiceOp) {
        if self.in_flight.is_some() {
            // 已有操作在途：忽略重复点击（按钮本应已禁用，这里是第二道闸）。
            return;
        }
        self.in_flight = Some(op);
        self.last_message = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let outcome = run_op(op);
            // 发送失败只可能是 UI 已退出——不是错误。
            let _ = tx.send(outcome);
        });
    }

    /// 非 Windows：无人值守被控端不可用，直接把话说清楚（UI 里那一节本就不出按钮）。
    #[cfg(not(windows))]
    pub fn spawn_op(&mut self, op: ServiceOp) {
        self.last_message = Some(format!("{}：无人值守被控端仅在 Windows 上可用", op.label()));
    }
}

/// 查询 SCM 并映射成本模块的 [`ServicePresence`]。
///
/// 映射复用 `crate::service_watch::presence_from_state`（**唯一一份**）：设置页与托盘对同一个
/// 状态必须给出同一个说法，各写一份迟早漂移。
#[cfg(windows)]
fn query_presence() -> ServicePresence {
    match rust_tunnel_win_platform::service::query_state() {
        Ok(state) => crate::service_watch::presence_from_state(state),
        Err(err) => {
            tracing::debug!(?err, "查询 Windows 服务状态失败");
            ServicePresence::Unknown
        }
    }
}

/// 非 Windows：没有 Windows 服务可查。
#[cfg(not(windows))]
fn query_presence() -> ServicePresence {
    ServicePresence::Unknown
}

// ── Windows 胶水：提权 / SCM / 结果 ──────────────────────────────

/// 正在运行的 GUI exe（装服务时把它复制进 `%ProgramFiles%`）。
#[cfg(windows)]
fn current_gui_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// 正在运行的 GUI exe 所在目录（服务 exe 与它并排时就在这儿找）。
#[cfg(windows)]
fn current_gui_exe_dir() -> PathBuf {
    current_gui_exe()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 把候选路径拼成一句给用户看的列表。
#[cfg(windows)]
fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("、")
}

/// 执行一次操作：**唯一**路径——提权重新拉起服务 exe（UAC），等它结束并读它的退出码。
///
/// 四个操作一视同仁（见模块文档）；不再有「本进程已提权就直接调 SCM」那条近路。
#[cfg(windows)]
fn run_op(op: ServiceOp) -> ServiceOpOutcome {
    use rust_tunnel_win_platform::{elevate, paths};

    let gui_exe_dir = current_gui_exe_dir();
    let candidates = service_exe_candidates(&paths::installed_service_exe(), &gui_exe_dir);
    let Some(exe) = locate_service_exe(&candidates) else {
        return ServiceOpOutcome::Failed(format!(
            "找不到服务 exe（找过：{}）",
            join_paths(&candidates)
        ));
    };
    let args = op_args(op, current_gui_exe().as_deref());
    match elevate::run_elevated(&exe, &args) {
        Ok(elevate::ElevationOutcome::Completed { exit_code: 0 }) => ServiceOpOutcome::Ok,
        Ok(elevate::ElevationOutcome::Completed { exit_code }) => {
            ServiceOpOutcome::Failed(format!("服务 exe 退出码 {exit_code}"))
        }
        // 用户点了 UAC 的「否」：合法选择，不是错误。
        Ok(elevate::ElevationOutcome::Cancelled) => ServiceOpOutcome::Cancelled,
        Err(err) => ServiceOpOutcome::Failed(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    //! 纯逻辑单测（不碰 Win32/SCM/提权，故本机可跑）。

    use super::*;
    use crate::service_watch::ServicePresence::{Absent, Pending, Running, Stopped, Unknown};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rust-tunnel-service-ui-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 表驱动：四种操作 × 有无 GUI exe。
    #[test]
    fn op_args_table() {
        let cases: &[(ServiceOp, Option<&str>, &[&str])] = &[
            (ServiceOp::Install, None, &["--install"]),
            (
                ServiceOp::Install,
                Some(r"C:\dl\rust-tunnel-client-gui.exe"),
                &[
                    "--install",
                    "--gui-exe",
                    r"C:\dl\rust-tunnel-client-gui.exe",
                ],
            ),
            (ServiceOp::Uninstall, None, &["--uninstall"]),
            // 卸载不接受 --gui-exe：即使给了也不该出现在参数里。
            (ServiceOp::Uninstall, Some(r"C:\x.exe"), &["--uninstall"]),
            (ServiceOp::Start, None, &["--start"]),
            (ServiceOp::Stop, None, &["--stop"]),
        ];
        for (op, gui, expected) in cases {
            let got = op_args(*op, gui.map(Path::new));
            let want: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(got, want, "{op:?} + {gui:?}");
        }
        // 表必须覆盖「四种操作 × 有无路径」的全部组合。
        assert_eq!(cases.len(), 6);
    }

    /// 四种操作**都**经提权重新拉起服务 exe（模块文档）——本模块只有这一条到达服务 exe 的路径。
    ///
    /// 机械锁落在 [`op_args`] 上：每个操作都必须映射到服务 exe 的一条旗标（`--install` /
    /// `--uninstall` / `--start` / `--stop`），runner 拿到它就去 `elevate::run_elevated`。若将来
    /// 给某个操作加一条不走服务 exe 的近路，它会没有 argv，这里先炸。
    #[test]
    fn every_service_op_is_an_elevated_launch() {
        for op in [
            ServiceOp::Install,
            ServiceOp::Uninstall,
            ServiceOp::Start,
            ServiceOp::Stop,
        ] {
            let args = op_args(op, None);
            assert_eq!(
                args.len(),
                1,
                "{op:?} 应当恰好映射到一个服务 exe 旗标，得到 {args:?}"
            );
            assert!(args[0].starts_with("--"), "{op:?}: {args:?}");
        }
    }

    /// `locate_service_exe`：全不存在 → `None`；有存在的 → 返回它；多个存在 → **第一个**胜出。
    #[test]
    fn locate_service_exe_is_first_existing_and_none_when_absent() {
        let dir = temp_dir("locate");
        let absent = dir.join("absent.exe");
        let first = dir.join("first.exe");
        let second = dir.join("second.exe");

        assert_eq!(locate_service_exe(&[absent.clone(), first.clone()]), None);

        std::fs::write(&second, b"b").unwrap();
        assert_eq!(
            locate_service_exe(&[absent.clone(), second.clone()]),
            Some(second.clone())
        );

        std::fs::write(&first, b"a").unwrap();
        assert_eq!(
            locate_service_exe(&[first.clone(), second.clone()]),
            Some(first.clone())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 候选顺序：**安装后的固定落点在最前**，其后是 GUI exe 同目录下的两个名字。
    #[test]
    fn service_exe_candidates_puts_the_installed_path_first() {
        let installed = Path::new(r"C:\Program Files\rust-tunnel\rust-tunnel-client-service.exe");
        let dir = Path::new(r"C:\dl");
        let got = service_exe_candidates(installed, dir);
        assert_eq!(got[0], installed);
        assert_eq!(got[1], dir.join("rust-tunnel-client-service.exe"));
        assert_eq!(
            got[2],
            dir.join("rust-tunnel-client-service-windows-x86_64.exe")
        );
        assert_eq!(got.len(), SERVICE_EXE_CANDIDATES.len() + 1);
        // 两个候选名必须互不相同且都是 `.exe`（按 Path 的扩展名判，避开大小写比较）。
        assert_ne!(SERVICE_EXE_CANDIDATES[0], SERVICE_EXE_CANDIDATES[1]);
        for name in SERVICE_EXE_CANDIDATES {
            assert_eq!(
                Path::new(name).extension().and_then(|ext| ext.to_str()),
                Some("exe"),
                "{name}"
            );
        }
    }

    /// 结果文案：`Ok` 说成功；**`Cancelled` 静默**；`Failed` 带错误文本。
    #[test]
    fn outcome_message_is_silent_on_cancellation() {
        for op in [
            ServiceOp::Install,
            ServiceOp::Uninstall,
            ServiceOp::Start,
            ServiceOp::Stop,
        ] {
            let ok = outcome_message(op, &ServiceOpOutcome::Ok).unwrap();
            assert!(ok.contains(op.label()), "{ok}");
            // 用户主动取消 UAC：**静默**，不是错误。
            assert_eq!(outcome_message(op, &ServiceOpOutcome::Cancelled), None);
            let failed =
                outcome_message(op, &ServiceOpOutcome::Failed(String::from("boom"))).unwrap();
            assert!(failed.contains(op.label()), "{failed}");
            assert!(failed.contains("boom"), "{failed}");
        }
    }

    /// 启动前置条件：三态表。**`Unknown` 必须放行**（fail open）——本窗口多半根本读不到
    /// 那个目录（DACL 只放行 SYSTEM + Administrators），把「读不到」当成「不存在」正是
    /// 启动按钮被**永久禁用**的那个 bug。
    #[test]
    fn start_is_blocked_only_when_host_config_is_known_absent() {
        let cases = [
            (HostConfigPresence::Present, false),
            (HostConfigPresence::Unknown, false),
            (HostConfigPresence::Absent, true),
        ];
        for (presence, blocked) in cases {
            assert_eq!(
                start_blocked_reason(presence).is_some(),
                blocked,
                "{presence:?}"
            );
        }
        assert_eq!(cases.len(), 3);
        // `Absent` 的理由必须说清后果：只说「按钮点不动」用户看不懂。
        let reason = start_blocked_reason(HostConfigPresence::Absent).unwrap();
        assert!(reason.contains("host.toml"), "{reason}");
        assert!(reason.contains("失去客户端身份"), "{reason}");
        assert!(reason.contains("无法接入"), "{reason}");
        // `Unknown` 明确**不**拦（三态里唯一容易写反的一格，单独再钉一次）。
        assert_eq!(start_blocked_reason(HostConfigPresence::Unknown), None);
    }

    /// `Unknown` 的补充说明：只在读不到时出现，且必须点明「这是预期的，不是故障」与读不到
    /// 时的后果（服务会自停、把身份交回来）。`Present` / `Absent` 都不该有补充。
    #[test]
    fn host_config_advisory_only_speaks_when_the_gui_cannot_read_the_dir() {
        assert_eq!(host_config_advisory(HostConfigPresence::Present), None);
        assert_eq!(host_config_advisory(HostConfigPresence::Absent), None);
        let note = host_config_advisory(HostConfigPresence::Unknown).unwrap();
        assert!(note.contains("读不到"), "{note}");
        // 必须点明「预期，不是故障」，否则用户会去查一个并不存在的问题。
        assert!(note.contains("预期"), "{note}");
        // 以及读不到时的后果：服务会自己停、把身份交回来。
        assert!(note.contains("自行停止"), "{note}");
        assert!(note.contains("身份"), "{note}");
    }

    /// 非 Windows 上恒 `Unknown`（那条路径不存在；这一节也不渲染）。
    #[cfg(not(windows))]
    #[test]
    fn host_config_presence_is_unknown_off_windows() {
        assert_eq!(host_config_presence(), HostConfigPresence::Unknown);
    }

    /// 五个状态都有互不相同的标签（撞车会让用户分不清「没装」与「没跑」）。
    #[test]
    fn presence_label_covers_every_state_uniquely() {
        let cases = [
            (Unknown, "未知"),
            (Absent, "未安装"),
            (Stopped, "已停止"),
            (Pending, "启动/停止中"),
            (Running, "运行中"),
        ];
        for (presence, expected) in cases {
            assert_eq!(presence_label(presence), expected, "{presence:?}");
        }
        assert_eq!(cases.len(), 5);
        let mut labels: Vec<&str> = cases.iter().map(|(_, label)| *label).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), cases.len());
    }

    /// 托盘那一行随平台变：Windows 报状态，别处明说不可用。
    #[test]
    fn service_menu_text_reflects_the_platform() {
        if SERVICE_SUPPORTED {
            assert_eq!(service_menu_text(Running), "服务：运行中");
            assert_eq!(service_menu_text(Absent), "服务：未安装");
        } else {
            assert_eq!(service_menu_text(Running), "服务：仅 Windows 可用");
        }
    }

    /// 身份归属的两个说法。
    #[test]
    fn owner_label_names_both_owners() {
        assert!(owner_label(IdentityOwner::Gui).contains("本窗口"));
        assert!(owner_label(IdentityOwner::Service).contains("服务"));
    }

    /// 刚建出来的状态：没有在途操作、没有结果，`poll` 无事可做。
    #[test]
    fn a_fresh_service_ops_is_idle() {
        let mut ops = ServiceOps::default();
        assert!(ops.in_flight().is_none());
        assert!(ops.last_message().is_none());
        assert!(!ops.poll());
    }
}
