//! `git patch-id` hash core: the streaming diff scanner and stable/unstable
//! digest folding (`get_one_patchid` / `flush_one_hunk`), independent of any
//! repository or I/O. Extracted verbatim from
//! `sley-cli/src/commands/patch_id.rs`; callers hand in pre-split lines.

use sley_core::{ObjectFormat, Result};

/// Resolved behavior after merging command-line flags with `patchid.stable`.
pub struct PatchIdOptions {
    /// Use the order-independent stable algorithm (true for `--stable` and
    /// `--verbatim`, and when `patchid.stable` is true with no overriding flag).
    pub stable: bool,
    /// Keep whitespace instead of stripping it (`--verbatim`). Implies `stable`.
    pub verbatim: bool,
}

/// Compute the patch-id of a single rendered diff (as produced by
/// `git diff` / `render_tree_to_tree_patch`), for rebase's `--cherry-mark`
/// duplicate detection. Returns `None` when the diff carries no patch content
/// (e.g. an empty commit), so the caller can treat such commits as non-matching.
///
/// git's `--cherry-mark` uses the default *unstable* commit patch-id
/// (diff_flush_patch_id with `diff_header_only` off, no stable reordering), so
/// this hashes with `stable: false`. The same mode is used for both sides of
/// the comparison, which is all the dedup requires.
pub fn patch_id_for_diff(diff: &[u8], format: ObjectFormat) -> Option<Vec<u8>> {
    patch_id_for_diff_with_mode(diff, format, false)
}

pub fn stable_patch_id_for_diff(diff: &[u8], format: ObjectFormat) -> Option<Vec<u8>> {
    patch_id_for_diff_with_mode(diff, format, true)
}

pub fn patch_id_for_diff_with_mode(
    diff: &[u8],
    format: ObjectFormat,
    stable: bool,
) -> Option<Vec<u8>> {
    let options = PatchIdOptions {
        stable,
        verbatim: false,
    };
    let lines = split_keep_newlines(diff);
    let mut cursor = 0usize;
    let patch = get_one_patchid(&lines, &mut cursor, format, &options);
    if patch.patchlen == 0 {
        return None;
    }
    Some(patch.result)
}

/// The accumulated state of one patch parsed from the input stream.
pub struct OnePatchId {
    /// The raw digest bytes of this patch's id (length matches the object format).
    pub result: Vec<u8>,
    /// The commit id recorded for the *following* patch, if a `commit`/`From`
    /// boundary line was consumed while scanning this one.
    pub next_commit: Option<Vec<u8>>,
    /// Total number of (post-`remove_space`) bytes hashed; zero means "no patch
    /// content", which suppresses output for this entry.
    pub patchlen: usize,
}

/// A running hash over a hunk's worth of patch bytes, plus the byte-wise
/// add-with-carry accumulator used to fold hunk digests in stable mode.
pub struct PatchHash {
    format: ObjectFormat,
    /// Bytes fed since the last flush; hashed lazily on `flush`/`finish` so the
    /// implementation stays independent of any incremental hashing API.
    buffer: Vec<u8>,
    /// The running result digest, folded into on every stable flush.
    result: Vec<u8>,
}

impl PatchHash {
    fn new(format: ObjectFormat) -> Self {
        PatchHash {
            format,
            buffer: Vec::new(),
            result: vec![0u8; format.raw_len()],
        }
    }

