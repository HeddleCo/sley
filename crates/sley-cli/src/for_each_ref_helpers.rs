//! Shared `for-each-ref` / `refs list` helpers (format rendering, sort keys, upstream tracking).
#![allow(clippy::expect_used)]

use crate::commands;
use crate::{
    GitConfig, GitError, ObjectFormat, ObjectId, RefTarget, Result, parse_refspec, remote_exists,
    remote_names, repository_objects_dir, resolve_revision, sley_rev, worktree_root_for_git_dir,
    write_object_id_hex,
};
use sley::plumbing::sley_core::DateMode;
use sley::plumbing::sley_object::{Commit, EncodedObject, ObjectType, Tag};
use sley::plumbing::sley_odb::FileObjectDatabase;
use sley::plumbing::sley_odb::ObjectReader;
use sley::plumbing::sley_refs::{self, FileRefStore};
use sley_protocol::refspec_map_source;
use sley_ref_filter::{
    ForEachRefAtom, ForEachRefAtomIdentityPart, ForEachRefAtomIdentityRole, ForEachRefEmailMode,
    ForEachRefFormat, ForEachRefFormatSegment, ForEachRefNameFormat, ForEachRefNameSource,
    ForEachRefQuoteMode, ForEachRefStripDirection, ForEachRefTrack, for_each_ref_abbrev_oid,
    for_each_ref_copy_subject, for_each_ref_identity_date, for_each_ref_identity_email,
    for_each_ref_identity_name, for_each_ref_identity_timestamp, for_each_ref_lstrip_name,
    for_each_ref_message_parts, for_each_ref_rstrip_name, for_each_ref_sanitize_subject,
    for_each_ref_short_name, for_each_ref_track_short, parse_for_each_ref_abbrev_width,
    parse_for_each_ref_contents_lines_count, parse_for_each_ref_hex_color,
    parse_for_each_ref_strip_count, write_for_each_ref_format, write_for_each_ref_identity,
    write_for_each_ref_identity_date_mode, write_for_each_ref_identity_date_raw,
    write_for_each_ref_identity_email_mode, write_for_each_ref_identity_name,
    write_for_each_ref_track,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub(crate) struct ForEachRefIdentitySortField {
    pub(crate) source: ForEachRefIdentitySource,
    pub(crate) role: ForEachRefIdentityRole,
    pub(crate) part: ForEachRefIdentityPart,
}

#[derive(Clone, Copy)]
pub(crate) enum ForEachRefIdentitySource {
    Direct,
    Peeled,
}

#[derive(Clone, Copy)]
pub(crate) enum ForEachRefIdentityRole {
    Author,
    Committer,
    Tagger,
    Creator,
}

#[derive(Clone, Copy)]
pub(crate) enum ForEachRefIdentityPart {
    Full,
    Name,
    Email,
}

pub(crate) fn parse_for_each_ref_identity_sort(
    value: &str,
) -> Option<(ForEachRefIdentitySortField, bool)> {
    let (value, descending) = value
        .strip_prefix('-')
        .map(|value| (value, true))
        .unwrap_or((value, false));
    let (value, source) = value
        .strip_prefix('*')
        .map(|value| (value, ForEachRefIdentitySource::Peeled))
        .unwrap_or((value, ForEachRefIdentitySource::Direct));
    let (role, part) = match value {
        "author" => (ForEachRefIdentityRole::Author, ForEachRefIdentityPart::Full),
        "authorname" => (ForEachRefIdentityRole::Author, ForEachRefIdentityPart::Name),
        "authoremail" => (
            ForEachRefIdentityRole::Author,
            ForEachRefIdentityPart::Email,
        ),
        "committer" => (
            ForEachRefIdentityRole::Committer,
            ForEachRefIdentityPart::Full,
        ),
        "committername" => (
            ForEachRefIdentityRole::Committer,
            ForEachRefIdentityPart::Name,
        ),
        "committeremail" => (
            ForEachRefIdentityRole::Committer,
            ForEachRefIdentityPart::Email,
        ),
        "tagger" => (ForEachRefIdentityRole::Tagger, ForEachRefIdentityPart::Full),
        "taggername" => (ForEachRefIdentityRole::Tagger, ForEachRefIdentityPart::Name),
        "taggeremail" => (
            ForEachRefIdentityRole::Tagger,
            ForEachRefIdentityPart::Email,
        ),
        "creator" => (
            ForEachRefIdentityRole::Creator,
            ForEachRefIdentityPart::Full,
        ),
        _ => return None,
    };
    Some((
        ForEachRefIdentitySortField { source, role, part },
        descending,
    ))
}

pub(crate) fn for_each_ref_sort_identity_key(
    contents: Option<&ForEachRefContents<'_>>,
    field: ForEachRefIdentitySortField,
) -> String {
    let identity = match field.role {
        ForEachRefIdentityRole::Author => contents.and_then(|contents| contents.author.as_deref()),
        ForEachRefIdentityRole::Committer => {
            contents.and_then(|contents| contents.committer.as_deref())
        }
        ForEachRefIdentityRole::Tagger => contents.and_then(|contents| contents.tagger.as_deref()),
        ForEachRefIdentityRole::Creator => {
            contents.and_then(|contents| contents.creator.as_deref())
        }
    };
    let value = match field.part {
        ForEachRefIdentityPart::Full => identity,
        ForEachRefIdentityPart::Name => identity.and_then(for_each_ref_identity_name),
        ForEachRefIdentityPart::Email => identity.and_then(|identity| {
            for_each_ref_identity_email(identity, ForEachRefEmailMode::Bracketed)
        }),
    };
    value
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .unwrap_or_default()
}

// states: S_N normal, S_I integral part, S_F fractional parts, S_Z idem but
// leading zeroes only (from glibc strverscmp, as in git's versioncmp.c).
const VS_S_N: usize = 0x0;
const VS_S_I: usize = 0x3;
const VS_S_F: usize = 0x6;
const VS_S_Z: usize = 0x9;
// result_type sentinels: CMP return diff, LEN compare via len_diff/diff.
const VS_CMP: i8 = 2;
const VS_LEN: i8 = 3;

#[rustfmt::skip]
const VS_NEXT_STATE: [usize; 12] = [
    /* state    x    d    0  */
    /* S_N */  VS_S_N, VS_S_I, VS_S_Z,
    /* S_I */  VS_S_N, VS_S_I, VS_S_I,
    /* S_F */  VS_S_N, VS_S_F, VS_S_F,
    /* S_Z */  VS_S_N, VS_S_F, VS_S_Z,
];

#[rustfmt::skip]
const VS_RESULT_TYPE: [i8; 36] = [
    /* state   x/x  x/d  x/0  d/x  d/d  d/0  0/x  0/d  0/0  */
    /* S_N */  VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_LEN, VS_CMP, VS_CMP, VS_CMP, VS_CMP,
    /* S_I */  VS_CMP, -1,     -1,     1,      VS_LEN, VS_LEN, 1,      VS_LEN, VS_LEN,
    /* S_F */  VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP,
    /* S_Z */  VS_CMP, 1,      1,      -1,     VS_CMP, VS_CMP, -1,     VS_CMP, VS_CMP,
];

#[inline]
pub(crate) fn vs_digit_class(c: u8) -> usize {
    // 0 if not a digit, 1 if digit 1-9, 2 if '0' (matches git's
    // (c=='0') + (isdigit(c) != 0)).
    (c == b'0') as usize + c.is_ascii_digit() as usize
}

pub(crate) struct VsSuffixMatch {
    conf_pos: i64,
    start: usize,
    len: i64,
}

pub(crate) fn vs_find_better_matching_suffix(
    tagname: &[u8],
    suffix: &[u8],
    start: usize,
    conf_pos: usize,
    m: &mut VsSuffixMatch,
) {
    // A better match either starts earlier, or at the same offset but longer.
    let end = if m.len < suffix.len() as i64 {
        m.start
    } else {
        m.start.saturating_sub(1)
    };
    for i in start..=end {
        if tagname.len() >= i && tagname[i..].starts_with(suffix) {
            m.conf_pos = conf_pos as i64;
            m.start = i;
            m.len = suffix.len() as i64;
            break;
        }
    }
}

/// Port of git's swap_prereleases(). `off` is the offset of the first
/// differing character. Returns Some(diff) if a prerelease suffix forces an
/// order.
pub(crate) fn vs_swap_prereleases(
    s1: &[u8],
    s2: &[u8],
    off: usize,
    prereleases: &[String],
) -> Option<std::cmp::Ordering> {
    let mut m1 = VsSuffixMatch {
        conf_pos: -1,
        start: off,
        len: -1,
    };
    let mut m2 = VsSuffixMatch {
        conf_pos: -1,
        start: off,
        len: -1,
    };
    for (i, suffix) in prereleases.iter().enumerate() {
        let suffix = suffix.as_bytes();
        let suffix_len = suffix.len();
        let start = if suffix_len < off {
            off - suffix_len
        } else {
            0
        };
        vs_find_better_matching_suffix(s1, suffix, start, i, &mut m1);
        vs_find_better_matching_suffix(s2, suffix, start, i, &mut m2);
    }
    if m1.conf_pos == -1 && m2.conf_pos == -1 {
        return None;
    }
    if m1.conf_pos == m2.conf_pos {
        // Same suffix in both: caller decides by the rest.
        return None;
    }
    let ord = if m1.conf_pos >= 0 && m2.conf_pos >= 0 {
        m1.conf_pos.cmp(&m2.conf_pos)
    } else if m1.conf_pos >= 0 {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Greater
    };
    Some(ord)
}

/// Faithful port of git's versioncmp() (glibc strverscmp + prerelease swap).
pub(crate) fn version_sort_cmp(s1: &str, s2: &str, prereleases: &[String]) -> std::cmp::Ordering {
    let b1 = s1.as_bytes();
    let b2 = s2.as_bytes();
    // Iterate with a sentinel NUL so we faithfully follow git's pointer walk.
    let get1 = |i: usize| -> u8 { if i < b1.len() { b1[i] } else { 0 } };
    let get2 = |i: usize| -> u8 { if i < b2.len() { b2[i] } else { 0 } };

    if std::ptr::eq(b1.as_ptr(), b2.as_ptr()) && b1.len() == b2.len() {
        return std::cmp::Ordering::Equal;
    }

    let mut p1 = 0usize;
    let mut p2 = 0usize;
    let mut c1 = get1(p1);
    let mut c2 = get2(p2);
    p1 += 1;
    p2 += 1;
    let mut state = VS_S_N + vs_digit_class(c1);

    let diff = loop {
        let d = c1 as i32 - c2 as i32;
        if d != 0 {
            break d;
        }
        if c1 == 0 {
            return std::cmp::Ordering::Equal;
        }
        state = VS_NEXT_STATE[state];
        c1 = get1(p1);
        c2 = get2(p2);
        p1 += 1;
        p2 += 1;
        state += vs_digit_class(c1);
    };

    // off is the index of the first differing character: pointer is one past it.
    if !prereleases.is_empty()
        && let Some(ord) = vs_swap_prereleases(b1, b2, p1 - 1, prereleases)
    {
        return ord;
    }

    let result = VS_RESULT_TYPE[state * 3 + vs_digit_class(c2)];
    match result {
        VS_CMP => diff.cmp(&0),
        VS_LEN => {
            // while (isdigit(*p1++)) if (!isdigit(*p2++)) return 1;
            loop {
                let d1 = get1(p1).is_ascii_digit();
                p1 += 1;
                if !d1 {
                    break;
                }
                let d2 = get2(p2).is_ascii_digit();
                p2 += 1;
                if !d2 {
                    return std::cmp::Ordering::Greater;
                }
            }
            if get2(p2).is_ascii_digit() {
                std::cmp::Ordering::Less
            } else {
                diff.cmp(&0)
            }
        }
        other => (other as i32).cmp(&0),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ForEachRefDateSortField {
    Author,
    Committer,
    Tagger,
    Creator,
}

pub(crate) fn for_each_ref_sort_date_key(
    contents: Option<ForEachRefContents<'_>>,
    field: ForEachRefDateSortField,
) -> i128 {
    let contents = contents.as_ref();
    let identity = match field {
        ForEachRefDateSortField::Author => contents.and_then(|contents| contents.author.as_deref()),
        ForEachRefDateSortField::Committer => {
            contents.and_then(|contents| contents.committer.as_deref())
        }
        ForEachRefDateSortField::Tagger => contents.and_then(|contents| contents.tagger.as_deref()),
        ForEachRefDateSortField::Creator => {
            contents.and_then(|contents| contents.creator.as_deref())
        }
    };
    identity
        .and_then(for_each_ref_identity_timestamp)
        .map(i128::from)
        .unwrap_or(0)
}

pub(crate) fn resolve_for_each_ref_target(
    store: &FileRefStore,
    reference: &sley_refs::Ref,
) -> Result<Option<(ObjectId, Option<String>)>> {
    let mut target = reference.target.clone();
    let mut symref = None;
    for _ in 0..5 {
        match target {
            RefTarget::Direct(oid) => return Ok(Some((oid, symref))),
            RefTarget::Symbolic(name) => {
                symref.get_or_insert_with(|| name.clone());
                if sley_refs::validate_ref_name(&name).is_err() {
                    return Ok(None);
                }
                let Some(next) = store.read_ref(&name)? else {
                    return Ok(None);
                };
                target = next;
            }
        }
    }
    Ok(None)
}

pub(crate) fn for_each_ref_loose_object_disk_size(
    git_dir: &Path,
    oid: &ObjectId,
) -> Result<Option<u64>> {
    let hex = oid.to_hex();
    if hex.len() < 2 {
        return Ok(None);
    }
    let (fanout, file) = hex.split_at(2);
    let path = repository_objects_dir(git_dir).join(fanout).join(file);
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn for_each_ref_worktree_path(
    git_dir: &Path,
    head_ref: Option<&str>,
    refname: &str,
) -> Result<Option<String>> {
    if head_ref == Some(refname)
        && let Ok(worktree_root) = worktree_root_for_git_dir(git_dir)
    {
        return Ok(Some(
            fs::canonicalize(worktree_root)?
                .to_string_lossy()
                .into_owned(),
        ));
    }

    let worktrees_dir = git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(None);
    };
    for entry in entries {
        let entry = entry?;
        let admin_dir = entry.path();
        let Ok(head) = fs::read_to_string(admin_dir.join("HEAD")) else {
            continue;
        };
        if head.trim().strip_prefix("ref: ") != Some(refname) {
            continue;
        }
        let Ok(gitdir) = fs::read_to_string(admin_dir.join("gitdir")) else {
            continue;
        };
        let gitdir = gitdir.trim();
        if gitdir.is_empty() {
            continue;
        }
        let gitdir_path = PathBuf::from(gitdir);
        let gitdir_path = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            admin_dir.join(gitdir_path)
        };
        if let Some(worktree_root) = gitdir_path.parent() {
            return Ok(Some(
                fs::canonicalize(worktree_root)?
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

/// Resolve every `refname -> checked-out worktree path` mapping in a single pass,
/// so `for-each-ref` need not re-scan `$GIT_DIR/worktrees` once per ref. Mirrors
/// the per-ref logic in `for_each_ref_worktree_path`: the current branch maps to
/// the main worktree root, and each linked worktree's `HEAD`/`gitdir` admin files
/// name the ref it has checked out and where its working tree lives.
pub(crate) fn for_each_ref_worktree_paths(
    git_dir: &Path,
    head_ref: Option<&str>,
) -> Result<HashMap<String, String>> {
    let mut paths = HashMap::new();
    if let Some(head_ref) = head_ref
        && let Ok(worktree_root) = worktree_root_for_git_dir(git_dir)
    {
        let canonical = fs::canonicalize(worktree_root)?;
        paths.insert(
            head_ref.to_string(),
            canonical.to_string_lossy().into_owned(),
        );
    }

    let worktrees_dir = git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(paths);
    };
    for entry in entries {
        let entry = entry?;
        let admin_dir = entry.path();
        let Ok(head) = fs::read_to_string(admin_dir.join("HEAD")) else {
            continue;
        };
        let Some(refname) = head.trim().strip_prefix("ref: ") else {
            continue;
        };
        // The current branch's mapping (the main worktree root) takes precedence
        // and is already inserted above.
        if paths.contains_key(refname) {
            continue;
        }
        let Ok(gitdir) = fs::read_to_string(admin_dir.join("gitdir")) else {
            continue;
        };
        let gitdir = gitdir.trim();
        if gitdir.is_empty() {
            continue;
        }
        let gitdir_path = PathBuf::from(gitdir);
        let gitdir_path = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            admin_dir.join(gitdir_path)
        };
        if let Some(worktree_root) = gitdir_path.parent() {
            let canonical = fs::canonicalize(worktree_root)?;
            paths.insert(
                refname.to_string(),
                canonical.to_string_lossy().into_owned(),
            );
        }
    }
    Ok(paths)
}

#[derive(Clone)]
pub(crate) struct ForEachRefUpstream {
    pub(crate) refname: String,
    pub(crate) remote: String,
    pub(crate) merge: String,
}

#[derive(Clone)]
pub(crate) struct ForEachRefPush {
    pub(crate) refname: Option<String>,
    pub(crate) remote: String,
    pub(crate) remote_ref: Option<String>,
}

pub(crate) struct ForEachRefPushRemote {
    name: String,
    expose_name: bool,
}

pub(crate) fn for_each_ref_upstream(
    config: &GitConfig,
    refname: &str,
) -> Option<ForEachRefUpstream> {
    let branch = refname.strip_prefix("refs/heads/")?;
    let remote = config.get("branch", Some(branch), "remote")?;
    let merge = config.get("branch", Some(branch), "merge")?;
    if remote == "." {
        return Some(ForEachRefUpstream {
            refname: merge.to_string(),
            remote: remote.to_string(),
            merge: merge.to_string(),
        });
    }
    let fetch = config.get("remote", Some(remote), "fetch")?;
    Some(ForEachRefUpstream {
        refname: map_remote_fetch_refspec(fetch, merge)?,
        remote: remote.to_string(),
        merge: merge.to_string(),
    })
}

pub(crate) fn for_each_ref_push(config: &GitConfig, refname: &str) -> Option<ForEachRefPush> {
    let branch = refname.strip_prefix("refs/heads/")?;
    let push_remote = for_each_ref_push_remote(config, branch)?;
    let remote_name = push_remote.name.clone();
    // The display name is exposed by `%(push:remotename)` even when the push
    // destination itself does not resolve, so compute it up front and keep it
    // on every return path (git's branch_get_push reports the remote regardless).
    let display_remote = remote_display_name(push_remote);
    if remote_name == "." {
        return Some(ForEachRefPush {
            refname: None,
            remote: display_remote,
            remote_ref: None,
        });
    }
    // An explicit push refspec (remote.<name>.push) takes precedence over
    // push.default — mirrors `remote->push.nr` in git's branch_get_push_1.
    if let Some(push) = config.get("remote", Some(remote_name.as_str()), "push") {
        if let Some(remote_ref) = map_remote_push_refspec(push, refname) {
            let tracking = map_remote_tracking_ref(config, &remote_name, &remote_ref);
            return Some(ForEachRefPush {
                refname: tracking,
                remote: display_remote,
                remote_ref: Some(remote_ref),
            });
        }
        return Some(ForEachRefPush {
            refname: None,
            remote: display_remote,
            remote_ref: None,
        });
    }
    // Otherwise resolve the destination through push.default, exactly as
    // git's branch_get_push_1 switch does.
    let push_default = config.get("push", None, "default").unwrap_or("simple");
    let tracking = match push_default {
        "nothing" => None,
        // matching/current push the branch's own ref through the push remote's
        // fetch refspec (tracking_for_push_dest on branch->refname).
        "matching" | "current" => map_remote_tracking_ref(config, &remote_name, refname),
        // upstream uses the branch's configured upstream destination.
        "upstream" => for_each_ref_upstream(config, refname).map(|up| up.refname),
        // simple/unspecified (the default): the push destination must equal the
        // upstream destination, otherwise there is no single 'simple' target and
        // %(push) is empty (the remote name is still reported).
        _ => {
            let up = for_each_ref_upstream(config, refname).map(|up| up.refname);
            let cur = map_remote_tracking_ref(config, &remote_name, refname);
            match (up, cur) {
                (Some(up), Some(cur)) if up == cur => Some(cur),
                _ => None,
            }
        }
    };
    Some(ForEachRefPush {
        refname: tracking,
        remote: display_remote,
        remote_ref: None,
    })
}

pub(crate) fn for_each_ref_push_remote(
    config: &GitConfig,
    branch: &str,
) -> Option<ForEachRefPushRemote> {
    if let Some(remote) = config.get("branch", Some(branch), "pushRemote") {
        return Some(ForEachRefPushRemote {
            name: remote.to_string(),
            expose_name: true,
        });
    }
    if let Some(remote) = config.get("remote", None, "pushDefault") {
        return Some(ForEachRefPushRemote {
            name: remote.to_string(),
            expose_name: true,
        });
    }
    if let Some(remote) = config.get("branch", Some(branch), "remote") {
        return Some(ForEachRefPushRemote {
            name: remote.to_string(),
            expose_name: true,
        });
    }
    if remote_exists(config, "origin") {
        return Some(ForEachRefPushRemote {
            name: "origin".to_string(),
            expose_name: false,
        });
    }
    let remotes = remote_names(config);
    match remotes.as_slice() {
        [remote] => Some(ForEachRefPushRemote {
            name: remote.clone(),
            expose_name: false,
        }),
        _ => None,
    }
}

pub(crate) fn remote_display_name(remote: ForEachRefPushRemote) -> String {
    if remote.expose_name {
        remote.name.to_string()
    } else {
        String::new()
    }
}

pub(crate) fn map_remote_tracking_ref(
    config: &GitConfig,
    remote: &str,
    remote_ref: &str,
) -> Option<String> {
    let fetch = config.get("remote", Some(remote), "fetch")?;
    map_remote_fetch_refspec(fetch, remote_ref)
}

pub(crate) fn map_remote_push_refspec(refspec: &str, refname: &str) -> Option<String> {
    let refspec = parse_refspec(refspec).ok()?;
    if refspec.negative || refspec.src.is_none() || refspec.dst.is_none() {
        return None;
    }
    refspec_map_source(&refspec, refname).ok()?
}

pub(crate) fn map_remote_fetch_refspec(refspec: &str, merge: &str) -> Option<String> {
    let refspec = parse_refspec(refspec).ok()?;
    if refspec.negative || refspec.dst.is_none() {
        return None;
    }
    refspec_map_source(&refspec, merge).ok()?
}

pub(crate) fn for_each_ref_upstream_track(
    store: &FileRefStore,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    upstream: &str,
) -> Result<Option<ForEachRefTrack>> {
    // git: a configured-but-unresolvable upstream reports `[gone]`, distinct
    // from "no upstream configured" (which the caller already filtered out).
    let gone_track = ForEachRefTrack {
        ahead: 0,
        behind: 0,
        gone: true,
    };
    let Some(upstream_target) = store.read_ref(upstream)? else {
        return Ok(Some(gone_track));
    };
    let upstream_ref = sley_refs::Ref {
        name: upstream.to_string(),
        target: upstream_target,
    };
    let Some((upstream_oid, _)) = resolve_for_each_ref_target(store, &upstream_ref)? else {
        return Ok(Some(gone_track));
    };
    for_each_ref_ahead_behind(git_dir, db, format, oid, &upstream_oid)
}

pub(crate) fn for_each_ref_ahead_behind_with_diagnostic(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    target: &ObjectId,
) -> Result<Option<ForEachRefTrack>> {
    let Ok(local_commit) = sley_rev::peel_to_commit(db, format, oid) else {
        if let Ok(object) = db.read_object(oid) {
            eprintln!(
                "error: object {} is a {}, not a commit",
                oid,
                object.object_type.as_str()
            );
        }
        return Ok(None);
    };
    let Ok(target_commit) = sley_rev::peel_to_commit(db, format, target) else {
        return Ok(None);
    };
    let (ahead, behind) =
        sley_rev::ahead_behind_counts(git_dir, format, db, &local_commit, &target_commit)?;
    Ok(Some(ForEachRefTrack {
        ahead,
        behind,
        gone: false,
    }))
}

pub(crate) fn for_each_ref_ahead_behind(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    target: &ObjectId,
) -> Result<Option<ForEachRefTrack>> {
    let Ok(local_commit) = sley_rev::peel_to_commit(db, format, oid) else {
        return Ok(None);
    };
    let Ok(target_commit) = sley_rev::peel_to_commit(db, format, target) else {
        return Ok(None);
    };
    let (ahead, behind) =
        sley_rev::ahead_behind_counts(git_dir, format, db, &local_commit, &target_commit)?;
    Ok(Some(ForEachRefTrack {
        ahead,
        behind,
        gone: false,
    }))
}

pub(crate) struct ForEachRefContents<'a> {
    pub(crate) message: Cow<'a, [u8]>,
    pub(crate) tree: Option<ObjectId>,
    pub(crate) parents: Vec<ObjectId>,
    pub(crate) tag: Option<Cow<'a, [u8]>>,
    pub(crate) tag_object_type: Option<ObjectType>,
    pub(crate) tag_object: Option<ObjectId>,
    pub(crate) author: Option<Cow<'a, [u8]>>,
    pub(crate) committer: Option<Cow<'a, [u8]>>,
    pub(crate) tagger: Option<Cow<'a, [u8]>>,
    pub(crate) creator: Option<Cow<'a, [u8]>>,
}

impl ForEachRefContents<'_> {
    pub(crate) fn into_owned(self) -> ForEachRefContents<'static> {
        ForEachRefContents {
            message: Cow::Owned(self.message.into_owned()),
            tree: self.tree,
            parents: self.parents,
            tag: self.tag.map(|tag| Cow::Owned(tag.into_owned())),
            tag_object_type: self.tag_object_type,
            tag_object: self.tag_object,
            author: self.author.map(|author| Cow::Owned(author.into_owned())),
            committer: self
                .committer
                .map(|committer| Cow::Owned(committer.into_owned())),
            tagger: self.tagger.map(|tagger| Cow::Owned(tagger.into_owned())),
            creator: self.creator.map(|creator| Cow::Owned(creator.into_owned())),
        }
    }
}

pub(crate) fn for_each_ref_contents<'a>(
    format: ObjectFormat,
    object: &'a EncodedObject,
) -> Result<Option<ForEachRefContents<'a>>> {
    let contents = match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse_ref(format, &object.body)?;
            ForEachRefContents {
                message: Cow::Borrowed(commit.message),
                tree: Some(commit.tree),
                parents: commit.parents,
                tag: None,
                tag_object_type: None,
                tag_object: None,
                author: Some(Cow::Borrowed(commit.author)),
                committer: Some(Cow::Borrowed(commit.committer)),
                tagger: None,
                creator: Some(Cow::Borrowed(commit.committer)),
            }
        }
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            ForEachRefContents {
                message: Cow::Borrowed(tag.message),
                tree: None,
                parents: Vec::new(),
                tag: Some(Cow::Borrowed(tag.name)),
                tag_object_type: Some(tag.object_type),
                tag_object: Some(tag.object),
                author: None,
                committer: None,
                tagger: tag.tagger.map(Cow::Borrowed),
                creator: tag.tagger.map(Cow::Borrowed),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(contents))
}

