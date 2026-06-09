use std::borrow::Borrow;
use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub const UPSTREAM_GIT_COMPAT_VERSION: &str = "2.54.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    pub const fn raw_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    pub const fn hex_len(self) -> usize {
        self.raw_len() * 2
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
}

impl FromStr for ObjectFormat {
    type Err = GitError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "sha1" => Ok(Self::Sha1),
            "sha256" => Ok(Self::Sha256),
            other => Err(GitError::Unsupported(format!("object format {other}"))),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId {
    format: ObjectFormat,
    bytes: [u8; 32],
}

impl ObjectId {
    pub fn from_raw(format: ObjectFormat, raw: &[u8]) -> Result<Self> {
        if raw.len() != format.raw_len() {
            return Err(GitError::InvalidObjectId(format!(
                "expected {} bytes for {}, got {}",
                format.raw_len(),
                format.name(),
                raw.len()
            )));
        }
        let mut bytes = [0; 32];
        bytes[..raw.len()].copy_from_slice(raw);
        Ok(Self { format, bytes })
    }

    pub fn from_hex(format: ObjectFormat, hex: &str) -> Result<Self> {
        if hex.len() != format.hex_len() {
            return Err(GitError::InvalidObjectId(format!(
                "expected {} hex digits for {}, got {}",
                format.hex_len(),
                format.name(),
                hex.len()
            )));
        }
        let mut raw = [0; 32];
        for (i, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
            raw[i] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
        }
        Ok(Self { format, bytes: raw })
    }

    pub const fn format(&self) -> ObjectFormat {
        self.format
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.format.raw_len()]
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(self.format.hex_len());
        self.write_hex(&mut out)
            .expect("writing object id hex to a String cannot fail");
        out
    }

    pub fn write_hex(&self, out: &mut impl fmt::Write) -> fmt::Result {
        write_hex_bytes(self.as_bytes(), out)
    }

    pub fn hex_prefix_matches(&self, prefix: &[u8]) -> bool {
        if prefix.len() > self.format.hex_len() {
            return false;
        }

        prefix.iter().enumerate().all(|(index, expected)| {
            let Some(expected) = hex_nibble_value(*expected) else {
                return false;
            };
            let byte = self.as_bytes()[index / 2];
            let actual = if index % 2 == 0 {
                byte >> 4
            } else {
                byte & 0x0f
            };
            actual == expected
        })
    }

    pub const fn abbrev_hex_len(&self, width: usize) -> usize {
        let hex_len = self.format.hex_len();
        if width < hex_len { width } else { hex_len }
    }

    /// The all-zero ("null") object id for `format`.
    pub fn null(format: ObjectFormat) -> Self {
        Self {
            format,
            bytes: [0; 32],
        }
    }

    /// True when every byte is zero (the null oid).
    pub fn is_null(&self) -> bool {
        self.as_bytes().iter().all(|byte| *byte == 0)
    }

    /// The id of the canonical empty tree for `format` (`4b825dc6…` for SHA-1).
    pub fn empty_tree(format: ObjectFormat) -> Self {
        Self::digest_object(format, "tree", b"")
    }

    /// The id of the canonical empty blob for `format` (`e69de29b…` for SHA-1).
    pub fn empty_blob(format: ObjectFormat) -> Self {
        Self::digest_object(format, "blob", b"")
    }

    /// Hash `"<type> <len>\0<body>"` straight into an id, bypassing the
    /// fallible length check in [`ObjectId::from_raw`] (our own digests are
    /// always the right length) so the well-known constants stay infallible.
    fn digest_object(format: ObjectFormat, object_type: &str, body: &[u8]) -> Self {
        let mut framed = Vec::with_capacity(object_type.len() + body.len() + 32);
        framed.extend_from_slice(object_type.as_bytes());
        framed.push(b' ');
        framed.extend_from_slice(body.len().to_string().as_bytes());
        framed.push(0);
        framed.extend_from_slice(body);
        let mut bytes = [0u8; 32];
        match format {
            ObjectFormat::Sha1 => bytes[..20].copy_from_slice(&sha1(&framed)),
            ObjectFormat::Sha256 => bytes[..32].copy_from_slice(&sha256(&framed)),
        }
        Self { format, bytes }
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ObjectId").field(&self.to_hex()).finish()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_hex(f)
    }
}

impl FromStr for ObjectId {
    type Err = GitError;

