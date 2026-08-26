//! 基于 tantivy 的全文检索索引。

use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, TantivyDocument, Value, STORED, STRING, TEXT};
use tantivy::{Index, IndexReader};
use thiserror::Error;

use crate::note::{Note, NoteKey};

/// 全文检索错误。
#[derive(Debug, Error)]
pub enum SearchError {
    /// tantivy 底层错误。
    #[error("tantivy 错误：{0}")]
    Tantivy(#[from] tantivy::TantivyError),
    /// IO 错误。
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
}

/// 可持久化的全文索引。
pub struct SearchIndex {
    /// tantivy 索引句柄。
    pub index: Index,
    /// 索引读取器（并发搜索入口）。
    pub reader: IndexReader,
}

/// 一条搜索结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// 命中的笔记键。
    pub note_key: NoteKey,
    /// 命中笔记的标题。
    pub title: String,
    /// 命中片段。
    pub snippet: String,
    /// 相关度分数。
    pub score: f64,
}

/// 构建本 crate 约定的 [`Schema`]。
fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("key", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("aliases", STRING | STORED);
    builder.add_text_field("tags", STRING | STORED);
    builder.add_text_field("body", TEXT | STORED);
    builder.build()
}

/// 从 `body` 截取前 200 字符作为 `snippet`。
fn snippet_from_body(body: &str) -> String {
    if body.chars().count() <= 200 {
        body.to_owned()
    } else {
        body.chars().take(200).collect()
    }
}

