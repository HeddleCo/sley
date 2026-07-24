#![allow(clippy::expect_used)]
use super::{
    BranchColumnStyle, BranchCreateOptions, BranchDeleteMode, BranchDeleteOptions,
    BranchFormatListOptions, BranchGeneralListOptions, BranchListFilters, BranchListMode,
    BranchMoveKind, BranchMoveOptions, BranchSort, BranchTrackMode, BranchUpstreamAction,
    BranchUpstreamOptions, BranchVerboseListOptions, branch_ahead_behind_sort_value,
    branch_contains_eq_value, branch_date_sort_value, branch_merged_eq_value,
    branch_no_contains_eq_value, branch_no_merged_eq_value, branch_objectname_sort_value,
    branch_objectsize_sort_value, branch_objecttype_sort_value, branch_push_sort_value,
    branch_upstream_sort_value, branch_version_sort_value,
};
use crate::commands::cli_options::{last_tri_state_bool, opt_bool, opt_str, option_bool};
use crate::*;
use sley_options::{
    CallbackValue, OptFlags, OptValue, OptionName, OptionSpec, Parsed, ParsedValue, UsageError,
    parse_options,
};

const BRANCH_USAGE_LINES: [&str; 8] = [
    "git branch [<options>] [-r | -a] [--merged] [--no-merged]",
    "git branch [<options>] [-f] [--recurse-submodules] <branch-name> [<start-point>]",
    "git branch [<options>] [-l] [<pattern>...]",
    "git branch [<options>] [-r] (-d | -D) <branch-name>...",
    "git branch [<options>] (-m | -M) [<old-branch>] <new-branch>",
    "git branch [<options>] (-c | -C) [<old-branch>] <new-branch>",
    "git branch [<options>] [-r | -a] [--points-at]",
    "git branch [<options>] [-r | -a] [--format]",
];

fn branch_usage_error(error: UsageError) -> GitError {
    eprint!("{}", error.render_stderr());
    GitError::Exit(error.exit_code())
}

fn branch_error_is_unknown(error: &UsageError) -> bool {
    error.message().is_some_and(|message| {
        message.starts_with("unknown option `") || message.starts_with("unknown switch `")
    })
}

pub(super) fn setup_branch_options<'a>(
    args: &'a [String],
    specs: &'a [OptionSpec<'a>],
) -> Result<Option<Parsed<'a>>> {
    match parse_options(args, specs, &BRANCH_USAGE_LINES) {
        Ok(parsed) => Ok(Some(parsed)),
        Err(error) if branch_error_is_unknown(&error) => Ok(None),
        Err(error) => Err(branch_usage_error(error)),
    }
}

fn branch_positionals(parsed: &Parsed<'_>) -> Vec<String> {
    parsed
        .positionals
        .iter()
        .map(|value| (*value).to_string())
        .collect()
}

