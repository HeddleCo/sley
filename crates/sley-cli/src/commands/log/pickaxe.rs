use super::*;

pub(super) enum PickaxeSpec {
    /// `-S<string>`: count occurrences of the needle in the old vs new blob; a
    /// filepair matches when the counts differ. With `--pickaxe-regex` the
    /// needle is a regex (occurrence count), else a literal substring.
    String(String),
    /// `-G<regex>`: the regex matches some added or removed line of the textual
    /// diff (the leading `+`/`-` is trimmed before matching).
    Grep(String),
    /// `--find-object=<oid>`: a filepair matches when either side's blob oid is
    /// in the object set.
    FindObject(Vec<String>),
}

/// A compiled pickaxe predicate, ready to test a commit's diff filepairs.
pub(super) enum CompiledPickaxe {
    /// Literal-substring `-S`: count occurrences of `needle`.
    StringLiteral { needle: Vec<u8> },
    /// Regex `-S --pickaxe-regex`: count regex matches.
    StringRegex { regex: crate::grep_source::Regex },
    /// `-G<regex>`: regex matches an added/removed diff line.
    Grep { regex: crate::grep_source::Regex },
    /// `--find-object`: blob oid set.
    FindObject { oids: HashSet<ObjectId> },
}

// `--diff-filter` status bits (git `diff_status_letters` order is independent;
// we key by the status letter directly).
const DIFF_FILTER_ADDED: u32 = 1 << 0;
const DIFF_FILTER_COPIED: u32 = 1 << 1;
const DIFF_FILTER_DELETED: u32 = 1 << 2;
const DIFF_FILTER_MODIFIED: u32 = 1 << 3;
const DIFF_FILTER_RENAMED: u32 = 1 << 4;
const DIFF_FILTER_TYPE_CHANGED: u32 = 1 << 5;
const DIFF_FILTER_UNMERGED: u32 = 1 << 6;
const DIFF_FILTER_UNKNOWN: u32 = 1 << 7;
const DIFF_FILTER_BROKEN: u32 = 1 << 8;
// `*` (all-or-none): show the whole changeset if any filepair matches.
const DIFF_FILTER_AON: u32 = 1 << 9;
// All status bits except the `*` (all-or-none) sentinel — the base set a
// negation-only `--diff-filter` starts from before clearing the negated bits.
const DIFF_FILTER_ALL: u32 = DIFF_FILTER_ADDED
    | DIFF_FILTER_COPIED
    | DIFF_FILTER_DELETED
    | DIFF_FILTER_MODIFIED
    | DIFF_FILTER_RENAMED
    | DIFF_FILTER_TYPE_CHANGED
    | DIFF_FILTER_UNMERGED
    | DIFF_FILTER_UNKNOWN
    | DIFF_FILTER_BROKEN;

/// Map a `--diff-filter` status letter (uppercased) to its bit.
fn diff_filter_letter_bit(letter: char) -> u32 {
    match letter {
        'A' => DIFF_FILTER_ADDED,
        'C' => DIFF_FILTER_COPIED,
        'D' => DIFF_FILTER_DELETED,
        'M' => DIFF_FILTER_MODIFIED,
        'R' => DIFF_FILTER_RENAMED,
        'T' => DIFF_FILTER_TYPE_CHANGED,
        'U' => DIFF_FILTER_UNMERGED,
        'X' => DIFF_FILTER_UNKNOWN,
        'B' => DIFF_FILTER_BROKEN,
        '*' => DIFF_FILTER_AON,
        _ => 0,
    }
}

/// Parse a `--diff-filter` argument: each uppercase letter adds a positive bit,
/// each lowercase letter adds a negated bit (git `diff_opt_diff_filter`).
pub(super) fn parse_diff_filter_arg(arg: &str, filter: &mut u32, filter_not: &mut u32) -> Result<()> {
    for ch in arg.chars() {
        let (negate, upper) = if ch.is_ascii_lowercase() {
            (true, ch.to_ascii_uppercase())
        } else {
            (false, ch)
        };
        let bit = diff_filter_letter_bit(upper);
        if bit == 0 {
            eprintln!("fatal: unknown change class '{ch}' in --diff-filter={arg}");
            return Err(GitError::Exit(128));
        }
        if negate {
            *filter_not |= bit;
        } else {
            *filter |= bit;
        }
    }
    Ok(())
}

