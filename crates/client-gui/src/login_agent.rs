//! macOS 登录窗口只读 agent（P1）与 LaunchAgent 安装器。
//!
//! # 为什么是 LaunchAgent，不是 LaunchDaemon
//!
//! `ScreenCaptureKit` 只能在**有 WindowServer 连接的会话**里工作。root LaunchDaemon
//! 在 system/global session，`SCShareableContent` 会超时；登录窗口的 WindowServer
//! 只在 `LoginWindow` 会话里可达。Apple DTS 的正规做法是
//! `/Library/LaunchAgents` + `LimitLoadToSessionType = ["LoginWindow"]`，该会话里的
//! agent 以 **root** 运行（见根目录 `MACOS-UNATTENDED.md` 与其中的来源）。
//!
//! # 能力边界
//!
//! - **只读**：引擎的 `RemoteManager::set_read_only(true)` 在 `RemoteSessionIncoming`
//!   阶段拒绝「完全控制 / 仅文件传输」，只接受「仅观看」。平台层再装一个空注入器
//!   （见 `remote::login_window_handles`）作为纵深防御。
//! - **无剪贴板/文件/音频**：平台手柄不提供剪贴板，`share_dir` 为 `None`，
//!   `audio_allowed = false`。
//! - **登录窗口注入不可行**：这不是本模块没实现，而是 macOS 没有公开/可靠的注入路径
//!   （`CGEventPost` 在 loginwindow 下被 WindowServer 丢弃）。因此不提供「控制」模式。
//!
//! # TCC 与 entitlement
//!
//! 采集需要「屏幕录制」授权。登录窗口下系统可能弹出授权提示，但无人值守时没人能点；
//! 长期无人值守需要 Apple 审批的 `com.apple.developer.persistent-content-capture`
//! （「远程桌面」权限）。安装器会把**当前 `.app` 整体**复制到 root-only 目录，保留
//! bundle id 与签名，因此用户在普通会话里授予的权限更可能沿用。
//!
//! # 安装布局
//!
//! ```text
//! /Library/PrivilegedHelperTools/RustTunnelLoginAgent.app   （或单文件 helper）
//! /Library/Application Support/rust-tunnel/host.toml        （0600 root:wheel）
//! /Library/LaunchAgents/com.rust-tunnel.loginagent.plist    （0644 root:wheel）
//! /Library/Logs/rust-tunnel-loginagent.log
//! ```

use std::path::PathBuf;

/// 登录窗口 agent 的运行参数。
#[derive(Debug, Clone)]
pub struct LoginAgentOptions {
    /// `host.toml` 路径。
    pub config_path: PathBuf,
}

impl Default for LoginAgentOptions {
    fn default() -> Self {
        Self {
            config_path: default_config_path(),
        }
    }
}

/// 登录窗口 agent 的 host 配置路径（root-only 目录）。
#[must_use]
pub fn default_config_path() -> PathBuf {
    PathBuf::from("/Library/Application Support/rust-tunnel/host.toml")
}

/// 运行登录窗口只读 agent（由 LaunchAgent 以 `--login-agent` 拉起）。
///
/// # Errors
/// 非 macOS、配置缺失/非法、运行时创建失败时返回错误。
#[cfg(target_os = "macos")]
pub fn run(opts: &LoginAgentOptions) -> anyhow::Result<()> {
    imp::run(opts)
}

/// 运行登录窗口只读 agent（非 macOS 恒失败）。
///
/// # Errors
/// 恒返回「仅 macOS 支持」。
#[cfg(not(target_os = "macos"))]
pub fn run(opts: &LoginAgentOptions) -> anyhow::Result<()> {
    anyhow::bail!(
        "登录窗口 agent 仅 macOS 支持（配置：{}）",
        opts.config_path.display()
    )
}

