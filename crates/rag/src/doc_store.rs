//! 文档原文存储：落盘路径约定与旧目录迁移。
//!
//! 原文是 reindex 的真相源（二进制文档无法从分块/页面反推），故两个索引共享
//! 同一份文件。整合前向量侧存 `rag_docs/`、pages 侧存 `wiki_docs/`，同一容器
//! 开双索引时会要求用户把同一个文件传两遍；统一到 `knowledge_docs/` 后一次
//! 上传即可喂两个索引。

use std::path::{Path, PathBuf};

/// 统一原文目录名。
const DOCS_DIR: &str = "knowledge_docs";

/// 整合前的两个原文目录：向量侧、pages 侧。仅迁移用。
const LEGACY_DIRS: [&str; 2] = ["rag_docs", "wiki_docs"];

/// 文档原文落盘路径：`<data_dir>/knowledge_docs/<source_id>/<doc_id>.<ext>`。
///
/// 保留真实扩展名——二进制原文 reindex 时要按 `file_type` 重新解析。
#[must_use]
pub fn doc_source_path(data_dir: &Path, source_id: &str, doc_id: &str, ext: &str) -> PathBuf {
    data_dir
        .join(DOCS_DIR)
        .join(source_id)
        .join(format!("{doc_id}.{ext}"))
}

/// 容器的原文目录：`<data_dir>/knowledge_docs/<source_id>/`（删容器时整体清理）。
#[must_use]
pub fn source_docs_dir(data_dir: &Path, source_id: &str) -> PathBuf {
    data_dir.join(DOCS_DIR).join(source_id)
}

/// 迁移结果统计（仅供日志，调用方不据此分支）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MigrationReport {
    /// 成功搬迁的容器目录数。
    pub moved: usize,
    /// 目标已存在而跳过的容器目录数。
    pub skipped: usize,
    /// 搬迁失败的容器目录数（已记 warn，旧目录原样保留）。
    pub failed: usize,
}

/// 把 `rag_docs/*` 与 `wiki_docs/*` 下的容器目录搬到 `knowledge_docs/`。
///
/// 幂等：旧目录不存在即无操作；目标目录已存在则**跳过而不合并**——批 1 的 DB
/// 迁移把 kb 与 wiki 的 id 原样搬进 `knowledge_sources.id`（都是 uuid，正常不
/// 会撞），真撞上说明数据反常，静默合并会让两个容器的文档互相覆盖，宁可留在
/// 旧目录里等人来看日志。
///
/// 空的旧父目录不删：留着是无害的，而删除会掩盖"迁移到底跑过没有"的痕迹。
pub async fn migrate_legacy_doc_dirs(data_dir: &Path) -> MigrationReport {
    let mut report = MigrationReport::default();
    let target_root = data_dir.join(DOCS_DIR);

    for legacy in LEGACY_DIRS {
        let legacy_root = data_dir.join(legacy);
        let Ok(mut entries) = tokio::fs::read_dir(&legacy_root).await else {
            continue; // 旧目录不存在（新库或已迁完）
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let src = entry.path();
            if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                continue; // 只搬容器目录，旧根下的杂散文件不动
            }
            let Some(name) = src.file_name() else {
                continue;
            };
            let dst = target_root.join(name);
            if tokio::fs::metadata(&dst).await.is_ok() {
                tracing::warn!(
                    from = %src.display(), to = %dst.display(),
                    "knowledge docs migration: target exists, skipping (possible id collision)"
                );
                report.skipped += 1;
                continue;
            }
            if let Err(e) = tokio::fs::create_dir_all(&target_root).await {
                tracing::warn!(error = %e, dir = %target_root.display(), "knowledge docs migration: mkdir failed");
                report.failed += 1;
                continue;
            }
            match move_dir(&src, &dst).await {
                Ok(()) => {
                    tracing::info!(from = %src.display(), to = %dst.display(), "knowledge docs migrated");
                    report.moved += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, from = %src.display(), "knowledge docs migration failed; leaving source in place");
                    report.failed += 1;
                }
            }
        }
    }
    report
}