fn parse_branch_list_filter_arg(
    args: &[String],
    idx: &mut usize,
    filters: &mut BranchListFilters,
) -> Result<bool> {
    let Some(arg) = args.get(*idx).map(String::as_str) else {
        return Ok(false);
    };
    let mut read_optional_rev = |name: &str| -> String {
        if let Some(next) = args.get(*idx + 1)
            && !next.starts_with('-')
        {
            *idx += 1;
            return next.clone();
        }
        name.to_string()
    };
    match arg {
        "--contains" => {
            filters.contains.push(read_optional_rev("HEAD"));
            Ok(true)
        }
        "--no-contains" => {
            filters.no_contains.push(read_optional_rev("HEAD"));
            Ok(true)
        }
        "--merged" => {
            filters.merged.push(read_optional_rev("HEAD"));
            Ok(true)
        }
        "--no-merged" => {
            filters.no_merged.push(read_optional_rev("HEAD"));
            Ok(true)
        }
        value if branch_contains_eq_value(value).is_some() => {
            filters.contains.push(
                branch_contains_eq_value(value)
                    .expect("guard checked branch option")
                    .to_string(),
            );
            Ok(true)
        }
        value if branch_no_contains_eq_value(value).is_some() => {
            filters.no_contains.push(
                branch_no_contains_eq_value(value)
                    .expect("guard checked branch option")
                    .to_string(),
            );
            Ok(true)
        }
        value if branch_merged_eq_value(value).is_some() => {
            filters.merged.push(
                branch_merged_eq_value(value)
                    .expect("guard checked branch option")
                    .to_string(),
            );
            Ok(true)
        }
        value if branch_no_merged_eq_value(value).is_some() => {
            filters.no_merged.push(
                branch_no_merged_eq_value(value)
                    .expect("guard checked branch option")
                    .to_string(),
            );
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn branch_track_mode(value: &str) -> BranchTrackMode {
    match value {
        "inherit" => BranchTrackMode::Inherit,
        "never" => BranchTrackMode::Never,
        _ => BranchTrackMode::Direct,
    }
}

fn parse_branch_track_value(
    value: CallbackValue<'_>,
) -> std::result::Result<Option<String>, String> {
    if value.unset {
        return Ok(Some("never".into()));
    }
    match value.value.unwrap_or("direct") {
        "direct" => Ok(Some("direct".into())),
        "inherit" => Ok(Some("inherit".into())),
        _ => {
            let option = match value.option {
                OptionName::Short(short) => format!("-{short}"),
                OptionName::Long(long) | OptionName::NegatedLong(long) => format!("--{long}"),
            };
            Err(format!(
                "option `{option}' expects \"direct\" or \"inherit\""
            ))
        }
    }
}

const BRANCH_TRACK_OPTION: OptionSpec<'static> = OptionSpec {
    short: Some('t'),
    long: Some("track"),
    value: OptValue::Callback {
        metavar: Some("(direct|inherit)"),
        parse: parse_branch_track_value,
    },
    flags: OptFlags::OPTARG,
    help: "set branch tracking configuration",
};

fn branch_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(
            Some('v'),
            Some("verbose"),
            OptFlags::NONE,
            "show hash and subject, give twice for upstream branch",
        ),
        opt_bool(
            Some('q'),
            Some("quiet"),
            OptFlags::NONE,
            "suppress informational messages",
        ),
        BRANCH_TRACK_OPTION,
        opt_bool(
            None,
            Some("unset-upstream"),
            OptFlags::NONE,
            "unset the upstream info",
        ),
        opt_str(
            None,
            Some("color"),
            "when",
            OptFlags::OPTARG,
            "use colored output",
        ),
        opt_bool(
            Some('r'),
            Some("remotes"),
            OptFlags::NONEG,
            "act on remote-tracking branches",
        ),
        opt_str(
            None,
            Some("abbrev"),
            "n",
            OptFlags::OPTARG,
            "use <n> digits to display object names",
        ),
        opt_bool(
            Some('a'),
            Some("all"),
            OptFlags::NONEG,
            "list both remote-tracking and local branches",
        ),
        opt_bool(
            Some('d'),
            Some("delete"),
            OptFlags::NONE,
            "delete fully merged branch",
        ),
        opt_bool(
            Some('D'),
            None,
            OptFlags::NONE,
            "delete branch (even if not merged)",
        ),
        opt_bool(
            Some('m'),
            Some("move"),
            OptFlags::NONE,
            "move/rename a branch and its reflog",
        ),
        opt_bool(
            Some('M'),
            None,
            OptFlags::NONE,
            "move/rename a branch, even if target exists",
        ),
        opt_bool(
            None,
            Some("omit-empty"),
            OptFlags::NONE,
            "do not output a newline after empty formatted refs",
        ),
        opt_bool(
            Some('c'),
            Some("copy"),
            OptFlags::NONE,
            "copy a branch and its reflog",
        ),
        opt_bool(
            Some('C'),
            None,
            OptFlags::NONE,
            "copy a branch, even if target exists",
        ),
        opt_bool(Some('l'), Some("list"), OptFlags::NONE, "list branch names"),
        opt_bool(
            None,
            Some("show-current"),
            OptFlags::NONE,
            "show current branch name",
        ),
        opt_bool(
            None,
            Some("create-reflog"),
            OptFlags::NONE,
            "create the branch's reflog",
        ),
        opt_bool(
            None,
            Some("edit-description"),
            OptFlags::NONE,
            "edit the description for the branch",
        ),
        opt_bool(
            Some('f'),
            Some("force"),
            OptFlags::NONE,
            "force creation, move/rename, deletion",
        ),
        opt_str(
            None,
            Some("column"),
            "style",
            OptFlags::OPTARG,
            "list branches in columns",
        ),
        opt_str(
            None,
            Some("sort"),
            "key",
            OptFlags::NONE,
            "field name to sort on",
        ),
        opt_bool(
            Some('i'),
            Some("ignore-case"),
            OptFlags::NONE,
            "sorting and filtering are case insensitive",
        ),
        opt_bool(
            None,
            Some("recurse-submodules"),
            OptFlags::NONE,
            "recurse through submodules",
        ),
        opt_str(
            None,
            Some("format"),
            "format",
            OptFlags::NONE,
            "format to use for the output",
        ),
    ];
    SPECS
}

