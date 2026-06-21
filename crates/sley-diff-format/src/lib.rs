//! Public pre-1.0 API surface for formatting diffs.
//!
//! This crate is intentionally an extraction boundary, not the final diff
//! engine. It names the option and rendering shapes that later code can fill
//! from `sley-diff-merge` without forcing callers to allocate whole rendered
//! patches. Inputs are borrowed (`&[u8]` paths and line bodies), while output is
//! pushed through [`DiffSink`] so a CLI, pager, network response, or test can
//! stream events as they are produced.
//!
//! The API is pre-1.0 and may grow as extraction continues. Prefer the provided
//! constructors and accessors over exhaustive struct literals in downstream
//! code.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::io::{self, Write};

/// Git's default amount of context around changed lines.
pub const DEFAULT_CONTEXT_LINES: usize = 3;

/// Options that affect textual diff rendering.
///
/// The struct is owned and cheap to clone so command parsing can build it once,
/// then pass shared references to streaming renderers. Payloads that are tied
/// to repository data, such as paths and hunk lines, live in [`DiffEvent`]
/// instead and stay borrowed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiffRenderOptions {
    /// Number of context lines around each change (`-U<n>`).
    pub context_lines: usize,
    /// Number of unchanged lines that may appear between hunks before they are
    /// merged (`--inter-hunk-context=<n>`).
    pub interhunk_context: usize,
    /// Line-diff algorithm requested by the caller.
    pub algorithm: DiffAlgorithm,
    /// Whether and how color markers should be emitted.
    pub color: ColorMode,
    /// Optional word-diff rendering mode.
    pub word_diff: Option<WordDiffMode>,
    /// Whether paths and mode/index headers are emitted before hunk bodies.
    pub emit_file_headers: bool,
}

impl Default for DiffRenderOptions {
    fn default() -> Self {
        Self {
            context_lines: DEFAULT_CONTEXT_LINES,
            interhunk_context: 0,
            algorithm: DiffAlgorithm::Myers,
            color: ColorMode::Never,
            word_diff: None,
            emit_file_headers: true,
        }
    }
}

impl DiffRenderOptions {
    /// Return a copy with a different context line count.
    #[must_use]
    pub fn with_context_lines(mut self, context_lines: usize) -> Self {
        self.context_lines = context_lines;
        self
    }

    /// Return a copy with a different inter-hunk merge distance.
    #[must_use]
    pub fn with_interhunk_context(mut self, interhunk_context: usize) -> Self {
        self.interhunk_context = interhunk_context;
        self
    }
}

/// Line-level diff algorithm requested by a renderer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DiffAlgorithm {
    /// Standard Myers shortest-edit-script diff.
    #[default]
    Myers,
    /// Myers with Git's `--minimal` search.
    Minimal,
    /// Patience diff.
    Patience,
    /// Histogram diff.
    Histogram,
}

/// Output coloring policy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Never write color escapes.
    #[default]
    Never,
    /// Emit colors when the eventual renderer considers the sink interactive.
    Auto,
    /// Always emit color escapes.
    Always,
}

/// Word-diff style requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordDiffMode {
    /// Inline `{+ +}` / `[- -]` markers.
    Plain,
    /// Color-only word diff.
    Color,
    /// Porcelain word-diff records.
    Porcelain,
    /// Regex-delimited word spans.
    Regex,
}

/// A borrowed path-like value in a diff.
///
/// Git paths are bytes, not guaranteed UTF-8. Keeping them borrowed lets the
/// diff engine point into index/tree/pathspec storage while a renderer streams
/// the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffPath<'a> {
    /// The special `/dev/null` side of creates and deletes.
    DevNull,
    /// Repository-relative path bytes.
    Path(&'a [u8]),
}

impl<'a> DiffPath<'a> {
    /// Borrow repository-relative path bytes.
    #[must_use]
    pub const fn borrowed(bytes: &'a [u8]) -> Self {
        Self::Path(bytes)
    }

    /// Returns the raw path bytes, or `None` for `/dev/null`.
    #[must_use]
    pub const fn as_bytes(self) -> Option<&'a [u8]> {
        match self {
            Self::DevNull => None,
            Self::Path(bytes) => Some(bytes),
        }
    }
}

/// File-level status for a formatted diff.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// Existing path changed in place.
    #[default]
    Modified,
    /// New path was added.
    Added,
    /// Existing path was removed.
    Deleted,
    /// Path was renamed.
    Renamed,
    /// Path was copied.
    Copied,
}