    /// Append bytes to the current hunk's hash input.
    fn update(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Finalize the current hunk and fold its digest into the running result with
    /// a byte-wise addition with carry, then reset for the next hunk. This is
    /// git's `flush_one_hunk`; it runs unconditionally (even on an empty buffer)
    /// so the digest of empty input participates exactly as git's does.
    fn flush(&mut self) -> Result<()> {
        let digest = sley_core::digest_bytes(self.format, &self.buffer)?;
        let bytes = digest.as_bytes();
        let mut carry: u16 = 0;
        for (slot, &add) in self.result.iter_mut().zip(bytes.iter()) {
            carry += u16::from(*slot) + u16::from(add);
            *slot = (carry & 0xff) as u8;
            carry >>= 8;
        }
        self.buffer.clear();
        Ok(())
    }

    /// Produce the final patch-id digest. Stable mode folds the trailing hunk via
    /// `flush` and returns the accumulator; unstable mode ignores the accumulator
    /// and returns the single hash of everything fed so far.
    fn finish(mut self, stable: bool) -> Result<Vec<u8>> {
        if stable {
            self.flush()?;
            Ok(self.result)
        } else {
            Ok(sley_core::digest_bytes(self.format, &self.buffer)?
                .as_bytes()
                .to_vec())
        }
    }
}

/// Parse the next patch from `lines` starting at `*cursor`, advancing `*cursor`
/// past the consumed lines. A faithful port of git's `get_one_patchid`.
pub fn get_one_patchid(
    lines: &[&[u8]],
    cursor: &mut usize,
    format: ObjectFormat,
    options: &PatchIdOptions,
) -> OnePatchId {
    let mut hash = PatchHash::new(format);
    let mut patchlen: usize = 0;
    // `before`/`after` track remaining context+removed / context+added lines in the
    // current hunk, exactly like git: -1 means "between hunks / parsing a header",
    // 0/0 means "hunk consumed, expecting the next `@@` or `diff`".
    let mut before: i64 = -1;
    let mut after: i64 = -1;
    let mut diff_is_binary = false;
    // The pre-/post-image object names captured from an `index` line, hashed only
    // when a binary patch follows (git hashes these as the binary hunk's content).
    let mut pre_oid: Vec<u8> = Vec::new();
    let mut post_oid: Vec<u8> = Vec::new();
    let mut next_commit: Option<Vec<u8>> = None;

    while *cursor < lines.len() {
        let line = lines[*cursor];
        *cursor += 1;

        // A `commit <oid>` / `From <oid> …` boundary records the next commit id and
        // ends this patch. A `\ No newline at end of file` marker is skipped (and,
        // under `--verbatim`, hashed verbatim) without affecting hunk accounting.
        if let Some(rest) = strip_line_prefix(line, b"commit ") {
            if let Some(oid) = leading_object_id(rest, format) {
                next_commit = Some(oid);
                break;
            }
        } else if let Some(rest) = strip_line_prefix(line, b"From ") {
            if let Some(oid) = leading_object_id(rest, format) {
                next_commit = Some(oid);
                break;
            }
        } else if line.starts_with(b"\\ ") && line.len() > 12 {
            if options.verbatim {
                hash.update(line);
            }
            continue;
        }

        // Skip commit-message text and other preamble until the first `diff` line.
        if patchlen == 0 && !line.starts_with(b"diff ") {
            continue;
        }

        // Parsing a diff header (no hunk seen yet for this file).
        if before == -1 {
            if line.starts_with(b"GIT binary patch") || line.starts_with(b"Binary files") {
                diff_is_binary = true;
                before = 0;
                hash.update(&pre_oid);
                hash.update(&post_oid);
                if options.stable {
                    // A flush error is impossible for in-memory hashing; ignore.
                    let _ = hash.flush();
                }
                continue;
            } else if let Some(rest) = strip_line_prefix(line, b"index ") {
                capture_index_oids(rest, &mut pre_oid, &mut post_oid);
                continue;
            } else if line.starts_with(b"--- ") {
                before = 1;
                after = 1;
            } else if !line.first().is_some_and(u8::is_ascii_alphabetic) {
                // A non-alphabetic line where a header was expected ends the patch
                // (e.g. trailing notes); leave it for the caller's next scan.
                *cursor -= 1;
                break;
            }
        }

        if diff_is_binary {
            if line.starts_with(b"diff ") {
                diff_is_binary = false;
                before = -1;
            }
            continue;
        }

        // Between hunks: either a new `@@` header or the start of the next file.
        if before == 0 && after == 0 {
            if line.starts_with(b"@@ -") {
                // Parse the next hunk's line counts; the header itself is not hashed.
                let (b, a) = scan_hunk_header(line);
                before = b;
                after = a;
                continue;
            }
            if !line.starts_with(b"diff ") {
                // End of this patch; let the caller re-read this line.
                *cursor -= 1;
                break;
            }
            if options.stable {
                let _ = hash.flush();
            }
            before = -1;
            after = -1;
        }

        // Inside a hunk: account for the line against the remaining counts.
        match line.first() {
            Some(b'-') => before -= 1,
            Some(b'+') => after -= 1,
            Some(b' ') => {
                before -= 1;
                after -= 1;
            }
            _ => {}
        }

        // Hash the line (whitespace-stripped unless `--verbatim`).
        if options.verbatim {
            patchlen += line.len();
            hash.update(line);
        } else {
            let stripped = remove_space(line);
            patchlen += stripped.len();
            hash.update(&stripped);
        }
    }

    let result = hash
        .finish(options.stable)
        .unwrap_or_else(|_| vec![0u8; format.raw_len()]);
    OnePatchId {
        result,
        next_commit,
        patchlen,
    }
}

/// Split a byte buffer into lines, keeping each line's trailing `\n` (a final line
/// without a newline is kept as-is). git reads whole lines including the newline,
/// then strips whitespace, so retaining the newline matters under `--verbatim`.
pub fn split_keep_newlines(input: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, &byte) in input.iter().enumerate() {
        if byte == b'\n' {
            lines.push(&input[start..=index]);
            start = index + 1;
        }
    }
    if start < input.len() {
        lines.push(&input[start..]);
    }
    lines
}