pub(super) fn setup_branch_show_current_options(args: &[String]) -> Result<Option<bool>> {
    let Some(parsed) = setup_branch_options(args, branch_option_specs())? else {
        return Ok(None);
    };
    let show_current = last_tri_state_bool(&parsed, "show-current");
    let has_other_options = parsed
        .options
        .iter()
        .any(|option| option.long != Some("show-current"));
    match show_current {
        Some(true) => Ok(Some(true)),
        Some(false) if parsed.positionals.is_empty() && !has_other_options => Ok(Some(false)),
        _ => Ok(None),
    }
}

pub(super) fn setup_branch_general_list_options(
    git_dir: &Path,
    replace_objects: bool,
    args: &[String],
) -> Result<Option<BranchGeneralListOptions>> {
    let mut mode = BranchListMode::Local;
    let mut patterns = Vec::new();
    let mut filters = BranchListFilters::default();
    let mut ignore_case = false;
    let mut color = false;
    let mut column = None;
    let mut sort = None;
    let mut explicit_no_sort = false;
    let mut explicit_list = false;
    let mut saw_list_control = args.is_empty();
    let mut idx = 0;
    while idx < args.len() {
        if parse_branch_list_filter_arg(args, &mut idx, &mut filters)? {
            saw_list_control = true;
            idx += 1;
            continue;
        }
        match args[idx].as_str() {
            "-l" | "--list" => {
                explicit_list = true;
                saw_list_control = true;
            }
            "-r" | "--remotes" => {
                mode = BranchListMode::Remote;
                saw_list_control = true;
            }
            "-a" | "--all" => {
                mode = BranchListMode::All;
                saw_list_control = true;
            }
            "-i" | "--ignore-case" => {
                ignore_case = true;
                saw_list_control = true;
            }
            "--no-ignore-case" => {
                ignore_case = false;
                saw_list_control = true;
            }
            "--color" | "--color=always" => {
                color = true;
                saw_list_control = true;
            }
            "--no-color" | "--color=never" | "--color=auto" => {
                color = false;
                saw_list_control = true;
            }
            "--column" | "--column=column" => {
                column = Some(BranchColumnStyle::Column);
                saw_list_control = true;
            }
            "--column=dense" => {
                column = Some(BranchColumnStyle::Dense);
                saw_list_control = true;
            }
            "--no-column" | "--column=never" | "--column=plain" | "--column=auto" => {
                column = None;
                saw_list_control = true;
            }
            "--sort" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--sort requires a value".into()));
                };
                sort = Some(branch_sort_from_key(
                    git_dir,
                    repository_object_format(git_dir)?,
                    replace_objects,
                    value,
                )?);
                explicit_no_sort = false;
                saw_list_control = true;
            }
            value if value.starts_with("--sort=") => {
                let value = value
                    .strip_prefix("--sort=")
                    .expect("prefix checked by match guard");
                sort = Some(branch_sort_from_key(
                    git_dir,
                    repository_object_format(git_dir)?,
                    replace_objects,
                    value,
                )?);
                explicit_no_sort = false;
                saw_list_control = true;
            }
            "--no-sort" => {
                sort = None;
                explicit_no_sort = true;
                saw_list_control = true;
            }
            value if value.starts_with('-') => return Ok(None),
            value => patterns.push(value.to_string()),
        }
        idx += 1;
    }

    if !patterns.is_empty()
        && !explicit_list
        && filters.is_empty()
        && !matches!(mode, BranchListMode::Remote | BranchListMode::All)
    {
        return Ok(None);
    }

    let config = read_repo_config(git_dir)?;
    if sort.is_none()
        && !explicit_no_sort
        && let Some(config_sort) = config.get("branch", None, "sort")
    {
        sort = Some(branch_sort_from_key(
            git_dir,
            repository_object_format(git_dir)?,
            replace_objects,
            config_sort,
        )?);
        saw_list_control = true;
    }
    if column.is_none()
        && !args
            .iter()
            .any(|arg| arg.starts_with("--column") || arg == "--no-column")
        && branch_config_enables_columns(&config)
    {
        column = Some(branch_config_column_style(&config));
        saw_list_control = true;
    }

    Ok(saw_list_control.then_some(BranchGeneralListOptions {
        mode,
        patterns,
        filters,
        ignore_case,
        color,
        column,
        sort,
    }))
}

