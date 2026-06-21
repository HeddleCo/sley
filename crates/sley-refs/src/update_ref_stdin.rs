//! Borrowed parser for `git update-ref --stdin` command records.
//!
//! The parser mirrors the CLI command stream lexer shape without performing
//! object resolution, ref validation, or transaction dispatch. Plain arguments
//! borrow from the input; C-quoted or lossy UTF-8 arguments are represented as
//! owned [`Cow`] values.

use std::borrow::Cow;
use std::error::Error;
use std::fmt;

/// Result type returned by the `update-ref --stdin` parser.
pub type ParseResult<T> = std::result::Result<T, UpdateRefStdinParseError>;

/// Error produced while parsing an `update-ref --stdin` command record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRefStdinParseError {
    message: String,
}

impl UpdateRefStdinParseError {
    /// Create a parser error with a user-facing message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Return the parser error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for UpdateRefStdinParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for UpdateRefStdinParseError {}

/// Terminator mode used by an `update-ref --stdin` stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateRefStdinTerminator {
    /// Newline-terminated text input. Arguments are separated by one space and
    /// may use Git's C-style quoting.
    Newline,
    /// NUL-terminated binary input. Arguments after the first command record are
    /// separate NUL records and are not C-unquoted.
    Nul,
}

impl UpdateRefStdinTerminator {
    /// Return the terminator byte for this mode.
    pub const fn byte(self) -> u8 {
        match self {
            Self::Newline => b'\n',
            Self::Nul => b'\0',
        }
    }
}

/// Command verb recognized by `update-ref --stdin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateRefStdinVerb {
    Update,
    Create,
    Delete,
    Verify,
    SymrefUpdate,
    SymrefCreate,
    SymrefDelete,
    SymrefVerify,
    Option,
    Start,
    Prepare,
    Abort,
    Commit,
}

impl UpdateRefStdinVerb {
    /// Return the command spelling used in the stdin stream.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Create => "create",
            Self::Delete => "delete",
            Self::Verify => "verify",
            Self::SymrefUpdate => "symref-update",
            Self::SymrefCreate => "symref-create",
            Self::SymrefDelete => "symref-delete",
            Self::SymrefVerify => "symref-verify",
            Self::Option => "option",
            Self::Start => "start",
            Self::Prepare => "prepare",
            Self::Abort => "abort",
            Self::Commit => "commit",
        }
    }

    /// Number of additional NUL records a caller must read after the first
    /// command record before calling [`parse_update_ref_stdin_nul`].
    pub const fn additional_nul_records(self) -> usize {
        match self {
            Self::Update => 2,
            Self::Create
            | Self::Delete
            | Self::Verify
            | Self::SymrefCreate
            | Self::SymrefDelete
            | Self::SymrefVerify => 1,
            Self::SymrefUpdate => 3,
            Self::Option | Self::Start | Self::Prepare | Self::Abort | Self::Commit => 0,
        }
    }
}

/// A parsed `update-ref --stdin` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRefStdinCommand<'a> {
    Update {
        refname: Cow<'a, str>,
        new: UpdateRefStdinOid<'a>,
        old: Option<UpdateRefStdinOid<'a>>,
    },
    Create {
        refname: Cow<'a, str>,
        new: UpdateRefStdinOid<'a>,
    },
    Delete {
        refname: Cow<'a, str>,
        old: Option<UpdateRefStdinOid<'a>>,
    },
    Verify {
        refname: Cow<'a, str>,
        old: Option<UpdateRefStdinOid<'a>>,
    },
    SymrefUpdate {
        refname: Cow<'a, str>,
        target: Cow<'a, str>,
        old: Option<UpdateRefStdinSymrefOld<'a>>,
    },
    SymrefCreate {
        refname: Cow<'a, str>,
        target: Cow<'a, str>,
    },
    SymrefDelete {
        refname: Cow<'a, str>,
        target: Option<Cow<'a, str>>,
    },
    SymrefVerify {
        refname: Cow<'a, str>,
        target: Option<Cow<'a, str>>,
    },
    Option(UpdateRefStdinOption),
    Start,
    Prepare,
    Abort,
    Commit,
}

