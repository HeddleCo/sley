use super::replace::{ReplaceListFormat, ReplaceMode, ReplaceOptions};
use crate::*;

pub(super) fn setup_replace_options(args: &[String]) -> Result<ReplaceOptions> {
    let mut force = false;
    let mut format = ReplaceListFormat::Short;
    let mut list = false;
    let mut delete = false;
    let mut edit = false;
    let mut graft = false;
    let mut convert_graft_file = false;
    let mut raw = false;
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
            "-e" | "--edit" => edit = true,
            "-g" | "--graft" => graft = true,
            "--convert-graft-file" => convert_graft_file = true,
            "--raw" => raw = true,
            "--no-raw" => raw = false,
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
                        'e' => edit = true,
                        'g' => graft = true,
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
    let mode_count = usize::from(list)
        + usize::from(delete)
        + usize::from(edit)
        + usize::from(graft)
        + usize::from(convert_graft_file);
    if mode_count > 1 {
        return replace_usage();
    }
    if convert_graft_file {
        if !positional.is_empty() {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            raw,
            mode: ReplaceMode::ConvertGraftFile,
        });
    }
    if edit {
        if positional.len() != 1 {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            raw,
            mode: ReplaceMode::Edit {
                object: positional.remove(0),
            },
        });
    }
    if graft {
        if positional.is_empty() {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            raw,
            mode: ReplaceMode::Graft {
                object: positional.remove(0),
                parents: positional,
            },
        });
    }
    if delete {
        if positional.is_empty() {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            raw,
            mode: ReplaceMode::Delete {
                objects: positional,
            },
        });
    }
    if force && positional.is_empty() {
        return replace_usage();
    }
    if list || positional.len() <= 1 {
        if positional.len() > 1 {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            raw,
            mode: ReplaceMode::List {
                pattern: positional.pop(),
            },
        });
    }
    if positional.len() == 2 {
        return Ok(ReplaceOptions {
            force,
            format,
            raw,
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