    /// Parse a full hex id, inferring the hash from its length (40 hex digits =
    /// SHA-1, 64 = SHA-256).
    fn from_str(text: &str) -> Result<Self> {
        let format = match text.len() {
            40 => ObjectFormat::Sha1,
            64 => ObjectFormat::Sha256,
            other => {
                return Err(GitError::InvalidObjectId(format!(
                    "expected 40 or 64 hex digits, got {other}"
                )));
            }
        };
        Self::from_hex(format, text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteString(Vec<u8>);

impl ByteString {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<&str> for ByteString {
    fn from(value: &str) -> Self {
        Self(value.as_bytes().to_vec())
    }
}

/// A validated git ref name (e.g. `refs/heads/main`, `HEAD`).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FullName(String);

impl FullName {
    /// Construct a ref name, rejecting empty names, ASCII control characters,
    /// leading/trailing whitespace, and consecutive slashes.
    pub fn new(name: impl AsRef<str>) -> Result<Self> {
        let name = name.as_ref();
        validate_full_name(name)?;
        Ok(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for FullName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FullName").field(&self.0).finish()
    }
}

impl fmt::Display for FullName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<FullName> for String {
    fn from(value: FullName) -> Self {
        value.0
    }
}

impl Borrow<str> for FullName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for FullName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for FullName {
    type Error = GitError;

    fn try_from(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for FullName {
    type Error = GitError;

    fn try_from(value: String) -> Result<Self> {
        validate_full_name(&value)?;
        Ok(Self(value))
    }
}

impl PartialEq<&str> for FullName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<FullName> for &str {
    fn eq(&self, other: &FullName) -> bool {
        *self == other.0
    }
}

fn validate_full_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(GitError::InvalidFormat("ref name must not be empty".into()));
    }
    if name.chars().next().is_some_and(|ch| ch.is_whitespace())
        || name.chars().last().is_some_and(|ch| ch.is_whitespace())
    {
        return Err(GitError::InvalidFormat(
            "ref name must not have leading or trailing whitespace".into(),
        ));
    }
    if name.contains("//") {
        return Err(GitError::InvalidFormat(
            "ref name must not contain consecutive slashes".into(),
        ));
    }
    if name.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(GitError::InvalidFormat(
            "ref name must not contain control characters".into(),
        ));
    }
    Ok(())
}

/// A byte string for git paths and similar on-disk identifiers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BString(Vec<u8>);

impl BString {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl From<&str> for BString {
    fn from(v: &str) -> Self {
        Self::from_bytes(v.as_bytes())
    }
}
impl From<&[u8]> for BString {
    fn from(v: &[u8]) -> Self {
        Self::from_bytes(v)
    }
}
impl<const N: usize> From<&[u8; N]> for BString {
    fn from(v: &[u8; N]) -> Self {
        Self::from_bytes(v.as_slice())
    }
}
impl From<Vec<u8>> for BString {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}
impl PartialEq<&[u8]> for BString {
    fn eq(&self, o: &&[u8]) -> bool {
        self.0.as_slice() == *o
    }
}
impl<const N: usize> PartialEq<&[u8; N]> for BString {
    fn eq(&self, o: &&[u8; N]) -> bool {
        self.as_bytes() == o.as_slice()
    }
}
impl PartialEq<BString> for &[u8] {
    fn eq(&self, o: &BString) -> bool {
        *self == o.as_bytes()
    }
}
impl<const N: usize> PartialEq<BString> for &[u8; N] {
    fn eq(&self, o: &BString) -> bool {
        self.as_slice() == o.as_bytes()
    }
}
impl PartialEq<Vec<u8>> for BString {
    fn eq(&self, o: &Vec<u8>) -> bool {
        self.0 == *o
    }
}
impl PartialEq<BString> for Vec<u8> {
    fn eq(&self, o: &BString) -> bool {
        *self == o.0
    }
}

impl fmt::Display for BString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.0))
    }
}

impl Borrow<[u8]> for BString {
    fn borrow(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for BString {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for BString {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoPath(PathBuf);

impl RepoPath {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.is_absolute() {
            return Err(GitError::InvalidPath(
                "repository paths must be relative".into(),
            ));
        }
        if path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        }) {
            return Err(GitError::InvalidPath(
                "repository paths must not escape".into(),
            ));
        }
        Ok(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// A typed *parse-view* of a git identity line (`Name <email> <secs> <tz>`) as
/// found on a commit's `author`/`committer` or a tag's `tagger` header.
///
/// This is a read-only lens over bytes that are stored and re-serialized
/// verbatim elsewhere (see [`Signature::raw`]). It exists so callers can read
/// the typed `name`/`email`/`time` of an identity without re-implementing git's
/// ident-splitting rules, *not* as a storage format: the object model keeps the
/// original raw bytes as its source of truth, and round-tripping through this
/// view is byte-exact precisely because the raw line is retained alongside the
/// parsed fields (see [`Signature::to_ident_bytes`]).
///
/// Parse one with [`Signature::from_ident_line`]. The `time`'s timezone
/// preserves git's distinction between `+0000` (UTC) and `-0000` (a sentinel git
/// writes to mean "timezone unknown"); see [`GitTime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// The identity's name: the bytes before the ` <` that opens the email,
    /// with one trailing space (the separator) removed. May be empty.
    pub name: ByteString,
    /// The identity's email: the bytes between the `<` and `>` delimiters. May
    /// be empty.
    pub email: ByteString,
    /// The commit/authorship time and its timezone offset.
    pub time: GitTime,
    /// The exact original ident-line bytes this view was parsed from, retained
    /// so [`Signature::to_ident_bytes`] can reproduce the input byte-for-byte
    /// regardless of any non-canonical whitespace or formatting it contained.
    pub raw: Vec<u8>,
}

impl Signature {
    /// Parse a raw git identity line (`Name <email> <unix-secs> <tz>`) into a
    /// typed view, returning `None` when the bytes do not form a well-formed
    /// identity.
    ///
    /// The splitting mirrors git's own `split_ident_line`: the email is the run
    /// of bytes between the last `<` and the first following `>`; the name is
    /// everything before that `<` (one separating space is dropped); after the
    /// `>` come a space, the decimal Unix timestamp, a space, and the timezone
    /// token. The name and email may legitimately be empty, but a missing
    /// `<`/`>` pair, a non-numeric timestamp, or a malformed timezone token all
    /// yield `None` rather than a lossy guess — this is a *best-effort* parse
    /// that never panics. The original bytes are retained in
    /// [`Signature::raw`] so the parsed view re-serializes byte-identically.
    pub fn from_ident_line(line: &[u8]) -> Option<Self> {
        // Email is delimited by the last '<' whose matching '>' follows it, the
        // way git scans an ident from the right. Find the last '>' first, then
        // the last '<' before it.
        let mail_end = line.iter().rposition(|byte| *byte == b'>')?;
        let mail_begin = line[..mail_end].iter().rposition(|byte| *byte == b'<')? + 1;
        let email = &line[mail_begin..mail_end];

        // The name is everything before the '<', with a single trailing space
        // (the separator git inserts) trimmed if present.
        let mut name_end = mail_begin.saturating_sub(1);
        if name_end > 0 && line[name_end - 1] == b' ' {
            name_end -= 1;
        }
        let name = &line[..name_end];

        // After '>' git expects "<space><secs><space><tz>". Trim the single
        // separating space, then split the timestamp from the timezone token.
        let rest = line.get(mail_end + 1..)?;
        let rest = rest.strip_prefix(b" ")?;
        let time = GitTime::from_time_fields(rest)?;

        Some(Self {
            name: ByteString::new(name.to_vec()),
            email: ByteString::new(email.to_vec()),
            time,
            raw: line.to_vec(),
        })
    }

    /// Reproduce the original identity-line bytes.
    ///
    /// This returns [`Signature::raw`] verbatim, so for any line that
    /// [`Signature::from_ident_line`] accepted, `from_ident_line(line)?
    /// .to_ident_bytes() == line` holds byte-for-byte — including the `-0000`
    /// timezone and any non-canonical spacing the source contained.
    pub fn to_ident_bytes(&self) -> Vec<u8> {
        self.raw.clone()
    }

    /// Re-derive the canonical ident line from the parsed fields alone
    /// (`name <email> secs tz`), ignoring [`Signature::raw`].
    ///
    /// For an identity in git's canonical form this equals
    /// [`Signature::to_ident_bytes`]; it differs only when the source line
    /// carried non-canonical whitespace. Callers wanting byte-exact
    /// reproduction should use [`Signature::to_ident_bytes`]; this is provided
    /// for constructing a normalized line from typed parts.
    pub fn to_canonical_ident_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.raw.len());
        out.extend_from_slice(self.name.as_bytes());
        out.extend_from_slice(b" <");
        out.extend_from_slice(self.email.as_bytes());
        out.extend_from_slice(b"> ");
        out.extend_from_slice(self.time.to_ident_suffix().as_bytes());
        out
    }
}

impl fmt::Display for Signature {
    /// Renders the original ident line (lossy only for bytes that are not valid
    /// UTF-8, which are replaced with `U+FFFD`). Use
    /// [`Signature::to_ident_bytes`] for the exact bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.raw))
    }
}

/// A git timestamp: a Unix time plus the committer's timezone offset.
///
/// The offset is stored as signed minutes east of UTC ([`timezone_offset_minutes`])
/// *and* a separate [`negative_utc`] flag. The flag exists because git
/// distinguishes the timezone token `-0000` from `+0000`: both are zero minutes
/// from UTC, but git writes `-0000` as a sentinel meaning "timezone unknown"
/// (e.g. for dates parsed without zone information), and that distinction is
/// part of a commit's byte-exact identity. `timezone_offset_minutes` alone
/// cannot represent it, so `negative_utc` carries the sign of a zero offset.
///
/// [`timezone_offset_minutes`]: GitTime::timezone_offset_minutes
/// [`negative_utc`]: GitTime::negative_utc
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitTime {
    /// Seconds since the Unix epoch.
    pub seconds: i64,
    /// Timezone offset east of UTC, in minutes (e.g. `+0530` -> `330`,
    /// `-0500` -> `-300`). Zero for both `+0000` and `-0000`; consult
    /// [`GitTime::negative_utc`] to tell those apart.
    pub timezone_offset_minutes: i16,
    /// `true` only when the timezone token had a negative sign with a zero
    /// magnitude (`-0000`), git's "timezone unknown" sentinel. Always `false`
    /// for any non-zero offset.
    pub negative_utc: bool,
}

impl GitTime {
    /// A `GitTime` with the given seconds and minute offset, treating a zero
    /// offset as the ordinary `+0000` (not the `-0000` sentinel). Use
    /// [`GitTime::with_negative_utc`] to construct the `-0000` case.
    pub const fn new(seconds: i64, timezone_offset_minutes: i16) -> Self {
        Self {
            seconds,
            timezone_offset_minutes,
            negative_utc: false,
        }
    }

