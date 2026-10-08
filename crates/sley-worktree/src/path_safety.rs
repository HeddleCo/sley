//! Worktree path validation: git's `verify_path` and its `.git` alias rules.
//!
//! Every path sley writes into a worktree comes from a tree, an index, or a
//! patch, and a remote controls all three. Git refuses a path whose component
//! names the repository's `.git` directory on *any* filesystem it protects,
//! not only on the one it happens to run on, because a worktree written on
//! one system is routinely read on another:
//!
//! * `.git` in any ASCII case (case-insensitive filesystems). Always on.
//! * `.git` followed by trailing dots or spaces, an alternate data stream
//!   (`.git::$INDEX_ALLOCATION`), the 8.3 short name `GIT~1`, or a backslash
//!   separator (NTFS; `core.protectNTFS`, on by default as in git).
//! * `.git` with code points HFS+ ignores, such as `.g\u{200c}it`
//!   (`core.protectHFS`). Git enables this by default only on macOS; sley
//!   enables it everywhere, because a hostile name costs nothing to refuse
//!   and heddle worktrees are synced across platforms.
//! * `.gitmodules` (under the same aliases) as a symlink.
//! * `.`, `..` and empty components.
//!
//! The functions here are byte-for-byte ports of git's `read-cache.c`
//! (`verify_path_internal`, `verify_dotfile`), `path.c` (`is_ntfs_dotgit`,
//! `is_ntfs_dot_generic`) and `utf8.c` (`is_hfs_dot_generic`,
//! `next_hfs_char`), so they agree with git on every input. A NUL-terminated
//! C string is modelled as a byte slice whose out-of-range reads yield `0`.
//!
//! A caller may reserve extra names with [`WorktreePathPolicy::reserve_root_name`]
//! (heddle reserves a root `.heddle`) or [`WorktreePathPolicy::reserve_name`].
//! Reserved names are matched under the same aliases as `.git`, always,
//! regardless of the NTFS and HFS switches.

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_odb::FileObjectDatabase;

// Preserve the public worktree predicate paths while fsck and remote consumers
// share the same pure implementation without depending on worktree I/O.
use sley_core::path_safety::is_reserved_alias;
pub use sley_core::path_safety::{
    is_hfs_dot_generic, is_hfs_dotgit, is_hfs_dotgitmodules, is_ntfs_dot_generic, is_ntfs_dotgit,
    is_ntfs_dotgitmodules,
};

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

/// Rules a worktree path must satisfy before sley writes it.
///
/// [`WorktreePathPolicy::default`] matches git's defaults on the strictest
/// platform: NTFS and HFS+ protection on, nothing extra reserved.
/// [`WorktreePathPolicy::from_config`] honours `core.protectNTFS` and
/// `core.protectHFS`, which a user may set to `false` exactly as in git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreePathPolicy {
    protect_ntfs: bool,
    protect_hfs: bool,
    reserved_root_names: Vec<Vec<u8>>,
    reserved_names: Vec<Vec<u8>>,
}

impl Default for WorktreePathPolicy {
    fn default() -> Self {
        Self {
            protect_ntfs: true,
            protect_hfs: true,
            reserved_root_names: Vec::new(),
            reserved_names: Vec::new(),
        }
    }
}

impl WorktreePathPolicy {
    /// The default policy: NTFS and HFS+ protection on.
    pub fn new() -> Self {
        Self::default()
    }

    /// The default policy adjusted by `core.protectNTFS` and
    /// `core.protectHFS` from `config`. Both default to `true`.
    pub fn from_config(config: &GitConfig) -> Self {
        Self {
            protect_ntfs: config.get_bool("core", None, "protectNTFS").unwrap_or(true),
            protect_hfs: config.get_bool("core", None, "protectHFS").unwrap_or(true),
            ..Self::default()
        }
    }

    /// Refuse (or allow) NTFS aliases of `.git`: trailing dots and spaces,
    /// alternate data streams, `GIT~1`, and `\` as a separator.
    pub fn protect_ntfs(mut self, enabled: bool) -> Self {
        self.protect_ntfs = enabled;
        self
    }

    /// Refuse (or allow) `.git` spelled with code points HFS+ ignores.
    pub fn protect_hfs(mut self, enabled: bool) -> Self {
        self.protect_hfs = enabled;
        self
    }