impl UpdateRefStdinCommand<'_> {
    /// Return this command's verb.
    pub fn verb(&self) -> UpdateRefStdinVerb {
        match self {
            Self::Update { .. } => UpdateRefStdinVerb::Update,
            Self::Create { .. } => UpdateRefStdinVerb::Create,
            Self::Delete { .. } => UpdateRefStdinVerb::Delete,
            Self::Verify { .. } => UpdateRefStdinVerb::Verify,
            Self::SymrefUpdate { .. } => UpdateRefStdinVerb::SymrefUpdate,
            Self::SymrefCreate { .. } => UpdateRefStdinVerb::SymrefCreate,
            Self::SymrefDelete { .. } => UpdateRefStdinVerb::SymrefDelete,
            Self::SymrefVerify { .. } => UpdateRefStdinVerb::SymrefVerify,
            Self::Option(_) => UpdateRefStdinVerb::Option,
            Self::Start => UpdateRefStdinVerb::Start,
            Self::Prepare => UpdateRefStdinVerb::Prepare,
            Self::Abort => UpdateRefStdinVerb::Abort,
            Self::Commit => UpdateRefStdinVerb::Commit,
        }
    }
}

/// Parsed OID-shaped argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRefStdinOid<'a> {
    /// The argument was present but empty.
    Empty,
    /// The argument was present and non-empty.
    Value(Cow<'a, str>),
}

impl<'a> UpdateRefStdinOid<'a> {
    /// Construct a non-empty OID argument.
    pub fn value(value: impl Into<Cow<'a, str>>) -> Self {
        Self::Value(value.into())
    }

    /// Return the non-empty value, if any.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Empty => None,
            Self::Value(value) => Some(value.as_ref()),
        }
    }
}

/// Parsed `option` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateRefStdinOption {
    NoDeref,
}

/// Parsed old-value selector for `symref-update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRefStdinSymrefOld<'a> {
    Ref(Cow<'a, str>),
    Oid(Cow<'a, str>),
}

struct CommandSpec {
    verb: UpdateRefStdinVerb,
    prefix: &'static str,
    args: usize,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        verb: UpdateRefStdinVerb::Update,
        prefix: "update",
        args: 3,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::Create,
        prefix: "create",
        args: 2,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::Delete,
        prefix: "delete",
        args: 2,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::Verify,
        prefix: "verify",
        args: 2,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::SymrefUpdate,
        prefix: "symref-update",
        args: 4,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::SymrefCreate,
        prefix: "symref-create",
        args: 2,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::SymrefDelete,
        prefix: "symref-delete",
        args: 2,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::SymrefVerify,
        prefix: "symref-verify",
        args: 2,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::Option,
        prefix: "option",
        args: 1,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::Start,
        prefix: "start",
        args: 0,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::Prepare,
        prefix: "prepare",
        args: 0,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::Abort,
        prefix: "abort",
        args: 0,
    },
    CommandSpec {
        verb: UpdateRefStdinVerb::Commit,
        prefix: "commit",
        args: 0,
    },
];

/// Classify the first record before dispatching it.
pub fn classify_update_ref_stdin_record(record: &[u8]) -> ParseResult<()> {
    match record.first().copied() {
        None => Err(parse_error("empty command in input")),
        Some(c) if is_space(c) => Err(parse_error(format!(
            "whitespace before command: {}",
            String::from_utf8_lossy(record)
        ))),
        _ => Ok(()),
    }
}

/// Return how many more NUL records are needed to parse `first_record`.
pub fn update_ref_stdin_nul_additional_records(first_record: &[u8]) -> ParseResult<usize> {
    classify_update_ref_stdin_record(first_record)?;
    let Some((spec, _)) = match_command(first_record, UpdateRefStdinTerminator::Nul) else {
        return Err(bad_command(first_record));
    };
    Ok(spec.verb.additional_nul_records())
}

