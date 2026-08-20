use super::{
    WorktreeAddOptions, WorktreeListOptions, WorktreeLockOptions, WorktreeMoveOptions,
    WorktreePruneOptions, WorktreeRemoveOptions, WorktreeRepairOptions,
};
use crate::commands::cli_options::{
    cli_usage_error, count_force_occurrences, last_tri_state_bool, opt_bool, opt_str, option_str,
};
use crate::*;
use sley_options::{OptFlags, OptionName, ParsedValue, parse_options};

pub(super) fn setup_worktree_list_options(args: &[String]) -> Result<WorktreeListOptions> {
    let parsed = parse_options(args, worktree_list_option_specs(), WORKTREE_LIST_USAGE)
        .map_err(cli_usage_error)?;
    if !parsed.positionals.is_empty() {
        return worktree_list_usage();
    }
    let porcelain = parsed.last_bool("porcelain", false);
    let verbose = parsed.last_bool("verbose", false);
    let z = parsed
        .options
        .iter()
        .any(|option| option.short == Some('z'));
    let mut expire = true;
    for option in &parsed.options {
        if option.long == Some("expire") {
            expire = !matches!(option.name, OptionName::NegatedLong("expire"));
        }
    }
    if z && !porcelain {
        eprintln!("fatal: the option '-z' requires '--porcelain'");
        return Err(GitError::Exit(128));
    }
    if verbose && porcelain {
        eprintln!("fatal: options '--verbose' and '--porcelain' cannot be used together");
        return Err(GitError::Exit(128));
    }
    Ok(WorktreeListOptions {
        porcelain,
        verbose,
        z,
        expire,
    })
}

pub(super) fn setup_worktree_prune_options(args: &[String]) -> Result<WorktreePruneOptions> {
    let parsed = parse_options(args, worktree_prune_option_specs(), WORKTREE_PRUNE_USAGE)
        .map_err(cli_usage_error)?;
    if !parsed.positionals.is_empty() {
        return worktree_prune_usage();
    }
    let mut expire = i64::MAX;
    for option in &parsed.options {
        if option.long != Some("expire") {
            continue;
        }
        if matches!(option.name, OptionName::NegatedLong("expire")) {
            expire = 0;
            continue;
        }
        if let ParsedValue::Str(value) = &option.value {
            expire = parse_worktree_prune_expire(value)?;
        }
    }
    Ok(WorktreePruneOptions {
        dry_run: parsed.last_bool("dry-run", false),
        verbose: parsed.last_bool("verbose", false),
        expire,
    })
}

pub(super) fn setup_worktree_lock_options(args: &[String]) -> Result<WorktreeLockOptions> {
    let parsed = parse_options(args, worktree_lock_option_specs(), WORKTREE_LOCK_USAGE)
        .map_err(cli_usage_error)?;
    let mut reason = None;
    for option in &parsed.options {
        if option.long != Some("reason") {
            continue;
        }
        if matches!(option.name, OptionName::NegatedLong("reason")) {
            reason = Some("(null)".to_string());
        } else if let ParsedValue::Str(value) = &option.value {
            reason = Some(value.to_string());
        }
    }
    match parsed.positionals.as_slice() {
        [path] => Ok(WorktreeLockOptions {
            reason,
            path: path.to_string(),
        }),
        _ => worktree_lock_usage(),
    }
}

pub(super) fn setup_worktree_remove_options(args: &[String]) -> Result<WorktreeRemoveOptions> {
    let parsed = parse_options(args, worktree_remove_option_specs(), WORKTREE_REMOVE_USAGE)
        .map_err(cli_usage_error)?;
    match parsed.positionals.as_slice() {
        [path] => Ok(WorktreeRemoveOptions {
            force: count_force_occurrences(&parsed),
            path: path.to_string(),
        }),
        _ => worktree_remove_usage(),
    }
}

pub(super) fn setup_worktree_move_options(args: &[String]) -> Result<WorktreeMoveOptions> {
    let parsed = parse_options(args, worktree_move_option_specs(), WORKTREE_MOVE_USAGE)
        .map_err(cli_usage_error)?;
    match parsed.positionals.as_slice() {
        [source, destination] => Ok(WorktreeMoveOptions {
            force: count_force_occurrences(&parsed),
            relative_paths: last_tri_state_bool(&parsed, "relative-paths"),
            source: source.to_string(),
            destination: destination.to_string(),
        }),
        _ => worktree_move_usage(),
    }
}

