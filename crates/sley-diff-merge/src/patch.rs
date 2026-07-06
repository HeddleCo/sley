//! Unified diff patch parsing and application.

use sley_core::{GitError, ObjectFormat, ObjectId, RepoPath, Result};
use std::path::{Path, PathBuf};

use crate::{name, ws};

// ---------------------------------------------------------------------------
// Unified / git diff patch parsing and application (engine for `git apply`/`git am`).
//
// Operates purely on in-memory byte buffers; the caller is responsible for
// reading/writing blobs from the working tree or the object database. The
// parser understands the textual format git produces (`diff --git`, `---`/`+++`
// file headers, `@@` hunk headers, context/`+`/`-` body lines, the
// `\ No newline at end of file` marker, `/dev/null` for added/deleted files,
// file mode headers, and `rename from`/`rename to` headers).
// ---------------------------------------------------------------------------

/// A single line inside a hunk. The stored bytes never include the trailing
/// line terminator; whether the line is terminated by `\n` is tracked
/// separately on the [`Hunk`] (see [`Hunk::old_no_newline`] /
/// [`Hunk::new_no_newline`]) so the no-final-newline case can be reproduced
/// byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    /// A line present in both the old and new versions.
    Context(Vec<u8>),
    /// A line added by the patch (present only in the new version).
    Insert(Vec<u8>),
    /// A line removed by the patch (present only in the old version).
    Delete(Vec<u8>),
}

impl HunkLine {
    /// The line content, without any trailing newline.
    pub fn content(&self) -> &[u8] {
        match self {
            Self::Context(bytes) | Self::Insert(bytes) | Self::Delete(bytes) => bytes,
        }
    }
}

/// A single `@@ -old_start,old_len +new_start,new_len @@` hunk.
///
/// `old_start` / `new_start` are 1-based line numbers as they appear in the
/// patch header. The `*_no_newline` flags record that the final line on that
/// side of the hunk is *not* terminated by a newline (the `\ No newline at end
/// of file` marker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<HunkLine>,
    /// The last context/deleted line of the old file lacks a trailing newline.
    pub old_no_newline: bool,
    /// The last context/inserted line of the new file lacks a trailing newline.
    pub new_no_newline: bool,
    /// The 1-based line number (in the patch input) of each entry in `lines`,
    /// used by `git apply`'s whitespace-error reporting (git's `state->linenr`).
    /// Empty when the patch was not parsed from input (e.g. synthesised hunks).
    pub line_input_lines: Vec<usize>,
}

/// A patch targeting a single file. Produced by [`parse_unified_patch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// Path on the `a/` (old) side, or `None` for a newly created file.
    pub old_path: Option<Vec<u8>>,
    /// Path on the `b/` (new) side, or `None` for a deleted file.
    pub new_path: Option<Vec<u8>>,
    /// Mode of the old file, when a mode header was present.
    pub old_mode: Option<u32>,
    /// Mode of the new file, when a mode header was present.
    pub new_mode: Option<u32>,
    pub hunks: Vec<Hunk>,
    /// The patch creates a new file (`--- /dev/null` / `new file mode`).
    pub is_new: bool,
    /// The patch deletes the file (`+++ /dev/null` / `deleted file mode`).
    pub is_delete: bool,
    /// The patch renames the file (`rename from`/`rename to`).
    pub is_rename: bool,
    /// The patch copies the file (`copy from`/`copy to`).
    pub is_copy: bool,
    /// Similarity score from `similarity index N%`, used for rename/copy summaries.
    pub similarity: Option<u8>,
    /// Dissimilarity score from `dissimilarity index N%`, used for rewrite summaries.
    pub dissimilarity: Option<u8>,
    /// Hex object id prefixes from the `index <old>..<new>[ mode]` line, if any.
    /// Carried verbatim (abbreviated or full); the binary apply and the `-3`
    /// fallback need these to resolve the pre-/post-image blobs.
    pub old_oid_hex: Option<Vec<u8>>,
    pub new_oid_hex: Option<Vec<u8>>,
    /// True when the patch is binary: either a `GIT binary patch` block (with
    /// `binary` payload) or a metadata-only `Binary files ... differ` line
    /// (no payload — the postimage must be reconstructed from the object store).
    pub is_binary: bool,
    /// The `GIT binary patch` payload, when this is a binary file patch. The
    /// fragment bytes are still zlib-deflated (the caller inflates them with
    /// the recorded original length), matching git's two-hunk forward/reverse
    /// layout.
    pub binary: Option<BinaryPatch>,
    /// True for git (`diff --git`) patches, whose names are relative to the
    /// repository top-level; false for traditional diffs, whose names are
    /// relative to the current directory (git's `is_toplevel_relative`). The
    /// `apply` cwd-prefix is only prepended to non-toplevel-relative patches.
    pub is_toplevel_relative: bool,
}

/// A `GIT binary patch` payload: a mandatory forward hunk (preimage → postimage)
/// and an optional reverse hunk (postimage → preimage), mirroring git's
/// `parse_binary`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryPatch {
    pub forward: BinaryHunk,
    pub reverse: Option<BinaryHunk>,
}

/// One binary hunk: the encoding method and the still-deflated data, plus the
/// declared original (inflated) length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryHunk {
    pub method: BinaryMethod,
    /// Length of the data *after* inflation (the `literal <N>` / `delta <N>`
    /// number). The caller inflates `deflated` to exactly this many bytes.
    pub origlen: usize,
    /// base85-decoded, still zlib-deflated bytes.
    pub deflated: Vec<u8>,
}

/// How a binary hunk encodes the postimage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryMethod {
    /// The inflated bytes ARE the postimage (`literal <N>`).
    Literal,
    /// The inflated bytes are a git delta to apply to the preimage (`delta <N>`).
    Delta,
}

/// Outcome of applying a [`FilePatch`] to a base buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The patch applied cleanly; carries the resulting file bytes.
    Applied(Vec<u8>),
    /// At least one hunk's context/deleted lines did not match the base.
    Rejected,
}

/// The minimum number of context lines git's `apply` insists on keeping when
/// it tries to fuzz a hunk into place — git's `apply_state.p_context`, which is
/// initialised to `UINT_MAX` (the `-C<n>` option lowers it). The fuzz loop in
/// `apply_one_fragment` stops the moment both leading and trailing context have
/// been reduced to this floor; with the default `UINT_MAX` floor that test is
/// already satisfied on the first failure, so **the default `git apply` / `git
/// am` path does no context fuzz and no begin/end relaxation at all** — a hunk
/// whose full preimage does not match at a valid position is simply rejected.
/// We keep the floor configurable so the structure mirrors git's, but the
/// shared apply engine only ever runs with the default.
const MIN_FUZZ_CONTEXT: usize = usize::MAX;

/// Parse a unified/git diff into one [`FilePatch`] per file it touches.
///
/// The parser is intentionally lenient about leading commentary (commit
/// messages, `index <oid>..<oid>` lines, etc.): anything that is not part of a
/// recognised header or hunk body is skipped. It errors only on structurally
/// invalid hunks (bad `@@` headers, body lines that overflow the declared hunk
/// counts, or hunk bodies that appear with no preceding file header).
pub fn parse_unified_patch(input: &[u8]) -> Result<Vec<FilePatch>> {
    parse_unified_patch_with_recount(input, false)
}

/// Parse a unified/git diff, optionally ignoring hunk header line counts and
/// recounting them from the hunk body. This mirrors `git apply --recount`.
pub fn parse_unified_patch_with_recount(input: &[u8], recount: bool) -> Result<Vec<FilePatch>> {
    parse_unified_patch_with_options(input, recount, &PatchPathOptions::default())
}

/// Path-resolution options for [`parse_unified_patch_with_options`], mirroring
/// `git apply`'s `-p<n>` strip (`p_value`) and `--directory=<root>` prefix.
#[derive(Clone)]
pub struct PatchPathOptions {
    /// Number of leading path components to strip (`-p<n>`); default 1.
    pub p_value: usize,
    /// Whether `p_value` was given explicitly. When false, traditional (non-git)
    /// diffs guess it from the `---`/`+++` lines.
    pub p_value_known: bool,
    /// `--directory=<dir>` root, normalised with a trailing slash (or empty).
    pub root: Vec<u8>,
    /// The cwd prefix (`state->prefix`): the current directory relative to the
    /// work tree, with a trailing slash (empty at the top level). Used only to
    /// guess `-p<n>` for traditional patches run from a subdirectory; the prefix
    /// itself is prepended to names by the caller, not here.
    pub prefix: Vec<u8>,
}

impl Default for PatchPathOptions {
    fn default() -> Self {
        PatchPathOptions {
            p_value: 1,
            p_value_known: false,
            root: Vec::new(),
            prefix: Vec::new(),
        }
    }
}

/// Parse a unified/git diff, applying `-p<n>` strip and `--directory` prefix to
/// every resolved pathname exactly as `git apply` does.
pub fn parse_unified_patch_with_options(
    input: &[u8],
    recount: bool,
    options: &PatchPathOptions,
) -> Result<Vec<FilePatch>> {
    let lines = split_patch_lines(input);
    let mut parser = PatchParser {
        lines: &lines,
        index: 0,
        recount,
        p_value: options.p_value,
        p_value_known: options.p_value_known,
        root: options.root.clone(),
        prefix: options.prefix.clone(),
    };
    parser.parse()
}

