//! 文件传输的服务端（被控端）侧文件系统访问：**沙箱化的共享目录**。
//!
//! # 为什么不做成平台 trait
//!
//! 三个平台都有文件系统，`tokio::fs` 已抹平差异；做成 trait 只会多一层无意义的间接。
//! （截屏/输入/剪贴板才需要 trait，因为它们真的按平台实现。）
//!
//! # 沙箱
//!
//! 所有路径都是**共享根下的虚拟路径**（`/` 分隔，`""` = 根）。[`FileService::resolve`]
//! 逐段校验：拒绝 `..`、空段、盘符与反斜杠，再 `canonicalize` 后**验证前缀**——
//! 后者防的是符号链接逃逸（`root/link -> /etc` 这类）。虚拟路径也让 UI 不必泄露被控端的
//! 真实目录结构。
//!
//! 校验分两层，缺一不可：
//!
//! 1. [`validate_virtual_path`] 做**语法**校验，把 `..`/绝对分量/反斜杠等直接挡在门外；
//! 2. [`FileService::resolve`] 做**语义**校验：`canonicalize` 后要求结果落在共享根的
//!    规范路径之下（`Path::starts_with`，而非字符串前缀——`/root2` 不能冒充 `/root`）。
//!    对**尚不存在**的路径（上传目标），先找到最深的已存在祖先、`canonicalize` 并验证它，
//!    再把剩余分量拼回去；[`FileService::open_write`] 只在完成这步校验之后才创建目录。

use crate::error::{RemoteError, RemoteResult};
use crate::screen::FileEntry;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

/// 单次传输的字节上限（1 TiB，纯粹防溢出/防呆，不是配额）。
pub const MAX_TRANSFER_BYTES: u64 = 1 << 40;

/// 一次文件传输的元信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferMeta {
    /// 共享根下的虚拟路径。
    pub path: String,
    /// 文件名（不含路径）。
    pub name: String,
    /// 字节数。
    pub size: u64,
}

/// 共享目录的文件服务。
#[derive(Debug, Clone)]
pub struct FileService {
    /// 共享根（绝对路径）。
    root: PathBuf,
}

impl FileService {
    /// 以 `root` 为共享根构造。
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 共享根。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把虚拟路径解析为共享根下的绝对路径，并做沙箱校验。
    ///
    /// 返回的是**规范（canonical）**路径：既解析了符号链接，也验证过它落在共享根之下。
    /// 对尚不存在的目标（上传），返回「最深已存在祖先的规范路径 + 剩余分量」，因此返回值
    /// 一定仍在共享根之下。
    ///
    /// # Errors
    /// 路径含 `..`/空段/绝对分量/反斜杠/盘符、共享根不可用、或解析后逃出共享根时返回
    /// [`RemoteError::Platform`]。
    pub fn resolve(&self, virtual_path: &str) -> RemoteResult<PathBuf> {
        let segments = validated_segments(virtual_path)?;
        let canonical_root = std::fs::canonicalize(&self.root)
            .map_err(|e| RemoteError::Platform(format!("共享根不可用: {e}")))?;

        let mut candidate = self.root.clone();
        for seg in &segments {
            candidate.push(seg);
        }

        // 找到最深的已存在祖先（`symlink_metadata` 连坏符号链接也算「存在」，
        // 于是坏链接会走 canonicalize 失败而不是被当成可新建路径）。
        let mut missing: Vec<OsString> = Vec::new();
        let mut probe = candidate;
        let existing = loop {
            if std::fs::symlink_metadata(&probe).is_ok() {
                break probe;
            }
            match probe.file_name() {
                Some(name) => missing.push(name.to_os_string()),
                None => return Err(RemoteError::Platform(format!("无法解析路径: {virtual_path}"))),
            }
            if !probe.pop() {
                return Err(RemoteError::Platform(format!("无法解析路径: {virtual_path}")));
            }
        };

        let canonical_existing = std::fs::canonicalize(&existing)
            .map_err(|e| RemoteError::Platform(format!("路径不可用 {}: {e}", existing.display())))?;
        if !canonical_existing.starts_with(&canonical_root) {
            return Err(RemoteError::Platform(format!(
                "路径逃出共享根: {virtual_path}"
            )));
        }

        let mut resolved = canonical_existing;
        for name in missing.iter().rev() {
            resolved.push(name);
        }
        Ok(resolved)
    }