pub(super) fn setup_worktree_repair_options(args: &[String]) -> Result<WorktreeRepairOptions> {
    let parsed = parse_options(args, worktree_repair_option_specs(), WORKTREE_REPAIR_USAGE)
        .map_err(cli_usage_error)?;
    Ok(WorktreeRepairOptions {
        relative_paths: last_tri_state_bool(&parsed, "relative-paths"),
        paths: parsed
            .positionals
            .iter()
            .map(|path| (*path).to_string())
            .collect(),
    })
}

pub(super) fn setup_worktree_unlock_options(args: &[String]) -> Result<String> {
    let parsed = parse_options(args, &[], WORKTREE_UNLOCK_USAGE).map_err(cli_usage_error)?;
    if !parsed.options.is_empty() {
        return worktree_unlock_usage();
    }
    match parsed.positionals.as_slice() {
        [path] => Ok(path.to_string()),
        _ => worktree_unlock_usage(),
    }
}

pub(super) fn setup_worktree_add_options(args: &[String]) -> Result<WorktreeAddOptions> {
    let parsed = parse_options(args, worktree_add_option_specs(), WORKTREE_ADD_USAGE)
        .map_err(cli_usage_error)?;
    let force = count_force_occurrences(&parsed);
    let quiet = parsed.last_bool("quiet", false);
    let detach = parsed.last_bool("detach", false);
    let checkout = parsed.last_bool("checkout", true);
    let keep_locked = parsed.last_bool("lock", false);
    let orphan = parsed.last_bool("orphan", false);
    let guess_remote = last_tri_state_bool(&parsed, "guess-remote");
    let track = last_tri_state_bool(&parsed, "track");
    let relative_paths = last_tri_state_bool(&parsed, "relative-paths");
    let mut lock_reason = None;
    for option in &parsed.options {
        if option.long != Some("reason") {
            continue;
        }
        if matches!(option.name, OptionName::NegatedLong("reason")) {
            lock_reason = None;
        } else if let ParsedValue::Str(value) = &option.value {
            lock_reason = Some(value.to_string());
        }
    }
    let mut new_branch = None;
    let mut new_branch_force = None;
    for option in &parsed.options {
        match option.short {
            Some('b') => new_branch = option_str(option).map(str::to_string),
            Some('B') => new_branch_force = option_str(option).map(str::to_string),
            _ => {}
        }
    }
    let mut paths = parsed
        .positionals
        .iter()
        .map(|path| (*path).to_string())
        .collect::<Vec<_>>();

    if (detach as usize) + (new_branch.is_some() as usize) + (new_branch_force.is_some() as usize)
        > 1
    {
        eprintln!("fatal: options '-b', '-B', and '--detach' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if detach && orphan {
        eprintln!("fatal: options '--orphan' and '--detach' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if orphan && track.is_some() {
        eprintln!("fatal: options '--orphan' and '--track' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if orphan && !checkout {
        eprintln!("fatal: options '--orphan' and '--no-checkout' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if orphan && paths.len() == 2 {
        eprintln!("fatal: option '--orphan' and commit-ish cannot be used together");
        return Err(GitError::Exit(128));
    }
    if lock_reason.is_some() && !keep_locked {
        eprintln!("fatal: the option '--reason' requires '--lock'");
        return Err(GitError::Exit(128));
    }
    if paths.is_empty() || paths.len() > 2 {
        return worktree_add_usage();
    }

    let force_branch = new_branch_force.is_some();
    let branch = new_branch_force.or(new_branch);
    Ok(WorktreeAddOptions {
        force,
        quiet,
        detach,
        checkout,
        lock: keep_locked,
        lock_reason,
        branch,
        force_branch,
        orphan,
        guess_remote_flag: guess_remote,
        track,
        relative_paths,
        path: paths.remove(0),
        start: paths.pop(),
    })
}

fn parse_worktree_prune_expire(value: &str) -> Result<i64> {
    let Some(timestamp) = crate::commands::approxidate::parse_expiry_date(value) else {
        eprintln!("fatal: invalid approxidate value: '{value}'");
        return Err(GitError::Exit(128));
    };
    let timestamp = timestamp as u64;
    Ok(if timestamp >= i64::MAX as u64 {
        i64::MAX
    } else {
        timestamp as i64
    })
}

const WORKTREE_LIST_USAGE: &[&str] = &["git worktree list [-v | --porcelain [-z]]"];
const WORKTREE_PRUNE_USAGE: &[&str] = &["git worktree prune [-n] [-v] [--expire <expire>]"];
const WORKTREE_LOCK_USAGE: &[&str] = &["git worktree lock [--reason <string>] <worktree>"];
const WORKTREE_REMOVE_USAGE: &[&str] = &["git worktree remove [-f] <worktree>"];
const WORKTREE_MOVE_USAGE: &[&str] = &["git worktree move <worktree> <new-path>"];
const WORKTREE_REPAIR_USAGE: &[&str] = &["git worktree repair [<path>...]"];
const WORKTREE_UNLOCK_USAGE: &[&str] = &["git worktree unlock <worktree>"];
const WORKTREE_ADD_USAGE: &[&str] = &[
    "git worktree add [-f] [--detach] [--checkout] [--lock [--reason <string>]]\n                        [--orphan] [(-b | -B) <new-branch>] <path> [<commit-ish>]",
];

fn worktree_list_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[
        opt_bool(
            None,
            Some("porcelain"),
            OptFlags::NONE,
            "machine-readable output",
        ),
        opt_bool(
            Some('v'),
            Some("verbose"),
            OptFlags::NONE,
            "show extended annotations and reasons, if available",
        ),
        opt_str(
            None,
            Some("expire"),
            "<expiry-date>",
            OptFlags::NONE,
            "add 'prunable' annotation to missing worktrees older than <time>",
        ),
        opt_bool(
            Some('z'),
            None,
            OptFlags::NONEG,
            "terminate records with a NUL character",
        ),
    ];
    SPECS
}

fn worktree_prune_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[
        opt_bool(
            Some('n'),
            Some("dry-run"),
            OptFlags::NONE,
            "do not remove, show only",
        ),
        opt_bool(
            Some('v'),
            Some("verbose"),
            OptFlags::NONE,
            "report pruned working trees",
        ),
        opt_str(
            None,
            Some("expire"),
            "<expiry-date>",
            OptFlags::NONE,
            "prune missing working trees older than <time>",
        ),
    ];
    SPECS
}

fn worktree_lock_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[opt_str(
        None,
        Some("reason"),
        "<string>",
        OptFlags::NONE,
        "reason for locking",
    )];
    SPECS
}

fn worktree_remove_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[opt_bool(
        Some('f'),
        Some("force"),
        OptFlags::NONE,
        "force removal even if worktree is dirty or locked",
    )];
    SPECS
}