/// Apply a single-file patch to `base`, returning the patched bytes.
///
/// This mirrors git's `apply.c` (`apply_one_fragment` / `find_pos` /
/// `match_fragment`) for the default, no-whitespace-fuzz settings `git am`
/// and `git apply` use:
///
/// * Each hunk builds a *preimage* (context + deleted lines) and *postimage*
///   (context + inserted lines).
/// * A hunk anchored at the file start (`old_start <= 1`) must match the
///   beginning of the file (`match_beginning`); a hunk with no trailing context
///   must match the end of the file (`match_end`).
/// * The full preimage is matched byte-for-byte; the search starts at the
///   recorded position and ping-pongs outward across the whole image.
/// * Fuzz is applied *only* by dropping leading/trailing context lines (never
///   by jumping to a spurious context-only match); if no position matches even
///   after dropping all context, the hunk — and thus the whole patch — is
///   [`ApplyOutcome::Rejected`].
///
/// Rejecting (rather than spuriously applying at a wrong offset) is what lets
/// `git am -3` correctly fall back to its 3-way merge path.
///
/// New-file patches (empty/ignored base) and the no-final-newline case are
/// handled byte-accurately. Clean exact-position applies are byte-identical to
/// the previous behaviour.
pub fn apply_file_patch(base: &[u8], patch: &FilePatch) -> ApplyOutcome {
    apply_file_patch_with_options(base, patch, &ApplyFileOptions::default())
}

/// Options for [`apply_file_patch_with_options`], mirroring the `git apply`
/// flags that change fragment placement.
#[derive(Clone, Default)]
pub struct ApplyFileOptions {
    /// `--unidiff-zero`: trust the line numbers of context-free hunks instead of
    /// forcing them to anchor at the file's beginning/end.
    pub unidiff_zero: bool,
}

/// Reverse a file patch (`git apply -R`): swap the old/new names, modes, hunk
/// ranges, and no-newline flags, exchange add↔delete status, and flip every
/// `Insert`/`Delete` line. Applying the result undoes the original patch.
pub fn reverse_file_patch(patch: &FilePatch) -> FilePatch {
    let hunks = patch
        .hunks
        .iter()
        .map(|hunk| {
            let lines = hunk
                .lines
                .iter()
                .map(|line| match line {
                    HunkLine::Context(b) => HunkLine::Context(b.clone()),
                    HunkLine::Insert(b) => HunkLine::Delete(b.clone()),
                    HunkLine::Delete(b) => HunkLine::Insert(b.clone()),
                })
                .collect();
            Hunk {
                old_start: hunk.new_start,
                old_len: hunk.new_len,
                new_start: hunk.old_start,
                new_len: hunk.old_len,
                lines,
                old_no_newline: hunk.new_no_newline,
                new_no_newline: hunk.old_no_newline,
                // Reversal keeps the line order (only the +/- sense flips), so the
                // per-line patch-input line numbers carry over unchanged.
                line_input_lines: hunk.line_input_lines.clone(),
            }
        })
        .collect();
    // git's `reverse_patches` only swaps the modes when the patch actually
    // carries a new mode (a mode change) or is a deletion; a content-only patch
    // keeps its (old) mode so the type-mismatch check still compares against it.
    let (old_mode, new_mode) = if patch.new_mode.is_some() || patch.is_delete {
        (patch.new_mode, patch.old_mode)
    } else {
        (patch.old_mode, patch.new_mode)
    };
    FilePatch {
        old_path: patch.new_path.clone(),
        new_path: patch.old_path.clone(),
        old_mode,
        new_mode,
        hunks,
        is_new: patch.is_delete,
        is_delete: patch.is_new,
        is_rename: patch.is_rename,
        is_copy: patch.is_copy,
        similarity: patch.similarity,
        dissimilarity: patch.dissimilarity,
        // Swap the index OIDs so a reverse-applied binary patch resolves the
        // (formerly new) preimage and (formerly old) postimage correctly.
        old_oid_hex: patch.new_oid_hex.clone(),
        new_oid_hex: patch.old_oid_hex.clone(),
        is_binary: patch.is_binary,
        binary: patch.binary.as_ref().map(|binary| BinaryPatch {
            // `-R` swaps forward and reverse hunks (git's apply_in_reverse).
            forward: binary
                .reverse
                .clone()
                .unwrap_or_else(|| binary.forward.clone()),
            reverse: Some(binary.forward.clone()),
        }),
        is_toplevel_relative: patch.is_toplevel_relative,
    }
}

/// Apply a single-file patch with explicit fragment-placement options.
pub fn apply_file_patch_with_options(
    base: &[u8],
    patch: &FilePatch,
    options: &ApplyFileOptions,
) -> ApplyOutcome {
    // A pure deletion with no hunks yields an empty file.
    if patch.is_delete && patch.hunks.is_empty() {
        return ApplyOutcome::Applied(Vec::new());
    }
    // A new file: the only sensible base is empty; ignore whatever was passed
    // and build the result from the inserted lines.
    let base_for_match: &[u8] = if patch.is_new { b"" } else { base };

    // The "image" git mutates as each hunk applies. We splice in place so later
    // hunks see the effect of earlier ones (git carries the running offset for
    // the same reason).
    let mut image = split_blob_lines(base_for_match);

    // git seeds the search for hunk N at `newpos-1` *plus* the offset earlier
    // hunks drifted by, so a uniform shift only costs the search once.
    let mut running_offset: isize = 0;

    for hunk in &patch.hunks {
        match apply_one_hunk(&mut image, hunk, running_offset, options.unidiff_zero) {
            Some(drift) => running_offset += drift,
            None => return ApplyOutcome::Rejected,
        }
    }

    ApplyOutcome::Applied(join_lines(&image))
}

/// The outcome of a hunk-by-hunk apply (`git apply --reject`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectApply {
    /// The bytes after every hunk that applied (rejected hunks are skipped).
    pub content: Vec<u8>,
    /// Indices into `patch.hunks` of the hunks that did not apply.
    pub rejected: Vec<usize>,
}

/// Apply a single-file patch hunk-by-hunk, collecting the hunks that do not
/// apply rather than rejecting the whole patch — `git apply --reject`.
///
/// Each hunk is tried independently against the running image; an applied hunk
/// contributes its offset to later hunks (git's `apply_fragments` carries the
/// running line shift), a rejected hunk is recorded and left out. The returned
/// `content` is the image after all applicable hunks; `rejected` lists the
/// 0-based indices of the hunks the caller must write to `<file>.rej`.
pub fn apply_file_patch_rejecting(
    base: &[u8],
    patch: &FilePatch,
    options: &ApplyFileOptions,
) -> RejectApply {
    if patch.is_delete && patch.hunks.is_empty() {
        return RejectApply {
            content: Vec::new(),
            rejected: Vec::new(),
        };
    }
    let base_for_match: &[u8] = if patch.is_new { b"" } else { base };
    let mut image = split_blob_lines(base_for_match);
    let mut running_offset: isize = 0;
    let mut rejected = Vec::new();
    for (index, hunk) in patch.hunks.iter().enumerate() {
        match apply_one_hunk(&mut image, hunk, running_offset, options.unidiff_zero) {
            Some(drift) => running_offset += drift,
            None => rejected.push(index),
        }
    }
    RejectApply {
        content: join_lines(&image),
        rejected,
    }
}