/// Borrowed file-level metadata for a diff entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileHeader<'a> {
    /// Old-side path, or [`DiffPath::DevNull`] for creations.
    pub old_path: DiffPath<'a>,
    /// New-side path, or [`DiffPath::DevNull`] for deletions.
    pub new_path: DiffPath<'a>,
    /// File status used by header renderers.
    pub status: FileStatus,
    /// Old object id text, when already available to the caller.
    pub old_oid: Option<&'a str>,
    /// New object id text, when already available to the caller.
    pub new_oid: Option<&'a str>,
    /// Old file mode text, such as `100644`.
    pub old_mode: Option<&'a str>,
    /// New file mode text, such as `100755`.
    pub new_mode: Option<&'a str>,
}

impl<'a> FileHeader<'a> {
    /// Construct a borrowed modified-file header.
    #[must_use]
    pub const fn modified(old_path: &'a [u8], new_path: &'a [u8]) -> Self {
        Self {
            old_path: DiffPath::Path(old_path),
            new_path: DiffPath::Path(new_path),
            status: FileStatus::Modified,
            old_oid: None,
            new_oid: None,
            old_mode: None,
            new_mode: None,
        }
    }
}

/// A hunk range in unified-diff notation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HunkRange {
    /// One-based start line. Git uses `0` for empty sides in create/delete
    /// hunks.
    pub start: u32,
    /// Number of lines covered by the hunk.
    pub len: u32,
}

impl HunkRange {
    /// Construct a range from unified-diff start/count values.
    #[must_use]
    pub const fn new(start: u32, len: u32) -> Self {
        Self { start, len }
    }
}

/// Borrowed hunk metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HunkHeader<'a> {
    /// Old-side range.
    pub old: HunkRange,
    /// New-side range.
    pub new: HunkRange,
    /// Optional section heading bytes after the second `@@`.
    pub section: Option<&'a [u8]>,
}

impl<'a> HunkHeader<'a> {
    /// Construct a hunk header with no section heading.
    #[must_use]
    pub const fn new(old: HunkRange, new: HunkRange) -> Self {
        Self {
            old,
            new,
            section: None,
        }
    }
}

/// Origin marker for a hunk body line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// Unchanged context line (` `).
    Context,
    /// Removed old-side line (`-`).
    Delete,
    /// Added new-side line (`+`).
    Insert,
}

impl DiffLineKind {
    const fn marker(self) -> u8 {
        match self {
            Self::Context => b' ',
            Self::Delete => b'-',
            Self::Insert => b'+',
        }
    }
}

/// Borrowed line payload for a hunk body.
///
/// `content` is raw line bytes and may include a trailing `\n`; renderers
/// should not require ownership just to prefix and forward it. Optional line
/// numbers allow later extraction to support blame-style and word-diff hooks
/// without changing the event shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffLine<'a> {
    /// Context/delete/insert marker.
    pub kind: DiffLineKind,
    /// Raw line bytes, normally including the trailing newline when present.
    pub content: &'a [u8],
    /// One-based old-side line number, when known.
    pub old_lineno: Option<u32>,
    /// One-based new-side line number, when known.
    pub new_lineno: Option<u32>,
}

impl<'a> DiffLine<'a> {
    /// Construct a borrowed hunk body line.
    #[must_use]
    pub const fn new(kind: DiffLineKind, content: &'a [u8]) -> Self {
        Self {
            kind,
            content,
            old_lineno: None,
            new_lineno: None,
        }
    }
}