    /// 列出目录内容（目录在前，各自按名排序）。
    ///
    /// 排序为**目录优先**，同类之间按名称**忽略大小写的升序**（`A` 与 `a` 视为同名）。
    /// 名称不是合法 UTF-8 的条目直接跳过（虚拟路径只能是 UTF-8）。
    ///
    /// 符号链接的处理：逐个 `canonicalize`，**指向共享根之外的一律跳过**（不列出、不可读）；
    /// 指向根内的符号链接照常列出，其类型/大小取自链接目标。
    ///
    /// [`FileEntry::path`] 是**虚拟路径**（父虚拟路径 + `/` + 名称，根为 `""`），不是绝对路径。
    /// 目录 `size` 恒为 0；文件取 `metadata.len()`。`modified_ms` 取修改时间的 Unix 毫秒数，
    /// 取不到时为 0。
    ///
    /// # Errors
    /// 路径非法、目录不存在或不可读时返回 [`RemoteError::Platform`]。
    pub async fn list(&self, virtual_path: &str) -> RemoteResult<Vec<FileEntry>> {
        let canonical_root = self.resolve("")?;
        let dir = self.resolve(virtual_path)?;

        let mut reader = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| RemoteError::Platform(format!("读取目录失败 {virtual_path}: {e}")))?;