/// Strip an exact byte prefix, returning the remainder when it matches.
pub fn strip_line_prefix<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    line.strip_prefix(prefix)
}

/// Parse a leading object id (exactly `hex_len` hex digits) from `bytes`, ignoring
/// any trailing content. Mirrors git's `get_oid_hex`, which only requires the
/// leading hex run and tolerates the `… Mon Sep 17 …` tail of a `From` line.
pub fn leading_object_id(bytes: &[u8], format: ObjectFormat) -> Option<Vec<u8>> {
    let width = format.hex_len();
    if bytes.len() < width {
        return None;
    }
    let head = &bytes[..width];
    if head.iter().all(u8::is_ascii_hexdigit) {
        Some(head.to_ascii_lowercase())
    } else {
        None
    }
}

/// Capture the pre-/post-image abbreviated object names from the body of an
/// `index <a>..<b>[ <mode>]` line, matching git's parsing (split at `..`, then at
/// the following space or end of line). Used only to seed a binary hunk's hash.
pub fn capture_index_oids(rest: &[u8], pre_oid: &mut Vec<u8>, post_oid: &mut Vec<u8>) {
    let Some(dots) = find_subslice(rest, b"..") else {
        return;
    };
    let pre = &rest[..dots];
    let after_dots = &rest[dots + 2..];
    // Stop the post-image at the first space (the mode), trimming a trailing
    // newline when no mode is present.
    let post_end = after_dots
        .iter()
        .position(|&byte| byte == b' ')
        .unwrap_or_else(|| trimmed_len(after_dots));
    let post = &after_dots[..post_end];
    *pre_oid = pre.to_vec();
    *post_oid = post.to_vec();
}

/// Length of `bytes` excluding a single trailing `\n`, if present.
pub fn trimmed_len(bytes: &[u8]) -> usize {
    match bytes.last() {
        Some(b'\n') => bytes.len() - 1,
        _ => bytes.len(),
    }
}

/// Find the first occurrence of `needle` within `haystack`.
pub fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parse the `-<l>,<n> +<l>,<n>` counts from a `@@ … @@` hunk header, ignoring the
/// line numbers (git's `scan_hunk_header`). A missing `,<n>` defaults to 1.
pub fn scan_hunk_header(line: &[u8]) -> (i64, i64) {
    // line begins with "@@ -"; parse "<old>[,<oldcount>] +<new>[,<newcount>]".
    let body = &line[b"@@ -".len()..];
    let (_old_start, old_count, after_old) = scan_range(body);
    // After the old range, skip up to and including the "+".
    let plus = match find_subslice(after_old, b"+") {
        Some(index) => &after_old[index + 1..],
        None => after_old,
    };
    let (_new_start, new_count, _rest) = scan_range(plus);
    (old_count, new_count)
}

/// Parse a `<number>[,<number>]` range at the start of `bytes`, returning the
/// start, the count (default 1 when no `,<count>` is present), and the remaining
/// bytes after the range.
pub fn scan_range(bytes: &[u8]) -> (i64, i64, &[u8]) {
    let (start, rest) = scan_number(bytes);
    if let Some(after_comma) = rest.strip_prefix(b",") {
        let (count, rest) = scan_number(after_comma);
        (start, count, rest)
    } else {
        (start, 1, rest)
    }
}

/// Parse a leading run of ASCII digits as an `i64`, returning the value (0 when no
/// digits) and the bytes following the run. Saturates rather than overflowing.
pub fn scan_number(bytes: &[u8]) -> (i64, &[u8]) {
    let mut value: i64 = 0;
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(bytes[index] - b'0'));
        index += 1;
    }
    (value, &bytes[index..])
}

/// Remove every ASCII whitespace byte from a line (git's `remove_space`): space,
/// tab, newline, vertical tab, form feed, and carriage return.
pub fn remove_space(line: &[u8]) -> Vec<u8> {
    line.iter()
        .copied()
        .filter(|byte| !is_patch_id_space(*byte))
        .collect()
}

/// Whether a byte is ASCII whitespace for `remove_space` purposes. Matches C's
/// `isspace` for the ASCII range git operates on.
pub fn is_patch_id_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}
