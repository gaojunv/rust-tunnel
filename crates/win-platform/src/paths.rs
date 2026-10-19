//! 无人值守被控端的**规范落点**（可移植：只读环境变量，不调任何 Win32）。
//!
//! # 为什么要有这一层
//!
//! 服务与实例是**两个进程、两个身份**：服务以 LocalSystem 跑在会话 0，实例由服务用
//! winlogon 令牌拉进控制台会话。任何「按当前用户 profile 解析」的路径在实例里都会落到
//! `C:\Windows\System32\config\systemprofile\...`（SYSTEM 的 profile），而不是用户的
//! profile——实例既读不到用户的配置，用户也看不见实例写的文件。所以两个进程必须对
//! 「东西放在哪」有**同一份、与 profile 无关**的答案，这一层就是那份答案。
//!
//! # 谁负责创建这些目录
//!
//! - `%ProgramData%\rust-tunnel\`（日志、`host.toml`、共享/下载目录、探针报告）与
//!   `%ProgramFiles%\rust-tunnel\`（服务与实例 exe 的副本）都由 `--install`（提权）
//!   创建并加固 DACL；服务启动时再幂等地确保一次（见 `security::harden_directory`）。
//! - **`host.toml` 由运维/安装器写入，不由本程序生成**：它含服务端**全局 token**，
//!   而那个值只有服务端 Web 界面能给出（见 `WINDOWS-SECURE-DESKTOP.md` 的安全模型）。
//!
//! # 探针报告目录为什么也在这里
//!
//! 阶段 0 把它放在 `C:\Users\Public\rust-tunnel-probe\`——那是个**全局可写**目录，
//! 本机任何用户都能预建一条 **junction**（junction 不需要 `SeCreateSymbolicLinkPrivilege`，
//! 只要对父目录有写权限就能用 `FSCTL_SET_REPARSE_POINT` 建），于是管理员之后跑
//! `--probe-in-service` 时，SYSTEM 实例的写入会顺着 junction 落到攻击者指定的位置
//! （以 SYSTEM 身份任意写文件）；而报告里含**锁屏/登录界面的截图**，本就不该全局可读。
//! 故与日志、`host.toml` 一并收敛到产品目录下（见 [`probe_dir`]）。
//!
//! # 与 `crates/client-gui/src/instance.rs` 的重复
//!
//! 那边也有一份 `%ProgramData%\rust-tunnel` 的默认值（`--instance` 单独手工跑时用）。
//! 本 crate **不能**依赖 `client-gui`（那是 GUI crate，会把 libvpx 拖进来），故两处
//! 各写一份。服务总是把 `--config/--share/--download` **显式**传给实例，所以两份默认值
//! 即使漂移也不会静默走错——但改路径时两处都要改。

use std::path::{Path, PathBuf};

/// 产品目录名（`%ProgramData%\<它>` 与 `%ProgramFiles%\<它>` 的最后一段）。
pub const PRODUCT_DIR: &str = "rust-tunnel";

/// 服务 exe **安装后**的文件名（`%ProgramFiles%\rust-tunnel\` 下）。
///
/// 固定成这个名字（而不是保留下载时的 `rust-tunnel-client-service-windows-x86_64.exe`）
/// 是为了让 `ImagePath`、升级路径、文档里的命令三者都不必随归档名变化。
pub const SERVICE_EXE_NAME: &str = "rust-tunnel-client-service.exe";

/// 实例（GUI）exe **安装后**的文件名（`%ProgramFiles%\rust-tunnel\` 下）。
///
/// 与 [`SERVICE_EXE_NAME`] 同理：服务要拉起的那个 exe 在安装时被重命名到这个固定名字，
/// 于是服务侧不必去猜归档名（CI 是 `rust-tunnel-client-gui-windows-x86_64.exe`、
/// 本地构建是 `rust-tunnel-client-gui.exe`）。
pub const INSTANCE_EXE_NAME: &str = "rust-tunnel-client-gui.exe";

/// `%ProgramData%\rust-tunnel`：日志、`host.toml`、共享与下载目录的父目录。
///
/// **不是** `%ProgramData%` 本身：那个目录对 `BUILTIN\Users` 给了 read/create，
/// 直接放它下面等于全局可读——而 `host.toml` 含服务端全局 token。
#[must_use]
pub fn program_data_root() -> PathBuf {
    env_dir("ProgramData").map_or_else(|| PathBuf::from(PRODUCT_DIR), |base| base.join(PRODUCT_DIR))
}

/// `%ProgramData%\rust-tunnel\host.toml`（实例的配置文件）。
#[must_use]
pub fn host_config_path() -> PathBuf {
    program_data_root().join("host.toml")
}

/// `%ProgramData%\rust-tunnel\share`（被控端对外共享的目录）。
#[must_use]
pub fn share_dir() -> PathBuf {
    program_data_root().join("share")
}

/// `%ProgramData%\rust-tunnel\download`（控制端下载文件的落盘目录）。
#[must_use]
pub fn download_dir() -> PathBuf {
    program_data_root().join("download")
}

/// 服务日志文件（`%ProgramData%\rust-tunnel\service.log`）。
///
/// 阶段 0 曾把它写在 `C:\Users\Public\rust-tunnel-probe\`——那是个**全局可写**目录，
/// 任何本机用户都能预建 junction 把 SYSTEM 的写入重定向走，或者直接读走日志。
#[must_use]
pub fn service_log_path() -> PathBuf {
    program_data_root().join("service.log")
}

