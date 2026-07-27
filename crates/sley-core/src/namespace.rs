//! Git ref namespaces (`GIT_NAMESPACE` / `--namespace`).
//!
//! When a namespace is active, server-side ref advertisement and updates are
//! confined to `refs/namespaces/<ns>/…`, while clients see the un-namespaced
//! logical names. Mirrors `environment.c::get_git_namespace` /
//! `strip_namespace`.
//!
//! The CLI cannot call `setenv` (workspace ban on `env::set_var`), so
//! `--namespace` is recorded in a process-local override that takes precedence
//! over `GIT_NAMESPACE`. Child processes spawned for `ext::` remotes inherit
//! the real environment and re-parse `--namespace` from their argv.

use std::env;
use std::sync::Mutex;

static NAMESPACE_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

/// Record a `--namespace` / `--namespace=` global option for this process.
///
/// Pass `Some("")` to clear an active override; pass `None` to leave the
/// override untouched. An explicit override always wins over `GIT_NAMESPACE`.
pub fn set_git_namespace_override(raw: Option<String>) {
    if let Ok(mut guard) = NAMESPACE_OVERRIDE.lock() {
        *guard = raw;
    }
}

/// Clear any `--namespace` override so subsequent calls read only the env.
pub fn clear_git_namespace_override() {
    if let Ok(mut guard) = NAMESPACE_OVERRIDE.lock() {
        *guard = None;
    }
}

/// The active git namespace prefix, e.g. `refs/namespaces/foo/`, or empty when
/// no namespace is active. Nested raw namespaces (`a/b`) expand to
/// `refs/namespaces/a/refs/namespaces/b/`.
pub fn get_git_namespace() -> String {
    let raw = namespace_raw();
    if raw.is_empty() {
        return String::new();
    }
    let mut buf = String::new();
    for component in raw.split('/') {
        if component.is_empty() {
            continue;
        }
        buf.push_str("refs/namespaces/");
        buf.push_str(component);
        buf.push('/');
    }
    buf
}

/// Strip the active namespace prefix from `namespaced_ref`, returning the
/// logical name. Returns `None` when the ref is outside the current namespace
/// (or when no namespace is active and the name is not itself).
pub fn strip_namespace(namespaced_ref: &str) -> Option<&str> {
    let ns = get_git_namespace();
    if ns.is_empty() {
        return Some(namespaced_ref);
    }
    namespaced_ref.strip_prefix(ns.as_str())
}

/// Expand a logical (client-visible) ref name into the on-disk namespaced form.
pub fn expand_namespace(logical_ref: &str) -> String {
    let ns = get_git_namespace();
    if ns.is_empty() {
        logical_ref.to_string()
    } else {
        format!("{ns}{logical_ref}")
    }
}

/// Whether a namespace is currently active.
pub fn namespace_active() -> bool {
    !get_git_namespace().is_empty()
}

fn namespace_raw() -> String {
    if let Ok(guard) = NAMESPACE_OVERRIDE.lock()
        && let Some(ref value) = *guard
    {
        return value.clone();
    }
    env::var("GIT_NAMESPACE").unwrap_or_default()
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

    static TEST_NAMESPACE_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn empty_namespace_is_identity() {
        let _guard = TEST_NAMESPACE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_git_namespace_override();
        set_git_namespace_override(Some(String::new()));
        assert_eq!(get_git_namespace(), "");
        assert_eq!(strip_namespace("refs/heads/main"), Some("refs/heads/main"));
        assert_eq!(expand_namespace("refs/heads/main"), "refs/heads/main");
        clear_git_namespace_override();
    }

    #[test]
    fn simple_namespace_expands_and_strips() {
        let _guard = TEST_NAMESPACE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_git_namespace_override();
        set_git_namespace_override(Some("namespace".into()));
        assert_eq!(get_git_namespace(), "refs/namespaces/namespace/");
        assert_eq!(
            expand_namespace("refs/heads/main"),
            "refs/namespaces/namespace/refs/heads/main"
        );
        assert_eq!(
            strip_namespace("refs/namespaces/namespace/refs/heads/main"),
            Some("refs/heads/main")
        );
        assert_eq!(
            strip_namespace("refs/namespaces/other/refs/heads/main"),
            None
        );
        clear_git_namespace_override();
    }

    #[test]
    fn nested_namespace_expands() {
        let _guard = TEST_NAMESPACE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_git_namespace_override();
        set_git_namespace_override(Some("a/b".into()));
        assert_eq!(get_git_namespace(), "refs/namespaces/a/refs/namespaces/b/");
        clear_git_namespace_override();
    }

    #[test]
    fn hide_refs_caret_uses_full_name() {
        let patterns = vec!["^refs/namespaces/namespace/refs/tags".into()];
        assert!(ref_is_hidden(
            Some("refs/tags/1"),
            "refs/namespaces/namespace/refs/tags/1",
            &patterns
        ));
        // Without caret, unstripped pattern does not match logical name.
        let patterns = vec!["refs/namespaces/namespace/refs/tags".into()];
        assert!(!ref_is_hidden(
            Some("refs/tags/1"),
            "refs/namespaces/namespace/refs/tags/1",
            &patterns
        ));
        // Logical pattern hides stripped name.
        let patterns = vec!["refs/tags".into()];
        assert!(ref_is_hidden(
            Some("refs/tags/1"),
            "refs/namespaces/namespace/refs/tags/1",
            &patterns
        ));
    }
}
