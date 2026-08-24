//! Object-content validation, mirroring upstream git's `fsck.c`
//! (`fsck_commit_buffer`, `fsck_tree`, `fsck_tag_buffer`).
//!
//! Unlike [`sley_object`]'s strict parsers (which reject malformed bytes), this
//! module is a *lenient* validator: it scans the raw object body and emits one
//! typed [`ContentFinding`] per detected problem, with the exact `fsck.<msgid>`
//! message id and detail string that git prints. The caller renders these as
//! `error in <type> <oid>: <msgid>: <detail>` / `warning in <type> <oid>: ...`.

use sley_core::ObjectFormat;
use sley_object::ObjectType;

/// Default severity of an fsck message id, before `fsck.<id>` config overrides.
///
/// Mirrors git's `FOREACH_FSCK_MSG_ID` table: `FATAL`/`ERROR` map to
/// [`Severity::Error`], `WARN` to [`Severity::Warn`], `INFO` to
/// [`DefaultSeverity::Info`] (rendered as a warning but distinct so `--strict` and
/// `fsck.<id>` can promote it), and `IGNORE` to [`Severity::Ignore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSeverity {
    Error,
    Warn,
    Info,
    Ignore,
}

/// The effective severity after config resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warn,
    Ignore,
}

/// A single fsck message id. The string form is the camelCased token git uses
/// for both the printed `<msgid>:` prefix and the `fsck.<msgid>` config key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgId {
    // commit / tag header structure
    NulInHeader,
    UnterminatedHeader,
    BadHeaderContinuation,
    MissingTree,
    BadTreeSha1,
    BadParentSha1,
    MissingAuthor,
    MultipleAuthors,
    MissingCommitter,
    NulInCommit,
    MissingObject,
    BadObjectSha1,
    MissingTypeEntry,
    MissingType,
    BadType,
    MissingTagEntry,
    MissingTag,
    BadTagName,
    MissingTaggerEntry,
    BadGpgsig,
    ExtraHeaderEntry,
    // ident line
    MissingNameBeforeEmail,
    MissingEmail,
    BadName,
    MissingSpaceBeforeEmail,
    BadEmail,
    MissingSpaceBeforeDate,
    BadDate,
    ZeroPaddedDate,
    BadDateOverflow,
    BadTimezone,
    // tree
    NullSha1,
    FullPathname,
    EmptyName,
    HasDot,
    HasDotdot,
    HasDotgit,
    ZeroPaddedFilemode,
    BadFilemode,
    DuplicateEntries,
    TreeNotSorted,
    LargePathname,
    BadTree,
    // gitmodules blob/tree checks
    GitmodulesMissing,
    GitmodulesBlob,
    GitmodulesLarge,
    GitmodulesName,
    GitmodulesParse,
    GitmodulesPath,
    GitmodulesSymlink,
    GitmodulesUpdate,
    GitmodulesUrl,
    // gitattributes blob content (checked when a tree entry names .gitattributes)
    GitattributesMissing,
    GitattributesBlob,
    GitattributesLarge,
    GitattributesLineLength,
    GitattributesSymlink,
    GitignoreSymlink,
    MailmapSymlink,
    // ref consistency (`git refs verify` / `git fsck --references`)
    BadRefName,
    BadRefContent,
    BadRefFiletype,
    BadReferentName,
    BadRefOid,
    BadHeadTarget,
    RefMissingNewline,
    TrailingRefContent,
    SymrefTargetIsNotARef,
    SymlinkRef,
    BadPackedRefHeader,
    BadPackedRefEntry,
    PackedRefEntryNotTerminated,
    PackedRefUnsorted,
    EmptyPackedRefsFile,
    BadReftableTableName,
}

impl MsgId {
    /// The camelCased token git prints and uses as the `fsck.<id>` config key.
    pub const fn camel(self) -> &'static str {
        match self {
            MsgId::NulInHeader => "nulInHeader",
            MsgId::UnterminatedHeader => "unterminatedHeader",
            MsgId::BadHeaderContinuation => "badHeaderContinuation",
            MsgId::MissingTree => "missingTree",
            MsgId::BadTreeSha1 => "badTreeSha1",
            MsgId::BadParentSha1 => "badParentSha1",
            MsgId::MissingAuthor => "missingAuthor",
            MsgId::MultipleAuthors => "multipleAuthors",
            MsgId::MissingCommitter => "missingCommitter",
            MsgId::NulInCommit => "nulInCommit",
            MsgId::MissingObject => "missingObject",
            MsgId::BadObjectSha1 => "badObjectSha1",
            MsgId::MissingTypeEntry => "missingTypeEntry",
            MsgId::MissingType => "missingType",
            MsgId::BadType => "badType",
            MsgId::MissingTagEntry => "missingTagEntry",
            MsgId::MissingTag => "missingTag",
            MsgId::BadTagName => "badTagName",
            MsgId::MissingTaggerEntry => "missingTaggerEntry",
            MsgId::BadGpgsig => "badGpgsig",
            MsgId::ExtraHeaderEntry => "extraHeaderEntry",
            MsgId::MissingNameBeforeEmail => "missingNameBeforeEmail",
            MsgId::MissingEmail => "missingEmail",
            MsgId::BadName => "badName",
            MsgId::MissingSpaceBeforeEmail => "missingSpaceBeforeEmail",
            MsgId::BadEmail => "badEmail",
            MsgId::MissingSpaceBeforeDate => "missingSpaceBeforeDate",
            MsgId::BadDate => "badDate",
            MsgId::ZeroPaddedDate => "zeroPaddedDate",
            MsgId::BadDateOverflow => "badDateOverflow",
            MsgId::BadTimezone => "badTimezone",
            MsgId::NullSha1 => "nullSha1",
            MsgId::FullPathname => "fullPathname",
            MsgId::EmptyName => "emptyName",
            MsgId::HasDot => "hasDot",
            MsgId::HasDotdot => "hasDotdot",
            MsgId::HasDotgit => "hasDotgit",
            MsgId::ZeroPaddedFilemode => "zeroPaddedFilemode",
            MsgId::BadFilemode => "badFilemode",
            MsgId::DuplicateEntries => "duplicateEntries",
            MsgId::TreeNotSorted => "treeNotSorted",
            MsgId::LargePathname => "largePathname",
            MsgId::BadTree => "badTree",
            MsgId::GitmodulesMissing => "gitmodulesMissing",
            MsgId::GitmodulesBlob => "gitmodulesBlob",
            MsgId::GitmodulesLarge => "gitmodulesLarge",
            MsgId::GitmodulesName => "gitmodulesName",
            MsgId::GitmodulesParse => "gitmodulesParse",
            MsgId::GitmodulesPath => "gitmodulesPath",
            MsgId::GitmodulesSymlink => "gitmodulesSymlink",
            MsgId::GitmodulesUpdate => "gitmodulesUpdate",
            MsgId::GitmodulesUrl => "gitmodulesUrl",
            MsgId::GitattributesMissing => "gitattributesMissing",
            MsgId::GitattributesBlob => "gitattributesBlob",
            MsgId::GitattributesLarge => "gitattributesLarge",
            MsgId::GitattributesLineLength => "gitattributesLineLength",
            MsgId::GitattributesSymlink => "gitattributesSymlink",
            MsgId::GitignoreSymlink => "gitignoreSymlink",
            MsgId::MailmapSymlink => "mailmapSymlink",
            MsgId::BadRefName => "badRefName",
            MsgId::BadRefContent => "badRefContent",
            MsgId::BadRefFiletype => "badRefFiletype",
            MsgId::BadReferentName => "badReferentName",
            MsgId::BadRefOid => "badRefOid",
            MsgId::BadHeadTarget => "badHeadTarget",
            MsgId::RefMissingNewline => "refMissingNewline",
            MsgId::TrailingRefContent => "trailingRefContent",
            MsgId::SymrefTargetIsNotARef => "symrefTargetIsNotARef",
            MsgId::SymlinkRef => "symlinkRef",
            MsgId::BadPackedRefHeader => "badPackedRefHeader",
            MsgId::BadPackedRefEntry => "badPackedRefEntry",
            MsgId::PackedRefEntryNotTerminated => "packedRefEntryNotTerminated",
            MsgId::PackedRefUnsorted => "packedRefUnsorted",
            MsgId::EmptyPackedRefsFile => "emptyPackedRefsFile",
            MsgId::BadReftableTableName => "badReftableTableName",
        }
    }

    /// Resolve a config spelling case-insensitively, matching git's
    /// `fsck_msg_id()` lookup for `fsck.<msg-id>` and its fetch/receive peers.
    pub fn from_config_name(name: &str) -> Option<Self> {
        const ALL: &[MsgId] = &[
            MsgId::NulInHeader,
            MsgId::UnterminatedHeader,
            MsgId::BadHeaderContinuation,
            MsgId::MissingTree,
            MsgId::BadTreeSha1,
            MsgId::BadParentSha1,
            MsgId::MissingAuthor,
            MsgId::MultipleAuthors,
            MsgId::MissingCommitter,
            MsgId::NulInCommit,
            MsgId::MissingObject,
            MsgId::BadObjectSha1,
            MsgId::MissingTypeEntry,
            MsgId::MissingType,
            MsgId::BadType,
            MsgId::MissingTagEntry,
            MsgId::MissingTag,
            MsgId::BadTagName,
            MsgId::MissingTaggerEntry,
            MsgId::BadGpgsig,
            MsgId::ExtraHeaderEntry,
            MsgId::MissingNameBeforeEmail,
            MsgId::MissingEmail,
            MsgId::BadName,
            MsgId::MissingSpaceBeforeEmail,
            MsgId::BadEmail,
            MsgId::MissingSpaceBeforeDate,
            MsgId::BadDate,
            MsgId::ZeroPaddedDate,
            MsgId::BadDateOverflow,
            MsgId::BadTimezone,
            MsgId::NullSha1,
            MsgId::FullPathname,
            MsgId::EmptyName,
            MsgId::HasDot,
            MsgId::HasDotdot,
            MsgId::HasDotgit,
            MsgId::ZeroPaddedFilemode,
            MsgId::BadFilemode,
            MsgId::DuplicateEntries,
            MsgId::TreeNotSorted,
            MsgId::LargePathname,
            MsgId::BadTree,
            MsgId::GitmodulesMissing,
            MsgId::GitmodulesBlob,
            MsgId::GitmodulesLarge,
            MsgId::GitmodulesName,
            MsgId::GitmodulesParse,
            MsgId::GitmodulesPath,
            MsgId::GitmodulesSymlink,
            MsgId::GitmodulesUpdate,
            MsgId::GitmodulesUrl,
            MsgId::GitattributesMissing,
            MsgId::GitattributesBlob,
            MsgId::GitattributesLarge,
            MsgId::GitattributesLineLength,
            MsgId::GitattributesSymlink,
            MsgId::GitignoreSymlink,
            MsgId::MailmapSymlink,
            MsgId::BadRefName,
            MsgId::BadRefContent,
            MsgId::BadRefFiletype,
            MsgId::BadReferentName,
            MsgId::BadRefOid,
            MsgId::BadHeadTarget,
            MsgId::RefMissingNewline,
            MsgId::TrailingRefContent,
            MsgId::SymrefTargetIsNotARef,
            MsgId::SymlinkRef,
            MsgId::BadPackedRefHeader,
            MsgId::BadPackedRefEntry,
            MsgId::PackedRefEntryNotTerminated,
            MsgId::PackedRefUnsorted,
            MsgId::EmptyPackedRefsFile,
            MsgId::BadReftableTableName,
        ];
        ALL.iter()
            .copied()
            .find(|message| message.camel().eq_ignore_ascii_case(name))
    }

    /// FATAL fsck messages cannot be demoted through configuration.
    pub const fn cannot_demote(self) -> bool {
        matches!(self, MsgId::NulInHeader | MsgId::UnterminatedHeader)
    }

    /// Default severity, before `fsck.<id>` / `--strict` overrides.
    pub const fn default_severity(self) -> DefaultSeverity {
        match self {
            // FATAL + ERROR in git's table.
            MsgId::NulInHeader
            | MsgId::UnterminatedHeader
            | MsgId::BadHeaderContinuation
            | MsgId::BadGpgsig
            | MsgId::MissingTree
            | MsgId::BadTreeSha1
            | MsgId::BadParentSha1
            | MsgId::MissingAuthor
            | MsgId::MultipleAuthors
            | MsgId::MissingCommitter
            | MsgId::MissingObject
            | MsgId::BadObjectSha1
            | MsgId::MissingTypeEntry
            | MsgId::MissingType
            | MsgId::BadType
            | MsgId::MissingTagEntry
            | MsgId::MissingTag
            | MsgId::MissingNameBeforeEmail
            | MsgId::MissingEmail
            | MsgId::BadName
            | MsgId::MissingSpaceBeforeEmail
            | MsgId::BadEmail
            | MsgId::MissingSpaceBeforeDate
            | MsgId::BadDate
            | MsgId::ZeroPaddedDate
            | MsgId::BadDateOverflow
            | MsgId::BadTimezone
            | MsgId::DuplicateEntries
            | MsgId::TreeNotSorted
            | MsgId::GitmodulesMissing
            | MsgId::GitmodulesBlob
            | MsgId::GitmodulesLarge
            | MsgId::GitmodulesName
            | MsgId::GitmodulesPath
            | MsgId::GitmodulesSymlink
            | MsgId::GitmodulesUpdate
            | MsgId::GitmodulesUrl
            | MsgId::GitattributesMissing
            | MsgId::GitattributesBlob
            | MsgId::GitattributesLarge
            | MsgId::GitattributesLineLength
            | MsgId::BadRefName
            | MsgId::BadRefContent
            | MsgId::BadRefFiletype
            | MsgId::BadReferentName
            | MsgId::BadRefOid
            | MsgId::BadHeadTarget
            | MsgId::BadPackedRefHeader
            | MsgId::BadPackedRefEntry
            | MsgId::PackedRefEntryNotTerminated
            | MsgId::PackedRefUnsorted
            | MsgId::BadTree => DefaultSeverity::Error,
            // WARN in git's table.
            MsgId::NulInCommit
            | MsgId::NullSha1
            | MsgId::FullPathname
            | MsgId::EmptyName
            | MsgId::HasDot
            | MsgId::HasDotdot
            | MsgId::HasDotgit
            | MsgId::ZeroPaddedFilemode
            | MsgId::GitmodulesParse
            | MsgId::GitattributesSymlink
            | MsgId::GitignoreSymlink
            | MsgId::MailmapSymlink
            | MsgId::BadReftableTableName
            | MsgId::LargePathname => DefaultSeverity::Warn,
            // INFO in git's table (rendered as warning, ignored when promoted off).
            MsgId::BadFilemode
            | MsgId::BadTagName
            | MsgId::MissingTaggerEntry
            | MsgId::RefMissingNewline
            | MsgId::TrailingRefContent
            | MsgId::SymrefTargetIsNotARef
            | MsgId::SymlinkRef
            | MsgId::EmptyPackedRefsFile => DefaultSeverity::Info,
            // IGNORE in git's table (only surfaces when elevated by config).
            MsgId::ExtraHeaderEntry => DefaultSeverity::Ignore,
        }
    }
}

