use super::{
    SubmoduleAddOptions, SubmoduleDeinitOptions, SubmoduleForeachOptions, SubmoduleSetBranchAction,
    SubmoduleStatusOptions, SubmoduleSummaryOptions, SubmoduleUpdateOptions,
    parse_submodule_summary_limit, submodule_set_branch_usage, submodule_set_url_usage,
    submodule_usage,
};
use crate::commands::cli_options::{opt_bool, opt_str};
use crate::*;
use sley_options::{parse_options, OptFlags, OptValue, OptionSpec, Parsed, ParsedValue};
use sley_submodule::UpdateType;

fn submodule_parse_args<'a>(
    args: &'a [String],
    specs: &'static [OptionSpec<'static>],
    usage: &'static [&'static str],
) -> Result<Parsed<'a>> {
    match parse_options(args, specs, usage) {
        Ok(parsed) => Ok(parsed),
        Err(_) => submodule_usage(),
    }
}

const SUBMODULE_STATUS_USAGE: &[&str] = &["git submodule status [--cached] [--recursive] [--] [<path>...]"];
const SUBMODULE_ADD_USAGE: &[&str] =
    &["git submodule add [-b <branch>] [-f|--force] [--name <name>] [--reference <repository>] [--] <repository> [<path>]"];
const SUBMODULE_UPDATE_USAGE: &[&str] =
    &["git submodule update [--init] [--remote] [--] [<path>...]"];
const SUBMODULE_INIT_USAGE: &[&str] = &["git submodule init [--] [<path>...]"];
const SUBMODULE_DEINIT_USAGE: &[&str] = &["git submodule deinit [-f|--force] (--all| [--] <path>...)"];
const SUBMODULE_SYNC_USAGE: &[&str] = &["git submodule sync [--recursive] [--] [<path>...]"];
const SUBMODULE_ABSORBGITDIRS_USAGE: &[&str] = &["git submodule absorbgitdirs [--] [<path>...]"];
const SUBMODULE_FOREACH_USAGE: &[&str] = &["git submodule foreach [--recursive] <command>"];
const SUBMODULE_SUMMARY_USAGE: &[&str] =
    &["git submodule summary [--cached|--files] [--summary-limit <n>] [commit] [--] [<path>...]"];
const SUBMODULE_SET_URL_USAGE: &[&str] = &["git submodule set-url [--] <path> <newurl>"];
const SUBMODULE_SET_BRANCH_USAGE: &[&str] =
    &["git submodule set-branch (-d|--default) <path>", "git submodule set-branch (-b|--branch) <branch> <path>"];

const SUBMODULE_QUIET_SPEC: OptionSpec<'static> = OptionSpec {
    short: Some('q'),
    long: Some("quiet"),
    value: OptValue::Bool,
    flags: OptFlags::NONE,
    help: "suppress output",
};

fn parse_submodule_depth(value: &str) -> Result<u32> {
    value.parse::<u32>().map_err(|_| {
        eprintln!("fatal: invalid depth '{value}'");
        GitError::Exit(128)
    })
}

pub(super) fn setup_submodule_status_options(
    args: &[String],
) -> Result<SubmoduleStatusOptions<'_>> {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(None, Some("cached"), OptFlags::NONE, "show cached values"),
        opt_bool(Some('q'), Some("quiet"), OptFlags::NONE, "suppress output"),
        opt_bool(None, Some("recursive"), OptFlags::NONEG, "traverse submodules recursively"),
    ];
    let parsed = submodule_parse_args(args, SPECS, SUBMODULE_STATUS_USAGE)?;
    Ok(SubmoduleStatusOptions {
        cached: parsed.last_bool("cached", false),
        quiet: parsed.last_bool("quiet", false),
        recursive: parsed.last_bool("recursive", false),
        paths: parsed.positionals,
    })
}