/// Resolve the final `--diff-filter` mask after the option scan (git applies the
/// `filter_not` negation against the all-bits base when no positive bits exist).
pub(super) fn resolve_diff_filter_mask(filter: u32, filter_not: u32) -> u32 {
    if filter_not != 0 {
        let base = if filter == 0 { DIFF_FILTER_ALL } else { filter };
        base & !filter_not
    } else {
        filter
    }
}

/// The status bit for a name-status entry (git `match_filter`: a `Modified`
/// entry with a break score counts as Broken).
fn diff_filter_entry_bit(entry: &sley_diff_merge::NameStatusEntry) -> u32 {
    diff_filter_letter_bit(entry.status.code())
}

#[derive(Clone, Copy)]
pub(super) struct DiffFilterMatchOptions<'a> {
    pub(super) mask: u32,
    pub(super) detect_renames: bool,
    pub(super) detect_copies: bool,
    pub(super) find_copies_harder: bool,
    pub(super) pathspec: Option<&'a DiffPathspec>,
}

/// Whether a commit's first-parent diff contains a filepair matching the
/// `--diff-filter` mask. With rename/copy bits requested, rename detection runs.
pub(super) fn diff_filter_commit_matches(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    opts: DiffFilterMatchOptions<'_>,
) -> Result<bool> {
    let parents = &record.commit.parents;
    let parent_tree = match parents.first() {
        Some(parent) => {
            let object = db.read_object(parent)?;
            Some(Commit::parse_ref(format, &object.body)?.tree)
        }
        None => None,
    };
    let tree = &record.commit.tree;
    let base = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: opts.detect_renames,
        detect_copies: opts.detect_copies,
        find_copies_harder: opts.find_copies_harder,
        rename_empty: true,
    };
    let entries = match (&parent_tree, opts.detect_renames) {
        (Some(parent), true) => sley_diff_merge::diff_name_status_trees_with_rename_options(
            db,
            format,
            parent,
            tree,
            sley_diff_merge::RenameDetectionOptions {
                base,
                detect_inexact: true,
                rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            },
        )?,
        (Some(parent), false) => {
            sley_diff_merge::diff_name_status_trees_with_options(db, format, parent, tree, base)?
        }
        (None, _) => {
            sley_diff_merge::diff_name_status_empty_tree_with_options(db, format, tree, base)?
        }
    };
    let entries = match opts.pathspec {
        Some(pathspec) => apply_diff_pathspec(entries, pathspec),
        None => entries,
    };
    // The `*` (all-or-none) bit doesn't change the "is any filepair a match"
    // question for commit selection (it only affects which filepairs are kept
    // for output), so test the status bits directly.
    let status_mask = opts.mask & !DIFF_FILTER_AON;
    Ok(entries
        .iter()
        .any(|entry| diff_filter_entry_bit(entry) & status_mask != 0))
}

pub(super) fn log_follow_single_path<'a>(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    selected: Vec<&'a sley_rev::CommitRecord>,
    start_path: &[u8],
    detect_renames: bool,
) -> Result<Vec<&'a sley_rev::CommitRecord>> {
    let mut path = start_path.to_vec();
    let mut kept = Vec::new();
    for record in selected {
        let parent_tree = match record.commit.parents.first() {
            Some(parent) => {
                let object = db.read_object(parent)?;
                Some(Commit::parse_ref(format, &object.body)?.tree)
            }
            None => None,
        };
        let base = sley_diff_merge::DiffNameStatusOptions {
            detect_renames,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
        };
        let entries = match (&parent_tree, detect_renames) {
            (Some(parent), true) => sley_diff_merge::diff_name_status_trees_with_rename_options(
                db,
                format,
                parent,
                &record.commit.tree,
                sley_diff_merge::RenameDetectionOptions {
                    base,
                    detect_inexact: true,
                    rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                    copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                },
            )?,
            (Some(parent), false) => sley_diff_merge::diff_name_status_trees_with_options(
                db,
                format,
                parent,
                &record.commit.tree,
                base,
            )?,
            (None, _) => sley_diff_merge::diff_name_status_empty_tree_with_options(
                db,
                format,
                &record.commit.tree,
                base,
            )?,
        };
        let mut matched = false;
        for entry in entries {
            if entry.path.as_bytes() == path.as_slice() {
                matched = true;
                if matches!(entry.status, sley_diff_merge::NameStatus::Renamed(_))
                    && let Some(old_path) = entry.old_path
                {
                    path = old_path.as_bytes().to_vec();
                }
                break;
            }
        }
        if matched {
            kept.push(record);
        }
    }
    Ok(kept)
}