/// Reconstruct the unified-diff text of one hunk for a `.rej` file. Mirrors the
/// raw fragment text git copies into `<file>.rej`: the `@@ -os[,oc] +ns[,nc] @@`
/// header (the `,1` count is omitted, matching git) followed by each line with
/// its ` `/`+`/`-` prefix, plus the `\ No newline at end of file` note where the
/// old/new side's final line is unterminated.
pub fn render_reject_hunk(hunk: &Hunk) -> Vec<u8> {
    fn range(start: usize, count: usize) -> String {
        if count == 1 {
            start.to_string()
        } else {
            format!("{start},{count}")
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"@@ -");
    out.extend_from_slice(range(hunk.old_start, hunk.old_len).as_bytes());
    out.extend_from_slice(b" +");
    out.extend_from_slice(range(hunk.new_start, hunk.new_len).as_bytes());
    out.extend_from_slice(b" @@\n");
    // The last old-side line is the last Context/Delete; the last new-side line
    // is the last Context/Insert. Their no-newline state drives the markers.
    let last_old = hunk
        .lines
        .iter()
        .rposition(|line| matches!(line, HunkLine::Context(_) | HunkLine::Delete(_)));
    let last_new = hunk
        .lines
        .iter()
        .rposition(|line| matches!(line, HunkLine::Context(_) | HunkLine::Insert(_)));
    for (index, line) in hunk.lines.iter().enumerate() {
        let (prefix, content) = match line {
            HunkLine::Context(bytes) => (b' ', bytes),
            HunkLine::Insert(bytes) => (b'+', bytes),
            HunkLine::Delete(bytes) => (b'-', bytes),
        };
        out.push(prefix);
        out.extend_from_slice(content);
        out.push(b'\n');
        let old_incomplete = hunk.old_no_newline && Some(index) == last_old;
        let new_incomplete = hunk.new_no_newline && Some(index) == last_new;
        if old_incomplete || new_incomplete {
            out.extend_from_slice(b"\\ No newline at end of file\n");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Whitespace-aware apply (`git apply --whitespace=fix` / `--ignore-space-change`)
//
// A faithful port of git's `apply_one_fragment` matching path (`match_fragment`,
// `find_pos`, `line_by_line_fuzzy_match`, `update_pre_post_images`,
// `update_image`). It adds the matching concerns that need the whitespace rule —
// blank-at-EOF tolerance, whitespace-corrected / whitespace-ignoring context
// matching, and removal of newly-added blank lines at EOF — on top of the plain
// exact-match engine above. The patch's `+` lines are expected to already carry
// their whitespace fixes (the apply command's whitespace pass applies them);
// this routine fixes the *context* lines as part of matching.
// ---------------------------------------------------------------------------

/// Options for [`apply_file_patch_ws`].
#[derive(Clone, Copy)]
pub struct WsApplyOptions {
    /// `--unidiff-zero`.
    pub unidiff_zero: bool,
    /// The per-path whitespace rule.
    pub ws_rule: ws::WsRule,
    /// `--whitespace=fix` (git's `correct_ws_error`).
    pub ws_fix: bool,
    /// `--ignore-space-change` / `--ignore-whitespace` (git's `ignore_ws_change`).
    pub ws_ignore_change: bool,
}

/// Outcome of [`apply_file_patch_ws`].
pub enum WsApplyOutcome {
    /// Applied; carries the bytes and the count of blank lines removed at EOF.
    Applied {
        content: Vec<u8>,
        blank_at_eof_removed: usize,
    },
    /// At least one hunk could not be placed.
    Rejected,
}

/// A pre/postimage line carrying git's `LINE_COMMON` flag (set for context lines,
/// clear for added/deleted lines).
#[derive(Clone)]
struct WsImageLine {
    content: Vec<u8>,
    no_newline: bool,
    common: bool,
}

impl WsImageLine {
    fn bytes(&self) -> Vec<u8> {
        let mut out = self.content.clone();
        if !self.no_newline {
            out.push(b'\n');
        }
        out
    }
}

fn line_bytes(line: &Line) -> Vec<u8> {
    let mut out = line.content.clone();
    if !line.no_newline {
        out.push(b'\n');
    }
    out
}

/// Split ws-fixed line bytes back into a [`WsImageLine`] (content sans trailing
/// newline, plus the no-newline flag).
fn ws_line_from_bytes(bytes: Vec<u8>, common: bool) -> WsImageLine {
    if bytes.last() == Some(&b'\n') {
        WsImageLine {
            content: bytes[..bytes.len() - 1].to_vec(),
            no_newline: false,
            common,
        }
    } else {
        WsImageLine {
            content: bytes,
            no_newline: true,
            common,
        }
    }
}

/// Whitespace-aware single-file apply — git's `apply_one_fragment` matching path.
pub fn apply_file_patch_ws(
    base: &[u8],
    patch: &FilePatch,
    opts: &WsApplyOptions,
) -> WsApplyOutcome {
    if patch.is_delete && patch.hunks.is_empty() {
        return WsApplyOutcome::Applied {
            content: Vec::new(),
            blank_at_eof_removed: 0,
        };
    }
    let base_for_match: &[u8] = if patch.is_new { b"" } else { base };
    let mut image = split_blob_lines(base_for_match);
    let mut running_offset: isize = 0;
    let mut blank_removed = 0usize;
    for hunk in &patch.hunks {
        match apply_one_fragment_ws(&mut image, hunk, running_offset, opts, &mut blank_removed) {
            Some(drift) => running_offset += drift,
            None => return WsApplyOutcome::Rejected,
        }
    }
    WsApplyOutcome::Applied {
        content: join_lines(&image),
        blank_at_eof_removed: blank_removed,
    }
}

fn apply_one_fragment_ws(
    image: &mut Vec<Line>,
    hunk: &Hunk,
    running_offset: isize,
    opts: &WsApplyOptions,
    blank_removed: &mut usize,
) -> Option<isize> {
    let blank_eof = opts.ws_rule & ws::WS_BLANK_AT_EOF != 0;
    let mut preimage: Vec<WsImageLine> = Vec::new();
    let mut postimage: Vec<WsImageLine> = Vec::new();
    let mut leading = 0usize;
    let mut trailing = 0usize;
    let mut seen_change = false;
    // git's `new_blank_lines_at_end`: blank lines added at the end, where a blank
    // *context* line does not reset the run but a non-blank line does.
    let mut new_blank_lines_at_end = 0usize;
    for hl in &hunk.lines {
        let mut added_blank_line = false;
        let mut is_blank_context = false;
        match hl {
            HunkLine::Context(bytes) => {
                if blank_eof && ws::ws_blank_line(bytes) {
                    is_blank_context = true;
                }
                preimage.push(WsImageLine {
                    content: bytes.clone(),
                    no_newline: false,
                    common: true,
                });
                postimage.push(WsImageLine {
                    content: bytes.clone(),
                    no_newline: false,
                    common: true,
                });
                if !seen_change {
                    leading += 1;
                }
                trailing += 1;
            }
            HunkLine::Delete(bytes) => {
                preimage.push(WsImageLine {
                    content: bytes.clone(),
                    no_newline: false,
                    common: false,
                });
                seen_change = true;
                trailing = 0;
            }
            HunkLine::Insert(bytes) => {
                postimage.push(WsImageLine {
                    content: bytes.clone(),
                    no_newline: false,
                    common: false,
                });
                if blank_eof && ws::ws_blank_line(bytes) {
                    added_blank_line = true;
                }
                seen_change = true;
                trailing = 0;
            }
        }
        if added_blank_line {
            new_blank_lines_at_end += 1;
        } else if is_blank_context {
            // leave the running count alone
        } else {
            new_blank_lines_at_end = 0;
        }
    }
    if hunk.old_no_newline
        && let Some(last) = preimage.last_mut()
    {
        last.no_newline = true;
    }
    if hunk.new_no_newline
        && let Some(last) = postimage.last_mut()
    {
        last.no_newline = true;
    }

    let mut match_beginning = hunk.old_start == 0 || (hunk.old_start == 1 && !opts.unidiff_zero);
    let mut match_end = !opts.unidiff_zero && trailing == 0;

    let mut expected = if preimage.is_empty() {
        new_side_position(hunk, running_offset)
    } else {
        expected_position(hunk, running_offset)
    };
    let hunk_expected = expected;
    let mut leading_v = leading;
    let mut trailing_v = trailing;

    let applied_pos = loop {
        if let Some(pos) = find_pos_ws(
            image,
            &mut preimage,
            &mut postimage,
            expected,
            opts,
            match_beginning,
            match_end,
        ) {
            break pos;
        }
        #[allow(clippy::absurd_extreme_comparisons)]
        if leading_v <= MIN_FUZZ_CONTEXT && trailing_v <= MIN_FUZZ_CONTEXT {
            return None;
        }
        if match_beginning || match_end {
            match_beginning = false;
            match_end = false;
            continue;
        }
        if leading_v >= trailing_v {
            preimage.remove(0);
            postimage.remove(0);
            expected -= 1;
            leading_v -= 1;
        }
        if trailing_v > leading_v {
            preimage.pop();
            postimage.pop();
            trailing_v -= 1;
        }
    };

    // Remove the blank lines added at EOF when the hunk lands at (or beyond) the
    // end of the image — git's `--whitespace=fix` blank-at-EOF correction.
    if new_blank_lines_at_end > 0
        && preimage.len() + applied_pos >= image.len()
        && blank_eof
        && opts.ws_fix
    {
        for _ in 0..new_blank_lines_at_end {
            postimage.pop();
        }
        *blank_removed += new_blank_lines_at_end;
    }

    // git's `update_image`: the preimage may extend beyond EOF, so only the part
    // that falls within the image is removed.
    let preimage_limit = preimage.len().min(image.len() - applied_pos);
    let replacement: Vec<Line> = postimage
        .iter()
        .map(|line| Line {
            content: line.content.clone(),
            no_newline: line.no_newline,
        })
        .collect();
    image.splice(applied_pos..applied_pos + preimage_limit, replacement);
    Some(applied_pos as isize - hunk_expected)
}

/// Port of git's `find_pos`: ping-pong outward from `expected` calling
/// [`match_fragment_ws`] at each candidate line. On a match the preimage and the
/// common lines of the postimage may be rewritten in place (whitespace fix).
fn find_pos_ws(
    image: &[Line],
    preimage: &mut Vec<WsImageLine>,
    postimage: &mut Vec<WsImageLine>,
    expected: isize,
    opts: &WsApplyOptions,
    match_beginning: bool,
    match_end: bool,
) -> Option<usize> {
    let line_nr = image.len();
    let pre_nr = preimage.len();
    let mut line: isize = if match_beginning {
        0
    } else if match_end {
        line_nr as isize - pre_nr as isize
    } else {
        expected
    };
    if line < 0 {
        line = 0;
    }
    if line as usize > line_nr {
        line = line_nr as isize;
    }
    let start = line as usize;
    let mut backwards = start;
    let mut forwards = start;
    let mut current = start;
    let mut i: u64 = 0;
    loop {
        if match_fragment_ws(
            image,
            preimage,
            postimage,
            current,
            opts,
            match_beginning,
            match_end,
        ) {
            return Some(current);
        }
        loop {
            if backwards == 0 && forwards == line_nr {
                return None;
            }
            if i & 1 == 1 {
                if backwards == 0 {
                    i += 1;
                    continue;
                }
                backwards -= 1;
                current = backwards;
            } else {
                if forwards == line_nr {
                    i += 1;
                    continue;
                }
                forwards += 1;
                current = forwards;
            }
            break;
        }
        i += 1;
    }
}

/// Port of git's `match_fragment`. Returns whether `preimage` matches `image` at
/// `current_lno`, trying (in order) an exact match, then — when `--whitespace=fix`
/// or `--ignore-space-change` is in effect — a whitespace-corrected or
/// whitespace-ignoring match, rewriting the preimage and the postimage's common
/// lines to the matched whitespace on success.
fn match_fragment_ws(
    image: &[Line],
    preimage: &mut Vec<WsImageLine>,
    postimage: &mut Vec<WsImageLine>,
    current_lno: usize,
    opts: &WsApplyOptions,
    match_beginning: bool,
    match_end: bool,
) -> bool {
    let blank_eof = opts.ws_rule & ws::WS_BLANK_AT_EOF != 0;
    let preimage_limit: usize;
    if preimage.len() + current_lno <= image.len() {
        preimage_limit = preimage.len();
        if match_end && (preimage.len() + current_lno != image.len()) {
            return false;
        }
    } else if opts.ws_fix && blank_eof {
        // The hunk extends beyond EOF and we are removing blank lines there; only
        // the in-image prefix must match, the rest of the preimage must be blank.
        preimage_limit = image.len() - current_lno;
    } else {
        return false;
    }

    if match_beginning && current_lno != 0 {
        return false;
    }

    if preimage_limit == preimage.len() {
        // Try an exact byte match of the whole preimage.
        let mut exact = true;
        if match_end && current_lno + preimage_limit != image.len() {
            exact = false;
        }
        if exact {
            for i in 0..preimage_limit {
                let img = &image[current_lno + i];
                let pre = &preimage[i];
                if img.content != pre.content || img.no_newline != pre.no_newline {
                    exact = false;
                    break;
                }
            }
        }
        if exact {
            return true;
        }
    } else {
        // The preimage extends beyond EOF: there must be at least one non-blank
        // context line within the in-image prefix.
        let mut all_blank = true;
        for line in preimage.iter().take(preimage_limit) {
            if !line.content.iter().all(|&b| ws::is_space(b)) {
                all_blank = false;
                break;
            }
        }
        if all_blank {
            return false;
        }
    }

    // No exact match. Try fuzzy / whitespace-corrected matching.
    if opts.ws_ignore_change {
        return fuzzy_match_ws(image, preimage, postimage, current_lno, preimage_limit);
    }
    if !opts.ws_fix {
        return false;
    }

    // Whitespace-corrected match: fix the in-image preimage lines and the target
    // lines, requiring equality; the beyond-EOF preimage lines must become blank.
    let mut fixed: Vec<WsImageLine> = Vec::with_capacity(preimage.len());
    for i in 0..preimage_limit {
        let fixed_pre = ws::ws_fix_bytes(&preimage[i].bytes(), opts.ws_rule);
        let fixed_tgt = ws::ws_fix_bytes(&line_bytes(&image[current_lno + i]), opts.ws_rule);
        if fixed_pre != fixed_tgt {
            return false;
        }
        fixed.push(ws_line_from_bytes(fixed_pre, preimage[i].common));
    }
    for line in preimage.iter().skip(preimage_limit) {
        let fixed_pre = ws::ws_fix_bytes(&line.bytes(), opts.ws_rule);
        if !fixed_pre.iter().all(|&b| ws::is_space(b)) {
            return false;
        }
        fixed.push(ws_line_from_bytes(fixed_pre, line.common));
    }
    update_pre_post_images_ws(preimage, postimage, fixed);
    true
}

/// Port of git's `line_by_line_fuzzy_match` (the `--ignore-space-change` path):
/// compare each line ignoring whitespace runs; on success the matched lines use
/// the *target's* whitespace in-image and the *preimage's* whitespace beyond EOF.
fn fuzzy_match_ws(
    image: &[Line],
    preimage: &mut Vec<WsImageLine>,
    postimage: &mut Vec<WsImageLine>,
    current_lno: usize,
    preimage_limit: usize,
) -> bool {
    for i in 0..preimage_limit {
        if !fuzzy_matchlines(&line_bytes(&image[current_lno + i]), &preimage[i].bytes()) {
            return false;
        }
    }
    // The beyond-EOF preimage lines must be all whitespace.
    for line in preimage.iter().skip(preimage_limit) {
        if !line.bytes().iter().all(|&b| ws::is_space(b)) {
            return false;
        }
    }
    // Build the fixed preimage: in-image lines take the target's whitespace, the
    // beyond-EOF lines keep the preimage's whitespace.
    let mut fixed: Vec<WsImageLine> = Vec::with_capacity(preimage.len());
    for i in 0..preimage_limit {
        let img = &image[current_lno + i];
        fixed.push(WsImageLine {
            content: img.content.clone(),
            no_newline: img.no_newline,
            common: preimage[i].common,
        });
    }
    for line in preimage.iter().skip(preimage_limit) {
        fixed.push(line.clone());
    }
    update_pre_post_images_ws(preimage, postimage, fixed);
    true
}

/// Port of git's `fuzzy_matchlines`: compare two lines ignoring whitespace
/// differences (any whitespace run matches any other; line endings are ignored).
fn fuzzy_matchlines(s1: &[u8], s2: &[u8]) -> bool {
    let trim = |s: &[u8]| {
        let mut end = s.len();
        while end > 0 && (s[end - 1] == b'\r' || s[end - 1] == b'\n') {
            end -= 1;
        }
        end
    };
    let end1 = trim(s1);
    let end2 = trim(s2);
    let (mut i, mut j) = (0usize, 0usize);
    while i < end1 && j < end2 {
        if ws::is_space(s1[i]) {
            if !ws::is_space(s2[j]) {
                return false;
            }
            while i < end1 && ws::is_space(s1[i]) {
                i += 1;
            }
            while j < end2 && ws::is_space(s2[j]) {
                j += 1;
            }
        } else if s1[i] != s2[j] {
            return false;
        } else {
            i += 1;
            j += 1;
        }
    }
    i == end1 && j == end2
}

/// Port of git's `update_pre_post_images`: replace the preimage with the fixed
/// lines (carrying the original common flags), then rewrite each *common* line of
/// the postimage to use the fixed preimage's content. A common postimage line
/// whose fixed-preimage counterpart ran out (a trailing blank trimmed at EOF) is
/// dropped (git's `reduced`).
fn update_pre_post_images_ws(
    preimage: &mut Vec<WsImageLine>,
    postimage: &mut Vec<WsImageLine>,
    fixed: Vec<WsImageLine>,
) {
    *preimage = fixed;
    let mut new_post: Vec<WsImageLine> = Vec::with_capacity(postimage.len());
    let mut ctx = 0usize;
    for line in postimage.iter() {
        if !line.common {
            new_post.push(line.clone());
            continue;
        }
        while ctx < preimage.len() && !preimage[ctx].common {
            ctx += 1;
        }
        if ctx >= preimage.len() {
            // preimage ran out (a fixed-away trailing blank): drop this line.
            continue;
        }
        new_post.push(WsImageLine {
            content: preimage[ctx].content.clone(),
            no_newline: preimage[ctx].no_newline,
            common: true,
        });
        ctx += 1;
    }
    *postimage = new_post;
}

/// Splice a single hunk into `image`, returning the offset (applied position −
/// expected position) so later hunks can carry it forward, or `None` if the
/// hunk cannot be located (which rejects the whole patch).
///
/// Faithful to git's `apply_one_fragment`: build preimage/postimage, try the
/// full preimage at progressively-reduced context, and on a match replace the
/// matched preimage region with the postimage.
fn apply_one_hunk(
    image: &mut Vec<Line>,
    hunk: &Hunk,
    running_offset: isize,
    unidiff_zero: bool,
) -> Option<isize> {
    // preimage = context + deletes (the old side we must find in the image).
    // postimage = context + inserts (what replaces it). They share their
    // leading/trailing *context* runs, which fuzz peels off symmetrically.
    let mut preimage: Vec<Line> = Vec::new();
    let mut postimage: Vec<Line> = Vec::new();
    let mut leading = 0usize; // context lines before the first +/-
    let mut trailing = 0usize; // context lines after the last +/-
    let mut seen_change = false;
    for hl in &hunk.lines {
        match hl {
            HunkLine::Context(bytes) => {
                preimage.push(Line {
                    content: bytes.clone(),
                    no_newline: false,
                });
                postimage.push(Line {
                    content: bytes.clone(),
                    no_newline: false,
                });
                if !seen_change {
                    leading += 1;
                }
                trailing += 1;
            }
            HunkLine::Delete(bytes) => {
                preimage.push(Line {
                    content: bytes.clone(),
                    no_newline: false,
                });
                seen_change = true;
                trailing = 0;
            }
            HunkLine::Insert(bytes) => {
                postimage.push(Line {
                    content: bytes.clone(),
                    no_newline: false,
                });
                seen_change = true;
                trailing = 0;
            }
        }
    }

    // Mark the no-final-newline state on the last preimage/postimage line so the
    // exact-match check and the spliced result reproduce a missing terminal
    // newline byte-for-byte.
    if hunk.old_no_newline
        && let Some(last) = preimage.last_mut()
    {
        last.no_newline = true;
    }
    if hunk.new_no_newline
        && let Some(last) = postimage.last_mut()
    {
        last.no_newline = true;
    }

    // A hunk that is `@@ -1,L ... @@` (or `@@ -0,0 ... @@` for an add-to-empty)
    // must match the beginning, and a hunk with no trailing context must match
    // the end — UNLESS `--unidiff-zero` was given, which tells apply to trust the
    // line numbers of a context-free hunk (`match_beginning = !oldpos ||
    // (oldpos == 1 && !unidiff_zero)`, `match_end = !unidiff_zero && !trailing`).
    let mut match_beginning = hunk.old_start == 0 || (hunk.old_start == 1 && !unidiff_zero);
    let mut match_end = !unidiff_zero && trailing == 0;

    // git anchors the search at `newpos-1` (0-based), carried by the running
    // offset from earlier hunks. The anchor (`pos` in git) shifts up whenever a
    // *leading* context line is peeled, because the preimage then begins one
    // line later in its own content. For a context-free pure insertion the
    // preimage is empty and matches anywhere, so the anchor alone decides the
    // result — there we must use the new-side line number exactly as git's
    // `newpos - 1` does (the old-side `oldpos` differs for `@@ -1,0 +2,1 @@`
    // inserts).
    let mut expected = if preimage.is_empty() {
        new_side_position(hunk, running_offset)
    } else {
        expected_position(hunk, running_offset)
    };
    // The full hunk's expected position never moves, so the returned drift is
    // measured against it (not the context-reduced anchor).
    let hunk_expected = expected;

    loop {
        if let Some(pos) = find_hunk_pos(image, &preimage, expected, match_beginning, match_end) {
            // Splice: drop the matched preimage lines, insert the postimage.
            let take = preimage.len();
            let replacement: Vec<Line> = postimage.clone();
            image.splice(pos..pos + take, replacement);
            return Some(pos as isize - hunk_expected);
        }

        // No position matched. Mirror git's guard *order* exactly: it first
        // checks whether context is already at the floor (`p_context`) and, if
        // so, gives up BEFORE relaxing match_beginning/match_end or peeling
        // context. With the default `UINT_MAX` floor this fires on the very
        // first failure, so the default path never fuzzes and never relaxes the
        // begin/end anchors — it rejects. (The comparison is intentionally
        // against the floor so the structure stays faithful to git even though
        // the default floor makes it unconditionally true.)
        #[allow(clippy::absurd_extreme_comparisons)]
        if leading <= MIN_FUZZ_CONTEXT && trailing <= MIN_FUZZ_CONTEXT {
            return None;
        }

        // git relaxes the begin/end anchors before peeling context: a hunk that
        // "must match the start/end" but didn't is retried free-floating first.
        if match_beginning || match_end {
            match_beginning = false;
            match_end = false;
            continue;
        }

        // Reduce context: peel the larger side (both if equal), exactly as git.
        if leading >= trailing {
            // Drop the first context line from pre+post; the anchor slides up.
            preimage.remove(0);
            postimage.remove(0);
            expected -= 1;
            leading -= 1;
        }
        if trailing > leading {
            preimage.pop();
            postimage.pop();
            trailing -= 1;
        }
    }
}

/// A line with its content (sans terminator) and whether it is newline-terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) content: Vec<u8>,
    pub(crate) no_newline: bool,
}

/// Split a blob into [`Line`]s. A trailing `\n` does not produce an empty final
/// line; instead the last real line is marked `no_newline = false`. A file that
/// does not end in `\n` marks its final line `no_newline = true`. An empty blob
/// produces no lines.
pub(crate) fn split_blob_lines(data: &[u8]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < data.len() {
        match data[start..].iter().position(|&b| b == b'\n') {
            Some(rel) => {
                let end = start + rel;
                lines.push(Line {
                    content: data[start..end].to_vec(),
                    no_newline: false,
                });
                start = end + 1;
            }
            None => {
                lines.push(Line {
                    content: data[start..].to_vec(),
                    no_newline: true,
                });
                start = data.len();
            }
        }
    }
    lines
}

/// Reassemble lines into a byte buffer, honouring per-line newline state.
fn join_lines(lines: &[Line]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(&line.content);
        if !line.no_newline {
            out.push(b'\n');
        }
    }
    out
}

/// The naive 0-based position where a hunk expects to apply, given the running
/// offset accumulated from earlier hunks.
fn expected_position(hunk: &Hunk, running_offset: isize) -> isize {
    // `old_start` is 1-based; an empty old side (new-file hunk) uses 0.
    let base = if hunk.old_start == 0 {
        0
    } else {
        hunk.old_start as isize - 1
    };
    base + running_offset
}

/// git's `pos = frag->newpos ? newpos - 1 : 0` anchor, used for a context-free
/// pure insertion whose empty preimage matches anywhere.
fn new_side_position(hunk: &Hunk, running_offset: isize) -> isize {
    let base = if hunk.new_start == 0 {
        0
    } else {
        hunk.new_start as isize - 1
    };
    base + running_offset
}

/// Find the 0-based line index in `image` where `preimage` (the hunk's context
/// + deleted lines, possibly already context-reduced by fuzz) matches.
///
/// Port of git's `find_pos`: start the search at `expected` (clamped, or forced
/// to 0/end when `match_beginning`/`match_end`), then ping-pong outward across
/// the *whole* image — backward and forward alternately — until both ends are
/// exhausted. Returns the first matching line index, or `None`.
fn find_hunk_pos(
    image: &[Line],
    preimage: &[Line],
    expected: isize,
    match_beginning: bool,
    match_end: bool,
) -> Option<usize> {
    let line_nr = image.len();
    let pre_nr = preimage.len();

    // git: if we must match the beginning, start at 0; if we must match the
    // end, start where the preimage would end exactly at EOF.
    let mut line: isize = if match_beginning {
        0
    } else if match_end {
        line_nr as isize - pre_nr as isize
    } else {
        expected
    };
    if line < 0 {
        line = 0;
    }
    if line as usize > line_nr {
        line = line_nr as isize;
    }

    let start = line as usize;
    let mut backwards = start;
    let mut forwards = start;
    let mut current = start;

    let mut i: u64 = 0;
    loop {
        if preimage_matches_at(image, preimage, current, match_beginning, match_end) {
            return Some(current);
        }

        loop {
            // Both ends exhausted: no match anywhere.
            if backwards == 0 && forwards == line_nr {
                return None;
            }
            if i & 1 == 1 {
                // Step backward.
                if backwards == 0 {
                    i += 1;
                    continue;
                }
                backwards -= 1;
                current = backwards;
            } else {
                // Step forward.
                if forwards == line_nr {
                    i += 1;
                    continue;
                }
                forwards += 1;
                current = forwards;
            }
            break;
        }
        i += 1;
    }
}

/// Whether `preimage` matches `image` starting at line `pos`.
///
/// Port of git's `match_fragment` for the default (no whitespace-fuzz) path:
/// a byte-exact full-preimage match. Honours `match_beginning` (pos must be 0)
/// and `match_end` (the preimage must reach *exactly* the end of the image),
/// and reproduces git's terminal-newline semantics — a preimage line marked
/// "no newline" only matches when it is the image's final line and that line is
/// itself newline-free.
fn preimage_matches_at(
    image: &[Line],
    preimage: &[Line],
    pos: usize,
    match_beginning: bool,
    match_end: bool,
) -> bool {
    if match_beginning && pos != 0 {
        return false;
    }
    // The whole preimage must fall within the image.
    if pos + preimage.len() > image.len() {
        return false;
    }
    if match_end && pos + preimage.len() != image.len() {
        return false;
    }
    for (i, pre) in preimage.iter().enumerate() {
        let img = &image[pos + i];
        if img.content != pre.content {
            return false;
        }
        // git compares the raw byte buffers, so a missing terminal newline on
        // either side only matches the other when both agree. A preimage line
        // that lacks a newline can only sit on the image's final line (which
        // must itself lack one); a preimage line that *has* a newline cannot
        // match a newline-free image line.
        if pre.no_newline != img.no_newline {
            return false;
        }
    }
    true
}

/// Split raw patch bytes into lines, preserving the *content* without the
/// trailing `\n` (a final unterminated line is kept). Carriage returns are kept
/// as-is so CRLF patch bodies round-trip.
fn split_patch_lines(input: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < input.len() {
        match input[start..].iter().position(|&b| b == b'\n') {
            Some(rel) => {
                let end = start + rel;
                lines.push(&input[start..end]);
                start = end + 1;
            }
            None => {
                lines.push(&input[start..]);
                start = input.len();
            }
        }
    }
    lines
}

struct PatchParser<'a> {
    lines: &'a [&'a [u8]],
    index: usize,
    recount: bool,
    /// `-p<n>` strip count (git's `state->p_value`); shared across the input so
    /// a guessed value sticks for subsequent traditional patches.
    p_value: usize,
    p_value_known: bool,
    /// `--directory` root (normalised, trailing slash) prepended to every name.
    root: Vec<u8>,
    /// The cwd prefix (`state->prefix`), used only to guess `-p<n>` for
    /// traditional patches run from a subdirectory.
    prefix: Vec<u8>,
}

