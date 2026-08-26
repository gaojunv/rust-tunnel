//! 链接图：由笔记与 wiki 链接推导的图结构，支持入边/孤儿/断链查询。

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;

use crate::link::{resolve_link, ResolvedLink, WikiLink};
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
    /// 以 `NoteKey` 为键建立 `nodes`，并对每条 `wikilinks` 用
    /// [`resolve_link`](crate::link::resolve_link) 做三级解析；仅
    /// [`Resolved`](crate::link::ResolvedLink::Resolved) 的结果会在
    /// [`DiGraph`] 中添加一条有向边。被引用但不在 `nodes` 中的目标不
    /// 创建结点也不建边（`petgraph` 要求结点索引有效）。
    #[must_use]
    pub fn new(notes: Vec<Note>) -> Self {
        let mut by_key: HashMap<NoteKey, Note> = HashMap::with_capacity(notes.len());
        for note in notes {
            by_key.insert(note.key.clone(), note);
        }
        let all_keys: Vec<NoteKey> = by_key.keys().cloned().collect();

        let mut edges: DiGraph<NoteKey, ()> = DiGraph::new();
        let mut index_map: HashMap<NoteKey, NodeIndex> = HashMap::with_capacity(by_key.len());
        for key in by_key.keys() {
            let idx = edges.add_node(key.clone());
            index_map.insert(key.clone(), idx);
        }

        for note in by_key.values() {
            let Some(src_idx) = index_map.get(&note.key).copied() else {
                continue;
            };
            for link in &note.wikilinks {
                match resolve_link(link, &all_keys) {
                    ResolvedLink::Resolved(dst) => {
                        if let Some(dst_idx) = index_map.get(&dst).copied() {
                            edges.add_edge(src_idx, dst_idx, ());
                        }
                    }
                    ResolvedLink::Ambiguous(_) | ResolvedLink::Broken(_) => {}
                }
            }
        }

        Self {
            nodes: by_key,
            edges,
        }
    }

    /// 返回 `key` 对应结点的图索引。
    ///
    /// 在 [`edges`](Self::edges) 中线性查找权重等于 `key` 的结点。
    #[must_use]
    pub fn node_index(&self, key: &NoteKey) -> Option<NodeIndex> {
        self.edges
            .node_indices()
            .find(|idx| self.edges.node_weight(*idx) == Some(key))
    }

    /// 指向 `key` 的全部入边笔记。
    ///
    /// 使用 [`Direction::Incoming`] 遍历 `edges` 的入边邻居。
    #[must_use]
    pub fn backlinks(&self, key: &NoteKey) -> Vec<&NoteKey> {
        let Some(idx) = self.node_index(key) else {
            return Vec::new();
        };
        self.edges
            .neighbors_directed(idx, Direction::Incoming)
            .filter_map(|nbr| self.edges.node_weight(nbr))
            .collect()
    }

    /// 无入边也无出边的孤立笔记。
    #[must_use]
    pub fn orphans(&self) -> Vec<&NoteKey> {
        self.edges
            .node_indices()
            .filter(|idx| {
                self.edges
                    .neighbors_directed(*idx, Direction::Incoming)
                    .next()
                    .is_none()
                    && self
                        .edges
                        .neighbors_directed(*idx, Direction::Outgoing)
                        .next()
                        .is_none()
            })
            .filter_map(|idx| self.edges.node_weight(idx))
            .collect()
    }

    /// 指向不存在笔记的断链。
    ///
    /// 遍历全部笔记的 `wikilinks` 并用 [`resolve_link`] 解析，结果为
    /// [`Broken`](crate::link::ResolvedLink::Broken) 或
    /// [`Ambiguous`](crate::link::ResolvedLink::Ambiguous) 的加入列表。
    #[must_use]
    pub fn broken_links(&self) -> Vec<(&NoteKey, &WikiLink)> {
        let all_keys: Vec<NoteKey> = self.nodes.keys().cloned().collect();
        let mut out = Vec::new();
        for note in self.nodes.values() {
            for link in &note.wikilinks {
                match resolve_link(link, &all_keys) {
                    ResolvedLink::Broken(_) | ResolvedLink::Ambiguous(_) => {
                        out.push((&note.key, link));
                    }
                    ResolvedLink::Resolved(_) => {}
                }
            }
        }
        out
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
