//! Small Git-shaped argument parsing helpers.
//!
//! Git's command line is not regular enough for a generic CLI derive layer:
//! commands disagree about option grouping, `--flag=value`, where parsing stops,
//! and the exact diagnostics/exit codes. These helpers keep the mechanical bits
//! shared while leaving command-specific compatibility decisions close to each
//! command parser.

use sley_core::{GitError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LongOption<'a> {
    name: &'a str,
    value: Option<&'a str>,
}

impl<'a> LongOption<'a> {
    pub(crate) fn parse(arg: &'a str) -> Option<Self> {
        let rest = arg.strip_prefix("--")?;
        if rest.is_empty() {
            return None;
        }
        let (name, value) = rest
            .split_once('=')
            .map(|(name, value)| (name, Some(value)))
            .unwrap_or((rest, None));
        Some(Self { name, value })
    }

    pub(crate) fn name(self) -> &'a str {
        self.name
    }

    pub(crate) fn value(self) -> Option<&'a str> {
        self.value
    }

    pub(crate) fn optional_value(self) -> OptionalValue<'a> {
        self.value
            .map(FlagValue::attached)
            .map(OptionalValue::Present)
            .unwrap_or(OptionalValue::Absent)
    }

    pub(crate) fn is(self, name: &str) -> bool {
        self.name == name
    }

    pub(crate) fn value_for(self, name: &str) -> Option<&'a str> {
        (self.name == name).then_some(self.value).flatten()
    }

    pub(crate) fn has_value(self) -> bool {
        self.value.is_some()
    }

    pub(crate) fn negated(self) -> Option<NegatedFlag<'a>> {
        NegatedFlag::parse(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlagValue<'a> {
    Attached(&'a str),
    Separate(&'a str),
}

impl<'a> FlagValue<'a> {
    pub(crate) fn attached(value: &'a str) -> Self {
        Self::Attached(value)
    }

    pub(crate) fn separate(value: &'a str) -> Self {
        Self::Separate(value)
    }

    pub(crate) fn value(self) -> &'a str {
        match self {
            Self::Attached(value) | Self::Separate(value) => value,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptionalValue<'a> {
    Absent,
    Present(FlagValue<'a>),
}

impl<'a> OptionalValue<'a> {
    pub(crate) fn value(self) -> Option<&'a str> {
        match self {
            Self::Absent => None,
            Self::Present(value) => Some(value.value()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NegatedFlag<'a> {
    option: LongOption<'a>,
    name: &'a str,
}

impl<'a> NegatedFlag<'a> {
    pub(crate) fn parse(option: LongOption<'a>) -> Option<Self> {
        let name = option.name().strip_prefix("no-")?;
        if name.is_empty() {
            return None;
        }
        Some(Self { option, name })
    }

    pub(crate) fn name(self) -> &'a str {
        self.name
    }

    pub(crate) fn option_name(self) -> &'a str {
        self.option.name()
    }

    pub(crate) fn has_value(self) -> bool {
        self.option.has_value()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Terminator;

impl Terminator {
    pub(crate) fn parse(arg: &str) -> Option<Self> {
        (arg == "--").then_some(Self)
    }

    pub(crate) fn is(arg: &str) -> bool {
        Self::parse(arg).is_some()
    }
}

pub(crate) struct GitArgCursor<'a> {
    args: &'a [String],
    position: usize,
}

impl<'a> GitArgCursor<'a> {
    pub(crate) fn new(args: &'a [String]) -> Self {
        Self { args, position: 0 }
    }

    pub(crate) fn next(&mut self) -> Option<&'a str> {
        let value = self.args.get(self.position)?;
        self.position += 1;
        Some(value)
    }

    pub(crate) fn next_value(&mut self) -> Option<&'a str> {
        self.next()
    }

    pub(crate) fn next_required_value(
        &mut self,
        missing: impl FnOnce() -> GitError,
    ) -> Result<&'a str> {
        self.next().ok_or_else(missing)
    }

    pub(crate) fn resolve_value(
        &mut self,
        option: LongOption<'a>,
        missing: impl FnOnce() -> GitError,
    ) -> Result<FlagValue<'a>> {
        match option.value() {
            Some(value) => Ok(FlagValue::attached(value)),
            None => self.next_required_value(missing).map(FlagValue::separate),
        }
    }

    pub(crate) fn resolve_value_for(
        &mut self,
        option: LongOption<'a>,
        name: &str,
        missing: impl FnOnce() -> GitError,
    ) -> Result<Option<FlagValue<'a>>> {
        if option.is(name) {
            self.resolve_value(option, missing).map(Some)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn rest(&self) -> &'a [String] {
        &self.args[self.position..]
    }
}

pub(crate) fn long_option_value<'a>(arg: &'a str, option: &str) -> Option<&'a str> {
    LongOption::parse(arg)?.value_for(option)
}

pub(crate) fn option_takes_no_value<T>(option: &str) -> Result<T> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

pub(crate) fn switch_requires_value(switch: &str) -> GitError {
    eprintln!("error: switch `{switch}' requires a value");
    GitError::Exit(129)
}