/// A detected content problem, before severity resolution. `severity` is the
/// effective severity after applying config (see [`SeverityConfig`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentFinding {
    pub msg_id: MsgId,
    pub severity: Severity,
    pub detail: String,
    /// True once a FATAL/structural problem is hit; the checker stops scanning
    /// the rest of the buffer (git returns -1 immediately). Memory-safety in
    /// git; for us it just means later findings on the same object are skipped.
    pub fatal: bool,
    /// A raw `error: <msg>` line git prints to stderr *before* the formatted
    /// finding (e.g. tree-walk's `empty filename in tree entry`, emitted by
    /// `decode_tree_entry` separately from the `badTree` `report()` line). The
    /// caller prints `error: <raw_stderr>` ahead of the `error in <type> ...`
    /// line. `None` for the common case.
    pub raw_stderr: Option<String>,
}

/// git's default `max_tree_entry_len` (the `largePathname` threshold).
pub const DEFAULT_LARGE_PATHNAME_LEN: usize = 4096;

/// Resolves a message id's effective severity from `fsck.<id>` config plus the
/// `--strict` flag. Built from the repo config by the caller.
#[derive(Debug, Clone)]
pub struct SeverityConfig {
    /// `(camelCasedId, severity)` overrides parsed from `fsck.<id>=<sev>`.
    overrides: Vec<(String, Severity)>,
    /// `--strict`: promotes warnings (and infos) to errors.
    pub strict: bool,
    /// The `largePathname` threshold: a tree entry name longer than this trips
    /// `FSCK_MSG_LARGE_PATHNAME`. Set via `fsck.largePathname=<sev>:<len>`.
    pub large_pathname_len: usize,
}

impl Default for SeverityConfig {
    fn default() -> Self {
        Self {
            overrides: Vec::new(),
            strict: false,
            large_pathname_len: DEFAULT_LARGE_PATHNAME_LEN,
        }
    }
}

impl SeverityConfig {
    pub fn new(strict: bool) -> Self {
        Self {
            strict,
            ..Default::default()
        }
    }

    /// Record a single `fsck.<id>=<value>` override. `value` is one of
    /// `error`, `warn`, `ignore` (case-insensitive); unknown values are ignored
    /// (git errors, but for parity we only need the recognised set).
    ///
    /// `fsck.largePathname` additionally accepts a `<sev>:<len>` form that sets
    /// the maximum tree-entry length (git's `max_tree_entry_len`).
    pub fn set(&mut self, id: &str, value: &str) {
        let lower_id = id.to_ascii_lowercase();
        // largePathname carries an optional `:<len>` threshold suffix.
        let sev_str = if lower_id == "largepathname" {
            if let Some((sev, len)) = value.split_once(':') {
                if let Ok(parsed) = len.trim().parse::<usize>() {
                    self.large_pathname_len = parsed;
                }
                sev
            } else {
                value
            }
        } else {
            value
        };
        let severity = match sev_str.trim().to_ascii_lowercase().as_str() {
            "error" => Severity::Error,
            "warn" | "warning" => Severity::Warn,
            "ignore" => Severity::Ignore,
            _ => return,
        };
        self.overrides.push((lower_id, severity));
    }

    /// Effective severity for `msg_id`, applying overrides then `--strict`.
    pub fn resolve(&self, msg_id: MsgId) -> Severity {
        let canonical = msg_id.camel().to_ascii_lowercase();
        // Last matching override wins (git folds later config over earlier).
        let configured = self
            .overrides
            .iter()
            .rev()
            .find(|(id, _)| *id == canonical)
            .map(|(_, sev)| *sev);
        let default = msg_id.default_severity();
        let (base, strict_promotes) = match configured {
            Some(sev) => (sev, sev == Severity::Warn),
            None => {
                let severity = match default {
                    DefaultSeverity::Error => Severity::Error,
                    DefaultSeverity::Warn | DefaultSeverity::Info => Severity::Warn,
                    DefaultSeverity::Ignore => Severity::Ignore,
                };
                (severity, default == DefaultSeverity::Warn)
            }
        };
        // `--strict` promotes a WARN to ERROR (git: `options->strict`).
        if self.strict && strict_promotes {
            Severity::Error
        } else {
            base
        }
    }
}

/// git's `ATTR_MAX_LINE_LENGTH` (attr.h): a `.gitattributes` line at or over
/// this length is unparseable.
pub const ATTR_MAX_LINE_LENGTH: usize = 2048;
/// git's `ATTR_MAX_FILE_SIZE` (attr.h): a `.gitattributes` blob at or over this
/// size is too large to parse.
pub const ATTR_MAX_FILE_SIZE: usize = 100 * 1024 * 1024;

