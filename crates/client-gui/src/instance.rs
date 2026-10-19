//! `--instance`：SYSTEM 服务拉进控制台会话的**无头实例**。
//!
//! # 它是什么
//!
//! `rust-tunnel-client-service.exe`（服务侧）用 `CreateProcessAsUserW(lpDesktop =
//! "winsta0\\default")` 把它拉进**控制台会话**、以 SYSTEM 完整性运行。它承载客户端身份
//! （注册到服务端、跑远程控制引擎），但**不起 eframe / 托盘 / 任何窗口**，采集与注入改用
//! `win-platform` 并跟随输入桌面（见 [`crate::remote::win_desktop`]）。
//!
//! 预期的命令行（由服务侧用 `rust_tunnel_win_platform::process::build_command_line` 拼，
//! **不要**手工字符串拼接——它实现了 `CommandLineToArgvW` 的引号规则）：
//!
//! ```text
//! rust-tunnel-client-gui.exe --instance \
//!     --config "%ProgramData%\rust-tunnel\host.toml" \
//!     --share  "<控制端可读写的共享目录>" \
//!     --download "<控制端下载落盘目录>"
//! ```
//!
//! # 为什么所有路径都要**由服务侧显式传入**
//!
//! 实例拿的是 winlogon 令牌，也就是**以 SYSTEM 身份**跑。于是任何「按当前用户 profile 解析」
//! 的路径都会落到 `C:\Windows\System32\config\systemprofile\...`，而不是用户的 profile：
//! 实例既读不到用户的配置/偏好，`remote::share_dir()` 指向的共享目录用户也**看不见**，
//! keyring 兜底读的是 SYSTEM 的凭据库、会**静默失败**。
//!
//! 阶段 0 已经撞上过这个问题的形状并用了对的做法（父进程先算好子实例的报告路径、用命令行
//! 传下去，见 `crates/win-platform/src/probe.rs`）。本模块照同一条路子：**默认值明确落到
//! `%ProgramData%`**（一个与 profile 无关的常量路径），或由命令行/环境变量覆盖，绝不猜 profile。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rust_tunnel_client::ClientConfig;
use rust_tunnel_client::config::ClientCli;
use rust_tunnel_remote::{ApprovalPolicy, RemoteManager, APPROVAL_TIMEOUT};

use crate::remote::{PlatformMode, SessionRelease};
use crate::{build_remote_manager, run_client_loop, LoopExit, ManagerOptions};

/// 实例的路径参数（全部可由命令行 / 环境变量覆盖；默认值**明确落到 `%ProgramData%`**）。
#[derive(Debug, Clone)]
pub struct InstanceOptions {
    /// `host.toml` 的路径（含服务端地址 + 客户端 token + 审批策略）。
    pub config_path: PathBuf,
    /// 被控端对外共享的目录（控制端可读写它）。
    pub share_dir: PathBuf,
    /// 控制端下载文件的落盘目录。
    pub download_dir: PathBuf,
}

impl InstanceOptions {
    /// 由命令行给出的可选路径解析出最终选项（`None` → 环境变量 → 明确默认值）。
    ///
    /// 环境变量：`RUST_TUNNEL_HOST_CONFIG` / `RUST_TUNNEL_SHARE_DIR` /
    /// `RUST_TUNNEL_DOWNLOAD_DIR`（便于真机上手工试）。
    #[must_use]
    pub fn resolve(
        config_path: Option<PathBuf>,
        share_dir: Option<PathBuf>,
        download_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            config_path: config_path
                .or_else(|| env_path("RUST_TUNNEL_HOST_CONFIG"))
                .unwrap_or_else(default_host_config_path),
            share_dir: share_dir
                .or_else(|| env_path("RUST_TUNNEL_SHARE_DIR"))
                .unwrap_or_else(default_share_dir),
            download_dir: download_dir
                .or_else(|| env_path("RUST_TUNNEL_DOWNLOAD_DIR"))
                .unwrap_or_else(default_download_dir),
        }
    }
}