pub(crate) fn for_each_ref_validate_tag_pointer(
    tag_oid: &ObjectId,
    contents: &ForEachRefContents<'_>,
    target_oid: &ObjectId,
    target: &EncodedObject,
) -> Result<()> {
    if contents
        .tag_object_type
        .is_some_and(|object_type| object_type != target.object_type)
    {
        eprintln!("error: bad tag pointer to {target_oid} in {tag_oid}");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

pub(crate) struct ForEachRefFormatContext<'a> {
    pub(crate) git_dir: &'a Path,
    pub(crate) db: &'a FileObjectDatabase,
    pub(crate) format: ObjectFormat,
    pub(crate) refname: &'a str,
    pub(crate) oid: &'a ObjectId,
    pub(crate) deltabase: &'a ObjectId,
    pub(crate) object_type: ObjectType,
    pub(crate) object_body: &'a [u8],
    pub(crate) object_size: usize,
    pub(crate) object_disk_size: Option<u64>,
    pub(crate) color: bool,
    pub(crate) quote: ForEachRefQuoteMode,
    pub(crate) objectname_abbrev: Option<usize>,
    pub(crate) objectname_candidates: &'a [ObjectId],
    pub(crate) worktree_path: Option<&'a str>,
    pub(crate) is_head: bool,
    pub(crate) symref: Option<&'a str>,
    pub(crate) upstream: Option<ForEachRefUpstream>,
    pub(crate) push: Option<ForEachRefPush>,
    pub(crate) upstream_track: Option<ForEachRefTrack>,
    pub(crate) push_track: Option<ForEachRefTrack>,
    pub(crate) contents: Option<ForEachRefContents<'a>>,
    pub(crate) peeled_object: Option<ForEachRefPeeledObject<'a>>,
    // %(signature*) verification of the ref object and its peeled tag target.
    pub(crate) signature: Option<commands::signing::GpgVerification>,
    pub(crate) peeled_signature: Option<commands::signing::GpgVerification>,
    pub(crate) mailmap: &'a commands::utility::Mailmap,
    // All ref names in the store + `core.warnambiguousrefs`, for the
    // `:short` atoms' shorten_unambiguous_ref resolution.
    pub(crate) ref_names: &'a std::collections::HashSet<String>,
    pub(crate) warn_ambiguous_refs: bool,
}

impl ForEachRefFormatContext<'_> {
    /// Shorten a fully-qualified refname to its unambiguous abbreviation, the
    /// way git's `%(refname:short)` / `%(symref:short)` / `%(upstream:short)` do.
    fn shorten_ref(&self, refname: &str) -> String {
        sley_ref_filter::shorten_unambiguous_ref(refname, self.warn_ambiguous_refs, |candidate| {
            self.ref_names.contains(candidate)
        })
    }
}

