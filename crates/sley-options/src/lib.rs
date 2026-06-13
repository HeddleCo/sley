use std::fmt;

#[derive(Clone, Copy)]
pub struct OptionSpec<'a> {
    pub short: Option<char>,
    pub long: Option<&'a str>,
    pub value: OptValue<'a>,
    pub flags: OptFlags,
    pub help: &'a str,
}

#[derive(Clone, Copy)]
pub enum OptValue<'a> {
    Bool,
    Int(&'a str),
    Magnitude(&'a str),
    Str(&'a str),
    Enum {
        metavar: &'a str,
        parse: fn(&str) -> bool,
    },
    Callback {
        metavar: Option<&'a str>,
        parse: fn(CallbackValue<'_>) -> Result<Option<String>, String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptFlags(u8);

impl OptFlags {
    pub const NONE: Self = Self(0);
    pub const NONEG: Self = Self(1 << 0);
    pub const OPTARG: Self = Self(1 << 1);
    pub const NODASH: Self = Self(1 << 2);
    pub const HIDDEN: Self = Self(1 << 3);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for OptFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackValue<'a> {
    pub option: OptionName<'a>,
    pub value: Option<&'a str>,
    pub unset: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionName<'a> {
    Short(char),
    Long(&'a str),
    NegatedLong(&'a str),
}

impl fmt::Display for OptionName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short(short) => write!(f, "switch `{short}'"),
            Self::Long(long) => write!(f, "option `{long}'"),
            Self::NegatedLong(long) => write!(f, "option `no-{long}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed<'a> {
    pub options: Vec<ParsedOption<'a>>,
    pub positionals: Vec<&'a str>,
}

impl<'a> Parsed<'a> {
    pub fn occurrences(&self, long: &str) -> impl Iterator<Item = &ParsedOption<'a>> {
        self.options
            .iter()
            .filter(move |option| option.long == Some(long))
    }

    pub fn last_bool(&self, long: &str, default: bool) -> bool {
        self.occurrences(long)
            .filter_map(|option| match option.value {
                ParsedValue::Bool(value) => Some(value),
                _ => None,
            })
            .last()
            .unwrap_or(default)
    }

    pub fn last_str(&self, long: &str) -> Option<&'a str> {
        self.occurrences(long)
            .filter_map(|option| match option.value {
                ParsedValue::Str(value) => Some(value),
                _ => None,
            })
            .last()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedOption<'a> {
    pub spec_index: usize,
    pub short: Option<char>,
    pub long: Option<&'a str>,
    pub name: OptionName<'a>,
    pub value: ParsedValue<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedValue<'a> {
    Bool(bool),
    Int(i64),
    Magnitude(i64),
    Str(&'a str),
    Enum(&'a str),
    Callback(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
    message: Option<String>,
    usage: String,
    exit_code: i32,
}

impl UsageError {
    pub fn new(message: impl Into<String>, usage: String) -> Self {
        Self {
            message: Some(message.into()),
            usage,
            exit_code: 129,
        }
    }

    pub fn usage_only(usage: String) -> Self {
        Self {
            message: None,
            usage,
            exit_code: 129,
        }
    }

    pub const fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn usage(&self) -> &str {
        &self.usage
    }

    pub fn render_stderr(&self) -> String {
        let mut out = String::new();
        if let Some(message) = &self.message {
            out.push_str("error: ");
            out.push_str(message);
            out.push('\n');
        }
        out.push_str(&self.usage);
        out
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render_stderr())
    }
}

impl std::error::Error for UsageError {}

pub fn parse_options<'a>(
    args: &'a [String],
    specs: &'a [OptionSpec<'a>],
    usage: &'a [&'a str],
) -> Result<Parsed<'a>, UsageError> {
    let mut parser = Parser {
        args,
        specs,
        usage,
        index: 0,
        parsed: Parsed {
            options: Vec::new(),
            positionals: Vec::new(),
        },
    };
    parser.parse()
}

pub fn usage_with_options(specs: &[OptionSpec<'_>], usage: &[&str]) -> String {
    let mut out = String::new();
    for (index, line) in usage.iter().enumerate() {
        if index == 0 {
            out.push_str("usage: ");
        } else {
            out.push_str("   or: ");
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');

    for spec in specs {
        if spec.flags.contains(OptFlags::HIDDEN) {
            continue;
        }

        let mut option = String::from("    ");
        if let Some(short) = spec.short {
            if spec.flags.contains(OptFlags::NODASH) {
                option.push(short);
            } else {
                option.push('-');
                option.push(short);
            }
        }
        if let Some(long) = spec.long {
            if spec.short.is_some() {
                option.push_str(", ");
            }
            option.push_str("--");
            if spec.value.is_bool() && !spec.flags.contains(OptFlags::NONEG) {
                option.push_str("[no-]");
            }
            option.push_str(long);
        }
        option.push_str(&spec.value.usage_suffix(spec.flags, spec.long.is_some()));

        let width = option.chars().count();
        out.push_str(&option);
        if width < 30 {
            out.push_str(&" ".repeat(30 - width));
        } else {
            out.push('\n');
            out.push_str(&" ".repeat(30));
        }
        out.push_str(spec.help);
        out.push('\n');
    }
    out.push('\n');
    out
}

struct Parser<'a> {
    args: &'a [String],
    specs: &'a [OptionSpec<'a>],
    usage: &'a [&'a str],
    index: usize,
    parsed: Parsed<'a>,
}

impl<'a> Parser<'a> {
    fn parse(&mut self) -> Result<Parsed<'a>, UsageError> {
        while let Some(arg) = self.args.get(self.index).map(String::as_str) {
            if arg == "--" {
                self.index += 1;
                self.parsed
                    .positionals
                    .extend(self.args[self.index..].iter().map(String::as_str));
                self.index = self.args.len();
                break;
            }

            if arg == "-h" || arg == "--help" {
                return Err(UsageError::usage_only(self.usage_text()));
            }

            if let Some(rest) = arg.strip_prefix("--") {
                self.parse_long(rest)?;
                self.index += 1;
                continue;
            }

            if arg.starts_with('-') && arg.len() > 1 {
                self.parse_short_bundle(&arg[1..])?;
                self.index += 1;
                continue;
            }

            if self.parse_nodash(arg)? {
                self.index += 1;
                continue;
            }

            self.parsed.positionals.push(arg);
            self.index += 1;
        }

        Ok(std::mem::replace(
            &mut self.parsed,
            Parsed {
                options: Vec::new(),
                positionals: Vec::new(),
            },
        ))
    }

    fn parse_long(&mut self, arg: &'a str) -> Result<(), UsageError> {
        let (name, attached) = arg.split_once('=').unwrap_or((arg, ""));
        let has_attached = arg.contains('=');
        let parsed = self
            .resolve_long(name)?
            .ok_or_else(|| self.error(format!("unknown option `{name}'")))?;

        let value = if has_attached { Some(attached) } else { None };
        self.apply_option(parsed.spec_index, parsed.name, value, false)
    }

    fn parse_short_bundle(&mut self, bundle: &'a str) -> Result<(), UsageError> {
        let mut rest = bundle;
        while let Some(short) = rest.chars().next() {
            let short_len = short.len_utf8();
            rest = &rest[short_len..];
            let spec_index = self
                .specs
                .iter()
                .position(|spec| {
                    spec.short == Some(short) && !spec.flags.contains(OptFlags::NODASH)
                })
                .ok_or_else(|| self.error(format!("unknown switch `{short}'")))?;
            let spec = self.specs[spec_index];
            if spec.value.expects_value() {
                let attached = (!rest.is_empty()).then_some(rest);
                self.apply_option(spec_index, OptionName::Short(short), attached, true)?;
                return Ok(());
            }
            self.apply_option(spec_index, OptionName::Short(short), None, true)?;
        }
        Ok(())
    }

    fn parse_nodash(&mut self, arg: &'a str) -> Result<bool, UsageError> {
        let mut chars = arg.chars();
        let Some(short) = chars.next() else {
            return Ok(false);
        };
        if chars.next().is_some() {
            return Ok(false);
        }
        if let Some(spec_index) = self
            .specs
            .iter()
            .position(|spec| spec.short == Some(short) && spec.flags.contains(OptFlags::NODASH))
        {
            self.apply_option(spec_index, OptionName::Short(short), None, true)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn resolve_long(&self, arg_name: &'a str) -> Result<Option<ResolvedLong<'a>>, UsageError> {
        let mut name = arg_name;
        let mut unset = false;
        let mut no_no = false;
        if let Some(rest) = name.strip_prefix("no-") {
            if let Some(rest) = rest.strip_prefix("no-") {
                name = rest;
                no_no = true;
            } else {
                name = rest;
                unset = true;
            }
        }

        for (spec_index, spec) in self.specs.iter().enumerate() {
            let Some(long) = spec.long else {
                continue;
            };
            if no_no {
                continue;
            }
            if long == name {
                if unset && (!spec.value.is_bool() || spec.flags.contains(OptFlags::NONEG)) {
                    return Ok(None);
                }
                return Ok(Some(ResolvedLong {
                    spec_index,
                    name: if unset {
                        OptionName::NegatedLong(long)
                    } else {
                        OptionName::Long(long)
                    },
                }));
            }
        }

        let mut matches = Vec::new();
        for (spec_index, spec) in self.specs.iter().enumerate() {
            let Some(long) = spec.long else {
                continue;
            };
            if no_no {
                continue;
            }
            if long.starts_with(name) {
                if unset && (!spec.value.is_bool() || spec.flags.contains(OptFlags::NONEG)) {
                    continue;
                }
                matches.push(ResolvedLong {
                    spec_index,
                    name: if unset {
                        OptionName::NegatedLong(long)
                    } else {
                        OptionName::Long(long)
                    },
                });
            }
        }

        match matches.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(*one)),
            [first, second, ..] => {
                let message = format!(
                    "ambiguous option: {arg_name} (could be {} or {})",
                    first.name.long_form(),
                    second.name.long_form()
                );
                Err(self.error(message))
            }
        }
    }

    fn apply_option(
        &mut self,
        spec_index: usize,
        name: OptionName<'a>,
        attached: Option<&'a str>,
        short: bool,
    ) -> Result<(), UsageError> {
        let spec = self.specs[spec_index];
        let unset = matches!(name, OptionName::NegatedLong(_));
        if unset && attached.is_some() {
            return Err(self.error(format!("{name} takes no value")));
        }
        if attached.is_some() && !spec.value.expects_value() {
            return Err(self.error(format!("{name} takes no value")));
        }

        let value = match spec.value {
            OptValue::Bool => ParsedValue::Bool(!unset),
            OptValue::Int(_) => {
                let raw = self.required_value(spec, name, attached, short)?;
                ParsedValue::Int(
                    parse_plain_int(raw)
                        .map_err(|_| self.error(format!("{name} expects an integer value")))?,
                )
            }
            OptValue::Magnitude(_) => {
                let raw = self.required_value(spec, name, attached, short)?;
                ParsedValue::Magnitude(parse_magnitude(raw).map_err(|err| {
                    self.error(match err {
                        NumberError::Empty => format!("{name} expects a numerical value"),
                        NumberError::Invalid => {
                            format!("{name} expects an integer value with an optional k/m/g suffix")
                        }
                    })
                })?)
            }
            OptValue::Str(_) => {
                if unset {
                    ParsedValue::Str("")
                } else {
                    let raw = self.value_for(spec, name, attached, short)?;
                    ParsedValue::Str(raw.unwrap_or(""))
                }
            }
            OptValue::Enum { parse, .. } => {
                let raw = self.required_value(spec, name, attached, short)?;
                if !parse(raw) {
                    return Err(self.error(format!("invalid value for '{}'", name.cli_spelling())));
                }
                ParsedValue::Enum(raw)
            }
            OptValue::Callback { parse, .. } => {
                let raw = if spec.value.expects_value() {
                    self.value_for(spec, name, attached, short)?
                } else {
                    attached
                };
                let callback_value = CallbackValue {
                    option: name,
                    value: raw,
                    unset,
                };
                ParsedValue::Callback(parse(callback_value).map_err(|message| self.error(message))?)
            }
        };

        self.parsed.options.push(ParsedOption {
            spec_index,
            short: spec.short,
            long: spec.long,
            name,
            value,
        });
        Ok(())
    }

    fn value_for(
        &mut self,
        spec: OptionSpec<'a>,
        name: OptionName<'a>,
        attached: Option<&'a str>,
        short: bool,
    ) -> Result<Option<&'a str>, UsageError> {
        if let Some(value) = attached {
            return Ok(Some(value));
        }
        if spec.flags.contains(OptFlags::OPTARG) {
            return Ok(None);
        }
        if short || spec.value.expects_value() {
            self.index += 1;
            return self
                .args
                .get(self.index)
                .map(String::as_str)
                .map(Some)
                .ok_or_else(|| self.error(format!("{name} requires a value")));
        }
        Ok(None)
    }

    fn required_value(
        &mut self,
        spec: OptionSpec<'a>,
        name: OptionName<'a>,
        attached: Option<&'a str>,
        short: bool,
    ) -> Result<&'a str, UsageError> {
        self.value_for(spec, name, attached, short)?
            .ok_or_else(|| self.error(format!("{name} requires a value")))
    }

    fn error(&self, message: String) -> UsageError {
        UsageError::new(message, self.usage_text())
    }

    fn usage_text(&self) -> String {
        usage_with_options(self.specs, self.usage)
    }
}

#[derive(Clone, Copy)]
struct ResolvedLong<'a> {
    spec_index: usize,
    name: OptionName<'a>,
}

impl OptionName<'_> {
    fn long_form(self) -> String {
        match self {
            Self::Short(short) => format!("-{short}"),
            Self::Long(long) => format!("--{long}"),
            Self::NegatedLong(long) => format!("--no-{long}"),
        }
    }

    fn cli_spelling(self) -> String {
        match self {
            Self::Short(short) => format!("-{short}"),
            Self::Long(long) => format!("--{long}"),
            Self::NegatedLong(long) => format!("--no-{long}"),
        }
    }
}