/// Whether a commit's diff (against its first parent, or the empty tree for a
/// root) contains a filepair matching the pickaxe. Mirrors git's pickaxe diff
/// transform: it runs on the post-rename filepair queue, so we diff with rename
/// detection enabled and test every resulting old/new blob pair.
#[allow(clippy::too_many_arguments)]
pub(super) fn pickaxe_commit_matches(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    pickaxe: &CompiledPickaxe,
    ignore_case: bool,
    text: bool,
    detect_renames: bool,
    pathspec: Option<&DiffPathspec>,
) -> Result<bool> {
    let parents = &record.commit.parents;
    let parent_tree = match parents.first() {
        Some(parent) => {
            let object = db.read_object(parent)?;
            Some(Commit::parse_ref(format, &object.body)?.tree)
        }
        None => None,
    };
    let tree = &record.commit.tree;
    let base = sley_diff_merge::DiffNameStatusOptions {
        detect_renames,
        detect_copies: false,
        find_copies_harder: false,
        rename_empty: true,
    };
    let entries = match (&parent_tree, detect_renames) {
        (Some(parent), true) => sley_diff_merge::diff_name_status_trees_with_rename_options(
            db,
            format,
            parent,
            tree,
            sley_diff_merge::RenameDetectionOptions {
                base,
                detect_inexact: true,
                rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            },
        )?,
        (Some(parent), false) => {
            sley_diff_merge::diff_name_status_trees_with_options(db, format, parent, tree, base)?
        }
        (None, _) => {
            sley_diff_merge::diff_name_status_empty_tree_with_options(db, format, tree, base)?
        }
    };
    let entries = match pathspec {
        Some(pathspec) => apply_diff_pathspec(entries, pathspec),
        None => entries,
    };
    // --find-object: match purely on blob oids, no blob reads.
    if let CompiledPickaxe::FindObject { oids } = pickaxe {
        return Ok(entries.iter().any(|entry| {
            entry.old_oid.as_ref().is_some_and(|oid| oids.contains(oid))
                || entry.new_oid.as_ref().is_some_and(|oid| oids.contains(oid))
        }));
    }
    let skips_binary = pickaxe.skips_binary() && !text;
    for entry in &entries {
        let old = match entry.old_oid.as_ref() {
            Some(oid) => Some(pickaxe_read_blob(db, oid)?),
            None => None,
        };
        let new = match entry.new_oid.as_ref() {
            Some(oid) => Some(pickaxe_read_blob(db, oid)?),
            None => None,
        };
        // -G skips a filepair where either side is binary (unless --text).
        if skips_binary
            && (old.as_deref().is_some_and(pickaxe_is_binary)
                || new.as_deref().is_some_and(pickaxe_is_binary))
        {
            continue;
        }
        if pickaxe.filepair_matches(old.as_deref(), new.as_deref(), ignore_case) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Read a blob body for pickaxe inspection.
fn pickaxe_read_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = db.read_object(oid)?;
    Ok(object.body.to_vec())
}

/// git's `buffer_is_binary`: a NUL byte in the first 8000 bytes.
fn pickaxe_is_binary(bytes: &[u8]) -> bool {
    let scan = &bytes[..bytes.len().min(8000)];
    scan.contains(&0)
}

/// `-G<regex>`: run a textual diff between `old` and `new` and report whether
/// the regex matches any added or removed line (the leading `+`/`-` is trimmed
/// before matching, like git's `diffgrep_consume`).
fn pickaxe_diff_grep(old: &[u8], new: &[u8], regex: &crate::grep_source::Regex) -> bool {
    let old_lines = sley_diff_merge::split_lines(old);
    let new_lines = sley_diff_merge::split_lines(new);
    let mut old_idx = 0;
    let mut new_idx = 0;
    for op in sley_diff_merge::myers_diff_lines(&old_lines, &new_lines) {
        match op {
            sley_diff_merge::DiffOp::Equal(n) => {
                old_idx += n;
                new_idx += n;
            }
            sley_diff_merge::DiffOp::Delete(n) => {
                for line in &old_lines[old_idx..old_idx + n] {
                    if regex.is_match_with_case(line.bytes_without_newline(), false) {
                        return true;
                    }
                }
                old_idx += n;
            }
            sley_diff_merge::DiffOp::Insert(n) => {
                for line in &new_lines[new_idx..new_idx + n] {
                    if regex.is_match_with_case(line.bytes_without_newline(), false) {
                        return true;
                    }
                }
                new_idx += n;
            }
        }
    }
    false
}

/// Compile a pickaxe regex. git uses POSIX ERE (`REG_EXTENDED | REG_NEWLINE`,
/// plus `REG_ICASE` under `-i`) for both `-G` and `-S --pickaxe-regex`.
pub(super) fn compile_pickaxe_regex(
    pattern: &str,
    ignore_case: bool,
) -> Result<crate::grep_source::Regex> {
    crate::grep_source::Regex::compile(
        pattern,
        crate::grep_source::RegexMode::Ere,
        ignore_case,
        false,
    )
    .map_err(|_| {
        eprintln!("fatal: invalid regex: {pattern}");
        GitError::Exit(128)
    })
}

impl CompiledPickaxe {
    /// `-G` ignores binary files unless `--text`. The other kinds always look.
    fn skips_binary(&self) -> bool {
        matches!(self, CompiledPickaxe::Grep { .. })
    }

    /// Count occurrences of a literal needle (lowercasing the haystack when the
    /// needle was pre-lowercased for `-i`), capped at `limit` (0 = uncapped).
    fn count_literal(needle: &[u8], data: &[u8], ignore_case: bool, limit: usize) -> usize {
        if needle.is_empty() {
            return 0;
        }
        let mut cnt = 0;
        let mut i = 0;
        while i + needle.len() <= data.len() {
            let window = &data[i..i + needle.len()];
            let matched = if ignore_case {
                window.eq_ignore_ascii_case(needle)
            } else {
                window == needle
            };
            if matched {
                cnt += 1;
                if limit != 0 && cnt == limit {
                    return cnt;
                }
                i += needle.len();
            } else {
                i += 1;
            }
        }
        cnt
    }

    /// Count non-overlapping regex matches in `data`, capped at `limit`.
    fn count_regex(regex: &crate::grep_source::Regex, data: &[u8], limit: usize) -> usize {
        let mut cnt = 0;
        let mut from = 0;
        while from <= data.len() {
            match regex.find_from(data, from) {
                Some((start, end)) => {
                    cnt += 1;
                    if limit != 0 && cnt == limit {
                        return cnt;
                    }
                    from = if end > start { end } else { start + 1 };
                }
                None => break,
            }
        }
        cnt
    }

    /// Whether this filepair (old/new blob bytes) matches the pickaxe.
    fn filepair_matches(
        &self,
        old: Option<&[u8]>,
        new: Option<&[u8]>,
        ignore_case: bool,
    ) -> bool {
        match self {
            CompiledPickaxe::StringLiteral { needle } => {
                let c1 = old.map_or(0, |d| Self::count_literal(needle, d, ignore_case, 0));
                let c2 = new.map_or(0, |d| Self::count_literal(needle, d, ignore_case, c1 + 1));
                c1 != c2
            }
            CompiledPickaxe::StringRegex { regex } => {
                let c1 = old.map_or(0, |d| Self::count_regex(regex, d, 0));
                let c2 = new.map_or(0, |d| Self::count_regex(regex, d, c1 + 1));
                c1 != c2
            }
            CompiledPickaxe::Grep { regex } => {
                let old = old.unwrap_or(&[]);
                let new = new.unwrap_or(&[]);
                pickaxe_diff_grep(old, new, regex)
            }
            CompiledPickaxe::FindObject { .. } => false,
        }
    }
}