/// Content-check a blob that a tree entry named `.gitattributes`, mirroring
/// git's `fsck_blob` gitattributes branch: a blob at/over `ATTR_MAX_FILE_SIZE`
/// is `gitattributesLarge`; otherwise the first line at/over
/// `ATTR_MAX_LINE_LENGTH` is `gitattributesLineLength`. Returns the resolved
/// findings (rendered as `error in blob <oid>: <msgid>: <detail>`).
pub fn check_gitattributes_blob(body: &[u8], config: &SeverityConfig) -> Vec<ContentFinding> {
    let mut raw = Vec::new();
    if body.len() >= ATTR_MAX_FILE_SIZE {
        raw.push(finding(
            MsgId::GitattributesLarge,
            ".gitattributes too large to parse",
            false,
        ));
    } else {
        // git scans up to the NUL terminator of the in-memory buffer; a blob
        // body has no implicit NUL, so we walk the whole body line by line.
        let mut start = 0usize;
        while start < body.len() {
            let eol = body[start..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|off| start + off)
                .unwrap_or(body.len());
            if eol - start >= ATTR_MAX_LINE_LENGTH {
                raw.push(finding(
                    MsgId::GitattributesLineLength,
                    ".gitattributes has too long lines to parse",
                    false,
                ));
                break;
            }
            start = if eol < body.len() { eol + 1 } else { eol };
        }
    }
    raw.retain_mut(|finding| {
        finding.severity = config.resolve(finding.msg_id);
        finding.severity != Severity::Ignore
    });
    raw
}

/// Whether a tree-entry name is `.gitattributes` (HFS/NTFS spellings included,
/// mirroring git's `is_hfs_dotgitattributes`/`is_ntfs_dotgitattributes`). For
/// the parity suite the plain ASCII form is what the tests exercise.
pub fn is_dotgitattributes_name(name: &[u8]) -> bool {
    is_hfs_dot_name(name, "gitattributes") || is_ntfs_dot_name(name, "gitattributes", "gi7d29")
}

pub fn is_dotgitmodules_name(name: &[u8]) -> bool {
    is_hfs_dot_name(name, "gitmodules") || is_ntfs_dot_name(name, "gitmodules", "gi7eba")
}

pub fn is_dotgitignore_name(name: &[u8]) -> bool {
    is_hfs_dot_name(name, "gitignore") || is_ntfs_dot_name(name, "gitignore", "gi250a")
}

pub fn is_dotmailmap_name(name: &[u8]) -> bool {
    is_hfs_dot_name(name, "mailmap") || is_ntfs_dot_name(name, "mailmap", "maba30")
}

fn is_hfs_dot_name(name: &[u8], needle: &str) -> bool {
    let Ok(text) = std::str::from_utf8(name) else {
        return false;
    };
    let folded: String = text.chars().filter(|ch| !is_hfs_ignorable(*ch)).collect();
    folded.eq_ignore_ascii_case(&format!(".{needle}"))
}

fn is_ntfs_dot_name(name: &[u8], needle: &str, short_prefix: &str) -> bool {
    for segment in name.split(|&byte| byte == b'\\') {
        let stream_name = segment
            .iter()
            .position(|&byte| byte == b':')
            .map_or(segment, |colon| &segment[..colon]);
        if ntfs_long_name_matches(stream_name, needle)
            || ntfs_short_name_matches(stream_name, needle, short_prefix)
        {
            return true;
        }
    }
    false
}

fn ntfs_long_name_matches(name: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    if name.len() < needle.len() + 1 || name[0] != b'.' {
        return false;
    }
    if !name[1..1 + needle.len()].eq_ignore_ascii_case(needle) {
        return false;
    }
    ntfs_suffix_is_ignorable(&name[1 + needle.len()..])
}

fn ntfs_short_name_matches(name: &[u8], needle: &str, short_prefix: &str) -> bool {
    let prefix = needle.as_bytes();
    if prefix.len() >= 6
        && name.len() >= 8
        && name[..6].eq_ignore_ascii_case(&prefix[..6])
        && name[6] == b'~'
        && matches!(name[7], b'1'..=b'4')
    {
        return ntfs_suffix_is_ignorable(&name[8..]);
    }

    let short = short_prefix.as_bytes();
    if name.len() < 8 {
        return false;
    }
    let mut saw_tilde = false;
    for i in 0..8 {
        let c = name[i];
        if c == 0 || c & 0x80 != 0 {
            return false;
        }
        if saw_tilde {
            if !c.is_ascii_digit() {
                return false;
            }
        } else if c == b'~' {
            if i + 1 >= 8 || !matches!(name[i + 1], b'1'..=b'9') {
                return false;
            }
            saw_tilde = true;
        } else if i >= 6 || ![c].eq_ignore_ascii_case(&[short[i]]) {
            return false;
        }
    }
    saw_tilde && ntfs_suffix_is_ignorable(&name[8..])
}

fn ntfs_suffix_is_ignorable(mut suffix: &[u8]) -> bool {
    if let Some(colon) = suffix.iter().position(|&byte| byte == b':') {
        suffix = &suffix[..colon];
    }
    suffix.iter().all(|&byte| byte == b'.' || byte == b' ')
}

/// Validate a loaded object body, returning every content finding whose
/// resolved severity is not [`Severity::Ignore`].
pub fn check_object_content(
    object_type: ObjectType,
    body: &[u8],
    config: &SeverityConfig,
) -> Vec<ContentFinding> {
    check_object_content_with_format(
        infer_object_format(object_type, body),
        object_type,
        body,
        config,
    )
}

/// Validate a loaded object body using the repository's explicit object
/// format. Commit, tag, and tree object ids have different widths under SHA-1
/// and SHA-256, so callers that own repository context should always use this
/// entry point.
pub fn check_object_content_with_format(
    format: ObjectFormat,
    object_type: ObjectType,
    body: &[u8],
    config: &SeverityConfig,
) -> Vec<ContentFinding> {
    let mut raw = match object_type {
        ObjectType::Commit => check_commit(format, body),
        ObjectType::Tag => check_tag(format, body),
        ObjectType::Tree => check_tree(format, body, config.large_pathname_len),
        ObjectType::Blob => Vec::new(),
    };
    // Resolve severities, apply tag-specific report-overwrite masking, then drop
    // ignored findings while preserving printed order.
    for finding in &mut raw {
        finding.severity = config.resolve(finding.msg_id);
    }
    mask_tag_ident_error_if_extra_header_overwrites_return(object_type, &mut raw);
    raw.retain(|finding| finding.severity != Severity::Ignore);
    raw
}

/// The tag/commit identity-line message ids whose fsck_ident report falls
/// through in `fsck_tag_standalone` instead of short-circuiting (`goto done`).
pub(crate) fn is_tag_ident_msg(msg_id: MsgId) -> bool {
    matches!(
        msg_id,
        MsgId::MissingNameBeforeEmail
            | MsgId::MissingEmail
            | MsgId::BadName
            | MsgId::MissingSpaceBeforeEmail
            | MsgId::BadEmail
            | MsgId::MissingSpaceBeforeDate
            | MsgId::BadDate
            | MsgId::ZeroPaddedDate
            | MsgId::BadDateOverflow
            | MsgId::BadTimezone
    )
}

/// Outcome of `git hash-object`'s format check over one object body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashFormatCheckReport {
    /// Diagnostics in print order. Each may carry a raw tree-walk stderr line
    /// (`raw_stderr`) that precedes its formatted line.
    pub diagnostics: Vec<ContentFinding>,
    /// True when git refuses to create the object (`fsck_buffer` returned
    /// non-zero ⇒ `fatal: refusing to create malformed object`, exit 128).
    /// False even when `diagnostics` is non-empty for the one case where git
    /// prints but still hashes: a tag whose tagger-ident error is overwritten
    /// by a following extra-header report (IGNORE ⇒ ret 0).
    pub refuse: bool,
}

/// Diagnostics for `git hash-object`'s format check — object-file.c's
/// `index_mem` pass under `INDEX_FORMAT_CHECK`, which frames the bytes as
/// `object_type` and runs strict fsck before hashing (`--literally` skips it).
///
/// Semantics differ from [`check_object_content_with_format`] in three ways,
/// all upstream-exact:
///
/// * No repository config applies (`index_mem` never reads `fsck.<id>`), but
///   `opts.strict = 1`, so WARN-severity ids are promoted; every non-IGNORE
///   finding prints and fails (`hash_format_check_report` returns 1).
/// * Tag control flow mirrors `fsck_tag_standalone`: a badTagName or missing
///   tagger stops the scan (`goto done` on its non-zero report), while a
///   tagger-ident error falls through and is *overwritten* by a following
///   extra-header report (IGNORE ⇒ 0) — git prints the ident diagnostic but
///   still hashes that object ([`HashFormatCheckReport::refuse`] is false).
/// * Findings come back in print order; nothing past the first hash-lethal
///   tag finding appears, because git's `goto done` never reaches it.
///
/// The caller prints each diagnostic as `error: object fails fsck:
/// <camelCasedMsgId>: <detail>` and dies only when `report.refuse` is set.
pub fn hash_format_check(
    format: ObjectFormat,
    object_type: ObjectType,
    body: &[u8],
) -> HashFormatCheckReport {
    // Blobs are always valid: git's fsck has no blob checks.
    let mut findings = match object_type {
        ObjectType::Blob => Vec::new(),
        _ => check_object_content_with_format(
            format,
            object_type,
            body,
            // opts.strict = 1, no fsck.* overrides.
            &SeverityConfig::new(true),
        ),
    };
    let refuse = if object_type == ObjectType::Tag {
        // A surviving hash-lethal finding fails; a tail of masked ident
        // findings (fatal cleared by the extra-header overwrite) only prints.
        truncate_tag_findings_at_hash_lethal(&mut findings)
    } else {
        !findings.is_empty()
    };
    HashFormatCheckReport {
        diagnostics: findings,
        refuse,
    }
}

/// Reproduce `fsck_tag_standalone`'s return-value bookkeeping for the
/// hash-object path, where every printed report returns 1: everything except
/// the fall-through tagger-ident report short-circuits via `goto done`, so any
/// finding after the first non-fall-through one is never reached (and must not
/// be printed). A fall-through ident report survives to the end only when a
/// later extra header overwrote it to zero — exactly the ident findings whose
/// masking cleared `fatal`. Returns true when a hash-lethal finding remains.
fn truncate_tag_findings_at_hash_lethal(findings: &mut Vec<ContentFinding>) -> bool {
    for idx in 0..findings.len() {
        if !(is_tag_ident_msg(findings[idx].msg_id) && !findings[idx].fatal) {
            findings.truncate(idx + 1);
            return true;
        }
    }
    false
}

