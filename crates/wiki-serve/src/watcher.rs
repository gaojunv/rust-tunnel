// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

/*!
Vault 目录外部变更监听（debounce 聚合 + 过滤）。

本模块是纯逻辑层，完全不依赖 `tauri`：`notify` 的原始事件先经
[`should_track_path`] 过滤、[`note_key_from_rel`] 转 key，再由 [`ChangeBatcher`]
按时间窗口聚合去重。Tauri 装配（`RecommendedWatcher` 创建、`emit` 广播、
生命周期管理）位于 `tauri_app` 的 `watcher_runtime` 部分，本模块仅提供
可单测的纯函数与批量器。

过滤规则与 [`crate::vault_ops`] / `wiki-core` 的 vault 扫描保持一致：
- 隐藏文件/目录（任意路径段以 `.` 开头，如 `.obsidian/`、`.wiki-sync.json`）
- `VaultScanner::default_ignore_patterns` 命中的段（`.obsidian`/`.git`/`.trash`/`node_modules`）
- 非 `.md`/`.markdown` 文件（大小写不敏感，如附件图片、临时文件）
- 编辑器临时文件（`~` 后缀、`~` 前缀、`.swp`/`.tmp` 等）
*/

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Debounce 聚合窗口：窗口内的重复事件合并为一次广播。
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);

/// Tauri 事件名：vault 文件系统变更（与前端 `onVaultFsChanged` 对接）。
pub const VAULT_FS_CHANGED_EVENT: &str = "vault:fs-changed";

/// vault 扫描默认忽略的路径段（与 `wiki-core` 的 `VaultScanner` 一致）。
const IGNORED_SEGMENTS: &[&str] = &[".obsidian", ".git", ".trash", "node_modules"];

/// 判断 vault 相对路径是否应被监听（命中返回 `true`）。
///
/// `rel` 为相对 vault 根的路径（`/` 或 `\` 分隔均可）。凡命中以下任一即忽略：
/// - 任一路径段以 `.` 开头（隐藏文件/目录）
/// - 任一路径段命中 [`IGNORED_SEGMENTS`]
/// - 文件扩展名非 `.md`/`.markdown`（大小写不敏感）
/// - 文件名为编辑器临时文件（`~` 前缀/后缀、`.swp`/`.swx`/`.tmp` 后缀、`.bak` 后缀）
#[must_use]
pub fn should_track_path(rel: &str) -> bool {
    let normalized = rel.replace('\\', "/");
    if normalized.is_empty() {
        return false;
    }
    let mut segments = normalized.split('/');
    let Some(file_name) = segments.next_back() else {
        return false;
    };
    if file_name.is_empty() {
        return false;
    }
    // 路径段检查：中间目录 + 文件名统一做隐藏/忽略判断
    for seg in normalized.split('/') {
        if seg.is_empty() {
            return false;
        }
        if seg.starts_with('.') {
            return false;
        }
        if IGNORED_SEGMENTS.contains(&seg) {
            return false;
        }
    }
    if is_temp_file_name(file_name) {
        return false;
    }
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown")
}

/// 判断文件名是否为编辑器/同步工具的临时文件。
///
/// 扩展名比较一律走 `Path::extension` + `eq_ignore_ascii_case`（大小写不敏感，
/// 且满足 `clippy::case_sensitive_file_extension_comparisons`）。
fn is_temp_file_name(file_name: &str) -> bool {
    if file_name.starts_with('~') || file_name.ends_with('~') {
        return true;
    }
    if file_name.starts_with(".~") {
        return true;
    }
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    ext.eq_ignore_ascii_case("swp")
        || ext.eq_ignore_ascii_case("swx")
        || ext.eq_ignore_ascii_case("tmp")
        || ext.eq_ignore_ascii_case("bak")
        || ext.eq_ignore_ascii_case("orig")
}

/// 将 vault 相对路径转为前端可用的 note key（`/` 分隔、无扩展名）。
///
/// 复用 [`rust_tunnel_wiki_core::note::note_key_from_path`] 的扩展名规则，
/// 因此 `.md`/`.markdown`（大小写不敏感）均可转换；调用前应先经
/// [`should_track_path`] 过滤。`vault_root` 仅用于借 `strip_prefix`，传
/// `Path::new("")` 亦可——本函数自行拼接后再 strip。
#[must_use]
pub fn note_key_from_rel(vault_root: &Path, rel: &str) -> Option<String> {
    let normalized = rel.replace('\\', "/");
    let abs = vault_root.join(&normalized);
    rust_tunnel_wiki_core::note::note_key_from_path(vault_root, &abs)
        .map(|k| k.as_str().to_owned())
}