    /// A `GitTime` whose timezone is the `-0000` sentinel ("timezone unknown").
    /// The minute offset is zero; `negative_utc` is `true`.
    pub const fn with_negative_utc(seconds: i64) -> Self {
        Self {
            seconds,
            timezone_offset_minutes: 0,
            negative_utc: true,
        }
    }

    /// Parse the `<secs> <tz>` tail of an ident line (the bytes after the
    /// `"> "` separating the email from the time), returning `None` if either
    /// field is malformed.
    fn from_time_fields(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let (seconds_text, tz_text) = text.split_once(' ')?;
        let seconds = seconds_text.parse::<i64>().ok()?;
        let (timezone_offset_minutes, negative_utc) = parse_timezone_token(tz_text)?;
        Some(Self {
            seconds,
            timezone_offset_minutes,
            negative_utc,
        })
    }

    /// The canonical `<secs> <±HHMM>` rendering of this time, as git writes it.
    /// Preserves the `-0000` sentinel.
    fn to_ident_suffix(self) -> String {
        format!("{} {}", self.seconds, self.offset_token())
    }

    /// The canonical 5-character timezone token for this offset (sign plus four
    /// digits), e.g. `+0000`, `-0500`, `+0530`. Returns `-0000` when
    /// [`GitTime::negative_utc`] is set.
    pub fn offset_token(self) -> String {
        let sign = if self.negative_utc || self.timezone_offset_minutes < 0 {
            '-'
        } else {
            '+'
        };
        let magnitude = self.timezone_offset_minutes.unsigned_abs();
        format!("{sign}{:02}{:02}", magnitude / 60, magnitude % 60)
    }
}

/// Parse a git timezone token (`±HHMM`) into `(minutes east of UTC, negative_utc)`.
///
/// Git accepts a leading `+`/`-` followed by four digits where the last two are
/// minutes. A negative sign with a zero magnitude (`-0000`) sets `negative_utc`.
/// Returns `None` for anything that is not a well-formed token.
fn parse_timezone_token(token: &str) -> Option<(i16, bool)> {
    let bytes = token.as_bytes();
    if bytes.len() != 5 {
        return None;
    }
    let negative = match bytes[0] {
        b'+' => false,
        b'-' => true,
        _ => return None,
    };
    if !bytes[1..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let hours = i16::from(bytes[1] - b'0') * 10 + i16::from(bytes[2] - b'0');
    let minutes = i16::from(bytes[3] - b'0') * 10 + i16::from(bytes[4] - b'0');
    let total = hours * 60 + minutes;
    let negative_utc = negative && total == 0;
    let signed = if negative { -total } else { total };
    Some((signed, negative_utc))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotFoundKind {
    Message(String),
    Remote { name: String },
    Object { oid: ObjectId },
    Reference { name: String },
    Repository { path: String },
}

impl fmt::Display for NotFoundKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(msg) => write!(f, "{msg}"),
            Self::Remote { name } => write!(f, "remote {name}"),
            Self::Object { oid } => write!(f, "object {oid}"),
            Self::Reference { name } => write!(f, "{name}"),
            Self::Repository { path } => write!(f, "{path}"),
        }
    }
}