    /// Whether NTFS aliases of `.git` are refused.
    pub fn protects_ntfs(&self) -> bool {
        self.protect_ntfs
    }

    /// Whether HFS+ aliases of `.git` are refused.
    pub fn protects_hfs(&self) -> bool {
        self.protect_hfs
    }

    /// Reserve `name` as the first component of a path, under every alias
    /// `.git` has (any case; trailing dots or spaces; `:` streams; `\`
    /// separators; the 8.3 short name; HFS+ ignorable code points).
    ///
    /// heddle reserves `.heddle` this way: a hostile tree may not create a
    /// root `.heddle`, but `docs/.heddle` is ordinary content.
    pub fn reserve_root_name(mut self, name: impl Into<Vec<u8>>) -> Self {
        let name = name.into();
        if !name.is_empty() {
            self.reserved_root_names.push(name);
        }
        self
    }

    /// Reserve `name` at every depth, under the same aliases as
    /// [`WorktreePathPolicy::reserve_root_name`].
    pub fn reserve_name(mut self, name: impl Into<Vec<u8>>) -> Self {
        let name = name.into();
        if !name.is_empty() {
            self.reserved_names.push(name);
        }
        self
    }

    /// Names reserved only as the first path component.
    pub fn reserved_root_names(&self) -> impl Iterator<Item = &[u8]> {
        self.reserved_root_names.iter().map(Vec::as_slice)
    }

    /// Names reserved at every depth.
    pub fn reserved_names(&self) -> impl Iterator<Item = &[u8]> {
        self.reserved_names.iter().map(Vec::as_slice)
    }

    /// Whether `path` (repository-relative, `/`-separated) with tree/index
    /// `mode` may be written into a worktree. This is git's `verify_path`
    /// plus the caller's reserved names.
    pub fn is_valid_path(&self, path: &[u8], mode: u32) -> bool {
        verify_path_internal(path, mode, self.protect_ntfs, self.protect_hfs) == PathCheck::Ok
            && !self.has_reserved_component(path)
    }

    /// [`WorktreePathPolicy::is_valid_path`] as a `Result`, with git's
    /// `invalid path '<path>'` message.
    pub fn verify_path(&self, path: &[u8], mode: u32) -> Result<()> {
        if self.is_valid_path(path, mode) {
            Ok(())
        } else {
            Err(invalid_path_error(path))
        }
    }

    /// Whether some component of `path` is a caller-reserved name.
    fn has_reserved_component(&self, path: &[u8]) -> bool {
        if self.reserved_root_names.is_empty() && self.reserved_names.is_empty() {
            return false;
        }
        path.split(|byte| *byte == b'/')
            .enumerate()
            .any(|(index, component)| {
                self.reserved_names
                    .iter()
                    .any(|name| is_reserved_alias(component, name))
                    || (index == 0
                        && self
                            .reserved_root_names
                            .iter()
                            .any(|name| is_reserved_alias(component, name)))
            })
    }
}

/// The error sley reports for a refused worktree path.
pub(crate) fn invalid_path_error(path: &[u8]) -> GitError {
    GitError::InvalidPath(format!("'{}'", String::from_utf8_lossy(path)))
}

/// Verify every path in a tree before it is checked out, without touching
/// the worktree. A checkout that would write any refused path writes nothing.
pub fn verify_tree_paths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    policy: &WorktreePathPolicy,
) -> Result<()> {
    for (path, (mode, _)) in sley_diff_merge::flatten_tree(db, format, tree_oid)? {
        policy.verify_path(&path, mode)?;
    }
    Ok(())
}

/// Verify a set of `(path, mode)` entries, failing on the first refused one.
pub(crate) fn verify_entry_paths<'a>(
    policy: &WorktreePathPolicy,
    entries: impl IntoIterator<Item = (&'a [u8], u32)>,
) -> Result<()> {
    for (path, mode) in entries {
        policy.verify_path(path, mode)?;
    }
    Ok(())
}

/// Repository-derived policy for callers without an explicit policy.
pub(crate) fn checkout_path_policy(config: &GitConfig) -> WorktreePathPolicy {
    WorktreePathPolicy::from_config(config)
}

pub(crate) fn checkout_path_policy_for_git_dir(git_dir: &std::path::Path) -> WorktreePathPolicy {
    let config = sley_config::read_effective_worktree_config(git_dir, None).unwrap_or_default();
    WorktreePathPolicy::from_config(&config)
}

