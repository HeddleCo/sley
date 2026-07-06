use super::{
    ReplaceListFormat, ReplaceMode, ReplaceOptions, RerereOptions, RerereSubcommand,
};
use crate::commands::cli_options::opt_bool;
use crate::*;
use sley_options::{parse_options, OptionSpec, ParsedValue};

const RERERE_USAGE: &[&str] =
    &["git rerere [clear | forget <pathspec>... | diff | status | remaining | gc]"];

fn rerere_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[opt_bool(
        None,
        Some("rerere-autoupdate"),
        sley_options::OptFlags::NONE,
        "register clean resolutions in index",
    )];
    SPECS
}

pub(super) fn setup_replace_options(args: &[String]) -> Result<ReplaceOptions> {
    let mut force = false;
    let mut format = ReplaceListFormat::Short;
    let mut list = false;
    let mut delete = false;
    let mut unsupported_mode = None::<&str>;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                positional.extend(iter.cloned());
                break;
            }
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-l" | "--list" => list = true,
            "-d" | "--delete" => delete = true,
            "-e" | "--edit" => unsupported_mode = Some("--edit"),
            "-g" | "--graft" => unsupported_mode = Some("--graft"),
            "--convert-graft-file" => unsupported_mode = Some("--convert-graft-file"),
            "--raw" | "--no-raw" => {}
            "--format" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `format' requires a value");
                    return Err(GitError::Exit(129));
                };
                format = parse_replace_list_format(value)?;
            }
            "--no-format" => format = ReplaceListFormat::Short,
            value if let Some(value) = long_option_value(value, "format") => {
                format = parse_replace_list_format(value)?;
            }
            value if value.starts_with("--no-force=") => {
                eprintln!("error: option `no-force' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return replace_usage();
            }
            value if value.starts_with('-') && value.len() > 1 => {
                for option in value[1..].chars() {
                    match option {
                        'f' => force = true,
                        'l' => list = true,
                        'd' => delete = true,
                        'e' => unsupported_mode = Some("--edit"),
                        'g' => unsupported_mode = Some("--graft"),
                        other => {
                            eprintln!("error: unknown switch `{other}'");
                            return replace_usage();
                        }
                    }
                }
            }
            value => positional.push(value.to_string()),
        }
    }
    if let Some(mode) = unsupported_mode {
        return Err(GitError::Unsupported(format!("replace {mode}")));
    }
    if delete {
        if positional.is_empty() {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            mode: ReplaceMode::Delete {
                objects: positional,
            },
        });
    }
    if list || positional.len() <= 1 {
        if positional.len() > 1 {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            mode: ReplaceMode::List {
                pattern: positional.pop(),
            },
        });
    }
    if positional.len() == 2 {
        return Ok(ReplaceOptions {
            force,
            format,
            mode: ReplaceMode::Create {
                object: positional.remove(0),
                replacement: positional.remove(0),
            },
        });
    }
    replace_usage()
}

fn parse_replace_list_format(value: &str) -> Result<ReplaceListFormat> {
    match value {
        "short" => Ok(ReplaceListFormat::Short),
        "medium" => Ok(ReplaceListFormat::Medium),
        "long" => Ok(ReplaceListFormat::Long),
        other => {
            eprintln!("error: invalid replace format '{other}'");
            eprintln!("valid formats are 'short', 'medium' and 'long'");
            Err(GitError::Exit(255))
        }
    }
}

fn replace_usage<T>() -> Result<T> {
    eprintln!("usage: git replace [-f] <object> <replacement>");
    eprintln!("   or: git replace [-f] --edit <object>");
    eprintln!("   or: git replace [-f] --graft <commit> [<parent>...]");
    eprintln!("   or: git replace [-f] --convert-graft-file");
    eprintln!("   or: git replace -d <object>...");
    eprintln!("   or: git replace [--format=<format>] [-l [<pattern>]]");
    eprintln!();
    eprintln!("    -l, --list            list replace refs");
    eprintln!("    -d, --delete          delete replace refs");
    eprintln!("    -e, --edit            edit existing object");
    eprintln!("    -g, --graft           change a commit's parents");
    eprintln!("    --convert-graft-file  convert existing graft file");
    eprintln!("    -f, --[no-]force      replace the ref if it exists");
    eprintln!("    --[no-]raw            do not pretty-print contents for --edit");
    eprintln!("    --[no-]format <format>");
    eprintln!("                          use this format");
    eprintln!();
    Err(GitError::Exit(129))
}

pub(super) fn setup_rerere_options(args: &[String]) -> Result<RerereOptions> {
    let parsed = match parse_options(args, rerere_option_specs(), RERERE_USAGE) {
        Ok(parsed) => parsed,
        Err(_) => return rerere_usage(),
    };
    let mut autoupdate = None;
    for option in &parsed.options {
        if option.long == Some("rerere-autoupdate") {
            if let ParsedValue::Bool(value) = option.value {
                autoupdate = Some(value);
            }
        }
    }
    let mut subcommand = None;
    let mut paths = Vec::new();
    for arg in &parsed.positionals {
        match *arg {
            "clear" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Clear),
            "forget" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Forget),
            "gc" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Gc),
            "status" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Status),
            _ if subcommand.is_none() => return rerere_usage(),
            value => paths.push(value.to_string()),
        }
    }
    if matches!(subcommand, Some(RerereSubcommand::Forget)) && paths.is_empty() {
        eprintln!("warning: 'git rerere forget' without paths is deprecated");
    }
    let _ = autoupdate;
    Ok(RerereOptions { subcommand, paths })
}

fn rerere_usage<T>() -> Result<T> {
    eprintln!("usage: git rerere [clear | forget <pathspec>... | diff | status | remaining | gc]");
    eprintln!();
    eprintln!("    --[no-]rerere-autoupdate");
    eprintln!("                          register clean resolutions in index");
    eprintln!();
    Err(GitError::Exit(129))
}