impl SearchIndex {
    /// 打开或创建位于 `path` 的索引。
    ///
    /// 若目录不存在则创建；若目录内已存在索引则直接打开，否则按约定 [`Schema`]
    /// 创建新索引，并初始化 [`IndexReader`]。
    ///
    /// # Errors
    ///
    /// 目录创建失败或 tantivy 打开/创建/创建 reader 失败时返回 [`SearchError`]。
    #[must_use = "返回值需处理错误"]
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SearchError> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let schema = build_schema();
        let index = match Index::open_in_dir(path) {
            Ok(idx) => idx,
            Err(_) => Index::create_in_dir(path, schema)?,
        };
        let reader = index.reader()?;
        Ok(Self { index, reader })
    }

    /// 将笔记写入索引。
    ///
    /// 为 [`Note`] 的 `key/title/aliases/tags/body` 构造 [`TantivyDocument`]，
    /// 以 `NUM_THREADS=1` 的 writer 写入、提交并刷新 reader。
    ///
    /// # Errors
    ///
    /// 索引写入、提交或 reader 刷新失败时返回 [`SearchError`]。
    pub fn add_note(&mut self, note: &Note) -> Result<(), SearchError> {
        let schema = self.index.schema();
        let key_field = schema.get_field("key")?;
        let title_field = schema.get_field("title")?;
        let aliases_field = schema.get_field("aliases")?;
        let tags_field = schema.get_field("tags")?;
        let body_field = schema.get_field("body")?;

        let mut doc = TantivyDocument::default();
        doc.add_text(key_field, note.key.as_str());
        doc.add_text(title_field, &note.title);
        for alias in &note.aliases {
            doc.add_text(aliases_field, alias);
        }
        for tag in &note.tags {
            doc.add_text(tags_field, tag);
        }
        doc.add_text(body_field, &note.body);

        let mut writer = self.index.writer_with_num_threads(1, 15_000_000)?;
        writer.add_document(doc)?;
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// 按 `query` 检索，返回前 `limit` 条命中。
    ///
    /// 在 `title/body/aliases/tags` 四字段上解析查询，返回按相关度排序的
    /// [`SearchHit`] 列表。`snippet` 取 `body` 前 200 字符。
    ///
    /// # Errors
    ///
    /// 查询解析或检索失败时返回 [`SearchError`]。
    #[must_use = "返回值需处理错误"]
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let schema = self.index.schema();
        let key_field = schema.get_field("key")?;
        let title_field = schema.get_field("title")?;
        let aliases_field = schema.get_field("aliases")?;
        let tags_field = schema.get_field("tags")?;
        let body_field = schema.get_field("body")?;

        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(
            &self.index,
            vec![title_field, body_field, aliases_field, tags_field],
        );
        let parsed = parser
            .parse_query(trimmed)
            .map_err(tantivy::TantivyError::from)?;
        let top_docs = searcher.search(&parsed, &TopDocs::with_limit(limit).order_by_score())?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_address)?;
            let note_key = doc
                .get_first(key_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let title = doc
                .get_first(title_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let body = doc
                .get_first(body_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            hits.push(SearchHit {
                note_key: NoteKey::new(note_key.to_owned()),
                title: title.to_owned(),
                snippet: snippet_from_body(body),
                score: f64::from(score),
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use tempfile::tempdir;

    use super::*;
    use crate::frontmatter::FrontMatter;
    use crate::note::Note;

    fn make_note(key: &str, title: &str, body: &str) -> Note {
        Note {
            key: NoteKey::new(key.to_owned()),
            ref_id: None,
            title: title.to_owned(),
            aliases: Vec::new(),
            tags: Vec::new(),
            body: body.to_owned(),
            wikilinks: Vec::new(),
            frontmatter: FrontMatter::default(),
            modified: SystemTime::now(),
        }
    }

    #[test]
    fn open_creates_index_and_search_empty() {
        let dir = tempdir().unwrap();
        let idx = SearchIndex::open(dir.path()).unwrap();
        let hits = idx.search("anything", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_single_note_by_title() {
        let dir = tempdir().unwrap();
        let mut idx = SearchIndex::open(dir.path()).unwrap();
        let note = make_note("a/b", "hello world", "some body text");
        idx.add_note(&note).unwrap();
        let hits = idx.search("hello", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].note_key.as_str(), "a/b");
        assert_eq!(hits[0].title, "hello world");
        assert!(!hits[0].snippet.is_empty());
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn search_multiple_notes_by_body() {
        let dir = tempdir().unwrap();
        let mut idx = SearchIndex::open(dir.path()).unwrap();
        let n1 = make_note("n1", "first", "rust is great for systems");
        let n2 = make_note("n2", "second", "python is great for scripting");
        let n3 = make_note("n3", "third", "rust and tantivy search engine");
        idx.add_note(&n1).unwrap();
        idx.add_note(&n2).unwrap();
        idx.add_note(&n3).unwrap();
        let hits = idx.search("rust", 10).unwrap();
        assert_eq!(hits.len(), 2);
        let keys: Vec<&str> = hits.iter().map(|h| h.note_key.as_str()).collect();
        assert!(keys.contains(&"n1"));
        assert!(keys.contains(&"n3"));
    }

    #[test]
    fn search_nonexistent_keyword_returns_empty() {
        let dir = tempdir().unwrap();
        let mut idx = SearchIndex::open(dir.path()).unwrap();
        let note = make_note("k1", "title", "hello world body");
        idx.add_note(&note).unwrap();
        let hits = idx.search("qwertyuiop_nonexistent", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_empty_index_returns_empty() {
        let dir = tempdir().unwrap();
        let idx = SearchIndex::open(dir.path()).unwrap();
        let hits = idx.search("hello", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_error_display() {
        let io_err = SearchError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing file",
        ));
        let msg = io_err.to_string();
        assert!(msg.contains("missing file"));

        let tantivy_err = SearchError::Tantivy(tantivy::TantivyError::InvalidArgument(
            "bad query".to_owned(),
        ));
        let msg2 = tantivy_err.to_string();
        assert!(msg2.contains("bad query"));
    }

    #[test]
    fn snippet_is_truncated_to_200_chars() {
        let dir = tempdir().unwrap();
        let mut idx = SearchIndex::open(dir.path()).unwrap();
        let long_body = "a".repeat(500);
        let note = make_note("k", "title", &long_body);
        idx.add_note(&note).unwrap();
        let hits = idx.search("title", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet.chars().count(), 200);
    }

    #[test]
    fn search_limit_is_respected() {
        let dir = tempdir().unwrap();
        let mut idx = SearchIndex::open(dir.path()).unwrap();
        for i in 0..5 {
            let n = make_note(&format!("k{i}"), "common", "common body text");
            idx.add_note(&n).unwrap();
        }
        let hits = idx.search("common", 2).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn aliases_and_tags_are_indexed() {
        let dir = tempdir().unwrap();
        let mut idx = SearchIndex::open(dir.path()).unwrap();
        let mut note = make_note("k", "title", "body");
        note.aliases = vec!["myalias".to_owned()];
        note.tags = vec!["mytag".to_owned()];
        idx.add_note(&note).unwrap();
        let hits_alias = idx.search("myalias", 10).unwrap();
        assert_eq!(hits_alias.len(), 1);
        let hits_tag = idx.search("mytag", 10).unwrap();
        assert_eq!(hits_tag.len(), 1);
    }

    #[test]
    fn reopen_persists_index() {
        let dir = tempdir().unwrap();
        {
            let mut idx = SearchIndex::open(dir.path()).unwrap();
            let note = make_note("k", "hello", "world");
            idx.add_note(&note).unwrap();
        }
        let idx2 = SearchIndex::open(dir.path()).unwrap();
        let hits = idx2.search("hello", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