pub(crate) fn checkout_path_policy_for(
    config: Option<&GitConfig>,
    git_dir: &std::path::Path,
) -> WorktreePathPolicy {
    match config {
        Some(config) => checkout_path_policy(config),
        None => checkout_path_policy_for_git_dir(git_dir),
    }
}

/// Resolve the worktree's gitdir, including linked worktrees' `.git` files.
pub(crate) fn writer_path_policy(root: &std::path::Path) -> WorktreePathPolicy {
    let dotgit = root.join(".git");
    let git_dir = std::fs::read(&dotgit)
        .ok()
        .and_then(|bytes| {
            let value = std::str::from_utf8(&bytes)
                .ok()?
                .trim()
                .strip_prefix("gitdir: ")?;
            Some(root.join(value))
        })
        .unwrap_or(dotgit);
    checkout_path_policy_for_git_dir(&git_dir)
}

/// Verify every target of a tree checkout before any of it is written, as
/// git's unpack-trees does when it adds each entry to the result index.
pub(crate) fn verify_tracked_entries(
    policy: &WorktreePathPolicy,
    entries: &std::collections::BTreeMap<Vec<u8>, crate::index_io::TrackedEntry>,
) -> Result<()> {
    verify_entry_paths(
        policy,
        entries
            .iter()
            .map(|(path, entry)| (path.as_slice(), entry.mode)),
    )
}

/// The checks git applies to every path regardless of configuration
/// (`verify_dotfile`): no empty, `.` or `..` component and no `.git` in any
/// ASCII case. The worktree writer applies these to every path it creates,
/// as a backstop for callers that bypass checkout pre-flight.
pub(crate) fn verify_path_unconditional(path: &[u8], mode: u32) -> Result<()> {
    if verify_path_internal(path, mode, false, false) == PathCheck::Ok {
        Ok(())
    } else {
        Err(invalid_path_error(path))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathCheck {
    Ok,
    Invalid,
    /// A trailing `/` on a directory entry (sparse-index directory).
    DirWithSep,
}

/// Byte at `index`, or `0` past the end (C string semantics).
fn at(bytes: &[u8], index: usize) -> u8 {
    bytes.get(index).copied().unwrap_or(0)
}

fn tail(bytes: &[u8], index: usize) -> &[u8] {
    bytes.get(index..).unwrap_or(&[])
}

/// git `read-cache.c` `verify_path_internal`, with native Windows validation.
fn verify_path_internal(
    path: &[u8],
    mode: u32,
    protect_ntfs: bool,
    protect_hfs: bool,
) -> PathCheck {
    if path.contains(&0) {
        return PathCheck::Invalid;
    }
    #[cfg(windows)]
    if path.contains(&b'\\')
        || (path.get(1) == Some(&b':'))
        || (protect_ntfs && !is_valid_win32_path(path))
    {
        return PathCheck::Invalid;
    }
    let is_link = mode & S_IFMT == S_IFLNK;
    let mut i = 0usize;
    loop {
        // `inside:` — `path[i..]` starts a component.
        let rest = tail(path, i);
        if protect_hfs && (is_hfs_dotgit(rest) || (is_link && is_hfs_dotgitmodules(rest))) {
            return PathCheck::Invalid;
        }
        if protect_ntfs && (is_ntfs_dotgit(rest) || (is_link && is_ntfs_dotgitmodules(rest))) {
            return PathCheck::Invalid;
        }
        let c = at(path, i);
        i += 1;
        if (c == b'.' && !verify_dotfile(tail(path, i), mode)) || c == b'/' {
            return PathCheck::Invalid;
        }
        if c == 0 {
            return if mode & S_IFMT == S_IFDIR {
                PathCheck::DirWithSep
            } else {
                PathCheck::Invalid
            };
        }
        // The rest of the component.
        loop {
            let c = at(path, i);
            i += 1;
            if c == 0 {
                return PathCheck::Ok;
            }
            if c == b'/' {
                break;
            }
            if c == b'\\' && protect_ntfs {
                let rest = tail(path, i);
                if is_ntfs_dotgit(rest) || (is_link && is_ntfs_dotgitmodules(rest)) {
                    return PathCheck::Invalid;
                }
            }
        }
    }
}

/// Git for Windows `compat/mingw.c::is_valid_win32_path` (creation form).
/// Backslashes and drive prefixes are refused separately, even with NTFS off.
#[cfg(any(windows, test))]
fn is_valid_win32_path(path: &[u8]) -> bool {
    for component in path.split(|byte| *byte == b'/') {
        if component
            .iter()
            .any(|byte| *byte < 0x20 || b":<>\"|?*".contains(byte))
        {
            return false;
        }
        if component
            .last()
            .is_some_and(|byte| matches!(byte, b' ' | b'.'))
            && component != b"."
            && component != b".."
        {
            return false;
        }
        let reserved_len = if [b"AUX".as_slice(), b"CON", b"NUL", b"PRN"]
            .iter()
            .any(|name| {
                component
                    .get(..name.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
            }) {
            if component
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"CONIN$"))
            {
                6
            } else if component
                .get(..7)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"CONOUT$"))
            {
                7
            } else {
                3
            }
        } else if component
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"COM"))
            && component
                .get(3)
                .is_some_and(|byte| (b'1'..=b'9').contains(byte))
            || component
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"LPT"))
                && component.get(3).is_some_and(u8::is_ascii_digit)
        {
            4
        } else {
            continue;
        };
        let rest = tail(component, reserved_len);
        let rest = rest
            .iter()
            .position(|byte| *byte != b' ')
            .map_or(&[][..], |i| &rest[i..]);
        if rest.is_empty() || matches!(rest[0], b'.' | b':') {
            return false;
        }
    }
    true
}

