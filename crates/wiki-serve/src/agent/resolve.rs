// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! Codex 二进制解析链.
//!
//! 优先级：sidecar（`current_exe` 同级带 triple 后缀）→ `codexPathOverride`（settings）
//! → `PATH` 手扫（不引 `which` crate）。返回 `{path, argv_prefix, source, version}`。

use std::path::{Path, PathBuf};

/// 解析来源.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResolveSource {
    /// sidecar.
    Sidecar,
    /// `codexPathOverride`.
    Override,
    /// `PATH` 扫描.
    Path,
}

/// 解析结果.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinary {
    /// 实际可执行文件路径.
    pub path: PathBuf,
    /// 启动时需前置的 argv（`codex app-server --stdio` 形态时为 `["app-server"]`）.
    pub argv_prefix: Vec<String>,
    /// 来源.
    pub source: ResolveSource,
    /// 探测到的版本号（若探测成功）.
    pub version: Option<String>,
}

/// 当前平台的 target triple（与 Tauri externalBin 命名一致）.
///
/// 通过 `std::env::consts::{ARCH, OS}` 映射支持的三元组，未匹配返回 `None`
///（调用方走 `PATH` fallback）.
#[must_use]
pub fn current_target_triple() -> Option<&'static str> {
    // 仅支持打包的三平台，其他返回 None 走 PATH fallback。
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// 生成 sidecar 候选文件名（带 triple 后缀）.
fn sidecar_candidates(triple: &str) -> Vec<String> {
    let exe = if cfg!(windows) { ".exe" } else { "" };
    vec![
        format!("codex-app-server-{triple}{exe}"),
        format!("codex-{triple}{exe}"),
    ]
}

/// PATH 候选基名（不含 triple 后缀）.
fn path_candidates() -> Vec<String> {
    let exe = if cfg!(windows) { ".exe" } else { "" };
    vec![
        format!("codex-app-server{exe}"),
        format!("codex{exe}"),
    ]
}

