//! Canonical shared path helpers (phase-2 consolidation).
//!
//! Single home for:
//! - the faithful port of git's `relative_path()` (path.c) over raw bytes,
//! - lexical (no-filesystem) normalization and absolute↔relative computation,
//! - bytes↔path conversions matching git's byte-oriented path handling.
//!
//! Everything here is pure string/component math unless documented otherwise;
//! only [`relative_path_from_absolute`] touches the filesystem.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{GitError, Result};

// ---------------------------------------------------------------------------
// git path.c relative_path() — faithful byte-level port
// ---------------------------------------------------------------------------

/// Render `input` relative to `prefix`, a faithful byte-level port of git's
/// `relative_path()` (path.c) for the POSIX, both-relative case (no DOS drive).
/// `prefix` is the cwd prefix and must end with `/` when non-empty, matching
/// git's `cmd_prefix`. Emits `../` for each `prefix` component not shared with
/// `input`, then the unshared tail of `input`.
///
// `i` and `j` are independent cursors because repeated separators can advance
// the prefix and input by different amounts.
#[allow(clippy::suspicious_operation_groupings)]
pub fn relative_path_bytes(input: &[u8], prefix: &[u8]) -> Vec<u8> {
    let in_len = input.len();
    let prefix_len = prefix.len();
    if in_len == 0 {
        return b"./".to_vec();
    }
    if prefix_len == 0 {
        return input.to_vec();
    }
    let is_sep = |byte: u8| byte == b'/';
    let mut i = 0usize;
    let mut j = 0usize;
    let mut prefix_off = 0usize;
    let mut in_off = 0usize;
    while i < prefix_len && j < in_len && prefix.get(i) == input.get(j) {
        if is_sep(prefix[i]) {
            while i < prefix_len && is_sep(prefix[i]) {
                i += 1;
            }
            while j < in_len && is_sep(input[j]) {
                j += 1;
            }
            prefix_off = i;
            in_off = j;
        } else {
            i += 1;
            j += 1;
        }
    }

    if i >= prefix_len && prefix_off < prefix_len {
        if j >= in_len {
            in_off = in_len;
        } else if is_sep(input[j]) {
            while j < in_len && is_sep(input[j]) {
                j += 1;
            }
            in_off = j;
        } else {
            i = prefix_off;
        }
    } else if j >= in_len && in_off < in_len && i < prefix_len && is_sep(prefix[i]) {
        while i < prefix_len && is_sep(prefix[i]) {
            i += 1;
        }
        in_off = in_len;
    }

    let input = &input[in_off..];
    if i >= prefix_len {
        if input.is_empty() {
            return b"./".to_vec();
        }
        return input.to_vec();
    }

    let mut out = Vec::new();
    while i < prefix_len {
        if is_sep(prefix[i]) {
            out.extend_from_slice(b"../");
            while i < prefix_len && is_sep(prefix[i]) {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    if !is_sep(prefix[prefix_len - 1]) {
        out.extend_from_slice(b"../");
    }
    out.extend_from_slice(input);
    out
}

// ---------------------------------------------------------------------------
// Lexical normalization
// ---------------------------------------------------------------------------

/// Lexically normalize a path (collapse `.`/`..`, no filesystem access).
///
/// SAFEST documented semantics, chosen deliberately over the drifting copies
/// this replaces: leading `..` components are *retained* rather than silently
/// dropped (`a/../../b` normalizes to `../b`, never `b`), so an input that
/// escapes its base stays visibly escapable instead of being quietly rewritten
/// into a different path. Mirrors the component cleanup git's
/// `strbuf_realpath_forgiving` performs on the parts of a path that do not
/// exist on disk.
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Compute `target` expressed relative to `base` as a slash-separated string,
/// purely lexically (both sides normalized with [`normalize_lexical`], no
/// filesystem access). Returns `"."` when the two paths coincide.
pub fn relative_path_lexical(target: &Path, base: &Path) -> String {
    let target = normalize_lexical(target);
    let base = normalize_lexical(base);
    let target_components: Vec<_> = target.components().collect();
    let base_components: Vec<_> = base.components().collect();
    let common = target_components
        .iter()
        .zip(base_components.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut result = PathBuf::new();
    for _ in common..base_components.len() {
        result.push("..");
    }
    for component in &target_components[common..] {
        result.push(component.as_os_str());
    }
    if result.as_os_str().is_empty() {
        ".".to_string()
    } else {
        result.display().to_string()
    }
}

/// Compute `target` expressed relative to the directory `cwd`, both expected to
/// be absolute. Like git's `relative_path()` common-ancestor case: emits
/// `../` per unshared `cwd` component, then the unshared tail of `target`,
/// with a trailing `/` when the result names `cwd` itself.
///
/// When the two share no root component (e.g. different DOS drives), `target`
/// is returned unchanged (rendered lossily) — there is no meaningful relative
/// spelling across roots.
pub fn relative_path_from_absolute(cwd: &Path, target: &Path) -> Result<String> {
    let cwd = fs::canonicalize(cwd).map_err(|err| GitError::Io(err.to_string()))?;
    relative_path_from_absolute_components(&cwd, target)
}

/// Component-math core of [`relative_path_from_absolute`] with no filesystem
/// access: callers that already hold canonicalized/real paths use this directly.
pub fn relative_path_from_absolute_components(cwd: &Path, target: &Path) -> Result<String> {
    let cwd_components = cwd.components().collect::<Vec<_>>();
    let target_components = target.components().collect::<Vec<_>>();
    let common = cwd_components
        .iter()
        .zip(target_components.iter())
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return Ok(target.display().to_string());
    }

    let up_count = cwd_components.len().saturating_sub(common);
    let mut parts = Vec::new();
    parts.extend((0..up_count).map(|_| "..".to_string()));
    parts.extend(
        target_components[common..]
            .iter()
            .map(|component| component.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        return Ok("./".into());
    }
    let mut relative = parts.join("/");
    if common == target_components.len() {
        relative.push('/');
    }
    Ok(relative)
}

/// Compute `to_path` expressed relative to the directory `from_dir`, both
/// expected to be absolute, returning a [`PathBuf`].
///
/// Edge semantics pinned by callers (`git worktree move/remove` link rewriting):
/// when the two share no root component, `to_path` (normalized) is returned
/// verbatim, and when the two coincide the result is `.`.
pub fn relative_path_between(from_dir: &Path, to_path: &Path) -> PathBuf {
    let from = normalize_lexical(from_dir);
    let to = normalize_lexical(to_path);
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();
    let mut common = 0usize;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }
    if common == 0 {
        return to;
    }
    let mut relative = PathBuf::new();
    for component in &from_components[common..] {
        if matches!(component, Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &to_components[common..] {
        match component {
            Component::Normal(value) => relative.push(value),
            Component::ParentDir => relative.push(".."),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    relative
}

// ---------------------------------------------------------------------------
// Bytes ↔ path conversions
// ---------------------------------------------------------------------------

/// A path/`OsStr`'s byte encoding. On Unix these are the OS-native bytes; off
/// Unix they are decoded lossily as UTF-8 with `\` normalized to `/`, matching
/// git's forward-slash path convention.
pub fn os_str_to_bytes(value: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

/// Convenience wrapper over [`os_str_to_bytes`] taking a whole path.
pub fn path_to_bytes(path: &Path) -> Vec<u8> {
    os_str_to_bytes(path.as_os_str())
}

/// Interpret raw path bytes as a (relative) [`PathBuf`]. On Unix the bytes are
/// the OS-native path encoding; off Unix they are decoded lossily as UTF-8.
pub fn bytes_to_os_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Interpret raw path bytes as a UTF-8 `String`, failing rather than guessing:
/// git stores paths as opaque bytes, and callers that must echo them back into
/// string-shaped data structures refuse non-UTF-8 input explicitly.
pub fn bytes_to_path_string(bytes: &[u8]) -> Result<String> {
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))
}

/// Render a path with forward slashes (git uses `/` in trace prefixes and
/// on-wire path fields): the join of its normal components, dropping any root.
pub fn path_to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Oracle-derived expectations: upstream git 2.55 renders ls-files/diff
    // paths from a subdirectory through `relative_path()` (path.c) with the
    // cwd prefix (`git rev-parse --show-prefix`) always ending in '/'.
    #[test]
    fn relative_path_bytes_matches_git_relative_path() {
        // No prefix: input passes through untouched.
        assert_eq!(relative_path_bytes(b"a/b/c.txt", b""), b"a/b/c.txt");
        // Empty input: git returns "./".
        assert_eq!(relative_path_bytes(b"", b"a/b/"), b"./");
        // Fully shared prefix: bare tail.
        assert_eq!(relative_path_bytes(b"a/b/c.txt", b"a/b/"), b"c.txt");
        // Input equals the prefix directory itself.
        assert_eq!(relative_path_bytes(b"a/b/", b"a/b/"), b"./");
        // One shared level: single ../.
        assert_eq!(relative_path_bytes(b"a/x.txt", b"a/b/"), b"../x.txt");
        // Sibling subtree several levels up: ../.. per unshared prefix level.
        assert_eq!(
            relative_path_bytes(b"sib/out.txt", b"a/b/c/"),
            b"../../../sib/out.txt"
        );
        // Root file viewed from deep inside.
        assert_eq!(
            relative_path_bytes(b"root.txt", b"a/b/c/"),
            b"../../../root.txt"
        );
        // Deeper tail under the prefix keeps inner directories.
        assert_eq!(relative_path_bytes(b"a/b/c/d/e.txt", b"a/b/c/"), b"d/e.txt");
        // Prefix without trailing slash still terminates one level short
        // (git's cmd_prefix always supplies '/', but path.c tolerates both).
        assert_eq!(relative_path_bytes(b"a/top.txt", b"a/b"), b"../top.txt");
        // Divergent components at the same depth are NOT shared: 's' != 't'.
        assert_eq!(relative_path_bytes(b"t/c.txt", b"same/"), b"../t/c.txt");
    }

    #[test]
    fn normalize_lexical_retains_leading_dotdot_and_drops_curdir() {
        assert_eq!(
            normalize_lexical(Path::new("a/b/../c")),
            PathBuf::from("a/c")
        );
        assert_eq!(
            normalize_lexical(Path::new("./a/./b")),
            PathBuf::from("a/b")
        );
        // Leading .. must survive: the path genuinely escapes its base.
        assert_eq!(normalize_lexical(Path::new("../b")), PathBuf::from("../b"));
        assert_eq!(
            normalize_lexical(Path::new("a/../../b")),
            PathBuf::from("../b")
        );
        // Ascending past an absolute root cannot go further: git's forgiving
        // realpath keeps the residual `..` visible rather than clamping.
        assert_eq!(normalize_lexical(Path::new("/..")), PathBuf::from("/.."));
        assert_eq!(
            normalize_lexical(Path::new("/a/../../c")),
            PathBuf::from("/../c")
        );
        assert_eq!(normalize_lexical(Path::new("")), PathBuf::from(""));
    }

    #[test]
    fn relative_path_lexical_handles_sibling_worktree_layouts() {
        // Main admin dir and linked-worktree .git under one parent.
        let admin = Path::new("/repo/.git");
        let wt = Path::new("/repo/wt/.git");
        assert_eq!(relative_path_lexical(wt, admin), "../wt/.git");
        assert_eq!(relative_path_lexical(admin, wt), "../../.git");
        assert_eq!(relative_path_lexical(admin, admin), ".");
    }

    #[test]
    fn relative_path_from_absolute_components_pins_edges() {
        // Same directory: "./" (trailing slash marks the directory itself).
        assert_eq!(
            relative_path_from_absolute_components(Path::new("/r/wt"), Path::new("/r/wt"))
                .unwrap_or_default(),
            "./"
        );
        // Descendant: plain tail.
        assert_eq!(
            relative_path_from_absolute_components(Path::new("/r/wt"), Path::new("/r/wt/a/b"))
                .unwrap_or_default(),
            "a/b"
        );
        // Ancestor: ../ chain.
        assert_eq!(
            relative_path_from_absolute_components(Path::new("/r/wt/sub"), Path::new("/r/.git"))
                .unwrap_or_default(),
            "../../.git"
        );
        // Sharing only the root still walks up one level (POSIX): /a → /b/c.
        assert_eq!(
            relative_path_from_absolute_components(Path::new("/a"), Path::new("/b/c"))
                .unwrap_or_default(),
            "../b/c"
        );
        // Truly disjoint roots (no common prefix component at all — different
        // DOS drives, or an empty cwd side): target verbatim.
        assert_eq!(
            relative_path_from_absolute_components(Path::new(""), Path::new("/b/c"))
                .unwrap_or_default(),
            "/b/c"
        );
    }

    #[test]
    fn relative_path_between_keeps_move_remove_edge_semantics() {
        // Same dir yields "." (not "./") for link-file rewriting.
        assert_eq!(
            relative_path_between(Path::new("/r/.git"), Path::new("/r/.git")),
            PathBuf::from(".")
        );
        assert_eq!(
            relative_path_between(Path::new("/r/.git"), Path::new("/r/wt/.git")),
            PathBuf::from("../wt/.git")
        );
        assert_eq!(
            relative_path_between(Path::new("/r/wt/.git"), Path::new("/r/.git")),
            PathBuf::from("../../.git")
        );
        // Lexical normalization applies before the walk.
        assert_eq!(
            relative_path_between(
                Path::new("/r/wt/../.git"),
                Path::new("/r/linked/../wt2/.git")
            ),
            PathBuf::from("../wt2/.git")
        );
    }

    #[test]
    fn byte_conversions_round_trip_and_slash_normalize() {
        let weird = bytes_to_os_path(b"\xff\xfe/weird.txt");
        assert_eq!(path_to_bytes(&weird), b"\xff\xfe/weird.txt");
        assert_eq!(os_str_to_bytes(OsStr::new("plain/path")), b"plain/path");
        assert_eq!(path_to_slash(Path::new("/a/b/c")), "a/b/c");
        assert_eq!(path_to_slash(Path::new("a/b")), "a/b");
        assert_eq!(
            bytes_to_path_string(b"ok.txt").ok(),
            Some("ok.txt".to_string())
        );
        assert!(bytes_to_path_string(b"\xff").is_err());
    }
}