/// Parse one newline-mode `update-ref --stdin` record.
///
/// Pass the record without its trailing newline.
pub fn parse_update_ref_stdin_line(line: &[u8]) -> ParseResult<UpdateRefStdinCommand<'_>> {
    classify_update_ref_stdin_record(line)?;
    let Some((spec, arg_start)) = match_command(line, UpdateRefStdinTerminator::Newline) else {
        return Err(bad_command(line));
    };
    let cursor = ArgCursor::new(&line[arg_start..]);
    parse_cursor_command(spec.verb, cursor)
}

/// Parse one NUL-mode `update-ref --stdin` logical command.
///
/// `first_record` is the command record without its trailing NUL. Read
/// [`update_ref_stdin_nul_additional_records`] more records and pass them as
/// `additional_records`.
pub fn parse_update_ref_stdin_nul<'a>(
    first_record: &'a [u8],
    additional_records: &[&'a [u8]],
) -> ParseResult<UpdateRefStdinCommand<'a>> {
    classify_update_ref_stdin_record(first_record)?;
    let Some((spec, arg_start)) = match_command(first_record, UpdateRefStdinTerminator::Nul) else {
        return Err(bad_command(first_record));
    };
    let expected = spec.verb.additional_nul_records();
    if additional_records.len() != expected {
        return Err(parse_error(format!(
            "{}: expected {expected} additional NUL record(s), got {}",
            spec.verb.as_str(),
            additional_records.len()
        )));
    }
    parse_nul_command(spec.verb, &first_record[arg_start..], additional_records)
}

fn match_command(
    input: &[u8],
    terminator: UpdateRefStdinTerminator,
) -> Option<(&'static CommandSpec, usize)> {
    for spec in COMMANDS {
        let prefix = spec.prefix.as_bytes();
        if !input.starts_with(prefix) {
            continue;
        }
        let sep = if spec.args > 0 {
            b' '
        } else {
            terminator.byte()
        };
        let after = input.get(prefix.len()).copied();
        let matched = match after {
            Some(byte) => byte == sep,
            None => spec.args == 0,
        };
        if matched {
            let start = prefix.len() + usize::from(spec.args > 0 && after.is_some());
            return Some((spec, start.min(input.len())));
        }
    }
    None
}

