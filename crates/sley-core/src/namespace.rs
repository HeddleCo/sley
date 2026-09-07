//! Repository/operation-owned Git ref namespace. Never reads process environment.

/// An expanded Git namespace. Construct once at the caller boundary and share
/// by reference across an operation's workers. Nested names retain Git's layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Namespace(String);

impl Namespace {
    pub fn new(raw: &str) -> Self {
        let mut prefix = String::new();
        for component in raw.split('/').filter(|part| !part.is_empty()) {
            prefix.push_str("refs/namespaces/");
            prefix.push_str(component);
            prefix.push('/');
        }
        Self(prefix)
    }

    pub fn prefix(&self) -> &str {
        &self.0
    }

    pub fn strip<'a>(&self, physical: &'a str) -> Option<&'a str> {
        physical.strip_prefix(self.prefix())
    }

    pub fn expand(&self, logical: &str) -> String {
        format!("{}{logical}", self.0)
    }

    pub fn is_active(&self) -> bool {
        !self.0.is_empty()
    }
}

/// Match `transfer.hideRefs` / `uploadpack.hideRefs` / `receive.hideRefs`
/// patterns against a ref, honoring git's full-vs-stripped subject rules:
/// - patterns without `^` match the logical (namespace-stripped) name
/// - patterns with a leading `^` match the full physical name
/// - leading `!` negates a match
///
/// Patterns are evaluated last-to-first; the first match wins.
pub fn ref_is_hidden(refname: Option<&str>, refname_full: &str, patterns: &[String]) -> bool {
    for pattern in patterns.iter().rev() {
        let mut match_pat = pattern.as_str();
        let mut negated = false;
        if let Some(rest) = match_pat.strip_prefix('!') {
            negated = true;
            match_pat = rest;
        }
        let subject = if let Some(rest) = match_pat.strip_prefix('^') {
            match_pat = rest;
            Some(refname_full)
        } else {
            refname
        };
        if let Some(subject) = subject
            && hidden_ref_pattern_matches(subject, match_pat)
        {
            return !negated;
        }
    }
    false
}

/// Trim trailing slashes from a hideRefs pattern value (git's
/// `parse_hide_refs_config`).
pub fn trim_hidden_ref_pattern(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

fn hidden_ref_pattern_matches(refname: &str, pattern: &str) -> bool {
    refname
        .strip_prefix(pattern)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_namespace_is_identity() {
        let ns = Namespace::default();
        assert_eq!(ns.prefix(), "");
        assert_eq!(ns.strip("refs/heads/main"), Some("refs/heads/main"));
        assert_eq!(ns.expand("refs/heads/main"), "refs/heads/main");
    }

    #[test]
    fn namespaces_expand_and_strip_independently() {
        let first = Namespace::new("namespace");
        let second = Namespace::new("a/b");
        assert_eq!(second.prefix(), "refs/namespaces/a/refs/namespaces/b/");
        assert_eq!(
            first.expand("refs/heads/main"),
            "refs/namespaces/namespace/refs/heads/main"
        );
        assert_eq!(
            first.strip("refs/namespaces/namespace/refs/heads/main"),
            Some("refs/heads/main")
        );
        assert_eq!(first.strip("refs/namespaces/other/refs/heads/main"), None);
    }

    #[test]
    fn hide_refs_caret_uses_full_name() {
        let ns = Namespace::new("namespace");
        let full = ns.expand("refs/tags/1");
        assert!(ref_is_hidden(
            ns.strip(&full),
            &full,
            &["^refs/namespaces/namespace/refs/tags".into()]
        ));
        assert!(!ref_is_hidden(
            ns.strip(&full),
            &full,
            &["refs/namespaces/namespace/refs/tags".into()]
        ));
        assert!(ref_is_hidden(ns.strip(&full), &full, &["refs/tags".into()]));
    }
}