/// 读一个路径型环境变量（空串视同未设置）。
fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// 实例数据根目录：**`%ProgramData%\rust-tunnel`**（与用户 profile 无关的常量路径）。
fn program_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(pd) = std::env::var_os("ProgramData") {
            return PathBuf::from(pd).join("rust-tunnel");
        }
    }
    // 非 Windows（`--instance` 本就不可用）或 `%ProgramData%` 缺失：退回一个明确的相对路径，
    // **绝不**退回 SYSTEM/用户 profile（那正是要避免的）。
    PathBuf::from("rust-tunnel")
}

/// 默认的 `host.toml` 路径（`%ProgramData%\rust-tunnel\host.toml`）。
#[must_use]
pub fn default_host_config_path() -> PathBuf {
    program_data_dir().join("host.toml")
}

/// 默认的共享目录（`%ProgramData%\rust-tunnel\share`）。
#[must_use]
pub fn default_share_dir() -> PathBuf {
    program_data_dir().join("share")
}

/// 默认的下载目录（`%ProgramData%\rust-tunnel\download`）。
#[must_use]
pub fn default_download_dir() -> PathBuf {
    program_data_dir().join("download")
}

/// 读 `host.toml`：复用 `client` 的 [`ClientConfig`]（同一套 `server`/`password`/`name`/
/// `tls*` 字段），审批策略取同文件里的 `approval` 键（[`ClientConfigFile`] 不认识它，
/// 故单独解析一次）。
///
/// [`ClientConfigFile`]: rust_tunnel_client::config::ClientConfigFile
///
/// # Errors
///
/// 文件不存在（给出期望路径与「应由服务安装时创建」的说明）或字段不合法时返回 `Err`。
pub fn load_host_config(path: &Path) -> anyhow::Result<(ClientConfig, ApprovalPolicy)> {
    if !path.exists() {
        anyhow::bail!(
            "找不到实例配置 {}。\n\
             它应当由服务安装时创建：`%ProgramData%\\rust-tunnel\\host.toml`，内含服务端地址、\
             客户端 token 与审批策略。**注意** `%ProgramData%` 本身对 `BUILTIN\\Users` 可读，\
             直接放在它下面的文件是全局可读的——该文件含服务端全局 token，必须放进子目录并显式\
             设 DACL（去掉继承，只留 SYSTEM + Administrators）。\n\
             也可用 `--config <路径>` 或环境变量 `RUST_TUNNEL_HOST_CONFIG` 覆盖。",
            path.display()
        );
    }
    // 复用 `ClientConfig` 的合并逻辑：构造一个只带 `config_file` 的 `ClientCli`
    // （**不能**用 `ClientConfig::load()`——那会拿 clap 去解析 `--instance` 这些参数）。
    let cli = ClientCli {
        config_file: Some(path.to_string_lossy().into_owned()),
        server: None,
        name: None,
        password: None,
        tls: None,
        tls_server_name: None,
        tls_insecure: None,
        mesh: None,
        mesh_name: None,
        mesh_services: Vec::new(),
        enable_agent: false,
        agent_pty_port: None,
        log: None,
    };
    let config = ClientConfig::from_cli(cli)
        .map_err(|err| anyhow::anyhow!("实例配置无效（{}）：{err}", path.display()))?;
    let approval = read_approval(path).unwrap_or_default();
    Ok((config, approval))
}

/// `host.toml` 里与实例有关、但 [`ClientConfig`] 不认识的字段。
#[derive(Debug, serde::Deserialize)]
struct ApprovalFile {
    /// 接入确认策略；缺省 [`ApprovalPolicy::Confirm`]（安全侧默认）。
    #[serde(default)]
    approval: Option<ApprovalPolicy>,
}