/// Git-compatible CLI exit status. See `git help exit-code` for the upstream taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliExit {
    /// Success (exit 0).
    Ok,
    /// User-facing fatal error (exit 128).
    UserError,
    /// Invalid usage / bad arguments (exit 129).
    Usage,
    /// Command-specific exit code (e.g. grep returning 1 when no matches).
    Custom(i32),
}

impl CliExit {
    pub const fn code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::UserError => 128,
            Self::Usage => 129,
            Self::Custom(code) => code,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    Io(String),
    InvalidObjectId(String),
    InvalidObject(String),
    InvalidFormat(String),
    InvalidPath(String),
    Unsupported(String),
    NotFound(NotFoundKind),
    Transaction(String),
    Command(String),
    /// Typed CLI exit with a user-facing message printed by the binary entrypoint.
    Cli(CliExit, String),
    /// Legacy explicit exit code; the message (if any) was already printed by the command.
    Exit(i32),
}

pub type Result<T> = std::result::Result<T, GitError>;

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "io error: {msg}"),
            Self::InvalidObjectId(msg) => write!(f, "invalid object id: {msg}"),
            Self::InvalidObject(msg) => write!(f, "invalid object: {msg}"),
            Self::InvalidFormat(msg) => write!(f, "invalid format: {msg}"),
            Self::InvalidPath(msg) => write!(f, "invalid path: {msg}"),
            Self::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            Self::NotFound(kind) => write!(f, "not found: {kind}"),
            Self::Transaction(msg) => write!(f, "transaction failed: {msg}"),
            Self::Command(msg) => write!(f, "command failed: {msg}"),
            Self::Cli(_, msg) => f.write_str(msg),
            Self::Exit(code) => write!(f, "exit {code}"),
        }
    }
}

