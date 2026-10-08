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

std::thread_local! {
    static SCOPED_POLICY: std::cell::RefCell<Option<WorktreePathPolicy>> =
        const { std::cell::RefCell::new(None) };
}

/// Restores the previous scoped policy on drop.
pub(crate) struct ScopedPolicyGuard {
    previous: Option<WorktreePathPolicy>,
}

impl Drop for ScopedPolicyGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        SCOPED_POLICY.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Install `policy` for checkout pre-flight on the current thread until the
/// guard drops. Mirrors `set_process_filter_metadata`: the policy-taking entry
/// points scope it around the shared checkout machinery instead of threading
/// it through every internal signature. Pre-flight always runs on the calling
/// thread, before parallel checkout workers start.
pub(crate) fn scope_worktree_path_policy(policy: &WorktreePathPolicy) -> ScopedPolicyGuard {
    let previous = SCOPED_POLICY.with(|slot| slot.borrow_mut().replace(policy.clone()));
    ScopedPolicyGuard { previous }
}

/// The policy for a checkout into a repository with `config`: the caller's
/// scoped policy when one is installed, otherwise `config`'s.
pub(crate) fn checkout_path_policy(config: &GitConfig) -> WorktreePathPolicy {
    SCOPED_POLICY
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(|| WorktreePathPolicy::from_config(config))
}

/// [`checkout_path_policy`] for callers that only know the git directory.
pub(crate) fn checkout_path_policy_for_git_dir(git_dir: &std::path::Path) -> WorktreePathPolicy {
    if let Some(policy) = SCOPED_POLICY.with(|slot| slot.borrow().clone()) {
        return policy;
    }
    let config = sley_config::read_effective_worktree_config(git_dir, None).unwrap_or_default();
    WorktreePathPolicy::from_config(&config)
}