        let mut entries = Vec::new();
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| RemoteError::Platform(format!("读取目录失败 {virtual_path}: {e}")))?
        {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            // 符号链接：解析目标并做根内检查；逃逸则跳过。非链接条目天然在规范父目录之下。
            let target = if file_type.is_symlink() {
                let Ok(canonical) = tokio::fs::canonicalize(entry.path()).await else {
                    continue;
                };
                if !canonical.starts_with(&canonical_root) {
                    continue;
                }
                canonical
            } else {
                entry.path()
            };
            let Ok(metadata) = tokio::fs::metadata(&target).await else {
                continue;
            };
            let is_dir = metadata.is_dir();
            let size = if is_dir { 0 } else { metadata.len() };
            let modified_ms = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| u64::try_from(d.as_millis()).ok())
                .unwrap_or(0);
            // `entry.path()` 规范父目录 + 名称，字面上就在规范根之下，故 `to_virtual` 无需再解析。
            let path = to_virtual(&canonical_root, &entry.path())?;

            entries.push(FileEntry {
                name,
                path,
                is_dir,
                size,
                modified_ms,
            });
        }

        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    /// 打开一个文件用于下载（读取）。
    ///
    /// 先 [`FileService::resolve`]（含 `canonicalize`），因此符号链接会被**跟随**，且根内检查
    /// 作用于**最终目标**——这正是安全的行为：指向根外的链接解析后前缀检查必然失败。
    /// 目标必须是**已存在的普通文件**（目录、设备、管道一律拒绝）。
    ///
    /// 返回的 [`TransferMeta::path`] 是规范化后的虚拟路径（去掉一个前导 `/`），
    /// [`TransferMeta::size`] 是磁盘上的实际大小。
    ///
    /// # Errors
    /// 路径非法、不是普通文件、不可读、或大小超过 [`MAX_TRANSFER_BYTES`] 时返回
    /// [`RemoteError::Platform`]。
    pub async fn open_read(&self, virtual_path: &str) -> RemoteResult<(TransferMeta, tokio::fs::File)> {
        let abs = self.resolve(virtual_path)?;
        let metadata = tokio::fs::metadata(&abs)
            .await
            .map_err(|e| RemoteError::Platform(format!("读取文件失败 {virtual_path}: {e}")))?;
        if !metadata.is_file() {
            return Err(RemoteError::Platform(format!("不是普通文件: {virtual_path}")));
        }
        if metadata.len() > MAX_TRANSFER_BYTES {
            return Err(RemoteError::Platform(format!(
                "文件超过传输上限 {MAX_TRANSFER_BYTES} 字节: {virtual_path}"
            )));
        }
        let name = abs
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| RemoteError::Platform(format!("文件名不是合法 UTF-8: {virtual_path}")))?
            .to_owned();
        let file = tokio::fs::File::open(&abs)
            .await
            .map_err(|e| RemoteError::Platform(format!("打开文件失败 {virtual_path}: {e}")))?;

        Ok((
            TransferMeta {
                path: normalize_virtual(virtual_path),
                name,
                size: metadata.len(),
            },
            file,
        ))
    }

    /// 打开一个文件用于上传（写入），必要时创建父目录。
    ///
    /// **会覆盖**同路径的既有文件（`create + truncate + write`）。
    ///
    /// 创建目录的顺序是安全的：先 [`FileService::resolve`] 完成「最深已存在祖先的
    /// `canonicalize` + 根内检查」，**只有校验通过之后**才对父目录 `create_dir_all`。
    /// 直接在未校验路径上创建目录正是要避免的漏洞。
    ///
    /// 返回的 [`TransferMeta::size`] 是调用方**声明的**大小（回显），不是磁盘实际大小。
    ///
    /// # Errors
    /// 声明大小超过 [`MAX_TRANSFER_BYTES`]、虚拟路径为空、目标是已存在的目录、路径非法或
    /// 逃出共享根、父目录不可创建、目标不可写时返回 [`RemoteError::Platform`]。
    pub async fn open_write(
        &self,
        virtual_path: &str,
        size: u64,
    ) -> RemoteResult<(TransferMeta, tokio::fs::File)> {
        if size > MAX_TRANSFER_BYTES {
            return Err(RemoteError::Platform(format!(
                "声明大小超过传输上限 {MAX_TRANSFER_BYTES} 字节"
            )));
        }
        let normalized = normalize_virtual(virtual_path);
        if normalized.is_empty() {
            return Err(RemoteError::Platform("不能写入共享根".to_owned()));
        }

        let abs = self.resolve(virtual_path)?;
        if let Ok(metadata) = tokio::fs::metadata(&abs).await {
            if metadata.is_dir() {
                return Err(RemoteError::Platform(format!(
                    "目标是目录，不能写入: {virtual_path}"
                )));
            }
        }
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| RemoteError::Platform(format!("创建父目录失败: {e}")))?;
        }
        let name = abs
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| RemoteError::Platform(format!("文件名不是合法 UTF-8: {virtual_path}")))?
            .to_owned();
        let file = tokio::fs::File::create(&abs)
            .await
            .map_err(|e| RemoteError::Platform(format!("创建文件失败 {virtual_path}: {e}")))?;

        Ok((
            TransferMeta {
                path: normalized,
                name,
                size,
            },
            file,
        ))
    }
}