/// git `read-cache.c` `verify_dotfile`: `rest` follows a leading `.`.
fn verify_dotfile(rest: &[u8], mode: u32) -> bool {
    let c0 = at(rest, 0);
    if c0 == 0 || c0 == b'/' {
        return false;
    }
    match c0 {
        b'g' | b'G' => {
            if !at(rest, 1).eq_ignore_ascii_case(&b'i') || !at(rest, 2).eq_ignore_ascii_case(&b't')
            {
                return true;
            }
            let c3 = at(rest, 3);
            if c3 == 0 || c3 == b'/' {
                return false;
            }
            if mode & S_IFMT == S_IFLNK {
                let after = tail(rest, 3);
                if after.len() >= 7 && after[..7].eq_ignore_ascii_case(b"modules") {
                    let c = at(after, 7);
                    if c == 0 || c == b'/' {
                        return false;
                    }
                }
            }
            true
        }
        b'.' => {
            let c1 = at(rest, 1);
            !(c1 == 0 || c1 == b'/')
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(path: &str, mode: u32) -> bool {
        WorktreePathPolicy::default().is_valid_path(path.as_bytes(), mode)
    }

    #[test]
    fn win32_creation_rules_match_git_for_windows() {
        for path in [
            "AUX",
            "aux.txt",
            "CON",
            "CONIN$",
            "CONOUT$.txt",
            "NUL",
            "PRN ",
            "COM1",
            "COM9.x",
            "LPT0",
            "LPT9",
            "dir/aux",
            "a.",
            "a ",
            "a:b",
            "a?b",
            "a*b",
            "a<b",
            "a>b",
            "a|b",
            "a\"b",
            "a\u{1f}b",
        ] {
            assert!(!is_valid_win32_path(path.as_bytes()), "{path:?}");
        }
        for path in [
            "COM0",
            "COM10",
            "LPT10",
            "auxiliary",
            "conifer",
            "CONIN",
            "CONOUT",
            "AUX x",
            ".",
            "..",
            "dir/file",
            "..dots",
            "a. b",
        ] {
            assert!(is_valid_win32_path(path.as_bytes()), "{path:?}");
        }
    }

    #[test]
    fn literal_and_case_aliases_are_always_refused() {
        let lax = WorktreePathPolicy::default()
            .protect_ntfs(false)
            .protect_hfs(false);
        for path in [
            ".git", ".GIT/x", "a/.Git/b", ".gIt", ".", "a/./b", "..", "a/../b", "/a", "a//b", "",
        ] {
            assert!(!lax.is_valid_path(path.as_bytes(), 0o100644), "{path:?}");
        }
        // A directory entry may carry a trailing separator, but a file may not.
        assert!(!lax.is_valid_path(b"a/", 0o100644));
        assert!(!lax.is_valid_path(b"a/", 0o040000));
        assert!(lax.is_valid_path(b"git~1/x", 0o100644));
        assert!(lax.is_valid_path(".g\u{200c}it/x".as_bytes(), 0o100644));
    }

    #[test]
    fn ntfs_aliases_follow_git() {
        for path in [
            ".git.",
            ".git ",
            ".git. . ",
            "git~1",
            "GIT~1/x",
            "git~1.",
            ".git::$INDEX_ALLOCATION",
            ".git:x",
            "git~1:y",
            "a\\.git",
            "a\\.git\\hooks",
            ".git\\x",
        ] {
            assert!(!valid(path, 0o100644), "{path:?}");
        }
        for path in [
            "git~2", "git~10", ".gitx", ".git.x", "x\\git", "\\.git", "gi~1",
        ] {
            assert_eq!(
                valid(path, 0o100644),
                !cfg!(windows) || !path.contains('\\'),
                "{path:?}"
            );
        }
    }

    #[test]
    fn gitmodules_symlink_aliases_follow_git() {
        for path in [
            ".gitmodules",
            ".GITMODULES",
            ".gitmodules.",
            ".gitmodules ::$DATA",
            "gitmod~1",
            "GITMOD~4",
            "gi7eba~1",
            "gi7eb~12",
            ".g\u{200c}itmodules",
            "a/.gitmodules",
        ] {
            assert!(!valid(path, 0o120000), "{path:?}");
            if !path.ends_with(".gitmodules") || path.contains('\u{200c}') {
                // Non-literal aliases are plain names when not a symlink.
                assert_eq!(
                    valid(path, 0o100644),
                    !cfg!(windows) || is_valid_win32_path(path.as_bytes()),
                    "{path:?} as a file"
                );
            }
        }
        assert!(valid(".gitmodules", 0o100644));
        assert!(valid("gitmod~5", 0o120000));
        assert!(valid(".gitmodulesx", 0o120000));
    }

    #[test]
    fn hfs_aliases_follow_git() {
        for path in [
            ".g\u{200c}it",
            "\u{feff}.git/x",
            ".GI\u{206f}T",
            ".git\u{200d}",
            ".git\u{200d}/x",
        ] {
            assert!(!valid(path, 0o100644), "{path:?}");
        }
        assert!(
            valid(".g\u{00ad}it", 0o100644),
            "U+00AD is not HFS-ignorable in git"
        );
        // Malformed UTF-8 after `.git` ends the name in git's reader.
        assert!(!valid_bytes(b".git\xff"));
        assert!(valid_bytes(b".gi\xfft"));
    }

    fn valid_bytes(path: &[u8]) -> bool {
        WorktreePathPolicy::default().is_valid_path(path, 0o100644)
    }

    #[test]
    fn protections_follow_config() {
        let config = GitConfig::parse(b"[core]\n\tprotectNTFS = false\n\tprotectHFS = false\n")
            .expect("parse config");
        let policy = WorktreePathPolicy::from_config(&config);
        assert!(!policy.protects_ntfs());
        assert!(!policy.protects_hfs());
        assert!(policy.is_valid_path(b"git~1/x", 0o100644));
        assert!(!policy.is_valid_path(b".GIT/x", 0o100644));
        let defaults = WorktreePathPolicy::from_config(&GitConfig::default());
        assert!(defaults.protects_ntfs() && defaults.protects_hfs());
    }

    #[test]
    fn reserved_root_names_match_aliases_only_at_the_root() {
        let policy = WorktreePathPolicy::default().reserve_root_name(".heddle");
        for path in [
            ".heddle",
            ".heddle/config",
            ".HEDDLE/x",
            ".heddle./x",
            ".heddle /x",
            ".heddle::$INDEX_ALLOCATION/x",
            ".heddle\\x",
            "HEDDLE~1/x",
            "heddle~3",
            ".hed\u{200c}dle/x",
        ] {
            assert!(!policy.is_valid_path(path.as_bytes(), 0o100644), "{path:?}");
        }
        for path in [
            "docs/.heddle",
            "a/.HEDDLE/x",
            ".heddlex",
            ".heddle-notes",
            "heddle",
            "heddle~5",
        ] {
            assert!(policy.is_valid_path(path.as_bytes(), 0o100644), "{path:?}");
        }
        let everywhere = WorktreePathPolicy::default().reserve_name(".heddle");
        assert!(!everywhere.is_valid_path(b"docs/.heddle/x", 0o100644));
    }
}