/// Compatibility inference for content-only callers that do not own repository
/// context. Valid commit/tag headers are unambiguous; malformed headers retain
/// the historical SHA-1 default and still produce their existing fsck finding.
/// Raw trees cannot reliably advertise their hash format, so their legacy path
/// also remains SHA-1. Repository-aware callers use
/// [`check_object_content_with_format`] instead.
fn infer_object_format(object_type: ObjectType, body: &[u8]) -> ObjectFormat {
    let prefix = match object_type {
        ObjectType::Commit => b"tree ".as_slice(),
        ObjectType::Tag => b"object ".as_slice(),
        ObjectType::Tree | ObjectType::Blob => return ObjectFormat::Sha1,
    };
    let Some(value) = body.strip_prefix(prefix) else {
        return ObjectFormat::Sha1;
    };
    let hex_len = value.iter().position(|byte| *byte == b'\n').unwrap_or(0);
    if hex_len == ObjectFormat::Sha256.hex_len()
        && value[..hex_len].iter().all(u8::is_ascii_hexdigit)
    {
        ObjectFormat::Sha256
    } else {
        ObjectFormat::Sha1
    }
}

fn mask_tag_ident_error_if_extra_header_overwrites_return(
    object_type: ObjectType,
    findings: &mut [ContentFinding],
) {
    if object_type != ObjectType::Tag {
        return;
    }
    let Some(extra_idx) = findings
        .iter()
        .position(|finding| finding.msg_id == MsgId::ExtraHeaderEntry)
    else {
        return;
    };
    if findings[extra_idx].severity == Severity::Error {
        return;
    }
    for finding in &mut findings[..extra_idx] {
        if is_tag_ident_msg(finding.msg_id) {
            finding.fatal = false;
        }
    }
}

fn finding(msg_id: MsgId, detail: impl Into<String>, fatal: bool) -> ContentFinding {
    ContentFinding {
        msg_id,
        // Placeholder; resolved by `check_object_content`.
        severity: Severity::Error,
        detail: detail.into(),
        fatal,
        raw_stderr: None,
    }
}

/// A finding that carries a preceding raw `error: <raw>` stderr line, mirroring
/// git's tree-walk `error()` calls that fire before the `report()` finding.
fn finding_with_raw(
    msg_id: MsgId,
    detail: impl Into<String>,
    fatal: bool,
    raw: impl Into<String>,
) -> ContentFinding {
    ContentFinding {
        raw_stderr: Some(raw.into()),
        ..finding(msg_id, detail, fatal)
    }
}

/// Mirror git's `verify_headers`: the header block must end with `\n\n` or at
/// least a trailing `\n`, and contain no NUL. Returns a fatal finding on
/// failure (the caller stops parsing), else `None`.
fn verify_headers(body: &[u8]) -> Option<ContentFinding> {
    for (i, &b) in body.iter().enumerate() {
        match b {
            0 => {
                return Some(finding(
                    MsgId::NulInHeader,
                    format!("unterminated header: NUL at offset {i}"),
                    true,
                ));
            }
            b'\n' if body.get(i + 1) == Some(&b'\n') => return None,
            _ => {}
        }
    }
    if body.last() == Some(&b'\n') {
        return None;
    }
    Some(finding(
        MsgId::UnterminatedHeader,
        "unterminated header",
        true,
    ))
}

/// True if `b` is an ASCII digit (git uses C `isdigit`).
fn is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}

/// Mirror git's `fsck_ident`: validate one `author`/`committer`/`tagger` value
/// (the bytes *after* the `"author "` prefix, including the trailing `\n`).
/// `p` points at the start of the ident value within `body`; returns the first
/// finding or `None` if the ident is well-formed. On success advances nothing
/// (the caller re-derives the next line).
fn fsck_ident(body: &[u8], start: usize) -> Result<usize, ContentFinding> {
    // verify_headers guarantees a newline exists from here.
    let nl = body[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|off| start + off)
        .expect("verify_headers guarantees a newline");
    let next = nl + 1;
    let line_end = nl; // exclusive end of the ident value (the '\n')

    let mut p = start;
    if body.get(p) == Some(&b'<') {
        return Err(finding(
            MsgId::MissingNameBeforeEmail,
            "invalid author/committer line - missing space before email",
            true,
        ));
    }
    // Scan the name up to '<'.
    loop {
        if p >= line_end {
            return Err(finding(
                MsgId::MissingEmail,
                "invalid author/committer line - missing email",
                true,
            ));
        }
        match body[p] {
            b'\n' => {
                return Err(finding(
                    MsgId::MissingEmail,
                    "invalid author/committer line - missing email",
                    true,
                ));
            }
            b'>' => {
                return Err(finding(
                    MsgId::BadName,
                    "invalid author/committer line - bad name",
                    true,
                ));
            }
            b'<' => break,
            _ => p += 1,
        }
    }
    // p is at '<'; the byte before must be a space.
    if p == start || body[p - 1] != b' ' {
        return Err(finding(
            MsgId::MissingSpaceBeforeEmail,
            "invalid author/committer line - missing space before email",
            true,
        ));
    }
    p += 1; // past '<'
    // Scan the email up to '>'.
    loop {
        if p >= line_end || body[p] == b'<' || body[p] == b'\n' {
            return Err(finding(
                MsgId::BadEmail,
                "invalid author/committer line - bad email",
                true,
            ));
        }
        if body[p] == b'>' {
            break;
        }
        p += 1;
    }
    p += 1; // past '>'
    if body.get(p) != Some(&b' ') {
        return Err(finding(
            MsgId::MissingSpaceBeforeDate,
            "invalid author/committer line - missing space before date",
            true,
        ));
    }
    p += 1;
    // Skip linear whitespace (space/tab) but not newlines.
    while body.get(p) == Some(&b' ') || body.get(p) == Some(&b'\t') {
        p += 1;
    }
    if p >= body.len() || !is_digit(body[p]) {
        return Err(finding(
            MsgId::BadDate,
            "invalid author/committer line - bad date",
            true,
        ));
    }
    if body[p] == b'0' && body.get(p + 1) != Some(&b' ') {
        return Err(finding(
            MsgId::ZeroPaddedDate,
            "invalid author/committer line - zero-padded date",
            true,
        ));
    }
    // Parse the timestamp; git treats >2^64 / overflow as date overflow.
    let date_start = p;
    while p < body.len() && is_digit(body[p]) {
        p += 1;
    }
    let digits = &body[date_start..p];
    if timestamp_overflows(digits) {
        return Err(finding(
            MsgId::BadDateOverflow,
            "invalid author/committer line - date causes integer overflow",
            true,
        ));
    }
    if body.get(p) != Some(&b' ') {
        return Err(finding(
            MsgId::BadDate,
            "invalid author/committer line - bad date",
            true,
        ));
    }
    p += 1;
    // Timezone: [+-]NNNN\n
    let tz_ok = matches!(body.get(p), Some(b'+') | Some(b'-'))
        && body.get(p + 1).is_some_and(|b| is_digit(*b))
        && body.get(p + 2).is_some_and(|b| is_digit(*b))
        && body.get(p + 3).is_some_and(|b| is_digit(*b))
        && body.get(p + 4).is_some_and(|b| is_digit(*b))
        && body.get(p + 5) == Some(&b'\n');
    if !tz_ok {
        return Err(finding(
            MsgId::BadTimezone,
            "invalid author/committer line - bad time zone",
            true,
        ));
    }
    Ok(next)
}

/// Git's `date_overflows` over `parse_timestamp_from_buf`. `timestamp_t` is
/// `uintmax_t`; the parse fills a 24-byte buffer, so a 24th digit short-circuits
/// to `TIME_MAX` (`UINTMAX_MAX`) which always overflows. Smaller inputs go
/// through `strtoumax` (out-of-range clamps to `TIME_MAX`) and then the
/// `time_t` round-trip in `date_overflows`, so any value above `INT64_MAX`
/// fails to convert on 64-bit time_t. Verified against `git hash-object -t
/// commit`: `9223372036854775807` hashes, `9223372036854775808` reports
/// `badDateOverflow`.
fn timestamp_overflows(digits: &[u8]) -> bool {
    if digits.len() >= 24 {
        return true;
    }
    let text = std::str::from_utf8(digits).unwrap_or("");
    match text.parse::<u64>() {
        Ok(value) => value > i64::MAX as u64,
        Err(_) => true,
    }
}

fn check_commit(format: ObjectFormat, body: &[u8]) -> Vec<ContentFinding> {
    let mut out = Vec::new();
    if let Some(f) = verify_headers(body) {
        out.push(f);
        return out;
    }
    let mut pos = 0usize;

    // tree line
    match strip_line_prefix(body, pos, b"tree ") {
        Some(after) => {
            // Validate the sha1 + trailing newline.
            if !valid_oid_line(format, body, after) {
                out.push(finding(
                    MsgId::BadTreeSha1,
                    "invalid 'tree' line format - bad sha1",
                    true,
                ));
                return out;
            }
            pos = line_end(body, after) + 1;
        }
        None => {
            out.push(finding(
                MsgId::MissingTree,
                "invalid format - expected 'tree' line",
                true,
            ));
            return out;
        }
    }

    // zero or more parent lines
    while let Some(after) = strip_line_prefix(body, pos, b"parent ") {
        if !valid_oid_line(format, body, after) {
            out.push(finding(
                MsgId::BadParentSha1,
                "invalid 'parent' line format - bad sha1",
                true,
            ));
            return out;
        }
        pos = line_end(body, after) + 1;
    }

    // one or more author lines
    let mut author_count = 0usize;
    while let Some(after) = strip_line_prefix(body, pos, b"author ") {
        author_count += 1;
        match fsck_ident(body, after) {
            Ok(next) => pos = next,
            Err(f) => {
                out.push(f);
                return out;
            }
        }
    }
    if author_count < 1 {
        out.push(finding(
            MsgId::MissingAuthor,
            "invalid format - expected 'author' line",
            true,
        ));
        return out;
    } else if author_count > 1 {
        out.push(finding(
            MsgId::MultipleAuthors,
            "invalid format - multiple 'author' lines",
            true,
        ));
        return out;
    }

    // committer line
    match strip_line_prefix(body, pos, b"committer ") {
        Some(after) => match fsck_ident(body, after) {
            Ok(_) => {}
            Err(f) => {
                out.push(f);
                return out;
            }
        },
        None => {
            out.push(finding(
                MsgId::MissingCommitter,
                "invalid format - expected 'committer' line",
                true,
            ));
            return out;
        }
    }

    // NUL anywhere in the body (git checks the *whole* object body here, not
    // just headers; verify_headers only catches NUL before the header end).
    if body.contains(&0) {
        out.push(finding(
            MsgId::NulInCommit,
            "NUL byte in the commit object body",
            false,
        ));
    }
    out
}