/// The checkout policy from `config` when the caller supplied one, else from
/// the repository's effective config.
pub(crate) fn checkout_path_policy_for(
    config: Option<&GitConfig>,
    git_dir: &std::path::Path,
) -> WorktreePathPolicy {
    match config {
        Some(config) => checkout_path_policy(config),
        None => checkout_path_policy_for_git_dir(git_dir),
    }
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

/// git `read-cache.c` `verify_path_internal` on a POSIX build: no DOS drive
/// prefix and no win32 path validation, `/` the only directory separator.
fn verify_path_internal(
    path: &[u8],
    mode: u32,
    protect_ntfs: bool,
    protect_hfs: bool,
) -> PathCheck {
    if path.contains(&0) {
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

fn is_xplatform_dir_sep(c: u8) -> bool {
    c == b'/' || c == b'\\'
}

/// git `path.c` `is_ntfs_dotgit`: `name` is the remainder of a path starting
/// at a component. True for `.git` or `git~1` (any case), followed by only
/// dots and spaces up to the end, a `/` or `\`, or a `:` stream suffix.
pub fn is_ntfs_dotgit(name: &[u8]) -> bool {
    let mut i;
    let c = at(name, 0);
    if c == b'.' {
        if !at(name, 1).eq_ignore_ascii_case(&b'g')
            || !at(name, 2).eq_ignore_ascii_case(&b'i')
            || !at(name, 3).eq_ignore_ascii_case(&b't')
        {
            return false;
        }
        i = 4;
    } else if c == b'g' || c == b'G' {
        if !at(name, 1).eq_ignore_ascii_case(&b'i')
            || !at(name, 2).eq_ignore_ascii_case(&b't')
            || at(name, 3) != b'~'
            || at(name, 4) != b'1'
        {
            return false;
        }
        i = 5;
    } else {
        return false;
    }
    loop {
        let c = at(name, i);
        i += 1;
        if c == 0 || is_xplatform_dir_sep(c) || c == b':' {
            return true;
        }
        if c != b'.' && c != b' ' {
            return false;
        }
    }
}

/// git `path.c` `is_ntfs_dotgitmodules`.
pub fn is_ntfs_dotgitmodules(name: &[u8]) -> bool {
    is_ntfs_dot_generic(name, b"gitmodules", b"gi7eba")
}

/// git `path.c` `is_ntfs_dot_generic`: `.<dotgit_name>`, its regular 8.3
/// short name (first six characters, `~1`..`~4`), or the hashed fall-back
/// short name `<shortname_prefix>~N`, then only dots and spaces up to the
/// end or a `:` stream suffix.
fn is_ntfs_dot_generic(name: &[u8], dotgit_name: &[u8], shortname_prefix: &[u8]) -> bool {
    let len = dotgit_name.len();
    if at(name, 0) == b'.' && strncasecmp_eq(tail(name, 1), dotgit_name, len) {
        return only_spaces_and_periods(name, len + 1);
    }
    if strncasecmp_eq(name, dotgit_name, 6)
        && at(name, 6) == b'~'
        && (b'1'..=b'4').contains(&at(name, 7))
    {
        return only_spaces_and_periods(name, 8);
    }
    let mut saw_tilde = false;
    let mut i = 0usize;
    while i < 8 {
        let c = at(name, i);
        if c == 0 {
            return false;
        } else if saw_tilde {
            if !c.is_ascii_digit() {
                return false;
            }
        } else if c == b'~' {
            i += 1;
            if !(b'1'..=b'9').contains(&at(name, i)) {
                return false;
            }
            saw_tilde = true;
        } else if i >= 6 || c & 0x80 != 0 {
            return false;
        } else if c.to_ascii_lowercase() != at(shortname_prefix, i) {
            return false;
        }
        i += 1;
    }
    only_spaces_and_periods(name, i)
}

fn only_spaces_and_periods(name: &[u8], mut i: usize) -> bool {
    loop {
        let c = at(name, i);
        i += 1;
        if c == 0 || c == b':' {
            return true;
        }
        if c != b' ' && c != b'.' {
            return false;
        }
    }
}

/// C `strncasecmp(a, b, n) == 0` over NUL-terminated views.
fn strncasecmp_eq(a: &[u8], b: &[u8], n: usize) -> bool {
    for index in 0..n {
        let (x, y) = (at(a, index), at(b, index));
        if !x.eq_ignore_ascii_case(&y) {
            return false;
        }
        if x == 0 {
            return true;
        }
    }
    true
}

/// git `utf8.c` `is_hfs_dotgit`: after dropping the code points HFS+
/// ignores, `.git` (ASCII case-insensitive) followed by the end or `/`.
pub fn is_hfs_dotgit(path: &[u8]) -> bool {
    is_hfs_dot_generic(path, b"git")
}

/// git `utf8.c` `is_hfs_dotgitmodules`.
pub fn is_hfs_dotgitmodules(path: &[u8]) -> bool {
    is_hfs_dot_generic(path, b"gitmodules")
}

fn is_hfs_dot_generic(path: &[u8], needle: &[u8]) -> bool {
    let mut cursor = Some(0usize);
    if next_hfs_char(path, &mut cursor) != u32::from(b'.') {
        return false;
    }
    for expected in needle {
        let c = next_hfs_char(path, &mut cursor);
        if c > 127 {
            return false;
        }
        // `c <= 127` was just checked, so the narrowing is lossless.
        if (c as u8).to_ascii_lowercase() != *expected {
            return false;
        }
    }
    let c = next_hfs_char(path, &mut cursor);
    c == 0 || c == u32::from(b'/')
}

/// git `utf8.c` `next_hfs_char`. `cursor` is `None` once malformed UTF-8 has
/// been seen, which reads as the end of the string (as in git).
fn next_hfs_char(path: &[u8], cursor: &mut Option<usize>) -> u32 {
    loop {
        let Some(position) = *cursor else {
            return 0;
        };
        let Some((ch, width)) = pick_one_utf8_char(tail(path, position)) else {
            *cursor = None;
            return 0;
        };
        *cursor = Some(position + width);
        if is_hfs_ignorable(ch) {
            continue;
        }
        return ch;
    }
}

/// The code points HFS+ drops when comparing names (git `next_hfs_char`).
pub(crate) fn is_hfs_ignorable(ch: u32) -> bool {
    matches!(
        ch,
        0x200c..=0x200f | 0x202a..=0x202e | 0x206a..=0x206f | 0xfeff
    )
}

/// git `utf8.c` `pick_one_utf8_char` on a NUL-terminated string: the code
/// point and its width, or `None` for malformed UTF-8.
fn pick_one_utf8_char(s: &[u8]) -> Option<(u32, usize)> {
    let b = |index: usize| u32::from(at(s, index));
    let s0 = b(0);
    if s0 < 0x80 {
        return Some((s0, 1));
    }
    let cont = |index: usize| b(index) & 0xc0 == 0x80;
    if s0 & 0xe0 == 0xc0 {
        if !cont(1) || s0 & 0xfe == 0xc0 {
            return None;
        }
        return Some((((s0 & 0x1f) << 6) | (b(1) & 0x3f), 2));
    }
    if s0 & 0xf0 == 0xe0 {
        if !cont(1)
            || !cont(2)
            || (s0 == 0xe0 && b(1) & 0xe0 == 0x80)
            || (s0 == 0xed && b(1) & 0xe0 == 0xa0)
            || (s0 == 0xef && b(1) == 0xbf && b(2) & 0xfe == 0xbe)
        {
            return None;
        }
        return Some((
            ((s0 & 0x0f) << 12) | ((b(1) & 0x3f) << 6) | (b(2) & 0x3f),
            3,
        ));
    }
    if s0 & 0xf8 == 0xf0 {
        if !cont(1)
            || !cont(2)
            || !cont(3)
            || (s0 == 0xf0 && b(1) & 0xf0 == 0x80)
            || (s0 == 0xf4 && b(1) > 0x8f)
            || s0 > 0xf4
        {
            return None;
        }
        return Some((
            ((s0 & 0x07) << 18) | ((b(1) & 0x3f) << 12) | ((b(2) & 0x3f) << 6) | (b(3) & 0x3f),
            4,
        ));
    }
    None
}

/// Whether the path component `component` aliases the reserved `name` under
/// any of the rules git applies to `.git`: ASCII case; trailing dots and
/// spaces, a `:` stream or a `\` separator (NTFS); the 8.3 short name of a
/// dot-name (`HEDDLE~1` for `.heddle`); or HFS+ ignorable code points.
fn is_reserved_alias(component: &[u8], name: &[u8]) -> bool {
    let ntfs_tail = |rest: &[u8]| {
        let mut index = 0;
        loop {
            match at(rest, index) {
                0 | b'/' | b'\\' | b':' => return true,
                b'.' | b' ' => index += 1,
                _ => return false,
            }
        }
    };
    if strncasecmp_eq(component, name, name.len()) && ntfs_tail(tail(component, name.len())) {
        return true;
    }
    if let Some(stem) = name.strip_prefix(b".")
        && !stem.is_empty()
    {
        let short = &stem[..stem.len().min(6)];
        if strncasecmp_eq(component, short, short.len())
            && at(component, short.len()) == b'~'
            && (b'1'..=b'4').contains(&at(component, short.len() + 1))
            && ntfs_tail(tail(component, short.len() + 2))
        {
            return true;
        }
    }
    // HFS+: compare with ignorable code points dropped.
    let mut cursor = Some(0usize);
    for expected in name {
        let c = next_hfs_char(component, &mut cursor);
        if c > 127 || !(c as u8).eq_ignore_ascii_case(expected) {
            return false;
        }
    }
    let c = next_hfs_char(component, &mut cursor);
    c == 0 || c == u32::from(b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(path: &str, mode: u32) -> bool {
        WorktreePathPolicy::default().is_valid_path(path.as_bytes(), mode)
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
            assert!(valid(path, 0o100644), "{path:?}");
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
                assert!(valid(path, 0o100644), "{path:?} as a file");
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