pub(super) fn setup_branch_format_list_options(
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    args: &[String],
) -> Result<Option<BranchFormatListOptions>> {
    if !args
        .iter()
        .any(|arg| arg == "--format" || arg.starts_with("--format="))
    {
        return Ok(None);
    }

    let mut mode = BranchListMode::Local;
    let mut patterns = Vec::new();
    let mut ignore_case = false;
    let mut color = false;
    let mut sort = None;
    let mut format_spec = None;
    let mut omit_empty = false;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "-l" | "--list" => {}
            "-r" | "--remotes" => mode = BranchListMode::Remote,
            "-a" | "--all" => mode = BranchListMode::All,
            "-i" | "--ignore-case" => ignore_case = true,
            "--no-ignore-case" => ignore_case = false,
            "--color" | "--color=always" => color = true,
            "--no-color" | "--color=never" | "--color=auto" => color = false,
            "--omit-empty" => omit_empty = true,
            "--no-omit-empty" => omit_empty = false,
            "--no-format" => format_spec = None,
            "--format" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("branch --format requires a value".into()));
                };
                format_spec = Some(value.to_string());
            }
            value if value.starts_with("--format=") => {
                format_spec = Some(
                    value
                        .strip_prefix("--format=")
                        .expect("prefix checked by match guard")
                        .to_string(),
                );
            }
            "--sort" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--sort requires a value".into()));
                };
                sort = Some(branch_sort_from_key(
                    git_dir,
                    format,
                    replace_objects,
                    value,
                )?);
            }
            value if value.starts_with("--sort=") => {
                let value = value
                    .strip_prefix("--sort=")
                    .expect("prefix checked by match guard");
                sort = Some(branch_sort_from_key(
                    git_dir,
                    format,
                    replace_objects,
                    value,
                )?);
            }
            "--no-sort" => sort = None,
            value if value.starts_with('-') => return Ok(None),
            value => patterns.push(value.to_string()),
        }
        idx += 1;
    }

    Ok(format_spec.map(|format_spec| BranchFormatListOptions {
        mode,
        patterns,
        ignore_case,
        color,
        sort,
        format_spec,
        omit_empty,
    }))
}

