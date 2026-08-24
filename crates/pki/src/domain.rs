//! 域名工具函数（通配符证书覆盖计算）。

/// Compute the one-level wildcard pattern for a domain.
///
/// - `foo.example.com` -> `Some("*.example.com")`
/// - `foo.bar.example.com` -> `Some("*.bar.example.com")` (only one level up)
/// - `example.com` -> `None` (would produce `*.top`, refused)
#[must_use]
pub fn wildcard_for(domain: &str) -> Option<String> {
    let (_, rest) = domain.split_once('.')?;
    if rest.contains('.') {
        Some(format!("*.{rest}"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_for_three_labels() {
        assert_eq!(
            wildcard_for("foo.example.com"),
            Some("*.example.com".to_string())
        );
    }

    #[test]
    fn wildcard_for_four_labels_one_level() {
        assert_eq!(
            wildcard_for("foo.bar.example.com"),
            Some("*.bar.example.com".to_string())
        );
    }

    #[test]
    fn wildcard_for_two_labels_refused() {
        assert_eq!(wildcard_for("example.com"), None);
    }

    #[test]
    fn wildcard_for_single_label() {
        assert_eq!(wildcard_for("localhost"), None);
    }

    #[test]
    fn wildcard_for_trailing_dot() {
        // 严格串处理：不特殊处理 trailing dot
        assert_eq!(
            wildcard_for("foo.example.com."),
            Some("*.example.com.".to_string())
        );
    }
}