pub(crate) struct ForEachRefPeeledObject<'a> {
    pub(crate) oid: ObjectId,
    pub(crate) object_type: ObjectType,
    pub(crate) object_body: Cow<'a, [u8]>,
    pub(crate) object_size: usize,
    pub(crate) object_disk_size: Option<u64>,
    pub(crate) tree: Option<ObjectId>,
    pub(crate) parents: Vec<ObjectId>,
    pub(crate) message: Option<Cow<'a, [u8]>>,
    pub(crate) author: Option<Cow<'a, [u8]>>,
    pub(crate) committer: Option<Cow<'a, [u8]>>,
    pub(crate) creator: Option<Cow<'a, [u8]>>,
}

/// Emit one `%(signature[:opt])` (or `%(*signature[:opt])`) sub-field from a
/// verified signature, mirroring git's `grab_signature` field mapping. `option`
/// is the placeholder text after `signature` — `""` for the bare atom, or
/// `":grade"`, `":key"`, … for the typed sub-fields.
pub(crate) fn write_for_each_ref_signature(
    stdout: &mut impl Write,
    verification: &commands::signing::GpgVerification,
    option: &str,
) -> Result<()> {
    match option.strip_prefix(':').unwrap_or("") {
        // The bare atom prints gpg's human-readable verification output.
        "" => stdout.write_all(&commands::signing::bare_signature_output(verification))?,
        // grade: 'G'/'U'/'B'/'E'/'N' — git downgrades a good-but-untrusted
        // signature to 'U', which pretty_code already encodes.
        "grade" => stdout.write_all(&[verification.pretty_code()])?,
        "key" => stdout.write_all(verification.key.as_bytes())?,
        "signer" => stdout.write_all(verification.signer.as_bytes())?,
        "fingerprint" => stdout.write_all(verification.fingerprint.as_bytes())?,
        "primarykeyfingerprint" => stdout.write_all(verification.primary_fingerprint.as_bytes())?,
        "trustlevel" => stdout.write_all(verification.trust.as_bytes())?,
        _ => {}
    }
    Ok(())
}