fn branch_sort_from_key(
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    key: &str,
) -> Result<BranchSort> {
    let key = key.strip_prefix("--sort=").unwrap_or(key);
    match key {
        "refname" => Ok(BranchSort::Refname(false)),
        "-refname" => Ok(BranchSort::Refname(true)),
        value if branch_version_sort_value(value).is_some() => Ok(BranchSort::Version(
            branch_version_sort_value(value).expect("checked branch version sort"),
        )),
        value if branch_objectname_sort_value(value).is_some() => Ok(BranchSort::ObjectName(
            branch_objectname_sort_value(value).expect("checked branch objectname sort"),
        )),
        value if branch_objecttype_sort_value(value).is_some() => Ok(BranchSort::ObjectType(
            branch_objecttype_sort_value(value).expect("checked branch objecttype sort"),
        )),
        value if branch_objectsize_sort_value(value).is_some() => Ok(BranchSort::ObjectSize(
            branch_objectsize_sort_value(value).expect("checked branch objectsize sort"),
        )),
        value if branch_date_sort_value(value).is_some() => {
            let (field, descending) =
                branch_date_sort_value(value).expect("checked branch date sort");
            Ok(BranchSort::Date(field, descending))
        }
        value if branch_upstream_sort_value(value).is_some() => Ok(BranchSort::Upstream(
            branch_upstream_sort_value(value).expect("checked branch upstream sort"),
        )),
        value if branch_push_sort_value(value).is_some() => Ok(BranchSort::Push(
            branch_push_sort_value(value).expect("checked branch push sort"),
        )),
        value if branch_ahead_behind_sort_value(value).is_some() => {
            let (rev, descending) =
                branch_ahead_behind_sort_value(value).expect("checked ahead-behind sort");
            let oid = resolve_revision(git_dir, format, rev, replace_objects)?;
            Ok(BranchSort::AheadBehind(oid, descending))
        }
        _ => {
            eprintln!("fatal: unknown field name: {key}");
            Err(GitError::Exit(128))
        }
    }
}

fn branch_config_enables_columns(config: &GitConfig) -> bool {
    config
        .get("column", Some("branch"), "branch")
        .or_else(|| config.get("column", None, "branch"))
        .or_else(|| config.get("column", None, "ui"))
        .is_some_and(|value| matches!(value, "column" | "dense" | "always"))
}

fn branch_config_column_style(config: &GitConfig) -> BranchColumnStyle {
    match config
        .get("column", Some("branch"), "branch")
        .or_else(|| config.get("column", None, "branch"))
    {
        Some("dense") => BranchColumnStyle::Dense,
        _ => BranchColumnStyle::Column,
    }
}

fn branch_verbose_list_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(
            Some('v'),
            Some("verbose"),
            OptFlags::NONE,
            "show hash and subject, give twice for upstream branch",
        ),
        opt_bool(Some('l'), Some("list"), OptFlags::NONE, "list branch names"),
        opt_bool(
            None,
            Some("no-delete"),
            OptFlags::NONEG,
            "do not delete branches",
        ),
        opt_bool(
            None,
            Some("show-current"),
            OptFlags::NONE,
            "show current branch name",
        ),
        opt_bool(
            Some('r'),
            Some("remotes"),
            OptFlags::NONEG,
            "act on remote-tracking branches",
        ),
        opt_bool(
            Some('a'),
            Some("all"),
            OptFlags::NONEG,
            "list both remote-tracking and local branches",
        ),
        opt_bool(
            Some('i'),
            Some("ignore-case"),
            OptFlags::NONE,
            "sorting and filtering are case insensitive",
        ),
        opt_str(
            None,
            Some("color"),
            "when",
            OptFlags::OPTARG,
            "use colored output",
        ),
        opt_str(
            None,
            Some("column"),
            "style",
            OptFlags::OPTARG,
            "list branches in columns",
        ),
        opt_str(
            None,
            Some("abbrev"),
            "n",
            OptFlags::OPTARG,
            "use <n> digits to display object names",
        ),
        opt_str(
            None,
            Some("sort"),
            "key",
            OptFlags::NONE,
            "field name to sort on",
        ),
        opt_str(
            None,
            Some("contains"),
            "commit",
            OptFlags::OPTARG,
            "print only branches that contain the commit",
        ),
        opt_str(
            None,
            Some("no-contains"),
            "commit",
            OptFlags::OPTARG,
            "print only branches that don't contain the commit",
        ),
        opt_str(
            None,
            Some("merged"),
            "commit",
            OptFlags::OPTARG,
            "print only branches that are merged",
        ),
        opt_str(
            None,
            Some("no-merged"),
            "commit",
            OptFlags::OPTARG,
            "print only branches that are not merged",
        ),
    ];
    SPECS
}