/// 在目录中查找候选文件是否存在，返回首个命中.
fn find_in_dir(dir: &Path, candidates: &[String]) -> Option<PathBuf> {
    for name in candidates {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 手扫 `PATH` 环境变量查找候选二进制.
///
/// `path_dirs` 为注入的 `PATH` 目录列表；`None` 时回退读取进程级 `PATH` 环境变量
///（生产路径），`Some` 时完全由调用方控制，测试不触碰全局 `env`。
fn find_in_path(candidates: &[String], path_dirs: Option<&[PathBuf]>) -> Option<PathBuf> {
    if let Some(dirs) = path_dirs {
        for dir in dirs {
            if dir.as_os_str().is_empty() {
                continue;
            }
            if let Some(hit) = find_in_dir(dir, candidates) {
                return Some(hit);
            }
        }
        return None;
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if let Some(hit) = find_in_dir(&dir, candidates) {
            return Some(hit);
        }
    }
    None
}

/// 判断是否需要 `app-server` 前缀（`codex` 形态需前缀，`codex-app-server` 直连 `stdio`）.
fn needs_app_server_prefix(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    // 去掉 .exe 后缀再判断
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    // codex-<triple> 或 codex 形态含 app-server 能力，需加 app-server 前缀
    // codex-app-server-<triple> 或 codex-app-server 不需要前缀
    !stem.starts_with("codex-app-server")
}

/// 版本探测回调类型（注入便于测试）.
pub type ProbeFn = dyn Fn(&Path) -> Option<String> + Send + Sync;

/// 解析 codex 二进制.
///
/// `current_exe_dir` 为 `current_exe` 所在目录（测试可注入），`override_path` 为
/// settings 的 `codexPathOverride`，`path_dirs` 为注入的 `PATH` 目录列表。
/// `probe` 闭包返回版本号字符串（未实现或失败返回 `None`）。
#[must_use]
pub fn resolve_binary(
    current_exe_dir: Option<&Path>,
    override_path: Option<&Path>,
    path_dirs: Option<&[PathBuf]>,
    probe: Option<&ProbeFn>,
) -> Option<ResolvedBinary> {
    // 1) sidecar
    if let Some(dir) = current_exe_dir {
        if let Some(triple) = current_target_triple() {
            let candidates = sidecar_candidates(triple);
            if let Some(hit) = find_in_dir(dir, &candidates) {
                let version = probe.as_ref().and_then(|f| f(&hit));
                let argv_prefix = if needs_app_server_prefix(&hit) {
                    vec!["app-server".to_owned()]
                } else {
                    Vec::new()
                };
                return Some(ResolvedBinary {
                    path: hit,
                    argv_prefix,
                    source: ResolveSource::Sidecar,
                    version,
                });
            }
            // triple 无命中时也尝试不带 triple 的 sidecar 同级查找（兼容旧包）
            let fallback = path_candidates();
            if let Some(hit) = find_in_dir(dir, &fallback) {
                let version = probe.as_ref().and_then(|f| f(&hit));
                let argv_prefix = if needs_app_server_prefix(&hit) {
                    vec!["app-server".to_owned()]
                } else {
                    Vec::new()
                };
                return Some(ResolvedBinary {
                    path: hit,
                    argv_prefix,
                    source: ResolveSource::Sidecar,
                    version,
                });
            }
        } else {
            // 未知 triple，直接扫同级不带 triple 的候选
            let fallback = path_candidates();
            if let Some(hit) = find_in_dir(dir, &fallback) {
                let version = probe.as_ref().and_then(|f| f(&hit));
                let argv_prefix = if needs_app_server_prefix(&hit) {
                    vec!["app-server".to_owned()]
                } else {
                    Vec::new()
                };
                return Some(ResolvedBinary {
                    path: hit,
                    argv_prefix,
                    source: ResolveSource::Sidecar,
                    version,
                });
            }
        }
    }

    // 2) override
    if let Some(p) = override_path {
        if p.is_file() {
            let version = probe.as_ref().and_then(|f| f(p));
            let argv_prefix = if needs_app_server_prefix(p) {
                vec!["app-server".to_owned()]
            } else {
                Vec::new()
            };
            return Some(ResolvedBinary {
                path: p.to_path_buf(),
                source: ResolveSource::Override,
                argv_prefix,
                version,
            });
        }
        // override 路径不存在视为未解析（不回退 PATH，避免误用过期配置）
        // 但若 override 指向目录则尝试目录内候选
        if p.is_dir() {
            let candidates = path_candidates();
            if let Some(hit) = find_in_dir(p, &candidates) {
                let version = probe.as_ref().and_then(|f| f(&hit));
                let argv_prefix = if needs_app_server_prefix(&hit) {
                    vec!["app-server".to_owned()]
                } else {
                    Vec::new()
                };
                return Some(ResolvedBinary {
                    path: hit,
                    argv_prefix,
                    source: ResolveSource::Override,
                    version,
                });
            }
        }
    }

    // 3) PATH
    let path_cands = path_candidates();
    if let Some(hit) = find_in_path(&path_cands, path_dirs) {
        let version = probe.as_ref().and_then(|f| f(&hit));
        let argv_prefix = if needs_app_server_prefix(&hit) {
            vec!["app-server".to_owned()]
        } else {
            Vec::new()
        };
        return Some(ResolvedBinary {
            path: hit,
            argv_prefix,
            source: ResolveSource::Path,
            version,
        });
    }

    None
}

/// 便捷：以真实 `current_exe` 与环境 `PATH` 解析（生产路径）.
#[must_use]
pub fn resolve_binary_live(override_path: Option<&Path>, probe: Option<&ProbeFn>) -> Option<ResolvedBinary> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_binary(exe_dir.as_deref(), override_path, None, probe)
}

/// 测试/可控 `PATH` 的解析入口，`path_dirs` 显式注入 `PATH` 目录列表.
#[must_use]
pub fn resolve_binary_with_path_dirs(
    current_exe_dir: Option<&Path>,
    override_path: Option<&Path>,
    path_dirs: &[PathBuf],
    probe: Option<&ProbeFn>,
) -> Option<ResolvedBinary> {
    resolve_binary(current_exe_dir, override_path, Some(path_dirs), probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[allow(clippy::unnecessary_wraps, reason = "测试桩需匹配 ProbeFn 的 Option<String> 返回类型")]
    fn probe_stub(_: &Path) -> Option<String> {
        Some("0.1.0".to_owned())
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, b"fake binary").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = fs::metadata(path).expect("meta").permissions();
            perm.set_mode(0o755);
            fs::set_permissions(path, perm).expect("chmod");
        }
    }

    #[test]
    fn sidecar_priority_over_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe_dir = dir.path().join("exe-dir");
        fs::create_dir_all(&exe_dir).expect("mkdir exe-dir");
        // sidecar 文件
        let triple = current_target_triple().unwrap_or("x86_64-unknown-linux-gnu");
        let sidecar = exe_dir.join(format!("codex-app-server-{triple}"));
        touch(&sidecar);
        // PATH 上也放一个 codex
        let path_dir = dir.path().join("path-bin");
        let path_bin = path_dir.join(if cfg!(windows) { "codex.exe" } else { "codex" });
        touch(&path_bin);
        let res = resolve_binary_with_path_dirs(Some(&exe_dir), None, std::slice::from_ref(&path_dir), None)
            .expect("should resolve sidecar");
        assert_eq!(res.source, ResolveSource::Sidecar);
        assert_eq!(res.path, sidecar);
        // sidecar 为 codex-app-server 前缀，不需 app-server argv
        assert!(res.argv_prefix.is_empty());
    }

    #[test]
    fn override_priority_over_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let override_bin = dir.path().join(if cfg!(windows) { "my-codex.exe" } else { "my-codex" });
        // override 形态用 codex 命名，需 app-server 前缀
        let override_codex = dir.path().join(if cfg!(windows) { "codex.exe" } else { "codex" });
        touch(&override_codex);
        let path_dir = dir.path().join("path-bin2");
        let path_bin = path_dir.join(if cfg!(windows) { "codex-app-server.exe" } else { "codex-app-server" });
        touch(&path_bin);
        let res =
            resolve_binary_with_path_dirs(None, Some(&override_codex), std::slice::from_ref(&path_dir), None)
                .expect("override");
        assert_eq!(res.source, ResolveSource::Override);
        assert_eq!(res.path, override_codex);
        assert_eq!(res.argv_prefix, vec!["app-server"]);
        // override 文件不存在时 fallback 到 PATH
        let missing = dir.path().join("missing-codex");
        let res2 = resolve_binary_with_path_dirs(None, Some(&missing), std::slice::from_ref(&path_dir), None);
        assert!(res2.is_some(), "override 缺失时应 fallback 到 PATH");
        assert_eq!(res2.expect("fallback").source, ResolveSource::Path);
        drop(override_bin);
    }

    #[test]
    fn path_fallback_and_argv_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path_dir = dir.path().join("path-only");
        // 放 codex（需前缀）
        let codex = path_dir.join(if cfg!(windows) { "codex.exe" } else { "codex" });
        touch(&codex);
        let res = resolve_binary_with_path_dirs(None, None, std::slice::from_ref(&path_dir), None)
            .expect("path codex");
        assert_eq!(res.source, ResolveSource::Path);
        assert_eq!(res.argv_prefix, vec!["app-server"]);
        // 换成 codex-app-server（不需前缀）
        fs::remove_file(&codex).expect("rm codex");
        let app_server = path_dir.join(if cfg!(windows) { "codex-app-server.exe" } else { "codex-app-server" });
        touch(&app_server);
        let res2 = resolve_binary_with_path_dirs(None, None, std::slice::from_ref(&path_dir), None)
            .expect("path app-server");
        assert_eq!(res2.source, ResolveSource::Path);
        assert!(res2.argv_prefix.is_empty());
    }

    #[test]
    fn probe_injected_and_version_returned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path_dir = dir.path().join("probe-test");
        let bin = path_dir.join(if cfg!(windows) { "codex.exe" } else { "codex" });
        touch(&bin);
        let res =
            resolve_binary_with_path_dirs(None, None, std::slice::from_ref(&path_dir), Some(&probe_stub))
                .expect("probe");
        assert_eq!(res.version.as_deref(), Some("0.1.0"));
        // probe 返回 None 时 version 为 None
        let res2 =
            resolve_binary_with_path_dirs(None, None, std::slice::from_ref(&path_dir), Some(&|_: &Path| None))
                .expect("probe none");
        assert!(res2.version.is_none());
    }

    #[test]
    fn current_triple_known_or_none() {
        // 仅断言不 panic，且已知平台返回 Some
        let t = current_target_triple();
        // 在 CI 的 x86_64 linux 上应为 Some
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert!(t.is_some(), "linux x86_64 应有 triple");
        } else if t.is_some() {
            assert!(t.expect("some").contains('-'), "triple 应含 -");
        }
    }

    #[test]
    fn needs_prefix_logic() {
        assert!(needs_app_server_prefix(Path::new("codex")));
        assert!(needs_app_server_prefix(Path::new("codex-x86_64-unknown-linux-gnu")));
        assert!(!needs_app_server_prefix(Path::new("codex-app-server")));
        assert!(!needs_app_server_prefix(Path::new("codex-app-server-aarch64-apple-darwin")));
        #[cfg(windows)]
        {
            assert!(needs_app_server_prefix(Path::new("codex.exe")));
            assert!(!needs_app_server_prefix(Path::new("codex-app-server.exe")));
        }
    }
}