pub(crate) fn print_for_each_ref_format(
    stdout: &mut impl Write,
    format_spec: &ForEachRefFormat,
    context: &ForEachRefFormatContext<'_>,
) -> Result<()> {
    print_for_each_ref_format_with_is_bases(stdout, format_spec, context, &HashMap::new())
}

pub(crate) fn print_for_each_ref_format_with_is_bases(
    stdout: &mut impl Write,
    format_spec: &ForEachRefFormat,
    context: &ForEachRefFormatContext<'_>,
    is_base_refs: &HashMap<String, String>,
) -> Result<()> {
    let reset_color_at_eol = context.color && format_spec.ends_with_unreset_color();
    write_for_each_ref_format(
        stdout,
        format_spec,
        context.quote,
        reset_color_at_eol,
        |stdout, atom| {
            let placeholder = match atom {
                ForEachRefAtom::Raw(placeholder) => placeholder.as_str(),
                atom => {
                    write_for_each_ref_typed_atom(stdout, atom, context)?;
                    return Ok(());
                }
            };
            match placeholder {
                "HEAD" => stdout.write_all(if context.is_head { b"*" } else { b" " })?,
                "refname" => stdout.write_all(context.refname.as_bytes())?,
                "refname:short" => {
                    stdout.write_all(context.shorten_ref(context.refname).as_bytes())?
                }
                "objectname" => write!(stdout, "{}", context.oid)?,
                "objectname:short" => stdout.write_all(
                    for_each_ref_abbrev_oid(
                        context.oid,
                        context.objectname_abbrev,
                        context.objectname_candidates,
                    )
                    .as_bytes(),
                )?,
                "*objectname" => {
                    if let Some(peeled) = &context.peeled_object {
                        write!(stdout, "{}", peeled.oid)?;
                    }
                }
                "*objectname:short" => {
                    if let Some(peeled) = &context.peeled_object {
                        stdout.write_all(
                            for_each_ref_abbrev_oid(
                                &peeled.oid,
                                context.objectname_abbrev,
                                context.objectname_candidates,
                            )
                            .as_bytes(),
                        )?;
                    }
                }
                "deltabase" => write!(stdout, "{}", context.deltabase)?,
                "*deltabase" => {
                    if context.peeled_object.is_some() {
                        write!(stdout, "{}", context.deltabase)?;
                    }
                }
                "raw" => stdout.write_all(context.object_body)?,
                "raw:size" => write!(stdout, "{}", context.object_body.len())?,
                "*raw" => {
                    if let Some(peeled) = &context.peeled_object {
                        stdout.write_all(&peeled.object_body)?;
                    }
                }
                "*raw:size" => {
                    if let Some(peeled) = &context.peeled_object {
                        write!(stdout, "{}", peeled.object_body.len())?;
                    }
                }
                "objectsize" => write!(stdout, "{}", context.object_size)?,
                "*objectsize" => {
                    if let Some(peeled) = &context.peeled_object {
                        write!(stdout, "{}", peeled.object_size)?;
                    }
                }
                "objectsize:disk" => {
                    if let Some(size) = context.object_disk_size {
                        write!(stdout, "{size}")?;
                    }
                }
                "*objectsize:disk" => {
                    if let Some(size) = context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.object_disk_size)
                    {
                        write!(stdout, "{size}")?;
                    }
                }
                "objecttype" => stdout.write_all(context.object_type.as_str().as_bytes())?,
                "*objecttype" => {
                    if let Some(peeled) = &context.peeled_object {
                        stdout.write_all(peeled.object_type.as_str().as_bytes())?;
                    }
                }
                "worktreepath" => {
                    stdout.write_all(context.worktree_path.unwrap_or("").as_bytes())?
                }
                "symref" => stdout.write_all(context.symref.unwrap_or("").as_bytes())?,
                "symref:short" => stdout.write_all(
                    context
                        .symref
                        .map(|symref| context.shorten_ref(symref))
                        .unwrap_or_default()
                        .as_bytes(),
                )?,
                "upstream" => stdout.write_all(
                    context
                        .upstream
                        .as_ref()
                        .map(|upstream| upstream.refname.as_str())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "upstream:short" => stdout.write_all(
                    context
                        .upstream
                        .as_ref()
                        .map(|upstream| context.shorten_ref(&upstream.refname))
                        .unwrap_or_default()
                        .as_bytes(),
                )?,
                "upstream:remotename" => stdout.write_all(
                    context
                        .upstream
                        .as_ref()
                        .map(|upstream| upstream.remote.as_str())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "upstream:remoteref" => stdout.write_all(
                    context
                        .upstream
                        .as_ref()
                        .map(|upstream| upstream.merge.as_str())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "upstream:track" => {
                    if let Some(track) = context.upstream_track {
                        write_for_each_ref_track(stdout, track, true)?;
                    }
                }
                "upstream:track,nobracket" | "upstream:nobracket,track" => {
                    if let Some(track) = context.upstream_track {
                        write_for_each_ref_track(stdout, track, false)?;
                    }
                }
                "upstream:trackshort" => {
                    if let Some(track) = context.upstream_track {
                        stdout.write_all(for_each_ref_track_short(track).as_bytes())?;
                    }
                }
                "push" => stdout.write_all(
                    context
                        .push
                        .as_ref()
                        .and_then(|push| push.refname.as_deref())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "push:short" => stdout.write_all(
                    context
                        .push
                        .as_ref()
                        .and_then(|push| push.refname.as_deref())
                        .map(|refname| context.shorten_ref(refname))
                        .unwrap_or_default()
                        .as_bytes(),
                )?,
                "push:remotename" => stdout.write_all(
                    context
                        .push
                        .as_ref()
                        .map(|push| push.remote.as_str())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "push:remoteref" => stdout.write_all(
                    context
                        .push
                        .as_ref()
                        .and_then(|push| push.remote_ref.as_deref())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "push:track" => {
                    if let Some(track) = context.push_track {
                        write_for_each_ref_track(stdout, track, true)?;
                    }
                }
                "push:track,nobracket" | "push:nobracket,track" => {
                    if let Some(track) = context.push_track {
                        write_for_each_ref_track(stdout, track, false)?;
                    }
                }
                "push:trackshort" => {
                    if let Some(track) = context.push_track {
                        stdout.write_all(for_each_ref_track_short(track).as_bytes())?;
                    }
                }
                "signature"
                | "signature:grade"
                | "signature:key"
                | "signature:signer"
                | "signature:fingerprint"
                | "signature:primarykeyfingerprint"
                | "signature:trustlevel" => {
                    if let Some(signature) = context.signature.as_ref() {
                        write_for_each_ref_signature(
                            stdout,
                            signature,
                            &placeholder["signature".len()..],
                        )?;
                    }
                }
                "*signature"
                | "*signature:grade"
                | "*signature:key"
                | "*signature:signer"
                | "*signature:fingerprint"
                | "*signature:primarykeyfingerprint"
                | "*signature:trustlevel" => {
                    if let Some(signature) = context.peeled_signature.as_ref() {
                        write_for_each_ref_signature(
                            stdout,
                            signature,
                            &placeholder["*signature".len()..],
                        )?;
                    }
                }
                "subject" | "contents:subject" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        let parts = for_each_ref_message_parts(message);
                        stdout.write_all(for_each_ref_copy_subject(parts.subject).as_bytes())?;
                    }
                }
                "*subject" | "*contents:subject" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        let parts = for_each_ref_message_parts(message);
                        stdout.write_all(for_each_ref_copy_subject(parts.subject).as_bytes())?;
                    }
                }
                "subject:sanitize" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        let parts = for_each_ref_message_parts(message);
                        let subject = for_each_ref_copy_subject(parts.subject);
                        stdout.write_all(for_each_ref_sanitize_subject(&subject).as_bytes())?;
                    }
                }
                "*subject:sanitize" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        let parts = for_each_ref_message_parts(message);
                        let subject = for_each_ref_copy_subject(parts.subject);
                        stdout.write_all(for_each_ref_sanitize_subject(&subject).as_bytes())?;
                    }
                }
                "contents:body" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        stdout.write_all(for_each_ref_message_parts(message).body_without_sig)?;
                    }
                }
                "*contents:body" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        stdout.write_all(for_each_ref_message_parts(message).body_without_sig)?;
                    }
                }
                "contents:signature" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        stdout.write_all(for_each_ref_message_parts(message).signature)?;
                    }
                }
                "*contents:signature" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        stdout.write_all(for_each_ref_message_parts(message).signature)?;
                    }
                }
                "body" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        stdout.write_all(for_each_ref_message_parts(message).body_with_sig)?;
                    }
                }
                "*body" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        stdout.write_all(for_each_ref_message_parts(message).body_with_sig)?;
                    }
                }
                "contents" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        stdout.write_all(for_each_ref_message_parts(message).bare)?;
                    }
                }
                "*contents" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        stdout.write_all(for_each_ref_message_parts(message).bare)?;
                    }
                }
                "contents:size" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        write!(stdout, "{}", for_each_ref_message_parts(message).bare.len())?;
                    }
                }
                "*contents:size" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        write!(stdout, "{}", for_each_ref_message_parts(message).bare.len())?;
                    }
                }
                "author" => write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.author.as_deref()),
                )?,
                "*author" => write_for_each_ref_identity(
                    stdout,
                    context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.author.as_deref()),
                )?,
                "authorname" | "*authorname" => {
                    for_each_ref_try_name_atom(stdout, placeholder, context)
                        .expect("name atom recognized")?
                }
                "authoremail" | "*authoremail" => {
                    for_each_ref_try_email_atom(stdout, placeholder, context)
                        .expect("email atom recognized")?
                }
                "committer" => write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.committer.as_deref()),
                )?,
                "*committer" => write_for_each_ref_identity(
                    stdout,
                    context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.committer.as_deref()),
                )?,
                "committername" | "*committername" => {
                    for_each_ref_try_name_atom(stdout, placeholder, context)
                        .expect("name atom recognized")?
                }
                "committeremail" | "*committeremail" => {
                    for_each_ref_try_email_atom(stdout, placeholder, context)
                        .expect("email atom recognized")?
                }
                "tagger" => write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tagger.as_deref()),
                )?,
                "*tagger" => write_for_each_ref_identity(stdout, None)?,
                "taggername" | "*taggername" => {
                    for_each_ref_try_name_atom(stdout, placeholder, context)
                        .expect("name atom recognized")?
                }
                "taggeremail" | "*taggeremail" => {
                    for_each_ref_try_email_atom(stdout, placeholder, context)
                        .expect("email atom recognized")?
                }
                "creator" => write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.creator.as_deref()),
                )?,
                "*creator" => write_for_each_ref_identity(
                    stdout,
                    context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.creator.as_deref()),
                )?,
                "authordate" | "*authordate" | "committerdate" | "*committerdate"
                | "taggerdate" | "*taggerdate" | "creatordate" | "*creatordate" => {
                    for_each_ref_try_date_atom(stdout, placeholder, context)
                        .expect("date atom recognized")?
                }
                "tree" => {
                    if let Some(tree) = context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tree.as_ref())
                    {
                        write!(stdout, "{tree}")?;
                    }
                }
                "parent" => {
                    if let Some(contents) = &context.contents {
                        for (idx, parent) in contents.parents.iter().enumerate() {
                            if idx > 0 {
                                stdout.write_all(b" ")?;
                            }
                            write!(stdout, "{parent}")?;
                        }
                    }
                }
                "numparent" => {
                    if let Some(contents) = &context.contents
                        && contents.tree.is_some()
                    {
                        write!(stdout, "{}", contents.parents.len())?;
                    }
                }
                "*tree" => {
                    if let Some(tree) = context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.tree.as_ref())
                    {
                        write!(stdout, "{tree}")?;
                    }
                }
                "*parent" => {
                    if let Some(peeled) = &context.peeled_object {
                        for (idx, parent) in peeled.parents.iter().enumerate() {
                            if idx > 0 {
                                stdout.write_all(b" ")?;
                            }
                            write!(stdout, "{parent}")?;
                        }
                    }
                }
                "*numparent" => {
                    if let Some(peeled) = &context.peeled_object
                        && peeled.tree.is_some()
                    {
                        write!(stdout, "{}", peeled.parents.len())?;
                    }
                }
                "tag" => {
                    if let Some(tag) = context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tag.as_ref())
                    {
                        stdout.write_all(tag)?;
                    }
                }
                "type" => {
                    if let Some(object_type) = context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tag_object_type)
                    {
                        stdout.write_all(object_type.as_str().as_bytes())?;
                    }
                }
                "object" => {
                    if let Some(object) = context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tag_object.as_ref())
                    {
                        write!(stdout, "{object}")?;
                    }
                }
                other => {
                    if let Some(value) = other.strip_prefix("color:") {
                        let color = for_each_ref_color_escape(value)?;
                        if context.color {
                            stdout.write_all(color.as_bytes())?;
                        }
                    } else if let Some(value) = other
                        .strip_prefix("refname:lstrip=")
                        .or_else(|| other.strip_prefix("refname:strip="))
                    {
                        let count = parse_for_each_ref_strip_count(value)?;
                        stdout.write_all(
                            for_each_ref_lstrip_name(context.refname, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other.strip_prefix("refname:rstrip=") {
                        let count = parse_for_each_ref_strip_count(value)?;
                        stdout.write_all(
                            for_each_ref_rstrip_name(context.refname, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other
                        .strip_prefix("upstream:lstrip=")
                        .or_else(|| other.strip_prefix("upstream:strip="))
                    {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let upstream = context
                            .upstream
                            .as_ref()
                            .map(|upstream| upstream.refname.as_str())
                            .unwrap_or("");
                        stdout.write_all(for_each_ref_lstrip_name(upstream, count).as_bytes())?;
                    } else if let Some(value) = other.strip_prefix("upstream:rstrip=") {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let upstream = context
                            .upstream
                            .as_ref()
                            .map(|upstream| upstream.refname.as_str())
                            .unwrap_or("");
                        stdout.write_all(for_each_ref_rstrip_name(upstream, count).as_bytes())?;
                    } else if let Some(value) = other
                        .strip_prefix("push:lstrip=")
                        .or_else(|| other.strip_prefix("push:strip="))
                    {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let push = context
                            .push
                            .as_ref()
                            .and_then(|push| push.refname.as_deref())
                            .unwrap_or("");
                        stdout.write_all(for_each_ref_lstrip_name(push, count).as_bytes())?;
                    } else if let Some(value) = other.strip_prefix("push:rstrip=") {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let push = context
                            .push
                            .as_ref()
                            .and_then(|push| push.refname.as_deref())
                            .unwrap_or("");
                        stdout.write_all(for_each_ref_rstrip_name(push, count).as_bytes())?;
                    } else if let Some(value) = other
                        .strip_prefix("symref:lstrip=")
                        .or_else(|| other.strip_prefix("symref:strip="))
                    {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let symref = context.symref.unwrap_or("");
                        stdout.write_all(for_each_ref_lstrip_name(symref, count).as_bytes())?;
                    } else if let Some(value) = other.strip_prefix("symref:rstrip=") {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let symref = context.symref.unwrap_or("");
                        stdout.write_all(for_each_ref_rstrip_name(symref, count).as_bytes())?;
                    } else if let Some(width) = other.strip_prefix("objectname:short=") {
                        let width = parse_for_each_ref_abbrev_width(width)?;
                        stdout.write_all(
                            for_each_ref_abbrev_oid(
                                context.oid,
                                Some(width),
                                context.objectname_candidates,
                            )
                            .as_bytes(),
                        )?;
                    } else if let Some(width) = other.strip_prefix("*objectname:short=") {
                        let width = parse_for_each_ref_abbrev_width(width)?;
                        if let Some(peeled) = &context.peeled_object {
                            stdout.write_all(
                                for_each_ref_abbrev_oid(
                                    &peeled.oid,
                                    Some(width),
                                    context.objectname_candidates,
                                )
                                .as_bytes(),
                            )?;
                        }
                    } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "tree") {
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
                        if let Some(tree) = context
                            .contents
                            .as_ref()
                            .and_then(|contents| contents.tree.as_ref())
                        {
                            stdout.write_all(
                                for_each_ref_abbrev_oid(tree, width, context.objectname_candidates)
                                    .as_bytes(),
                            )?;
                        }
                    } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "*tree") {
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
                        if let Some(tree) = context
                            .peeled_object
                            .as_ref()
                            .and_then(|peeled| peeled.tree.as_ref())
                        {
                            stdout.write_all(
                                for_each_ref_abbrev_oid(tree, width, context.objectname_candidates)
                                    .as_bytes(),
                            )?;
                        }
                    } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "parent") {
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
                        if let Some(contents) = &context.contents {
                            for (idx, parent) in contents.parents.iter().enumerate() {
                                if idx > 0 {
                                    stdout.write_all(b" ")?;
                                }
                                stdout.write_all(
                                    for_each_ref_abbrev_oid(
                                        parent,
                                        width,
                                        context.objectname_candidates,
                                    )
                                    .as_bytes(),
                                )?;
                            }
                        }
                    } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "*parent") {
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
                        if let Some(peeled) = &context.peeled_object {
                            for (idx, parent) in peeled.parents.iter().enumerate() {
                                if idx > 0 {
                                    stdout.write_all(b" ")?;
                                }
                                stdout.write_all(
                                    for_each_ref_abbrev_oid(
                                        parent,
                                        width,
                                        context.objectname_candidates,
                                    )
                                    .as_bytes(),
                                )?;
                            }
                        }
                    } else if let Some(result) =
                        for_each_ref_try_trailers_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(result) = for_each_ref_try_email_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(result) = for_each_ref_try_name_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(result) = for_each_ref_try_date_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(target) = other.strip_prefix("is-base:") {
                        if is_base_refs
                            .get(target)
                            .is_some_and(|refname| refname == context.refname)
                        {
                            write!(stdout, "({target})")?;
                        }
                    } else if let Some(rev) = other.strip_prefix("ahead-behind:") {
                        let target = resolve_revision(context.git_dir, context.format, rev)?;
                        if let Some(track) = for_each_ref_ahead_behind_with_diagnostic(
                            context.git_dir,
                            context.db,
                            context.format,
                            context.oid,
                            &target,
                        )? {
                            write!(stdout, "{} {}", track.ahead, track.behind)?;
                        }
                    } else if let Some(value) = other.strip_prefix("contents:lines=") {
                        let count = parse_for_each_ref_contents_lines_count(value)?;
                        if let Some(contents) = &context.contents {
                            write_for_each_ref_contents_lines(stdout, &contents.message, count)?;
                        }
                    } else if let Some(value) = other.strip_prefix("*contents:lines=") {
                        let count = parse_for_each_ref_contents_lines_count(value)?;
                        if let Some(message) = context
                            .peeled_object
                            .as_ref()
                            .and_then(|peeled| peeled.message.as_ref())
                        {
                            write_for_each_ref_contents_lines(stdout, message, count)?;
                        }
                    } else if let Some(arg) = other
                        .strip_prefix("contents:")
                        .or_else(|| other.strip_prefix("*contents:"))
                    {
                        // A `%(contents:XXX)` that none of the contents sub-atoms
                        // above recognized — git reports the bare contents arg.
                        eprintln!("fatal: unrecognized %(contents) argument: {arg}");
                        return Err(GitError::Exit(128));
                    } else if let Some((peeled, opts)) = for_each_ref_describe_atom(other) {
                        // %(describe[:opts]) / %(*describe[:opts]) reuse the same
                        // describe engine as log's %(describe); git treats describe
                        // failures as an empty placeholder.
                        let spec = for_each_ref_parse_describe_opts(opts)?;
                        let target = if peeled {
                            context.peeled_object.as_ref().map(|object| object.oid)
                        } else {
                            Some(*context.oid)
                        };
                        if let Some(target) = target
                            && let Some(text) = crate::commands::describe::describe_for_format(
                                context.git_dir,
                                context.format,
                                context.db,
                                &target,
                                spec.tags,
                                spec.abbrev,
                                &spec.matches,
                                &spec.excludes,
                            )?
                        {
                            stdout.write_all(text.as_bytes())?;
                        }
                    } else if other.starts_with("HEAD:") {
                        // git's head_atom_parser: %(HEAD) takes no arguments.
                        eprintln!("fatal: %(HEAD) does not take arguments");
                        return Err(GitError::Exit(128));
                    } else if let Some(arg) = other
                        .strip_prefix("subject:")
                        .or_else(|| other.strip_prefix("*subject:"))
                    {
                        // The only valid %(subject) arg is `sanitize` (matched
                        // above); anything else is rejected like git's
                        // subject_atom_parser.
                        eprintln!("fatal: unrecognized %(subject) argument: {arg}");
                        return Err(GitError::Exit(128));
                    } else {
                        return Err(GitError::Command(format!(
                            "unsupported for-each-ref format placeholder %({other})"
                        )));
                    }
                }
            }
            Ok(())
        },
    )
}