/// 搬一个目录：先试 rename，跨设备（`EXDEV`）时退化为逐文件复制 + 删源。
///
/// 复制路径只处理一层文件——原文目录的结构是扁平的 `<doc_id>.<ext>`，真出现
/// 子目录说明不是我们写的，留在原地比递归搬走安全。
async fn move_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    if tokio::fs::rename(src, dst).await.is_ok() {
        return Ok(());
    }
    tokio::fs::create_dir_all(dst).await?;
    let mut entries = tokio::fs::read_dir(src).await?;
    let mut copied_all = true;
    while let Some(entry) = entries.next_entry().await? {
        let from = entry.path();
        if !entry.file_type().await.is_ok_and(|t| t.is_file()) {
            copied_all = false;
            continue;
        }
        let Some(name) = from.file_name() else {
            continue;
        };
        tokio::fs::copy(&from, dst.join(name)).await?;
        tokio::fs::remove_file(&from).await?;
    }
    // 只有全部内容都搬走了才删源目录，否则留着让人能看到剩下什么。
    if copied_all {
        let _ = tokio::fs::remove_dir(src).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            tokio::fs::create_dir_all(p).await.unwrap();
        }
        tokio::fs::write(path, body).await.unwrap();
    }

    #[test]
    fn doc_source_path_layout() {
        let p = doc_source_path(Path::new("/data"), "src-1", "doc-1", "pdf");
        assert_eq!(p, Path::new("/data/knowledge_docs/src-1/doc-1.pdf"));
    }

    #[tokio::test]
    async fn migrates_both_legacy_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("rag_docs/kb-1/d1.md"), "vector side").await;
        write(&root.join("wiki_docs/wk-1/d2.md"), "pages side").await;

        let report = migrate_legacy_doc_dirs(root).await;

        assert_eq!(report.moved, 2);
        assert_eq!(report.skipped, 0);
        assert_eq!(
            tokio::fs::read_to_string(root.join("knowledge_docs/kb-1/d1.md"))
                .await
                .unwrap(),
            "vector side"
        );
        assert_eq!(
            tokio::fs::read_to_string(root.join("knowledge_docs/wk-1/d2.md"))
                .await
                .unwrap(),
            "pages side"
        );
        assert!(!root.join("rag_docs/kb-1").exists());
    }

    #[tokio::test]
    async fn is_idempotent_and_noop_without_legacy_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("rag_docs/kb-1/d1.md"), "x").await;

        assert_eq!(migrate_legacy_doc_dirs(root).await.moved, 1);
        // 二次运行：旧目录已空/已删，不该再搬也不该报错。
        let second = migrate_legacy_doc_dirs(root).await;
        assert_eq!(second, MigrationReport::default());
        assert!(root.join("knowledge_docs/kb-1/d1.md").exists());
    }

    #[tokio::test]
    async fn id_collision_skips_without_merging() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("rag_docs/dup/old.md"), "legacy").await;
        write(&root.join("knowledge_docs/dup/new.md"), "already here").await;

        let report = migrate_legacy_doc_dirs(root).await;

        assert_eq!(report.skipped, 1);
        assert_eq!(report.moved, 0);
        // 关键：两边都完好，没有互相覆盖也没有合并。
        assert!(root.join("rag_docs/dup/old.md").exists());
        assert!(!root.join("knowledge_docs/dup/old.md").exists());
        assert!(root.join("knowledge_docs/dup/new.md").exists());
    }

    #[tokio::test]
    async fn ignores_stray_files_in_legacy_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("rag_docs/stray.txt"), "not a container dir").await;

        assert_eq!(migrate_legacy_doc_dirs(root).await, MigrationReport::default());
        assert!(root.join("rag_docs/stray.txt").exists());
    }
}