impl Error for GitError {}

impl GitError {
    pub fn usage(msg: impl Into<String>) -> Self {
        Self::Cli(CliExit::Usage, msg.into())
    }

    pub fn user_error(msg: impl Into<String>) -> Self {
        Self::Cli(CliExit::UserError, msg.into())
    }

    pub fn cli_exit(kind: CliExit, msg: impl Into<String>) -> Self {
        Self::Cli(kind, msg.into())
    }

    pub fn cli_exit_code(&self) -> i32 {
        cli_exit_code(self)
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(NotFoundKind::Message(msg.into()))
    }

    pub fn remote_not_found(name: impl Into<String>) -> Self {
        Self::NotFound(NotFoundKind::Remote { name: name.into() })
    }

    pub fn object_not_found(oid: ObjectId) -> Self {
        Self::NotFound(NotFoundKind::Object { oid })
    }

    pub fn reference_not_found(name: impl Into<String>) -> Self {
        Self::NotFound(NotFoundKind::Reference { name: name.into() })
    }

    pub fn repository_not_found(path: impl Into<String>) -> Self {
        Self::NotFound(NotFoundKind::Repository { path: path.into() })
    }

    pub fn not_found_kind(&self) -> Option<&NotFoundKind> {
        match self {
            Self::NotFound(kind) => Some(kind),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GitError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

/// Map a [`GitError`] to the process exit code the CLI should use.
pub fn cli_exit_code(err: &GitError) -> i32 {
    match err {
        GitError::Exit(code) => *code,
        GitError::Cli(kind, _) => kind.code(),
        // During migration, usage-style validation still returns `Command`; treat as
        // general failure until those call sites adopt `GitError::usage`.
        GitError::Command(_) => 1,
        _ => 1,
    }
}

pub fn object_id_for_bytes(
    format: ObjectFormat,
    object_type: &str,
    body: &[u8],
) -> Result<ObjectId> {
    match format {
        // Hash the `"<type> <len>\0"` header and the body as separate updates so
        // the (potentially large) body is never copied into a combined buffer just
        // to feed the digest.
        ObjectFormat::Sha1 => ObjectId::from_raw(format, &sha1_object_digest(object_type, body)),
        ObjectFormat::Sha256 => {
            let mut framed = Vec::with_capacity(object_type.len() + body.len() + 32);
            framed.extend_from_slice(object_type.as_bytes());
            framed.push(b' ');
            framed.extend_from_slice(body.len().to_string().as_bytes());
            framed.push(0);
            framed.extend_from_slice(body);
            ObjectId::from_raw(format, &sha256(&framed))
        }
    }
}

pub fn digest_bytes(format: ObjectFormat, bytes: &[u8]) -> Result<ObjectId> {
    match format {
        ObjectFormat::Sha1 => ObjectId::from_raw(format, &sha1(bytes)),
        ObjectFormat::Sha256 => ObjectId::from_raw(format, &sha256(bytes)),
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    write_hex_bytes(bytes, &mut out).expect("writing hex to a String cannot fail");
    out
}

fn write_hex_bytes(bytes: &[u8], out: &mut impl fmt::Write) -> fmt::Result {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        out.write_char(HEX[(byte >> 4) as usize] as char)?;
        out.write_char(HEX[(byte & 0x0f) as usize] as char)?;
    }
    Ok(())
}

fn hex_nibble_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_nibble(byte: u8) -> Result<u8> {
    hex_nibble_value(byte)
        .ok_or_else(|| GitError::InvalidObjectId(format!("non-hex byte {:?}", byte as char)))
}

// ---------------------------------------------------------------------------
// SHA-1
//
// The default is a pure-Rust streaming implementation that hashes 64-byte blocks
// straight from the caller's slices, so neither the body nor the framed object is
// copied just to be digested. Enabling the `fast-sha1` feature swaps in the
// RustCrypto `sha1` crate, which dispatches to ARMv8-SHA1 / x86 SHA-NI at runtime;
// the digests are byte-identical, so OIDs are unchanged either way.
// ---------------------------------------------------------------------------

/// SHA-1 of a raw byte slice (already-framed object, bundle prerequisite, etc.).
#[cfg(not(feature = "fast-sha1"))]
fn sha1(input: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1Hasher::new();
    hasher.update(input);
    hasher.finalize()
}

/// SHA-1 of a raw byte slice using the hardware-accelerated backend.
#[cfg(feature = "fast-sha1")]
fn sha1(input: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(input);
    hasher.finalize().into()
}

/// SHA-1 of a git object framed as `"<type> <len>\0<body>"`, fed as separate
/// updates so the body is never copied into a combined buffer.
#[cfg(not(feature = "fast-sha1"))]
fn sha1_object_digest(object_type: &str, body: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1Hasher::new();
    hasher.update(object_type.as_bytes());
    hasher.update(b" ");
    hasher.update(body.len().to_string().as_bytes());
    hasher.update(&[0u8]);
    hasher.update(body);
    hasher.finalize()
}

#[cfg(feature = "fast-sha1")]
fn sha1_object_digest(object_type: &str, body: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(object_type.as_bytes());
    hasher.update(b" ");
    hasher.update(body.len().to_string().as_bytes());
    hasher.update([0u8]);
    hasher.update(body);
    hasher.finalize().into()
}

/// Streaming pure-Rust SHA-1: feeds full 64-byte blocks directly from each
/// `update` slice and buffers only the sub-block remainder, so large inputs are
/// hashed without an intermediate copy.
#[cfg(not(feature = "fast-sha1"))]
struct Sha1Hasher {
    state: [u32; 5],
    block: [u8; 64],
    block_len: usize,
    total_len: u64,
}

#[cfg(not(feature = "fast-sha1"))]
impl Sha1Hasher {
    fn new() -> Self {
        Self {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0],
            block: [0u8; 64],
            block_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.block_len > 0 {
            let take = (64 - self.block_len).min(data.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
            self.block_len += take;
            data = &data[take..];
            if self.block_len == 64 {
                let block = self.block;
                sha1_compress(&mut self.state, &block);
                self.block_len = 0;
            }
        }
        while data.len() >= 64 {
            sha1_compress(&mut self.state, &data[..64]);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.block_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 20] {
        let bit_len = self.total_len.wrapping_mul(8);
        // 0x80, zero pad to a 56 mod 64 boundary, then the 64-bit big-endian length.
        // From a sub-block remainder this is at most two more blocks (128 bytes).
        let mut tail = [0u8; 128];
        tail[..self.block_len].copy_from_slice(&self.block[..self.block_len]);
        tail[self.block_len] = 0x80;
        let total = if self.block_len < 56 { 64 } else { 128 };
        tail[total - 8..total].copy_from_slice(&bit_len.to_be_bytes());
        sha1_compress(&mut self.state, &tail[..64]);
        if total == 128 {
            sha1_compress(&mut self.state, &tail[64..128]);
        }
        let mut out = [0u8; 20];
        out[0..4].copy_from_slice(&self.state[0].to_be_bytes());
        out[4..8].copy_from_slice(&self.state[1].to_be_bytes());
        out[8..12].copy_from_slice(&self.state[2].to_be_bytes());
        out[12..16].copy_from_slice(&self.state[3].to_be_bytes());
        out[16..20].copy_from_slice(&self.state[4].to_be_bytes());
        out
    }
}

/// Mix one 64-byte block into the SHA-1 state. `block` must be at least 64 bytes.
#[cfg(not(feature = "fast-sha1"))]
fn sha1_compress(state: &mut [u32; 5], block: &[u8]) {
    let mut w = [0u32; 80];
    for (i, word) in w.iter_mut().take(16).enumerate() {
        let offset = i * 4;
        *word = u32::from_be_bytes([
            block[offset],
            block[offset + 1],
            block[offset + 2],
            block[offset + 3],
        ]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }

    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];

    for (i, word) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | ((!b) & d), 0x5a827999u32),
            20..=39 => (b ^ c ^ d, 0x6ed9eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
            _ => (b ^ c ^ d, 0xca62c1d6),
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (input.len() as u64) * 8;
    let mut msg = input.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let offset = i * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0; 32];
    for (idx, word) in h.iter().enumerate() {
        out[idx * 4..idx * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_blob_matches_git_known_value() {
        let oid = object_id_for_bytes(ObjectFormat::Sha1, "blob", b"hello\n")
            .expect("known blob should hash as sha1");
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    #[test]
    fn sha256_blob_matches_git_known_value() {
        let oid = object_id_for_bytes(ObjectFormat::Sha256, "blob", b"hello\n")
            .expect("known blob should hash as sha256");
        assert_eq!(
            oid.to_hex(),
            "2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4"
        );
    }

    #[test]
    fn object_id_round_trips_hex() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("valid sha1 hex");
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    #[test]
    fn object_id_writes_hex_without_allocating_in_the_writer() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "CE013625030BA8DBA906F756967F9E9CA394464A",
        )
        .expect("valid uppercase sha1 hex");

        let mut out = String::new();
        oid.write_hex(&mut out)
            .expect("writing object id hex to a String should not fail");

        assert_eq!(out, "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(oid.to_hex(), out);
        assert_eq!(format!("{oid}"), out);
    }

    #[test]
    fn object_id_matches_hex_prefixes_by_nibble() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("valid sha1 hex");

        assert!(oid.hex_prefix_matches(b""));
        assert!(oid.hex_prefix_matches(b"c"));
        assert!(oid.hex_prefix_matches(b"ce013"));
        assert!(oid.hex_prefix_matches(b"CE013625"));
        assert!(oid.hex_prefix_matches(b"ce013625030ba8dba906f756967f9e9ca394464a"));

        assert!(!oid.hex_prefix_matches(b"d"));
        assert!(!oid.hex_prefix_matches(b"ce014"));
        assert!(!oid.hex_prefix_matches(b"ce01x"));

        let mut too_long = oid.to_hex();
        too_long.push('0');
        assert!(!oid.hex_prefix_matches(too_long.as_bytes()));
    }

    #[test]
    fn object_id_abbrev_hex_len_clamps_to_format_width() {
        let sha1 = ObjectId::null(ObjectFormat::Sha1);
        let sha256 = ObjectId::null(ObjectFormat::Sha256);

        assert_eq!(sha1.abbrev_hex_len(0), 0);
        assert_eq!(sha1.abbrev_hex_len(12), 12);
        assert_eq!(sha1.abbrev_hex_len(80), ObjectFormat::Sha1.hex_len());
        assert_eq!(sha256.abbrev_hex_len(80), ObjectFormat::Sha256.hex_len());
    }

    #[test]
    fn signature_parses_a_normal_ident_and_round_trips() {
        let line = b"A U Thor <author@example.com> 1700000000 +0000";
        let sig = Signature::from_ident_line(line).expect("well-formed ident parses");
        assert_eq!(sig.name.as_bytes(), b"A U Thor");
        assert_eq!(sig.email.as_bytes(), b"author@example.com");
        assert_eq!(sig.time.seconds, 1_700_000_000);
        assert_eq!(sig.time.timezone_offset_minutes, 0);
        assert!(!sig.time.negative_utc);
        // Byte-exact round-trip, and the canonical form matches here too.
        assert_eq!(sig.to_ident_bytes(), line);
        assert_eq!(sig.to_canonical_ident_bytes(), line);
    }

    #[test]
    fn signature_parses_positive_half_hour_offset() {
        let line = b"Half Hour <hh@example.com> 1500000000 +0530";
        let sig = Signature::from_ident_line(line).expect("offset ident parses");
        assert_eq!(sig.time.timezone_offset_minutes, 330);
        assert!(!sig.time.negative_utc);
        assert_eq!(sig.time.offset_token(), "+0530");
        assert_eq!(sig.to_ident_bytes(), line);
        assert_eq!(sig.to_canonical_ident_bytes(), line);
    }

    #[test]
    fn signature_parses_negative_offset() {
        let line = b"Western <w@example.com> 1500000000 -0500";
        let sig = Signature::from_ident_line(line).expect("negative offset parses");
        assert_eq!(sig.time.timezone_offset_minutes, -300);
        assert!(!sig.time.negative_utc);
        assert_eq!(sig.time.offset_token(), "-0500");
        assert_eq!(sig.to_ident_bytes(), line);
    }

    #[test]
    fn signature_preserves_negative_zero_timezone_distinct_from_positive_zero() {
        let negative = b"Unknown Zone <uz@example.com> 1500000000 -0000";
        let positive = b"Known Zone <kz@example.com> 1500000000 +0000";

        let neg = Signature::from_ident_line(negative).expect("-0000 parses");
        let pos = Signature::from_ident_line(positive).expect("+0000 parses");

        // Both are zero minutes from UTC...
        assert_eq!(neg.time.timezone_offset_minutes, 0);
        assert_eq!(pos.time.timezone_offset_minutes, 0);
        // ...but the sentinel flag distinguishes them, so the times differ.
        assert!(neg.time.negative_utc);
        assert!(!pos.time.negative_utc);
        assert_ne!(neg.time, pos.time);

        // And the distinction survives re-serialization, byte-for-byte.
        assert_eq!(neg.time.offset_token(), "-0000");
        assert_eq!(pos.time.offset_token(), "+0000");
        assert_eq!(neg.to_ident_bytes(), negative);
        assert_eq!(pos.to_ident_bytes(), positive);
        assert_eq!(neg.to_canonical_ident_bytes(), negative);
        assert_eq!(pos.to_canonical_ident_bytes(), positive);
        assert_ne!(neg.to_ident_bytes(), pos.to_ident_bytes());
    }

    #[test]
    fn signature_handles_empty_name_and_email() {
        // git permits an empty name and/or empty email; the delimiters still
        // anchor the parse.
        let line = b" <> 0 +0000";
        let sig = Signature::from_ident_line(line).expect("empty name/email parses");
        assert_eq!(sig.name.as_bytes(), b"");
        assert_eq!(sig.email.as_bytes(), b"");
        assert_eq!(sig.time.seconds, 0);
        assert_eq!(sig.to_ident_bytes(), line);
    }

    #[test]
    fn signature_keeps_angle_brackets_inside_the_name() {
        // The email is delimited by the *last* '<'/'>' pair, so a name that
        // itself contains angle brackets parses with the trailing pair as the
        // email and round-trips exactly.
        let line = b"Weird <Name> <weird@example.com> 1 +0000";
        let sig = Signature::from_ident_line(line).expect("bracketed name parses");
        assert_eq!(sig.name.as_bytes(), b"Weird <Name>");
        assert_eq!(sig.email.as_bytes(), b"weird@example.com");
        assert_eq!(sig.to_ident_bytes(), line);
    }

    #[test]
    fn signature_round_trips_non_canonical_whitespace_via_raw() {
        // An ident with two spaces before the email is not git's canonical form,
        // but the parse-view must still reproduce it byte-for-byte from `raw`.
        // (Only the canonical renderer normalizes the spacing.)
        let line = b"Spaced  <spaced@example.com> 5 +0000";
        let sig = Signature::from_ident_line(line).expect("non-canonical ident parses");
        // The name keeps the extra space (only one separator space is trimmed).
        assert_eq!(sig.name.as_bytes(), b"Spaced ");
        assert_eq!(sig.to_ident_bytes(), line);
    }

    #[test]
    fn signature_rejects_malformed_idents() {
        // No email delimiters.
        assert!(Signature::from_ident_line(b"No Email Here 0 +0000").is_none());
        // Missing the time tail entirely.
        assert!(Signature::from_ident_line(b"A U Thor <a@example.com>").is_none());
        // Non-numeric timestamp.
        assert!(Signature::from_ident_line(b"A U Thor <a@example.com> later +0000").is_none());
        // Malformed timezone token (wrong width).
        assert!(Signature::from_ident_line(b"A U Thor <a@example.com> 0 +00").is_none());
        // Timezone token missing a sign.
        assert!(Signature::from_ident_line(b"A U Thor <a@example.com> 0 0000").is_none());
    }

    #[test]
    fn git_time_constructors_set_the_sentinel() {
        assert!(!GitTime::new(0, 0).negative_utc);
        assert_eq!(GitTime::new(0, 330).offset_token(), "+0530");
        let unknown = GitTime::with_negative_utc(42);
        assert!(unknown.negative_utc);
        assert_eq!(unknown.seconds, 42);
        assert_eq!(unknown.offset_token(), "-0000");
    }

    #[test]
    fn full_name_accepts_valid_ref_names() {
        let name = FullName::new("refs/heads/main").expect("valid ref name");
        assert_eq!(name.as_str(), "refs/heads/main");
        assert_eq!(name, "refs/heads/main");
        assert_eq!(format!("{name}"), "refs/heads/main");
        assert_eq!(String::from(name.clone()), "refs/heads/main");
        let borrowed: &str = name.borrow();
        assert_eq!(borrowed, "refs/heads/main");
    }

    #[test]
    fn full_name_rejects_invalid_ref_names() {
        assert!(FullName::new("").is_err());
        assert!(FullName::new(" refs/heads/main").is_err());
        assert!(FullName::new("refs/heads/main ").is_err());
        assert!(FullName::new("refs//heads/main").is_err());
        assert!(FullName::new("refs/heads/\nmain").is_err());
    }

    #[test]
    fn cli_exit_codes_match_git_taxonomy() {
        assert_eq!(CliExit::Ok.code(), 0);
        assert_eq!(CliExit::UserError.code(), 128);
        assert_eq!(CliExit::Usage.code(), 129);
        assert_eq!(CliExit::Custom(1).code(), 1);
        assert_eq!(CliExit::Custom(5).code(), 5);
    }

    #[test]
    fn git_error_cli_exit_code_mapping() {
        assert_eq!(GitError::Exit(129).cli_exit_code(), 129);
        assert_eq!(GitError::Exit(128).cli_exit_code(), 128);
        assert_eq!(GitError::usage("unknown option").cli_exit_code(), 129);
        assert_eq!(
            GitError::user_error("not a git repository").cli_exit_code(),
            128
        );
        assert_eq!(
            GitError::cli_exit(CliExit::Custom(2), "diff found changes").cli_exit_code(),
            2
        );
        assert_eq!(GitError::Command("bad value".into()).cli_exit_code(), 1);
        assert_eq!(GitError::not_found("missing ref").cli_exit_code(), 1);
    }

    #[test]
    fn git_error_cli_displays_message_only() {
        let err = GitError::usage("unknown option `--foo'");
        assert_eq!(err.to_string(), "unknown option `--foo'");
    }

    #[test]
    fn bstring_round_trips_bytes_and_displays_lossily() {
        let path = BString::from_bytes(b"src/\xFF.txt");
        assert_eq!(path.as_bytes(), b"src/\xFF.txt");
        let borrowed: &[u8] = path.borrow();
        assert_eq!(borrowed, b"src/\xFF.txt".as_slice());
        assert_eq!(format!("{path}"), "src/\u{FFFD}.txt");
        assert_eq!(path, b"src/\xFF.txt");
        assert_eq!(path.clone().into_bytes(), b"src/\xFF.txt".to_vec());
    }
}