/// 校验虚拟路径的每一段（供 [`FileService::resolve`] 与 UI 复用）。
///
/// 允许：空串、`/`、`a`、`a/b`、`/a/b`（一个前导 `/` 表示「从根开始」，会被剥掉）。
/// 拒绝：空段（`a//b`）、`.`、`..`、任何反斜杠（Windows 分隔符）、Windows 盘符前缀
/// （`C:` 这类）、以及控制字符（含 NUL）。
///
/// # Errors
/// 命中上述任一非法情形时返回 [`RemoteError::Platform`]，消息里带上出错的段。
pub fn validate_virtual_path(virtual_path: &str) -> RemoteResult<()> {
    // 允许并剥掉恰好一个前导 `/`（`"/"` 与 `""` 都表示根）。
    let trimmed = virtual_path.strip_prefix('/').unwrap_or(virtual_path);
    if trimmed.is_empty() {
        return Ok(());
    }

    for segment in trimmed.split('/') {
        if segment.is_empty() {
            return Err(RemoteError::Platform("路径含空段".to_owned()));
        }
        if segment == "." || segment == ".." {
            return Err(RemoteError::Platform(format!("路径含非法段: {segment}")));
        }
        if segment.contains('\\') {
            return Err(RemoteError::Platform(format!(
                "路径含反斜杠: {segment}"
            )));
        }
        let bytes = segment.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            return Err(RemoteError::Platform(format!("路径含盘符前缀: {segment}")));
        }
        if segment.chars().any(char::is_control) {
            return Err(RemoteError::Platform(format!(
                "路径含控制字符: {segment}"
            )));
        }
    }
    Ok(())
}

/// 把绝对路径转成虚拟路径（相对共享根，`/` 分隔）。
///
/// 根自身 → `""`。
///
/// 直接 `strip_prefix` 失败时会退化为**先 `canonicalize` 双方再剥离**：被控端共享根在
/// macOS 上常是符号链接（`/var` → `/private/var`），而 [`FileService::resolve`] 返回规范路径，
/// 于是「原样根 + 规范路径」字面不匹配；这一步让往返转换仍然成立。
///
/// # Errors
/// `abs` 不在 `root` 之下，或分量不是合法 UTF-8 时返回 [`RemoteError::Platform`]。
pub fn to_virtual(root: &Path, abs: &Path) -> RemoteResult<String> {
    let relative = if let Ok(rel) = abs.strip_prefix(root) {
        rel.to_path_buf()
    } else {
        let canonical_root = std::fs::canonicalize(root)
            .map_err(|e| RemoteError::Platform(format!("共享根不可用: {e}")))?;
        let canonical_abs = std::fs::canonicalize(abs)
            .map_err(|e| RemoteError::Platform(format!("路径不可用 {}: {e}", abs.display())))?;
        canonical_abs
            .strip_prefix(&canonical_root)
            .map_err(|_| {
                RemoteError::Platform(format!(
                    "路径 {} 不在共享根 {} 之下",
                    abs.display(),
                    root.display()
                ))
            })?
            .to_path_buf()
    };

    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| RemoteError::Platform("路径含非 UTF-8 分量".to_owned()))?;
                parts.push(part);
            }
            Component::CurDir => {}
            _ => {
                return Err(RemoteError::Platform(format!(
                    "路径含非普通分量: {}",
                    abs.display()
                )));
            }
        }
    }
    Ok(parts.join("/"))
}

/// 把虚拟路径规范化为「无前导 `/`」的形式（`""` 与 `"/"` 都归一为 `""`）。
fn normalize_virtual(virtual_path: &str) -> String {
    virtual_path.strip_prefix('/').unwrap_or(virtual_path).to_owned()
}