impl<'a> PatchParser<'a> {
    fn parse(&mut self) -> Result<Vec<FilePatch>> {
        let mut patches = Vec::new();
        while self.index < self.lines.len() {
            let line = self.lines[self.index];
            if line.starts_with(b"diff --git ") {
                patches.push(self.parse_file(Some(line))?);
            } else if line.starts_with(b"--- ") {
                // A bare unified diff with no `diff --git` header.
                patches.push(self.parse_file(None)?);
            } else if line.starts_with(b"@@ ") {
                return Err(GitError::InvalidFormat(
                    "hunk header encountered before any file header".to_string(),
                ));
            } else {
                // Skip commentary / unrelated lines.
                self.index += 1;
            }
        }
        Ok(patches)
    }

    /// Parse one file's headers and hunks. When `diff_line` is `Some`, the
    /// current line is the `diff --git` header (already inspected by the
    /// caller); otherwise parsing starts at a `--- ` line of a traditional diff.
    fn parse_file(&mut self, diff_line: Option<&[u8]>) -> Result<FilePatch> {
        match diff_line {
            Some(diff_line) => self.parse_git_file(diff_line),
            None => self.parse_traditional_file(),
        }
    }

    /// p_value with one component removed — git uses `p_value - 1` for the
    /// `rename`/`copy from`/`to` extended headers, whose names lack the `a/`/`b/`
    /// prefix the `---`/`+++` lines carry.
    fn p_minus_one(&self) -> usize {
        self.p_value.saturating_sub(1)
    }