fn check_tag(format: ObjectFormat, body: &[u8]) -> Vec<ContentFinding> {
    let mut out = Vec::new();
    if let Some(f) = verify_headers(body) {
        out.push(f);
        return out;
    }
    let mut pos = 0usize;

    // object line
    match strip_line_prefix(body, pos, b"object ") {
        Some(after) => {
            if !valid_oid_line(format, body, after) {
                out.push(finding(
                    MsgId::BadObjectSha1,
                    "invalid 'object' line format - bad sha1",
                    true,
                ));
                return out;
            }
            pos = line_end(body, after) + 1;
        }
        None => {
            out.push(finding(
                MsgId::MissingObject,
                "invalid format - expected 'object' line",
                true,
            ));
            return out;
        }
    }

    // type line
    match strip_line_prefix(body, pos, b"type ") {
        Some(after) => {
            let eol = match memchr(body, after, b'\n') {
                Some(e) => e,
                None => {
                    out.push(finding(
                        MsgId::MissingType,
                        "invalid format - unexpected end after 'type' line",
                        true,
                    ));
                    return out;
                }
            };
            let type_str = &body[after..eol];
            if !is_known_object_type(type_str) {
                out.push(finding(MsgId::BadType, "invalid 'type' value", true));
                return out;
            }
            pos = eol + 1;
        }
        None => {
            out.push(finding(
                MsgId::MissingTypeEntry,
                "invalid format - expected 'type' line",
                true,
            ));
            return out;
        }
    }

    // tag line
    match strip_line_prefix(body, pos, b"tag ") {
        Some(after) => {
            let eol = match memchr(body, after, b'\n') {
                Some(e) => e,
                None => {
                    out.push(finding(
                        MsgId::MissingTag,
                        "invalid format - unexpected end after 'type' line",
                        true,
                    ));
                    return out;
                }
            };
            let name = &body[after..eol];
            if !valid_tag_name(name) {
                let name_str = String::from_utf8_lossy(name);
                out.push(finding(
                    MsgId::BadTagName,
                    format!("invalid 'tag' name: {name_str}"),
                    false,
                ));
            }
            pos = eol + 1;
        }
        None => {
            out.push(finding(
                MsgId::MissingTagEntry,
                "invalid format - expected 'tag' line",
                true,
            ));
            return out;
        }
    }

    // tagger line (early tags omit it: warn, then keep going)
    match strip_line_prefix(body, pos, b"tagger ") {
        Some(after) => match fsck_ident(body, after) {
            Ok(next) => pos = next,
            Err(f) => {
                out.push(f);
                pos = line_end(body, after) + 1;
            }
        },
        None => {
            out.push(finding(
                MsgId::MissingTaggerEntry,
                "invalid format - expected 'tagger' line",
                false,
            ));
            // git continues scanning for the gpgsig / extra-header checks.
        }
    }

    // optional gpgsig / gpgsig-sha256 header + continuation lines
    let after_sig = strip_line_prefix(body, pos, b"gpgsig ")
        .or_else(|| strip_line_prefix(body, pos, b"gpgsig-sha256 "));
    if let Some(after) = after_sig {
        match memchr(body, after, b'\n') {
            None => {
                out.push(finding(
                    MsgId::BadGpgsig,
                    "invalid format - unexpected end after 'gpgsig' or 'gpgsig-sha256' line",
                    true,
                ));
                return out;
            }
            Some(eol) => {
                pos = eol + 1;
                // continuation lines start with a space
                while pos < body.len() && body.get(pos) == Some(&b' ') {
                    match memchr(body, pos, b'\n') {
                        None => {
                            out.push(finding(
                                MsgId::BadHeaderContinuation,
                                "invalid format - unexpected end in 'gpgsig' or 'gpgsig-sha256' continuation line",
                                true,
                            ));
                            return out;
                        }
                        Some(eol) => pos = eol + 1,
                    }
                }
            }
        }
    }

    // After the tagger (+ optional gpgsig), the next thing must be the blank
    // line that starts the message; anything else is an extra header.
    if pos < body.len() && body.get(pos) != Some(&b'\n') {
        out.push(finding(
            MsgId::ExtraHeaderEntry,
            "invalid format - extra header(s) after 'tagger'",
            false,
        ));
    }
    out
}

/// One raw tree entry produced by [`scan_tree_entry`] (git's
/// `decode_tree_entry` / `tree_entry_extract`).
struct ScannedEntry<'a> {
    /// Offset of this entry's start (the first mode digit).
    start: usize,
    mode: u32,
    name: &'a [u8],
    oid: &'a [u8],
    /// Offset just past this entry, i.e. the successor's start.
    next: usize,
}

/// Decode the tree entry beginning at `start`, or `None` when git's
/// `decode_tree_entry` would fail (the matching stderr wording is recoverable
/// via [`tree_decode_error`]).
fn scan_tree_entry(body: &[u8], start: usize, oid_len: usize) -> Option<ScannedEntry<'_>> {
    let mut pos = start;
    // parse_mode: octal digits up to a space (NUL also stops the scan so an
    // empty/terminated mode fails below).
    while pos < body.len() && body[pos] != b' ' && body[pos] != 0 {
        pos += 1;
    }
    if pos >= body.len() || body[pos] != b' ' {
        return None;
    }
    let mode = parse_octal_mode(&body[start..pos])?;
    pos += 1; // past space
    let name_start = pos;
    while pos < body.len() && body[pos] != 0 {
        pos += 1;
    }
    if pos >= body.len() {
        return None;
    }
    let name = &body[name_start..pos];
    // `if (!*path)` — decode rejects an empty filename outright.
    if name.is_empty() {
        return None;
    }
    pos += 1; // past NUL
    if pos + oid_len > body.len() {
        return None;
    }
    Some(ScannedEntry {
        start,
        mode,
        name,
        oid: &body[pos..pos + oid_len],
        next: pos + oid_len,
    })
}

/// The plain `error: <msg>` wording git's tree-walk decoder prints before
/// `fsck_tree` reports `badTree` (`decode_tree_entry` in tree-walk.c). Mirrors
/// its three checks in order.
fn tree_decode_error(remaining: &[u8], oid_len: usize) -> &'static str {
    if remaining.len() < oid_len + 3 || remaining[remaining.len() - (oid_len + 1)] != 0 {
        return "too-short tree object";
    }
    if remaining[0] == b' ' {
        return "malformed mode in tree entry";
    }
    let mut idx = 0usize;
    loop {
        match remaining.get(idx) {
            Some(b' ') => break,
            Some(byte) if (b'0'..=b'7').contains(byte) => idx += 1,
            _ => return "malformed mode in tree entry",
        }
    }
    idx += 1; // past space; the path starts here
    if remaining.get(idx) == Some(&0) {
        return "empty filename in tree entry";
    }
    "too-short tree object"
}

fn check_tree(format: ObjectFormat, body: &[u8], large_pathname_len: usize) -> Vec<ContentFinding> {
    // Walk raw tree entries: "<mode> <name>\0<20-or-32-byte-oid>".
    let oid_len = format.raw_len();
    let mut out = Vec::new();

    let mut has_null_sha1 = false;
    let mut has_full_path = false;
    let mut has_empty_name = false;
    let mut has_dot = false;
    let mut has_dotdot = false;
    let mut has_dotgit = false;
    let mut has_zero_pad = false;
    let mut has_bad_modes = false;
    let mut has_dup_entries = false;
    let mut not_sorted = false;
    let mut has_large_name = false;

    // An empty buffer is a valid (empty) tree; `init_tree_desc_internal`
    // decodes nothing and `fsck_tree` returns zero.
    if body.is_empty() {
        return out;
    }

    // git's `df_dup_candidates`: non-directory names awaiting a later directory
    // that would collide via the implicitly-added '/'.
    let mut df_candidates: Vec<Vec<u8>> = Vec::new();
    let mut prev: Option<(u32, Vec<u8>)> = None;

    // init_tree_desc_gently decodes the first entry up front; a failure there
    // reports badTree and returns immediately (no property findings exist yet).
    let mut current = match scan_tree_entry(body, 0, oid_len) {
        Some(entry) => entry,
        None => {
            out.push(finding_with_raw(
                MsgId::BadTree,
                "cannot be parsed as a tree",
                true,
                tree_decode_error(body, oid_len),
            ));
            return out;
        }
    };

    // Set when advancing to the successor fails mid-walk: git prints badTree
    // *during* the loop (before the post-walk property reports) and breaks —
    // skipping the mode and ordering checks for the current entry.
    let mut walk_bad_tree: Option<ContentFinding> = None;

    loop {
        // Per-entry flag accumulation (runs for every extracted entry, even one
        // whose successor later fails to decode).
        if current.oid.iter().all(|&b| b == 0) {
            has_null_sha1 = true;
        }
        if current.name.is_empty() {
            has_empty_name = true;
        }
        let name = current.name;
        if name.contains(&b'/') {
            has_full_path = true;
        }
        if name == b"." {
            has_dot = true;
        }
        if name == b".." {
            has_dotdot = true;
        }
        if is_dotgit_name(name) {
            has_dotgit = true;
        }
        // NTFS treats `\` as a path separator, so every backslash-delimited
        // segment (including the leading one) can independently be `.git`:
        // `.git\foobar` and `foo\.git` both trip the check. git's
        // `is_ntfs_dotgit` inspects the name up to the first `\`, then its
        // caller re-checks each subsequent segment.
        if name.contains(&b'\\') {
            for seg in name.split(|&b| b == b'\\') {
                if is_dotgit_name(seg) {
                    has_dotgit = true;
                }
            }
        }
        // `has_zero_pad |= *(char *)desc.buffer == '0'`: the entry-start byte.
        if body[current.start] == b'0' {
            has_zero_pad = true;
        }
        if name.len() > large_pathname_len {
            has_large_name = true;
        }

        // Advance (update_tree_entry_gently): git runs this BEFORE the mode and
        // ordering checks, and on a decode failure reports badTree and breaks —
        // so those checks are skipped for the current entry. A clean
        // end-of-buffer advance still counts as success.
        let mut next_entry = None;
        let advance_failed = current.next < body.len()
            && match scan_tree_entry(body, current.next, oid_len) {
                Some(next) => {
                    next_entry = Some(next);
                    false
                }
                None => {
                    walk_bad_tree = Some(finding_with_raw(
                        MsgId::BadTree,
                        "cannot be parsed as a tree",
                        true,
                        tree_decode_error(&body[current.next..], oid_len),
                    ));
                    true
                }
            };

        if advance_failed {
            break;
        }

        // switch (mode): standard modes only.
        if !is_valid_mode(current.mode) {
            has_bad_modes = true;
        }
        if let Some((prev_mode, prev_name)) = prev.as_ref() {
            match verify_ordered(
                *prev_mode,
                prev_name,
                current.mode,
                current.name,
                &mut df_candidates,
            ) {
                Ordering2::Equal => has_dup_entries = true,
                Ordering2::Unordered => not_sorted = true,
                Ordering2::Ordered => {}
            }
        }
        prev = Some((current.mode, current.name.to_vec()));

        match next_entry {
            Some(next) => current = next,
            None => break, // end of buffer reached
        }
    }

    // git emits badTree at its in-loop position, then the accumulated property
    // reports in fixed order.
    if let Some(bad_tree) = walk_bad_tree {
        out.push(bad_tree);
    }
    if has_null_sha1 {
        out.push(finding(
            MsgId::NullSha1,
            "contains entries pointing to null sha1",
            false,
        ));
    }
    if has_full_path {
        out.push(finding(
            MsgId::FullPathname,
            "contains full pathnames",
            false,
        ));
    }
    if has_empty_name {
        out.push(finding(MsgId::EmptyName, "contains empty pathname", false));
    }
    if has_dot {
        out.push(finding(MsgId::HasDot, "contains '.'", false));
    }
    if has_dotdot {
        out.push(finding(MsgId::HasDotdot, "contains '..'", false));
    }
    if has_dotgit {
        out.push(finding(MsgId::HasDotgit, "contains '.git'", false));
    }
    if has_zero_pad {
        out.push(finding(
            MsgId::ZeroPaddedFilemode,
            "contains zero-padded file modes",
            false,
        ));
    }
    if has_bad_modes {
        out.push(finding(
            MsgId::BadFilemode,
            "contains bad file modes",
            false,
        ));
    }
    if has_dup_entries {
        out.push(finding(
            MsgId::DuplicateEntries,
            "contains duplicate file entries",
            false,
        ));
    }
    if not_sorted {
        out.push(finding(MsgId::TreeNotSorted, "not properly sorted", false));
    }
    if has_large_name {
        out.push(finding(
            MsgId::LargePathname,
            "contains excessively large pathname",
            false,
        ));
    }
    out
}