fn worktree_move_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[
        opt_bool(
            Some('f'),
            Some("force"),
            OptFlags::NONE,
            "force move even if worktree is dirty or locked",
        ),
        opt_bool(
            None,
            Some("relative-paths"),
            OptFlags::NONE,
            "use relative paths for worktrees",
        ),
    ];
    SPECS
}

fn worktree_repair_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[opt_bool(
        None,
        Some("relative-paths"),
        OptFlags::NONE,
        "use relative paths for worktrees",
    )];
    SPECS
}

fn worktree_add_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[
        opt_bool(
            Some('f'),
            Some("force"),
            OptFlags::NONE,
            "checkout <branch> even if already checked out in other worktree",
        ),
        opt_str(
            Some('b'),
            None,
            "<branch>",
            OptFlags::NONE,
            "create a new branch",
        ),
        opt_str(
            Some('B'),
            None,
            "<branch>",
            OptFlags::NONE,
            "create or reset a branch",
        ),
        opt_bool(None, Some("orphan"), OptFlags::NONE, "create unborn branch"),
        opt_bool(
            Some('d'),
            Some("detach"),
            OptFlags::NONE,
            "detach HEAD at named commit",
        ),
        opt_bool(
            None,
            Some("checkout"),
            OptFlags::NONE,
            "populate the new working tree",
        ),
        opt_bool(
            None,
            Some("lock"),
            OptFlags::NONE,
            "keep the new working tree locked",
        ),
        opt_str(
            None,
            Some("reason"),
            "<string>",
            OptFlags::NONE,
            "reason for locking",
        ),
        opt_bool(
            Some('q'),
            Some("quiet"),
            OptFlags::NONE,
            "suppress progress reporting",
        ),
        opt_bool(
            None,
            Some("track"),
            OptFlags::NONE,
            "set up tracking mode (see git-branch(1))",
        ),
        opt_bool(
            None,
            Some("guess-remote"),
            OptFlags::NONE,
            "try to match the new branch name with a remote-tracking branch",
        ),
        opt_bool(
            None,
            Some("relative-paths"),
            OptFlags::NONE,
            "use relative paths for worktrees",
        ),
    ];
    SPECS
}