/// 探针报告目录（`%ProgramData%\rust-tunnel\probe`）。
///
/// 阶段 0 曾把报告与截图写在 `C:\Users\Public\rust-tunnel-probe\`——那是全局可写目录，
/// 本机任何用户都能预建 junction 把 SYSTEM 的写入重定向走（见模块文档），而报告里含
/// 锁屏/登录界面的截图。挪到这里后**读报告要管理员**：探针本身就需要管理员
/// （见 `win-service` 的 USAGE），故这个代价可以接受。
#[must_use]
pub fn probe_dir() -> PathBuf {
    program_data_root().join("probe")
}

/// `%ProgramFiles%\rust-tunnel`：服务与实例 exe 的**副本**落点。
///
/// `%ProgramFiles%` 默认不授予非管理员写权限，所以放在这里的 exe 不会被普通用户替换
/// ——服务以 SYSTEM 运行，若能替换它启动的 exe 就等于拿到了 SYSTEM（见
/// `security::harden_directory` 的威胁模型）。
#[must_use]
pub fn program_files_root() -> PathBuf {
    // `ProgramW6432` 恒指 64 位 Program Files（即便从 32 位进程读也是它），
    // 优先用它以免 32 位上下文把 exe 装到 `Program Files (x86)`。
    env_dir("ProgramW6432")
        .or_else(|| env_dir("ProgramFiles"))
        .map_or_else(|| PathBuf::from(PRODUCT_DIR), |base| base.join(PRODUCT_DIR))
}

/// 服务 exe 安装后的完整路径。
#[must_use]
pub fn installed_service_exe() -> PathBuf {
    program_files_root().join(SERVICE_EXE_NAME)
}

/// 实例 exe 安装后的完整路径（服务要拉起的那个）。
#[must_use]
pub fn installed_instance_exe() -> PathBuf {
    program_files_root().join(INSTANCE_EXE_NAME)
}

/// 读一个目录型环境变量（空串视同未设置）。
fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty() && path.is_absolute())
}

/// 判断一个路径是否落在产品目录里（用于诊断「exe 是不是从用户可写路径跑的」）。
///
/// # 比较是**逐组件**且**大小写敏感**的（这是刻意的）
///
/// `Path::starts_with` 按**路径组件**比较，不做任何规范化：`C:\Program Files\...` 与
/// `C:\PROGRAM FILES\...` 在 Windows 上指同一个目录，但这里会判成**不匹配**；8.3 短名
/// （`C:\PROGRA~1\...`）同理。所以大小写不同或用了短名的**合法**路径会被**误判为不在
/// 产品目录下**（对 `register_service` 而言就是一次多余的拒绝）。
///
/// 这里**故意**不做规范化：`canonicalize` 会解析符号链接/junction，反而给了「用一条指向
/// `%ProgramFiles%` 的 junction 骗过检查」的可乘之机——而本函数要挡的正是那种攻击。
/// 宁可对大小写/短名误拒（fail-closed），也不放过一个真正落在用户可写位置的路径。
#[must_use]
pub fn is_under_program_files(path: &Path) -> bool {
    path.starts_with(program_files_root())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_product_dir_is_a_subdirectory_not_program_data_itself() {
        // 直接把 host.toml 放在 %ProgramData% 下等于全局可读（那个目录对 Users 开放），
        // 而它含服务端全局 token。故落点必须是子目录。
        let root = program_data_root();
        assert!(root.ends_with(PRODUCT_DIR), "{}", root.display());
        let host = host_config_path();
        assert!(host.ends_with(Path::new(PRODUCT_DIR).join("host.toml")));
        assert_eq!(host.file_name().unwrap_or_default(), "host.toml");
    }

    #[test]
    fn the_installed_exe_names_are_fixed_not_archive_names() {
        // 安装时重命名到固定名：服务侧不必去猜归档名（CI 与本地构建的名字不同）。
        assert!(installed_service_exe().ends_with(SERVICE_EXE_NAME));
        assert!(installed_instance_exe().ends_with(INSTANCE_EXE_NAME));
        for name in [SERVICE_EXE_NAME, INSTANCE_EXE_NAME] {
            assert_eq!(
                Path::new(name).extension().and_then(|ext| ext.to_str()),
                Some("exe"),
                "{name}"
            );
        }
        assert_ne!(SERVICE_EXE_NAME, INSTANCE_EXE_NAME);
    }

    #[test]
    fn every_data_path_lives_under_the_product_dir() {
        for path in [
            host_config_path(),
            share_dir(),
            download_dir(),
            service_log_path(),
            probe_dir(),
        ] {
            assert!(
                path.starts_with(program_data_root()),
                "{} 不在产品目录下",
                path.display()
            );
            // `starts_with` 通过只说明它是产品目录的**子项**——`%ProgramData%` 本身
            // 对 `BUILTIN\Users` 开放，故落点绝不能是它自己。
            assert_ne!(path, program_data_root(), "{} 就是产品目录本身", path.display());
        }
    }

    #[test]
    fn program_files_root_is_the_executable_root() {
        assert!(program_files_root().ends_with(PRODUCT_DIR));
        assert!(is_under_program_files(&installed_service_exe()));
        assert!(is_under_program_files(&installed_instance_exe()));
        // 真机上两者必须是**不同**的根（可执行文件落点 vs 数据落点）。macOS 上两个
        // 环境变量都不存在、都回退到相对路径，故只在环境变量存在时才断言。
        if env_dir("ProgramData").is_some() && env_dir("ProgramW6432").is_some() {
            assert_ne!(program_files_root(), program_data_root());
            assert!(!is_under_program_files(&service_log_path()));
        }
    }
}