// --- byte-scanning helpers --------------------------------------------------

/// If `body[pos..]` starts with `prefix`, return the index just past it.
fn strip_line_prefix(body: &[u8], pos: usize, prefix: &[u8]) -> Option<usize> {
    if pos < body.len() && body[pos..].starts_with(prefix) {
        Some(pos + prefix.len())
    } else {
        None
    }
}

/// Index of the next `\n` at or after `from`, or the end of the buffer.
fn line_end(body: &[u8], from: usize) -> usize {
    memchr(body, from, b'\n').unwrap_or(body.len())
}

fn memchr(body: &[u8], from: usize, needle: u8) -> Option<usize> {
    body.get(from..)
        .and_then(|s| s.iter().position(|&b| b == needle))
        .map(|off| from + off)
}

/// A `<hex-oid>\n` line: hex chars of the right length followed immediately by
/// a newline.
fn valid_oid_line(format: ObjectFormat, body: &[u8], from: usize) -> bool {
    let oid_hex_len = format.hex_len();
    let end = from + oid_hex_len;
    body.len() > end && body[from..end].iter().all(u8::is_ascii_hexdigit) && body[end] == b'\n'
}

fn is_known_object_type(s: &[u8]) -> bool {
    matches!(s, b"blob" | b"tree" | b"commit" | b"tag")
}

/// A tag name is valid iff `refs/tags/<name>` is a valid refname. We mirror the
/// subset of `check_refname_format` git applies: reject names with spaces, with
/// components that are `.`/`..`/empty, ending in `.lock`, containing control
/// chars or any of ` ~^:?*[\` or `..` or `@{`, or a trailing `/` or `.`.
fn valid_tag_name(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    // No control chars, no DEL, none of the forbidden punctuation, no space.
    for &b in name {
        if b < 0x20 || b == 0x7f {
            return false;
        }
        if matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\') {
            return false;
        }
    }
    // No "..", no "@{", no leading/trailing '/', no "//", no trailing '.',
    // no ".lock" suffix on any component, no component beginning with '.'.
    if name.windows(2).any(|w| w == b".." || w == b"@{") {
        return false;
    }
    if name.first() == Some(&b'/') || name.last() == Some(&b'/') {
        return false;
    }
    if name.last() == Some(&b'.') {
        return false;
    }
    for component in name.split(|&b| b == b'/') {
        if component.is_empty() {
            return false;
        }
        if component.first() == Some(&b'.') {
            return false;
        }
        if component.ends_with(b".lock") {
            return false;
        }
    }
    true
}

/// Standard git tree-entry modes.
fn is_valid_mode(mode: u32) -> bool {
    matches!(mode, 0o100755 | 0o100644 | 0o120000 | 0o040000 | 0o160000)
}

/// git's `parse_mode` (object.h): octal digits accumulate into an `unsigned
/// int` with silent 32-bit wrap-around, and the result is stored through a
/// `uint16_t *`, truncating the high bits. A mode like `77777777777` therefore
/// becomes `0o177777` (a `badFilemode` warning) rather than a parse failure.
fn parse_octal_mode(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut mode: u32 = 0;
    for &b in bytes {
        if !(b'0'..=b'7').contains(&b) {
            return None;
        }
        mode = mode.wrapping_mul(8).wrapping_add((b - b'0') as u32);
    }
    Some(mode as u16 as u32)
}

/// HFS/NTFS `.git` detection (the common cases t1450 exercises): `.git`,
/// `.GIT`, `.Git`, `git~1`, `.git.`, trailing dots/spaces, and the zero-width
/// joiner variant `.gI{u200c}T`. We approximate git's `is_hfs_dotgit` /
/// `is_ntfs_dotgit` by normalising the candidate.
fn is_dotgit_name(name: &[u8]) -> bool {
    // NTFS 8.3 short name for ".git".
    if name.eq_ignore_ascii_case(b"git~1") {
        return true;
    }
    // Strip a single trailing run of dots/spaces (NTFS strips these).
    let trimmed = {
        let mut end = name.len();
        while end > 0 && (name[end - 1] == b'.' || name[end - 1] == b' ') {
            end -= 1;
        }
        &name[..end]
    };
    if trimmed.eq_ignore_ascii_case(b".git") {
        return true;
    }
    // HFS ignores certain zero-width code points; drop them and re-compare.
    let folded = strip_hfs_ignorable(name);
    if folded.eq_ignore_ascii_case(b".git") {
        return true;
    }
    false
}

/// Remove the Unicode code points HFS+ ignores in its dotgit check (the ones
/// t1450 uses: U+200C zero-width non-joiner, plus the broader git set).
fn strip_hfs_ignorable(name: &[u8]) -> Vec<u8> {
    // The bytes are UTF-8; decode, drop ignorable code points, re-encode ASCII.
    let s = match std::str::from_utf8(name) {
        Ok(s) => s,
        Err(_) => return name.to_vec(),
    };
    s.chars()
        .filter(|c| !is_hfs_ignorable(*c))
        .collect::<String>()
        .into_bytes()
}

fn is_hfs_ignorable(c: char) -> bool {
    matches!(
        c as u32,
        0x200c | 0x200d | 0x200e | 0x200f | 0x202a..=0x202e | 0x206a..=0x206f
        | 0xfeff | 0x00ad | 0x034f | 0x115f | 0x1160 | 0x17b4 | 0x17b5 | 0x2060..=0x2064
    )
}

#[derive(PartialEq, Eq)]
enum Ordering2 {
    Equal,
    Ordered,
    Unordered,
}

fn is_dir_mode(mode: u32) -> bool {
    mode == 0o040000
}

/// git's `is_less_than_slash`: a byte in `(0x00, '/')`.
fn is_less_than_slash(c: u8) -> bool {
    c > 0 && c < b'/'
}