pub(crate) fn write_for_each_ref_typed_atom(
    stdout: &mut impl Write,
    atom: &ForEachRefAtom,
    context: &ForEachRefFormatContext<'_>,
) -> Result<()> {
    match atom {
        ForEachRefAtom::Raw(_) => unreachable!("raw atoms are handled by the compatibility path"),
        ForEachRefAtom::Color(value) => {
            let color = for_each_ref_color_escape(value)?;
            if context.color {
                stdout.write_all(color.as_bytes())?;
            }
        }
        ForEachRefAtom::RefName { source, format } => {
            let refname = for_each_ref_typed_refname(context, *source);
            match format {
                ForEachRefNameFormat::Full => stdout.write_all(refname.as_bytes())?,
                ForEachRefNameFormat::Short => {
                    stdout.write_all(context.shorten_ref(refname).as_bytes())?
                }
                ForEachRefNameFormat::Strip(strip) => {
                    let refname = match strip.direction {
                        ForEachRefStripDirection::Left => {
                            for_each_ref_lstrip_name(refname, strip.count)
                        }
                        ForEachRefStripDirection::Right => {
                            for_each_ref_rstrip_name(refname, strip.count)
                        }
                    };
                    stdout.write_all(refname.as_bytes())?;
                }
            }
        }
        ForEachRefAtom::ObjectName { peeled, abbrev } => {
            let oid = if *peeled {
                context.peeled_object.as_ref().map(|peeled| &peeled.oid)
            } else {
                Some(context.oid)
            };
            if let Some(oid) = oid {
                match abbrev {
                    None => write_object_id_hex(stdout, oid, None)?,
                    Some(0) => stdout.write_all(
                        for_each_ref_abbrev_oid(
                            oid,
                            context.objectname_abbrev,
                            context.objectname_candidates,
                        )
                        .as_bytes(),
                    )?,
                    Some(width) => stdout.write_all(
                        for_each_ref_abbrev_oid(oid, Some(*width), context.objectname_candidates)
                            .as_bytes(),
                    )?,
                }
            }
        }
        ForEachRefAtom::Identity { peeled, role, part } => {
            let identity = for_each_ref_typed_identity(context, *peeled, *role);
            match part {
                ForEachRefAtomIdentityPart::Full => write_for_each_ref_identity(stdout, identity)?,
                ForEachRefAtomIdentityPart::Name => {
                    write_for_each_ref_identity_name(stdout, identity)?
                }
                ForEachRefAtomIdentityPart::Email(mode) => {
                    write_for_each_ref_identity_email_mode(stdout, identity, *mode)?
                }
                ForEachRefAtomIdentityPart::Date(mode) => {
                    write_for_each_ref_identity_date_mode(stdout, identity, mode)?
                }
                ForEachRefAtomIdentityPart::DateRaw => {
                    write_for_each_ref_identity_date_raw(stdout, identity)?
                }
            }
        }
        ForEachRefAtom::ContentsLines { peeled, count } => {
            let message = if *peeled {
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.message.as_deref())
            } else {
                context
                    .contents
                    .as_ref()
                    .map(|contents| contents.message.as_ref())
            };
            if let Some(message) = message {
                write_for_each_ref_contents_lines(stdout, message, *count)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn for_each_ref_typed_refname<'a>(
    context: &'a ForEachRefFormatContext<'_>,
    source: ForEachRefNameSource,
) -> &'a str {
    match source {
        ForEachRefNameSource::Ref => context.refname,
        ForEachRefNameSource::Upstream => context
            .upstream
            .as_ref()
            .map(|upstream| upstream.refname.as_str())
            .unwrap_or(""),
        ForEachRefNameSource::Push => context
            .push
            .as_ref()
            .and_then(|push| push.refname.as_deref())
            .unwrap_or(""),
    }
}

pub(crate) fn for_each_ref_typed_identity<'a>(
    context: &'a ForEachRefFormatContext<'_>,
    peeled: bool,
    role: ForEachRefAtomIdentityRole,
) -> Option<&'a [u8]> {
    if peeled {
        let peeled = context.peeled_object.as_ref();
        return match role {
            ForEachRefAtomIdentityRole::Author => {
                peeled.and_then(|peeled| peeled.author.as_deref())
            }
            ForEachRefAtomIdentityRole::Committer => {
                peeled.and_then(|peeled| peeled.committer.as_deref())
            }
            ForEachRefAtomIdentityRole::Tagger => None,
            ForEachRefAtomIdentityRole::Creator => {
                peeled.and_then(|peeled| peeled.creator.as_deref())
            }
        };
    }

    let contents = context.contents.as_ref();
    match role {
        ForEachRefAtomIdentityRole::Author => {
            contents.and_then(|contents| contents.author.as_deref())
        }
        ForEachRefAtomIdentityRole::Committer => {
            contents.and_then(|contents| contents.committer.as_deref())
        }
        ForEachRefAtomIdentityRole::Tagger => {
            contents.and_then(|contents| contents.tagger.as_deref())
        }
        ForEachRefAtomIdentityRole::Creator => {
            contents.and_then(|contents| contents.creator.as_deref())
        }
    }
}