fn parse_cursor_command<'a>(
    verb: UpdateRefStdinVerb,
    mut cursor: ArgCursor<'a>,
) -> ParseResult<UpdateRefStdinCommand<'a>> {
    match verb {
        UpdateRefStdinVerb::Option => {
            let option = cursor.remainder();
            if option == "no-deref" {
                Ok(UpdateRefStdinCommand::Option(UpdateRefStdinOption::NoDeref))
            } else {
                Err(parse_error(format!("option unknown: {option}")))
            }
        }
        UpdateRefStdinVerb::Update => {
            let refname = required_ref(&mut cursor, "update")?;
            let new = required_oid(&mut cursor, "update", refname.as_ref(), "<new-oid>", true)?;
            let old = optional_oid(&mut cursor, "update", refname.as_ref(), false)?;
            cursor.finish("update", refname.as_ref())?;
            Ok(UpdateRefStdinCommand::Update { refname, new, old })
        }
        UpdateRefStdinVerb::Create => {
            let refname = required_ref(&mut cursor, "create")?;
            let new = required_oid(&mut cursor, "create", refname.as_ref(), "<new-oid>", false)?;
            cursor.finish("create", refname.as_ref())?;
            Ok(UpdateRefStdinCommand::Create { refname, new })
        }
        UpdateRefStdinVerb::Delete => {
            let refname = required_ref(&mut cursor, "delete")?;
            let old = optional_oid(&mut cursor, "delete", refname.as_ref(), false)?;
            cursor.finish("delete", refname.as_ref())?;
            Ok(UpdateRefStdinCommand::Delete { refname, old })
        }
        UpdateRefStdinVerb::Verify => {
            let refname = required_ref(&mut cursor, "verify")?;
            let old = optional_oid(&mut cursor, "verify", refname.as_ref(), false)?;
            cursor.finish("verify", refname.as_ref())?;
            Ok(UpdateRefStdinCommand::Verify { refname, old })
        }
        UpdateRefStdinVerb::SymrefCreate => {
            let refname = required_ref(&mut cursor, "symref-create")?;
            let Some(target) = cursor.parse_next_refname()? else {
                return Err(missing_field(
                    "symref-create",
                    refname.as_ref(),
                    "<new-target>",
                ));
            };
            cursor.finish("symref-create", refname.as_ref())?;
            Ok(UpdateRefStdinCommand::SymrefCreate { refname, target })
        }
        UpdateRefStdinVerb::SymrefUpdate => {
            let refname = required_ref(&mut cursor, "symref-update")?;
            let Some(target) = cursor.parse_next_refname()? else {
                return Err(missing_field(
                    "symref-update",
                    refname.as_ref(),
                    "<new-target>",
                ));
            };
            let old = parse_symref_old(&mut cursor, refname.as_ref())?;
            cursor.finish("symref-update", refname.as_ref())?;
            Ok(UpdateRefStdinCommand::SymrefUpdate {
                refname,
                target,
                old,
            })
        }
        UpdateRefStdinVerb::SymrefDelete => {
            let refname = required_ref(&mut cursor, "symref-delete")?;
            let target = cursor.parse_next_refname()?;
            cursor.finish("symref-delete", refname.as_ref())?;
            Ok(UpdateRefStdinCommand::SymrefDelete { refname, target })
        }
        UpdateRefStdinVerb::SymrefVerify => {
            let refname = required_ref(&mut cursor, "symref-verify")?;
            let target = cursor.parse_next_refname()?;
            cursor.finish("symref-verify", refname.as_ref())?;
            Ok(UpdateRefStdinCommand::SymrefVerify { refname, target })
        }
        UpdateRefStdinVerb::Start => {
            cursor.finish("start", "")?;
            Ok(UpdateRefStdinCommand::Start)
        }
        UpdateRefStdinVerb::Prepare => {
            cursor.finish("prepare", "")?;
            Ok(UpdateRefStdinCommand::Prepare)
        }
        UpdateRefStdinVerb::Abort => {
            cursor.finish("abort", "")?;
            Ok(UpdateRefStdinCommand::Abort)
        }
        UpdateRefStdinVerb::Commit => {
            cursor.finish("commit", "")?;
            Ok(UpdateRefStdinCommand::Commit)
        }
    }
}