/// 校验并拆出虚拟路径的各段（已剥掉前导 `/`，不含空段）。
fn validated_segments(virtual_path: &str) -> RemoteResult<Vec<OsString>> {
    validate_virtual_path(virtual_path)?;
    let trimmed = virtual_path.strip_prefix('/').unwrap_or(virtual_path);
    Ok(trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .map(OsString::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 带自清理的临时根目录（不引入 `tempfile` 依赖）。
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("rt-remote-file-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn service(&self) -> FileService {
            FileService::new(self.0.clone())
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn validate_accepts_legal_paths() {
        for ok in ["", "/", "a", "a/b", "/a/b"] {
            assert!(validate_virtual_path(ok).is_ok(), "应接受 {ok:?}");
        }
    }

    #[test]
    fn validate_rejects_illegal_paths() {
        for bad in ["..", "a/../b", "a//b", ".", "a\\b", "C:\\x", "a\0b", "//a"] {
            assert!(validate_virtual_path(bad).is_err(), "应拒绝 {bad:?}");
        }
    }

    #[test]
    fn resolve_maps_virtual_to_absolute() {
        let temp = TempRoot::new("resolve");
        let service = temp.service();
        std::fs::create_dir_all(temp.0.join("a/b")).unwrap();

        let canonical_root = std::fs::canonicalize(&temp.0).unwrap();
        assert_eq!(service.resolve("").unwrap(), canonical_root);
        assert_eq!(service.resolve("/").unwrap(), canonical_root);
        assert_eq!(
            service.resolve("a/b").unwrap(),
            std::fs::canonicalize(temp.0.join("a/b")).unwrap()
        );
    }

    #[test]
    fn resolve_rejects_sandbox_escapes() {
        let temp = TempRoot::new("escape");
        let service = temp.service();
        for bad in ["..", "a/../../etc", "../outside", "a/../b"] {
            assert!(service.resolve(bad).is_err(), "应拒绝 {bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_escape() {
        let temp = TempRoot::new("symlink");
        let outside = TempRoot::new("outside");
        let service = temp.service();
        std::os::unix::fs::symlink(&outside.0, temp.0.join("escape")).unwrap();

        assert!(service.resolve("escape").is_err(), "符号链接逃逸必须被拒");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_skips_symlink_escape() {
        let temp = TempRoot::new("list-symlink");
        let outside = TempRoot::new("list-outside");
        let service = temp.service();
        std::os::unix::fs::symlink(&outside.0, temp.0.join("escape")).unwrap();
        std::fs::write(temp.0.join("keep.txt"), b"hi").unwrap();

        let entries = service.list("").await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"keep.txt"));
        assert!(!names.contains(&"escape"), "逃逸链接不应被列出");
    }

    #[tokio::test]
    async fn list_orders_dirs_first_and_reports_virtual_paths() {
        let temp = TempRoot::new("list-order");
        let service = temp.service();
        std::fs::create_dir_all(temp.0.join("subdir")).unwrap();
        std::fs::write(temp.0.join("zeta.txt"), b"12345").unwrap();
        std::fs::write(temp.0.join("Alpha.txt"), b"ab").unwrap();

        let entries = service.list("").await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["subdir", "Alpha.txt", "zeta.txt"]);

        let subdir = &entries[0];
        assert!(subdir.is_dir);
        assert_eq!(subdir.size, 0);
        assert_eq!(subdir.path, "subdir");

        let alpha = entries.iter().find(|e| e.name == "Alpha.txt").unwrap();
        assert!(!alpha.is_dir);
        assert_eq!(alpha.size, 2);
        assert_eq!(alpha.path, "Alpha.txt");
    }

    #[tokio::test]
    async fn list_uses_parent_virtual_prefix() {
        let temp = TempRoot::new("list-nested");
        let service = temp.service();
        std::fs::create_dir_all(temp.0.join("a/b")).unwrap();
        std::fs::write(temp.0.join("a/b/c.txt"), b"x").unwrap();

        let entries = service.list("a/b").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a/b/c.txt");
    }

    #[tokio::test]
    async fn open_read_returns_exact_bytes() {
        let temp = TempRoot::new("read");
        let service = temp.service();
        let payload = b"hello remote files";
        std::fs::write(temp.0.join("f.bin"), payload).unwrap();

        let (meta, mut file) = service.open_read("f.bin").await.unwrap();
        assert_eq!(meta.name, "f.bin");
        assert_eq!(meta.path, "f.bin");
        assert_eq!(meta.size, payload.len() as u64);

        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, payload);
    }

    #[tokio::test]
    async fn open_read_normalizes_leading_slash() {
        let temp = TempRoot::new("read-normalize");
        let service = temp.service();
        std::fs::write(temp.0.join("f.txt"), b"y").unwrap();
        let (meta, _file) = service.open_read("/f.txt").await.unwrap();
        assert_eq!(meta.path, "f.txt");
    }

    #[tokio::test]
    async fn open_read_rejects_directory() {
        let temp = TempRoot::new("read-dir");
        let service = temp.service();
        std::fs::create_dir_all(temp.0.join("d")).unwrap();
        assert!(service.open_read("d").await.is_err());
    }

    #[tokio::test]
    async fn open_write_creates_intermediate_dirs_and_writes() {
        let temp = TempRoot::new("write");
        let service = temp.service();
        let (meta, mut file) = service.open_write("sub/dir/f.txt", 5).await.unwrap();
        assert_eq!(meta.path, "sub/dir/f.txt");
        assert_eq!(meta.name, "f.txt");
        assert_eq!(meta.size, 5);
        file.write_all(b"hello").await.unwrap();
        file.flush().await.unwrap();
        drop(file);

        let on_disk = std::fs::read(temp.0.join("sub/dir/f.txt")).unwrap();
        assert_eq!(on_disk, b"hello");
    }

    #[tokio::test]
    async fn open_write_rejects_escape_and_creates_nothing() {
        let temp = TempRoot::new("write-escape");
        let service = temp.service();
        assert!(service.open_write("../evil.txt", 3).await.is_err());
        assert!(!std::env::temp_dir().join("evil.txt").exists());
    }

    #[tokio::test]
    async fn open_write_rejects_empty_path() {
        let temp = TempRoot::new("write-empty");
        let service = temp.service();
        assert!(service.open_write("", 3).await.is_err());
        assert!(service.open_write("/", 3).await.is_err());
    }

    #[tokio::test]
    async fn open_write_rejects_existing_directory() {
        let temp = TempRoot::new("write-dir");
        let service = temp.service();
        std::fs::create_dir_all(temp.0.join("d")).unwrap();
        assert!(service.open_write("d", 3).await.is_err());
    }

    #[tokio::test]
    async fn oversize_transfers_are_rejected() {
        let temp = TempRoot::new("oversize");
        let service = temp.service();

        assert!(service
            .open_write("big.bin", MAX_TRANSFER_BYTES + 1)
            .await
            .is_err());

        // 稀疏文件：只声明长度，不实际写 1 TiB。
        let sparse = std::fs::File::create(temp.0.join("sparse.bin")).unwrap();
        sparse.set_len(MAX_TRANSFER_BYTES + 1).unwrap();
        drop(sparse);
        assert!(service.open_read("sparse.bin").await.is_err());
    }

    #[test]
    fn to_virtual_round_trips_with_resolve() {
        let temp = TempRoot::new("roundtrip");
        let service = temp.service();
        std::fs::create_dir_all(temp.0.join("a/b")).unwrap();

        for virtual_path in ["", "a", "a/b"] {
            let abs = service.resolve(virtual_path).unwrap();
            let back = to_virtual(service.root(), &abs).unwrap();
            assert_eq!(back, virtual_path, "往返转换失败: {virtual_path:?}");
        }
    }

    #[test]
    fn to_virtual_rejects_outside_path() {
        let temp = TempRoot::new("to-virtual-outside");
        let service = temp.service();
        let outside = TempRoot::new("to-virtual-outside-target");
        assert!(to_virtual(service.root(), &outside.0).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_write_rejects_symlinked_parent_escape() {
        let temp = TempRoot::new("write-symlink");
        let outside = TempRoot::new("write-symlink-outside");
        let service = temp.service();
        std::os::unix::fs::symlink(&outside.0, temp.0.join("link")).unwrap();

        assert!(service.open_write("link/evil.txt", 3).await.is_err());
        assert!(!outside.0.join("evil.txt").exists(), "不得在根外落盘");
    }
}