pub(crate) fn write_for_each_ref_contents_lines(
    stdout: &mut impl Write,
    message: &[u8],
    count: usize,
) -> Result<()> {
    let mut lines = message.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    for (idx, line) in lines.into_iter().take(count).enumerate() {
        if idx > 0 {
            stdout.write_all(b"\n    ")?;
        }
        stdout.write_all(line)?;
    }
    Ok(())
}

/// The set of `%(...email)` options, mirroring git's `email_option` bitset
/// (ref-filter.c `EO_TRIM`/`EO_LOCALPART`/`EO_MAILMAP`).
#[derive(Clone, Copy, Default)]
pub(crate) struct ForEachRefEmailOptions {
    trim: bool,
    localpart: bool,
    mailmap: bool,
}

/// Parse the option string after `%(authoremail:...)` exactly as git's
/// `person_email_atom_parser` does. Options are comma-separated and may repeat;
/// each must be an exact `trim`/`localpart`/`mailmap` token between commas.
/// On an unrecognized token, returns `Err(bad_arg)` where `bad_arg` is the
/// unconsumed remainder at the point of failure (git reports this verbatim).
pub(crate) fn setup_for_each_ref_email_options(
    arg: &str,
) -> std::result::Result<ForEachRefEmailOptions, String> {
    let mut options = ForEachRefEmailOptions::default();
    let mut rest = arg;
    loop {
        // git's email_atom_option_parser advances past a matched prefix; the
        // `bad_arg` it later reports is the *remaining* string AFTER that
        // consume (so `mailmaptrim` reports `trim`, not `mailmaptrim`).
        let matched = if let Some(tail) = rest.strip_prefix("trim") {
            options.trim = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("localpart") {
            options.localpart = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("mailmap") {
            options.mailmap = true;
            Some(tail)
        } else {
            None
        };
        let Some(tail) = matched else {
            // No prefix consumed: the bad argument is the whole remainder.
            return Err(rest.to_string());
        };
        rest = tail;
        let bad_arg = rest;
        if rest.is_empty() {
            break;
        }
        if let Some(tail) = rest.strip_prefix(',') {
            rest = tail;
        } else {
            return Err(bad_arg.to_string());
        }
    }
    Ok(options)
}

/// If `placeholder` is an email atom (`(\*?)(author|committer|tagger)email`
/// with optional `:opts`), render it. Returns `Some(Ok(()))` when handled,
/// `Some(Err(_))` on a bad-option error (already reported to stderr), and
/// `None` when the placeholder is not an email atom.
pub(crate) fn for_each_ref_try_email_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (atom, arg) = match placeholder.split_once(':') {
        Some((atom, arg)) => (atom, Some(arg)),
        None => (placeholder, None),
    };
    let (peeled, role) = match atom {
        "authoremail" => (false, ForEachRefAtomIdentityRole::Author),
        "committeremail" => (false, ForEachRefAtomIdentityRole::Committer),
        "taggeremail" => (false, ForEachRefAtomIdentityRole::Tagger),
        "*authoremail" => (true, ForEachRefAtomIdentityRole::Author),
        "*committeremail" => (true, ForEachRefAtomIdentityRole::Committer),
        "*taggeremail" => (true, ForEachRefAtomIdentityRole::Tagger),
        _ => return None,
    };
    let options = match arg {
        Some(arg) => match setup_for_each_ref_email_options(arg) {
            Ok(options) => options,
            Err(bad_arg) => {
                let name = atom.strip_prefix('*').unwrap_or(atom);
                eprintln!("fatal: unrecognized %({name}) argument: {bad_arg}");
                return Some(Err(GitError::Exit(128)));
            }
        },
        None => ForEachRefEmailOptions::default(),
    };
    Some(for_each_ref_write_email(
        stdout, context, peeled, role, options,
    ))
}

/// If `placeholder` is a trailers atom (`%(trailers[:opts])` or
/// `%(contents:trailers[:opts])`, with optional `*` peel), render it. Returns
/// `Some(Err(_))` (after reporting to stderr) for the bad-argument cases.
pub(crate) fn for_each_ref_try_trailers_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (base, peeled) = placeholder
        .strip_prefix('*')
        .map(|rest| (rest, true))
        .unwrap_or((placeholder, false));

    // Accept `trailers`, `trailers:ARG`, `contents:trailers`,
    // `contents:trailers:ARG`. The `contents:` prefix shares git's
    // `%(contents)` bad-argument error for `contents:trailersXXX`.
    let arg: Option<&str> = if base == "trailers" {
        None
    } else if let Some(rest) = base.strip_prefix("trailers:") {
        Some(rest)
    } else if let Some(rest) = base.strip_prefix("contents:") {
        if rest == "trailers" {
            None
        } else if let Some(rest) = rest.strip_prefix("trailers:") {
            Some(rest)
        } else {
            return None;
        }
    } else {
        return None;
    };

    let options = match arg {
        None => sley_pretty::ForEachRefTrailerOptions::default(),
        Some(arg) => match sley_pretty::parse_for_each_ref_trailer_options(arg) {
            Ok(options) => options,
            Err(None) => {
                eprintln!("fatal: expected %(trailers:key=<value>)");
                return Some(Err(GitError::Exit(128)));
            }
            Err(Some(invalid)) => {
                eprintln!("fatal: unknown %(trailers) argument: {invalid}");
                return Some(Err(GitError::Exit(128)));
            }
        },
    };

    Some((|| -> Result<()> {
        if let Some(message) = for_each_ref_message(context, peeled) {
            // git formats trailers over the message from the subject start to
            // the signature start (sig stripped).
            let parts = for_each_ref_message_parts(message);
            let sig_len = parts.signature.len();
            let trailer_src = &parts.bare[..parts.bare.len().saturating_sub(sig_len)];
            let rendered = sley_pretty::format_trailers_from_commit(trailer_src, &options);
            stdout.write_all(&rendered)?;
        }
        Ok(())
    })())
}