fn parse_nul_command<'a>(
    verb: UpdateRefStdinVerb,
    first_arg: &'a [u8],
    records: &[&'a [u8]],
) -> ParseResult<UpdateRefStdinCommand<'a>> {
    match verb {
        UpdateRefStdinVerb::Option => {
            let option = cow_from_bytes(first_arg);
            if option == "no-deref" {
                Ok(UpdateRefStdinCommand::Option(UpdateRefStdinOption::NoDeref))
            } else {
                Err(parse_error(format!("option unknown: {option}")))
            }
        }
        UpdateRefStdinVerb::Update => {
            let refname = required_nul_ref("update", first_arg)?;
            let new = required_nul_oid("update", refname.as_ref(), "<new-oid>", records[0], true)?;
            let old = optional_nul_oid(records[1], false);
            Ok(UpdateRefStdinCommand::Update { refname, new, old })
        }
        UpdateRefStdinVerb::Create => {
            let refname = required_nul_ref("create", first_arg)?;
            let new = required_nul_oid("create", refname.as_ref(), "<new-oid>", records[0], false)?;
            Ok(UpdateRefStdinCommand::Create { refname, new })
        }
        UpdateRefStdinVerb::Delete => {
            let refname = required_nul_ref("delete", first_arg)?;
            let old = optional_nul_oid(records[0], false);
            Ok(UpdateRefStdinCommand::Delete { refname, old })
        }
        UpdateRefStdinVerb::Verify => {
            let refname = required_nul_ref("verify", first_arg)?;
            let old = optional_nul_oid(records[0], false);
            Ok(UpdateRefStdinCommand::Verify { refname, old })
        }
        UpdateRefStdinVerb::SymrefCreate => {
            let refname = required_nul_ref("symref-create", first_arg)?;
            let target = required_nul_ref_for("symref-create", refname.as_ref(), records[0])?;
            Ok(UpdateRefStdinCommand::SymrefCreate { refname, target })
        }
        UpdateRefStdinVerb::SymrefUpdate => {
            let refname = required_nul_ref("symref-update", first_arg)?;
            let target = required_nul_ref_for("symref-update", refname.as_ref(), records[0])?;
            let old = parse_nul_symref_old(refname.as_ref(), records[1], records[2])?;
            Ok(UpdateRefStdinCommand::SymrefUpdate {
                refname,
                target,
                old,
            })
        }
        UpdateRefStdinVerb::SymrefDelete => {
            let refname = required_nul_ref("symref-delete", first_arg)?;
            let target = optional_nul_arg(records[0]);
            Ok(UpdateRefStdinCommand::SymrefDelete { refname, target })
        }
        UpdateRefStdinVerb::SymrefVerify => {
            let refname = required_nul_ref("symref-verify", first_arg)?;
            let target = optional_nul_arg(records[0]);
            Ok(UpdateRefStdinCommand::SymrefVerify { refname, target })
        }
        UpdateRefStdinVerb::Start => Ok(UpdateRefStdinCommand::Start),
        UpdateRefStdinVerb::Prepare => Ok(UpdateRefStdinCommand::Prepare),
        UpdateRefStdinVerb::Abort => Ok(UpdateRefStdinCommand::Abort),
        UpdateRefStdinVerb::Commit => Ok(UpdateRefStdinCommand::Commit),
    }
}

struct ArgCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ArgCursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn cur(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    fn remainder(&self) -> Cow<'a, str> {
        cow_from_bytes(&self.buf[self.pos..])
    }

    fn remainder_string(&self) -> String {
        String::from_utf8_lossy(&self.buf[self.pos..]).into_owned()
    }

    fn parse_arg(&mut self) -> ParseResult<Cow<'a, str>> {
        if self.cur() == Some(b'"') {
            let original_start = self.pos;
            let mut out = Vec::new();
            let consumed = unquote_c_style(&self.buf[self.pos..], &mut out).ok_or_else(|| {
                parse_error(format!(
                    "badly quoted argument: {}",
                    String::from_utf8_lossy(&self.buf[original_start..])
                ))
            })?;
            self.pos += consumed;
            if self.cur().is_some_and(|byte| byte != 0 && !is_space(byte)) {
                return Err(parse_error(format!(
                    "unexpected character after quoted argument: {}",
                    String::from_utf8_lossy(&self.buf[original_start..])
                )));
            }
            Ok(Cow::Owned(String::from_utf8_lossy(&out).into_owned()))
        } else {
            let start = self.pos;
            while let Some(byte) = self.cur() {
                if byte == 0 || is_space(byte) {
                    break;
                }
                self.pos += 1;
            }
            Ok(cow_from_bytes(&self.buf[start..self.pos]))
        }
    }

    fn parse_refname(&mut self) -> ParseResult<Option<Cow<'a, str>>> {
        let refname = self.parse_arg()?;
        if refname.is_empty() {
            Ok(None)
        } else {
            Ok(Some(refname))
        }
    }

    fn parse_next_refname(&mut self) -> ParseResult<Option<Cow<'a, str>>> {
        if !self.skip_delimiter()? {
            return Ok(None);
        }
        self.parse_refname()
    }

    fn parse_next_arg(&mut self) -> ParseResult<Option<Cow<'a, str>>> {
        if !self.skip_delimiter()? {
            return Ok(None);
        }
        let arg = self.parse_arg()?;
        if arg.is_empty() {
            Ok(None)
        } else {
            Ok(Some(arg))
        }
    }

    fn parse_next_oid(
        &mut self,
        command: &str,
        refname: &str,
        _allow_empty: bool,
    ) -> ParseResult<NextOid<'a>> {
        match self.cur() {
            None | Some(0) => return Ok(NextOid::Missing),
            Some(b' ') => {}
            Some(_) => {
                return Err(parse_error(format!(
                    "{command} {refname}: expected SP but got: {}",
                    self.remainder_string()
                )));
            }
        }
        self.pos += 1;
        let arg = self.parse_arg()?;
        if arg.is_empty() {
            Ok(NextOid::Empty)
        } else {
            Ok(NextOid::Value(arg))
        }
    }

    fn finish(&self, command: &str, refname: &str) -> ParseResult<()> {
        match self.cur() {
            None => Ok(()),
            Some(_) => Err(parse_error(format!(
                "{command} {refname}: extra input: {}",
                self.remainder_string()
            ))),
        }
    }

    fn skip_delimiter(&mut self) -> ParseResult<bool> {
        match self.cur() {
            None | Some(0) => Ok(false),
            Some(b' ') => {
                self.pos += 1;
                Ok(true)
            }
            Some(_) => Err(parse_error(format!(
                "expected SP but got: {}",
                self.remainder_string()
            ))),
        }
    }
}

