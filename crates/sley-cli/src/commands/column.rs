//! Thin CLI wrapper for the embeddable column formatter.

use crate::*;
use std::io::{IsTerminal, Read, Write};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnEnable {
    Never,
    Always,
    Auto,
}

pub(crate) fn cmd_column(args: &[String]) -> Result<()> {
    let mut enable = ColumnEnable::Never;
    let mut options = sley::plumbing::sley_pretty::ColumnOptions {
        width: env::var("COLUMNS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(80usize)
            .saturating_sub(1),
        ..Default::default()
    };

    let mut arguments = args.iter();
    while let Some(arg) = arguments.next() {
        if arg == "--mode" || arg.starts_with("--mode=") {
            let mode = option_value(arg, "--mode", &mut arguments)?;
            let mut set_layout = false;
            let mut set_enable = false;
            for value in mode.split([',', ' ']).filter(|value| !value.is_empty()) {
                match value {
                    "never" => {
                        enable = ColumnEnable::Never;
                        set_enable = true;
                    }
                    "always" => {
                        enable = ColumnEnable::Always;
                        set_enable = true;
                    }
                    "auto" => {
                        enable = ColumnEnable::Auto;
                        set_enable = true;
                    }
                    "plain" => {
                        options.layout = sley::plumbing::sley_pretty::ColumnLayout::Plain;
                        set_layout = true;
                    }
                    "column" => {
                        options.layout = sley::plumbing::sley_pretty::ColumnLayout::ColumnFirst;
                        set_layout = true;
                    }
                    "row" => {
                        options.layout = sley::plumbing::sley_pretty::ColumnLayout::RowFirst;
                        set_layout = true;
                    }
                    "dense" => options.dense = true,
                    "nodense" => options.dense = false,
                    _ => return column_usage_error(&format!("invalid mode '{value}'")),
                }
            }
            if set_layout && !set_enable {
                enable = ColumnEnable::Always;
            }
        } else if arg == "--width" || arg.starts_with("--width=") {
            let value = option_value(arg, "--width", &mut arguments)?;
            options.width = value
                .parse()
                .map_err(|_| GitError::Command("column width must be a number".into()))?;
        } else if arg == "--padding" || arg.starts_with("--padding=") {
            let value = option_value(arg, "--padding", &mut arguments)?;
            let padding = value
                .parse::<i64>()
                .map_err(|_| GitError::Command("column padding must be a number".into()))?;
            if padding < 0 {
                eprintln!("fatal: --padding must be non-negative");
                return Err(GitError::Exit(128));
            }
            options.padding = usize::try_from(padding).unwrap_or(usize::MAX);
        } else if arg == "--indent" || arg.starts_with("--indent=") {
            let value = option_value(arg, "--indent", &mut arguments)?;
            options.indent = value.as_bytes().to_vec();
        } else if arg == "--nl" || arg.starts_with("--nl=") {
            let value = option_value(arg, "--nl", &mut arguments)?;
            options.line_terminator = value.as_bytes().to_vec();
        } else if matches!(arg.as_str(), "-h" | "--help-all") {
            crate::commands::help::print_command_usage("column");
            return Err(GitError::Exit(129));
        } else {
            return column_usage_error(&format!("unknown option '{arg}'"));
        }
    }

    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let items = if input.is_empty() {
        Vec::new()
    } else {
        let mut lines = input.split(|byte| *byte == b'\n').collect::<Vec<_>>();
        if input.ends_with(b"\n") {
            lines.pop();
        }
        lines
            .into_iter()
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line).to_vec())
            .collect::<Vec<_>>()
    };
    let enabled = match enable {
        ColumnEnable::Never => false,
        ColumnEnable::Always => true,
        ColumnEnable::Auto => io::stdout().is_terminal(),
    };
    if !enabled {
        options.layout = sley::plumbing::sley_pretty::ColumnLayout::Plain;
        options.indent.clear();
        options.line_terminator = vec![b'\n'];
    }
    let output = sley::plumbing::sley_pretty::format_columns(&items, &options);
    io::stdout().write_all(&output)?;
    Ok(())
}

fn option_value<'a, I>(arg: &'a str, name: &str, arguments: &mut I) -> Result<&'a str>
where
    I: Iterator<Item = &'a String>,
{
    if let Some(value) = arg
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('='))
    {
        return Ok(value);
    }
    arguments
        .next()
        .map(String::as_str)
        .ok_or_else(|| GitError::Command(format!("option '{name}' requires a value")))
}

fn column_usage_error<T>(message: &str) -> Result<T> {
    eprintln!("error: {message}");
    crate::commands::help::print_command_usage("column");
    Err(GitError::Exit(129))
}