pub(super) fn setup_submodule_add_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleAddOptions> {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(Some('q'), Some("quiet"), OptFlags::NONE, "suppress output"),
        opt_bool(Some('f'), Some("force"), OptFlags::NONE, "allow adding otherwise ignored paths"),
        opt_bool(None, Some("progress"), OptFlags::NONE, "show progress"),
        opt_str(None, Some("depth"), "n", OptFlags::NONE, "clone depth"),
        opt_str(None, Some("name"), "name", OptFlags::NONE, "submodule name"),
        opt_str(None, Some("reference"), "repo", OptFlags::NONE, "reference repository"),
        opt_str(
            None,
            Some("reference-if-able"),
            "repo",
            OptFlags::NONE,
            "reference repository if able",
        ),
        opt_bool(None, Some("dissociate"), OptFlags::NONE, "dissociate from reference"),
        OptionSpec {
            short: Some('b'),
            long: Some("branch"),
            value: OptValue::Str("branch"),
            flags: OptFlags::NONE,
            help: "branch to track",
        },
    ];
    let parsed = submodule_parse_args(args, SPECS, SUBMODULE_ADD_USAGE)?;
    quiet |= parsed.last_bool("quiet", false);
    let mut reference_args = Vec::new();
    for option in &parsed.options {
        match option.long {
            Some("reference") | Some("reference-if-able") => {
                if let ParsedValue::Str(value) = option.value {
                    reference_args.push(format!("--{}", option.long.expect("checked")));
                    reference_args.push(value.to_string());
                }
            }
            Some("dissociate") => {
                if matches!(option.value, ParsedValue::Bool(true)) {
                    reference_args.push("--dissociate".to_string());
                }
            }
            _ => {}
        }
    }
    let depth = parsed
        .last_str("depth")
        .map(parse_submodule_depth)
        .transpose()?;
    let values = parsed
        .positionals
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    match values.as_slice() {
        [repository] => Ok(SubmoduleAddOptions {
            repository: repository.clone(),
            path: None,
            branch: parsed.last_str("branch").map(str::to_string),
            name: parsed.last_str("name").map(str::to_string),
            force: parsed.last_bool("force", false),
            quiet,
            progress: parsed.last_bool("progress", false),
            depth,
            reference_args,
        }),
        [repository, path] => Ok(SubmoduleAddOptions {
            repository: repository.clone(),
            path: Some(path.clone()),
            branch: parsed.last_str("branch").map(str::to_string),
            name: parsed.last_str("name").map(str::to_string),
            force: parsed.last_bool("force", false),
            quiet,
            progress: parsed.last_bool("progress", false),
            depth,
            reference_args,
        }),
        _ => submodule_usage(),
    }
}

pub(super) fn setup_submodule_update_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleUpdateOptions<'_>> {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(Some('q'), Some("quiet"), OptFlags::NONE, "suppress output"),
        opt_bool(None, Some("init"), OptFlags::NONE, "initialize submodules"),
        opt_bool(None, Some("recursive"), OptFlags::NONE, "traverse submodules recursively"),
        opt_bool(Some('f'), Some("force"), OptFlags::NONE, "force update"),
        opt_bool(None, Some("remote"), OptFlags::NONE, "use remote branch"),
        opt_bool(Some('N'), Some("no-fetch"), OptFlags::NONE, "skip fetch"),
        opt_bool(None, Some("checkout"), OptFlags::NONE, "checkout mode"),
        opt_bool(None, Some("merge"), OptFlags::NONE, "merge mode"),
        opt_bool(None, Some("rebase"), OptFlags::NONE, "rebase mode"),
        opt_bool(None, Some("recommend-shallow"), OptFlags::NONE, "recommend shallow"),
        opt_bool(None, Some("no-recommend-shallow"), OptFlags::NONEG, "no recommend shallow"),
        opt_bool(None, Some("single-branch"), OptFlags::NONE, "single branch"),
        opt_bool(None, Some("no-single-branch"), OptFlags::NONEG, "no single branch"),
        opt_bool(None, Some("progress"), OptFlags::NONE, "show progress"),
        opt_bool(None, Some("no-progress"), OptFlags::NONEG, "no progress"),
        opt_str(None, Some("depth"), "n", OptFlags::NONE, "clone depth"),
        OptionSpec {
            short: Some('j'),
            long: Some("jobs"),
            value: OptValue::Str("n"),
            flags: OptFlags::NONE,
            help: "parallel jobs",
        },
        opt_str(None, Some("filter"), "spec", OptFlags::NONE, "partial clone filter"),
        opt_str(None, Some("reference"), "repo", OptFlags::NONE, "reference repository"),
        opt_str(
            None,
            Some("reference-if-able"),
            "repo",
            OptFlags::NONE,
            "reference repository if able",
        ),
        opt_bool(None, Some("dissociate"), OptFlags::NONE, "dissociate from reference"),
        opt_str(
            None,
            Some("super-prefix"),
            "path",
            OptFlags::HIDDEN,
            "recursion prefix",
        ),
    ];
    let parsed = submodule_parse_args(args, SPECS, SUBMODULE_UPDATE_USAGE)?;
    quiet |= parsed.last_bool("quiet", false);
    let mut cli_default = UpdateType::Unspecified;
    for option in &parsed.options {
        match option.long {
            Some("checkout") if matches!(option.value, ParsedValue::Bool(true)) => {
                cli_default = UpdateType::Checkout;
            }
            Some("merge") if matches!(option.value, ParsedValue::Bool(true)) => {
                cli_default = UpdateType::Merge;
            }
            Some("rebase") if matches!(option.value, ParsedValue::Bool(true)) => {
                cli_default = UpdateType::Rebase;
            }
            _ => {}
        }
    }
    let mut reference_args = Vec::new();
    for option in &parsed.options {
        match option.long {
            Some("reference") | Some("reference-if-able") => {
                if let ParsedValue::Str(value) = option.value {
                    reference_args.push(format!("--{}", option.long.expect("checked")));
                    if !value.is_empty() {
                        reference_args.push(value.to_string());
                    } else {
                        reference_args.push(String::new());
                    }
                }
            }
            Some("dissociate") => {
                if matches!(option.value, ParsedValue::Bool(true)) {
                    reference_args.push("--dissociate".to_string());
                }
            }
            _ => {}
        }
    }
    let init = parsed.last_bool("init", false);
    let filter = parsed.last_str("filter").map(str::to_string);
    if filter.is_some() && !init {
        eprintln!("fatal: --filter can only be used with the --init option");
        return Err(GitError::Exit(129));
    }
    Ok(SubmoduleUpdateOptions {
        init,
        recursive: parsed.last_bool("recursive", false),
        quiet,
        force: parsed.last_bool("force", false),
        remote: parsed.last_bool("remote", false),
        nofetch: parsed.last_bool("no-fetch", false),
        cli_default,
        depth: parsed
            .last_str("depth")
            .map(parse_submodule_depth)
            .transpose()?,
        filter,
        reference_args,
        super_prefix: parsed.last_str("super-prefix").unwrap_or("").to_string(),
        paths: parsed.positionals,
    })
}