enum NextOid<'a> {
    Missing,
    Empty,
    Value(Cow<'a, str>),
}

fn required_ref<'a>(cursor: &mut ArgCursor<'a>, command: &str) -> ParseResult<Cow<'a, str>> {
    cursor
        .parse_refname()?
        .ok_or_else(|| parse_error(format!("{command}: missing <ref>")))
}

fn required_oid<'a>(
    cursor: &mut ArgCursor<'a>,
    command: &str,
    refname: &str,
    field: &str,
    allow_empty: bool,
) -> ParseResult<UpdateRefStdinOid<'a>> {
    match cursor.parse_next_oid(command, refname, allow_empty)? {
        NextOid::Missing => Err(missing_field(command, refname, field)),
        NextOid::Empty => Ok(UpdateRefStdinOid::Empty),
        NextOid::Value(value) => Ok(UpdateRefStdinOid::Value(value)),
    }
}

fn optional_oid<'a>(
    cursor: &mut ArgCursor<'a>,
    command: &str,
    refname: &str,
    allow_empty: bool,
) -> ParseResult<Option<UpdateRefStdinOid<'a>>> {
    match cursor.parse_next_oid(command, refname, allow_empty)? {
        NextOid::Missing => Ok(None),
        NextOid::Empty => Ok(Some(UpdateRefStdinOid::Empty)),
        NextOid::Value(value) => Ok(Some(UpdateRefStdinOid::Value(value))),
    }
}

fn parse_symref_old<'a>(
    cursor: &mut ArgCursor<'a>,
    refname: &str,
) -> ParseResult<Option<UpdateRefStdinSymrefOld<'a>>> {
    let Some(kind) = cursor.parse_next_arg()? else {
        return Ok(None);
    };
    let Some(value) = cursor.parse_next_arg()? else {
        return Err(parse_error(format!(
            "symref-update {refname}: expected old value"
        )));
    };
    match kind.as_ref() {
        "ref" => Ok(Some(UpdateRefStdinSymrefOld::Ref(value))),
        "oid" => Ok(Some(UpdateRefStdinSymrefOld::Oid(value))),
        other => Err(parse_error(format!(
            "symref-update {refname}: invalid arg '{other}' for old value"
        ))),
    }
}

