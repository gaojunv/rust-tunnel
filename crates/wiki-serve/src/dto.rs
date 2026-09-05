// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! IPC 数据传输对象（字段名与前端约定，不得更改）。

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use rust_tunnel_wiki_core::note::Note;
#[cfg(feature = "search")]
use rust_tunnel_wiki_core::search::SearchHit;

/// 将 `SystemTime` 转为 unix 秒，失败回退为 0。
fn system_time_to_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// 笔记摘要（列表用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteSummary {
    /// vault 内相对路径键（去扩展名，`/` 分隔）。
    pub key: String,
    /// 显示标题。
    pub title: String,
    /// 标签列表。
    pub tags: Vec<String>,
    /// 最后修改时间（unix 秒）。
    pub modified: u64,
    /// 关联的远端 `ref`（可选，前端按 `string | null` 接收）。
    #[serde(default)]
    pub ref_id: Option<String>,
}

impl From<&Note> for NoteSummary {
    fn from(n: &Note) -> Self {
        Self {
            key: n.key.as_str().to_owned(),
            title: n.title.clone(),
            tags: n.tags.clone(),
            modified: system_time_to_secs(n.modified),
            ref_id: n.ref_id.as_ref().map(|r| r.as_str().to_owned()),
        }
    }
}

/// 单篇笔记详情。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteDto {
    /// vault 内相对路径键。
    pub key: String,
    /// 显示标题。
    pub title: String,
    /// 别名列表。
    pub aliases: Vec<String>,
    /// 标签列表。
    pub tags: Vec<String>,
    /// 正文（已剥离 frontmatter）。
    pub body: String,
    /// 最后修改时间（unix 秒）。
    pub modified: u64,
    /// 关联的远端 `ref`（可选，前端按 `string | null` 接收）。
    #[serde(default)]
    pub ref_id: Option<String>,
}

impl From<&Note> for NoteDto {
    fn from(n: &Note) -> Self {
        Self {
            key: n.key.as_str().to_owned(),
            title: n.title.clone(),
            aliases: n.aliases.clone(),
            tags: n.tags.clone(),
            body: n.body.clone(),
            modified: system_time_to_secs(n.modified),
            ref_id: n.ref_id.as_ref().map(|r| r.as_str().to_owned()),
        }
    }
}

/// 搜索命中。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHitDto {
    /// 命中的笔记键。
    pub note_key: String,
    /// 命中笔记标题。
    pub title: String,
    /// 命中片段。
    pub snippet: String,
    /// 相关度分数。
    pub score: f64,
}

#[cfg(feature = "search")]
impl From<&SearchHit> for SearchHitDto {
    fn from(h: &SearchHit) -> Self {
        Self {
            note_key: h.note_key.as_str().to_owned(),
            title: h.title.clone(),
            snippet: h.snippet.clone(),
            score: h.score,
        }
    }
}

/// 图节点。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphNode {
    /// 笔记键。
    pub key: String,
    /// 显示标题。
    pub title: String,
}

/// 图边（有向）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphEdge {
    /// 起点笔记键。
    pub from: String,
    /// 终点笔记键。
    pub to: String,
}

/// 链接图 DTO。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphDto {
    /// 全部节点。
    pub nodes: Vec<GraphNode>,
    /// 全部有向边（已排序去重）。
    pub edges: Vec<GraphEdge>,
}

/// Vault 信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultInfo {
    /// vault 根目录字符串。
    pub root: String,
    /// 笔记数量。
    pub note_count: usize,
}

/// 单条重命名移动记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MovedEntry {
    /// 原 key。
    pub from_key: String,
    /// 新 key。
    pub to_key: String,
}

/// 单条失败记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailedEntry {
    /// 失败对应的 key（待移动或待删除）。
    pub key: String,
    /// 错误描述。
    pub error: String,
}

/// 文件夹重命名结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenameFolderResult {
    /// 成功移动的条目。
    pub moved: Vec<MovedEntry>,
    /// 失败的条目。
    pub failed: Vec<FailedEntry>,
    /// 发生链接重写的笔记 key 列表。
    pub link_rewritten: Vec<String>,
    /// 链接重写总次数。
    pub rewritten_count: usize,
}

/// 文件夹删除结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeleteFolderResult {
    /// 已删除的笔记 key 列表。
    pub deleted: Vec<String>,
    /// 失败的条目。
    pub failed: Vec<FailedEntry>,
}