/// The raw message bytes for the ref's own object (`peeled == false`) or the
/// peeled tag target (`peeled == true`), if available.
pub(crate) fn for_each_ref_message<'a>(
    context: &'a ForEachRefFormatContext<'_>,
    peeled: bool,
) -> Option<&'a [u8]> {
    if peeled {
        context
            .peeled_object
            .as_ref()
            .and_then(|peeled| peeled.message.as_deref())
    } else {
        context.contents.as_ref().map(|contents| &*contents.message)
    }
}

/// If `placeholder` is a date atom (`(\*?)(author|committer|tagger|creator)date`
/// with an optional `:spec`), render it through the full date grammar. Returns
/// `Some(Err(_))` (after reporting to stderr) on an invalid specifier.
pub(crate) fn for_each_ref_try_date_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (atom, arg) = match placeholder.split_once(':') {
        Some((atom, arg)) => (atom, Some(arg)),
        None => (placeholder, None),
    };
    let (peeled, role) = match atom {
        "authordate" => (false, ForEachRefAtomIdentityRole::Author),
        "committerdate" => (false, ForEachRefAtomIdentityRole::Committer),
        "taggerdate" => (false, ForEachRefAtomIdentityRole::Tagger),
        "creatordate" => (false, ForEachRefAtomIdentityRole::Creator),
        "*authordate" => (true, ForEachRefAtomIdentityRole::Author),
        "*committerdate" => (true, ForEachRefAtomIdentityRole::Committer),
        "*taggerdate" => (true, ForEachRefAtomIdentityRole::Tagger),
        "*creatordate" => (true, ForEachRefAtomIdentityRole::Creator),
        _ => return None,
    };
    let Some(mode) = DateMode::parse_atom_modifier(arg) else {
        let name = atom.strip_prefix('*').unwrap_or(atom);
        eprintln!(
            "fatal: unrecognized %({name}) argument: {}",
            arg.unwrap_or("")
        );
        return Some(Err(GitError::Exit(128)));
    };
    Some((|| -> Result<()> {
        if let Some(identity) = for_each_ref_typed_identity(context, peeled, role)
            && let Some(value) = for_each_ref_identity_date(identity, &mode)
        {
            stdout.write_all(value.as_bytes())?;
        }
        Ok(())
    })())
}