pub(super) fn setup_branch_verbose_list_options(
    args: &[String],
) -> Result<Option<BranchVerboseListOptions>> {
    let mut verbosity = 0usize;
    let mut explicit_list = false;
    let mut mode = BranchListMode::Local;
    let mut ignore_case = false;
    let mut abbrev = None;
    let mut color = false;
    let mut saw_verbose = false;
    let mut saw_column = false;
    let Some(parsed) = setup_branch_options(args, branch_verbose_list_option_specs())? else {
        return Ok(None);
    };
    for option in &parsed.options {
        match option.long {
            Some("verbose") => {
                saw_verbose = true;
                if option_bool(option).unwrap_or(true) {
                    verbosity = verbosity.saturating_add(1);
                } else {
                    verbosity = 0;
                }
            }
            Some("list") => {
                if option_bool(option).unwrap_or(true) {
                    explicit_list = true;
                }
            }
            Some("remotes") => mode = BranchListMode::Remote,
            Some("all") => mode = BranchListMode::All,
            Some("ignore-case") => ignore_case = option_bool(option).unwrap_or(true),
            Some("column") => saw_column = true,
            _ => {}
        }
    }
    for arg in args {
        match arg.as_str() {
            "--color" | "--color=always" => color = true,
            "--no-color" | "--color=never" | "--color=auto" => color = false,
            "--abbrev" => abbrev = None,
            "--no-abbrev" => abbrev = Some(None),
            value if value.starts_with("--abbrev=") => {
                let value = value
                    .strip_prefix("--abbrev=")
                    .expect("prefix checked by match guard");
                let width = value
                    .parse::<usize>()
                    .map_err(|_| GitError::Command(format!("invalid abbrev length {value}")))?;
                abbrev = if width == 0 {
                    Some(None)
                } else {
                    Some(Some(width))
                };
            }
            _ => {}
        }
    }
    if !saw_verbose {
        return Ok(None);
    }
    if saw_column && verbosity > 0 {
        eprintln!("fatal: options '--column' and '--verbose' cannot be used together");
        return Err(GitError::Exit(128));
    }
    let (patterns, filters) = branch_verbose_patterns_and_filters(args)?;
    if !explicit_list
        && !matches!(mode, BranchListMode::Remote | BranchListMode::All)
        && !patterns.is_empty()
        && filters.is_empty()
    {
        return Ok(None);
    }
    Ok(Some(BranchVerboseListOptions {
        mode,
        patterns,
        filters,
        ignore_case,
        verbosity,
        abbrev,
        color,
    }))
}

fn branch_verbose_patterns_and_filters(
    args: &[String],
) -> Result<(Vec<String>, BranchListFilters)> {
    let mut patterns = Vec::new();
    let mut filters = BranchListFilters::default();
    let mut idx = 0;
    while idx < args.len() {
        if parse_branch_list_filter_arg(args, &mut idx, &mut filters)? {
            idx += 1;
            continue;
        }
        match args[idx].as_str() {
            "-v" | "--verbose" | "--no-verbose" | "-l" | "--list" | "--no-list" | "-r"
            | "--remotes" | "-a" | "--all" | "-i" | "--ignore-case" | "--no-ignore-case"
            | "--color" | "--no-color" | "--column" | "--no-column" | "--abbrev"
            | "--no-abbrev" | "--show-current" | "--no-show-current" | "--no-delete" => {}
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..].bytes().all(|byte| byte == b'v') => {}
            value
                if value.starts_with("--color=")
                    || value.starts_with("--column=")
                    || value.starts_with("--abbrev=")
                    || value.starts_with("--sort=") => {}
            "--sort" => {
                idx += 1;
            }
            value if value.starts_with('-') => {}
            value => patterns.push(value.to_string()),
        }
        idx += 1;
    }
    Ok((patterns, filters))
}

fn branch_move_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(
            Some('m'),
            Some("move"),
            OptFlags::NONE,
            "move/rename a branch and its reflog",
        ),
        opt_bool(
            Some('M'),
            None,
            OptFlags::NONE,
            "move/rename a branch, even if target exists",
        ),
        opt_bool(
            Some('c'),
            Some("copy"),
            OptFlags::NONE,
            "copy a branch and its reflog",
        ),
        opt_bool(
            Some('C'),
            None,
            OptFlags::NONE,
            "copy a branch, even if target exists",
        ),
        opt_bool(
            Some('f'),
            Some("force"),
            OptFlags::NONE,
            "force creation, move/rename, deletion",
        ),
        opt_bool(
            Some('q'),
            Some("quiet"),
            OptFlags::NONE,
            "suppress informational messages",
        ),
        opt_bool(
            Some('v'),
            Some("verbose"),
            OptFlags::NONE,
            "show hash and subject, give twice for upstream branch",
        ),
    ];
    SPECS
}