    /// Parse a git (`diff --git`) file section, resolving every pathname through
    /// git's `git_header_name` / `find_name` with the active `-p<n>`/`--directory`.
    fn parse_git_file(&mut self, diff_line: &[u8]) -> Result<FilePatch> {
        let mut patch = empty_file_patch();
        // `def_name`: the common name from the `diff --git` line, used when the
        // section carries no explicit `---`/`+++`/rename names.
        let rest = &diff_line[b"diff --git ".len()..];
        let mut def_name = name::git_header_name(self.p_value, rest);
        if let (Some(d), false) = (def_name.as_mut(), self.root.is_empty()) {
            let mut s = self.root.clone();
            s.extend_from_slice(d);
            *d = s;
        }
        self.index += 1;

        // Git patches name files relative to the repository top-level, so the
        // `apply` cwd-prefix is never prepended to them (git's is_toplevel_relative).
        patch.is_toplevel_relative = true;

        // Set once a `GIT binary patch` / `Binary files … differ` body is seen,
        // so the file is not run through the textual hunk parser afterwards.
        let mut binary_seen = false;

        // Extended headers until the first `---`/`@@`/next `diff --git`.
        while self.index < self.lines.len() {
            let line = self.lines[self.index];
            if line.starts_with(b"--- ") {
                self.parse_git_old_header(&line[b"--- ".len()..], &mut patch);
                self.index += 1;
                break;
            } else if line.starts_with(b"@@ ") {
                // No `---`/`+++` (e.g. pure rename or mode change with no body).
                break;
            } else if line.starts_with(b"diff --git ") {
                // Next file began with no body for this one.
                break;
            } else if let Some(rest) = strip_prefix(line, b"old mode ") {
                patch.old_mode = Some(self.parse_mode_line(rest)?);
            } else if let Some(rest) = strip_prefix(line, b"new mode ") {
                patch.new_mode = Some(self.parse_mode_line(rest)?);
            } else if let Some(rest) = strip_prefix(line, b"new file mode ") {
                patch.is_new = true;
                patch.new_mode = Some(self.parse_mode_line(rest)?);
                patch.new_path = def_name.clone();
            } else if let Some(rest) = strip_prefix(line, b"deleted file mode ") {
                patch.is_delete = true;
                patch.old_mode = Some(self.parse_mode_line(rest)?);
                patch.old_path = def_name.clone();
            } else if let Some(rest) = strip_prefix(line, b"index ") {
                // `index <old>..<new>[ <mode>]`: capture the blob OIDs (needed by
                // the binary apply and the `-3` fallback) and the unchanged-file
                // mode (git's gitdiff_index → gitdiff_oldmode).
                self.parse_index_line(rest, &mut patch)?;
            } else if let Some(rest) = strip_prefix(line, b"rename from ") {
                patch.is_rename = true;
                patch.old_path = name::find_name(rest, None, self.p_minus_one(), 0, &self.root);
            } else if let Some(rest) = strip_prefix(line, b"rename to ") {
                patch.is_rename = true;
                patch.new_path = name::find_name(rest, None, self.p_minus_one(), 0, &self.root);
            } else if let Some(rest) = strip_prefix(line, b"copy from ") {
                patch.is_copy = true;
                patch.old_path = name::find_name(rest, None, self.p_minus_one(), 0, &self.root);
            } else if let Some(rest) = strip_prefix(line, b"copy to ") {
                patch.is_copy = true;
                patch.new_path = name::find_name(rest, None, self.p_minus_one(), 0, &self.root);
            } else if let Some(rest) = strip_prefix(line, b"similarity index ") {
                patch.similarity = parse_percent(rest);
            } else if let Some(rest) = strip_prefix(line, b"dissimilarity index ") {
                patch.dissimilarity = parse_percent(rest);
            } else if line == b"GIT binary patch" {
                // The binary payload follows (no `---`/`+++`, no `@@` hunks).
                let gitbin_line = self.index + 1;
                patch.is_binary = true;
                patch.binary = Some(self.parse_binary_block(gitbin_line)?);
                binary_seen = true;
                break;
            } else if apply_is_binary_files_differ(line) {
                // A `--binary`-less diff records only `Binary files … differ`;
                // the postimage has to come from the object store at apply time.
                patch.is_binary = true;
                binary_seen = true;
                self.index += 1;
                break;
            } else {
                // Unrecognised commentary line — ignore.
                self.index += 1;
                continue;
            }
            self.index += 1;
        }

        // `+++` header (the old-file branch above already advanced past `---`).
        if !binary_seen
            && self.index < self.lines.len()
            && self.lines[self.index].starts_with(b"+++ ")
        {
            let line = self.lines[self.index];
            self.parse_git_new_header(&line[b"+++ ".len()..], &mut patch);
            self.index += 1;
        }

        // No explicit names anywhere: fall back to `def_name`, or fail like git
        // when `-p<n>` stripped every component away.
        if patch.old_path.is_none() && patch.new_path.is_none() {
            match &def_name {
                Some(d) => {
                    patch.old_path = Some(d.clone());
                    patch.new_path = Some(d.clone());
                }
                None => {
                    return Err(GitError::InvalidFormat(format!(
                        "git diff header lacks filename information when removing {} \
                         leading pathname components",
                        self.p_value
                    )));
                }
            }
        }

        // Binary patches carry no `@@` hunks.
        if !binary_seen {
            self.parse_hunks(&mut patch)?;
        }
        Ok(patch)
    }