impl<'a> OptValue<'a> {
    const fn is_bool(self) -> bool {
        matches!(self, Self::Bool)
    }

    const fn expects_value(self) -> bool {
        !matches!(self, Self::Bool)
    }

    fn usage_suffix(self, flags: OptFlags, has_long: bool) -> String {
        let Some(metavar) = self.metavar() else {
            return String::new();
        };
        if flags.contains(OptFlags::OPTARG) {
            if has_long {
                format!("[=<{metavar}>]")
            } else {
                format!("[<{metavar}>]")
            }
        } else {
            format!(" <{metavar}>")
        }
    }

    fn metavar(self) -> Option<&'a str> {
        match self {
            Self::Bool => None,
            Self::Int(metavar) | Self::Magnitude(metavar) | Self::Str(metavar) => Some(metavar),
            Self::Enum { metavar, .. } => Some(metavar),
            Self::Callback { metavar, .. } => metavar,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberError {
    Empty,
    Invalid,
}

fn parse_plain_int(raw: &str) -> Result<i64, NumberError> {
    if raw.is_empty() {
        return Err(NumberError::Empty);
    }
    raw.parse::<i64>().map_err(|_| NumberError::Invalid)
}

fn parse_magnitude(raw: &str) -> Result<i64, NumberError> {
    if raw.is_empty() {
        return Err(NumberError::Empty);
    }
    let (digits, multiplier) = match raw.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&raw[..raw.len() - 1], 1024_i64),
        Some(b'm' | b'M') => (&raw[..raw.len() - 1], 1024_i64 * 1024),
        Some(b'g' | b'G') => (&raw[..raw.len() - 1], 1024_i64 * 1024 * 1024),
        _ => (raw, 1),
    };
    if digits.is_empty() {
        return Err(NumberError::Invalid);
    }
    let value = digits.parse::<i64>().map_err(|_| NumberError::Invalid)?;
    value.checked_mul(multiplier).ok_or(NumberError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn parse<'a>(
        args: &'a [String],
        specs: &'a [OptionSpec<'a>],
    ) -> Result<Parsed<'a>, UsageError> {
        parse_options(args, specs, &["git test [options] [--] <args>"])
    }

    #[test]
    fn unique_long_abbreviation_resolves_to_full_option() {
        let specs = [OptionSpec {
            short: None,
            long: Some("format"),
            value: OptValue::Str("format"),
            flags: OptFlags::NONE,
            help: "format output",
        }];
        let argv = args(&["--form=%H", "HEAD"]);
        let parsed = parse(&argv, &specs).expect("parse");
        assert_eq!(parsed.last_str("format"), Some("%H"));
        assert_eq!(parsed.positionals, ["HEAD"]);
    }

    #[test]
    fn ambiguous_long_abbreviation_lists_candidates_and_exits_129() {
        let specs = [
            OptionSpec {
                short: None,
                long: Some("format"),
                value: OptValue::Str("format"),
                flags: OptFlags::NONE,
                help: "format output",
            },
            OptionSpec {
                short: None,
                long: Some("follow"),
                value: OptValue::Bool,
                flags: OptFlags::NONE,
                help: "follow history",
            },
        ];
        let argv = args(&["--fo"]);
        let err = parse(&argv, &specs).expect_err("ambiguous");
        assert_eq!(err.exit_code(), 129);
        assert_eq!(
            err.message(),
            Some("ambiguous option: fo (could be --format or --follow)")
        );
    }

    #[test]
    fn bool_long_negation_is_recorded() {
        let specs = [OptionSpec {
            short: None,
            long: Some("quiet"),
            value: OptValue::Bool,
            flags: OptFlags::NONE,
            help: "be quiet",
        }];
        let argv = args(&["--quiet", "--no-quiet"]);
        let parsed = parse(&argv, &specs).expect("parse");
        assert!(!parsed.last_bool("quiet", true));
    }

    #[test]
    fn short_bundling_handles_flags_and_attached_values() {
        let specs = [
            OptionSpec {
                short: Some('a'),
                long: Some("all"),
                value: OptValue::Bool,
                flags: OptFlags::NONE,
                help: "all",
            },
            OptionSpec {
                short: Some('b'),
                long: Some("brief"),
                value: OptValue::Bool,
                flags: OptFlags::NONE,
                help: "brief",
            },
            OptionSpec {
                short: Some('m'),
                long: None,
                value: OptValue::Str("msg"),
                flags: OptFlags::NONE,
                help: "message",
            },
        ];
        let argv = args(&["-abmhello"]);
        let parsed = parse(&argv, &specs).expect("parse");
        assert!(parsed.last_bool("all", false));
        assert!(parsed.last_bool("brief", false));
        assert_eq!(
            parsed.options.last().map(|option| &option.value),
            Some(&ParsedValue::Str("hello"))
        );
    }

    #[test]
    fn optional_value_accepts_attached_but_does_not_consume_next_arg() {
        let specs = [OptionSpec {
            short: Some('o'),
            long: Some("output"),
            value: OptValue::Str("path"),
            flags: OptFlags::OPTARG,
            help: "optional output",
        }];
        let argv = args(&["--output", "file"]);
        let parsed = parse(&argv, &specs).expect("parse");
        assert_eq!(parsed.last_str("output"), Some(""));
        assert_eq!(parsed.positionals, ["file"]);

        let argv = args(&["--output="]);
        let parsed = parse(&argv, &specs).expect("parse");
        assert_eq!(parsed.last_str("output"), Some(""));

        let argv = args(&["-ofile"]);
        let parsed = parse(&argv, &specs).expect("parse");
        assert_eq!(parsed.last_str("output"), Some("file"));
    }

    #[test]
    fn equals_value_is_preserved_for_required_string() {
        let specs = [OptionSpec {
            short: None,
            long: Some("message"),
            value: OptValue::Str("msg"),
            flags: OptFlags::NONE,
            help: "message",
        }];
        let argv = args(&["--message="]);
        let parsed = parse(&argv, &specs).expect("parse");
        assert_eq!(parsed.last_str("message"), Some(""));
    }

    #[test]
    fn bool_rejects_long_equals_value() {
        let specs = [OptionSpec {
            short: None,
            long: Some("quiet"),
            value: OptValue::Bool,
            flags: OptFlags::NONE,
            help: "be quiet",
        }];
        let argv = args(&["--quiet=yes"]);
        let err = parse(&argv, &specs).expect_err("takes no value");
        assert_eq!(err.message(), Some("option `quiet' takes no value"));
        assert_eq!(err.exit_code(), 129);
    }

    #[test]
    fn unknown_option_is_usage_error_with_exit_129() {
        let specs = [OptionSpec {
            short: Some('q'),
            long: Some("quiet"),
            value: OptValue::Bool,
            flags: OptFlags::NONE,
            help: "be quiet",
        }];
        let argv = args(&["--bogus"]);
        let err = parse(&argv, &specs).expect_err("unknown");
        assert_eq!(err.message(), Some("unknown option `bogus'"));
        assert_eq!(err.exit_code(), 129);

        let argv = args(&["-x"]);
        let err = parse(&argv, &specs).expect_err("unknown");
        assert_eq!(err.message(), Some("unknown switch `x'"));
        assert_eq!(err.exit_code(), 129);
    }

    #[test]
    fn magnitude_uses_git_integer_suffix_diagnostic() {
        let specs = [OptionSpec {
            short: None,
            long: Some("unified"),
            value: OptValue::Magnitude("n"),
            flags: OptFlags::NONE,
            help: "context lines",
        }];
        let argv = args(&["--unified=2k"]);
        let parsed = parse(&argv, &specs).expect("parse");
        assert_eq!(
            parsed.options.first().map(|option| &option.value),
            Some(&ParsedValue::Magnitude(2048))
        );

        let argv = args(&["--unified=bad"]);
        let err = parse(&argv, &specs).expect_err("invalid magnitude");
        assert_eq!(
            err.message(),
            Some("option `unified' expects an integer value with an optional k/m/g suffix")
        );
    }

    #[test]
    fn enum_invalid_value_uses_option_name() {
        fn parse_mode(value: &str) -> bool {
            matches!(value, "one" | "two")
        }

        let specs = [OptionSpec {
            short: None,
            long: Some("mode"),
            value: OptValue::Enum {
                metavar: "mode",
                parse: parse_mode,
            },
            flags: OptFlags::NONE,
            help: "mode",
        }];
        let argv = args(&["--mode=three"]);
        let err = parse(&argv, &specs).expect_err("invalid enum");
        assert_eq!(err.message(), Some("invalid value for '--mode'"));
    }
}