pub(super) fn setup_branch_move_options(args: &[String]) -> Result<Option<BranchMoveOptions>> {
    let mut kind = None;
    let mut force = false;
    let Some(parsed) = setup_branch_options(args, branch_move_option_specs())? else {
        return Ok(None);
    };
    for option in &parsed.options {
        match (option.short, option.long) {
            (Some('m'), _) | (_, Some("move")) => {
                if option_bool(option).unwrap_or(true) {
                    kind = Some(BranchMoveKind::Rename);
                } else {
                    kind = None;
                }
            }
            (Some('M'), _) => {
                kind = Some(BranchMoveKind::Rename);
                force = true;
            }
            (Some('c'), _) | (_, Some("copy")) => {
                if option_bool(option).unwrap_or(true) {
                    kind = Some(BranchMoveKind::Copy);
                } else {
                    kind = None;
                }
            }
            (Some('C'), _) => {
                kind = Some(BranchMoveKind::Copy);
                force = true;
            }
            (_, Some("force")) => force = option_bool(option).unwrap_or(true),
            _ => {}
        }
    }
    Ok(kind.map(|kind| BranchMoveOptions {
        kind,
        force,
        branches: branch_positionals(&parsed),
    }))
}

fn branch_upstream_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(
            None,
            Some("set-upstream"),
            OptFlags::NONE,
            "set upstream for git pull/status",
        ),
        opt_str(
            Some('u'),
            Some("set-upstream-to"),
            "upstream",
            OptFlags::NONE,
            "change the upstream info",
        ),
        opt_bool(
            None,
            Some("unset-upstream"),
            OptFlags::NONE,
            "unset the upstream info",
        ),
    ];
    SPECS
}

pub(super) fn setup_branch_upstream_options(
    args: &[String],
) -> Result<Option<BranchUpstreamOptions>> {
    let mut action = None;
    let Some(parsed) = setup_branch_options(args, branch_upstream_option_specs())? else {
        return Ok(None);
    };
    for option in &parsed.options {
        match option.long {
            Some("set-upstream-to") => match option.value {
                ParsedValue::Str(_) if matches!(option.name, OptionName::NegatedLong(_)) => {
                    action = None;
                }
                ParsedValue::Str(value) => {
                    action = Some(BranchUpstreamAction::Set(value.to_string()));
                }
                _ => {}
            },
            Some("unset-upstream") => {
                if option_bool(option).unwrap_or(true) {
                    action = Some(BranchUpstreamAction::Unset);
                } else {
                    action = None;
                }
            }
            _ => {}
        }
    }
    Ok(action.map(|action| BranchUpstreamOptions {
        action,
        branches: branch_positionals(&parsed),
    }))
}

fn branch_create_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(
            Some('f'),
            Some("force"),
            OptFlags::NONE,
            "force creation, move/rename, deletion",
        ),
        opt_bool(
            Some('q'),
            Some("quiet"),
            OptFlags::NONE,
            "suppress informational messages",
        ),
        BRANCH_TRACK_OPTION,
        opt_bool(
            None,
            Some("recurse-submodules"),
            OptFlags::NONE,
            "recurse through submodules",
        ),
        opt_bool(
            None,
            Some("set-upstream"),
            OptFlags::NONE,
            "set upstream for git pull/status",
        ),
        opt_bool(
            None,
            Some("edit-description"),
            OptFlags::NONE,
            "edit the description for the branch",
        ),
        opt_bool(
            None,
            Some("create-reflog"),
            OptFlags::NONE,
            "create the branch's reflog",
        ),
        opt_bool(
            Some('v'),
            Some("verbose"),
            OptFlags::NONE,
            "show hash and subject, give twice for upstream branch",
        ),
    ];
    SPECS
}