    /// Parse a `index <old>..<new>[ <mode>]` line, capturing the blob OIDs and,
    /// when present, the unchanged-file mode (git's `gitdiff_index`).
    fn parse_index_line(&self, rest: &[u8], patch: &mut FilePatch) -> Result<()> {
        let Some(dotdot) = find_subslice(rest, b"..") else {
            return Ok(());
        };
        let old = &rest[..dotdot];
        let after = &rest[dotdot + 2..];
        // `new` runs to the first space (mode) or end of line.
        let (new, mode_part) = match after.iter().position(|&b| b == b' ') {
            Some(space) => (&after[..space], Some(&after[space + 1..])),
            None => (after, None),
        };
        if !old.is_empty() {
            patch.old_oid_hex = Some(old.to_vec());
        }
        if !new.is_empty() {
            patch.new_oid_hex = Some(new.to_vec());
        }
        if let Some(mode) = mode_part
            && !mode.is_empty()
        {
            patch.old_mode = Some(self.parse_mode_line(mode)?);
        }
        Ok(())
    }

    /// Parse a `<octal mode>` field, mirroring git's `parse_mode_line`: leading
    /// octal digits terminated by whitespace or end of line. Errors otherwise.
    fn parse_mode_line(&self, rest: &[u8]) -> Result<u32> {
        let mut value: u32 = 0;
        let mut i = 0;
        while i < rest.len() && (b'0'..=b'7').contains(&rest[i]) {
            value = value
                .checked_mul(8)
                .and_then(|value| value.checked_add((rest[i] - b'0') as u32))
                .ok_or_else(|| self.invalid_mode_error(rest))?;
            i += 1;
        }
        if i == 0 || (i < rest.len() && !rest[i].is_ascii_whitespace()) {
            return Err(self.invalid_mode_error(rest));
        }
        Ok(value)
    }

    fn invalid_mode_error(&self, rest: &[u8]) -> GitError {
        GitError::InvalidFormat(format!(
            "invalid mode on line {}: {}",
            self.index + 1,
            lossy(rest)
        ))
    }