pub(super) fn setup_submodule_init_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(Vec<&str>, bool, String)> {
    static SPECS: &[OptionSpec<'static>] = &[
        SUBMODULE_QUIET_SPEC,
        opt_str(
            None,
            Some("super-prefix"),
            "path",
            OptFlags::HIDDEN,
            "recursion prefix",
        ),
    ];
    let parsed = submodule_parse_args(args, SPECS, SUBMODULE_INIT_USAGE)?;
    quiet |= parsed.last_bool("quiet", false);
    let super_prefix = parsed.last_str("super-prefix").unwrap_or("").to_string();
    Ok((parsed.positionals, quiet, super_prefix))
}

pub(super) fn setup_submodule_deinit_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleDeinitOptions<'_>> {
    static SPECS: &[OptionSpec<'static>] = &[
        SUBMODULE_QUIET_SPEC,
        opt_bool(None, Some("all"), OptFlags::NONE, "deinit all submodules"),
        opt_bool(Some('f'), Some("force"), OptFlags::NONE, "force deinit"),
    ];
    let parsed = submodule_parse_args(args, SPECS, SUBMODULE_DEINIT_USAGE)?;
    quiet |= parsed.last_bool("quiet", false);
    Ok(SubmoduleDeinitOptions {
        all: parsed.last_bool("all", false),
        force: parsed.last_bool("force", false),
        quiet,
        paths: parsed.positionals,
    })
}

pub(super) fn setup_submodule_set_branch_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(&str, SubmoduleSetBranchAction<'_>, bool)> {
    static SPECS: &[OptionSpec<'static>] = &[
        SUBMODULE_QUIET_SPEC,
        opt_bool(Some('d'), Some("default"), OptFlags::NONEG, "set default branch"),
        OptionSpec {
            short: Some('b'),
            long: Some("branch"),
            value: OptValue::Str("branch"),
            flags: OptFlags::NONEG,
            help: "set tracking branch",
        },
    ];
    let parsed = submodule_parse_args(args, SPECS, SUBMODULE_SET_BRANCH_USAGE)?;
    quiet |= parsed.last_bool("quiet", false);
    let default = parsed.last_bool("default", false);
    let branch = parsed.last_str("branch");
    if branch.is_none() && !default {
        eprintln!("fatal: --branch or --default required");
        return Err(GitError::Exit(128));
    }
    if branch.is_some() && default {
        eprintln!("fatal: options '--branch' and '--default' cannot be used together");
        return Err(GitError::Exit(128));
    }
    match (parsed.positionals.as_slice(), branch, default) {
        ([path], Some(branch), false) => Ok((path, SubmoduleSetBranchAction::Branch(branch), quiet)),
        ([path], None, true) => Ok((path, SubmoduleSetBranchAction::Default, quiet)),
        _ => submodule_set_branch_usage(),
    }
}

