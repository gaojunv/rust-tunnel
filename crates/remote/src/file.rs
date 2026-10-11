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
//! 1. [`validate_virtual_path`] 做**语法**校验，把 `..`/绝对分量/反斜杠等直接挡在门外，
//!    并拒绝 Windows 会在系统调用层归一化的写法（段尾随 `.`/空格、全点段、段内 `:`、
//!    保留设备名——清单见该函数）；
//! 2. [`FileService::resolve`] 做**语义**校验：`canonicalize` 后要求结果落在共享根的
//!    规范路径之下（`Path::starts_with`，而非字符串前缀——`/root2` 不能冒充 `/root`）。
//!    对**尚不存在**的路径（上传目标），先找到最深的已存在祖先、`canonicalize` 并验证它，
//!    再把剩余分量拼回去，**并对最终路径的每个普通分量复跑一遍段级规则**——这样整条路径
//!    都过了同一套校验，而不是只有最深已存在祖先过了。[`FileService::open_write`] 只在
//!    完成这步校验之后才创建目录。

use crate::error::{RemoteError, RemoteResult};
use crate::screen::FileEntry;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;
use tracing::warn;

/// 单次传输的字节上限（1 TiB，纯粹防溢出/防呆，不是配额）。
pub const MAX_TRANSFER_BYTES: u64 = 1 << 40;

/// 单次目录列表的**条目数**上限（[`FileService::list`] 的硬限之一）。
pub const MAX_LIST_ENTRIES: usize = 10_000;

/// 单次目录列表的**估算载荷字节**上限（[`FileService::list`] 的另一重硬限）。
///
/// 估算 = `Σ(名称长度 + 虚拟路径长度) + 每条固定开销`（常量 `LIST_ENTRY_FIXED_BYTES`，
/// 取 64 字节，覆盖 bincode 里两个 `String` 的长度前缀、一个 `bool` 与两个 `u64`）。
/// 这是**估算**而非精确编码长度：精确值要真的编码一遍，而目的只是不撞上帧上限。
pub const MAX_LIST_PAYLOAD_BYTES: usize = 1 << 20;

