//! 链接图：由笔记与 wiki 链接推导的图结构，支持入边/孤儿/断链查询。
//!
//! 当前为骨架：`LinkGraph` 提供类型定义与可编译桩，真实建图在后续批次实现。

use std::collections::HashMap;

use petgraph::graph::DiGraph;

use crate::link::WikiLink;
use crate::note::{Note, NoteKey};

/// 整个 vault 的链接图。
#[derive(Debug, Clone)]
pub struct LinkGraph {
    /// 全部笔记，按 `NoteKey` 索引。
    pub nodes: HashMap<NoteKey, Note>,
    /// 有向边：`a → b` 表示 a 链接到 b。
    pub edges: DiGraph<NoteKey, ()>,
}

impl LinkGraph {
    /// 由笔记列表构建链接图。
    ///
    /// 当前为桩实现：返回空图，不读取 `notes`。
    #[must_use]
    pub fn new(_notes: Vec<Note>) -> Self {
        Self {
            nodes: HashMap::new(),
            edges: DiGraph::new(),
        }
    }

    /// 指向 `key` 的全部入边笔记。
    ///
    /// 当前为桩实现：恒返回空列表。
    #[must_use]
    pub fn backlinks(&self, _key: &NoteKey) -> Vec<&NoteKey> {
        Vec::new()
    }

    /// 无入边也无出边的孤立笔记。
    ///
    /// 当前为桩实现：恒返回空列表。
    #[must_use]
    pub fn orphans(&self) -> Vec<&NoteKey> {
        Vec::new()
    }

    /// 指向不存在笔记的断链。
    ///
    /// 当前为桩实现：恒返回空列表。
    #[must_use]
    pub fn broken_links(&self) -> Vec<(&NoteKey, &WikiLink)> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_graph_has_no_orphans_or_edges() {
        let graph = LinkGraph::new(Vec::new());
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.node_count() == 0);
        assert!(graph.orphans().is_empty());
        assert!(graph.backlinks(&NoteKey::new("a".to_owned())).is_empty());
        assert!(graph.broken_links().is_empty());
    }
}