fn worktree_list_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree list [-v | --porcelain [-z]]");
    eprintln!();
    eprintln!("    --[no-]porcelain      machine-readable output");
    eprintln!("    -v, --[no-]verbose    show extended annotations and reasons, if available");
    eprintln!(
        "    --[no-]expire <expiry-date>\n                          add 'prunable' annotation to missing worktrees older than <time>"
    );
    eprintln!("    -z                    terminate records with a NUL character");
    Err(GitError::Exit(129))
}

fn worktree_prune_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree prune [-n] [-v] [--expire <expire>]");
    eprintln!();
    eprintln!("    -n, --[no-]dry-run    do not remove, show only");
    eprintln!("    -v, --[no-]verbose    report pruned working trees");
    eprintln!(
        "    --[no-]expire <expiry-date>\n                          prune missing working trees older than <time>"
    );
    Err(GitError::Exit(129))
}

fn worktree_lock_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree lock [--reason <string>] <worktree>");
    eprintln!();
    eprintln!("    --[no-]reason <string>");
    eprintln!("                          reason for locking");
    eprintln!();
    Err(GitError::Exit(129))
}

fn worktree_unlock_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree unlock <worktree>");
    eprintln!();
    Err(GitError::Exit(129))
}

fn worktree_remove_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree remove [-f] <worktree>");
    eprintln!();
    eprintln!("    -f, --[no-]force      force removal even if worktree is dirty or locked");
    eprintln!();
    Err(GitError::Exit(129))
}

fn worktree_move_usage<T>() -> Result<T> {
    eprintln!("usage: git worktree move <worktree> <new-path>");
    eprintln!();
    eprintln!("    -f, --[no-]force      force move even if worktree is dirty or locked");
    eprintln!("    --[no-]relative-paths use relative paths for worktrees");
    eprintln!();
    Err(GitError::Exit(129))
}

fn worktree_add_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git worktree add [-f] [--detach] [--checkout] [--lock [--reason <string>]]\n                        [--orphan] [(-b | -B) <new-branch>] <path> [<commit-ish>]"
    );
    eprintln!();
    eprintln!(
        "    -f, --[no-]force      checkout <branch> even if already checked out in other worktree"
    );
    eprintln!("    -b <branch>           create a new branch");
    eprintln!("    -B <branch>           create or reset a branch");
    eprintln!("    --[no-]orphan         create unborn branch");
    eprintln!("    -d, --[no-]detach     detach HEAD at named commit");
    eprintln!("    --[no-]checkout       populate the new working tree");
    eprintln!("    --[no-]lock           keep the new working tree locked");
    eprintln!("    --[no-]reason <string>");
    eprintln!("                          reason for locking");
    eprintln!("    -q, --[no-]quiet      suppress progress reporting");
    eprintln!("    --[no-]track          set up tracking mode (see git-branch(1))");
    eprintln!(
        "    --[no-]guess-remote   try to match the new branch name with a remote-tracking branch"
    );
    eprintln!("    --[no-]relative-paths use relative paths for worktrees");
    eprintln!();
    Err(GitError::Exit(129))
}