pub(super) fn setup_submodule_set_url_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(&str, &str, bool)> {
    static SPECS: &[OptionSpec<'static>] = &[SUBMODULE_QUIET_SPEC];
    let parsed = match parse_options(args, SPECS, SUBMODULE_SET_URL_USAGE) {
        Ok(parsed) => parsed,
        Err(_) => return submodule_set_url_usage(),
    };
    quiet |= parsed.last_bool("quiet", false);
    match parsed.positionals.as_slice() {
        [path, new_url] => Ok((path, new_url, quiet)),
        _ => submodule_set_url_usage(),
    }
}

pub(super) fn setup_submodule_sync_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(Vec<&str>, bool, bool, String)> {
    static SPECS: &[OptionSpec<'static>] = &[
        SUBMODULE_QUIET_SPEC,
        opt_bool(None, Some("recursive"), OptFlags::NONEG, "traverse submodules recursively"),
        opt_str(
            None,
            Some("super-prefix"),
            "path",
            OptFlags::HIDDEN,
            "recursion prefix",
        ),
    ];
    let parsed = submodule_parse_args(args, SPECS, SUBMODULE_SYNC_USAGE)?;
    quiet |= parsed.last_bool("quiet", false);
    let recursive = parsed.last_bool("recursive", false);
    let super_prefix = parsed.last_str("super-prefix").unwrap_or("").to_string();
    Ok((parsed.positionals, quiet, recursive, super_prefix))
}

pub(super) fn setup_submodule_absorbgitdirs_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(Vec<&str>, bool, String)> {
    static SPECS: &[OptionSpec<'static>] = &[
        SUBMODULE_QUIET_SPEC,
        opt_str(
            None,
            Some("super-prefix"),
            "path",
            OptFlags::HIDDEN,
            "recursion prefix",
        ),
    ];
    let parsed = submodule_parse_args(args, SPECS, SUBMODULE_ABSORBGITDIRS_USAGE)?;
    quiet |= parsed.last_bool("quiet", false);
    let super_prefix = parsed.last_str("super-prefix").unwrap_or("").to_string();
    Ok((parsed.positionals, quiet, super_prefix))
}

pub(super) fn setup_submodule_foreach_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleForeachOptions> {
    let mut recursive = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--quiet" | "-q" => {
                quiet = true;
                index += 1;
            }
            "--recursive" => {
                recursive = true;
                index += 1;
            }
            value if value.starts_with('-') => return submodule_usage(),
            _ => break,
        }
    }
    Ok(SubmoduleForeachOptions {
        args: args[index..].to_vec(),
        quiet,
        recursive,
    })
}

pub(super) fn setup_submodule_summary_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleSummaryOptions> {
    static SPECS: &[OptionSpec<'static>] = &[
        SUBMODULE_QUIET_SPEC,
        opt_bool(None, Some("cached"), OptFlags::NONE, "show cached values"),
        opt_bool(None, Some("files"), OptFlags::NONE, "show files"),
        opt_bool(None, Some("for-status"), OptFlags::NONE, "for status"),
        OptionSpec {
            short: Some('n'),
            long: Some("summary-limit"),
            value: OptValue::Str("n"),
            flags: OptFlags::NONE,
            help: "summary limit",
        },
    ];
    let parsed = submodule_parse_args(args, SPECS, SUBMODULE_SUMMARY_USAGE)?;
    quiet |= parsed.last_bool("quiet", false);
    let cached = parsed.last_bool("cached", false);
    let files = parsed.last_bool("files", false);
    if cached && files {
        eprintln!("fatal: options '--cached' and '--files' cannot be used together");
        return Err(GitError::Exit(128));
    }
    let summary_limit = parsed
        .last_str("summary-limit")
        .map(parse_submodule_summary_limit)
        .transpose()?;
    let operands = parsed
        .positionals
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    let (commit, positionals) = if operands.is_empty() {
        (None, Vec::new())
    } else {
        let mut iter = operands.into_iter();
        let commit = iter.next();
        (commit, iter.collect())
    };
    Ok(SubmoduleSummaryOptions {
        cached,
        files,
        quiet,
        summary_limit,
        commit,
        positionals,
    })
}