fn required_nul_ref<'a>(command: &str, record: &'a [u8]) -> ParseResult<Cow<'a, str>> {
    optional_nul_arg(record).ok_or_else(|| parse_error(format!("{command}: missing <ref>")))
}

fn required_nul_ref_for<'a>(
    command: &str,
    refname: &str,
    record: &'a [u8],
) -> ParseResult<Cow<'a, str>> {
    optional_nul_arg(record).ok_or_else(|| missing_field(command, refname, "<new-target>"))
}

fn required_nul_oid<'a>(
    command: &str,
    refname: &str,
    field: &str,
    record: &'a [u8],
    allow_empty: bool,
) -> ParseResult<UpdateRefStdinOid<'a>> {
    optional_nul_oid(record, allow_empty).ok_or_else(|| missing_field(command, refname, field))
}

fn optional_nul_oid<'a>(record: &'a [u8], allow_empty: bool) -> Option<UpdateRefStdinOid<'a>> {
    if record.is_empty() {
        allow_empty.then_some(UpdateRefStdinOid::Empty)
    } else {
        Some(UpdateRefStdinOid::Value(cow_from_bytes(record)))
    }
}

fn optional_nul_arg(record: &[u8]) -> Option<Cow<'_, str>> {
    if record.is_empty() {
        None
    } else {
        Some(cow_from_bytes(record))
    }
}

fn parse_nul_symref_old<'a>(
    refname: &str,
    kind_record: &'a [u8],
    value_record: &'a [u8],
) -> ParseResult<Option<UpdateRefStdinSymrefOld<'a>>> {
    let Some(kind) = optional_nul_arg(kind_record) else {
        return Ok(None);
    };
    let Some(value) = optional_nul_arg(value_record) else {
        return Err(parse_error(format!(
            "symref-update {refname}: expected old value"
        )));
    };
    match kind.as_ref() {
        "ref" => Ok(Some(UpdateRefStdinSymrefOld::Ref(value))),
        "oid" => Ok(Some(UpdateRefStdinSymrefOld::Oid(value))),
        other => Err(parse_error(format!(
            "symref-update {refname}: invalid arg '{other}' for old value"
        ))),
    }
}

fn missing_field(command: &str, refname: &str, field: &str) -> UpdateRefStdinParseError {
    parse_error(format!("{command} {refname}: missing {field}"))
}

fn bad_command(input: &[u8]) -> UpdateRefStdinParseError {
    parse_error(format!("bad command: {}", String::from_utf8_lossy(input)))
}

fn parse_error(message: impl Into<String>) -> UpdateRefStdinParseError {
    UpdateRefStdinParseError::new(message)
}

fn cow_from_bytes(bytes: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn unquote_c_style(input: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    let mut i = 0usize;
    if input.get(i).copied()? != b'"' {
        return None;
    }
    i += 1;
    loop {
        while let Some(&byte) = input.get(i) {
            if byte == b'"' || byte == b'\\' || byte == 0 {
                break;
            }
            out.push(byte);
            i += 1;
        }
        match input.get(i).copied() {
            Some(b'"') => {
                i += 1;
                return Some(i);
            }
            Some(b'\\') => {
                i += 1;
            }
            _ => return None,
        }
        let escaped = input.get(i).copied()?;
        i += 1;
        let decoded = match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'\\' | b'"' => escaped,
            b'0'..=b'3' => {
                let mut value = ((escaped - b'0') as u32) << 6;
                let d1 = input.get(i).copied()?;
                if !(b'0'..=b'7').contains(&d1) {
                    return None;
                }
                i += 1;
                value |= ((d1 - b'0') as u32) << 3;
                let d2 = input.get(i).copied()?;
                if !(b'0'..=b'7').contains(&d2) {
                    return None;
                }
                i += 1;
                value |= (d2 - b'0') as u32;
                value as u8
            }
            _ => return None,
        };
        out.push(decoded);
    }
}