/// Recognize the `%(describe)` family. Returns `(peeled, opts)` where `peeled`
/// is set for the deref form `%(*describe…)` and `opts` is whatever follows the
/// colon (empty when there is none). Returns `None` for non-describe atoms.
pub(crate) fn for_each_ref_describe_atom(placeholder: &str) -> Option<(bool, &str)> {
    let (peeled, rest) = match placeholder.strip_prefix('*') {
        Some(rest) => (true, rest),
        None => (false, placeholder),
    };
    if rest == "describe" {
        Some((peeled, ""))
    } else {
        rest.strip_prefix("describe:").map(|opts| (peeled, opts))
    }
}

/// Parse `%(describe:opts)` like git's `describe_atom_parser`: walk the
/// comma-separated options, and on the first unrecognized token report
/// `unrecognized %(describe) argument: <bad-token-through-end>` (git keeps the
/// rest of the string, not just the offending token).
pub(crate) fn for_each_ref_parse_describe_opts(opts: &str) -> Result<sley_pretty::DescribeSpec> {
    let mut spec = sley_pretty::DescribeSpec::default();
    let mut rest = opts;
    while !rest.is_empty() {
        let (part, next) = match rest.split_once(',') {
            Some((part, next)) => (part, next),
            None => (rest, ""),
        };
        if part == "tags" {
            spec.tags = true;
        } else if let Some(value) = part.strip_prefix("abbrev=") {
            match value.parse::<usize>() {
                Ok(width) => spec.abbrev = Some(width),
                Err(_) => return Err(for_each_ref_bad_describe_arg(rest)),
            }
        } else if let Some(value) = part.strip_prefix("match=") {
            spec.matches.push(value.to_string());
        } else if let Some(value) = part.strip_prefix("exclude=") {
            spec.excludes.push(value.to_string());
        } else {
            return Err(for_each_ref_bad_describe_arg(rest));
        }
        rest = next;
    }
    Ok(spec)
}

pub(crate) fn for_each_ref_bad_describe_arg(bad: &str) -> GitError {
    eprintln!("fatal: unrecognized %(describe) argument: {bad}");
    GitError::Exit(128)
}

/// For an oid atom like `tree:short` / `parent:short=7`, return the option
/// argument (`short` or `short=7`) when `placeholder` is exactly `atom:<arg>`.
pub(crate) fn for_each_ref_oid_atom_arg<'a>(placeholder: &'a str, atom: &str) -> Option<&'a str> {
    let rest = placeholder.strip_prefix(atom)?;
    rest.strip_prefix(':')
}

/// Parse the `short`/`short=N` argument of an oid atom into an abbreviation
/// width, mirroring git's `oid_atom_parser` validation. A bare `short` resolves
/// to the repository's `DEFAULT_ABBREV` (git's `O_SHORT` case), supplied by the
/// caller via `default_abbrev`; `short=N` overrides it.
pub(crate) fn for_each_ref_oid_atom_width(
    arg: &str,
    atom: &str,
    default_abbrev: Option<usize>,
) -> Result<Option<usize>> {
    if arg == "short" {
        Ok(default_abbrev)
    } else if let Some(value) = arg.strip_prefix("short=") {
        Ok(Some(parse_for_each_ref_abbrev_width(value).map_err(
            |_| {
                eprintln!("fatal: positive value expected '{value}' in %({atom})");
                GitError::Exit(128)
            },
        )?))
    } else {
        eprintln!("fatal: unrecognized %({atom}) argument: {arg}");
        Err(GitError::Exit(128))
    }
}

/// If `placeholder` is a name atom (`(\*?)(author|committer|tagger)name` with an
/// optional `:mailmap`/`:` argument), render it. Mirrors git's
/// `person_name_atom_parser`: the only accepted argument is `mailmap`.
pub(crate) fn for_each_ref_try_name_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (atom, arg) = match placeholder.split_once(':') {
        Some((atom, arg)) => (atom, Some(arg)),
        None => (placeholder, None),
    };
    let (peeled, role) = match atom {
        "authorname" => (false, ForEachRefAtomIdentityRole::Author),
        "committername" => (false, ForEachRefAtomIdentityRole::Committer),
        "taggername" => (false, ForEachRefAtomIdentityRole::Tagger),
        "*authorname" => (true, ForEachRefAtomIdentityRole::Author),
        "*committername" => (true, ForEachRefAtomIdentityRole::Committer),
        "*taggername" => (true, ForEachRefAtomIdentityRole::Tagger),
        _ => return None,
    };
    let mailmap = match arg {
        None => false,
        Some("mailmap") => true,
        Some(bad_arg) => {
            let name = atom.strip_prefix('*').unwrap_or(atom);
            eprintln!("fatal: unrecognized %({name}) argument: {bad_arg}");
            return Some(Err(GitError::Exit(128)));
        }
    };
    Some((|| -> Result<()> {
        let Some(identity) = for_each_ref_typed_identity(context, peeled, role) else {
            return Ok(());
        };
        if mailmap {
            let (name, _) = context.mailmap.rewrite_identity(identity);
            stdout.write_all(&name)?;
        } else {
            write_for_each_ref_identity_name(stdout, Some(identity))?;
        }
        Ok(())
    })())
}

pub(crate) fn for_each_ref_write_email(
    stdout: &mut impl Write,
    context: &ForEachRefFormatContext<'_>,
    peeled: bool,
    role: ForEachRefAtomIdentityRole,
    options: ForEachRefEmailOptions,
) -> Result<()> {
    let Some(identity) = for_each_ref_typed_identity(context, peeled, role) else {
        return Ok(());
    };
    let mode = if options.localpart {
        ForEachRefEmailMode::LocalPart
    } else if options.trim {
        ForEachRefEmailMode::Trim
    } else {
        ForEachRefEmailMode::Bracketed
    };
    if options.mailmap {
        let (_, email) = context.mailmap.rewrite_identity(identity);
        // Reassemble a synthetic identity so the shared email extractor applies
        // trim/localpart over the rewritten address.
        let mut synthetic = Vec::with_capacity(email.len() + 2);
        synthetic.push(b'<');
        synthetic.extend_from_slice(&email);
        synthetic.push(b'>');
        if let Some(value) = for_each_ref_identity_email(&synthetic, mode) {
            stdout.write_all(value)?;
        }
    } else if let Some(value) = for_each_ref_identity_email(identity, mode) {
        stdout.write_all(value)?;
    }
    Ok(())
}

pub(crate) fn for_each_ref_color_escape(value: &str) -> Result<String> {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return Err(GitError::Command("empty for-each-ref color".into()));
    }
    if tokens.len() == 1
        && let Some((red, green, blue)) = parse_for_each_ref_hex_color(tokens[0])
    {
        return Ok(format!("\x1b[38;2;{red};{green};{blue}m"));
    }
    let mut attributes = Vec::new();
    let mut foreground = None;
    let mut background = None;
    for token in tokens.iter().copied() {
        match token {
            "reset" => return Ok("\x1b[m".to_string()),
            "normal" if tokens.len() == 1 || (foreground.is_some() && background.is_none()) => {}
            "bold" => attributes.push("1".to_string()),
            "dim" => attributes.push("2".to_string()),
            "italic" => attributes.push("3".to_string()),
            "ul" => attributes.push("4".to_string()),
            "blink" => attributes.push("5".to_string()),
            "reverse" => attributes.push("7".to_string()),
            "strike" => attributes.push("9".to_string()),
            "nobold" | "nodim" => attributes.push("22".to_string()),
            "noitalic" => attributes.push("23".to_string()),
            "noul" => attributes.push("24".to_string()),
            "noblink" => attributes.push("25".to_string()),
            "noreverse" => attributes.push("27".to_string()),
            "nostrike" => attributes.push("29".to_string()),
            "black" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 30)?,
            "red" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 31)?,
            "green" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 32)?,
            "yellow" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 33)?,
            "blue" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 34)?,
            "magenta" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 35)?,
            "cyan" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 36)?,
            "white" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 37)?,
            "brightblack" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 90)?
            }
            "brightred" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 91)?
            }
            "brightgreen" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 92)?
            }
            "brightyellow" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 93)?
            }
            "brightblue" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 94)?
            }
            "brightmagenta" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 95)?
            }
            "brightcyan" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 96)?
            }
            "brightwhite" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 97)?
            }
            _ => {
                return Err(GitError::Command(format!(
                    "unsupported for-each-ref color {value}"
                )));
            }
        }
    }
    let mut codes = attributes;
    if let Some(foreground) = foreground {
        codes.push(foreground.to_string());
    }
    if let Some(background) = background {
        codes.push(background.to_string());
    }
    if codes.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("\x1b[{}m", codes.join(";")))
}

pub(crate) fn for_each_ref_push_color_code(
    value: &str,
    foreground: &mut Option<u16>,
    background: &mut Option<u16>,
    code: u16,
) -> Result<()> {
    if foreground.is_none() {
        *foreground = Some(code);
    } else if background.is_none() {
        *background = Some(code + 10);
    } else {
        return Err(GitError::Command(format!(
            "unsupported for-each-ref color {value}"
        )));
    }
    Ok(())
}