    /// Parse a `GIT binary patch` body: a mandatory forward hunk and an optional
    /// reverse hunk, each base85-encoded over zlib-deflated data. Mirrors git's
    /// `parse_binary`. `gitbin_line` is the 1-based line of the `GIT binary patch`
    /// marker (used in the "unrecognized binary patch" message).
    fn parse_binary_block(&mut self, gitbin_line: usize) -> Result<BinaryPatch> {
        // self.index points at "GIT binary patch"; advance past it.
        self.index += 1;
        let forward = match self.parse_binary_hunk()? {
            Some(hunk) => hunk,
            None => {
                return Err(GitError::InvalidFormat(format!(
                    "binary-unrecognized:{gitbin_line}"
                )));
            }
        };
        let reverse = self.parse_binary_hunk()?;
        Ok(BinaryPatch { forward, reverse })
    }

    /// Parse one binary hunk (method line + base85 data lines + blank terminator),
    /// or `Ok(None)` when the current line is not a `literal`/`delta` method line.
    fn parse_binary_hunk(&mut self) -> Result<Option<BinaryHunk>> {
        if self.index >= self.lines.len() {
            return Ok(None);
        }
        let line = self.lines[self.index];
        let (method, num) = if let Some(rest) = strip_prefix(line, b"delta ") {
            (BinaryMethod::Delta, rest)
        } else if let Some(rest) = strip_prefix(line, b"literal ") {
            (BinaryMethod::Literal, rest)
        } else {
            return Ok(None);
        };
        let origlen = parse_leading_usize(num).map_err(|()| self.corrupt_binary_error())?;
        self.index += 1;

        let mut deflated = Vec::new();
        loop {
            if self.index >= self.lines.len() {
                // Ran out of input before the blank terminator (truncated patch).
                return Err(self.corrupt_binary_error());
            }
            let data = self.lines[self.index];
            if data.is_empty() {
                // Blank line terminates the hunk.
                self.index += 1;
                break;
            }
            // git counts the trailing newline in its line length; our split-off
            // lines do not carry it, so `git llen == data.len() + 1`.
            let len = data.len();
            if len < 6 || !(len - 1).is_multiple_of(5) {
                return Err(self.corrupt_binary_error());
            }
            let max_byte_length = (len - 1) / 5 * 4;
            let byte_length = match data[0] {
                b'A'..=b'Z' => (data[0] - b'A') as usize + 1,
                b'a'..=b'z' => (data[0] - b'a') as usize + 27,
                _ => return Err(self.corrupt_binary_error()),
            };
            if max_byte_length < byte_length || byte_length <= max_byte_length.saturating_sub(4) {
                return Err(self.corrupt_binary_error());
            }
            let decoded = decode_base85(&data[1..], byte_length)
                .ok_or_else(|| self.corrupt_binary_error())?;
            deflated.extend_from_slice(&decoded);
            self.index += 1;
        }
        Ok(Some(BinaryHunk {
            method,
            origlen,
            deflated,
        }))
    }

    fn corrupt_binary_error(&self) -> GitError {
        GitError::InvalidFormat(format!("binary-corrupt:{}", self.index + 1))
    }

    fn parse_git_old_header(&self, rest: &[u8], patch: &mut FilePatch) {
        if name::is_dev_null(rest) {
            patch.is_new = true;
            patch.old_path = None;
        } else if patch.old_path.is_none() {
            patch.old_path = name::find_name(rest, None, self.p_value, name::TERM_TAB, &self.root);
        }
    }

    fn parse_git_new_header(&self, rest: &[u8], patch: &mut FilePatch) {
        if name::is_dev_null(rest) {
            patch.is_delete = true;
            patch.new_path = None;
        } else if patch.new_path.is_none() {
            patch.new_path = name::find_name(rest, None, self.p_value, name::TERM_TAB, &self.root);
        }
    }

    /// Parse a traditional (non-git) diff section, mirroring git's
    /// `parse_traditional_patch`: guess the strip count, recognise epoch
    /// timestamps as creation/deletion, and prefer the shorter of the two names.
    fn parse_traditional_file(&mut self) -> Result<FilePatch> {
        let mut patch = empty_file_patch();
        let first_line = self.lines[self.index];
        let first = first_line[b"--- ".len()..].to_vec();
        self.index += 1;
        let second = if self.index < self.lines.len() && self.lines[self.index].starts_with(b"+++ ")
        {
            let s = self.lines[self.index][b"+++ ".len()..].to_vec();
            self.index += 1;
            Some(s)
        } else {
            None
        };

        if let Some(second) = &second {
            if !self.p_value_known {
                let p0 = name::guess_p_value(&first, &self.root, &self.prefix);
                let q0 = name::guess_p_value(second, &self.root, &self.prefix);
                let p = if p0.is_none() { q0 } else { p0 };
                if let Some(pv) = p
                    && Some(pv) == q0
                {
                    self.p_value = pv;
                    self.p_value_known = true;
                }
            }

            let name = if name::is_dev_null(&first) {
                patch.is_new = true;
                let name = name::find_name_traditional(second, None, self.p_value, &self.root);
                patch.new_path = name.clone();
                name
            } else if name::is_dev_null(second) {
                patch.is_delete = true;
                let name = name::find_name_traditional(&first, None, self.p_value, &self.root);
                patch.old_path = name.clone();
                name
            } else {
                let first_name =
                    name::find_name_traditional(&first, None, self.p_value, &self.root);
                let name = name::find_name_traditional(
                    second,
                    first_name.as_deref(),
                    self.p_value,
                    &self.root,
                );
                if name::has_epoch_timestamp(&first) {
                    patch.is_new = true;
                    patch.new_path = name.clone();
                } else if name::has_epoch_timestamp(second) {
                    patch.is_delete = true;
                    patch.old_path = name.clone();
                } else {
                    patch.old_path = name.clone();
                    patch.new_path = name.clone();
                }
                name
            };
            // git's `parse_traditional_patch`: a name that strips away every
            // component (e.g. `-p2` against a one-component `file_in_root`) is a
            // hard error — the whole apply fails rather than silently skipping the
            // unresolved file.
            if name.is_none() {
                return Err(GitError::InvalidFormat(format!(
                    "unable to find filename in patch at line {}",
                    self.index
                )));
            }
        }

        self.parse_hunks(&mut patch)?;
        Ok(patch)
    }

    /// Parse the hunk bodies that follow a file header, stopping at the next
    /// file header.
    fn parse_hunks(&mut self, patch: &mut FilePatch) -> Result<()> {
        while self.index < self.lines.len() {
            let line = self.lines[self.index];
            // git's `parse_single_patch` only treats a line as a fragment when it
            // begins with `@@ -` (old side first). A `@@ +…` line — e.g. the
            // malformed header a Subversion-generated diff emits — is not a hunk;
            // it (and the lines after it) are skipped as commentary, so a deletion
            // with no real hunk still applies from its metadata alone.
            if line.starts_with(b"@@ -") {
                let hunk = self.parse_hunk()?;
                patch.hunks.push(hunk);
            } else if line.starts_with(b"diff --git ") {
                break;
            } else if line.starts_with(b"--- ") {
                // Start of a subsequent bare diff.
                break;
            } else {
                // Trailing commentary between/after hunks.
                self.index += 1;
            }
        }
        Ok(())
    }

    fn parse_hunk(&mut self) -> Result<Hunk> {
        let header = self.lines[self.index];
        let (old_start, old_len, new_start, new_len) = parse_hunk_header(header)?;
        self.index += 1;

        let mut hunk = Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            lines: Vec::new(),
            old_no_newline: false,
            new_no_newline: false,
            line_input_lines: Vec::new(),
        };
        let mut old_seen = 0usize;
        let mut new_seen = 0usize;

        while self.index < self.lines.len() {
            // Stop when both sides are satisfied. In recount mode the header
            // counts are intentionally ignored; the next hunk/file header ends
            // the body.
            if !self.recount && old_seen >= old_len && new_seen >= new_len {
                break;
            }
            let line = self.lines[self.index];
            if self.recount
                && (line.starts_with(b"@@ ")
                    || line.starts_with(b"diff --git ")
                    || line.starts_with(b"diff a/")
                    || line.starts_with(b"--- "))
            {
                break;
            }
            if line.is_empty() {
                // A wholly empty line in a unified diff is a context line whose
                // content is the empty string (git emits a bare ` `, but some
                // tooling/email transport strips the trailing space).
                hunk.lines.push(HunkLine::Context(Vec::new()));
                hunk.line_input_lines.push(self.index + 1);
                old_seen += 1;
                new_seen += 1;
                self.index += 1;
                continue;
            }
            match line[0] {
                b' ' => {
                    hunk.lines.push(HunkLine::Context(line[1..].to_vec()));
                    hunk.line_input_lines.push(self.index + 1);
                    old_seen += 1;
                    new_seen += 1;
                }
                b'+' => {
                    hunk.lines.push(HunkLine::Insert(line[1..].to_vec()));
                    hunk.line_input_lines.push(self.index + 1);
                    new_seen += 1;
                }
                b'-' => {
                    hunk.lines.push(HunkLine::Delete(line[1..].to_vec()));
                    hunk.line_input_lines.push(self.index + 1);
                    old_seen += 1;
                }
                b'\\' => {
                    // `\ No newline at end of file` — applies to the line just
                    // emitted. Set the appropriate side flag(s).
                    self.mark_no_newline(&mut hunk);
                    self.index += 1;
                    continue;
                }
                _ => {
                    return Err(GitError::InvalidFormat(format!(
                        "corrupt-hunk-body:{}",
                        self.index + 1
                    )));
                }
            }
            self.index += 1;
        }