pub(crate) fn usage_error<T>(message: &str) -> Result<T> {
    eprintln!("error: {message}");
    Err(GitError::Exit(129))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_option_parses_name_and_value() {
        let option = LongOption::parse("--path=src/lib.rs").expect("long option");
        assert_eq!(option.name(), "path");
        assert_eq!(option.value(), Some("src/lib.rs"));
        assert_eq!(option.optional_value().value(), Some("src/lib.rs"));
        assert!(option.has_value());

        let empty = LongOption::parse("--batch=").expect("long option");
        assert_eq!(empty.name(), "batch");
        assert_eq!(empty.value(), Some(""));
        assert_eq!(
            empty.optional_value(),
            OptionalValue::Present(FlagValue::Attached(""))
        );

        let flag = LongOption::parse("--stdin").expect("long option");
        assert_eq!(flag.name(), "stdin");
        assert_eq!(flag.value(), None);
        assert_eq!(flag.optional_value(), OptionalValue::Absent);
        assert!(!flag.has_value());
    }

    #[test]
    fn long_option_value_matches_exact_name_only() {
        assert_eq!(long_option_value("--sort=refname", "sort"), Some("refname"));
        assert_eq!(long_option_value("--no-sort", "sort"), None);
        assert_eq!(long_option_value("--sort", "sort"), None);
        assert_eq!(long_option_value("-srefname", "sort"), None);
    }

    #[test]
    fn flag_values_report_value_and_source() {
        let attached = FlagValue::attached("inline");
        assert_eq!(attached.value(), "inline");
        assert!(matches!(attached, FlagValue::Attached("inline")));

        let separate = FlagValue::separate("next");
        assert_eq!(separate.value(), "next");
        assert!(matches!(separate, FlagValue::Separate("next")));

        let optional = OptionalValue::Present(separate);
        assert_eq!(optional.value(), Some("next"));
        assert_eq!(OptionalValue::Absent.value(), None);
    }

    #[test]
    fn negated_flag_parses_no_prefix_without_hiding_original_option() {
        let option = LongOption::parse("--no-prune").expect("long option");
        let flag = option.negated().expect("negated flag");
        assert_eq!(flag.name(), "prune");
        assert_eq!(flag.option_name(), "no-prune");
        assert!(!flag.has_value());

        let valued = LongOption::parse("--no-prune=false")
            .expect("long option")
            .negated()
            .expect("negated flag");
        assert_eq!(valued.name(), "prune");
        assert!(valued.has_value());

        assert!(
            LongOption::parse("--prune")
                .expect("long option")
                .negated()
                .is_none()
        );
        assert!(
            LongOption::parse("--no-")
                .expect("long option")
                .negated()
                .is_none()
        );
    }

    #[test]
    fn terminator_matches_double_dash_only() {
        assert_eq!(Terminator::parse("--"), Some(Terminator));
        assert!(Terminator::is("--"));
        assert!(!Terminator::is("--path"));
        assert!(LongOption::parse("--").is_none());
    }

    #[test]
    fn cursor_consumes_values_and_reports_missing_values() {
        let args = vec!["--path".to_string(), "file".to_string()];
        let mut cursor = GitArgCursor::new(&args);
        assert_eq!(cursor.next(), Some("--path"));
        assert_eq!(
            cursor
                .next_required_value(|| GitError::Command("missing".into()))
                .expect("value"),
            "file"
        );
        assert_eq!(cursor.next(), None);

        let mut empty = GitArgCursor::new(&[]);
        let err = empty
            .next_required_value(|| GitError::Command("missing".into()))
            .expect_err("missing value");
        assert!(matches!(err, GitError::Command(message) if message == "missing"));
    }

    #[test]
    fn cursor_resolves_attached_or_separate_long_option_values() {
        let args = vec!["--path=inline".to_string(), "tail".to_string()];
        let mut cursor = GitArgCursor::new(&args);
        let option = LongOption::parse(cursor.next().expect("arg")).expect("long option");
        let value = cursor
            .resolve_value(option, || GitError::Command("missing".into()))
            .expect("value");
        assert_eq!(value, FlagValue::Attached("inline"));
        assert_eq!(value.value(), "inline");
        assert_eq!(cursor.next(), Some("tail"));

        let args = vec![
            "--path".to_string(),
            "separate".to_string(),
            "tail".to_string(),
        ];
        let mut cursor = GitArgCursor::new(&args);
        let option = LongOption::parse(cursor.next().expect("arg")).expect("long option");
        let value = cursor
            .resolve_value(option, || GitError::Command("missing".into()))
            .expect("value");
        assert_eq!(value, FlagValue::Separate("separate"));
        assert_eq!(value.value(), "separate");
        assert_eq!(cursor.next(), Some("tail"));

        let args = vec!["next".to_string()];
        let mut cursor = GitArgCursor::new(&args);
        let option = LongOption::parse("--sort=refname").expect("long option");
        let value = cursor
            .resolve_value_for(option, "path", || GitError::Command("missing".into()))
            .expect("non-matching option");
        assert_eq!(value, None);
        assert_eq!(cursor.next(), Some("next"));
    }

    #[test]
    fn cursor_reports_missing_resolved_long_option_value() {
        let args = vec!["--path".to_string()];
        let mut cursor = GitArgCursor::new(&args);
        let option = LongOption::parse(cursor.next().expect("arg")).expect("long option");
        let err = cursor
            .resolve_value(option, || GitError::Command("missing".into()))
            .expect_err("missing value");
        assert!(matches!(err, GitError::Command(message) if message == "missing"));
    }
}