/// 将 `notify` 原始事件路径转为 vault 相对路径字符串（`/` 分隔）。
///
/// 失败（路径不在 `vault_root` 之下）返回 `None`。
#[must_use]
pub fn rel_of(vault_root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(vault_root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Debounce 聚合器：窗口内的路径事件合并去重，按窗口到期批量吐出。
///
/// 用法：`notify` 回调中调 [`ChangeBatcher::push`]（记录路径+时间）；
/// 定时轮询 [`ChangeBatcher::take_due`] 拿走已过 [`DEBOUNCE_WINDOW`] 的批次。
#[derive(Debug, Default)]
pub struct ChangeBatcher {
    pending: HashMap<PathBuf, Instant>,
}

impl ChangeBatcher {
    /// 新建空聚合器。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次路径事件（同路径重复到达则刷新时间为最新）。
    pub fn push(&mut self, path: PathBuf, now: Instant) {
        self.pending.insert(path, now);
    }

    /// 取出全部已到期（`now - pushed_at >= window`）的路径（排序保证确定性）。
    pub fn take_due(&mut self, now: Instant, window: Duration) -> Vec<PathBuf> {
        let mut due: Vec<PathBuf> = Vec::new();
        self.pending.retain(|path, pushed_at| {
            if now.duration_since(*pushed_at) >= window {
                due.push(path.clone());
                false
            } else {
                true
            }
        });
        due.sort();
        due
    }

    /// 当前挂起的路径数（测试/诊断用）。
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 是否无挂起事件。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// 对一批 vault 相对路径做「过滤 → 转 key → 去重排序」，得到广播 payload。
///
/// `vault_root` 用于 key 推导；返回的 key 列表按字母序排序、去重。
#[must_use]
pub fn collect_changed_keys(vault_root: &Path, rels: &[String]) -> Vec<String> {
    let mut keys: HashSet<String> = HashSet::new();
    for rel in rels {
        if !should_track_path(rel) {
            continue;
        }
        if let Some(key) = note_key_from_rel(vault_root, rel) {
            keys.insert(key);
        }
    }
    let mut out: Vec<String> = keys.into_iter().collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_accepts_notes_case_insensitive_ext() {
        assert!(should_track_path("hello.md"));
        assert!(should_track_path("docs/a/b.markdown"));
        assert!(should_track_path("UPPER.MD"));
        assert!(should_track_path("mixed.MarkDown"));
    }

    #[test]
    fn track_rejects_hidden_and_ignored_segments() {
        for bad in [
            ".obsidian/app.json",
            ".git/HEAD",
            ".trash/old.md",
            "node_modules/pkg/x.md",
            "docs/.git/y.md",
            ".wiki-sync.json",
            ".hidden.md",
            "docs/.hidden/note.md",
            ".DS_Store",
        ] {
            assert!(!should_track_path(bad), "应忽略 {bad:?}");
        }
    }

    #[test]
    fn track_rejects_non_markdown_and_temp_files() {
        for bad in [
            "assets/docs-a/20260905-120000-ab12cd34.png",
            "note.txt",
            "image.jpg",
            "noext",
            "note.md~",
            "~note.md",
            ".~lock.note.md#",
            "note.swp",
            "note.tmp",
            "note.bak",
            "note.md.tmp",
            "folder/note.swx",
        ] {
            assert!(!should_track_path(bad), "应忽略 {bad:?}");
        }
    }

    #[test]
    fn track_rejects_empty_and_dot_segments() {
        assert!(!should_track_path(""));
        assert!(!should_track_path("a//b.md"));
        assert!(!should_track_path("a/./b.md"));
    }

    #[test]
    fn track_handles_windows_separator() {
        assert!(should_track_path(r"docs\note.md"));
        assert!(!should_track_path(r".obsidian\app.json"));
    }

    #[test]
    fn key_from_rel_strips_extension() {
        let root = Path::new("/vault");
        assert_eq!(
            note_key_from_rel(root, "docs/note.md").as_deref(),
            Some("docs/note")
        );
        assert_eq!(
            note_key_from_rel(root, "hello.markdown").as_deref(),
            Some("hello")
        );
        assert_eq!(
            note_key_from_rel(root, "UPPER.MD").as_deref(),
            Some("UPPER")
        );
        assert_eq!(note_key_from_rel(root, "a.png"), None);
    }

    #[test]
    fn rel_of_joins_with_slash() {
        let root = Path::new("/vault");
        assert_eq!(
            rel_of(root, Path::new("/vault/docs/a.md")).as_deref(),
            Some("docs/a.md")
        );
        // root 之外返回 None
        assert_eq!(rel_of(root, Path::new("/other/a.md")), None);
        // root 本身返回 None（无相对路径）
        assert_eq!(rel_of(root, Path::new("/vault")), None);
    }

    #[test]
    fn batcher_merges_bursts_into_one_batch() {
        let mut b = ChangeBatcher::new();
        let t0 = Instant::now();
        b.push(PathBuf::from("a.md"), t0);
        b.push(PathBuf::from("b.md"), t0 + Duration::from_millis(50));
        // 同路径重复到达：刷新时间，不产生重复条目
        b.push(PathBuf::from("a.md"), t0 + Duration::from_millis(100));
        assert_eq!(b.pending_len(), 2);
        // 窗口未到期：取不出
        assert!(b.take_due(t0 + Duration::from_millis(200), DEBOUNCE_WINDOW).is_empty());
        // 窗口到期：一次吐出全部（排序）
        let due = b.take_due(t0 + Duration::from_millis(500), DEBOUNCE_WINDOW);
        assert_eq!(due, vec![PathBuf::from("a.md"), PathBuf::from("b.md")]);
        assert!(b.is_empty());
    }

    #[test]
    fn batcher_partial_due_keeps_fresh() {
        let mut b = ChangeBatcher::new();
        let t0 = Instant::now();
        b.push(PathBuf::from("old.md"), t0);
        b.push(PathBuf::from("new.md"), t0 + Duration::from_millis(400));
        let due = b.take_due(t0 + Duration::from_millis(350), DEBOUNCE_WINDOW);
        assert_eq!(due, vec![PathBuf::from("old.md")]);
        assert_eq!(b.pending_len(), 1);
    }

    #[test]
    fn collect_changed_keys_filters_dedups_sorts() {
        let root = Path::new("/vault");
        let rels = vec![
            "b.md".to_owned(),
            "a.md".to_owned(),
            "a.md".to_owned(),
            ".obsidian/x.md".to_owned(),
            "img.png".to_owned(),
            "sub/c.markdown".to_owned(),
        ];
        assert_eq!(collect_changed_keys(root, &rels), vec!["a", "b", "sub/c"]);
    }

    /// 端到端胶水：真实文件写入 → 模拟 `notify` 回调 `push` → 到期取出 →
    /// `rel_of` → `collect_changed_keys`，验证整条管线不依赖 Tauri 即可产出去重 key。
    #[test]
    fn pipeline_real_files_to_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("a.md"), "hello").expect("write a");
        std::fs::create_dir_all(root.join("docs")).expect("mkdir");
        std::fs::write(root.join("docs").join("b.markdown"), "world").expect("write b");
        // 不应进入 payload 的文件
        std::fs::write(root.join("img.png"), "bin").expect("write png");

        // 模拟 notify 回调：把事件路径推入 batcher
        let mut batcher = ChangeBatcher::new();
        let t0 = Instant::now();
        for name in ["a.md", "docs/b.markdown", "img.png"] {
            batcher.push(root.join(name), t0);
        }
        // window=0 立即到期（不睡 300ms，避免测试变慢/抖动）
        let due = batcher.take_due(t0, Duration::ZERO);
        assert_eq!(due.len(), 3);
        let rels: Vec<String> = due.iter().filter_map(|p| rel_of(root, p)).collect();
        assert_eq!(collect_changed_keys(root, &rels), vec!["a", "docs/b"]);
    }

    #[test]
    fn collect_changed_keys_empty() {
        let root = Path::new("/vault");
        let empty: Vec<String> = Vec::new();
        assert!(collect_changed_keys(root, &empty).is_empty());
        assert!(collect_changed_keys(root, &["x.png".to_owned()]).is_empty());
    }
}