        // A trailing `\ No newline` may follow the final body line even after
        // the counts are satisfied; consume it.
        if self.index < self.lines.len() && self.lines[self.index].starts_with(b"\\") {
            self.mark_no_newline(&mut hunk);
            self.index += 1;
        }

        if self.recount {
            hunk.old_len = old_seen;
            hunk.new_len = new_seen;
        } else if old_seen != old_len || new_seen != new_len {
            return Err(GitError::InvalidFormat(format!(
                "hunk body line counts mismatch: header declared -{old_len},+{new_len} \
                 but body had -{old_seen},+{new_seen}"
            )));
        }

        Ok(hunk)
    }

    /// Set the no-newline flag based on the kind of the most recently pushed
    /// hunk line.
    fn mark_no_newline(&self, hunk: &mut Hunk) {
        match hunk.lines.last() {
            Some(HunkLine::Context(_)) => {
                hunk.old_no_newline = true;
                hunk.new_no_newline = true;
            }
            Some(HunkLine::Insert(_)) => hunk.new_no_newline = true,
            Some(HunkLine::Delete(_)) => hunk.old_no_newline = true,
            None => {}
        }
    }
}

/// An all-empty [`FilePatch`] for the parser to fill in.
fn empty_file_patch() -> FilePatch {
    FilePatch {
        old_path: None,
        new_path: None,
        old_mode: None,
        new_mode: None,
        hunks: Vec::new(),
        is_new: false,
        is_delete: false,
        is_rename: false,
        is_copy: false,
        similarity: None,
        dissimilarity: None,
        old_oid_hex: None,
        new_oid_hex: None,
        is_binary: false,
        binary: None,
        is_toplevel_relative: false,
    }
}

/// Parse an `@@ -l,s +l,s @@` header into `(old_start, old_len, new_start,
/// new_len)`. A missing `,s` means a length of 1.
fn parse_hunk_header(line: &[u8]) -> Result<(usize, usize, usize, usize)> {
    let err = || GitError::InvalidFormat(format!("malformed hunk header: {}", lossy(line)));
    let rest = strip_prefix(line, b"@@ ").ok_or_else(err)?;
    // Up to the closing ` @@`.
    let close = find_subslice(rest, b" @@").ok_or_else(err)?;
    let ranges = &rest[..close];
    let mut parts = ranges.split(|&b| b == b' ').filter(|p| !p.is_empty());
    let old = parts.next().ok_or_else(err)?;
    let new = parts.next().ok_or_else(err)?;
    let old = strip_prefix(old, b"-").ok_or_else(err)?;
    let new = strip_prefix(new, b"+").ok_or_else(err)?;
    let (old_start, old_len) = parse_range(old).ok_or_else(err)?;
    let (new_start, new_len) = parse_range(new).ok_or_else(err)?;
    Ok((old_start, old_len, new_start, new_len))
}

/// Parse `start[,len]` into `(start, len)`, defaulting `len` to 1.
fn parse_range(range: &[u8]) -> Option<(usize, usize)> {
    match range.iter().position(|&b| b == b',') {
        Some(comma) => {
            let start = parse_usize(&range[..comma])?;
            let len = parse_usize(&range[comma + 1..])?;
            Some((start, len))
        }
        None => Some((parse_usize(range)?, 1)),
    }
}

fn parse_usize(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: usize = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    }
    Some(value)
}

fn parse_percent(bytes: &[u8]) -> Option<u8> {
    let trimmed = trim_ascii_end(bytes)
        .strip_suffix(b"%")
        .unwrap_or(trim_ascii_end(bytes));
    let value = parse_usize(trimmed)?;
    u8::try_from(value).ok().filter(|value| *value <= 100)
}

fn strip_prefix<'b>(line: &'b [u8], prefix: &[u8]) -> Option<&'b [u8]> {
    if line.starts_with(prefix) {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

/// Whether a diff body line is a metadata-only binary marker (`Binary files …
/// differ` / `Files … differ`), git's binhdr detection.
fn apply_is_binary_files_differ(line: &[u8]) -> bool {
    line.ends_with(b" differ")
        && (line.starts_with(b"Binary files ") || line.starts_with(b"Files "))
}

/// Parse leading decimal digits (git uses `strtoul`, which ignores trailing
/// junk). Returns `Ok(0)` when there are no leading digits. Returns `Err(())`
/// when the decimal value overflows `usize`.
pub(crate) fn parse_leading_usize(bytes: &[u8]) -> std::result::Result<usize, ()> {
    let mut value = 0usize;
    for &b in bytes {
        if !b.is_ascii_digit() {
            break;
        }
        value = value.checked_mul(10).ok_or(())?;
        value = value.checked_add((b - b'0') as usize).ok_or(())?;
    }
    Ok(value)
}

/// git's base85 alphabet (`base85.c` `en85`).
const BASE85_ALPHABET: &[u8; 85] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

fn base85_value(ch: u8) -> Option<u32> {
    BASE85_ALPHABET
        .iter()
        .position(|&c| c == ch)
        .map(|index| index as u32)
}

/// Decode `len` bytes from a base85 buffer (5 chars → 4 bytes, big-endian), a
/// port of git's `decode_85`. Returns `None` on an invalid alphabet character or
/// an overflowing 5-char group. `buffer` must contain `ceil(len/4) * 5` chars.
fn decode_base85(buffer: &[u8], len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut pos = 0usize;
    let mut remaining = len;
    while remaining > 0 {
        let mut acc: u32 = 0;
        // First four characters never overflow a u32 (85^4 < 2^32).
        for _ in 0..4 {
            let de = base85_value(*buffer.get(pos)?)?;
            pos += 1;
            acc = acc * 85 + de;
        }
        let de = base85_value(*buffer.get(pos)?)?;
        pos += 1;
        // The fifth character can overflow; reject it as git does.
        if 0xffff_ffffu32 / 85 < acc {
            return None;
        }
        acc *= 85;
        if 0xffff_ffffu32 - de < acc {
            return None;
        }
        acc += de;

        let cnt = remaining.min(4);
        remaining -= cnt;
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[..cnt]);
    }
    Some(out)
}

/// Apply a git delta (`delta.c` `patch_delta`) to reconstruct the postimage from
/// `base`. The delta begins with the base size and result size as varints,
/// followed by copy (`0x80` bit set: offset/size from base) and insert (literal
/// bytes) opcodes. Returns `None` on any malformed/inconsistent delta.
pub fn git_patch_delta(base: &[u8], delta: &[u8]) -> Option<Vec<u8>> {
    let mut data = 0usize;
    let read_hdr_size = |data: &mut usize| -> Option<usize> {
        let mut size = 0usize;
        let mut shift = 0u32;
        loop {
            let cmd = *delta.get(*data)?;
            *data += 1;
            size |= ((cmd & 0x7f) as usize).checked_shl(shift)?;
            shift += 7;
            if cmd & 0x80 == 0 {
                break;
            }
        }
        Some(size)
    };

    let base_size = read_hdr_size(&mut data)?;
    if base_size != base.len() {
        return None;
    }
    let result_size = read_hdr_size(&mut data)?;
    let mut out = Vec::with_capacity(sley_pack::inflate::bounded_inflate_reserve(
        result_size,
        delta.len(),
    ));

    while data < delta.len() {
        let cmd = delta[data];
        data += 1;
        if cmd & 0x80 != 0 {
            // Copy from base.
            let mut cp_off = 0usize;
            let mut cp_size = 0usize;
            if cmd & 0x01 != 0 {
                cp_off = *delta.get(data)? as usize;
                data += 1;
            }
            if cmd & 0x02 != 0 {
                cp_off |= (*delta.get(data)? as usize) << 8;
                data += 1;
            }
            if cmd & 0x04 != 0 {
                cp_off |= (*delta.get(data)? as usize) << 16;
                data += 1;
            }
            if cmd & 0x08 != 0 {
                cp_off |= (*delta.get(data)? as usize) << 24;
                data += 1;
            }
            if cmd & 0x10 != 0 {
                cp_size = *delta.get(data)? as usize;
                data += 1;
            }
            if cmd & 0x20 != 0 {
                cp_size |= (*delta.get(data)? as usize) << 8;
                data += 1;
            }
            if cmd & 0x40 != 0 {
                cp_size |= (*delta.get(data)? as usize) << 16;
                data += 1;
            }
            if cp_size == 0 {
                cp_size = 0x10000;
            }
            let end = cp_off.checked_add(cp_size)?;
            if end > base.len() || cp_size > result_size {
                return None;
            }
            out.extend_from_slice(&base[cp_off..end]);
        } else if cmd != 0 {
            // Insert literal bytes from the delta.
            let len = cmd as usize;
            let end = data.checked_add(len)?;
            if end > delta.len() {
                return None;
            }
            out.extend_from_slice(&delta[data..end]);
            data = end;
        } else {
            // Opcode 0 is reserved.
            return None;
        }
    }

    if data != delta.len() || out.len() != result_size {
        return None;
    }
    Some(out)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn trim_ascii_end(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