/// 从 `host.toml` 里单独解析 `approval`（解析失败按缺省处理并告警）。
fn read_approval(path: &Path) -> Option<ApprovalPolicy> {
    let content = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<ApprovalFile>(&content) {
        Ok(parsed) => parsed.approval,
        Err(err) => {
            tracing::warn!(
                "解析 {} 的 approval 字段失败（回落到 Confirm）：{err}",
                path.display()
            );
            None
        }
    }
}

/// 无人值守下的审批策略告警。
///
/// **`Confirm` 在无人值守下等于「拒掉所有接入」**：没有人能点确认弹窗，请求会在
/// [`APPROVAL_TIMEOUT`] 秒后超时被拒。这不是 bug，是安全默认——必须让它在日志里显眼。
fn warn_unattended(policy: ApprovalPolicy, config_path: &Path) {
    match policy {
        ApprovalPolicy::Auto => tracing::warn!(
            "接入确认策略 = Auto：**任何**持有服务端 token 的对端都能不经确认直接接入本机。\
             这是无人值守下唯一可用的策略，也是显式的风险选择。"
        ),
        ApprovalPolicy::Confirm => tracing::warn!(
            "⚠ 接入确认策略 = Confirm（每次询问），但**无人值守下没有人能点确认弹窗**：\
             接入请求会在 {} 秒后超时被拒。要无人值守可用，请在 {} 里写 `approval = \"Auto\"`。",
            APPROVAL_TIMEOUT.as_secs(),
            config_path.display()
        ),
    }
}

/// 实例日志文件的路径（`host.toml` 同目录下的 `instance.log`）。
///
/// 由 [`init_logging`] 与配置加载失败时的兜底写入（[`log_config_failure`]）**共用**：两处必须
/// 指向**同一个**文件，否则「起不来」的线索会落到另一个没人看的地方。`host.toml` 没有父目录
/// 时（相对文件名）退回当前目录，与 [`init_logging`] 原本的行为一致。
fn instance_log_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        .join("instance.log")
}

/// 初始化实例日志：**落文件**。
///
/// 实例由服务拉起（`CreateProcessAsUserW`），**不继承控制台**，stdout 直接丢弃——
/// 日志必须落文件，否则「起不来」时没有任何线索。落在 `host.toml` 同目录。
fn init_logging(level: &str, config_path: &Path) {
    use tracing_subscriber::EnvFilter;

    let log_path = instance_log_path(config_path);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let builder = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(true)
        .with_env_filter(filter);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            let _ = builder
                .with_writer(std::sync::Mutex::new(file))
                .try_init();
        }
        Err(err) => {
            eprintln!("实例日志文件打不开（{}）：{err}", log_path.display());
            let _ = builder.try_init();
        }
    }
    tracing::info!("实例日志文件：{}", log_path.display());
}

/// 把配置加载失败**兜底**写进 `instance.log`（best-effort）。
///
/// # 为什么要这一步（否则它看起来像多余的日志）
///
/// 实例由服务用 `CreateProcessAsUserW` 拉起、**不继承控制台**，stdout/stderr 直接丢弃：从
/// [`run`] 冒出来的 `anyhow` 错误**没有任何人看得到**。而 [`load_host_config`] 跑在
/// [`init_logging`] **之前**——日志级别来自配置，配置没读到就没有可初始化的级别——所以这条
/// 路径上 `instance.log` **根本还没被创建**。两者合起来：一个缺失或解析不了的 `host.toml`
/// 完全不可见，唯一残留的线索是服务侧那句「实例退出，退避后重启」，没有原因。故这里在正常
/// 日志还没起来时，**直接**把错误追加到日志文件（与 [`init_logging`] 用的是同一个路径）。
///
/// **best-effort**：写日志自己失败也**不影响**返回值——调用方仍拿到原始的配置错误，不会因为
/// 写日志失败而被掩盖，也不会多出一个「写日志失败」的次生错误。
fn log_config_failure(config_path: &Path, err: &anyhow::Error) {
    use std::io::Write;

    let log_path = instance_log_path(config_path);
    // 打不开就直接放弃：这是兜底写入，失败不该制造第二个错误（见函数文档）。
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    else {
        return;
    };
    // 此刻 tracing 还没初始化（级别来自配置），故这行没有 tracing 的时间戳——写清前缀让它在
    // 正常日志行之间仍可辨认。
    let _ = writeln!(
        file,
        "[instance] 配置加载失败（{}）：{err:#}",
        config_path.display()
    );
}