pub(super) fn setup_branch_create_options(args: &[String]) -> Result<Option<BranchCreateOptions>> {
    let mut saw_create_option = false;
    let mut force = false;
    let mut quiet = false;
    let mut track = None;
    let mut recurse_submodules = false;
    let mut legacy_set_upstream = false;
    let mut edit_description = false;
    let mut create_reflog = false;
    let saw_separator = args.iter().any(|arg| arg == "--");
    let Some(parsed) = setup_branch_options(args, branch_create_option_specs())? else {
        return Ok(None);
    };
    for option in &parsed.options {
        saw_create_option = true;
        match option.long {
            Some("force") => force = option_bool(option).unwrap_or(true),
            Some("quiet") => quiet = option_bool(option).unwrap_or(true),
            Some("track") => {
                if let ParsedValue::Callback(Some(value)) = &option.value {
                    track = Some(branch_track_mode(value));
                }
            }
            Some("recurse-submodules") => {
                recurse_submodules = option_bool(option).unwrap_or(true);
            }
            Some("set-upstream") => {
                legacy_set_upstream = option_bool(option).unwrap_or(true);
            }
            Some("edit-description") => {
                edit_description = option_bool(option).unwrap_or(true);
            }
            Some("create-reflog") => {
                create_reflog = option_bool(option).unwrap_or(true);
            }
            _ => {}
        }
    }

    let positionals = branch_positionals(&parsed);
    // Also treat bare `git branch <name> [<start>]` as create (no create-only
    // flag) so `git -c submodule.recurse=true branch branch-a` reaches the
    // recursive path (t3207). Listing (`git branch`) has empty positionals and
    // still falls through.
    Ok(
        (saw_create_option || saw_separator || !positionals.is_empty()).then_some(
            BranchCreateOptions {
                force,
                quiet,
                track,
                recurse_submodules,
                legacy_set_upstream,
                edit_description,
                create_reflog,
                positionals,
            },
        ),
    )
}

fn branch_delete_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(
            Some('d'),
            Some("delete"),
            OptFlags::NONE,
            "delete fully merged branch",
        ),
        opt_bool(
            Some('D'),
            None,
            OptFlags::NONE,
            "delete branch (even if not merged)",
        ),
        opt_bool(
            Some('f'),
            Some("force"),
            OptFlags::NONE,
            "force creation, move/rename, deletion",
        ),
        opt_bool(
            Some('q'),
            Some("quiet"),
            OptFlags::NONE,
            "suppress informational messages",
        ),
        opt_bool(
            Some('v'),
            Some("verbose"),
            OptFlags::NONE,
            "show hash and subject, give twice for upstream branch",
        ),
        opt_bool(
            Some('r'),
            Some("remotes"),
            OptFlags::NONEG,
            "act on remote-tracking branches",
        ),
        opt_bool(
            Some('a'),
            Some("all"),
            OptFlags::NONEG,
            "list both remote-tracking and local branches",
        ),
    ];
    SPECS
}

pub(super) fn setup_branch_delete_options(args: &[String]) -> Result<Option<BranchDeleteOptions>> {
    let mut saw_delete_option = false;
    let mut delete = false;
    let mut force = false;
    let mut quiet = false;
    let mut mode = BranchDeleteMode::Local;
    let Some(parsed) = setup_branch_options(args, branch_delete_option_specs())? else {
        return Ok(None);
    };
    for option in &parsed.options {
        match (option.short, option.long) {
            (Some('d'), _) | (_, Some("delete")) => {
                saw_delete_option = true;
                delete = option_bool(option).unwrap_or(true);
            }
            (Some('D'), _) => {
                saw_delete_option = true;
                delete = true;
                force = true;
            }
            (_, Some("force")) => force = option_bool(option).unwrap_or(true),
            (_, Some("quiet")) => quiet = option_bool(option).unwrap_or(true),
            (Some('r'), _) | (_, Some("remotes")) => mode = BranchDeleteMode::Remote,
            (Some('a'), _) | (_, Some("all")) => mode = BranchDeleteMode::All,
            _ => {}
        }
    }

    Ok(
        (saw_delete_option && delete).then_some(BranchDeleteOptions {
            force,
            quiet,
            mode,
            branches: branch_positionals(&parsed),
        }),
    )
}
