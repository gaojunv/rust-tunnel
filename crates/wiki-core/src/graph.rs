//! 链接图：由笔记与 wiki 链接推导的图结构，支持入边/孤儿/断链查询。

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;

use crate::link::{LinkIndex, ResolvedLink, WikiLink};
use crate::note::{Note, NoteKey};

/// 整个 vault 的链接图。
#[derive(Debug, Clone)]
pub struct LinkGraph {
    /// 全部笔记，按 `NoteKey` 索引。
    pub nodes: HashMap<NoteKey, Note>,
    /// 有向边：`a → b` 表示 a 链接到 b。
    pub edges: DiGraph<NoteKey, ()>,
    /// `NoteKey` → 图结点索引（建图时一次填入，避免线性扫描）。
    index_map: HashMap<NoteKey, NodeIndex>,
    /// 入边邻接表：`key` → 指向它的全部笔记键（建图时一次填入）。
    incoming: HashMap<NoteKey, Vec<NoteKey>>,
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
        // 预建查表：每条链接的解析从 O(notes) 线性扫描降为 O(1) 查表。
        let link_index = LinkIndex::new(&all_keys);

        let mut edges: DiGraph<NoteKey, ()> = DiGraph::new();
        let mut index_map: HashMap<NoteKey, NodeIndex> = HashMap::with_capacity(by_key.len());
        for key in by_key.keys() {
            let idx = edges.add_node(key.clone());
            index_map.insert(key.clone(), idx);
        }

        let mut incoming: HashMap<NoteKey, Vec<NoteKey>> =
            HashMap::with_capacity(by_key.len());
        for note in by_key.values() {
            let Some(src_idx) = index_map.get(&note.key).copied() else {
                continue;
            };
            for link in &note.wikilinks {
                match link_index.resolve(link) {
                    ResolvedLink::Resolved(dst) => {
                        if let Some(dst_idx) = index_map.get(&dst).copied() {
                            edges.add_edge(src_idx, dst_idx, ());
                            incoming.entry(dst).or_default().push(note.key.clone());
                        }
                    }
                    ResolvedLink::Ambiguous(_) | ResolvedLink::Broken(_) => {}
                }
            }
        }

        Self {
            nodes: by_key,
            edges,
            index_map,
            incoming,
        }
    }

    /// 返回 `key` 对应结点的图索引。
    ///
    /// 查建图时填入的 `index_map`（O(1)），不再线性扫描全部结点。
    #[must_use]
    pub fn node_index(&self, key: &NoteKey) -> Option<NodeIndex> {
        self.index_map.get(key).copied()
    }

    /// 指向 `key` 的全部入边笔记。
    ///
    /// 查建图时填入的入边邻接表（O(1)），不再遍历图边。
    #[must_use]
    pub fn backlinks(&self, key: &NoteKey) -> Vec<&NoteKey> {
        self.incoming
            .get(key)
            .map_or_else(Vec::new, |v| v.iter().collect())
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
        let link_index = LinkIndex::new(&all_keys);
        let mut out = Vec::new();
        for note in self.nodes.values() {
            for link in &note.wikilinks {
                match link_index.resolve(link) {
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
        assert_eq!(graph.edges.node_count(), 0);
        assert!(graph.orphans().is_empty());
        assert!(graph.backlinks(&NoteKey::new("a".to_owned())).is_empty());
        assert!(graph.broken_links().is_empty());
    }

    fn test_note(key: &str, links: &[&str]) -> Note {
        Note {
            key: NoteKey::new(key.to_owned()),
            ref_id: None,
            title: key.to_owned(),
            aliases: Vec::new(),
            tags: Vec::new(),
            body: String::new(),
            wikilinks: links
                .iter()
                .map(|t| crate::link::WikiLink {
                    target: (*t).to_owned(),
                    alias: None,
                    anchor: None,
                    embed: false,
                })
                .collect(),
            frontmatter: crate::frontmatter::FrontMatter::default(),
            modified: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn backlinks_use_prebuilt_adjacency() {
        let graph = LinkGraph::new(vec![
            test_note("a", &["b", "c"]),
            test_note("b", &[]),
            test_note("c", &["b"]),
        ]);
        let mut bl: Vec<&str> = graph
            .backlinks(&NoteKey::new("b".to_owned()))
            .iter()
            .map(|k| k.as_str())
            .collect();
        bl.sort_unstable();
        assert_eq!(bl, vec!["a", "c"]);
        assert!(graph.backlinks(&NoteKey::new("a".to_owned())).is_empty());
        assert!(graph.backlinks(&NoteKey::new("missing".to_owned())).is_empty());
    }

    #[test]
    fn node_index_hits_prebuilt_map() {
        let graph = LinkGraph::new(vec![test_note("a", &[]), test_note("b", &[])]);
        assert!(graph.node_index(&NoteKey::new("a".to_owned())).is_some());
        assert!(graph.node_index(&NoteKey::new("b".to_owned())).is_some());
        assert!(graph.node_index(&NoteKey::new("missing".to_owned())).is_none());
    }

    #[test]
    fn orphans_and_broken_links_still_work() {
        let graph = LinkGraph::new(vec![
            test_note("a", &["b"]),
            test_note("b", &[]),
            test_note("lone", &[]),
            test_note("c", &["missing", "b"]),
        ]);
        let mut orphans: Vec<&str> = graph
            .orphans()
            .iter()
            .map(|k| k.as_str())
            .collect();
        orphans.sort_unstable();
        assert_eq!(orphans, vec!["lone"]);
        let broken: Vec<(&str, &str)> = graph
            .broken_links()
            .iter()
            .map(|(k, l)| (k.as_str(), l.target.as_str()))
            .collect();
        assert!(broken.contains(&("c", "missing")));
        assert!(!broken.iter().any(|(_, t)| *t == "b"));
    }
}