/// Streaming event emitted by a diff producer and consumed by a formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffEvent<'a> {
    /// File-level metadata preceding hunk bodies.
    FileHeader(FileHeader<'a>),
    /// A unified hunk header.
    HunkHeader(HunkHeader<'a>),
    /// One borrowed hunk body line.
    Line(DiffLine<'a>),
    /// Git's `\ No newline at end of file` marker.
    NoNewlineAtEof,
}

/// Consumer for streaming diff events.
///
/// The trait takes each event by value, but the payloads inside each event are
/// borrowed from the producer. This lets a later renderer write to an arbitrary
/// sink while walking tree/blob data once.
pub trait DiffSink {
    /// Consume one diff event.
    fn write_event(&mut self, event: DiffEvent<'_>) -> io::Result<()>;
}

/// Stream events into a sink.
pub fn render_events<'a, I, S>(events: I, sink: &mut S) -> io::Result<()>
where
    I: IntoIterator<Item = DiffEvent<'a>>,
    S: DiffSink + ?Sized,
{
    for event in events {
        sink.write_event(event)?;
    }
    Ok(())
}

/// Minimal unified-diff text writer used by tests and early consumers.
///
/// This is not the final byte-for-byte renderer. It demonstrates the streaming
/// sink boundary by writing each borrowed event directly to an inner
/// [`Write`].
pub struct UnifiedDiffWriter<W> {
    inner: W,
    options: DiffRenderOptions,
}

impl<W> UnifiedDiffWriter<W> {
    /// Create a writer with default rendering options.
    #[must_use]
    pub fn new(inner: W) -> Self {
        Self::with_options(inner, DiffRenderOptions::default())
    }

    /// Create a writer with explicit rendering options.
    #[must_use]
    pub const fn with_options(inner: W, options: DiffRenderOptions) -> Self {
        Self { inner, options }
    }

    /// Borrow the configured options.
    #[must_use]
    pub const fn options(&self) -> &DiffRenderOptions {
        &self.options
    }

    /// Return the wrapped writer.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> DiffSink for UnifiedDiffWriter<W> {
    fn write_event(&mut self, event: DiffEvent<'_>) -> io::Result<()> {
        match event {
            DiffEvent::FileHeader(header) if self.options.emit_file_headers => {
                self.inner.write_all(b"diff --git ")?;
                write_prefixed_path(&mut self.inner, b"a/", header.old_path)?;
                self.inner.write_all(b" ")?;
                write_prefixed_path(&mut self.inner, b"b/", header.new_path)?;
                self.inner.write_all(b"\n--- ")?;
                write_prefixed_path(&mut self.inner, b"a/", header.old_path)?;
                self.inner.write_all(b"\n+++ ")?;
                write_prefixed_path(&mut self.inner, b"b/", header.new_path)?;
                self.inner.write_all(b"\n")
            }
            DiffEvent::FileHeader(_) => Ok(()),
            DiffEvent::HunkHeader(header) => {
                write_hunk_range(&mut self.inner, b"@@ -", header.old)?;
                write_hunk_range(&mut self.inner, b" +", header.new)?;
                self.inner.write_all(b" @@")?;
                if let Some(section) = header.section {
                    self.inner.write_all(b" ")?;
                    self.inner.write_all(section)?;
                }
                self.inner.write_all(b"\n")
            }
            DiffEvent::Line(line) => {
                self.inner.write_all(&[line.kind.marker()])?;
                self.inner.write_all(line.content)?;
                if line.content.ends_with(b"\n") {
                    Ok(())
                } else {
                    self.inner.write_all(b"\n")
                }
            }
            DiffEvent::NoNewlineAtEof => self.inner.write_all(b"\\ No newline at end of file\n"),
        }
    }
}

fn write_prefixed_path<W: Write>(out: &mut W, prefix: &[u8], path: DiffPath<'_>) -> io::Result<()> {
    match path {
        DiffPath::DevNull => out.write_all(b"/dev/null"),
        DiffPath::Path(bytes) => {
            out.write_all(prefix)?;
            out.write_all(bytes)
        }
    }
}

fn write_hunk_range<W: Write>(out: &mut W, prefix: &[u8], range: HunkRange) -> io::Result<()> {
    out.write_all(prefix)?;
    write!(out, "{}", range.start)?;
    if range.len != 1 {
        write!(out, ",{}", range.len)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_match_git_context() {
        let options = DiffRenderOptions::default();

        assert_eq!(options.context_lines, DEFAULT_CONTEXT_LINES);
        assert_eq!(options.algorithm, DiffAlgorithm::Myers);
        assert!(options.emit_file_headers);
    }

    #[test]
    fn unified_writer_streams_borrowed_events() -> io::Result<()> {
        let mut writer = UnifiedDiffWriter::new(Vec::new());
        let path = b"src/lib.rs";
        let section = b"fn demo";
        let events = [
            DiffEvent::FileHeader(FileHeader::modified(path, path)),
            DiffEvent::HunkHeader(HunkHeader {
                old: HunkRange::new(1, 2),
                new: HunkRange::new(1, 2),
                section: Some(section),
            }),
            DiffEvent::Line(DiffLine::new(DiffLineKind::Context, b"same\n")),
            DiffEvent::Line(DiffLine::new(DiffLineKind::Delete, b"old\n")),
            DiffEvent::Line(DiffLine::new(DiffLineKind::Insert, b"new\n")),
        ];

        render_events(events, &mut writer)?;
        let bytes = writer.into_inner();

        assert!(bytes.starts_with(b"diff --git a/src/lib.rs b/src/lib.rs\n"));
        assert!(bytes.ends_with(b" same\n-old\n+new\n"));
        Ok(())
    }
}