/// Faithful port of git's `verify_ordered`. Trees sort in *path* order: a
/// directory entry sorts as if its name had a trailing '/'. Detects both
/// out-of-order entries and (possibly non-consecutive) duplicates created by
/// the implicit slash, via the `candidates` stack.
fn verify_ordered(
    mode1: u32,
    name1: &[u8],
    mode2: u32,
    name2: &[u8],
    candidates: &mut Vec<Vec<u8>>,
) -> Ordering2 {
    let len = name1.len().min(name2.len());
    match name1[..len].cmp(&name2[..len]) {
        std::cmp::Ordering::Less => return Ordering2::Ordered,
        std::cmp::Ordering::Greater => return Ordering2::Unordered,
        std::cmp::Ordering::Equal => {}
    }

    // First `len` bytes equal; order the next byte, turning a name-end ('\0')
    // into '/' for a directory entry.
    let mut c1 = name1.get(len).copied().unwrap_or(0);
    let mut c2 = name2.get(len).copied().unwrap_or(0);
    if c1 == 0 && c2 == 0 {
        // Same name, one blob one tree (or identical) => duplicate.
        return Ordering2::Equal;
    }
    if c1 == 0 && is_dir_mode(mode1) {
        c1 = b'/';
    }
    if c2 == 0 && is_dir_mode(mode2) {
        c2 = b'/';
    }

    // Non-consecutive duplicate handling via the d/f candidate stack.
    if c1 == 0 && is_less_than_slash(c2) {
        candidates.push(name1.to_vec());
    } else if c2 == b'/' && is_less_than_slash(c1) {
        while let Some(f_name) = candidates.pop() {
            // skip_prefix(name2, f_name)
            let Some(rest) = name2.strip_prefix(f_name.as_slice()) else {
                continue;
            };
            if rest.is_empty() {
                return Ordering2::Equal;
            }
            if is_less_than_slash(rest[0]) {
                candidates.push(f_name);
                break;
            }
        }
    }

    if c1 < c2 {
        Ordering2::Ordered
    } else {
        Ordering2::Unordered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(findings: &[ContentFinding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.msg_id.camel()).collect()
    }

    fn cfg() -> SeverityConfig {
        SeverityConfig::new(false)
    }

    #[test]
    fn valid_commit_has_no_findings() {
        let body = b"tree 0000000000000000000000000000000000000000\n\
author A U Thor <author@example.com> 1234567890 +0000\n\
committer C O Mitter <committer@example.com> 1234567890 +0000\n\n\
message\n"
            .to_vec();
        let f = check_object_content(ObjectType::Commit, &body, &cfg());
        assert!(f.is_empty(), "{f:?}");
    }

    fn valid_tag_with_object_hex(object_hex: &str) -> Vec<u8> {
        format!(
            "object {object_hex}\ntype commit\ntag valid\n\
tagger T A Gger <tagger@example.com> 1234567890 +0000\n\nmessage\n"
        )
        .into_bytes()
    }

    #[test]
    fn tag_object_header_width_follows_explicit_object_format() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let body = valid_tag_with_object_hex(&"1".repeat(format.hex_len()));
            let findings = check_object_content_with_format(format, ObjectType::Tag, &body, &cfg());
            assert!(findings.is_empty(), "{format:?}: {findings:?}");
        }
    }

    #[test]
    fn tag_object_header_rejects_the_other_formats_width() {
        let sha1_body = valid_tag_with_object_hex(&"1".repeat(ObjectFormat::Sha1.hex_len()));
        let sha256_body = valid_tag_with_object_hex(&"2".repeat(ObjectFormat::Sha256.hex_len()));
        let sha1_as_sha256 = check_object_content_with_format(
            ObjectFormat::Sha256,
            ObjectType::Tag,
            &sha1_body,
            &cfg(),
        );
        let sha256_as_sha1 = check_object_content_with_format(
            ObjectFormat::Sha1,
            ObjectType::Tag,
            &sha256_body,
            &cfg(),
        );
        assert_eq!(ids(&sha1_as_sha256), vec!["badObjectSha1"]);
        assert_eq!(ids(&sha256_as_sha1), vec!["badObjectSha1"]);
    }

    #[test]
    fn compatibility_content_api_infers_valid_sha256_tag_header() {
        let body = valid_tag_with_object_hex(&"a".repeat(ObjectFormat::Sha256.hex_len()));
        let findings = check_object_content(ObjectType::Tag, &body, &cfg());
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn sha256_tree_entries_use_32_byte_object_ids() {
        let mut body = b"100644 file\0".to_vec();
        body.extend_from_slice(&[0x42; 32]);
        let findings =
            check_object_content_with_format(ObjectFormat::Sha256, ObjectType::Tree, &body, &cfg());
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn commit_bad_name_embedded_gt() {
        // "author@example.com" -> "author@>example.com" creates a '>' in name.
        let body = b"tree 0000000000000000000000000000000000000000\n\
author A U Thor <author@>example.com> 1234567890 +0000\n\
committer C O Mitter <committer@example.com> 1234567890 +0000\n\n\
m\n"
        .to_vec();
        let f = check_object_content(ObjectType::Commit, &body, &cfg());
        // The embedded '>' is inside the email; git reports missingSpaceBeforeDate
        // because the email closes early. Either way it must be a non-empty error.
        assert!(!f.is_empty());
        assert_eq!(f[0].severity, Severity::Error);
    }

    #[test]
    fn commit_missing_lt_is_bad_name() {
        let body = b"tree 0000000000000000000000000000000000000000\n\
author A U Thor author@example.com> 1234567890 +0000\n\
committer C O Mitter <committer@example.com> 1234567890 +0000\n\n\
m\n"
        .to_vec();
        let f = check_object_content(ObjectType::Commit, &body, &cfg());
        assert_eq!(ids(&f), vec!["badName"]);
    }

    #[test]
    fn commit_nul_in_header() {
        let mut body = b"tree 0000000000000000000000000000000000000000\n\
author"
            .to_vec();
        body.push(0);
        body.extend_from_slice(b" A <a@b.com> 1 +0000\n");
        let f = check_object_content(ObjectType::Commit, &body, &cfg());
        assert_eq!(ids(&f), vec!["nulInHeader"]);
        assert!(
            f[0].detail
                .starts_with("unterminated header: NUL at offset")
        );
    }

    #[test]
    fn commit_timestamp_overflow() {
        let body = b"tree 0000000000000000000000000000000000000000\n\
author A U Thor <a@b.com> 18446744073709551617 +0000\n\
committer C O Mitter <c@d.com> 1 +0000\n\nm\n"
            .to_vec();
        let f = check_object_content(ObjectType::Commit, &body, &cfg());
        assert_eq!(ids(&f), vec!["badDateOverflow"]);
    }

    #[test]
    fn tree_bad_filemode_is_info_warn_by_default() {
        // mode 100000 (bogus), one entry.
        let mut body = b"100000 foo\0".to_vec();
        body.extend_from_slice(&[0u8; 20]);
        let f = check_object_content(ObjectType::Tree, &body, &cfg());
        // null sha1 + bad filemode both fire; check badFilemode present as warn.
        let bad = f
            .iter()
            .find(|x| x.msg_id == MsgId::BadFilemode)
            .expect("badFilemode finding");
        assert_eq!(bad.severity, Severity::Warn);
    }

    #[test]
    fn fsck_config_promotes_badfilemode_to_error() {
        let mut config = SeverityConfig::new(false);
        config.set("badFilemode", "error");
        let mut body = b"100000 foo\0".to_vec();
        body.extend_from_slice(&[0x11u8; 20]);
        let f = check_object_content(ObjectType::Tree, &body, &config);
        let bad = f
            .iter()
            .find(|x| x.msg_id == MsgId::BadFilemode)
            .expect("badFilemode finding");
        assert_eq!(bad.severity, Severity::Error);
    }

    #[test]
    fn tree_dotgit_variants() {
        for name in [
            &b".git"[..],
            &b".GIT"[..],
            &b".Git"[..],
            &b"git~1"[..],
            &b".git."[..],
        ] {
            let mut body = b"100644 ".to_vec();
            body.extend_from_slice(name);
            body.push(0);
            body.extend_from_slice(&[0x22u8; 20]);
            let f = check_object_content(ObjectType::Tree, &body, &cfg());
            assert!(
                f.iter().any(|x| x.msg_id == MsgId::HasDotgit),
                "expected hasDotgit for {:?}: {f:?}",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn tree_zwnj_dotgit() {
        // ".gI<U+200C>T" -> HFS-ignorable joiner folds to ".gIT".
        let mut body = b"100644 ".to_vec();
        body.extend_from_slice(".gI\u{200c}T".as_bytes());
        body.push(0);
        body.extend_from_slice(&[0x33u8; 20]);
        let f = check_object_content(ObjectType::Tree, &body, &cfg());
        assert!(f.iter().any(|x| x.msg_id == MsgId::HasDotgit), "{f:?}");
    }

    #[test]
    fn tag_bad_name_and_missing_tagger() {
        let body = b"object 0000000000000000000000000000000000000000\n\
type commit\n\
tag wrong name format\n\n\
This is an invalid tag.\n"
            .to_vec();
        let f = check_object_content(ObjectType::Tag, &body, &cfg());
        assert_eq!(ids(&f), vec!["badTagName", "missingTaggerEntry"]);
        assert_eq!(f[0].detail, "invalid 'tag' name: wrong name format");
        assert_eq!(f[1].detail, "invalid format - expected 'tagger' line");
    }

    #[test]
    fn strict_does_not_promote_default_info_tag_findings() {
        let body = b"object 0000000000000000000000000000000000000000\n\
type commit\n\
tag wrong name format\n\n\
This is an invalid tag.\n"
            .to_vec();
        let f = check_object_content(ObjectType::Tag, &body, &SeverityConfig::new(true));
        let severities = f
            .iter()
            .map(|finding| (finding.msg_id.camel(), finding.severity))
            .collect::<Vec<_>>();
        assert_eq!(
            severities,
            vec![
                ("badTagName", Severity::Warn),
                ("missingTaggerEntry", Severity::Warn),
            ]
        );
    }

    #[test]
    fn tag_ident_error_followed_by_ignored_extra_header_is_not_fatal() {
        let body = b"object 0000000000000000000000000000000000000000\n\
type commit\n\
tag valid\n\
tagger T A Gger <\n\
 > 0 +0000\n\n"
            .to_vec();
        let f = check_object_content(ObjectType::Tag, &body, &cfg());
        assert_eq!(ids(&f), vec!["badEmail"]);
        assert!(!f[0].fatal, "{f:?}");
    }

    #[test]
    fn tag_extra_header_ignored_by_default() {
        // extraHeaderEntry is IGNORE by default; no finding unless promoted.
        let body = b"object 0000000000000000000000000000000000000000\n\
type commit\n\
tag valid\n\
tagger T A Gger <tagger@example.com> 1234567890 -0000\n\
bogus header\n\n\
msg\n"
            .to_vec();
        let f = check_object_content(ObjectType::Tag, &body, &cfg());
        assert!(f.is_empty(), "{f:?}");
        let mut config = SeverityConfig::new(false);
        config.set("extraHeaderEntry", "error");
        let f = check_object_content(ObjectType::Tag, &body, &config);
        assert_eq!(ids(&f), vec!["extraHeaderEntry"]);
    }

    #[test]
    fn tree_duplicate_entries() {
        // two identical entries
        let mut body = Vec::new();
        for _ in 0..2 {
            body.extend_from_slice(b"100644 x\0");
            body.extend_from_slice(&[0x44u8; 20]);
        }
        let f = check_object_content(ObjectType::Tree, &body, &cfg());
        assert!(
            f.iter().any(|x| x.msg_id == MsgId::DuplicateEntries),
            "{f:?}"
        );
    }

    #[test]
    fn strict_promotes_warning() {
        let config = SeverityConfig::new(true);
        let mut body = b"100644 .git\0".to_vec();
        body.extend_from_slice(&[0x55u8; 20]);
        let f = check_object_content(ObjectType::Tree, &body, &config);
        let dotgit = f
            .iter()
            .find(|x| x.msg_id == MsgId::HasDotgit)
            .expect("hasDotgit finding");
        assert_eq!(dotgit.severity, Severity::Error);
    }

    // --- ports of the former CLI-owned hash-object fsck unit tests -----------
    // (crates/sley-cli/src/commands/hash_object_fsck.rs, deleted when the
    // engine moved here), re-expectations verified against `git
    // hash-object -t <type> --stdin` (git 2.55).

    fn tree_entry(mode: &[u8], name: &[u8], fill: u8) -> Vec<u8> {
        let mut body = mode.to_vec();
        body.push(b' ');
        body.extend_from_slice(name);
        body.push(0);
        body.extend(std::iter::repeat_n(fill, 20));
        body
    }

    #[test]
    fn empty_tree_is_valid() {
        assert!(check_tree(ObjectFormat::Sha1, b"", DEFAULT_LARGE_PATHNAME_LEN).is_empty());
    }

    #[test]
    fn garbage_tree_fails_with_too_short_raw_line() {
        let f = check_tree(ObjectFormat::Sha1, b"garbage", DEFAULT_LARGE_PATHNAME_LEN);
        assert_eq!(ids(&f), vec!["badTree"]);
        assert_eq!(f[0].raw_stderr.as_deref(), Some("too-short tree object"));
        assert!(f[0].fatal);
    }

    #[test]
    fn decode_error_wording_matches_git_tree_walk() {
        assert_eq!(
            tree_decode_error(b"abc\n", ObjectFormat::Sha1.raw_len()),
            "too-short tree object"
        );
        // Leading space => parse_mode returns NULL.
        let mut buf = b" 100644 name\0".to_vec();
        buf.extend(std::iter::repeat_n(0u8, 20));
        assert_eq!(
            tree_decode_error(&buf, ObjectFormat::Sha1.raw_len()),
            "malformed mode in tree entry"
        );
        // Empty filename (`!*path` after the space).
        let mut buf = b"100644 \0".to_vec();
        buf.extend(std::iter::repeat_n(0u8, 20));
        assert_eq!(
            tree_decode_error(&buf, ObjectFormat::Sha1.raw_len()),
            "empty filename in tree entry"
        );
    }

    #[test]
    fn decode_accepts_valid_entry() {
        let buf = tree_entry(b"100644", b"file", 0x11);
        let entry = scan_tree_entry(&buf, 0, ObjectFormat::Sha1.raw_len()).expect("valid entry");
        assert_eq!(entry.mode, 0o100644);
        assert_eq!(entry.name, b"file");
        assert_eq!(entry.next, buf.len());
    }

    #[test]
    fn standard_modes_under_strict() {
        for &mode in &[0o100644u32, 0o100755, 0o120000, 0o040000, 0o160000] {
            let mut body = format!("{mode:o} file\0").into_bytes();
            body.extend(std::iter::repeat_n(0x11u8, 20));
            let f = check_object_content(ObjectType::Tree, &body, &cfg());
            assert!(
                !f.iter().any(|x| x.msg_id == MsgId::BadFilemode),
                "{mode:o} should be standard: {f:?}"
            );
        }
        // 0o100664 only passes git's fsck in non-strict mode; hash-object is strict.
        for &mode in &[0o100664u32, 0o100600] {
            let mut body = format!("{mode:o} file\0").into_bytes();
            body.extend(std::iter::repeat_n(0x22u8, 20));
            let f = check_object_content(ObjectType::Tree, &body, &cfg());
            assert!(
                f.iter().any(|x| x.msg_id == MsgId::BadFilemode),
                "{mode:o} should be a bad filemode: {f:?}"
            );
        }
    }

    #[test]
    fn consecutive_identical_names_are_dups() {
        let mut body = tree_entry(b"100644", b"file", 0x11);
        body.extend(tree_entry(b"100644", b"file", 0x22));
        let f = check_object_content(ObjectType::Tree, &body, &cfg());
        assert!(f.iter().any(|x| x.msg_id == MsgId::DuplicateEntries));
    }

    #[test]
    fn descending_names_are_unordered() {
        let mut body = tree_entry(b"100644", b"b", 0x11);
        body.extend(tree_entry(b"100644", b"a", 0x22));
        let f = check_object_content(ObjectType::Tree, &body, &cfg());
        assert!(f.iter().any(|x| x.msg_id == MsgId::TreeNotSorted));
    }

    #[test]
    fn ascending_names_are_ok() {
        let mut body = tree_entry(b"100644", b"a", 0x11);
        body.extend(tree_entry(b"100644", b"b", 0x22));
        assert!(check_object_content(ObjectType::Tree, &body, &cfg()).is_empty());
    }

    #[test]
    fn tag_refname_rules_match_check_refname_format() {
        // Verified against `git hash-object -t tag`: mid-path trailing dots and
        // a lone "@" are legal ("refs/tags/<name>" never trips the whole-refname
        // "@" or trailing-dot rules); the rest are rejected.
        for name in [b"v1.0".as_slice(), b"release/1.0", b"@", b"v1./x"] {
            assert!(valid_tag_name(name), "{name:?} should be valid");
        }
        for name in [
            b"".as_slice(),
            &b".hidden"[..],
            b"ends.",
            b"a..b",
            b"has space",
            b"caret^",
            b"name.lock",
            b"a/",
            b"a@{b",
        ] {
            assert!(!valid_tag_name(name), "{name:?} should be invalid");
        }
    }

    #[test]
    fn date_overflow_boundaries_match_date_overflows() {
        // TIME_MAX is UINTMAX_MAX; values above INT64_MAX fail date.c's time_t
        // round-trip. Confirmed against oracle hash-object exits.
        assert!(!timestamp_overflows(b"0"));
        assert!(!timestamp_overflows(b"1234567890"));
        assert!(!timestamp_overflows(b"9223372036854775807")); // i64::MAX
        assert!(timestamp_overflows(b"9223372036854775808")); // i64::MAX + 1
        assert!(timestamp_overflows(b"18446744073709551615")); // u64::MAX
        assert!(timestamp_overflows(b"99999999999999999999999")); // clamps to TIME_MAX
        assert!(timestamp_overflows(&[b'9'; 24])); // buffer-cap overflow
    }

    #[test]
    fn empty_commit_is_unterminated_header() {
        let report = hash_format_check(ObjectFormat::Sha1, ObjectType::Commit, b"");
        assert!(report.refuse);
        assert_eq!(ids(&report.diagnostics), vec!["unterminatedHeader"]);
    }

    #[test]
    fn empty_tag_is_unterminated_header() {
        let report = hash_format_check(ObjectFormat::Sha1, ObjectType::Tag, b"");
        assert!(report.refuse);
        assert_eq!(ids(&report.diagnostics), vec!["unterminatedHeader"]);
    }

    #[test]
    fn extra_header_after_tagger_masks_ident_error_but_still_prints_it() {
        // A bad tagger timezone reports an error, but the following extra
        // header (FSCK_IGNORE) overwrites `ret` to zero, so git prints the
        // diagnostic yet still hashes the object.
        let body = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag t\ntagger a <a> 0 bad\nextra h\n\nm\n";
        let report = hash_format_check(ObjectFormat::Sha1, ObjectType::Tag, body);
        assert!(!report.refuse, "{report:?}");
        assert_eq!(ids(&report.diagnostics), vec!["badTimezone"]);
    }

    #[test]
    fn hash_lethal_tag_finding_truncates_later_diagnostics() {
        // git's badTagName report short-circuits via `goto done`: the later
        // ident error is never reached and must not print.
        let body = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag bad~name\ntagger a <a> 0 bad\n\nm\n";
        let report = hash_format_check(ObjectFormat::Sha1, ObjectType::Tag, body);
        assert!(report.refuse);
        assert_eq!(ids(&report.diagnostics), vec!["badTagName"]);
    }

    #[test]
    fn missing_tagger_entry_is_hash_lethal() {
        let body = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag t\n\nm\n";
        let report = hash_format_check(ObjectFormat::Sha1, ObjectType::Tag, body);
        assert!(report.refuse);
        assert_eq!(ids(&report.diagnostics), vec!["missingTaggerEntry"]);
    }

    #[test]
    fn valid_tag_passes() {
        let body = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag t\ntagger a <a@b> 0 +0000\n\nmsg\n";
        let report = hash_format_check(ObjectFormat::Sha1, ObjectType::Tag, body);
        assert!(!report.refuse);
        assert!(report.diagnostics.is_empty(), "{report:?}");
    }

    #[test]
    fn timezone_must_be_exactly_four_digits() {
        let three = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag t\ntagger a <a> 0 +000\n";
        let four = b"object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            type commit\ntag t\ntagger a <a> 0 +0000\n";
        let report = hash_format_check(ObjectFormat::Sha1, ObjectType::Tag, three);
        assert!(report.refuse);
        let report = hash_format_check(ObjectFormat::Sha1, ObjectType::Tag, four);
        assert!(!report.refuse);
    }

    #[test]
    fn blob_is_never_validated() {
        let report = hash_format_check(ObjectFormat::Sha1, ObjectType::Blob, b"\0\0garbage");
        assert!(!report.refuse);
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn giant_octal_mode_wraps_like_parse_mode() {
        // git accumulates into an unsigned int with silent wrap-around and
        // stores through uint16_t, so 77777777777 becomes 0o177777 — a
        // badFilemode warning, not a parse failure (oracle: exit 128 via the
        // promoted warning alone, with no badTree).
        let mut body = b"77777777777 f\0".to_vec();
        body.extend(std::iter::repeat_n(0x22u8, 20));
        let f = check_object_content(ObjectType::Tree, &body, &cfg());
        assert_eq!(ids(&f), vec!["badFilemode"]);
    }

    #[test]
    fn undecodable_successor_reports_bad_tree_before_property_findings() {
        // A zero-padded first entry followed by an undecodable tail: git prints
        // the tree-walk error, then badTree, then the accumulated property
        // findings (zeroPaddedFilemode last).
        let mut body = b"0100644 f\0".to_vec();
        body.extend(std::iter::repeat_n(0x11u8, 20));
        body.extend(std::iter::repeat_n(0u8, 22));
        body.push(b'x');
        let f = check_object_content(ObjectType::Tree, &body, &cfg());
        assert_eq!(ids(&f), vec!["badTree", "zeroPaddedFilemode"], "{f:?}");
        assert_eq!(
            f[0].raw_stderr.as_deref(),
            Some("malformed mode in tree entry")
        );
    }
}