/// 运行无头实例（由 `main()` 的 argv 分派调用）。
///
/// # Errors
///
/// 非 Windows 平台、配置读不到、或 tokio 运行时建不起来时返回 `Err`。
pub fn run(opts: &InstanceOptions) -> anyhow::Result<()> {
    if !cfg!(target_os = "windows") {
        anyhow::bail!(
            "「--instance」是 Windows 专用模式：它由 SYSTEM 服务用 `CreateProcessAsUserW` \
             拉进控制台会话、以 SYSTEM 完整性运行，采集与注入改用 win-platform 并跟随输入桌面。"
        );
    }

    // 配置读不出来时，日志还没起来（级别来自配置），而服务把本进程的 stderr 丢弃了——
    // 直接落文件，否则这条失败对任何人都不可见（见 `log_config_failure` 的文档）。
    let (config, approval) = match load_host_config(&opts.config_path) {
        Ok(loaded) => loaded,
        Err(err) => {
            log_config_failure(&opts.config_path, &err);
            return Err(err);
        }
    };
    init_logging(&config.log, &opts.config_path);
    warn_unattended(approval, &opts.config_path);
    tracing::info!(
        server = %config.server,
        name = ?config.name,
        share = %opts.share_dir.display(),
        download = %opts.download_dir.display(),
        "无头实例启动"
    );

    let exit = Arc::new(AtomicBool::new(false));
    spawn_instance_runtime(config, approval, opts, Arc::clone(&exit));
    // 主线程 park：等运行时线程给出退出信号（注册被拒 / Ctrl+C）。
    wait_for_exit(&exit);
    tracing::info!("无头实例退出");
    Ok(())
}

/// 起后台运行时线程：控制通道 + 远程引擎（无头，不注入 `AppState` / `status_tx`）。
pub(crate) fn spawn_instance_runtime(
    config: ClientConfig,
    approval: ApprovalPolicy,
    opts: &InstanceOptions,
    exit: Arc<AtomicBool>,
) {
    // 目录在进线程之前克隆好（避免把 `&opts` 借进 `'static` 线程）。
    let share_dir = opts.share_dir.clone();
    let download_dir = opts.download_dir.clone();
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        else {
            tracing::error!("无法创建 tokio 运行时，实例退出");
            exit.store(true, Ordering::SeqCst);
            return;
        };
        rt.block_on(async move {
            let (manager, release) = build_remote_manager(
                &config,
                ManagerOptions {
                    // 无人值守实例**默认允许被远程控制**（开关本就是这个进程存在的理由）。
                    enabled: true,
                    approval_policy: approval,
                    platform_mode: PlatformMode::Instance,
                    share_dir: Some(share_dir),
                    download_dir,
                },
            );
            spawn_session_watcher(Arc::clone(&manager), release);
            // 控制通道循环与 Ctrl+C 竞速：任一结束即收尾退出。
            //
            // `service_owner` 传 `None`：实例**就是**服务的孩子，它必须始终注册——身份归属
            // 的监督只属于用户桌面的常规 GUI（见 `crate::service_watch`）。
            tokio::select! {
                exit = run_client_loop(config, None, None, None, manager, None) => match exit {
                    LoopExit::Unrecoverable => tracing::warn!(
                        "控制通道循环结束（注册被拒等不可重连的错误），实例退出"
                    ),
                    // 实例永远传 `None`，走不到这条（服务不会把身份让给自己的实例）。
                    LoopExit::YieldedToService => tracing::warn!(
                        "实例被要求让出客户端身份（不应发生），实例退出"
                    ),
                },
                result = tokio::signal::ctrl_c() => match result {
                    Ok(()) => tracing::info!("收到 Ctrl+C，实例退出"),
                    Err(err) => tracing::warn!("监听 Ctrl+C 失败：{err}"),
                },
            }
            exit.store(true, Ordering::SeqCst);
        });
    });
}

