//! Small Git-shaped argument parsing helpers.
//!
//! Git's command line is not regular enough for a generic CLI derive layer:
//! commands disagree about option grouping, `--flag=value`, where parsing stops,
//! and the exact diagnostics/exit codes. These helpers keep the mechanical bits
//! shared while leaving command-specific compatibility decisions close to each
//! command parser.

use sley_core::{GitError, Result};

#[derive(Debug, Clone, Copy)]
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

    pub(crate) fn value_for(self, name: &str) -> Option<&'a str> {
        (self.name == name).then_some(self.value).flatten()
    }

    pub(crate) fn has_value(self) -> bool {
        self.value.is_some()
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
        assert!(option.has_value());

        let empty = LongOption::parse("--batch=").expect("long option");
        assert_eq!(empty.name(), "batch");
        assert_eq!(empty.value(), Some(""));

        let flag = LongOption::parse("--stdin").expect("long option");
        assert_eq!(flag.name(), "stdin");
        assert_eq!(flag.value(), None);
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
}