/// 单个 [`FileEntry`] 在 bincode 载荷里的固定开销估算（见 [`MAX_LIST_PAYLOAD_BYTES`]）。
const LIST_ENTRY_FIXED_BYTES: usize = 64;

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
        // 纵深防御：上面只校验了「字面量虚拟路径的段」和「最深已存在祖先的 canonical 前缀」，
        // 拼回来的 `missing` 分量没有再过一遍语法校验。这里对**共享根之下**的每个普通分量复跑
        // 同一套段级规则，确保整条虚拟路径都过了同一个门（canonicalize 之后的真实分量也不例外
        // ——例如根内一个名为 `a.` 的真实目录，链接到它也照样被拒）。
        //
        // 只查根**之下**的分量：共享根自身的路径分量不是用户可控的虚拟路径（根可能叫
        // `/data/aux`、`/mnt/backup.` 或含 `:` 的目录），拿段规则去套会把整个共享目录判死。
        let under_root = resolved
            .strip_prefix(&canonical_root)
            .map_err(|_| RemoteError::Platform(format!("路径逃出共享根: {virtual_path}")))?;
        for component in under_root.components() {
            if let Component::Normal(part) = component {
                let part = part.to_str().ok_or_else(|| {
                    RemoteError::Platform(format!("路径含非 UTF-8 分量: {virtual_path}"))
                })?;
                validate_segment(part)?;
            }
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
    /// # 列表上限（为什么不是分页）
    ///
    /// 一次列表结果就是**一个** `SessionMessage::FileList` 帧（走 `File` lane）。帧有硬上限
    /// [`crate::session::MAX_SESSION_FRAME`]（16 MiB），一旦编码后越界，`writer.send` 直接报错、
    /// 会话循环随即 `break`——表现就是「打开一个大目录就断线」。所以必须在被控端**先**限死：
    /// 条目数 ≤ [`MAX_LIST_ENTRIES`]，且估算载荷 ≤ [`MAX_LIST_PAYLOAD_BYTES`]，先到者为准，
    /// 到限即**静默截断**（多余的条目直接丢弃），并 `warn!` 记一次被截断的条目数与目录名。
    ///
    /// 截断而不是分页，是因为**分页需要给 `FileList` 加字段**（游标/偏移），而会话协议目前
    /// 只靠 `proto_version` **相等**来门控：同一次会话里两端必须用同一份帧结构，给老客户端
    /// 塞一个多字段的帧会直接解不出来。所以要分页就得等一次协议版本升级，不能顺手加字段。
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
        // 估算的载荷字节（见 `MAX_LIST_PAYLOAD_BYTES` 的说明）。
        let mut estimated_bytes = 0usize;
        // 被上限丢弃的条目数（用于一次性告警）。
        let mut truncated = 0usize;
        // 字节预算已满后，不再构建后续条目（省掉 `metadata`/链接解析这类系统调用），只计数。
        let mut full = false;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| RemoteError::Platform(format!("读取目录失败 {virtual_path}: {e}")))?
        {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if full || entries.len() >= MAX_LIST_ENTRIES {
                truncated += 1;
                continue;
            }
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

            // 字节预算：先判再加，保证累计值**不越过**预算（否则最后一条会把总量顶出去）。
            let cost = LIST_ENTRY_FIXED_BYTES + name.len() + path.len();
            if estimated_bytes + cost > MAX_LIST_PAYLOAD_BYTES {
                full = true;
                truncated += 1;
                continue;
            }
            estimated_bytes += cost;

            entries.push(FileEntry {
                name,
                path,
                is_dir,
                size,
                modified_ms,
            });
        }

        if truncated > 0 {
            warn!(
                dir = %virtual_path,
                kept = entries.len(),
                truncated,
                "目录条目超过上限，已静默截断"
            );
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

/// Windows 保留设备名（大小写不敏感）。Windows 上打开 `CON`/`NUL`/`COM1`… 得到的是**设备**
/// 而非共享根下的文件，于是它们能绕过「文件都在共享根内」这一前提。
///
/// 带扩展名的形式同样命中（`CON.txt`），因为 Windows 只看第一个 `.` 之前的部分——
/// 判定逻辑见 [`validate_segment`]。
const WINDOWS_RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 校验虚拟路径的每一段（供 [`FileService::resolve`] 与 UI 复用）。
///
/// 允许：空串、`/`、`a`、`a/b`、`/a/b`（一个前导 `/` 表示「从根开始」，会被剥掉）。
/// 拒绝规则全部落在段级函数 [`validate_segment`] 里（这里只负责拆段）。
///
/// # Errors
/// 任一段命中 [`validate_segment`] 的非法情形时返回 [`RemoteError::Platform`]。
pub fn validate_virtual_path(virtual_path: &str) -> RemoteResult<()> {
    // 允许并剥掉恰好一个前导 `/`（`"/"` 与 `""` 都表示根）。
    let trimmed = virtual_path.strip_prefix('/').unwrap_or(virtual_path);
    if trimmed.is_empty() {
        return Ok(());
    }

    for segment in trimmed.split('/') {
        validate_segment(segment)?;
    }
    Ok(())
}

/// 校验**单个**路径段（[`validate_virtual_path`] 与 [`FileService::resolve`] 的纵深复检共用）。
///
/// 除字面量 `..`/空段/反斜杠/控制字符外，这里把「Windows 会在系统调用层归一化、从而让字面量
/// 检查失效」的写法一并挡掉。这些规则**与平台无关**（不按 `cfg(windows)` 分叉），因此
/// macOS/Linux 上的回归测试就能锁住它们：
///
/// - **段尾随 `.`/空格**：Windows 归一化（`".. "` 与 `".."` 等价、`"a. "` 与 `"a"` 等价），
///   字面量只拒 `"."`/`".."` 挡不住；
/// - **全点段**：`"..."` 会被 Windows 归一化成 `.`/`..`；
/// - **段内含 `:`**：NTFS 备用数据流，`"file.txt:secret"` 指向另一个数据流；
/// - **Windows 保留设备名**：`CON`/`NUL`/`COM1`…（含带扩展名形式），它们指向设备而非根内文件。
///
/// # 与 Unix 的取舍
/// `:` 与保留设备名在 Unix 上都是**合法**文件名。这里选择**跨平台一律拒绝**：虚拟路径只有
/// 一套规则，若按平台分叉，被控端与控制器可能对同一路径给出不同结论，沙箱边界随之模糊。
/// 代价是 Unix 上名为 `a:b`、`con` 的文件**仍会被 [`FileService::list`] 列出**（列表逐条不
/// 校验名字），但用户按列表给出的虚拟路径去下载时会被拒——即「列得出来、打不开」。这是有意
/// 的：宁可少开一个合法文件，也不能让 Windows 上一条路径逃出共享根。
///
/// # Errors
/// 命中任一非法情形时返回 [`RemoteError::Platform`]，消息里带上出错的段。
fn validate_segment(segment: &str) -> RemoteResult<()> {
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
    // 尾随 `.`/空格：Windows 系统调用层会剥掉它们（`".. "` → `..`、`"a. "` → `a`），
    // 于是字面量 `..` 检查形同虚设。全点段（`"..."`）同样被归一化，一并拒绝。
    if segment.trim_end_matches(['.', ' ']) != segment {
        return Err(RemoteError::Platform(format!(
            "路径段含尾随点或空格（Windows 会归一化）: {segment}"
        )));
    }
    // `:`：NTFS 备用数据流。这一条也顺带盖住了盘符前缀（`C:` 含 `:`）。
    if segment.contains(':') {
        return Err(RemoteError::Platform(format!("路径段含冒号: {segment}")));
    }
    if segment.chars().any(char::is_control) {
        return Err(RemoteError::Platform(format!(
            "路径含控制字符: {segment}"
        )));
    }
    // 保留设备名：只看第一个 `.` 之前的部分，故 `CON.txt` 也命中（大小写不敏感）。
    let base = segment.split('.').next().unwrap_or(segment);
    if WINDOWS_RESERVED_NAMES.contains(&base.to_ascii_uppercase().as_str()) {
        return Err(RemoteError::Platform(format!(
            "路径段是 Windows 保留设备名: {segment}"
        )));
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

    /// 段尾随空格/点、全点段、段内冒号、保留设备名——每条对应一种 Windows 在系统调用层
    /// 的归一化行为或 NTFS 特性。规则与平台无关，故本测试在 macOS/Linux 上同样有效。
    #[test]
    fn validate_rejects_windows_normalization_tricks() {
        let cases = [
            // 段尾随空格：Windows 归一化为 `..`，字面量 `..` 检查拦不住（越狱主力）。
            "a/.. /.. /evil",
            // 段尾随空格（非 `..` 形式）+ 非法段混排。
            "a/./.. ",
            // 全点段：Windows 归一化为 `.`/`..`。
            "...",
            // 尾随空段。
            "a/",
            // NTFS 备用数据流：`:` 之后是隐藏数据流。
            "x:y",
            "file.txt:secret",
            // Windows 保留设备名（含带扩展名形式、忽略大小写）。
            "con",
            "CON.txt",
            "NUL",
            "com1",
            "Lpt9.log",
        ];
        for bad in cases {
            assert!(validate_virtual_path(bad).is_err(), "应拒绝 {bad:?}");
        }
    }

    /// 正常名字不能被新版段级规则误伤（尤其是**段中间**的空格与点、以点开头的合法名）。
    #[test]
    fn validate_keeps_normal_names() {
        // 注：形如 `"a /b.txt"` 的路径第一段是 `"a "`，属**尾随空格**，按规则应拒（见上一条
        // 测试）——这里覆盖的是空格在段**中间**或段首的正常名。
        for ok in [
            "a/b.txt",
            "a b/c.txt",
            "a b.txt",
            "a.b",
            "b.txt",
            "..hidden",
            "a..b",
            " b.txt",
        ] {
            assert!(validate_virtual_path(ok).is_ok(), "不应误伤 {ok:?}");
        }
    }

    /// 纵深防御回归：段级规则也作用于 `canonicalize` 之后的**真实分量**。链接指向根内一个
    /// 名为 `a.`（尾随点）的真实目录时，语法层放行 `"link"`，但 `resolve` 的复检会拦下它。
    #[cfg(unix)]
    #[test]
    fn resolve_depth_check_rejects_nonconforming_canonical_component() {
        let temp = TempRoot::new("depth-check");
        let service = temp.service();
        std::fs::create_dir_all(temp.0.join("a.")).unwrap();
        std::os::unix::fs::symlink(temp.0.join("a."), temp.0.join("link")).unwrap();
        assert!(service.resolve("link").is_err(), "复检应拦下带尾随点的真实分量");
    }

    /// 共享根**自身**的路径分量不走段级规则：根目录恰好叫 `aux`/`backup.`/含 `:` 时，整个共享
    /// 目录不能被判死（段的规则是为**用户可控的虚拟路径**定的，根不是它的一部分）。
    ///
    /// 回归用例：复检曾在整条规范路径上跑，把自己的根前缀也套了进去 → `resolve("")` 直接失败，
    /// 于是 `list`、读写、上传全部挂掉。
    #[cfg(unix)]
    #[tokio::test]
    async fn share_root_with_nonconforming_own_segments_is_usable() {
        let temp = TempRoot::new("root-segments");
        // 根自身含两类会被段规则拒绝的分量：`aux.`（Windows 保留名 + 尾随点）、`a:b`（冒号）。
        let root = temp.0.join("aux.").join("a:b");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("f.txt"), b"hi").unwrap();
        let service = FileService::new(root);

        assert!(service.resolve("").is_ok(), "共享根自身的分量不应被段级规则拒绝");
        let entries = service.list("").await.expect("应能列出共享根");
        assert!(entries.iter().any(|e| e.name == "f.txt"), "根内条目应可见");

        // 根**之下**的虚拟路径分量仍受同一套规则约束（纵深防御没有被削弱）。
        assert!(service.resolve("con.txt").is_err(), "虚拟路径仍应拒绝保留名");
        assert!(service.resolve("c:evil").is_err(), "虚拟路径仍应拒绝冒号");
    }

    #[test]
    fn resolve_rejects_windows_normalization_tricks() {
        let temp = TempRoot::new("resolve-win");
        let service = temp.service();
        for bad in ["a/.. /.. /evil", "a/./.. ", "...", "a/", "x:y", "con", "CON.txt"] {
            assert!(service.resolve(bad).is_err(), "应拒绝 {bad:?}");
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

    /// 列表必须被预算截断，否则一条超大 `FileList` 帧会撞上 `MAX_SESSION_FRAME` 而把会话拆掉。
    /// 本测试用 200 字符的长名字把每条估算抬高，几千条即可越过 **字节预算**
    /// （而非条目数上限，那个要 10k 文件、太慢）。
    #[tokio::test]
    async fn list_truncates_to_payload_budget() {
        let temp = TempRoot::new("list-truncate");
        let service = temp.service();
        let total = 3_000usize;
        for i in 0..total {
            std::fs::write(temp.0.join(format!("{i:0>200}")), b"x").unwrap();
        }

        let entries = service.list("").await.unwrap();
        assert!(entries.len() < total, "超预算必须发生截断");
        assert!(entries.len() <= MAX_LIST_ENTRIES, "也不能越过条目数上限");

        // 截断后累计估算载荷必须落在预算内（这正是 warn! 路径也跑过了、没 panic 的证据）。
        let bytes: usize = entries
            .iter()
            .map(|e| LIST_ENTRY_FIXED_BYTES + e.name.len() + e.path.len())
            .sum();
        assert!(
            bytes <= MAX_LIST_PAYLOAD_BYTES,
            "截断后仍在预算内: {bytes} <= {MAX_LIST_PAYLOAD_BYTES}"
        );
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