/// 安装 LaunchAgent（需要 root）。
///
/// # Errors
/// 非 macOS、非 root、配置缺失/非法、文件复制或权限加固失败时返回错误。
#[cfg(target_os = "macos")]
pub fn install(source_config: Option<&std::path::Path>) -> anyhow::Result<()> {
    imp::install(source_config)
}

/// 安装 LaunchAgent（非 macOS 恒失败）。
///
/// # Errors
/// 恒返回「仅 macOS 支持」。
#[cfg(not(target_os = "macos"))]
pub fn install(_source_config: Option<&std::path::Path>) -> anyhow::Result<()> {
    anyhow::bail!("登录窗口 agent 仅 macOS 支持")
}

/// 卸载 LaunchAgent（需要 root）。
///
/// # Errors
/// 非 macOS、非 root、文件删除失败时返回错误。
#[cfg(target_os = "macos")]
pub fn uninstall() -> anyhow::Result<()> {
    imp::uninstall()
}

/// 卸载 LaunchAgent（非 macOS 恒失败）。
///
/// # Errors
/// 恒返回「仅 macOS 支持」。
#[cfg(not(target_os = "macos"))]
pub fn uninstall() -> anyhow::Result<()> {
    anyhow::bail!("登录窗口 agent 仅 macOS 支持")
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{default_config_path, LoginAgentOptions};
    use anyhow::{bail, Context};
    use std::ffi::CString;
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tracing::{info, warn};

    const LABEL: &str = "com.rust-tunnel.loginagent";
    const HELPER_DIR: &str = "/Library/PrivilegedHelperTools";
    const HELPER_BUNDLE_NAME: &str = "RustTunnelLoginAgent.app";
    const HELPER_BIN_NAME: &str = "rust-tunnel-login-agent";
    const CONFIG_DIR: &str = "/Library/Application Support/rust-tunnel";
    const PLIST_PATH: &str = "/Library/LaunchAgents/com.rust-tunnel.loginagent.plist";
    const LOG_PATH: &str = "/Library/Logs/rust-tunnel-loginagent.log";

    /// 启动登录窗口 agent（阻塞到控制通道不可恢复 / 进程被 launchd 终止）。
    pub(super) fn run(opts: &LoginAgentOptions) -> anyhow::Result<()> {
        let settings = crate::instance::load_host_config(&opts.config_path).with_context(|| {
            format!(
                "登录窗口 agent 找不到/读不懂 host.toml：{}。\n\
                 请先用 `--install-login-agent --config <host.toml>` 安装，或把配置放到 {}。",
                opts.config_path.display(),
                default_config_path().display()
            )
        })?;
        // 日志只初始化一次；launchd 的 StandardOut/Error 都指向 LOG_PATH。
        rust_tunnel_common::init_logging_with_level(&settings.config.log);

        if unsafe { libc::geteuid() } != 0 {
            warn!("登录窗口 agent 未以 root 运行；屏幕录制授权很可能不适用");
        }
        warn_macos_version();

        // 登录窗口没有可点的授权框；先探测，缺失时尝试触发一次（可能无人可见）。
        let permissions = crate::remote::permissions();
        if !permissions.screen_capture {
            warn!(
                "缺少「屏幕录制」授权：登录窗口画面会采集失败。\
                 请在已登录会话里授予 RustTunnel 屏幕录制权限；\
                 长期无人值守还需要 persistent-content-capture entitlement。"
            );
            let granted = crate::remote::request_screen_capture();
            warn!(granted, "已尝试触发屏幕录制授权请求");
        }

        let mut config = settings.config.clone();
        // 登录窗口 agent 只承载远程控制：不加入 mesh、不跑 agent 执行器、
        // 不接受文件（share_dir=None 已禁止，read_only 再在协议层拒绝）。
        config.enable_agent = false;
        config.mesh = None;
        config.mesh_name = None;
        config.mesh_services.clear();

        let (manager, _release) = crate::build_remote_manager(
            &config,
            crate::ManagerOptions {
                enabled: true,
                // 没有 UI 可点；只读策略保证只会接受「仅观看」。
                approval_policy: rust_tunnel_remote::ApprovalPolicy::Auto,
                auto_lock_on_disconnect: false,
                native_resolution: settings.native_resolution,
                prefer_h265: settings.prefer_h265,
                prefer_444: settings.prefer_444,
                punch_migration: true,
                platform_mode: crate::remote::PlatformMode::LoginWindow,
                share_dir: None,
                download_dir: std::env::temp_dir(),
                audio_enabled: false,
                audio_allowed: false,
                audio_volume: 0,
            },
        );
        // 协议层只读：完全控制/文件传输在 `RemoteSessionIncoming` 阶段就被拒绝。
        manager.set_read_only(true);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("创建登录窗口 agent 的 Tokio 运行时失败")?;
        info!(
            server = %config.server,
            name = config.name.as_deref().unwrap_or(""),
            "登录窗口只读 agent 已启动（仅接受「仅观看」）"
        );
        // launchd 在会话结束时以 SIGTERM 终止本进程；默认处理即可。
        runtime.block_on(crate::run_client_loop(
            config, None, None, None, manager, None, None,
        ));
        warn!("登录窗口 agent 的控制循环已退出");
        Ok(())
    }

    /// 安装 LaunchAgent：复制 helper、落盘 host.toml、写 plist 并加固属主。
    pub(super) fn install(source_config: Option<&Path>) -> anyhow::Result<()> {
        require_root("--install-login-agent")?;
        let source = source_config.map_or_else(default_config_path, Path::to_path_buf);
        let source = source
            .canonicalize()
            .with_context(|| format!("host.toml 不存在：{}", source.display()))?;
        // 先按真实解析逻辑校验一遍，避免把坏配置装成 root-only 后才发现。
        let _settings = crate::instance::load_host_config(&source)
            .with_context(|| format!("host.toml 非法：{}", source.display()))?;
        warn_if_world_writable(&source)?;

        let exe = std::env::current_exe().context("取当前可执行文件路径失败")?;
        let destination_config = PathBuf::from(CONFIG_DIR).join("host.toml");
        let (helper_exe, helper_desc) = install_helper(&exe)?;

        fs::create_dir_all(CONFIG_DIR)
            .with_context(|| format!("创建配置目录失败：{CONFIG_DIR}"))?;
        harden_tree(Path::new(CONFIG_DIR))?;
        if source == destination_config {
            set_mode_and_owner(&destination_config, 0o600)?;
        } else {
            copy_file_atomic(&source, &destination_config, 0o600)?;
        }

        let plist = render_plist(&helper_exe, &destination_config);
        fs::write(PLIST_PATH, plist)
            .with_context(|| format!("写 LaunchAgent 失败：{PLIST_PATH}"))?;
        set_mode_and_owner(Path::new(PLIST_PATH), 0o644)?;
        // 清掉可能残留的旧作业（忽略未加载）。
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &format!("system/{LABEL}")])
            .status();

        println!(
            "已安装登录窗口只读 agent。\n\
             \x20 LaunchAgent: {PLIST_PATH}\n\
             \x20 helper:      {helper_desc}\n\
             \x20 host config: {}\n\
             \n\
             生效时机：**登录窗口会话**（注销 / 重启后）。当前已登录会话不会启动它。\n\
             控制器只能发起「仅观看」；完全控制与文件传输会被拒绝。\n\
             日志：sudo tail -f {LOG_PATH}\n\
             卸载：sudo <本程序> --uninstall-login-agent\n\
             \n\
             前提：macOS 14.4+、已授予「屏幕录制」；长期无人值守还需\n\
             com.apple.developer.persistent-content-capture entitlement。",
            destination_config.display()
        );
        Ok(())
    }

    /// 卸载 LaunchAgent、helper、host.toml 与 plist（保留日志）。
    pub(super) fn uninstall() -> anyhow::Result<()> {
        require_root("--uninstall-login-agent")?;
        let uid = invoking_uid();
        for domain in ["system", "loginwindow"] {
            let _ = Command::new("/bin/launchctl")
                .args(["bootout", &format!("{domain}/{LABEL}")])
                .status();
        }
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &format!("gui/{uid}/{LABEL}")])
            .status();

        remove_if_exists(Path::new(PLIST_PATH))?;
        remove_if_exists(&PathBuf::from(HELPER_DIR).join(HELPER_BUNDLE_NAME))?;
        remove_if_exists(&PathBuf::from(HELPER_DIR).join(HELPER_BIN_NAME))?;
        remove_if_exists(&PathBuf::from(CONFIG_DIR).join("host.toml"))?;
        // 只删空目录，别误删用户放进去的其它文件。
        let _ = fs::remove_dir(CONFIG_DIR);
        println!(
            "已卸载登录窗口只读 agent。\n\
             \x20 已删除：{PLIST_PATH}、helper、{CONFIG_DIR}/host.toml\n\
             \x20 日志保留：{LOG_PATH}"
        );
        Ok(())
    }

    fn require_root(flag: &str) -> anyhow::Result<()> {
        // SAFETY: `geteuid` 无参数、无副作用，只读取当前进程的有效 uid。
        if unsafe { libc::geteuid() } == 0 {
            return Ok(());
        }
        bail!("登录窗口 agent 需要 root：请用 `sudo <本程序> {flag} ...` 运行")
    }

    fn invoking_uid() -> u32 {
        std::env::var("SUDO_UID")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| unsafe { libc::getuid() })
    }

    /// 把当前程序（优先整个 `.app`）复制到 root-only 的 helper 目录。
    fn install_helper(exe: &Path) -> anyhow::Result<(PathBuf, String)> {
        fs::create_dir_all(HELPER_DIR).with_context(|| format!("创建 {HELPER_DIR} 失败"))?;
        let bundle = enclosing_app(exe);
        if let Some(app) = bundle {
            let destination = PathBuf::from(HELPER_DIR).join(HELPER_BUNDLE_NAME);
            // 从已安装的 helper 自身重装：源与目标同一个目录，删掉再复制会把正在运行的
            // 源也删掉。原地加固即可。
            if app == destination {
                let relative = exe
                    .strip_prefix(&app)
                    .context("当前可执行文件不在其 .app 内")?;
                let helper_exe = destination.join(relative);
                harden_tree(&destination)?;
                return Ok((helper_exe, format!("{}（原地加固）", destination.display())));
            }
            remove_if_exists(&destination)?;
            copy_tree(&app, &destination)
                .with_context(|| format!("复制 .app 到 {} 失败", destination.display()))?;
            let relative = exe
                .strip_prefix(&app)
                .context("当前可执行文件不在其 .app 内")?;
            let helper_exe = destination.join(relative);
            if !helper_exe.is_file() {
                bail!("复制后找不到 helper 可执行文件：{}", helper_exe.display());
            }
            harden_tree(&destination)?;
            Ok((
                helper_exe,
                format!(
                    "{}（完整 .app，保留 bundle id/签名）",
                    destination.display()
                ),
            ))
        } else {
            let destination = PathBuf::from(HELPER_DIR).join(HELPER_BIN_NAME);
            // 从 helper 自身重装：复制到自身会失败/截断，原地加固即可。
            if exe == destination {
                set_mode_and_owner(&destination, 0o755)?;
                return Ok((
                    destination.clone(),
                    format!("{}（原地加固）", destination.display()),
                ));
            }
            fs::copy(exe, &destination)
                .with_context(|| format!("复制 helper 到 {} 失败", destination.display()))?;
            set_mode_and_owner(&destination, 0o755)?;
            Ok((destination.clone(), destination.display().to_string()))
        }
    }

    fn enclosing_app(exe: &Path) -> Option<PathBuf> {
        exe.ancestors()
            .find(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
            })
            .map(Path::to_path_buf)
    }

    fn render_plist(helper_exe: &Path, config: &Path) -> String {
        let program = xml_escape(&helper_exe.to_string_lossy());
        let config = xml_escape(&config.to_string_lossy());
        let log = xml_escape(LOG_PATH);
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
    <string>--login-agent</string>
    <string>--config</string>
    <string>{config}</string>
  </array>
  <key>LimitLoadToSessionType</key><array><string>LoginWindow</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
        )
    }

    fn xml_escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    fn warn_macos_version() {
        let output = Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output();
        let Ok(output) = output else {
            return;
        };
        let version = String::from_utf8_lossy(&output.stdout);
        let mut parts = version.trim().split('.');
        let major: u32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        if (major, minor) < (14, 4) {
            warn!(
                version = %version.trim(),
                "ScreenCaptureKit 在登录窗口下需要 macOS 14.4+；当前版本可能采集失败"
            );
        }
    }

    fn warn_if_world_writable(path: &Path) -> anyhow::Result<()> {
        let mode = fs::metadata(path)
            .with_context(|| format!("读取 {} 元数据失败", path.display()))?
            .permissions()
            .mode();
        if mode & 0o002 != 0 {
            bail!(
                "host.toml 对所有人可写（mode {:o}）：它含客户端 token，拒绝安装。\
                 请先 `chmod 600`。",
                mode & 0o777
            );
        }
        Ok(())
    }

    fn copy_file_atomic(source: &Path, destination: &Path, mode: u32) -> anyhow::Result<()> {
        let bytes = fs::read(source).with_context(|| format!("读取 {} 失败", source.display()))?;
        let temporary = destination.with_extension("toml.new");
        fs::write(&temporary, bytes)
            .with_context(|| format!("写临时配置 {} 失败", temporary.display()))?;
        set_mode_and_owner(&temporary, mode)?;
        fs::rename(&temporary, destination)
            .with_context(|| format!("替换 {} 失败", destination.display()))?;
        Ok(())
    }

    fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(source)?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(source)?;
            std::os::unix::fs::symlink(target, destination)?;
            return Ok(());
        }
        if metadata.is_dir() {
            fs::create_dir_all(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
            }
            Ok(())
        } else {
            fs::copy(source, destination)?;
            Ok(())
        }
    }

    /// 递归把 helper 置为 root:wheel，目录 0755、可执行文件 0755、普通文件 0644。
    fn harden_tree(path: &Path) -> anyhow::Result<()> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("读取 {} 元数据失败", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        chown_root(path)?;
        if metadata.is_dir() {
            set_mode(path, 0o755)?;
            for entry in
                fs::read_dir(path).with_context(|| format!("读取目录 {} 失败", path.display()))?
            {
                harden_tree(&entry?.path())?;
            }
        } else {
            let executable = metadata.permissions().mode() & 0o111 != 0;
            set_mode(path, if executable { 0o755 } else { 0o644 })?;
        }
        Ok(())
    }

    fn set_mode_and_owner(path: &Path, mode: u32) -> anyhow::Result<()> {
        set_mode(path, mode)?;
        chown_root(path)?;
        Ok(())
    }

    fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("设置 {} 权限失败", path.display()))
    }

    fn chown_root(path: &Path) -> anyhow::Result<()> {
        let raw = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("路径含 NUL：{}", path.display()))?;
        // SAFETY: `raw` 是有效的 NUL 结尾路径；uid/gid 0 是 root:wheel。
        if unsafe { libc::chown(raw.as_ptr(), 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("chown root:wheel {} 失败", path.display()));
        }
        Ok(())
    }

    fn remove_if_exists(path: &Path) -> anyhow::Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    fs::remove_dir_all(path)
                        .with_context(|| format!("删除目录 {} 失败", path.display()))?;
                } else {
                    fs::remove_file(path)
                        .with_context(|| format!("删除 {} 失败", path.display()))?;
                }
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("检查 {} 失败", path.display())),
        }
    }
}
