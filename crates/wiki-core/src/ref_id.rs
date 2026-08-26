//! Wiki `ref` 标识：与 server 端 `normalize_wiki_ref` 严格同规的规范化与校验。
//!
//! 规范来源为 `crates/persistence/src/wiki.rs` 的 `normalize_wiki_ref`：
//! `^[a-z0-9][a-z0-9/_-]{0,127}$`，`trim` + `lowercase`，禁 `//`、`./`、`../`，
//! 字节长度 ≤128。本模块**不复用**该函数（复用需拖入整个 sqlx DB 层），而是
//! 以本文件末尾的契约测试表逐条锁住规则——server 侧规则若漂移，测试即红。

use std::fmt;

use serde::{Deserialize, Serialize};

/// `ref` 最大字节长度（对齐 server 的 `s.len() > 128` 判定）。
pub const MAX_REF_LEN: usize = 128;

/// 一个已通过校验的 wiki `ref`。
///
/// 内部始终保存规范化后的形式（已 `trim` + `lowercase`），因此可直接用作
/// server API 的路径参数与本地 SQLite 主键。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RefId(String);

impl RefId {
    /// 规范化并校验 `raw`，非法返回 `None`。
    ///
    /// 与 server `normalize_wiki_ref` 的判定顺序保持一致：空/超长 → 路径穿越
    /// → 首字符类 → 字符集。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim().to_lowercase();
        if s.is_empty() || s.len() > MAX_REF_LEN {
            return None;
        }
        if s.contains("//") || s.contains("./") || s.contains("../") {
            return None;
        }
        let first = s.as_bytes().first().copied()?;
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return None;
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '/' || c == '_' || c == '-')
        {
            return None;
        }
        Some(Self(s))
    }

    /// 从 vault 内的相对路径推导 `ref`（去 `.md`/`.markdown` 扩展、`\` 归一为 `/`）。
    ///
    /// 路径含中文等非法字符时返回 `None`——此时该笔记仅本地可用、不绑定 remote，
    /// 需在 frontmatter 里显式写 `ref:` 才能同步到 server。
    #[must_use]
    pub fn from_relative_path(rel: &str) -> Option<Self> {
        let normalized = rel.replace('\\', "/");
        let trimmed = normalized
            .strip_suffix(".md")
            .or_else(|| normalized.strip_suffix(".markdown"))
            .unwrap_or(normalized.as_str());
        Self::parse(trimmed)
    }

    /// 规范化后的字符串形式。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 取出内部 `String`。
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    /// 按 `/` 切分的层级片段（`ref` 用 `/` 模拟目录层级，server 侧无 `parent_id` 列）。
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// 父级 `ref`；顶层返回 `None`。
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let idx = self.0.rfind('/')?;
        Self::parse(&self.0[..idx])
    }

    /// 最末一段（用于缺省显示名）。
    #[must_use]
    pub fn leaf(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }
}

impl fmt::Display for RefId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RefId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约测试表：逐条对齐 `crates/persistence/src/wiki.rs::normalize_wiki_ref`。
    ///
    /// 每个 case 为 `(输入, 期望输出)`，`None` 表示应被拒绝。server 侧规则变更时
    /// 这张表必须同步更新，否则本地推送会收到 `400 invalid ref`。
    #[test]
    fn ref_contract_matches_server_rules() {
        let cases: &[(&str, Option<&str>)] = &[
            // 基本合法形态
            ("deploy/prod-checklist", Some("deploy/prod-checklist")),
            ("a", Some("a")),
            ("0", Some("0")),
            ("a_b-c/d_e-f", Some("a_b-c/d_e-f")),
            ("deep/a/b/c/d", Some("deep/a/b/c/d")),
            // trim + lowercase
            ("  Deploy/Prod  ", Some("deploy/prod")),
            ("ABC", Some("abc")),
            // 空与空白
            ("", None),
            ("   ", None),
            // 路径穿越
            ("a//b", None),
            ("./a", None),
            ("../a", None),
            ("a/./b", None),
            ("a/../b", None),
            // 首字符必须是小写字母或数字
            ("/a", None),
            ("_a", None),
            ("-a", None),
            // 非法字符集
            ("a b", None),
            ("a.b", None),
            ("生产环境", None),
            ("a#b", None),
            ("a:b", None),
            ("a?b", None),
            // 尾随 `/` 合法（server 未额外拦截，字符集允许）
            ("a/", Some("a/")),
        ];

        for (input, expected) in cases {
            let got = RefId::parse(input).map(RefId::into_string);
            assert_eq!(
                got.as_deref(),
                *expected,
                "ref 契约不一致：输入 {input:?} 期望 {expected:?} 实际 {got:?}"
            );
        }
    }

    #[test]
    fn length_boundary_is_128_bytes() {
        let at_limit = "a".repeat(MAX_REF_LEN);
        assert!(RefId::parse(&at_limit).is_some(), "128 字节应合法");

        let over_limit = "a".repeat(MAX_REF_LEN + 1);
        assert!(RefId::parse(&over_limit).is_none(), "129 字节应被拒绝");
    }

    #[test]
    fn lowercase_happens_before_length_check() {
        // ASCII 大小写转换不改变字节长度，边界行为与 server 一致
        let at_limit = "A".repeat(MAX_REF_LEN);
        assert_eq!(
            RefId::parse(&at_limit).map(RefId::into_string),
            Some("a".repeat(MAX_REF_LEN))
        );
    }

    #[test]
    fn derives_ref_from_relative_path() {
        assert_eq!(
            RefId::from_relative_path("deploy/prod-checklist.md").map(RefId::into_string),
            Some("deploy/prod-checklist".to_owned())
        );
        assert_eq!(
            RefId::from_relative_path("Notes/Daily.markdown").map(RefId::into_string),
            Some("notes/daily".to_owned())
        );
        assert_eq!(
            RefId::from_relative_path("windows\\style\\path.md").map(RefId::into_string),
            Some("windows/style/path".to_owned())
        );
        // 中文路径无法推导，需显式 frontmatter ref
        assert_eq!(RefId::from_relative_path("部署/清单.md"), None);
    }

    #[test]
    fn hierarchy_helpers() {
        let r = RefId::parse("a/b/c").expect("合法 ref");
        assert_eq!(r.segments().collect::<Vec<_>>(), vec!["a", "b", "c"]);
        assert_eq!(r.leaf(), "c");
        assert_eq!(r.parent().map(RefId::into_string), Some("a/b".to_owned()));

        let top = RefId::parse("a").expect("合法 ref");
        assert_eq!(top.parent(), None);
        assert_eq!(top.leaf(), "a");
    }

    #[test]
    fn serde_is_transparent() {
        let r = RefId::parse("a/b").expect("合法 ref");
        let json = serde_json::to_string(&r).expect("序列化");
        assert_eq!(json, "\"a/b\"");
        let back: RefId = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, r);
    }
}