/// 监视会话结束，补发一次「全部抬起」。
///
/// 控制器侧的 `InputGate::release_all` 在会话结束时会把抬起事件发过线来，但**链路被硬
/// 切断**（进程被杀、网络断）时那些事件到不了，被控端若正按住某个键/鼠标键就会永久卡住。
/// 这里每秒看一次引擎快照，`active` 由「有」变「无」即触发（幂等：没有按住项时是空操作）。
fn spawn_session_watcher(manager: Arc<RemoteManager>, release: Option<Arc<dyn SessionRelease>>) {
    let Some(release) = release else {
        return;
    };
    tokio::spawn(async move {
        let mut was_active = false;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let active = manager.snapshot().active.is_some();
            if was_active && !active {
                tracing::info!("会话结束，补发一次「全部抬起」");
                release.release_all();
            }
            was_active = active;
        }
    });
}

/// 主线程 park：轮询退出标志（`Ctrl+C` / 进程被杀由默认动作终止，见 `run` 的说明）。
fn wait_for_exit(exit: &AtomicBool) {
    while !exit.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    //! 可移植单测：只测 [`log_config_failure`] / [`instance_log_path`]（纯 `std::fs`，
    //! 与 Win32 无关），故本机（macOS）可跑。

    use super::*;

    /// 造一个进程内唯一的临时目录（沿用 `service_ui` 的做法）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rust-tunnel-instance-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 配置加载失败必须**落进 `instance.log`**：服务拉起的实例没有控制台，这是唯一能被
    /// 人看到这条失败的地方（见 [`log_config_failure`] 的文档）。
    #[test]
    fn config_failure_is_appended_to_the_instance_log() {
        let dir = temp_dir("config-failure");
        let config_path = dir.join("host.toml");
        // 日志路径必须与 `init_logging` 用的是同一个（`host.toml` 同目录）。
        assert_eq!(instance_log_path(&config_path), dir.join("instance.log"));

        let err = anyhow::anyhow!("boom：host.toml 解析不了");
        log_config_failure(&config_path, &err);

        let log = std::fs::read_to_string(dir.join("instance.log")).unwrap();
        assert!(log.contains("配置加载失败"), "{log}");
        assert!(log.contains("boom"), "{log}");
        assert!(log.contains("host.toml"), "{log}");

        // **追加**而非覆盖：再写一次，两行都在。
        log_config_failure(&config_path, &err);
        let log = std::fs::read_to_string(dir.join("instance.log")).unwrap();
        assert_eq!(log.matches("配置加载失败").count(), 2, "{log}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写日志失败（父目录不存在）必须被吞掉：这是兜底写入，不能 panic，也不能把配置错误
    /// 变成「写日志失败」的次生错误（见 [`log_config_failure`] 的 best-effort 说明）。
    #[test]
    fn a_failing_log_write_is_swallowed() {
        let dir = temp_dir("unwritable");
        let missing = dir.join("does-not-exist").join("host.toml");
        // 父目录不存在 → 打不开 → 直接返回，不 panic、不建文件。
        log_config_failure(&missing, &anyhow::anyhow!("x"));
        assert!(!dir.join("does-not-exist").join("instance.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没有父目录（相对文件名）时退回当前目录，与 `init_logging` 原本的行为一致。
    #[test]
    fn a_bare_filename_falls_back_to_the_current_directory() {
        assert_eq!(
            instance_log_path(Path::new("host.toml")),
            PathBuf::from(".").join("instance.log")
        );
    }
}
